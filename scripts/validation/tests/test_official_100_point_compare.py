from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest


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
        self.assertEqual(compare.CAMS_HOURLY_LOCAL, compare.CAMS_HOURLY_OFFICIAL)
        self.assertEqual(compare.CAMS_DAILY, ())
        self.assertNotIn("chinese_aqi", compare.CAMS_HOURLY_LOCAL)

    def test_official_payload_uses_one_multi_location_request(self) -> None:
        payload = compare.official_payload("gfs", compare.sample_points())
        self.assertEqual(len(payload["latitude"]), 100)
        self.assertEqual(len(payload["longitude"]), 100)
        self.assertEqual(payload["cell_selection"], "nearest")
        self.assertIn("precipitation_probability", payload["hourly"])
        self.assertIn("precipitation_probability_max", payload["daily"])

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


if __name__ == "__main__":
    unittest.main()
