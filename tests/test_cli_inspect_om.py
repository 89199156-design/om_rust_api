import json
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from tests.test_om_format import _sample_om_file


class _CliOmRangeHandler(BaseHTTPRequestHandler):
    content = b""
    range_headers = []
    plain_get_count = 0

    def log_message(self, _format, *_args):
        return

    def do_HEAD(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(self.content)))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

    def do_GET(self):
        range_header = self.headers.get("Range")
        if range_header is None:
            _CliOmRangeHandler.plain_get_count += 1
            self.send_response(200)
            self.send_header("Content-Length", str(len(self.content)))
            self.end_headers()
            self.wfile.write(self.content)
            return

        self.range_headers.append(range_header)
        start_text, end_text = range_header.replace("bytes=", "").split("-", 1)
        start = int(start_text)
        end = int(end_text)
        payload = self.content[start : end + 1]
        self.send_response(206)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Content-Range", f"bytes {start}-{end}/{len(self.content)}")
        self.end_headers()
        self.wfile.write(payload)


class CliInspectOmTests(unittest.TestCase):
    def setUp(self):
        _CliOmRangeHandler.content = _sample_om_file()
        _CliOmRangeHandler.range_headers = []
        _CliOmRangeHandler.plain_get_count = 0
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _CliOmRangeHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/sample.om"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def test_cli_inspects_remote_om_inventory_with_range_requests(self):
        result = subprocess.run(
            [sys.executable, "-m", "om_downloader.cli", "--inspect-om-url", self.url],
            cwd=Path.cwd(),
            text=True,
            capture_output=True,
            check=True,
        )

        payload = json.loads(result.stdout)
        self.assertEqual(payload["available_variables"], ["relative_humidity_2m", "temperature_2m"])
        self.assertEqual(payload["variables"]["temperature_2m"]["dimensions"], [721, 1440, 385])
        self.assertEqual(payload["variables"]["temperature_2m"]["chunks"], [1, 50, 385])
        self.assertIn("bytes=0-39", _CliOmRangeHandler.range_headers)
        self.assertEqual(_CliOmRangeHandler.plain_get_count, 0)


if __name__ == "__main__":
    unittest.main()
