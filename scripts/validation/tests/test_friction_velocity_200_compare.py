from __future__ import annotations

import sys
from pathlib import Path
import struct
import unittest
from unittest import mock

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

    def test_reference_url_uses_flatbuffers_alias_without_losing_float_precision(self) -> None:
        url = compare.reference_point_url(
            "http://127.0.0.1:18080",
            {"latitude": 1.25, "longitude": 103.75},
            "wind_gusts_10m",
        )
        self.assertIn("hourly=wind_gusts_10m", url)
        self.assertIn("format=flatbuffers", url)
        self.assertIn("timeformat=unixtime", url)

    def test_flatbuffers_projection_preserves_three_decimal_contract(self) -> None:
        class FakeVariable:
            def Variable(self):
                return 58

            def Unit(self):
                return 28

            def ValuesLength(self):
                return 2

            def Values(self, index):
                return (0.17299999, float("nan"))[index]

        class FakeHourly:
            def VariablesLength(self):
                return 1

            def Variables(self, _index):
                return FakeVariable()

            def Interval(self):
                return 3600

            def Time(self):
                return 1_789_344_000

            def TimeEnd(self):
                return 1_789_351_200

        class FakeResponse:
            def Hourly(self):
                return FakeHourly()

        class FakeResponseType:
            @staticmethod
            def GetRootAs(_raw, _offset):
                return FakeResponse()

        class FakeVariableEnum:
            wind_gusts = 58

        class FakeUnitEnum:
            metre_per_second = 28

        raw = struct.pack("<I", 4) + b"test"
        with mock.patch.object(
            compare,
            "load_flatbuffers_sdk",
            return_value=(FakeResponseType, FakeVariableEnum, FakeUnitEnum),
        ):
            result = compare.flatbuffers_projection(raw, Path("unused"))

        self.assertEqual(result["hourly"]["friction_velocity"], [0.173, None])
        self.assertEqual(
            result["hourly"]["time"],
            ["2026-09-14T00:00", "2026-09-14T01:00"],
        )

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

    def test_null_reference_tail_after_local_model_end_is_allowed(self) -> None:
        reference = {
            "hourly_units": {"friction_velocity": "m/s"},
            "hourly": {
                "time": ["a", "b", "c"],
                "friction_velocity": [0.123, 0.124, None],
            },
        }
        local = {
            "hourly_units": {"friction_velocity": "m/s"},
            "hourly": {
                "time": ["a", "b"],
                "friction_velocity": [0.123, 0.124],
            },
        }
        self.assertIsNone(compare.first_difference(reference, local))

        reference["hourly"]["friction_velocity"][-1] = 0.125
        difference = compare.first_difference(reference, local)
        self.assertEqual(
            difference["reason"], "finite_reference_value_after_local_model_end"
        )


if __name__ == "__main__":
    unittest.main()
