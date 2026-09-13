from __future__ import annotations

import sys
from pathlib import Path
import unittest

VALIDATION_ROOT = Path(__file__).resolve().parents[1]
if str(VALIDATION_ROOT) not in sys.path:
    sys.path.insert(0, str(VALIDATION_ROOT))

import friction_velocity_200_compare as compare


class FrictionVelocity200CompareTests(unittest.TestCase):
    def test_point_url_uses_only_native_field_and_exact_sampling_options(self) -> None:
        url = compare.point_url(
            "http://127.0.0.1:18080", {"latitude": 1.25, "longitude": 103.75}
        )
        self.assertIn("hourly=friction_velocity", url)
        self.assertIn("cell_selection=nearest", url)
        self.assertIn("wind_speed_unit=ms", url)

    def test_projection_rejects_wrong_unit(self) -> None:
        raw = b'{"hourly_units":{"friction_velocity":"km/h"},"hourly":{"time":["t"],"friction_velocity":[1.0]}}'
        with self.assertRaises(compare.ValidationError):
            compare.projection(raw)

    def test_first_difference_is_strict(self) -> None:
        reference = {
            "hourly_units": {"friction_velocity": "m/s"},
            "hourly": {"time": ["t"], "friction_velocity": [0.123]},
        }
        self.assertIsNone(compare.first_difference(reference, reference))
        local = {
            "hourly_units": {"friction_velocity": "m/s"},
            "hourly": {"time": ["t"], "friction_velocity": [0.124]},
        }
        difference = compare.first_difference(reference, local)
        self.assertEqual(difference["variable"], "friction_velocity")


if __name__ == "__main__":
    unittest.main()
