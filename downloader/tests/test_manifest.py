import json
import tempfile
import unittest
from dataclasses import replace
from datetime import datetime, timezone
from pathlib import Path

from om_downloader.coverage import CoveragePlan, CoverageSlot
from om_downloader.manifest import (
    atomic_write_json,
    build_latest_manifest,
    product_config_fingerprint,
)
from om_downloader.metadata import OmRun
from om_downloader.model_config import Bounds, ProductConfig


def _region_plan():
    return {
        "requested_bounds": {"lon_min": 70.0, "lat_min": 0.0, "lon_max": 140.0, "lat_max": 58.0},
        "padded_bounds": {"lon_min": 68.0, "lat_min": -2.0, "lon_max": 142.0, "lat_max": 60.0},
        "grid_bounds": {"lon_min": 0.0, "lat_min": -90.0, "lon_max": 359.75, "lat_max": 90.0},
        "spatial_ranges": [{"grid_type": "regular_latlon", "x_range": [272, 569], "y_range": [352, 601]}],
    }


class ManifestTests(unittest.TestCase):
    def test_config_fingerprint_includes_first_available_forecast_hour(self):
        product = ProductConfig(
            name="ncep_gefs025",
            download_product="om_ncep_gefs025",
            openmeteo_model="ncep_gefs025",
            forecast_hour_end=240,
            run_cadence_hours=6,
            timezone_anchors=(8, 6),
            requested_bounds=Bounds(70.0, 0.0, 140.0, 58.0),
            bounds_padding_degrees=2.0,
            required_variables=("precipitation_probability",),
            optional_variables=(),
            requested_pressure_levels_hpa=(),
            forecast_hour_start=3,
        )

        self.assertNotEqual(
            product_config_fingerprint(product),
            product_config_fingerprint(replace(product, forecast_hour_start=0)),
        )

    def test_manifest_records_missing_pressure_levels(self):
        product = ProductConfig(
            name="gfs_pressure_profile",
            download_product="om_gfs_pressure_profile",
            openmeteo_model="ncep_gfs025",
            forecast_hour_end=384,
            run_cadence_hours=6,
            timezone_anchors=(8, 6),
            requested_bounds=Bounds(70.0, 0.0, 140.0, 58.0),
            bounds_padding_degrees=2.0,
            required_variables=("TMP", "RH"),
            optional_variables=("VVEL",),
            requested_pressure_levels_hpa=(1000, 975, 50),
        )
        run = OmRun(
            "2026070806",
            datetime(2026, 7, 8, 6, tzinfo=timezone.utc),
            384,
            ("TMP", "RH", "VVEL"),
            (1000, 50),
        )
        plan = CoveragePlan(
            product="gfs_pressure_profile",
            required_start_utc=datetime(2026, 7, 7, 16, tzinfo=timezone.utc),
            required_end_utc=datetime(2026, 7, 24, 6, tzinfo=timezone.utc),
            latest_complete_run="2026070806",
            slots=(CoverageSlot(datetime(2026, 7, 8, 6, tzinfo=timezone.utc), "2026070806", 0),),
        )
        manifest = build_latest_manifest(
            product,
            [run],
            plan,
            [{"path": "x.om", "bytes": 1, "sha256": "abc"}],
            _region_plan(),
        )
        self.assertEqual(manifest["status"], "incomplete")
        self.assertEqual(manifest["available_pressure_levels_hpa"], [1000, 50])
        self.assertEqual(manifest["missing_pressure_levels_hpa"], [975])

    def test_manifest_contains_region_and_download_gateway_fields(self):
        product = ProductConfig(
            name="gfs025",
            download_product="om_gfs025",
            openmeteo_model="ncep_gfs025",
            forecast_hour_end=384,
            run_cadence_hours=6,
            timezone_anchors=(8, 6),
            requested_bounds=Bounds(70.0, 0.0, 140.0, 58.0),
            bounds_padding_degrees=2.0,
            required_variables=("TMP",),
            optional_variables=("DPT",),
            requested_pressure_levels_hpa=(),
        )
        run = OmRun("2026070806", datetime(2026, 7, 8, 6, tzinfo=timezone.utc), 384, ("TMP",), ())
        plan = CoveragePlan(
            product="gfs025",
            required_start_utc=datetime(2026, 7, 7, 16, tzinfo=timezone.utc),
            required_end_utc=datetime(2026, 7, 24, 6, tzinfo=timezone.utc),
            latest_complete_run="2026070806",
            slots=(CoverageSlot(datetime(2026, 7, 8, 6, tzinfo=timezone.utc), "2026070806", 0),),
        )
        files = [{"path": "coverages/a.om", "bytes": 10, "sha256": "hash-a", "remote_content_length": 100, "downloaded_bytes": 10}]
        manifest = build_latest_manifest(product, [run], plan, files, _region_plan())
        self.assertEqual(manifest["status"], "complete")
        self.assertEqual(manifest["bytes"], 10)
        self.assertEqual(manifest["sha256"], {"coverages/a.om": "hash-a"})
        self.assertEqual(manifest["remote_content_length"], 100)
        self.assertEqual(manifest["downloaded_bytes"], 10)
        self.assertEqual(manifest["requested_bounds"]["lon_min"], 70.0)
        self.assertEqual(manifest["padded_bounds"]["lon_min"], 68.0)
        self.assertEqual(manifest["spatial_ranges"][0]["x_range"], [272, 569])

    def test_atomic_write_json_replaces_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "latest.json"
            atomic_write_json(path, {"status": "complete"})
            self.assertEqual(json.loads(path.read_text(encoding="utf-8"))["status"], "complete")


if __name__ == "__main__":
    unittest.main()
