import unittest

from om_downloader.om_byte_ranges import plan_array_data_byte_ranges, plan_array_lut_byte_ranges
from om_downloader.om_inventory import OmArrayInfo


def _array_info():
    return OmArrayInfo(
        name="temperature",
        path="root/temperature",
        data_type=20,
        compression=1,
        dimensions=(721, 1440, 385),
        chunks=(1, 50, 385),
        lut_offset=10_000,
        lut_size=32_700,
        scale_factor=1.0,
        add_offset=0.0,
    )


class OmByteRangeTests(unittest.TestCase):
    def test_plan_array_lut_byte_ranges_from_selection(self):
        ranges = plan_array_lut_byte_ranges(
            _array_info(),
            selection_ranges=((352, 354), (272, 569), (0, 385)),
        )
        self.assertEqual(ranges, [(25900, 26100)])

    def test_plan_array_data_byte_ranges_from_decoded_lut_offsets(self):
        array = OmArrayInfo(
            name="small",
            path="root/small",
            data_type=20,
            compression=1,
            dimensions=(2, 100, 1),
            chunks=(1, 50, 1),
            lut_offset=1000,
            lut_size=64,
            scale_factor=1.0,
            add_offset=0.0,
        )
        ranges = plan_array_data_byte_ranges(
            array,
            selection_ranges=((0, 2), (0, 100), (0, 1)),
            lut_offsets=[500, 525, 550, 600, 650],
        )
        self.assertEqual(ranges, [(500, 650)])

    def test_plan_array_data_byte_ranges_requires_lut_offsets(self):
        with self.assertRaises(ValueError) as ctx:
            plan_array_data_byte_ranges(
                _array_info(),
                selection_ranges=((352, 354), (272, 569), (0, 385)),
                lut_offsets=None,
            )
        self.assertIn("decoded LUT offsets", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
