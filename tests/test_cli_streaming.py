from datetime import datetime, timezone
import json
from types import SimpleNamespace
import tempfile
import time
from pathlib import Path
import unittest

from om_downloader import cli
from om_downloader.coverage import CoveragePlan, CoverageSlot
from om_downloader.metadata import OmRun
from om_downloader.model_config import Bounds, ProductConfig


class CliStreamingTests(unittest.TestCase):
    def test_download_product_streams_planned_entries_to_writer(self):
        events = []
        originals = {
            "coverage_object_records": cli.coverage_object_records,
            "HttpByteRangeSource": cli.HttpByteRangeSource,
            "load_remote_om_inventory": cli.load_remote_om_inventory,
            "load_remote_om_inventory_fast": cli.load_remote_om_inventory_fast,
            "plan_region_for_array": cli.plan_region_for_array,
            "plan_variable_range_bundle": cli.plan_variable_range_bundle,
            "write_om_coverage_bundle_file": cli.write_om_coverage_bundle_file,
        }

        product = ProductConfig(
            name="gfs025",
            download_product="om_gfs025",
            openmeteo_model="ncep_gfs025",
            forecast_hour_end=2,
            run_cadence_hours=6,
            timezone_anchors=(8, 6),
            requested_bounds=Bounds(lon_min=70.0, lat_min=0.0, lon_max=70.1, lat_max=0.1),
            bounds_padding_degrees=0.0,
            required_variables=("temperature_2m",),
            optional_variables=(),
            requested_pressure_levels_hpa=(),
        )
        base_time = datetime(2026, 7, 8, tzinfo=timezone.utc)
        runs = [
            OmRun(
                run_id="2026070800",
                base_time_utc=base_time,
                max_forecast_hour=2,
                variables=("temperature_2m",),
                pressure_levels_hpa=(),
                valid_times_utc=(),
            )
        ]
        plan = CoveragePlan(
            product="gfs025",
            required_start_utc=base_time,
            required_end_utc=base_time,
            latest_complete_run="2026070800",
            slots=(
                CoverageSlot(base_time, "2026070800", 0),
                CoverageSlot(base_time, "2026070800", 1),
            ),
        )
        object_records = [
            {
                "url": "https://example.test/first.om",
                "valid_time_utc": "2026-07-08T00:00:00Z",
                "source_run": "2026070800",
                "forecast_hour": 0,
            },
            {
                "url": "https://example.test/second.om",
                "valid_time_utc": "2026-07-08T01:00:00Z",
                "source_run": "2026070800",
                "forecast_hour": 1,
            },
        ]

        def fake_coverage_object_records(*_args, **_kwargs):
            return object_records

        class FakeSource:
            def __init__(self, url):
                self.url = url

            def content_length(self):
                return 100

        def fake_load_remote_om_inventory_fast(_source, _wanted_variables, **_kwargs):
            array = SimpleNamespace(name="temperature_2m")
            return SimpleNamespace(arrays={"temperature_2m": array})

        def fake_plan_region_for_array(_product, _array):
            return (
                {
                    "requested_bounds": {},
                    "padded_bounds": {},
                    "grid_bounds": {},
                    "grid": {},
                    "spatial_ranges": [{"y_range": [0, 1], "x_range": [0, 1]}],
                },
                ((0, 1), (0, 1)),
            )

        def fake_plan_variable_range_bundle(source, array, **_kwargs):
            self.assertEqual(_kwargs.get("io_size_merge"), 123456)
            if "second" in source.url:
                events.append("plan:second:start")
                time.sleep(0.3)
                events.append("plan:second:done")
            else:
                events.append("plan:first:done")
            return {
                "variable": array.name,
                "path": array.name,
                "selection_ranges": [[0, 1], [0, 1]],
                "array": {},
                "lut_byte_ranges": [],
                "data_byte_ranges": [],
                "lut_bytes_read": 0,
                "byte_ranges": [],
            }

        def fake_write_om_coverage_bundle_file(_output_root, _model, _coverage_id, entries, **_kwargs):
            events.append("writer:start")
            consumed = []
            for entry in entries:
                events.append(f"writer:consume:{entry['source_url']}")
                consumed.append(entry)
            return {
                "kind": "om_coverage_bundle",
                "path": "coverages/gfs025_2026070800_2h/gfs025.omranges",
                "bytes": len(consumed),
                "sha256": "0" * 64,
                "entries": [],
                "remote_content_length": None,
                "downloaded_bytes": len(consumed),
                "reused_existing": False,
            }

        cli.coverage_object_records = fake_coverage_object_records
        cli.HttpByteRangeSource = FakeSource
        cli.load_remote_om_inventory_fast = fake_load_remote_om_inventory_fast
        cli.plan_region_for_array = fake_plan_region_for_array
        cli.plan_variable_range_bundle = fake_plan_variable_range_bundle
        cli.write_om_coverage_bundle_file = fake_write_om_coverage_bundle_file
        try:
            with tempfile.TemporaryDirectory() as tmp:
                cli._download_openmeteo_product(
                    product,
                    now_utc=base_time,
                    output_root=Path(tmp),
                    bucket_url="https://example.test",
                    lut_codec="plain",
                    download_workers=2,
                    planning_workers=None,
                    range_workers=None,
                    range_io_merge_gap=123456,
                    range_io_size_max=None,
                    plan_data=(None, runs, plan),
                )
        finally:
            for name, value in originals.items():
                setattr(cli, name, value)

        self.assertLess(
            events.index("writer:consume:https://example.test/first.om"),
            events.index("plan:second:done"),
        )

    def test_manifest_match_rejects_missing_tmp_and_fingerprint_mismatch(self):
        product = ProductConfig(
            name="gfs025",
            download_product="om_gfs025",
            openmeteo_model="ncep_gfs025",
            forecast_hour_end=2,
            run_cadence_hours=6,
            timezone_anchors=(8, 6),
            requested_bounds=Bounds(lon_min=70.0, lat_min=0.0, lon_max=70.1, lat_max=0.1),
            bounds_padding_degrees=0.0,
            required_variables=("temperature_2m",),
            optional_variables=(),
            requested_pressure_levels_hpa=(),
        )
        base_time = datetime(2026, 7, 8, tzinfo=timezone.utc)
        plan = CoveragePlan(
            product="gfs025",
            required_start_utc=base_time,
            required_end_utc=base_time,
            latest_complete_run="2026070800",
            slots=(CoverageSlot(base_time, "2026070800", 0),),
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            published = root / "published" / "gfs025"
            bundle = published / "coverages" / "gfs025_2026070800_1h" / "gfs025.omranges"
            bundle.parent.mkdir(parents=True)
            bundle.write_bytes(b"abc")
            valid_manifest = {
                "status": "complete",
                "model": "gfs025",
                "coverage_id": "gfs025_2026070800_1h",
                "latest_complete_run": "2026070800",
                "required_start_utc": "2026-07-08T00:00:00Z",
                "required_end_utc": "2026-07-08T00:00:00Z",
                "valid_time_count": 1,
                "config_fingerprint": cli.product_config_fingerprint(product),
                "files": [
                    {
                        "kind": "om_coverage_bundle",
                        "path": "coverages/gfs025_2026070800_1h/gfs025.omranges",
                        "bytes": 3,
                        "downloaded_bytes": 3,
                        "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
                        "entries": [{"bundle_offset": 0, "bundle_bytes": 3}],
                    }
                ],
                "bytes": 3,
                "downloaded_bytes": 3,
            }

            self.assertTrue(cli._manifest_matches_plan(valid_manifest, plan, product, root))
            missing_file_manifest = json.loads(json.dumps(valid_manifest))
            bundle.unlink()
            self.assertFalse(cli._manifest_matches_plan(missing_file_manifest, plan, product, root))
            bundle.write_bytes(b"abc")
            tmp_manifest = json.loads(json.dumps(valid_manifest))
            tmp_manifest["files"][0]["path"] += ".tmp"
            self.assertFalse(cli._manifest_matches_plan(tmp_manifest, plan, product, root))
            fingerprint_manifest = json.loads(json.dumps(valid_manifest))
            fingerprint_manifest["config_fingerprint"] = "different"
            self.assertFalse(cli._manifest_matches_plan(fingerprint_manifest, plan, product, root))


if __name__ == "__main__":
    unittest.main()
