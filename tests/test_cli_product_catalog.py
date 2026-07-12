import json
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def _write_config(path):
    path.write_text(
        json.dumps(
            {
                "version": 1,
                "products": {
                    "gfs025": {
                        "download_product": "om_gfs025",
                        "openmeteo_model": "ncep_gfs025",
                        "forecast_hour_end": 384,
                        "run_cadence_hours": 6,
                        "timezone_anchors": [8, 6],
                        "requested_bounds": {
                            "lon_min": 70.0,
                            "lat_min": 0.0,
                            "lon_max": 140.0,
                            "lat_max": 58.0,
                        },
                        "bounds_padding_degrees": 2.0,
                        "required_variables": ["temperature_2m", "pressure_msl"],
                        "optional_variables": ["wind_gusts_10m", "missing_optional"],
                        "requested_pressure_levels_hpa": [],
                    }
                },
            }
        ),
        encoding="utf-8",
    )


class _ProductCatalogHandler(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, _format, *_args):
        return

    def do_GET(self):
        self.requests.append(self.path)
        if self.path != "/data_spatial/ncep_gfs025/latest.json":
            if self.path == "/data_spatial/ncep_gfs025/2026/07/07/1200Z/meta.json":
                payload = json.dumps(
                    [
                        {
                            "completed": True,
                            "reference_time": "2026-07-07T12:00:00Z",
                            "valid_times": [
                                "2026-07-07T12:00Z",
                                "2026-07-07T16:00Z",
                                "2026-07-07T18:00Z",
                                "2026-07-08T00:00Z",
                            ],
                            "variables": ["temperature_2m", "wind_gusts_10m"],
                        }
                    ]
                ).encode("utf-8")
            else:
                self.send_response(404)
                self.end_headers()
                return
        else:
            payload = json.dumps(
                [
                    {
                        "completed": True,
                        "reference_time": "2026-07-07T18:00:00Z",
                        "valid_times": ["2026-07-07T18:00Z", "2026-07-08T00:00Z"],
                        "variables": ["temperature_2m", "wind_gusts_10m"],
                    }
                ]
            ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class CliProductCatalogTests(unittest.TestCase):
    def setUp(self):
        _ProductCatalogHandler.requests = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _ProductCatalogHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.bucket_url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def test_cli_validates_product_required_variables_against_catalog(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "models.json"
            _write_config(config)
            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--inspect-product-catalog",
                    "gfs025",
                    "--config",
                    str(config),
                    "--openmeteo-bucket-url",
                    self.bucket_url,
                    "--now",
                    "2026-07-08T14:00:00Z",
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )

        payload = json.loads(result.stdout)
        self.assertEqual(payload["product"], "gfs025")
        self.assertEqual(payload["openmeteo_model"], "ncep_gfs025")
        self.assertEqual(payload["missing_required_variables"], ["pressure_msl"])
        self.assertEqual(payload["missing_optional_variables"], ["missing_optional"])
        self.assertEqual(payload["max_forecast_hour"], 6)
        self.assertEqual(payload["required_start_utc"], "2026-07-07T16:00:00Z")
        self.assertEqual(payload["required_end_utc"], "2026-07-08T00:00:00Z")
        self.assertEqual(payload["latest_complete_run"], "2026070718")
        self.assertEqual(payload["valid_time_count"], 3)
        self.assertEqual(payload["source_runs"], ["2026070712", "2026070718"])
        self.assertEqual(payload["object_count"], 3)
        self.assertEqual(
            payload["first_object_url"],
            f"{self.bucket_url}/data_spatial/ncep_gfs025/2026/07/07/1200Z/2026-07-07T1600.om",
        )
        self.assertEqual(
            payload["last_object_url"],
            f"{self.bucket_url}/data_spatial/ncep_gfs025/2026/07/07/1800Z/2026-07-08T0000.om",
        )
        self.assertEqual(
            _ProductCatalogHandler.requests,
            [
                "/data_spatial/ncep_gfs025/latest.json",
                "/data_spatial/ncep_gfs025/2026/07/07/1200Z/meta.json",
            ],
        )


if __name__ == "__main__":
    unittest.main()
