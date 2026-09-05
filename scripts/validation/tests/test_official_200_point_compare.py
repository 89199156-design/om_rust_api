from __future__ import annotations

import contextlib
import gzip
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "official_200_point_compare", ROOT / "official_200_point_compare.py"
)
assert SPEC is not None and SPEC.loader is not None
compare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compare)


class Official200PointCompareTests(unittest.TestCase):
    def test_parse_expected_runs_requires_unique_valid_pairs(self) -> None:
        self.assertEqual(
            compare.parse_expected_runs("gfs=2026090412,cams=2026090400"),
            {"gfs": "2026090412", "cams": "2026090400"},
        )
        with self.assertRaises(compare.ValidationError):
            compare.parse_expected_runs("gfs=bad")
        with self.assertRaises(compare.ValidationError):
            compare.parse_expected_runs("gfs=2026090412,gfs=2026090412")

    def test_probe_persists_matching_temporal_and_spatial_identity(self) -> None:
        temporal = json.dumps(
            {"last_run_initialisation_time": 1788523200}
        ).encode()
        spatial = json.dumps(
            {"reference_time": "2026-09-04T12:00:00Z", "completed": True}
        ).encode()

        def response(_method, url, **_kwargs):
            return (spatial if "data_spatial" in url else temporal), {}, 0.01

        with tempfile.TemporaryDirectory() as temporary:
            with mock.patch.object(compare, "request_json", side_effect=response):
                proof = compare.capture_source_run_probe(
                    "cams", Path(temporary), "before", "2026090412", 10, 0
                )
            self.assertEqual(proof["expected_run"], "2026090412")
            self.assertEqual(proof["identities"][0]["temporal_run"], "2026090412")
            self.assertTrue(
                (
                    Path(temporary)
                    / "source-probes/before-cams_global-temporal.json"
                ).is_file()
            )

    def test_probe_rejects_transition_before_snapshot_capture(self) -> None:
        temporal = json.dumps(
            {"last_run_initialisation_time": 1788523200}
        ).encode()
        spatial = json.dumps(
            {"reference_time": "2026-09-04T12:00:00Z", "completed": True}
        ).encode()

        def response(_method, url, **_kwargs):
            return (spatial if "data_spatial" in url else temporal), {}, 0.01

        with tempfile.TemporaryDirectory() as temporary:
            with mock.patch.object(compare, "request_json", side_effect=response):
                with self.assertRaisesRegex(
                    compare.ValidationError, "source run mismatch"
                ):
                    compare.capture_source_run_probe(
                        "cams", Path(temporary), "after", "2026090400", 10, 0
                    )

    def test_persistent_ssh_transport_decompresses_responses(self) -> None:
        expected = b'{"hourly":{"time":["2026-08-02T00:00"]}}'
        response = compare.canonical_bytes(
            {
                "ok": True,
                "body": compare.base64.b64encode(
                    gzip.compress(expected, compresslevel=1)
                ).decode("ascii"),
                "content_encoding": "gzip+base64",
                "elapsed": 0.012,
            }
        )
        process = mock.Mock()
        process.stdin = io.BytesIO()
        process.stdout = io.BytesIO(response + b"\n")
        client = compare.ProductionSshApiClient("singapore", 10.0, 0)
        client.process = process

        raw, headers, elapsed = client.request(
            "http://127.0.0.1:8088/v1/gfs?latitude=31.2",
            {"Accept": "application/json"},
        )

        self.assertEqual(raw, expected)
        self.assertEqual(headers, {})
        self.assertEqual(elapsed, 0.012)

    def test_persistent_ssh_transport_accepts_projection_digest(self) -> None:
        response = compare.canonical_bytes(
            {
                "ok": True,
                "projection_sha256": "a" * 64,
                "projection_valid": True,
                "value_count": 361,
                "source_response_bytes": 812345,
                "content_encoding": "sha256-json-projection-v1",
                "elapsed": 0.034,
            }
        )
        process = mock.Mock()
        process.stdin = io.BytesIO()
        process.stdout = io.BytesIO(response + b"\n")
        client = compare.ProductionSshApiClient("singapore", 10.0, 0)
        client.process = process

        digest = client.request_projection_digest(
            "http://127.0.0.1:8088/v1/ecmwf?latitude=31.2",
            {"Accept": "application/json"},
            {"hourly": ("temperature_2m",), "daily": ()},
        )

        self.assertEqual(digest["projection_sha256"], "a" * 64)
        self.assertTrue(digest["projection_valid"])
        self.assertEqual(digest["value_count"], 361)
        self.assertEqual(digest["source_response_bytes"], 812345)
        self.assertEqual(digest["transport_response_bytes"], len(response) + 1)
        request = json.loads(process.stdin.getvalue())
        self.assertEqual(request["projection"], {"hourly": ["temperature_2m"]})

    def test_ssh_request_decompresses_the_remote_response(self) -> None:
        expected = b'[{"ok":true}]'
        completed = mock.Mock(
            returncode=0,
            stdout=gzip.compress(expected),
            stderr=b"",
        )
        with mock.patch.object(compare.subprocess, "run", return_value=completed) as run:
            raw, headers, elapsed = compare.request_json_via_ssh(
                "server-alias",
                "POST",
                "https://api.open-meteo.com/v1/gfs",
                body=b"{}",
                headers={"Content-Type": "application/json"},
                timeout=10.0,
                retries=0,
            )

        self.assertEqual(raw, expected)
        self.assertEqual(headers, {})
        self.assertGreaterEqual(elapsed, 0)
        self.assertIn("gzip -1 -c", run.call_args.args[0][-1])

    def test_ssh_get_targets_the_real_loopback_api_without_a_body(self) -> None:
        expected = b'{"hourly":{"time":["2026-08-02T00:00"]}}'
        completed = mock.Mock(
            returncode=0,
            stdout=gzip.compress(expected),
            stderr=b"",
        )
        with mock.patch.object(compare.subprocess, "run", return_value=completed) as run:
            raw, _headers, _elapsed = compare.request_json_via_ssh(
                "singapore",
                "GET",
                "http://127.0.0.1:8088/v1/gfs?latitude=31.2",
                body=None,
                headers={"Accept": "application/json"},
                timeout=10.0,
                retries=0,
            )

        self.assertEqual(raw, expected)
        command = run.call_args.args[0][-1]
        self.assertIn("-X GET", command)
        self.assertNotIn("--data-binary", command)
        self.assertIn("127.0.0.1:8088", command)

    def test_plan_has_exactly_two_hundred_stable_points(self) -> None:
        points = compare.sample_points()
        self.assertEqual(len(points), 200)
        self.assertEqual(points, compare.sample_points())
        self.assertEqual(len({point["id"] for point in points}), 200)
        kinds = {point["kind"] for point in points}
        self.assertEqual(
            kinds,
            {
                "random_exact_common_native_grid",
                "random_offgrid_near_native_grid",
                "random_offgrid_uniform_crop",
            },
        )
        self.assertEqual(
            sum(point["kind"] == "random_exact_common_native_grid" for point in points),
            70,
        )
        self.assertEqual(
            sum(point["kind"] == "random_offgrid_near_native_grid" for point in points),
            70,
        )
        self.assertEqual(
            sum(point["kind"] == "random_offgrid_uniform_crop" for point in points),
            60,
        )

    def test_ec9_uses_official_ifs_and_local_nine_kilometre_endpoint(self) -> None:
        spec = compare.MODEL_SPECS["ec9"]
        self.assertEqual(spec["model_parameter"], ("models", ["ecmwf_ifs"]))
        self.assertEqual(spec["local_path"], "/v1/ecmwf-ifs9km")
        self.assertNotIn("precipitation_probability", spec["official_hourly"])
        self.assertIn("wind_speed_200m", spec["official_hourly"])
        self.assertIn("showers_sum", spec["daily"])

    def test_ec9_exact_cohort_uses_o1280_grid_coordinates(self) -> None:
        points = compare.sample_points(model="ec9")
        exact = [point for point in points if point["kind"] == "random_exact_common_native_grid"]
        self.assertEqual(len(exact), 70)
        for point in exact:
            self.assertEqual(
                (point["latitude"], point["longitude"]),
                compare._ec9_o1280_nearest_coordinate(
                    point["latitude"], point["longitude"]
                ),
            )
        self.assertNotEqual(points, compare.sample_points(model="ec"))

    def test_probability_daily_aggregations_are_in_both_weather_catalogs(self) -> None:
        expected = {
            "precipitation_probability_max",
            "precipitation_probability_min",
            "precipitation_probability_mean",
        }
        self.assertTrue(expected.issubset(compare.GFS_DAILY))
        self.assertTrue(expected.issubset(compare.ECMWF_DAILY))

    def test_cams_scope_excludes_local_only_chinese_and_daily_outputs(self) -> None:
        self.assertEqual(compare.CAMS_HOURLY_LOCAL, compare.CAMS_RAW)
        self.assertTrue(
            set(compare.CAMS_HOURLY_LOCAL).issubset(compare.CAMS_HOURLY_OFFICIAL)
        )
        self.assertTrue(
            set(compare.CAMS_OFFICIAL_DERIVED).isdisjoint(compare.CAMS_HOURLY_LOCAL)
        )
        self.assertEqual(compare.CAMS_DAILY, ())
        self.assertNotIn("chinese_aqi", compare.CAMS_HOURLY_LOCAL)

    def test_cams_source_proof_includes_greenhouse_gas_domain(self) -> None:
        self.assertEqual(
            compare.MODEL_SPECS["cams"]["source_probe_domains"],
            ("cams_global", "cams_global_greenhouse_gases"),
        )

    def test_weather_scope_excludes_all_pressure_level_fields(self) -> None:
        self.assertFalse(any("hPa" in variable for variable in compare.GFS_HOURLY))
        self.assertFalse(
            any("hPa" in variable for variable in compare.ECMWF_SURFACE_HOURLY)
        )

    def test_cams_direct_comparison_does_not_require_daily_period(self) -> None:
        original = compare.MODEL_SPECS["cams"]
        compare.MODEL_SPECS["cams"] = {
            **original,
            "official_hourly": ("pm10",),
            "daily": (),
        }
        try:
            payload = {"hourly": {"time": ["a"], "pm10": [1.0]}}
            difference, hourly_count, daily_count = compare.first_direct_difference(
                "cams", payload, payload
            )
            self.assertIsNone(difference)
            self.assertEqual(hourly_count, 1)
            self.assertEqual(daily_count, 0)
        finally:
            compare.MODEL_SPECS["cams"] = original

    def test_official_payload_uses_one_hundred_location_batch(self) -> None:
        payload = compare.official_payload(
            "gfs", compare.sample_points()[: compare.OFFICIAL_BATCH_SIZE]
        )
        self.assertEqual(len(payload["latitude"]), 100)
        self.assertEqual(len(payload["longitude"]), 100)
        self.assertEqual(payload["cell_selection"], "nearest")
        self.assertIn("precipitation_probability", payload["hourly"])
        self.assertIn("precipitation_probability_max", payload["daily"])

    def test_local_request_plan_pairs_hourly_and_daily_chunks(self) -> None:
        original = compare.MODEL_SPECS["gfs"]
        compare.MODEL_SPECS["gfs"] = {
            **original,
            "local_hourly": ("h1", "h2", "h3"),
            "daily": ("d1", "d2"),
        }
        try:
            plan = compare.request_plan("gfs", 2)
        finally:
            compare.MODEL_SPECS["gfs"] = original

        self.assertEqual(
            plan,
            [
                {"hourly": ("h1", "h2"), "daily": ("d1", "d2")},
                {"hourly": ("h3",), "daily": ()},
            ],
        )

    def test_default_plan_coalesces_each_model_period_without_parallelism(self) -> None:
        for model in ("gfs", "ec", "ec9"):
            plan = compare.request_plan(model, compare.DEFAULT_FIELD_CHUNK_SIZE)
            self.assertEqual(len(plan), 1)
            self.assertTrue(plan[0]["hourly"])
            self.assertTrue(plan[0]["daily"])
        cams_plan = compare.request_plan("cams", compare.DEFAULT_FIELD_CHUNK_SIZE)
        self.assertEqual(len(cams_plan), 1)
        self.assertTrue(cams_plan[0]["hourly"])
        self.assertEqual(cams_plan[0]["daily"], ())

    def test_progress_estimate_reports_live_eta(self) -> None:
        report: dict[str, object] = {}
        with mock.patch.object(compare.time, "monotonic", return_value=110.0):
            compare.update_progress_estimate(
                report,
                started_monotonic=100.0,
                request_units_completed=2,
                request_units_total=10,
                timed_request_units_completed=2,
            )
        self.assertEqual(report["elapsed_seconds"], 10.0)
        self.assertEqual(report["estimated_remaining_seconds"], 40.0)
        self.assertEqual(report["request_units_completed"], 2)
        self.assertEqual(report["request_units_total"], 10)
        self.assertEqual(report["request_units_reused"], 0)
        self.assertIsNotNone(report["estimated_finish_at"])

    def test_progress_estimate_excludes_reused_receipts_from_throughput(self) -> None:
        report: dict[str, object] = {}
        with mock.patch.object(compare.time, "monotonic", return_value=110.0):
            compare.update_progress_estimate(
                report,
                started_monotonic=100.0,
                request_units_completed=6,
                request_units_total=10,
                timed_request_units_completed=1,
            )
        self.assertEqual(report["request_units_reused"], 5)
        self.assertEqual(report["request_units_executed_this_attempt"], 1)
        self.assertEqual(report["estimated_remaining_seconds"], 40.0)

    def test_official_snapshot_rows_are_streamed_from_top_level_array(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = Path(temporary_directory) / "response.json"
            path.write_text(
                '[{"id": 1, "value": "a"}, {"id": 2, "value": "b"}]\n',
                encoding="utf-8",
            )
            rows = list(compare.iter_json_array_file(path, 2, chunk_size=7))

        self.assertEqual(rows, [{"id": 1, "value": "a"}, {"id": 2, "value": "b"}])

    def test_official_snapshot_stream_rejects_wrong_row_count(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = Path(temporary_directory) / "response.json"
            path.write_text('[{"id": 1}]', encoding="utf-8")
            with self.assertRaisesRegex(compare.ValidationError, "expected=2, actual=1"):
                list(compare.iter_json_array_file(path, 2, chunk_size=3))

    def test_official_snapshot_stream_rejects_missing_separator(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = Path(temporary_directory) / "response.json"
            path.write_text('[{"id": 1} {"id": 2}]', encoding="utf-8")
            with self.assertRaisesRegex(compare.ValidationError, "array separator"):
                list(compare.iter_json_array_file(path, 2, chunk_size=4))

    def test_sha256_file_matches_bytes_digest(self) -> None:
        raw = b"immutable-official-snapshot"
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = Path(temporary_directory) / "response.json"
            path.write_bytes(raw)
            digest = compare.sha256_file(path, chunk_size=4)

        self.assertEqual(digest, compare.sha256_bytes(raw))

    def test_local_url_can_request_one_period_chunk(self) -> None:
        url = compare.local_url(
            "http://127.0.0.1:8088",
            "gfs",
            compare.sample_points()[0],
            hourly=("temperature_2m", "precipitation_probability"),
            daily=(),
        )
        self.assertIn(
            "hourly=temperature_2m,precipitation_probability",
            url,
        )
        self.assertNotIn("daily=", url)

    def test_local_url_round_trips_native_float32_coordinates(self) -> None:
        point = {
            "latitude": 29.982425689697266,
            "longitude": 80.03496551513672,
        }

        url = compare.local_url(
            "http://127.0.0.1:8088",
            "ec9",
            point,
            hourly=("temperature_2m",),
            daily=(),
        )
        query = compare.urllib.parse.parse_qs(compare.urllib.parse.urlparse(url).query)

        self.assertEqual(query["latitude"], ["29.9824257"])
        self.assertEqual(query["longitude"], ["80.0349655"])

    def test_local_url_freezes_saved_daily_snapshot_range(self) -> None:
        url = compare.local_url(
            "http://127.0.0.1:8088",
            "ec",
            compare.sample_points()[0],
            hourly=(),
            daily=("temperature_2m_max",),
            daily_time_range=("2026-07-29", "2026-08-12"),
        )

        self.assertIn("start_date=2026-07-29", url)
        self.assertIn("end_date=2026-08-12", url)
        self.assertNotIn("forecast_days=", url)

    def test_resource_guard_refuses_a_third_local_api(self) -> None:
        with (
            mock.patch.object(compare, "local_om_api_process_count", return_value=3),
            self.assertRaisesRegex(compare.ValidationError, "3 om-api processes"),
        ):
            compare.wait_for_safe_local_resources(
                local_base="http://127.0.0.1:18089",
                min_available_memory_mib=0,
                max_io_full_pressure_avg10=100,
                max_local_om_api_processes=2,
                wait_timeout_seconds=0,
                poll_seconds=0.01,
            )

    def test_existing_full_manifest_allows_model_subset(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = Path(temporary_directory) / "manifest.json"
            compare.ensure_validation_manifest(path, ["gfs", "ec", "cams"])
            original = path.read_bytes()
            compare.ensure_validation_manifest(path, ["gfs"])
            self.assertEqual(path.read_bytes(), original)

    def test_existing_manifest_rejects_unplanned_model(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = Path(temporary_directory) / "manifest.json"
            compare.ensure_validation_manifest(path, ["gfs"])
            with self.assertRaisesRegex(
                compare.ValidationError, "requested models absent"
            ):
                compare.ensure_validation_manifest(path, ["ec"])

    def test_existing_manifest_still_rejects_tampering(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_directory:
            path = Path(temporary_directory) / "manifest.json"
            compare.ensure_validation_manifest(path, ["gfs", "ec", "cams"])
            manifest = json.loads(path.read_text(encoding="utf-8"))
            manifest["points"][0]["latitude"] += 0.25
            path.write_bytes(compare.pretty_bytes(manifest))
            with self.assertRaisesRegex(compare.ValidationError, "immutable artifact"):
                compare.ensure_validation_manifest(path, ["gfs"])

    def test_capture_sends_apikey_in_post_body_without_persisting_it(self) -> None:
        captured: dict[str, object] = {}
        response = json.dumps([{} for _ in range(100)]).encode()

        def fake_request(method, url, **kwargs):
            captured.update(method=method, url=url, **kwargs)
            return response, {}, 0.01

        with tempfile.TemporaryDirectory() as temporary_directory:
            output = Path(temporary_directory)
            with mock.patch.object(compare, "request_json", side_effect=fake_request):
                metadata = compare.capture_official(
                    "gfs", output, "commercial-secret", 10.0, 0
                )

            wire_payload = json.loads(captured["body"])
            persisted_payload = json.loads(
                (output / "gfs" / "official" / "request.json").read_text(
                    encoding="utf-8"
                )
            )
            persisted_metadata = json.loads(
                (output / "gfs" / "official" / "metadata.json").read_text(
                    encoding="utf-8"
                )
            )

        self.assertEqual(captured["method"], "POST")
        self.assertEqual(
            captured["url"], "https://customer-api.open-meteo.com/v1/gfs"
        )
        self.assertEqual(wire_payload["apikey"], "commercial-secret")
        self.assertNotIn("X-Api-Key", captured["headers"])
        self.assertNotIn("apikey", persisted_payload)
        self.assertNotIn("commercial-secret", json.dumps(persisted_metadata))
        self.assertFalse(metadata["api_key_persisted"])
        self.assertEqual(metadata["api_access_tier"], "customer_commercial")
        self.assertEqual(metadata["official_request_count"], 2)
        self.assertEqual(metadata["official_batch_size"], 100)
        self.assertEqual(len(metadata["batches"]), 2)

    def test_capture_without_key_uses_public_noncommercial_endpoint(self) -> None:
        captured: dict[str, object] = {}
        response = json.dumps([{} for _ in range(100)]).encode()

        def fake_request(method, url, **kwargs):
            captured.update(method=method, url=url, **kwargs)
            return response, {}, 0.01

        with tempfile.TemporaryDirectory() as temporary_directory:
            with mock.patch.object(compare, "request_json", side_effect=fake_request):
                metadata = compare.capture_official(
                    "gfs", Path(temporary_directory), None, 10.0, 0
                )

        self.assertEqual(captured["url"], "https://api.open-meteo.com/v1/gfs")
        self.assertNotIn("apikey", json.loads(captured["body"]))
        self.assertEqual(metadata["api_access_tier"], "public_noncommercial")
        self.assertEqual(metadata["api_key_transport"], "none")
        self.assertEqual(metadata["official_request_count"], 2)

    def test_capture_resumes_a_complete_partial_batch_without_requesting_it(self) -> None:
        response = json.dumps([{} for _ in range(100)]).encode()
        with tempfile.TemporaryDirectory() as temporary_directory:
            output = Path(temporary_directory)
            official_dir = output / "gfs" / "official"
            payload = compare.official_payload("gfs", compare.sample_points()[:100])
            compare.write_once(
                official_dir / "request-000.json", compare.pretty_bytes(payload)
            )
            compare.write_once(official_dir / "response-000.json", response)
            with mock.patch.object(
                compare,
                "request_json",
                return_value=(response, {}, 0.01),
            ) as request:
                metadata = compare.capture_official(
                    "gfs", output, None, 10.0, 0
                )

        self.assertEqual(request.call_count, 1)
        self.assertTrue(metadata["batches"][0]["resumed_from_disk"])
        self.assertEqual(metadata["batches"][0]["request_exit"], "persisted")
        self.assertFalse(metadata["batches"][1]["resumed_from_disk"])

    def test_capture_routes_public_batches_through_distinct_ssh_hosts(self) -> None:
        response = json.dumps([{} for _ in range(100)]).encode()
        with tempfile.TemporaryDirectory() as temporary_directory:
            with (
                mock.patch.object(
                    compare,
                    "request_json_via_ssh",
                    return_value=(response, {}, 0.01),
                ) as ssh_request,
                mock.patch.object(compare, "request_json") as local_request,
            ):
                metadata = compare.capture_official(
                    "gfs",
                    Path(temporary_directory),
                    None,
                    10.0,
                    0,
                    ssh_hosts=("first-exit", "second-exit"),
                )

        self.assertEqual(ssh_request.call_count, 2)
        local_request.assert_not_called()
        self.assertEqual(
            [batch["request_exit"] for batch in metadata["batches"]],
            ["ssh:first-exit", "ssh:second-exit"],
        )

    def test_direct_comparison_stops_at_first_value(self) -> None:
        original = compare.MODEL_SPECS["gfs"]
        compare.MODEL_SPECS["gfs"] = {
            **original,
            "official_hourly": ("temperature_2m",),
            "daily": ("temperature_2m_max",),
        }
        try:
            official = {
                "hourly": {"time": ["a", "b"], "temperature_2m": [1, 2]},
                "daily": {"time": ["d"], "temperature_2m_max": [2]},
            }
            local = {
                "hourly": {"time": ["a", "b"], "temperature_2m": [1, 3]},
                "daily": {"time": ["d"], "temperature_2m_max": [2]},
            }
            difference, _, _ = compare.first_direct_difference("gfs", official, local)
            self.assertEqual(difference["period"], "hourly")
            self.assertEqual(difference["variable"], "temperature_2m")
            self.assertEqual(difference["index"], 1)
        finally:
            compare.MODEL_SPECS["gfs"] = original

    def test_direct_comparison_ignores_local_history_outside_official_axis(self) -> None:
        original = compare.MODEL_SPECS["gfs"]
        compare.MODEL_SPECS["gfs"] = {
            **original,
            "official_hourly": ("temperature_2m",),
            "daily": (),
        }
        try:
            official = {
                "hourly": {"time": ["b", "c"], "temperature_2m": [2, 3]},
                "daily": {"time": []},
            }
            local = {
                "hourly": {
                    "time": ["a", "b", "c"],
                    "temperature_2m": [1, 2, 3],
                },
                "daily": {"time": ["older"]},
            }
            difference, hourly_count, daily_count = compare.first_direct_difference(
                "gfs", official, local
            )
            self.assertIsNone(difference)
            self.assertEqual(hourly_count, 2)
            self.assertEqual(daily_count, 0)
        finally:
            compare.MODEL_SPECS["gfs"] = original

    def test_direct_comparison_ignores_official_tail_after_raw_model_end(self) -> None:
        original = compare.MODEL_SPECS["gfs"]
        compare.MODEL_SPECS["gfs"] = {
            **original,
            "official_hourly": ("temperature_2m",),
            "daily": (),
        }
        try:
            official = {
                "hourly": {"time": ["b", "c"], "temperature_2m": [2, 3]},
                "daily": {"time": []},
            }
            local = {
                "hourly": {"time": ["a", "b"], "temperature_2m": [1, 2]},
                "daily": {"time": []},
            }
            difference, hourly_count, daily_count = compare.first_direct_difference(
                "gfs", official, local
            )
            self.assertIsNone(difference)
            self.assertEqual(hourly_count, 1)
            self.assertEqual(daily_count, 0)
        finally:
            compare.MODEL_SPECS["gfs"] = original

    def test_direct_comparison_rejects_missing_hour_inside_raw_model_axis(self) -> None:
        original = compare.MODEL_SPECS["gfs"]
        compare.MODEL_SPECS["gfs"] = {
            **original,
            "official_hourly": ("temperature_2m",),
            "daily": (),
        }
        try:
            official = {
                "hourly": {
                    "time": ["a", "b", "c"],
                    "temperature_2m": [1, 2, 3],
                },
                "daily": {"time": []},
            }
            local = {
                "hourly": {"time": ["a", "c"], "temperature_2m": [1, 3]},
                "daily": {"time": []},
            }
            difference, _, _ = compare.first_direct_difference(
                "gfs", official, local
            )
            self.assertEqual(difference["reason"], "missing_official_time")
            self.assertEqual(difference["time"], "b")
        finally:
            compare.MODEL_SPECS["gfs"] = original

    def test_daily_comparison_always_excludes_final_official_day(self) -> None:
        official = {
            "daily": {
                "time": ["2026-08-10", "2026-08-11"],
                "temperature_2m_max": [2, 99],
            }
        }
        local = {
            "daily": {
                "time": ["2026-08-10", "2026-08-11"],
                "temperature_2m_max": [2, 3],
            }
        }
        difference, hourly_count, daily_count = compare.first_period_difference(
            "daily", ("temperature_2m_max",), official, local
        )
        self.assertIsNone(difference)
        self.assertEqual(hourly_count, 0)
        self.assertEqual(daily_count, 1)

    def test_daily_comparison_still_rejects_difference_before_final_day(self) -> None:
        official = {
            "daily": {
                "time": ["2026-08-10", "2026-08-11"],
                "temperature_2m_max": [2, 99],
            }
        }
        local = {
            "daily": {
                "time": ["2026-08-10", "2026-08-11"],
                "temperature_2m_max": [3, 99],
            }
        }
        difference, _, _ = compare.first_period_difference(
            "daily", ("temperature_2m_max",), official, local
        )
        self.assertEqual(difference["reason"], "json_value")
        self.assertEqual(difference["time"], "2026-08-10")

    def test_hourly_comparison_can_record_official_rolling_value_over_local_nan(
        self,
    ) -> None:
        official = {
            "hourly": {
                "time": ["2026-09-04T02:00", "2026-09-04T03:00"],
                "convective_inhibition": [29, 11],
            }
        }
        local = {
            "hourly": {
                "time": ["2026-09-04T02:00", "2026-09-04T03:00"],
                "convective_inhibition": [29, None],
            }
        }
        accepted: list[dict[str, object]] = []

        difference, hourly_count, daily_count = compare.first_period_difference(
            "hourly",
            ("convective_inhibition",),
            official,
            local,
            allow_official_finite_local_nan=True,
            accepted_differences=accepted,
        )

        self.assertIsNone(difference)
        self.assertEqual(hourly_count, 2)
        self.assertEqual(daily_count, 0)
        self.assertEqual(
            accepted,
            [
                {
                    "period": "hourly",
                    "variable": "convective_inhibition",
                    "reason": "official_rolling_value_over_local_nan",
                    "index": 1,
                    "time": "2026-09-04T03:00",
                    "official": 11,
                    "local": None,
                }
            ],
        )

    def test_hourly_rolling_nan_policy_does_not_accept_other_differences(self) -> None:
        official = {
            "hourly": {
                "time": ["2026-09-04T03:00"],
                "convective_inhibition": [11],
            }
        }
        local_numeric = {
            "hourly": {
                "time": ["2026-09-04T03:00"],
                "convective_inhibition": [12],
            }
        }
        local_finite_over_official_nan = {
            "hourly": {
                "time": ["2026-09-04T03:00"],
                "convective_inhibition": [11],
            }
        }
        official_nan = {
            "hourly": {
                "time": ["2026-09-04T03:00"],
                "convective_inhibition": [None],
            }
        }

        numeric_difference, _, _ = compare.first_period_difference(
            "hourly",
            ("convective_inhibition",),
            official,
            local_numeric,
            allow_official_finite_local_nan=True,
        )
        reverse_difference, _, _ = compare.first_period_difference(
            "hourly",
            ("convective_inhibition",),
            official_nan,
            local_finite_over_official_nan,
            allow_official_finite_local_nan=True,
        )

        self.assertEqual(numeric_difference["reason"], "json_value")
        self.assertEqual(reverse_difference["reason"], "json_value")

    def test_hourly_weather_code_can_record_proven_rolling_cin_effect(self) -> None:
        official = {
            "latitude": 32.021087646484375,
            "hourly": {
                "time": ["2026-09-08T05:00"],
                "cloud_cover": [89],
                "precipitation": [0.2],
                "snowfall": [0.0],
                "cape": [1370],
                "showers": [0.2],
                "convective_inhibition": [1],
                "boundary_layer_height": [1565],
                "weather_code": [95],
            },
        }
        local = {
            "latitude": 32.021087646484375,
            "hourly": {
                "time": ["2026-09-08T05:00"],
                "cloud_cover": [89],
                "precipitation": [0.2],
                "snowfall": [0.0],
                "cape": [1370],
                "showers": [0.2],
                "convective_inhibition": [None],
                "boundary_layer_height": [1565],
                "weather_code": [51],
            },
        }
        accepted: list[dict[str, object]] = []

        difference, hourly_count, daily_count = compare.first_period_difference(
            "hourly",
            (
                "weather_code",
                "cloud_cover",
                "precipitation",
                "snowfall",
                "cape",
                "showers",
                "convective_inhibition",
                "boundary_layer_height",
            ),
            official,
            local,
            allow_official_finite_local_nan=True,
            accepted_differences=accepted,
        )

        self.assertIsNone(difference)
        self.assertEqual(hourly_count, 8)
        self.assertEqual(daily_count, 0)
        self.assertEqual(len(accepted), 2)
        self.assertEqual(
            accepted[0]["reason"],
            "official_weather_code_derived_from_rolling_value_over_local_nan",
        )
        self.assertEqual(
            accepted[0]["evidence"]["rolling_nan_dependencies"],
            [
                {
                    "variable": "convective_inhibition",
                    "official": 1,
                    "local": None,
                }
            ],
        )
        self.assertEqual(accepted[0]["evidence"]["recomputed_official"], 95)
        self.assertEqual(accepted[0]["evidence"]["recomputed_local"], 51)
        self.assertEqual(
            accepted[1]["reason"], "official_rolling_value_over_local_nan"
        )

    def test_weather_code_rolling_evidence_uses_prior_field_chunk(self) -> None:
        first_chunk = {
            "latitude": 32.021087646484375,
            "hourly": {
                "time": ["2026-09-16T07:00"],
                "cloud_cover": [75],
                "precipitation": [0.8],
                "snowfall": [0.0],
                "cape": [760],
            },
        }
        second_chunk = {
            "latitude": 32.021087646484375,
            "hourly": {
                "time": ["2026-09-16T07:00"],
                "showers": [0.7],
                "convective_inhibition": [None],
                "boundary_layer_height": [1180],
                "weather_code": [53],
            },
        }
        local = compare.merge_local_response_periods([first_chunk, second_chunk])
        official = {
            **local,
            "hourly": {
                **local["hourly"],
                "convective_inhibition": [8],
                "weather_code": [95],
            },
        }
        accepted: list[dict[str, object]] = []

        difference, hourly_count, _ = compare.first_period_difference(
            "hourly",
            ("weather_code", "convective_inhibition"),
            official,
            local,
            allow_official_finite_local_nan=True,
            accepted_differences=accepted,
        )

        self.assertIsNone(difference)
        self.assertEqual(hourly_count, 2)
        self.assertEqual(
            [item["reason"] for item in accepted],
            [
                "official_weather_code_derived_from_rolling_value_over_local_nan",
                "official_rolling_value_over_local_nan",
            ],
        )

    def test_hourly_weather_code_rolling_nan_policy_requires_causal_match(self) -> None:
        official = {
            "latitude": 32.021087646484375,
            "hourly": {
                "time": ["2026-09-08T05:00"],
                "cloud_cover": [89],
                "precipitation": [0.2],
                "snowfall": [0.0],
                "cape": [1370],
                "showers": [0.2],
                "convective_inhibition": [1],
                "boundary_layer_height": [1565],
                "weather_code": [99],
            },
        }
        local = {
            "latitude": 32.021087646484375,
            "hourly": {
                **official["hourly"],
                "convective_inhibition": [None],
                "weather_code": [51],
            },
        }

        difference, _, _ = compare.first_period_difference(
            "hourly",
            ("weather_code",),
            official,
            local,
            allow_official_finite_local_nan=True,
        )

        self.assertEqual(difference["reason"], "json_value")

    def test_daily_weather_code_can_record_proven_hourly_rolling_cin_effect(self) -> None:
        official = {
            "latitude": 29.982425689697266,
            "hourly": {
                "time": ["2026-09-14T07:00", "2026-09-14T08:00"],
                "cloud_cover": [95, 30],
                "precipitation": [1.4, 0.0],
                "snowfall": [0.0, 0.0],
                "cape": [790, 0],
                "showers": [1.1, 0.0],
                "convective_inhibition": [0, 12],
                "boundary_layer_height": [840, 100],
                "weather_code": [95, 1],
            },
            "daily": {"time": ["2026-09-14"], "weather_code": [95]},
        }
        local = {
            "latitude": 29.982425689697266,
            "hourly": {
                **official["hourly"],
                "convective_inhibition": [None, None],
                "weather_code": [80, 1],
            },
            "daily": {"time": ["2026-09-14"], "weather_code": [80]},
        }
        accepted: list[dict[str, object]] = []

        difference, hourly_count, daily_count = compare.first_period_difference(
            "daily",
            ("weather_code",),
            official,
            local,
            allow_official_finite_local_nan=True,
            accepted_differences=accepted,
        )

        self.assertIsNone(difference)
        self.assertEqual(hourly_count, 0)
        # The final daily item is outside the normal comparison scope. Add a
        # second day so the first one is actually compared.
        self.assertEqual(daily_count, 0)
        self.assertEqual(accepted, [])

        official["daily"]["time"].append("2026-09-15")
        official["daily"]["weather_code"].append(1)
        local["daily"]["time"].append("2026-09-15")
        local["daily"]["weather_code"].append(1)
        difference, _, daily_count = compare.first_period_difference(
            "daily",
            ("weather_code",),
            official,
            local,
            allow_official_finite_local_nan=True,
            accepted_differences=accepted,
        )

        self.assertIsNone(difference)
        self.assertEqual(daily_count, 1)
        self.assertEqual(len(accepted), 1)
        self.assertEqual(
            accepted[0]["reason"],
            "official_daily_weather_code_derived_from_rolling_value_over_local_nan",
        )
        self.assertEqual(
            accepted[0]["evidence"]["causal_hourly_differences"][0]["time"],
            "2026-09-14T07:00",
        )

    def test_validate_model_checkpoints_progress_and_throttles(self) -> None:
        original = compare.MODEL_SPECS["gfs"]
        compare.MODEL_SPECS["gfs"] = {
            **original,
            "official_hourly": ("temperature_2m",),
            "local_hourly": ("temperature_2m",),
            "daily": (),
        }
        response = {
            "hourly": {
                "time": ["2026-07-29T00:00"],
                "temperature_2m": [25.0],
            }
        }
        official_raw = compare.canonical_bytes([response] * compare.POINT_COUNT)
        local_raw = compare.canonical_bytes(response)
        try:
            with tempfile.TemporaryDirectory() as temporary_directory:
                output = Path(temporary_directory)
                official_dir = output / "gfs" / "official"
                official_dir.mkdir(parents=True)
                official_dir.joinpath("response.json").write_bytes(official_raw)
                official_dir.joinpath("metadata.json").write_bytes(
                    compare.pretty_bytes(
                        {
                            "response_sha256": compare.sha256_bytes(official_raw),
                            "official_request_count": 1,
                        }
                    )
                )
                stdout = io.StringIO()
                with (
                    mock.patch.object(
                        compare,
                        "request_json",
                        return_value=(local_raw, {}, 0.01),
                    ) as request,
                    mock.patch.object(compare.time, "sleep") as sleep,
                    contextlib.redirect_stdout(stdout),
                ):
                    report = compare.validate_model(
                        "gfs",
                        output,
                        "http://127.0.0.1:8088",
                        10.0,
                        0,
                        0.25,
                    )

            self.assertEqual(request.call_count, compare.POINT_COUNT)
            self.assertEqual(sleep.call_count, compare.POINT_COUNT - 1)
            self.assertEqual(report["status"], "passed")
            self.assertEqual(report["points_completed"], compare.POINT_COUNT)
            self.assertEqual(
                report["request_units_completed"],
                report["request_units_total"],
            )
            self.assertEqual(report["estimated_remaining_seconds"], 0.0)
            self.assertIsNone(report["current_point"])
            self.assertEqual(report["point_delay_seconds"], 0.25)
            self.assertIn("started_at", report)
            events = [json.loads(line) for line in stdout.getvalue().splitlines()]
            self.assertEqual(events[0]["event"], "point_started")
            self.assertEqual(events[-1]["event"], "point_passed")
        finally:
            compare.MODEL_SPECS["gfs"] = original

    def test_validate_model_uses_paired_attempt_scoped_field_chunks(self) -> None:
        original = compare.MODEL_SPECS["gfs"]
        compare.MODEL_SPECS["gfs"] = {
            **original,
            "official_hourly": ("h1", "h2"),
            "local_hourly": ("h1", "h2"),
            "daily": ("d1",),
        }
        response = {
            "hourly": {"time": ["h"], "h1": [1], "h2": [2]},
            "daily": {"time": ["d"], "d1": [3]},
        }
        official_raw = compare.canonical_bytes([response] * compare.POINT_COUNT)

        def fake_request(_method, url, **_kwargs):
            if "hourly=h1" in url:
                self.assertIn("daily=d1", url)
                value = {
                    "hourly": {"time": ["h"], "h1": [1]},
                    "daily": {"time": ["d"], "d1": [3]},
                }
            elif "hourly=h2" in url:
                value = {"hourly": {"time": ["h"], "h2": [2]}}
            else:
                self.fail(f"unexpected URL: {url}")
            return compare.canonical_bytes(value), {}, 0.01

        try:
            with tempfile.TemporaryDirectory() as temporary_directory:
                output = Path(temporary_directory)
                official_dir = output / "gfs" / "official"
                official_dir.mkdir(parents=True)
                official_dir.joinpath("response.json").write_bytes(official_raw)
                official_dir.joinpath("metadata.json").write_bytes(
                    compare.pretty_bytes(
                        {
                            "response_sha256": compare.sha256_bytes(official_raw),
                            "official_request_count": 1,
                        }
                    )
                )
                with (
                    mock.patch.object(compare, "request_json", side_effect=fake_request)
                    as request,
                    mock.patch.object(compare, "wait_for_safe_local_resources")
                    as resource_guard,
                    contextlib.redirect_stdout(io.StringIO()),
                ):
                    resource_guard.return_value = {
                        "available_memory_mib": 2048,
                        "io_full_pressure_avg10": 0,
                        "local_om_api_processes": 2,
                    }
                    report = compare.validate_model(
                        "gfs",
                        output,
                        "http://127.0.0.1:8088",
                        10.0,
                        0,
                        0,
                        field_chunk_size=1,
                        request_delay_seconds=0,
                        attempt_id="attempt-a",
                        point_limit=1,
                    )
                    replay_report = compare.validate_model(
                        "gfs",
                        output,
                        "http://127.0.0.1:8088",
                        10.0,
                        0,
                        0,
                        field_chunk_size=1,
                        request_delay_seconds=0,
                        attempt_id="attempt-b",
                        point_limit=1,
                    )
                receipt = json.loads(
                    (
                        output
                        / "gfs"
                        / "receipts"
                        / "attempts"
                        / "attempt-a"
                        / "000_p000.json"
                    ).read_text(
                        encoding="utf-8"
                    )
                )
                local_parts = sorted(
                    (
                        output
                        / "gfs"
                        / "local"
                        / "attempts"
                        / "attempt-a"
                        / "000_p000"
                    ).glob("*.json")
                )
                replay_receipt = (
                    output
                    / "gfs"
                    / "receipts"
                    / "attempts"
                    / "attempt-b"
                    / "000_p000.json"
                )
                replay_receipt_exists = replay_receipt.exists()

            self.assertEqual(request.call_count, 4)
            self.assertEqual(resource_guard.call_count, 4)
            self.assertEqual(report["status"], "partial")
            self.assertEqual(replay_report["status"], "partial")
            self.assertEqual(report["points_target"], 1)
            self.assertEqual(report["local_requests_per_point"], 2)
            self.assertEqual(report["local_requests_completed"], 2)
            self.assertEqual(receipt["local_request_count"], 2)
            self.assertEqual(len(receipt["local_response_parts"]), 2)
            self.assertEqual(len(local_parts), 2)
            self.assertTrue(replay_receipt_exists)
        finally:
            compare.MODEL_SPECS["gfs"] = original

    def test_validate_model_can_query_the_production_api_through_ssh(self) -> None:
        original = compare.MODEL_SPECS["gfs"]
        compare.MODEL_SPECS["gfs"] = {
            **original,
            "official_hourly": ("temperature_2m",),
            "local_hourly": ("temperature_2m",),
            "daily": (),
        }
        response = {
            "hourly": {
                "time": ["2026-08-02T00:00"],
                "temperature_2m": [25.0],
            }
        }
        official_raw = compare.canonical_bytes([response] * compare.POINT_COUNT)
        local_raw = compare.canonical_bytes(response)
        try:
            with tempfile.TemporaryDirectory() as temporary_directory:
                output = Path(temporary_directory)
                official_dir = output / "gfs" / "official"
                official_dir.mkdir(parents=True)
                official_dir.joinpath("response.json").write_bytes(official_raw)
                official_dir.joinpath("metadata.json").write_bytes(
                    compare.pretty_bytes(
                        {
                            "response_sha256": compare.sha256_bytes(official_raw),
                            "official_request_count": 2,
                        }
                    )
                )
                ssh_client = mock.Mock(ssh_host="singapore")
                ssh_client.request.return_value = (local_raw, {}, 0.01)
                with (
                    mock.patch.object(compare, "request_json") as direct_request,
                    mock.patch.object(compare, "wait_for_safe_local_resources") as guard,
                    contextlib.redirect_stdout(io.StringIO()),
                ):
                    report = compare.validate_model(
                        "gfs",
                        output,
                        "http://127.0.0.1:8088",
                        10.0,
                        0,
                        0,
                        point_limit=1,
                        attempt_id="production-ssh",
                        local_ssh_client=ssh_client,
                    )

            self.assertEqual(ssh_client.request.call_count, 1)
            self.assertIn("127.0.0.1:8088", ssh_client.request.call_args.args[0])
            direct_request.assert_not_called()
            guard.assert_not_called()
            self.assertEqual(report["local_request_transport"], "production_ssh:singapore")
        finally:
            compare.MODEL_SPECS["gfs"] = original

    def test_validate_model_reuses_a_separate_immutable_snapshot_root(self) -> None:
        original = compare.MODEL_SPECS["gfs"]
        compare.MODEL_SPECS["gfs"] = {
            **original,
            "official_hourly": ("temperature_2m",),
            "local_hourly": ("temperature_2m",),
            "daily": (),
        }
        response = {
            "hourly": {
                "time": ["2026-08-02T00:00"],
                "temperature_2m": [25.0],
            }
        }
        official_raw = compare.canonical_bytes([response] * compare.POINT_COUNT)
        try:
            with tempfile.TemporaryDirectory() as temporary_directory:
                root = Path(temporary_directory)
                snapshot_root = root / "official-capture"
                output = root / "singapore-validation"
                official_dir = snapshot_root / "gfs" / "official"
                official_dir.mkdir(parents=True)
                official_dir.joinpath("response.json").write_bytes(official_raw)
                official_dir.joinpath("metadata.json").write_bytes(
                    compare.pretty_bytes(
                        {
                            "response_sha256": compare.sha256_bytes(official_raw),
                            "official_request_count": 2,
                        }
                    )
                )
                with (
                    mock.patch.object(
                        compare,
                        "request_json",
                        return_value=(compare.canonical_bytes(response), {}, 0.01),
                    ),
                    contextlib.redirect_stdout(io.StringIO()),
                ):
                    report = compare.validate_model(
                        "gfs",
                        output,
                        "http://127.0.0.1:8088",
                        10.0,
                        0,
                        0,
                        point_limit=1,
                        official_snapshot_root=snapshot_root,
                    )

                self.assertTrue((output / "gfs" / "report.json").exists())
                self.assertFalse((output / "gfs" / "official").exists())
                self.assertEqual(
                    report["official_snapshot_root"], str(snapshot_root.resolve())
                )
        finally:
            compare.MODEL_SPECS["gfs"] = original


if __name__ == "__main__":
    unittest.main()
