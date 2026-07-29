import unittest
from datetime import datetime, timezone
from pathlib import Path

from om_downloader.coverage import (
    build_coverage_plan,
    build_run_forecast_hour_coverage_plan,
    build_run_native_forecast_hour_coverage_plan,
    required_start_for_anchors,
)
from om_downloader.metadata import OmRun
from om_downloader.model_config import Bounds, ProductConfig, load_models


class CoverageTests(unittest.TestCase):
    def test_builds_single_run_zero_through_five_hour_coverage(self):
        product = ProductConfig(
            name="gfs025",
            download_product="om_gfs025",
            openmeteo_model="ncep_gfs025",
            forecast_hour_end=384,
            run_cadence_hours=6,
            timezone_anchors=(8,),
            requested_bounds=Bounds(70.0, 0.0, 140.0, 58.0),
            bounds_padding_degrees=2.0,
            required_variables=("temperature_2m",),
            optional_variables=(),
            requested_pressure_levels_hpa=(),
        )
        base = datetime(2026, 7, 15, 0, tzinfo=timezone.utc)
        run = OmRun("2026071500", base, 384, ("temperature_2m",), ())

        plan = build_run_forecast_hour_coverage_plan(
            product,
            run,
            forecast_hour_end=5,
        )

        self.assertEqual(plan.required_start_utc, base)
        self.assertEqual(plan.required_end_utc, datetime(2026, 7, 15, 5, tzinfo=timezone.utc))
        self.assertEqual([slot.forecast_hour for slot in plan.slots], list(range(6)))
        self.assertEqual({slot.source_run for slot in plan.slots}, {"2026071500"})

    def test_gfs_probability_short_run_starts_at_native_f003_without_f000(self):
        config = load_models(Path("config/models.json"))
        base = datetime(2026, 7, 15, 0, tzinfo=timezone.utc)

        for product_name in ("ncep_gefs025", "ncep_gefs05"):
            with self.subTest(product=product_name):
                product = config.products[product_name]
                run = OmRun(
                    "2026071500",
                    base,
                    product.forecast_hour_end,
                    ("precipitation_probability",),
                    (),
                    valid_times_utc=(
                        base,
                        datetime(2026, 7, 15, 3, tzinfo=timezone.utc),
                        datetime(2026, 7, 15, 6, tzinfo=timezone.utc),
                    ),
                )

                plan = build_run_native_forecast_hour_coverage_plan(
                    product,
                    run,
                    forecast_hour_end=5,
                )

                self.assertEqual(plan.required_start_utc, base.replace(hour=3))
                self.assertEqual(plan.required_end_utc, base.replace(hour=3))
                self.assertEqual(
                    [slot.forecast_hour for slot in plan.slots],
                    [3],
                )
                self.assertNotIn(0, [slot.forecast_hour for slot in plan.slots])

    def test_required_start_uses_earliest_utc_anchor(self):
        now = datetime(2026, 7, 8, 14, 0, tzinfo=timezone.utc)
        start = required_start_for_anchors(now, (8, 6))
        self.assertEqual(start, datetime(2026, 7, 7, 16, 0, tzinfo=timezone.utc))

    def test_selects_latest_source_run_per_valid_hour(self):
        product = ProductConfig(
            name="gfs025",
            download_product="om_gfs025",
            openmeteo_model="ncep_gfs025",
            forecast_hour_end=12,
            run_cadence_hours=6,
            timezone_anchors=(8, 6),
            requested_bounds=Bounds(70.0, 0.0, 140.0, 58.0),
            bounds_padding_degrees=2.0,
            required_variables=("TMP",),
            optional_variables=(),
            requested_pressure_levels_hpa=(),
        )
        now = datetime(2026, 7, 8, 14, 0, tzinfo=timezone.utc)
        runs = [
            OmRun("2026070712", datetime(2026, 7, 7, 12, tzinfo=timezone.utc), 24, ("TMP",), ()),
            OmRun("2026070718", datetime(2026, 7, 7, 18, tzinfo=timezone.utc), 24, ("TMP",), ()),
            OmRun("2026070800", datetime(2026, 7, 8, 0, tzinfo=timezone.utc), 24, ("TMP",), ()),
            OmRun("2026070806", datetime(2026, 7, 8, 6, tzinfo=timezone.utc), 12, ("TMP",), ()),
        ]
        plan = build_coverage_plan(product, runs, now)
        self.assertEqual(plan.required_start_utc, datetime(2026, 7, 7, 16, tzinfo=timezone.utc))
        self.assertEqual(plan.required_end_utc, datetime(2026, 7, 8, 18, tzinfo=timezone.utc))
        selected = {slot.valid_time_utc: slot.source_run for slot in plan.slots}
        self.assertEqual(selected[datetime(2026, 7, 7, 16, tzinfo=timezone.utc)], "2026070712")
        self.assertEqual(selected[datetime(2026, 7, 7, 18, tzinfo=timezone.utc)], "2026070718")
        self.assertEqual(selected[datetime(2026, 7, 8, 5, tzinfo=timezone.utc)], "2026070800")
        self.assertEqual(selected[datetime(2026, 7, 8, 6, tzinfo=timezone.utc)], "2026070806")

    def test_uses_actual_sparse_valid_times_when_available(self):
        product = ProductConfig(
            name="gfs025",
            download_product="om_gfs025",
            openmeteo_model="ncep_gfs025",
            forecast_hour_end=6,
            run_cadence_hours=6,
            timezone_anchors=(8,),
            requested_bounds=Bounds(70.0, 0.0, 140.0, 58.0),
            bounds_padding_degrees=2.0,
            required_variables=("temperature_2m",),
            optional_variables=(),
            requested_pressure_levels_hpa=(),
        )
        run = OmRun(
            "2026070716",
            datetime(2026, 7, 7, 16, tzinfo=timezone.utc),
            6,
            ("temperature_2m",),
            (),
            valid_times_utc=(
                datetime(2026, 7, 7, 16, tzinfo=timezone.utc),
                datetime(2026, 7, 7, 19, tzinfo=timezone.utc),
                datetime(2026, 7, 7, 22, tzinfo=timezone.utc),
            ),
        )

        plan = build_coverage_plan(product, [run], datetime(2026, 7, 8, 1, tzinfo=timezone.utc))

        self.assertEqual(
            [slot.valid_time_utc for slot in plan.slots],
            list(run.valid_times_utc),
        )
        self.assertEqual([slot.forecast_hour for slot in plan.slots], [0, 3, 6])


if __name__ == "__main__":
    unittest.main()
