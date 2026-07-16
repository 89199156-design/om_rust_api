import struct
import unittest

from om_downloader.model_config import Bounds, ProductConfig
from om_downloader.om_inventory import OmArrayInfo, OmInventory
from om_downloader.om_product_download import (
    plan_region_for_array,
    plan_variable_range_bundle,
    selected_inventory_variables,
)


class _FakeByteRangeSource:
    def __init__(self, data):
        self.data = data
        self.requests = []

    def read_range(self, start, end):
        self.requests.append((start, end))
        return self.data[start:end]


def _product():
    return ProductConfig(
        name="gfs025",
        download_product="om_gfs025",
        openmeteo_model="ncep_gfs025",
        forecast_hour_end=384,
        run_cadence_hours=6,
        timezone_anchors=(8, 6),
        requested_bounds=Bounds(lon_min=70.0, lat_min=0.0, lon_max=140.0, lat_max=58.0),
        bounds_padding_degrees=2.0,
        required_variables=("temperature_2m", "pressure_msl"),
        optional_variables=("wind_gusts_10m", "missing_optional"),
        requested_pressure_levels_hpa=(),
    )


def _array(name="temperature_2m", *, lut_offset=100, lut_size=40):
    return OmArrayInfo(
        name=name,
        path=name,
        data_type=20,
        compression=0,
        dimensions=(721, 1440),
        chunks=(1, 1),
        lut_offset=lut_offset,
        lut_size=lut_size,
        scale_factor=1.0,
        add_offset=0.0,
    )


class OmProductDownloadTests(unittest.TestCase):
    def test_selected_inventory_variables_keeps_required_then_available_optional(self):
        inventory = OmInventory(
            arrays={
                "temperature_2m": _array("temperature_2m"),
                "wind_gusts_10m": _array("wind_gusts_10m"),
                "extra": _array("extra"),
            },
            pressure_levels_hpa=[],
        )

        self.assertEqual(
            selected_inventory_variables(_product(), inventory),
            ("temperature_2m", "wind_gusts_10m"),
        )

    def test_plan_region_for_array_uses_openmeteo_grid_and_array_dimensions(self):
        region_plan, selection_ranges = plan_region_for_array(_product(), _array())

        self.assertEqual(region_plan["spatial_ranges"][0]["x_range"], [992, 1289])
        self.assertEqual(region_plan["spatial_ranges"][0]["y_range"], [352, 601])
        self.assertEqual(selection_ranges, ((352, 601), (992, 1289)))

    def test_plan_variable_range_bundle_combines_lut_and_data_ranges(self):
        lut_values = [2000, 2020, 2060, 2100, 2140]
        lut_payload = b"".join(struct.pack("<Q", item) for item in lut_values)
        data = bytearray(200)
        data[100 : 100 + len(lut_payload)] = lut_payload
        source = _FakeByteRangeSource(bytes(data))
        array = OmArrayInfo(
            name="temperature_2m",
            path="temperature_2m",
            data_type=20,
            compression=0,
            dimensions=(4, 1),
            chunks=(1, 1),
            lut_offset=100,
            lut_size=len(lut_payload),
            scale_factor=1.0,
            add_offset=0.0,
        )

        bundle = plan_variable_range_bundle(
            source,
            array,
            selection_ranges=((1, 3), (0, 1)),
            lut_codec="plain",
        )

        self.assertEqual(source.requests, [(100, 140)])
        self.assertEqual(bundle["variable"], "temperature_2m")
        self.assertEqual(bundle["lut_byte_ranges"], [[100, 140]])
        self.assertEqual(bundle["data_byte_ranges"], [[2020, 2100]])
        self.assertEqual([item.as_manifest() for item in bundle["byte_ranges"]], [[100, 139], [2020, 2099]])

    def test_plan_variable_range_bundle_accepts_larger_io_size_to_avoid_tiny_splits(self):
        lut_values = [0, 200_000]
        lut_payload = b"".join(struct.pack("<Q", item) for item in lut_values)
        data = bytearray(100 + len(lut_payload))
        data[100 : 100 + len(lut_payload)] = lut_payload
        source = _FakeByteRangeSource(bytes(data))
        array = OmArrayInfo(
            name="temperature_2m",
            path="temperature_2m",
            data_type=20,
            compression=0,
            dimensions=(1, 1),
            chunks=(1, 1),
            lut_offset=100,
            lut_size=len(lut_payload),
            scale_factor=1.0,
            add_offset=0.0,
        )

        bundle = plan_variable_range_bundle(
            source,
            array,
            selection_ranges=((0, 1), (0, 1)),
            lut_codec="plain",
            io_size_max=512 * 1024,
        )

        self.assertEqual(bundle["data_byte_ranges"], [[0, 200_000]])
        self.assertEqual([item.as_manifest() for item in bundle["byte_ranges"]], [[0, 199999]])

    def test_plan_variable_range_bundle_accepts_larger_merge_gap_to_reduce_small_ranges(self):
        lut_values = [1000, 1020, 2000, 70000, 70020, 80000, 90000]
        lut_payload = b"".join(struct.pack("<Q", item) for item in lut_values)
        data = bytearray(100 + len(lut_payload))
        data[100 : 100 + len(lut_payload)] = lut_payload
        source = _FakeByteRangeSource(bytes(data))
        array = OmArrayInfo(
            name="temperature_2m",
            path="temperature_2m",
            data_type=20,
            compression=0,
            dimensions=(2, 3),
            chunks=(1, 1),
            lut_offset=100,
            lut_size=len(lut_payload),
            scale_factor=1.0,
            add_offset=0.0,
        )

        conservative = plan_variable_range_bundle(
            source,
            array,
            selection_ranges=((0, 2), (0, 1)),
            lut_codec="plain",
            io_size_merge=64 * 1024,
        )
        wider = plan_variable_range_bundle(
            source,
            array,
            selection_ranges=((0, 2), (0, 1)),
            lut_codec="plain",
            io_size_merge=128 * 1024,
        )

        self.assertEqual(conservative["data_byte_ranges"], [[1000, 1020], [70000, 70020]])
        self.assertEqual(wider["data_byte_ranges"], [[1000, 70020]])
        self.assertEqual([item.as_manifest() for item in wider["byte_ranges"]], [[100, 155], [1000, 70019]])


if __name__ == "__main__":
    unittest.main()
