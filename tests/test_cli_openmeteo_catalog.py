import json
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from tests.test_om_catalog import _latest_payload


class _CatalogHandler(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, _format, *_args):
        return

    def do_GET(self):
        self.requests.append(self.path)
        if self.path != "/data_spatial/ncep_gfs025/latest.json":
            self.send_response(404)
            self.end_headers()
            return
        payload = _latest_payload()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class CliOpenMeteoCatalogTests(unittest.TestCase):
    def setUp(self):
        _CatalogHandler.requests = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _CatalogHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.bucket_url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def test_cli_inspects_openmeteo_spatial_latest_catalog(self):
        result = subprocess.run(
            [
                sys.executable,
                "-m",
                "om_downloader.cli",
                "--inspect-openmeteo-model",
                "ncep_gfs025",
                "--openmeteo-bucket-url",
                self.bucket_url,
            ],
            cwd=Path.cwd(),
            text=True,
            capture_output=True,
            check=True,
        )

        payload = json.loads(result.stdout)
        self.assertEqual(_CatalogHandler.requests, ["/data_spatial/ncep_gfs025/latest.json"])
        self.assertEqual(payload["model"], "ncep_gfs025")
        self.assertEqual(payload["reference_time"], "2026-07-07T18:00:00Z")
        self.assertEqual(payload["max_forecast_hour"], 12)
        self.assertEqual(payload["valid_time_count"], 4)
        self.assertEqual(payload["variable_count"], 3)
        self.assertEqual(payload["variables"], ["relative_humidity_2m", "temperature_2m", "temperature_850hPa"])
        self.assertEqual(
            payload["first_object_url"],
            f"{self.bucket_url}/data_spatial/ncep_gfs025/2026/07/07/1800Z/2026-07-07T1800.om",
        )
        self.assertEqual(
            payload["last_object_url"],
            f"{self.bucket_url}/data_spatial/ncep_gfs025/2026/07/07/1800Z/2026-07-08T0600.om",
        )


if __name__ == "__main__":
    unittest.main()
