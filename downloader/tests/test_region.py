import unittest

from om_downloader.model_config import Bounds
from om_downloader.region import GridSpec, grid_spec_for_openmeteo_model, padded_bounds, regular_grid_ranges


class RegionTests(unittest.TestCase):
    def test_padding_expands_and_clamps_to_grid_bounds(self):
        requested = Bounds(lon_min=70.0, lat_min=0.0, lon_max=140.0, lat_max=58.0)
        grid = Bounds(lon_min=0.0, lat_min=-90.0, lon_max=359.75, lat_max=90.0)
        padded = padded_bounds(requested, padding_degrees=2.0, grid_bounds=grid)
        self.assertEqual(padded.lon_min, 68.0)
        self.assertEqual(padded.lon_max, 142.0)
        self.assertEqual(padded.lat_min, -2.0)
        self.assertEqual(padded.lat_max, 60.0)

    def test_regular_grid_ranges_are_not_global_for_china_bounds(self):
        grid = grid_spec_for_openmeteo_model("ncep_gfs025", dimensions=(721, 1440))
        bounds = Bounds(lon_min=68.0, lat_min=-2.0, lon_max=142.0, lat_max=60.0)
        result = regular_grid_ranges(grid, bounds)
        self.assertEqual(result["x_range"], [992, 1289])
        self.assertEqual(result["y_range"], [352, 601])
        self.assertLess(result["location_count"], grid.nx * grid.ny)
        self.assertFalse(result["is_global"])

    def test_grid_spec_for_openmeteo_model_uses_actual_gfs013_grid(self):
        grid = grid_spec_for_openmeteo_model("ncep_gfs013", dimensions=(1536, 3072))

        self.assertEqual(grid.nx, 3072)
        self.assertEqual(grid.ny, 1536)
        self.assertEqual(grid.lon_min, -180.0)
        self.assertAlmostEqual(grid.dx, 360 / 3072)
        self.assertAlmostEqual(grid.lat_min, -0.11714935 * (1536 - 1) / 2)
        self.assertAlmostEqual(grid.dy, 0.11714935)

    def test_grid_spec_for_openmeteo_model_rejects_dimension_mismatch(self):
        with self.assertRaises(ValueError) as ctx:
            grid_spec_for_openmeteo_model("ncep_gfs025", dimensions=(1536, 3072))
        self.assertIn("do not match", str(ctx.exception))

    def test_regular_grid_ranges_refuse_global_selection(self):
        grid = GridSpec(
            grid_type="regular_latlon",
            nx=1440,
            ny=721,
            lon_min=0.0,
            lat_min=-90.0,
            dx=0.25,
            dy=0.25,
        )
        with self.assertRaises(ValueError) as ctx:
            regular_grid_ranges(grid, grid.bounds)
        self.assertIn("refusing global", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
