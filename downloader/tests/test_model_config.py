import json
import tempfile
import unittest
from pathlib import Path

from om_downloader.model_config import load_models


class ModelConfigTests(unittest.TestCase):
    def test_loads_gfs_pressure_profile_requested_levels(self):
        config = load_models(Path("config/models.json"))
        product = config.products["gfs_pressure_profile"]
        self.assertEqual(product.openmeteo_model, "ncep_gfs025")
        self.assertEqual(product.forecast_hour_end, 384)
        self.assertEqual(product.requested_bounds.lon_min, 70.0)
        self.assertEqual(product.bounds_padding_degrees, 2.0)
        self.assertIn(1000, product.requested_pressure_levels_hpa)
        self.assertIn(50, product.requested_pressure_levels_hpa)

    def test_expands_pressure_level_variable_prefixes(self):
        config = load_models(Path("config/models.json"))
        product = config.products["gfs_pressure_profile"]
        self.assertIn("temperature_850hPa", product.required_variables)
        self.assertIn("relative_humidity_1000hPa", product.required_variables)
        self.assertIn("wind_u_component_50hPa", product.required_variables)
        self.assertIn("geopotential_height_500hPa", product.required_variables)
        self.assertIn("vertical_velocity_850hPa", product.optional_variables)
        self.assertNotIn("TMP", product.required_variables)

    def test_loads_openmeteo_source_models(self):
        config = load_models(Path("config/models.json"))
        self.assertEqual(config.products["gfs013_surface"].openmeteo_model, "ncep_gfs013")
        self.assertEqual(config.products["gfs025"].openmeteo_model, "ncep_gfs025")
        self.assertEqual(config.products["cams_global"].openmeteo_model, "cams_global")

    def test_includes_singapore_production_surface_and_cams_variables(self):
        config = load_models(Path("config/models.json"))

        gfs013 = config.products["gfs013_surface"]
        for variable in ("frozen_precipitation_percent", "pressure_msl"):
            self.assertIn(variable, gfs013.optional_variables)
            self.assertNotIn(variable, gfs013.required_variables)

        gfs025 = config.products["gfs025"]
        self.assertIn("latent_heat_flux", gfs025.optional_variables)
        self.assertNotIn("latent_heat_flux", gfs025.required_variables)

        cams = config.products["cams_global"]
        for variable in (
            "aerosol_optical_depth",
            "pm2_5",
            "pm10",
            "dust",
            "carbon_monoxide",
            "nitrogen_dioxide",
            "ozone",
            "sulphur_dioxide",
        ):
            self.assertIn(variable, cams.required_variables)
            self.assertNotIn(variable, cams.optional_variables)

    def test_rejects_missing_required_variables_key(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "models.json"
            path.write_text(json.dumps({"products": {"bad": {"forecast_hour_end": 1}}}), encoding="utf-8")
            with self.assertRaises(ValueError) as ctx:
                load_models(path)
            self.assertIn("required_variables", str(ctx.exception))

    def test_rejects_fallback_context_off_the_three_hour_axis(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "models.json"
            path.write_text(
                json.dumps(
                    {
                        "products": {
                            "bad": {
                                "download_product": "bad",
                                "forecast_hour_end": 1,
                                "run_cadence_hours": 6,
                                "timezone_anchors": [0],
                                "requested_bounds": {
                                    "lon_min": 0,
                                    "lat_min": 0,
                                    "lon_max": 1,
                                    "lat_max": 1,
                                },
                                "bounds_padding_degrees": 0,
                                "required_variables": [],
                                "missing_variable_fallback_context_hours": 4,
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(ValueError) as ctx:
                load_models(path)
            self.assertIn("three-hour regularization axis", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
