import importlib.util
from pathlib import Path
import tempfile
import unittest

import numpy as np
from PIL import Image


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "build_stargazing_layers.py"
SPEC = importlib.util.spec_from_file_location("build_stargazing_layers", SCRIPT)
assert SPEC and SPEC.loader
module = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(module)


class StargazingLayerBuilderTest(unittest.TestCase):
    def test_score_rounding_uses_half_up(self):
        values = np.array(
            [0.0, 0.49, 0.5, 1.49, 1.5, 99.49, 99.5, 100.0, 100.4, np.nan, np.inf, -np.inf],
            dtype=np.float32,
        )
        rounded = module.round_score_half_up(values)
        self.assertEqual(rounded.tolist(), [0, 0, 1, 1, 2, 99, 100, 100, 100, 0, 100, 0])

    def test_clear_dark_conditions_score_higher_than_cloud_or_daylight(self):
        shape = (2, 3)
        zeros = np.zeros(shape, dtype=np.float32)
        common = dict(
            artificial=np.full(shape, 40.0, dtype=np.float32),
            moon_elevation=zeros,
            moon_phase=0.0,
            visibility_m=np.full(shape, 30_000.0, dtype=np.float32),
            humidity=np.full(shape, 45.0, dtype=np.float32),
            temperature=np.full(shape, 10.0, dtype=np.float32),
            dew_point=np.full(shape, 2.0, dtype=np.float32),
            wind=np.full(shape, 2.0, dtype=np.float32),
            gust=np.full(shape, 3.0, dtype=np.float32),
            precipitation=zeros,
            thunder=zeros,
            aod=np.full(shape, 0.08, dtype=np.float32),
        )
        clear, valid, clear_magnitude, clear_weather = module.score_frame(
            **common,
            sun_elevation=np.full(shape, -30.0),
            cloud=zeros,
        )
        cloudy, _, _, cloudy_weather = module.score_frame(
            **common,
            sun_elevation=np.full(shape, -30.0),
            cloud=np.full(shape, 90.0),
        )
        daylight, _, _, _ = module.score_frame(
            **common,
            sun_elevation=np.full(shape, 20.0),
            cloud=zeros,
        )
        self.assertTrue(valid.all())
        self.assertTrue(np.all(clear > cloudy))
        self.assertTrue(np.all(clear_weather > cloudy_weather))
        self.assertTrue(np.isfinite(clear_magnitude).all())
        self.assertTrue(np.all(daylight == 0))

        gated, _, _, gated_weather = module.score_frame(
            **(common | {"precipitation": np.full(shape, 0.05, dtype=np.float32)}),
            sun_elevation=np.full(shape, -30.0),
            cloud=zeros,
        )
        self.assertTrue(np.all(gated == 0))
        self.assertTrue(np.all(gated_weather == 0.0))

    def test_missing_input_is_transparent_and_score_round_trips(self):
        score = np.array([[0, 49], [80, 100]], dtype=np.uint8)
        valid = np.array([[True, False], [True, True]])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "score.webp"
            module.encode_score(path, score, valid)
            with Image.open(path) as image:
                rgba = np.asarray(image.convert("RGBA"))
        decoded = (rgba[..., 0].astype(np.uint16) << 8) | rgba[..., 1].astype(np.uint16)
        expected = np.where(valid, score, 0).astype(np.uint16)
        self.assertEqual(decoded.tolist(), expected.tolist())
        self.assertEqual(rgba[..., 3].tolist(), [[255, 0], [255, 255]])

    def test_details_round_trip_is_map_matched_and_compact(self):
        magnitude = np.array([[21.31, 18.004], [22.0, np.nan]], dtype=np.float32)
        weather = np.array([[0.525, 1.0], [0.0, 0.75]], dtype=np.float32)
        score = np.array([[28, 100], [0, 42]], dtype=np.uint8)
        valid = np.array([[True, True], [True, False]])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "details.webp"
            module.encode_details(path, magnitude, weather, score, valid)
            with Image.open(path) as image:
                rgb = np.asarray(image.convert("RGB"), dtype=np.uint32)
        packed = (rgb[..., 0] << 16) | (rgb[..., 1] << 8) | rgb[..., 2]
        magnitude_code = packed >> 14
        decoded_magnitude = module.DETAIL_MAG_MIN + (magnitude_code - 1) * module.DETAIL_MAG_STEP
        decoded_weather = (packed >> 7) & 0x7F
        decoded_score = packed & 0x7F
        self.assertAlmostEqual(float(decoded_magnitude[0, 0]), 21.32, places=2)
        self.assertAlmostEqual(float(decoded_magnitude[0, 1]), 18.00, places=2)
        self.assertEqual(decoded_weather.tolist(), [[53, 100], [0, 0]])
        self.assertEqual(decoded_score.tolist(), [[28, 100], [0, 0]])
        self.assertEqual(int(packed[1, 1]), 0)

    def test_geometry_has_day_night_separation_and_finite_moon(self):
        timestamp = 1_725_192_000  # 2024-09-01 12:00:00 UTC
        sun, moon, phase = module.celestial_geometry(
            timestamp,
            np.array([0.0]),
            np.array([0.0, 180.0]),
        )
        self.assertGreater(float(sun[0, 0]), 60.0)
        self.assertLess(float(sun[0, 1]), -60.0)
        self.assertTrue(np.isfinite(moon).all())
        self.assertGreaterEqual(phase, 0.0)
        self.assertLess(phase, 360.0)


if __name__ == "__main__":
    unittest.main()
