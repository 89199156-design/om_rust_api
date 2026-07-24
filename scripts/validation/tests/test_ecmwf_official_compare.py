from __future__ import annotations

from contextlib import redirect_stdout
import datetime as dt
import io
import json
from pathlib import Path
from types import SimpleNamespace
import sys
import tempfile
import unittest
from unittest import mock
from typing import Callable
import urllib.parse


VALIDATION_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(VALIDATION_ROOT))

import ecmwf_official_compare as compare  # noqa: E402
import ecmwf_variable_catalog as catalog  # noqa: E402


RUN = "2026-07-23T00:00:00Z"
RUN_COMPACT = "2026072300"
SMALL_COUNTS = {
    "exact_native_grid": 1,
    "offgrid_nearest": 1,
    "offgrid_land": 1,
}


def boundary_fields() -> dict[str, object]:
    return {
        "coverage_plan": [
            {"source_run": "2026072212", "valid_time_utc": "2026-07-22T12:00:00Z", "forecast_hour": 0},
            {"source_run": "2026072212", "valid_time_utc": "2026-07-22T15:00:00Z", "forecast_hour": 3},
            {"source_run": "2026072218", "valid_time_utc": "2026-07-22T18:00:00Z", "forecast_hour": 0},
            {"source_run": "2026072218", "valid_time_utc": "2026-07-22T21:00:00Z", "forecast_hour": 3},
        ],
        "interpolation_support_records": [
            {
                "source_run": source_run,
                "valid_time_utc": valid_time,
                "forecast_hour": forecast_hour,
                "hidden": True,
                "right_support": True,
                "support_kind": "right_lookahead",
            }
            for source_run, valid_time, forecast_hour in (
                ("2026072212", "2026-07-22T18:00:00Z", 6),
                ("2026072212", "2026-07-22T21:00:00Z", 9),
                ("2026072218", "2026-07-23T00:00:00Z", 6),
                ("2026072218", "2026-07-23T03:00:00Z", 9),
            )
        ],
    }


def small_config() -> dict[str, object]:
    return {
        "schema_version": compare.SCHEMA_VERSION,
        "model": compare.MODEL,
        "official": {
            "endpoint": "http://127.0.0.1:19090/official",
            "public_endpoint": "https://api.open-meteo.com/v1/ecmwf",
            "source_probe_endpoint": "http://127.0.0.1:19090/source-meta",
            "spatial_probe_endpoint": "http://127.0.0.1:19090/spatial-latest",
            "multi_location_limit": 1000,
            "expected_request_count": 1,
            "public_request_weight_limit": 5000,
            "public_daily_weight_limit": 10000,
        },
        "local": {"endpoint": "http://127.0.0.1:19090/local"},
        "crop": {
            "longitude_min": 70.0,
            "longitude_max": 140.0,
            "latitude_min": 0.0,
            "latitude_max": 58.0,
        },
        "native_grid": {"resolution_degrees": 0.25},
        "sampling": {
            "point_count": 3,
            "seed": 20260723,
            "cohort_counts": dict(SMALL_COUNTS),
        },
        "horizon": {
            "forecast_hour_start": 0,
            "forecast_hour_end": 360,
            "hourly_frames": 361,
            "complete_daily_frames": 15,
        },
        "request_options": {
            "timezone": "GMT",
            "timeformat": "iso8601",
            "temperature_unit": "celsius",
            "wind_speed_unit": "ms",
            "precipitation_unit": "mm",
        },
        "ignored_dynamic_metadata": ["generationtime_ms", "location_id"],
        "rolling_hour0_inherited_variables": list(
            compare.ROLLING_HOUR0_VARIABLES
        ),
        "variables": {
            "hourly": ["temperature_2m"],
            "daily": ["temperature_2m_max"],
        },
    }


class FakeApi:
    def __init__(
        self,
        *,
        mismatch_local_call: int | None = None,
        mismatch_period: str = "daily",
        mismatch_frame: int = 14,
        sentinel_changes: bool = False,
        official_statuses: list[int] | None = None,
        mutate_after_local: Callable[[int], None] | None = None,
    ) -> None:
        self.mismatch_local_call = mismatch_local_call
        self.mismatch_period = mismatch_period
        self.mismatch_frame = mismatch_frame
        self.sentinel_changes = sentinel_changes
        self.official_statuses = list(official_statuses or [])
        self.mutate_after_local = mutate_after_local
        self.official_calls = 0
        self.local_calls = 0
        self.probe_calls = 0
        self.calls: list[dict[str, object]] = []
        self.sleeps: list[float] = []

    @staticmethod
    def _result(status: int, value: object, headers: dict[str, str] | None = None) -> compare.HttpResult:
        raw = (
            value
            if isinstance(value, bytes)
            else json.dumps(value, separators=(",", ":"), allow_nan=False).encode("utf-8")
        )
        return compare.HttpResult(
            status=status,
            raw=raw,
            headers=headers or {"content-type": "application/json"},
            elapsed_seconds=0.001,
        )

    @staticmethod
    def _axes(payload: dict[str, object]) -> tuple[list[str], list[str]]:
        start_hour = dt.datetime.fromisoformat(
            str(payload["start_hour"][0])
        ).replace(tzinfo=dt.timezone.utc)
        end_hour = dt.datetime.fromisoformat(
            str(payload["end_hour"][0])
        ).replace(tzinfo=dt.timezone.utc)
        hours: list[str] = []
        cursor = start_hour
        while cursor <= end_hour:
            hours.append(cursor.strftime("%Y-%m-%dT%H:%M"))
            cursor += dt.timedelta(hours=1)
        start_date = dt.date.fromisoformat(str(payload["start_date"][0]))
        end_date = dt.date.fromisoformat(str(payload["end_date"][0]))
        days: list[str] = []
        day = start_date
        while day <= end_date:
            days.append(day.isoformat())
            day += dt.timedelta(days=1)
        return hours, days

    def _row(
        self,
        payload: dict[str, object],
        row_index: int,
        *,
        local: bool,
    ) -> dict[str, object]:
        latitude = float(payload["latitude"][row_index])
        longitude = float(payload["longitude"][row_index])
        hours, days = self._axes(payload)
        hourly_variables = list(payload["hourly"])
        daily_variables = list(payload["daily"])
        base = round(latitude * 10 + longitude, 6)
        hourly: dict[str, object] = {"time": hours}
        daily: dict[str, object] = {"time": days}
        for variable_index, variable in enumerate(hourly_variables):
            hourly[variable] = [
                round(base + variable_index * 0.01 + frame * 0.1, 6)
                for frame in range(len(hours))
            ]
        for variable_index, variable in enumerate(daily_variables):
            daily[variable] = [
                round(base + 1000 + variable_index * 0.01 + frame * 0.2, 6)
                for frame in range(len(days))
            ]
        if (
            local
            and self.mismatch_local_call == self.local_calls
            and self.mismatch_period == "hourly"
        ):
            hourly[hourly_variables[0]][self.mismatch_frame] += 1.0
        if (
            local
            and self.mismatch_local_call == self.local_calls
            and self.mismatch_period == "daily"
        ):
            daily[daily_variables[0]][self.mismatch_frame] += 1.0
        if (
            not local
            and self.sentinel_changes
            and self.official_calls > 1
            and row_index == 0
        ):
            hourly[hourly_variables[0]][0] += 1.0
        row: dict[str, object] = {
            "latitude": latitude,
            "longitude": longitude,
            "generationtime_ms": 9.0 if local else 0.1,
            "utc_offset_seconds": 0,
            "timezone": "GMT",
            "timezone_abbreviation": "GMT",
            "elevation": 123.0,
            "hourly_units": {
                "time": "iso8601",
                **{variable: "unit" for variable in hourly_variables},
            },
            "hourly": hourly,
            "daily_units": {
                "time": "iso8601",
                **{variable: "unit" for variable in daily_variables},
            },
            "daily": daily,
        }
        if not local:
            row["location_id"] = row_index
        return row

    def __call__(
        self,
        method: str,
        url: str,
        *,
        body: bytes | None,
        headers: dict[str, str],
        timeout: float,
    ) -> compare.HttpResult:
        del timeout
        parsed = urllib.parse.urlsplit(url)
        self.calls.append(
            {
                "method": method,
                "path": parsed.path,
                "body": body,
                "headers": dict(headers),
            }
        )
        if parsed.path == "/source-meta":
            self.probe_calls += 1
            probe_run = (
                "2026-07-23T06:00:00Z"
                if getattr(self, "transitioned", False)
                else RUN
            )
            epoch = int(compare._parse_run_instant(probe_run).timestamp())
            return self._result(
                200,
                {
                    "last_run_initialisation_time": epoch,
                    "last_run_modification_time": epoch + 10,
                    "last_run_availability_time": epoch + 20,
                    "data_end_time": epoch + 360 * 3600,
                },
                {
                    "content-type": "application/json",
                    "last-modified": "Thu, 23 Jul 2026 13:06:01 GMT",
                    "etag": '"source"',
                },
            )
        if parsed.path == "/spatial-latest":
            self.probe_calls += 1
            probe_run = (
                "2026-07-23T06:00:00Z"
                if getattr(self, "transitioned", False)
                else RUN
            )
            return self._result(
                200,
                {
                    "completed": True,
                    "last_modified_time": "2026-07-23T13:07:00Z",
                    "reference_time": probe_run,
                    "valid_times": [
                        probe_run[:16] + "Z",
                        "2026-08-07T00:00Z",
                    ],
                    "variables": ["temperature_2m"],
                },
                {
                    "content-type": "application/json",
                    "last-modified": "Thu, 23 Jul 2026 13:07:00 GMT",
                    "etag": '"spatial"',
                },
            )
        if parsed.path == "/official":
            self.official_calls += 1
            self.assert_json_post(method, body, headers)
            if self.official_statuses:
                status = self.official_statuses.pop(0)
                if status != 200:
                    return self._result(
                        status,
                        {"error": True, "reason": "simulated"},
                        {"content-type": "application/json", "retry-after": "0"},
                    )
            payload = json.loads(body)
            rows = [
                self._row(payload, index, local=False)
                for index in range(len(payload["latitude"]))
            ]
            return self._result(
                200,
                rows[0] if len(rows) == 1 else rows,
                {
                    "content-type": "application/json",
                    "date": getattr(
                        self,
                        "official_http_date",
                        "Thu, 23 Jul 2026 12:21:02 GMT",
                    ),
                },
            )
        if parsed.path == "/local":
            self.local_calls += 1
            self.assert_json_post(method, body, headers)
            payload = json.loads(body)
            if len(payload["latitude"]) != 1 or payload.get("cell_selection") != "land":
                raise AssertionError("local POST is not exactly one land+DEM point")
            if "elevation" in payload:
                raise AssertionError("pass/fail POST must use server DEM elevation")
            row = self._row(payload, 0, local=True)
            if self.mutate_after_local is not None:
                self.mutate_after_local(self.local_calls)
            return self._result(200, row)
        raise AssertionError(f"unexpected fake request: {method} {url}")

    @staticmethod
    def assert_json_post(
        method: str,
        body: bytes | None,
        headers: dict[str, str],
    ) -> None:
        if method != "POST" or body is None:
            raise AssertionError("expected JSON POST")
        if headers.get("Content-Type") != "application/json":
            raise AssertionError("missing JSON content type")
        json.loads(body)


class Workflow:
    def __init__(self, root: Path, fake: FakeApi) -> None:
        self.root = root
        self.fake = fake
        self.config = small_config()
        self.config_path = root / "config.json"
        self.plan_path = root / "plan.json"
        self.cache_dir = root / "cache"
        self.release_path = root / "release.json"
        self.catalog_path = root / "catalog.json"
        self.freeze_path = root / "freeze.json"
        self.output_dir = root / "validation"
        self.config_path.write_text(
            json.dumps(self.config, separators=(",", ":")), encoding="utf-8"
        )
        plan = compare.generate_plan(
            RUN_COMPACT,
            count=compare.POINT_COUNT,
            seed=20260723,
            config=self.config,
            config_sha256=compare.sha256_file(self.config_path),
        )
        compare.write_json_exclusive(self.plan_path, plan)
        self.plan = plan
        self.release_path.write_text(
            json.dumps({"latest_complete_run": RUN_COMPACT}), encoding="utf-8"
        )
        self.catalog_path.write_text(
            json.dumps(
                {
                    "status": "complete",
                    "latest_complete_run": RUN_COMPACT,
                    "available_variables": [
                        "temperature_2m",
                        "temperature_2m_max",
                    ],
                    "available_hourly_variables": ["temperature_2m"],
                    "available_daily_variables": ["temperature_2m_max"],
                    **boundary_fields(),
                }
            ),
            encoding="utf-8",
        )
        compare.create_freeze_attestation(
            RUN_COMPACT,
            self.release_path,
            self.catalog_path,
            self.freeze_path,
            True,
            ["temperature_2m"],
            ["temperature_2m_max"],
        )

    def fetch(
        self,
        *,
        access_profile: str = "mock",
        max_new_requests: int = 10,
        retries: int = 0,
        api_key: str | None = None,
    ) -> dict[str, object]:
        return compare.fetch_official(
            self.plan,
            self.plan_path,
            self.config,
            self.cache_dir,
            str(self.config["official"]["endpoint"]),
            True,
            max_new_requests,
            0,
            30,
            retries,
            api_key,
            access_profile,
            requester=self.fake,
            sleeper=self.fake.sleeps.append,
        )

    def validate(self) -> dict[str, object]:
        args = SimpleNamespace(
            plan=str(self.plan_path),
            cache_dir=str(self.cache_dir),
            freeze_attestation=str(self.freeze_path),
            output_dir=str(self.output_dir),
            local_endpoint=str(self.config["local"]["endpoint"]),
            max_local_requests=compare.POINT_COUNT,
            timeout=30,
        )
        with redirect_stdout(io.StringIO()):
            return compare.run_validation(
                args,
                self.config,
                self.config_path,
                requester=self.fake,
            )


class EcmwfOfficialCompareTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    @staticmethod
    def small_contract() -> mock._patch:
        return mock.patch.multiple(
            compare,
            POINT_COUNT=3,
            COHORT_COUNTS=dict(SMALL_COUNTS),
            COVERAGE_ANCHORS=(),
        )

    def test_canonical_production_contract_and_land_dem_sampling(self) -> None:
        config_path = VALIDATION_ROOT / "ecmwf_validation_config.json"
        config = compare.load_config(config_path)
        self.assertEqual(len(catalog.SURFACE_HOURLY_VARIABLES), 57)
        self.assertEqual(len(catalog.PRESSURE_HOURLY_VARIABLES), 140)
        self.assertEqual(len(catalog.HOURLY_VARIABLES), 197)
        self.assertEqual(len(catalog.DAILY_VARIABLES), 65)
        self.assertEqual(len(catalog.AVAILABLE_VARIABLES), 257)
        self.assertEqual(
            catalog.OPEN_METEO_UPSTREAM_BASELINE,
            "acfe608b825da1a8b42a755297eb61121986e9da",
        )
        self.assertEqual(
            config["open_meteo_upstream_baseline"],
            catalog.OPEN_METEO_UPSTREAM_BASELINE,
        )
        self.assertEqual(
            catalog.HOURLY_CATALOG_SHA256,
            "a518f8c0ddfb5e11ac5661da7d6c5d588bbb56f33e5267378631947e3a52669c",
        )
        self.assertEqual(
            catalog.DAILY_CATALOG_SHA256,
            "87a46a349a767c1e015bf76ab506546865b483365f7e04c543c730d67cd65f33",
        )
        self.assertEqual(
            catalog.AVAILABLE_CATALOG_SHA256,
            "3789f8994822fc3e5820a71de8dab3e805fa13cd7bad1634e6a12c17d197bbe2",
        )
        self.assertEqual(config["variables"]["hourly"], list(catalog.HOURLY_VARIABLES))
        self.assertEqual(config["variables"]["daily"], list(catalog.DAILY_VARIABLES))
        all_variables = (*catalog.HOURLY_VARIABLES, *catalog.DAILY_VARIABLES)
        self.assertFalse(any("showers" in variable for variable in all_variables))
        self.assertFalse(
            any("precipitation_probability" in variable for variable in all_variables)
        )
        self.assertFalse(
            set(catalog.OCEAN_HOURLY_VARIABLES).intersection(
                catalog.HOURLY_VARIABLES
            )
        )
        plan = compare.generate_plan(
            RUN_COMPACT,
            config=config,
            config_sha256=compare.sha256_file(config_path),
        )
        self.assertEqual(len(plan["points"]), 500)
        self.assertEqual({point["cell_selection"] for point in plan["points"]}, {"land"})
        self.assertEqual({point["elevation_mode"] for point in plan["points"]}, {None})
        coverage = {point["coverage_class"] for point in plan["points"]}
        self.assertTrue(
            {"crop_boundary", "coastal", "high_mountain"}.issubset(coverage)
        )
        batches = compare.official_batches(plan, config, "customer")
        self.assertEqual(len(batches), 1)
        self.assertEqual(len(batches[0]["request_point_ids"]), 500)

    def test_full_public_matrix_is_statically_sharded_within_each_terminal_limit(
        self,
    ) -> None:
        config_path = VALIDATION_ROOT / "ecmwf_validation_config.json"
        config = compare.load_config(config_path)
        plan = compare.generate_plan(
            RUN_COMPACT,
            config=config,
            config_sha256=compare.sha256_file(config_path),
        )
        batches = compare.official_batches(plan, config, "public_noncommercial")
        self.assertEqual(len(batches), 3)
        daily_limit = float(config["official"]["public_daily_weight_limit"])
        with self.assertRaisesRegex(compare.ValidationError, "cannot hold"):
            compare.assign_public_batches(batches, ["only-one"], daily_limit)
        assigned = compare.assign_public_batches(
            batches,
            ["terminal-a", "terminal-b", "terminal-c"],
            daily_limit,
        )
        batch_ids, weights = compare.public_executor_summary(assigned)
        self.assertEqual(
            [batch["executor_id"] for batch in assigned],
            ["terminal-a", "terminal-b", "terminal-c"],
        )
        self.assertEqual(
            sorted(batch_id for values in batch_ids.values() for batch_id in values),
            sorted(batch["batch_id"] for batch in batches),
        )
        self.assertTrue(all(weight <= daily_limit for weight in weights.values()))

    def test_local_and_ssh_public_executors_keep_declared_static_order(self) -> None:
        requesters = compare.build_public_executor_requesters(
            "terminal-shanghai",
            [
                "terminal-156=ubuntu@43.156.81.216",
                "terminal-162=ubuntu@43.162.112.201",
            ],
        )

        self.assertEqual(
            list(requesters),
            ["terminal-shanghai", "terminal-156", "terminal-162"],
        )
        self.assertIs(requesters["terminal-shanghai"], compare._request_once)
        with self.assertRaisesRegex(compare.ValidationError, "duplicate"):
            compare.build_public_executor_requesters(
                "terminal-156",
                ["terminal-156=ubuntu@43.156.81.216"],
            )

    def test_ssh_executor_requests_gzip_and_returns_decompressed_json(self) -> None:
        raw = b'{"status":"ok"}'
        meta = {
            "status": 200,
            "headers": {"Content-Type": "application/json"},
            "elapsed_seconds": 1.25,
            "response_bytes": len(raw),
            "response_sha256": compare.sha256_bytes(raw),
        }
        completed = SimpleNamespace(
            returncode=0,
            stderr=b"",
            stdout=(
                b"\n"
                + compare._SSH_HTTP_MARKER
                + json.dumps(meta, separators=(",", ":")).encode()
                + b"\n"
                + raw
            ),
        )
        requester = compare.SshHttpRequester(
            "terminal-a",
            "ubuntu@example.invalid",
        )

        with mock.patch.object(
            compare.subprocess,
            "run",
            return_value=completed,
        ) as run:
            result = requester(
                "POST",
                "https://api.open-meteo.com/v1/ecmwf",
                body=b"{}",
                headers={"Content-Type": "application/json"},
                timeout=300,
            )

        program = run.call_args.kwargs["input"]
        self.assertIn(b'Accept-Encoding", "gzip"', program)
        self.assertIn(b"gzip.decompress(raw)", program)
        self.assertEqual(result.status, 200)
        self.assertEqual(result.raw, raw)

    def test_public_capture_records_fixed_executor_in_success_and_retry_evidence(
        self,
    ) -> None:
        with self.small_contract():
            fake = FakeApi(official_statuses=[429, 200])
            workflow = Workflow(self.root, fake)
            index = compare.fetch_official(
                workflow.plan,
                workflow.plan_path,
                workflow.config,
                workflow.cache_dir,
                str(workflow.config["official"]["endpoint"]),
                True,
                2,
                0,
                30,
                1,
                None,
                "public_noncommercial",
                requester=fake,
                public_executor_requesters={"terminal-a": fake},
                sleeper=fake.sleeps.append,
            )
            self.assertEqual(index["public_executor_batch_ids"], {"terminal-a": ["land_dem_000"]})
            self.assertEqual(index["entries"][0]["executor_id"], "terminal-a")
            success_meta = json.loads(
                (workflow.cache_dir / "official" / "land_dem_000.meta.json").read_bytes()
            )
            failure_meta = json.loads(
                next(
                    (workflow.cache_dir / "official" / "failed_attempts").glob(
                        "*.meta.json"
                    )
                ).read_bytes()
            )
            self.assertEqual(success_meta["executor_id"], "terminal-a")
            self.assertEqual(failure_meta["executor_id"], "terminal-a")
            self.assertEqual(fake.official_calls, 2)

    def test_linux_evidence_paths_are_confined_to_data_disk(self) -> None:
        config = compare.load_config(VALIDATION_ROOT / "ecmwf_validation_config.json")
        compare.require_production_evidence_path(
            Path("/data/validation/ecmwf/2026072300"),
            config,
            "evidence",
            platform_name="posix",
        )
        with self.assertRaisesRegex(compare.ValidationError, "under a configured /data"):
            compare.require_production_evidence_path(
                Path("/opt/1panel/apps/weather/evidence"),
                config,
                "evidence",
                platform_name="posix",
            )

    def test_freeze_rejects_an_incomplete_api_catalog(self) -> None:
        release = self.root / "release.json"
        catalog_path = self.root / "catalog.json"
        release.write_text(
            json.dumps({"latest_complete_run": RUN_COMPACT}), encoding="utf-8"
        )
        catalog_path.write_text(
            json.dumps(
                {
                    "latest_complete_run": RUN_COMPACT,
                    "available_variables": ["temperature_2m"],
                    "available_hourly_variables": ["temperature_2m"],
                    "available_daily_variables": [],
                }
            ),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(compare.ValidationError, "exact API contract"):
            compare.create_freeze_attestation(
                RUN_COMPACT,
                release,
                catalog_path,
                self.root / "freeze.json",
                True,
            )

    def test_freeze_requires_old_run_hidden_right_stencil(self) -> None:
        release = self.root / "release.json"
        catalog_path = self.root / "catalog.json"
        release.write_text(
            json.dumps({"latest_complete_run": RUN_COMPACT}), encoding="utf-8"
        )
        fields = boundary_fields()
        fields["interpolation_support_records"].pop()
        catalog_path.write_text(
            json.dumps(
                {
                    "latest_complete_run": RUN_COMPACT,
                    "available_variables": [
                        "temperature_2m",
                        "temperature_2m_max",
                    ],
                    "available_hourly_variables": ["temperature_2m"],
                    "available_daily_variables": ["temperature_2m_max"],
                    **fields,
                }
            ),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(compare.ValidationError, "boundary stencil"):
            compare.create_freeze_attestation(
                RUN_COMPACT,
                release,
                catalog_path,
                self.root / "freeze.json",
                True,
                ["temperature_2m"],
                ["temperature_2m_max"],
            )

    def test_official_capture_is_one_json_post_and_keeps_key_out_of_artifacts(self) -> None:
        with self.small_contract():
            fake = FakeApi()
            workflow = Workflow(self.root, fake)
            index = workflow.fetch(api_key="top-secret")
            self.assertEqual(index["successful_request_count"], 1)
            self.assertEqual(fake.official_calls, 1)
            request_files = list((workflow.cache_dir / "official").glob("*.request.json"))
            self.assertEqual(len(request_files), 1)
            payload = json.loads(request_files[0].read_bytes())
            self.assertEqual(len(payload["latitude"]), 3)
            self.assertEqual(payload["cell_selection"], "land")
            self.assertNotIn("elevation", payload)
            for path in workflow.cache_dir.rglob("*"):
                if path.is_file():
                    self.assertNotIn(b"top-secret", path.read_bytes())
            no_network = FakeApi()
            reused = compare.fetch_official(
                workflow.plan,
                workflow.plan_path,
                workflow.config,
                workflow.cache_dir,
                str(workflow.config["official"]["endpoint"]),
                False,
                0,
                0,
                30,
                0,
                None,
                "mock",
                requester=no_network,
            )
            self.assertEqual(reused["index_sha256"], index["index_sha256"])
            self.assertEqual(len(no_network.calls), 0)

    @staticmethod
    def prepare_post_capture_transition(workflow: Workflow) -> None:
        workflow.fetch()
        (workflow.cache_dir / "official_index.json").unlink()
        for label in ("after", "spatial_after"):
            for path in compare._probe_paths(workflow.cache_dir, label):
                path.unlink()
        workflow.fake.transitioned = True

    def test_explicit_post_capture_transition_finalizes_and_validates(self) -> None:
        with self.small_contract():
            fake = FakeApi()
            workflow = Workflow(self.root, fake)
            self.prepare_post_capture_transition(workflow)
            index = compare.fetch_official(
                workflow.plan,
                workflow.plan_path,
                workflow.config,
                workflow.cache_dir,
                str(workflow.config["official"]["endpoint"]),
                True,
                0,
                0,
                30,
                0,
                None,
                "mock",
                requester=fake,
                accept_proven_post_capture_transition=True,
            )
            self.assertEqual(index["capture_mode"], "post_capture_transition")
            proof = index["post_capture_transition_proof"]
            self.assertEqual(
                proof["max_batch_http_date"],
                "2026-07-23T12:21:02Z",
            )
            self.assertEqual(
                proof["source_transition_identity"]["last_run_initialisation_time"],
                int(
                    compare._parse_run_instant(
                        "2026-07-23T06:00:00Z"
                    ).timestamp()
                ),
            )
            self.assertEqual(fake.official_calls, 1)
            report = workflow.validate()
            self.assertEqual(report["status"], "passed")

    def test_post_capture_transition_requires_explicit_opt_in(self) -> None:
        with self.small_contract():
            fake = FakeApi()
            workflow = Workflow(self.root, fake)
            self.prepare_post_capture_transition(workflow)
            with self.assertRaisesRegex(compare.ValidationError, "not the frozen 00Z"):
                compare.fetch_official(
                    workflow.plan,
                    workflow.plan_path,
                    workflow.config,
                    workflow.cache_dir,
                    str(workflow.config["official"]["endpoint"]),
                    True,
                    0,
                    0,
                    30,
                    0,
                    None,
                    "mock",
                    requester=fake,
                )
            self.assertFalse((workflow.cache_dir / "official_index.json").exists())

    def test_post_capture_transition_rejects_batch_at_transition_boundary(self) -> None:
        with self.small_contract():
            fake = FakeApi()
            fake.official_http_date = "Thu, 23 Jul 2026 13:06:01 GMT"
            workflow = Workflow(self.root, fake)
            self.prepare_post_capture_transition(workflow)
            with self.assertRaisesRegex(
                compare.ValidationError,
                "not earlier than the temporal source transition",
            ):
                compare.fetch_official(
                    workflow.plan,
                    workflow.plan_path,
                    workflow.config,
                    workflow.cache_dir,
                    str(workflow.config["official"]["endpoint"]),
                    True,
                    0,
                    0,
                    30,
                    0,
                    None,
                    "mock",
                    requester=fake,
                    accept_proven_post_capture_transition=True,
                )
            self.assertFalse((workflow.cache_dir / "official_index.json").exists())

    def test_post_capture_transition_rejects_sentinel_drift(self) -> None:
        with self.small_contract():
            fake = FakeApi()
            workflow = Workflow(self.root, fake)
            self.prepare_post_capture_transition(workflow)
            index = compare.fetch_official(
                workflow.plan,
                workflow.plan_path,
                workflow.config,
                workflow.cache_dir,
                str(workflow.config["official"]["endpoint"]),
                True,
                0,
                0,
                30,
                0,
                None,
                "mock",
                requester=fake,
                accept_proven_post_capture_transition=True,
            )
            first_meta_path = (
                workflow.cache_dir / "official" / "land_dem_000.meta.json"
            )
            second_meta = json.loads(first_meta_path.read_bytes())
            second_meta["sentinel_sha256"] = "0" * 64
            second_meta_path = (
                workflow.cache_dir / "official" / "land_dem_001.meta.json"
            )
            second_meta_path.write_text(json.dumps(second_meta), encoding="utf-8")
            source_meta = json.loads(
                compare._probe_paths(
                    workflow.cache_dir, "after_transition"
                )[1].read_bytes()
            )
            spatial_meta = json.loads(
                compare._probe_paths(
                    workflow.cache_dir, "spatial_after_transition"
                )[1].read_bytes()
            )
            with self.assertRaisesRegex(compare.ValidationError, "sentinel changed"):
                compare._post_capture_transition_proof(
                    cache_dir=workflow.cache_dir,
                    batches=[
                        {"batch_id": "land_dem_000", "profile": "land_dem"},
                        {"batch_id": "land_dem_001", "profile": "land_dem"},
                    ],
                    target_run=RUN,
                    source_identity=index["source_identity"],
                    source_transition_identity=index[
                        "source_transition_identity"
                    ],
                    source_transition_meta=source_meta,
                    source_transition_sha256=index[
                        "source_probe_after_sha256"
                    ],
                    spatial_identity=index["spatial_identity"],
                    spatial_transition_identity=index[
                        "spatial_transition_identity"
                    ],
                    spatial_transition_meta=spatial_meta,
                    spatial_transition_sha256=index[
                        "spatial_probe_after_sha256"
                    ],
                )

    def test_complete_serial_local_post_validation_passes(self) -> None:
        with self.small_contract():
            fake = FakeApi()
            workflow = Workflow(self.root, fake)
            workflow.fetch()
            report = workflow.validate()
            self.assertEqual(report["status"], "passed")
            self.assertEqual(report["points_completed"], 3)
            self.assertEqual(fake.local_calls, 3)
            self.assertEqual(report["hourly_values_compared"], 3 * 361)
            self.assertEqual(report["daily_values_compared"], 3 * 15)
            request_files = list(
                (workflow.output_dir / "attempts").rglob("*.request.json")
            )
            self.assertEqual(len(request_files), 3)
            for request_path in request_files:
                payload = json.loads(request_path.read_bytes())
                self.assertEqual(len(payload["latitude"]), 1)
                self.assertIn("hourly", payload)
                self.assertIn("daily", payload)

    def test_daily_difference_stops_before_requesting_next_point(self) -> None:
        with self.small_contract():
            fake = FakeApi(
                mismatch_local_call=2,
                mismatch_period="daily",
                mismatch_frame=14,
            )
            workflow = Workflow(self.root, fake)
            workflow.fetch()
            report = workflow.validate()
            self.assertEqual(report["status"], "failed")
            self.assertEqual(fake.local_calls, 2)
            self.assertEqual(report["points_completed"], 1)
            self.assertEqual(
                report["failure"]["difference"]["path"],
                "$.daily.temperature_2m_max[14]",
            )
            self.assertEqual(
                report["failure"]["difference"]["reason"], "json_value"
            )
            self.assertEqual(
                len(list((workflow.output_dir / "receipts").glob("*.json"))), 1
            )

    def test_all_six_rolling_hour0_fields_are_compared_at_frame_zero(self) -> None:
        times = [
            (compare.parse_run(RUN) + dt.timedelta(hours=index)).strftime(
                "%Y-%m-%dT%H:%M"
            )
            for index in range(361)
        ]
        plan = {
            "hourly": {
                "variables": list(compare.ROLLING_HOUR0_VARIABLES),
                "time": times,
            },
            "daily": {"variables": ["temperature_2m_max"], "time": ["2026-07-23"]},
        }
        for variable in compare.ROLLING_HOUR0_VARIABLES:
            official = {
                "hourly": {
                    "time": times,
                    **{
                        item: [0.0] * 361
                        for item in compare.ROLLING_HOUR0_VARIABLES
                    },
                }
            }
            local = json.loads(json.dumps(official))
            local["hourly"][variable][0] = 1.0
            difference = compare.first_json_difference(official, local, plan)
            self.assertIsNotNone(difference)
            self.assertEqual(difference["path"], f"$.hourly.{variable}[0]")

    def test_json_number_type_and_signed_zero_are_strict(self) -> None:
        plan = {"hourly": {"variables": []}, "daily": {"variables": []}}
        integer_difference = compare.first_json_difference(
            {"value": 1}, {"value": 1.0}, plan
        )
        self.assertEqual(integer_difference["reason"], "json_type")
        signed_zero = compare.first_json_difference(
            {"value": 0.0}, {"value": -0.0}, plan
        )
        self.assertEqual(signed_zero["reason"], "json_value")
        boolean_difference = compare.first_json_difference(
            {"value": True}, {"value": 1}, plan
        )
        self.assertEqual(boolean_difference["reason"], "json_type")

    def test_target_day_interpolation_boundary_hours_are_not_skipped(self) -> None:
        variables = (
            "temperature_2m",
            "wind_speed_10m",
            "precipitation",
            "shortwave_radiation",
        )
        plan = {
            "hourly": {"variables": list(variables)},
            "daily": {"variables": []},
        }
        for frame in (16, 17, 22, 23):
            for variable in variables:
                official = {
                    "hourly": {item: [0.0] * 361 for item in variables}
                }
                local = json.loads(json.dumps(official))
                local["hourly"][variable][frame] = 1.0
                difference = compare.first_json_difference(official, local, plan)
                self.assertIsNotNone(difference)
                self.assertEqual(
                    difference["path"], f"$.hourly.{variable}[{frame}]"
                )

    def test_retry_after_failure_is_persisted_and_request_body_is_linked(self) -> None:
        with self.small_contract():
            fake = FakeApi(official_statuses=[429, 200])
            workflow = Workflow(self.root, fake)
            index = workflow.fetch(max_new_requests=2, retries=1)
            self.assertEqual(index["failed_http_attempt_count"], 1)
            self.assertEqual(fake.official_calls, 2)
            self.assertEqual(fake.sleeps, [0.0])
            failure_files = list(
                (workflow.cache_dir / "official" / "failed_attempts").glob(
                    "*.meta.json"
                )
            )
            self.assertEqual(len(failure_files), 1)
            failure = json.loads(failure_files[0].read_bytes())
            request_path = workflow.cache_dir / failure["request_payload_file"]
            self.assertTrue(request_path.is_file())
            self.assertEqual(
                compare.sha256_file(request_path),
                failure["request_payload_sha256"],
            )

    def test_incomplete_chunked_response_is_a_retryable_transport_failure(self) -> None:
        response = mock.MagicMock()
        response.__enter__.return_value = response
        response.read.side_effect = compare.http.client.IncompleteRead(
            b'{"partial":true',
            5058,
        )
        with mock.patch.object(compare.urllib.request, "urlopen", return_value=response):
            with self.assertRaisesRegex(
                compare.HttpRequestError,
                "IncompleteRead",
            ):
                compare._request_once(
                    "POST",
                    "https://api.open-meteo.com/v1/ecmwf",
                    body=b"{}",
                    headers={"Content-Type": "application/json"},
                    timeout=30,
                )

    def test_sentinel_change_between_public_batches_invalidates_snapshot(self) -> None:
        with self.small_contract():
            fake = FakeApi(sentinel_changes=True)
            workflow = Workflow(self.root, fake)
            workflow.config["official"]["multi_location_limit"] = 2
            workflow.config["official"]["public_daily_weight_limit"] = 10000
            workflow.config_path.write_text(
                json.dumps(workflow.config, separators=(",", ":")),
                encoding="utf-8",
            )
            workflow.plan = compare.generate_plan(
                RUN_COMPACT,
                count=3,
                seed=20260723,
                config=workflow.config,
                config_sha256=compare.sha256_file(workflow.config_path),
            )
            workflow.plan_path.unlink()
            compare.write_json_exclusive(workflow.plan_path, workflow.plan)
            with self.assertRaisesRegex(compare.ValidationError, "sentinel changed"):
                workflow.fetch(access_profile="public_noncommercial")
            self.assertEqual(fake.official_calls, 2)

    def test_manifest_change_during_point_is_a_hard_failure(self) -> None:
        with self.small_contract():
            holder: dict[str, Workflow] = {}

            def mutate(call: int) -> None:
                if call == 1:
                    holder["workflow"].release_path.write_text(
                        json.dumps(
                            {
                                "latest_complete_run": RUN_COMPACT,
                                "mutated": True,
                            }
                        ),
                        encoding="utf-8",
                    )

            fake = FakeApi(mutate_after_local=mutate)
            workflow = Workflow(self.root, fake)
            holder["workflow"] = workflow
            workflow.fetch()
            with self.assertRaisesRegex(compare.ValidationError, "hash changed"):
                workflow.validate()
            self.assertEqual(fake.local_calls, 1)


if __name__ == "__main__":
    unittest.main()
