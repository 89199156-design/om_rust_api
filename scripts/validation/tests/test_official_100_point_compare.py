from __future__ import annotations

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "official_100_point_compare", ROOT / "official_100_point_compare.py"
)
assert SPEC is not None and SPEC.loader is not None
compare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compare)


class Official100PointCompareTests(unittest.TestCase):
    def test_plan_has_exactly_one_hundred_stable_points(self) -> None:
        points = compare.sample_points()
        self.assertEqual(len(points), 100)
        self.assertEqual(points, compare.sample_points())
        self.assertEqual(len({point["id"] for point in points}), 100)
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
            35,
        )
        self.assertEqual(
            sum(point["kind"] != "random_exact_common_native_grid" for point in points),
            65,
        )

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

    def test_official_payload_uses_one_multi_location_request(self) -> None:
        payload = compare.official_payload("gfs", compare.sample_points())
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
        for model in ("gfs", "ec"):
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

    def test_direct_comparison_reports_missing_official_tail_time(self) -> None:
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
            difference, _, _ = compare.first_direct_difference(
                "gfs", official, local
            )
            self.assertEqual(difference["reason"], "missing_official_time")
            self.assertEqual(difference["time"], "c")
            self.assertEqual(difference["local_end"], "b")
        finally:
            compare.MODEL_SPECS["gfs"] = original

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
                receipt = json.loads(
                    (output / "gfs" / "receipts" / "000_p000.json").read_text(
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

            self.assertEqual(request.call_count, 2)
            self.assertEqual(resource_guard.call_count, 2)
            self.assertEqual(report["status"], "partial")
            self.assertEqual(report["points_target"], 1)
            self.assertEqual(report["local_requests_per_point"], 2)
            self.assertEqual(report["local_requests_completed"], 2)
            self.assertEqual(receipt["local_request_count"], 2)
            self.assertEqual(len(receipt["local_response_parts"]), 2)
            self.assertEqual(len(local_parts), 2)
        finally:
            compare.MODEL_SPECS["gfs"] = original


if __name__ == "__main__":
    unittest.main()
