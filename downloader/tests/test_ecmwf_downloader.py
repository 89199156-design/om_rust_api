import argparse
import json
import importlib.util
import re
import tempfile
import unittest
from contextlib import redirect_stdout
from datetime import datetime, timedelta, timezone
from io import StringIO
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from om_downloader import cli as cli_module
from om_downloader.coverage import (
    build_product_coverage_plan,
    build_run_native_forecast_hour_coverage_plan,
)
from om_downloader.metadata import OmRun
from om_downloader.model_config import Bounds, ProductConfig, load_models
from om_downloader.om_catalog import (
    OpenMeteoSpatialCatalog,
    discover_openmeteo_spatial_runs,
)
from om_downloader.region import grid_spec_for_openmeteo_model, regular_grid_ranges
from om_downloader import ecmwf_catalog


UTC = timezone.utc


def _valid_times(base: datetime, *, short: bool) -> tuple[datetime, ...]:
    if short:
        hours = range(0, 145, 3)
    else:
        hours = list(range(0, 145, 3)) + list(range(150, 361, 6))
    return tuple(base + timedelta(hours=hour) for hour in hours)


def _product() -> ProductConfig:
    return ProductConfig(
        name="ecmwf_ifs025",
        download_product="om_ecmwf_ifs025",
        openmeteo_model="ecmwf_ifs025",
        forecast_hour_end=360,
        run_cadence_hours=6,
        timezone_anchors=(8, 6),
        requested_bounds=Bounds(70.0, 0.0, 140.0, 58.0),
        bounds_padding_degrees=2.0,
        required_variables=("temperature_2m", "wind_gusts_10m"),
        optional_variables=(),
        required_initial_fallback_variables=(
            "wind_gusts_10m",
            "temperature_2m_max",
            "temperature_2m_min",
            "shortwave_radiation",
            "precipitation",
            "runoff",
        ),
        interpolation_support_hours=12,
        requested_pressure_levels_hpa=(),
        history_hours=6,
        coverage_strategy="latest_with_long_run_tail",
    )


class EcmwfDownloaderTests(unittest.TestCase):
    def test_api_downloader_and_validator_share_exact_public_catalog(self):
        validator_path = Path("../scripts/validation/ecmwf_variable_catalog.py")
        spec = importlib.util.spec_from_file_location("validator_ecmwf_catalog", validator_path)
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader)
        validator = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(validator)

        self.assertEqual(ecmwf_catalog.HOURLY_VARIABLES, validator.HOURLY_VARIABLES)
        self.assertEqual(ecmwf_catalog.DAILY_VARIABLES, validator.DAILY_VARIABLES)

        rust = Path("../om_api/src/query.rs").read_text(encoding="utf-8")

        def rust_strings(constant: str) -> tuple[str, ...]:
            match = re.search(
                rf"pub const {constant}: &\[[^]]+\] = &\[(.*?)\];",
                rust,
                re.DOTALL,
            )
            self.assertIsNotNone(match, constant)
            return tuple(re.findall(r'"([^"]+)"', match.group(1)))

        rust_surface = rust_strings("ECMWF_PUBLIC_SURFACE_VARIABLES")
        rust_types = rust_strings("ECMWF_PUBLIC_PRESSURE_VARIABLE_TYPES")
        rust_levels_match = re.search(
            r"pub const ECMWF_PUBLIC_PRESSURE_LEVELS: &\[u16\] = &\[(.*?)\];",
            rust,
            re.DOTALL,
        )
        self.assertIsNotNone(rust_levels_match)
        rust_levels = tuple(int(value) for value in re.findall(r"\d+", rust_levels_match.group(1)))
        rust_hourly = (*rust_surface, *(f"{kind}_{level}hPa" for level in rust_levels for kind in rust_types))
        self.assertEqual(rust_hourly, validator.HOURLY_VARIABLES)
        self.assertEqual(rust_strings("ECMWF_PUBLIC_DAILY_VARIABLES"), validator.DAILY_VARIABLES)

    def test_production_config_declares_free_ifs025_capabilities(self):
        config = load_models(Path("config/models.json"))
        product = config.products["ecmwf_ifs025"]

        self.assertEqual(product.openmeteo_model, "ecmwf_ifs025")
        self.assertEqual(product.forecast_hour_end, 360)
        self.assertEqual(product.history_hours, 6)
        self.assertEqual(product.coverage_strategy, "latest_with_long_run_tail")
        self.assertEqual(
            product.requested_pressure_levels_hpa,
            (1000, 925, 850, 700, 600, 500, 400, 300, 250, 200, 150, 100, 50, 10),
        )
        for variable in (
            "temperature_2m",
            "precipitation_type",
            "temperature_10hPa",
            "relative_humidity_925hPa",
            "wind_u_component_50hPa",
            "geopotential_height_500hPa",
        ):
            self.assertIn(variable, product.required_variables)
        self.assertEqual(product.required_sparse_variables, ())
        self.assertEqual(product.optional_variables, ())
        self.assertEqual(
            product.required_initial_fallback_variables,
            (
                "wind_gusts_10m",
                "temperature_2m_max",
                "temperature_2m_min",
                "shortwave_radiation",
                "precipitation",
                "runoff",
            ),
        )
        self.assertEqual(product.interpolation_support_hours, 12)
        self.assertEqual(product.missing_variable_fallback_context_hours, 18)
        self.assertEqual(product.missing_variable_fallback_predecessor_runs, 1)
        self.assertIn("wind_gusts_10m", product.required_variables)
        for unavailable in ("visibility", "uv_index", "showers", "precipitation_probability"):
            self.assertNotIn(unavailable, product.required_variables)
            self.assertNotIn(unavailable, product.optional_variables)

    def test_interpolation_support_extends_old_run_to_the_right_of_each_boundary(self):
        prior12_base = datetime(2026, 7, 22, 12, tzinfo=UTC)
        prior18_base = datetime(2026, 7, 22, 18, tzinfo=UTC)
        target00_base = datetime(2026, 7, 23, 0, tzinfo=UTC)
        runs = [
            OmRun(
                "2026072212",
                prior12_base,
                360,
                ("temperature_2m",),
                (),
                valid_times_utc=_valid_times(prior12_base, short=False),
            ),
            OmRun(
                "2026072218",
                prior18_base,
                144,
                ("temperature_2m",),
                (),
                valid_times_utc=_valid_times(prior18_base, short=True),
            ),
            OmRun(
                "2026072300",
                target00_base,
                360,
                ("temperature_2m",),
                (),
                valid_times_utc=_valid_times(target00_base, short=False),
            ),
        ]
        selected = [
            ("2026072212", prior12_base),
            ("2026072212", prior12_base + timedelta(hours=3)),
            ("2026072218", prior18_base),
            ("2026072218", prior18_base + timedelta(hours=3)),
            ("2026072300", target00_base),
        ]
        plan = SimpleNamespace(
            slots=[
                SimpleNamespace(source_run=run_id, valid_time_utc=valid_time)
                for run_id, valid_time in selected
            ]
        )
        records = [
            {
                "valid_time_utc": valid_time.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "source_run": run_id,
                "forecast_hour": int(
                    (valid_time - next(run.base_time_utc for run in runs if run.run_id == run_id))
                    .total_seconds()
                    // 3600
                ),
                "url": f"https://example.test/{run_id}/{valid_time:%H}",
            }
            for run_id, valid_time in selected
        ]

        enriched = cli_module._with_interpolation_support_records(
            _product(),
            plan,
            runs,
            records,
            bucket_url="https://openmeteo.s3.amazonaws.com",
        )
        support = {
            (entry["source_run"], entry["valid_time_utc"])
            for entry in enriched
            if entry["interpolation_support"]
        }

        self.assertIn(("2026072212", "2026-07-22T18:00:00Z"), support)
        self.assertIn(("2026072212", "2026-07-22T21:00:00Z"), support)
        self.assertIn(("2026072218", "2026-07-23T00:00:00Z"), support)
        self.assertIn(("2026072218", "2026-07-23T03:00:00Z"), support)
        self.assertNotIn(("2026072212", "2026-07-22T09:00:00Z"), support)

    def test_ecmwf_group_command_dispatches_frozen_reference_to_regular_release(self):
        with patch.object(
            cli_module,
            "_download_openmeteo_group_release",
            return_value=0,
        ) as release:
            result = cli_module.main(
                [
                    "--download-openmeteo-group",
                    "ecmwf",
                    "--config",
                    "config/models.json",
                    "--now",
                    "2026-07-23T08:00:00Z",
                    "--reference-time",
                    "2026-07-23T00:00:00Z",
                ]
            )

        self.assertEqual(result, 0)
        release.assert_called_once()
        forwarded = release.call_args.args[0]
        self.assertEqual(forwarded.download_openmeteo_group, "ecmwf")
        self.assertEqual(forwarded.reference_time, "2026-07-23T00:00:00Z")

    def test_ecmwf_production_group_dispatches_gfs_batch_window_reconciliation(self):
        with (
            patch.object(
                cli_module,
                "_reconcile_ecmwf_retention_window",
                return_value=0,
            ) as reconcile,
            patch.object(
                cli_module,
                "_download_openmeteo_group_release",
                side_effect=AssertionError("production must use retained-release reconciliation"),
            ),
        ):
            result = cli_module.main(
                [
                    "--download-openmeteo-group",
                    "ecmwf",
                    "--config",
                    "config/models.json",
                    "--now",
                    "2026-07-28T03:00:00Z",
                ]
            )

        self.assertEqual(result, 0)
        reconcile.assert_called_once()
        forwarded = reconcile.call_args.args[0]
        self.assertIsNone(forwarded.reference_time)
        self.assertEqual(
            forwarded.retain_complete_releases,
            cli_module.ECMWF_TOTAL_RELEASE_RETENTION,
        )
        self.assertEqual(
            cli_module.ECMWF_COMPLETE_RUN_RETENTION,
            cli_module.GFS_COMPLETE_RUN_RETENTION,
        )
        self.assertEqual(
            cli_module.ECMWF_PARTIAL_RUN_RETENTION,
            cli_module.GFS_PARTIAL_RUN_RETENTION,
        )
        self.assertEqual(cli_module.ECMWF_TOTAL_RELEASE_RETENTION, 5)
        self.assertEqual(cli_module._effective_group_retention("ecmwf", 1), 5)

    def test_ecmwf_reconciliation_backfills_two_full_and_three_short_releases(self):
        runs = (
            "2026072718",
            "2026072712",
            "2026072706",
            "2026072700",
            "2026072618",
        )
        plans = [
            (run, {"ecmwf_ifs025": (None, [], SimpleNamespace(latest_complete_run=run))})
            for run in runs
        ]
        retained = {
            run: {"latest_complete_run": run, "status": "complete"}
            for run in runs
        }

        def download_release(*_args, **kwargs):
            plan = kwargs["plan_by_product_override"]["ecmwf_ifs025"][2]
            print(json.dumps({"status": "complete", "latest_complete_run": plan.latest_complete_run}))
            return 0

        with tempfile.TemporaryDirectory() as directory:
            output = StringIO()
            args = SimpleNamespace(
                config="config/models.json",
                retain_complete_releases=5,
                now="2026-07-28T03:00:00Z",
                openmeteo_bucket_url="https://example.test",
                output=directory,
                publish_openmeteo_group_to=str(Path(directory) / "api"),
            )
            with (
                patch.object(
                    cli_module,
                    "load_models",
                    return_value=SimpleNamespace(
                        products={"ecmwf_ifs025": _product()}
                    ),
                ),
                patch.object(
                    cli_module,
                    "_discover_recent_ecmwf_retention_plans",
                    return_value=plans,
                ),
                patch.object(
                    cli_module,
                    "prune_expired_group_releases",
                    return_value=[],
                ),
                patch.object(
                    cli_module,
                    "_matching_group_releases",
                    side_effect=[{}, retained],
                ),
                patch.object(
                    cli_module,
                    "_download_openmeteo_group_release",
                    side_effect=download_release,
                ) as download,
                patch.object(
                    cli_module,
                    "retain_group_release_from_mirror",
                    return_value={"status": "retained"},
                ) as retain,
                patch.object(
                    cli_module,
                    "_clear_group_download_payloads",
                    return_value=[],
                ),
                patch.object(
                    cli_module,
                    "_read_json_if_exists",
                    return_value={"latest_complete_run": "2026072400"},
                ),
                patch.object(
                    cli_module,
                    "activate_group_release",
                    return_value={"status": "activated"},
                ) as activate,
                redirect_stdout(output),
            ):
                result = cli_module._reconcile_ecmwf_retention_window(
                    args,
                    argparse.ArgumentParser(),
                )

        self.assertEqual(result, 0)
        self.assertEqual(download.call_count, 5)
        downloaded_runs = [
            call.kwargs["plan_by_product_override"]["ecmwf_ifs025"][2].latest_complete_run
            for call in download.call_args_list
        ]
        self.assertEqual(downloaded_runs, list(reversed(runs)))
        self.assertEqual(retain.call_count, 5)
        activate.assert_called_once()
        self.assertEqual(activate.call_args.args[2], retained[runs[0]])
        report = json.loads(output.getvalue().splitlines()[-1])
        self.assertEqual(report["retained_complete_runs"], list(runs[:2]))
        self.assertEqual(report["retained_partial_runs"], list(runs[2:]))
        self.assertEqual(report["partial_forecast_hour_end"], 6)

    def test_ecmwf_short_release_keeps_native_frames_through_six_hours(self):
        base = datetime(2026, 7, 27, 6, tzinfo=UTC)
        run = OmRun(
            run_id="2026072706",
            base_time_utc=base,
            max_forecast_hour=144,
            variables=(),
            pressure_levels_hpa=(),
            valid_times_utc=_valid_times(base, short=True),
        )
        plan = build_run_native_forecast_hour_coverage_plan(
            _product(),
            run,
            forecast_hour_end=cli_module.ECMWF_PARTIAL_FORECAST_HOUR_END,
        )
        self.assertEqual(
            [slot.forecast_hour for slot in plan.slots],
            [0, 3, 6],
        )
        self.assertEqual(plan.required_end_utc, base + timedelta(hours=6))

    def test_ecmwf_grid_matches_public_ifs025_om_dimensions(self):
        grid = grid_spec_for_openmeteo_model("ecmwf_ifs025", dimensions=(721, 1440))
        selection = regular_grid_ranges(grid, Bounds(68.0, -2.0, 142.0, 60.0))

        self.assertEqual((grid.ny, grid.nx), (721, 1440))
        self.assertEqual(grid.dx, 0.25)
        self.assertEqual(grid.dy, 0.25)
        self.assertFalse(selection["is_global"])

    def test_stitches_frozen_short_run_over_previous_long_run_tail(self):
        previous_base = datetime(2026, 7, 18, 12, tzinfo=UTC)
        latest_base = datetime(2026, 7, 18, 18, tzinfo=UTC)
        previous = OmRun(
            "2026071812",
            previous_base,
            360,
            ("temperature_2m", "wind_gusts_10m"),
            (),
            valid_times_utc=_valid_times(previous_base, short=False),
        )
        latest = OmRun(
            "2026071818",
            latest_base,
            144,
            ("temperature_2m",),
            (),
            valid_times_utc=_valid_times(latest_base, short=True),
        )

        plan = build_product_coverage_plan(
            _product(),
            [previous, latest],
            datetime(2026, 7, 19, 3, tzinfo=UTC),
        )

        selected = {slot.valid_time_utc: slot.source_run for slot in plan.slots}
        self.assertEqual(plan.latest_complete_run, "2026071818")
        self.assertEqual(plan.public_start_utc, datetime(2026, 7, 18, 16, tzinfo=UTC))
        self.assertEqual(plan.required_start_utc, datetime(2026, 7, 18, 10, tzinfo=UTC))
        self.assertEqual(plan.required_end_utc, datetime(2026, 8, 2, 12, tzinfo=UTC))
        self.assertEqual(selected[latest_base], "2026071818")
        self.assertEqual(
            selected[latest_base + timedelta(hours=144)],
            "2026071818",
        )
        self.assertEqual(
            selected[datetime(2026, 7, 25, 0, tzinfo=UTC)],
            "2026071812",
        )
        self.assertEqual(plan.slots[-1].source_run, "2026071812")

    def test_discovery_walks_back_until_a_long_run_is_found(self):
        latest_base = datetime(2026, 7, 18, 18, tzinfo=UTC)
        previous_base = datetime(2026, 7, 18, 12, tzinfo=UTC)
        latest = OpenMeteoSpatialCatalog(
            model="ecmwf_ifs025",
            completed=True,
            reference_time_utc=latest_base,
            valid_times_utc=_valid_times(latest_base, short=True),
            variables=("temperature_2m",),
        )
        requested_urls = []

        def fetch(url):
            requested_urls.append(url)
            return json.dumps(
                [
                    {
                        "completed": True,
                        "reference_time": "2026-07-18T12:00:00Z",
                        "valid_times": [
                            value.strftime("%Y-%m-%dT%H:%MZ")
                            for value in _valid_times(previous_base, short=False)
                        ],
                        "variables": ["temperature_2m", "wind_gusts_10m"],
                    }
                ]
            ).encode("utf-8")

        runs = discover_openmeteo_spatial_runs(
            "ecmwf_ifs025",
            latest,
            bucket_url="https://openmeteo.s3.amazonaws.com",
            required_start_utc=latest_base,
            run_cadence_hours=6,
            required_long_run_forecast_hour=360,
            fetch=fetch,
        )

        self.assertEqual([run.run_id for run in runs], ["2026071812", "2026071818"])
        self.assertEqual(runs[0].max_forecast_hour, 360)
        self.assertEqual(
            requested_urls,
            [
                "https://openmeteo.s3.amazonaws.com/data_spatial/"
                "ecmwf_ifs025/2026/07/18/1200Z/meta.json"
            ],
        )

    def test_cli_reference_time_loads_exact_run_instead_of_latest(self):
        reference = datetime(2026, 7, 18, 18, tzinfo=UTC)
        catalog = OpenMeteoSpatialCatalog(
            model="ecmwf_ifs025",
            completed=True,
            reference_time_utc=reference,
            valid_times_utc=_valid_times(reference, short=True),
            variables=("temperature_2m",),
        )
        stdout = StringIO()
        with (
            patch.object(cli_module, "load_openmeteo_spatial_run", return_value=catalog) as load_run,
            patch.object(
                cli_module,
                "load_openmeteo_spatial_latest",
                side_effect=AssertionError("latest discovery must stay frozen"),
            ),
            redirect_stdout(stdout),
        ):
            result = cli_module.main(
                [
                    "--inspect-openmeteo-model",
                    "ecmwf_ifs025",
                    "--reference-time",
                    "2026-07-18T18:00:00Z",
                ]
            )

        self.assertEqual(result, 0)
        load_run.assert_called_once_with(
            "ecmwf_ifs025",
            reference,
            bucket_url="https://openmeteo.s3.amazonaws.com",
        )
        self.assertEqual(json.loads(stdout.getvalue())["reference_time"], "2026-07-18T18:00:00Z")
        self.assertEqual(cli_module.OPENMETEO_GROUP_PRODUCTS["ecmwf"], ("ecmwf_ifs025",))


if __name__ == "__main__":
    unittest.main()
