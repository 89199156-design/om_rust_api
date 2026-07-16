import json
import unittest
from datetime import datetime, timezone

from om_downloader.om_catalog import (
    OpenMeteoSpatialCatalog,
    coverage_object_records,
    discover_openmeteo_spatial_runs,
    om_run_from_spatial_catalog,
    openmeteo_spatial_object_key,
    openmeteo_spatial_object_url,
    openmeteo_spatial_run_meta_url,
    parse_openmeteo_spatial_latest,
    required_reference_times_for_coverage,
)
from om_downloader.coverage import CoveragePlan, CoverageSlot
from om_downloader.metadata import OmRun


def _latest_payload():
    return json.dumps(
        [
            {
                "completed": True,
                "last_modified_time": "2026-07-08T00:03:26Z",
                "reference_time": "2026-07-07T18:00:00Z",
                "valid_times": [
                    "2026-07-07T18:00Z",
                    "2026-07-07T19:00Z",
                    "2026-07-08T00:00Z",
                    "2026-07-08T06:00Z",
                ],
                "variables": [
                    "temperature_2m",
                    "relative_humidity_2m",
                    "temperature_850hPa",
                ],
                "crs_wkt": "GEOGCRS[\"WGS 84\"]",
            }
        ]
    ).encode("utf-8")


class OmCatalogTests(unittest.TestCase):
    def test_parse_openmeteo_spatial_latest_reads_actual_schema(self):
        catalog = parse_openmeteo_spatial_latest("ncep_gfs025", _latest_payload())

        self.assertIsInstance(catalog, OpenMeteoSpatialCatalog)
        self.assertEqual(catalog.model, "ncep_gfs025")
        self.assertTrue(catalog.completed)
        self.assertEqual(catalog.reference_time_utc.isoformat(), "2026-07-07T18:00:00+00:00")
        self.assertEqual(catalog.valid_times_utc[0].isoformat(), "2026-07-07T18:00:00+00:00")
        self.assertEqual(catalog.max_forecast_hour, 12)
        self.assertEqual(catalog.available_variables, ("relative_humidity_2m", "temperature_2m", "temperature_850hPa"))

    def test_om_run_from_spatial_catalog_uses_actual_metadata(self):
        catalog = parse_openmeteo_spatial_latest("ncep_gfs025", _latest_payload())

        run = om_run_from_spatial_catalog("gfs025", catalog)

        self.assertEqual(run.run_id, "2026070718")
        self.assertEqual(run.base_time_utc.isoformat(), "2026-07-07T18:00:00+00:00")
        self.assertEqual(run.max_forecast_hour, 12)
        self.assertEqual(run.variables, ("temperature_2m", "relative_humidity_2m", "temperature_850hPa"))
        self.assertEqual(run.pressure_levels_hpa, (850,))

    def test_required_reference_times_include_one_run_before_required_start(self):
        latest = parse_openmeteo_spatial_latest("ncep_gfs025", _latest_payload())

        reference_times = required_reference_times_for_coverage(
            latest,
            required_start_utc=datetime(2026, 7, 7, 16, tzinfo=timezone.utc),
            run_cadence_hours=6,
        )

        self.assertEqual(
            [item.strftime("%Y%m%d%H") for item in reference_times],
            ["2026070712", "2026070718"],
        )

    def test_discover_openmeteo_spatial_runs_loads_previous_meta(self):
        latest = parse_openmeteo_spatial_latest("ncep_gfs025", _latest_payload())
        requested_urls = []

        def fetch(url):
            requested_urls.append(url)
            self.assertTrue(url.endswith("/data_spatial/ncep_gfs025/2026/07/07/1200Z/meta.json"))
            return json.dumps(
                [
                    {
                        "completed": True,
                        "reference_time": "2026-07-07T12:00:00Z",
                        "valid_times": ["2026-07-07T12:00Z", "2026-07-07T18:00Z"],
                        "variables": ["temperature_2m", "temperature_850hPa"],
                    }
                ]
            ).encode("utf-8")

        runs = discover_openmeteo_spatial_runs(
            "gfs025",
            latest,
            bucket_url="https://openmeteo.s3.amazonaws.com",
            required_start_utc=datetime(2026, 7, 7, 16, tzinfo=timezone.utc),
            run_cadence_hours=6,
            fetch=fetch,
        )

        self.assertEqual(requested_urls, ["https://openmeteo.s3.amazonaws.com/data_spatial/ncep_gfs025/2026/07/07/1200Z/meta.json"])
        self.assertEqual([run.run_id for run in runs], ["2026070712", "2026070718"])
        self.assertEqual(runs[0].max_forecast_hour, 6)

    def test_openmeteo_spatial_object_urls_follow_bucket_layout(self):
        key = openmeteo_spatial_object_key(
            "ncep_gfs025",
            reference_time_utc="2026-07-07T18:00:00Z",
            valid_time_utc="2026-07-08T00:00:00Z",
        )
        self.assertEqual(
            key,
            "data_spatial/ncep_gfs025/2026/07/07/1800Z/2026-07-08T0000.om",
        )
        self.assertEqual(
            openmeteo_spatial_object_url(
                "https://openmeteo.s3.amazonaws.com",
                "ncep_gfs025",
                reference_time_utc="2026-07-07T18:00:00Z",
                valid_time_utc="2026-07-08T00:00:00Z",
            ),
            "https://openmeteo.s3.amazonaws.com/data_spatial/ncep_gfs025/2026/07/07/1800Z/2026-07-08T0000.om",
        )
        self.assertEqual(
            openmeteo_spatial_run_meta_url(
                "https://openmeteo.s3.amazonaws.com/",
                "ncep_gfs025",
                reference_time_utc="2026-07-07T18:00:00Z",
            ),
            "https://openmeteo.s3.amazonaws.com/data_spatial/ncep_gfs025/2026/07/07/1800Z/meta.json",
        )

    def test_coverage_object_records_build_urls_for_slots(self):
        runs = [
            OmRun(
                "2026070712",
                datetime(2026, 7, 7, 12, tzinfo=timezone.utc),
                12,
                ("temperature_2m",),
                (),
            ),
            OmRun(
                "2026070718",
                datetime(2026, 7, 7, 18, tzinfo=timezone.utc),
                12,
                ("temperature_2m",),
                (),
            ),
        ]
        plan = CoveragePlan(
            product="gfs025",
            required_start_utc=datetime(2026, 7, 7, 16, tzinfo=timezone.utc),
            required_end_utc=datetime(2026, 7, 8, 0, tzinfo=timezone.utc),
            latest_complete_run="2026070718",
            slots=(
                CoverageSlot(datetime(2026, 7, 7, 16, tzinfo=timezone.utc), "2026070712", 4),
                CoverageSlot(datetime(2026, 7, 7, 18, tzinfo=timezone.utc), "2026070718", 0),
            ),
        )

        records = coverage_object_records(
            plan,
            runs,
            bucket_url="https://openmeteo.s3.amazonaws.com",
            openmeteo_model="ncep_gfs025",
        )

        self.assertEqual(
            records,
            [
                {
                    "valid_time_utc": "2026-07-07T16:00:00Z",
                    "source_run": "2026070712",
                    "forecast_hour": 4,
                    "url": "https://openmeteo.s3.amazonaws.com/data_spatial/ncep_gfs025/2026/07/07/1200Z/2026-07-07T1600.om",
                },
                {
                    "valid_time_utc": "2026-07-07T18:00:00Z",
                    "source_run": "2026070718",
                    "forecast_hour": 0,
                    "url": "https://openmeteo.s3.amazonaws.com/data_spatial/ncep_gfs025/2026/07/07/1800Z/2026-07-07T1800.om",
                },
            ],
        )


if __name__ == "__main__":
    unittest.main()
