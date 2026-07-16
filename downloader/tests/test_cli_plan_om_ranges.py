import json
import struct
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from tests.test_om_format import OM_HEADER, OM_TRAILER_MAGIC, _pack_array, _pack_root


def _sample_om_with_plain_lut():
    lut_offset = 100
    lut_values = [2000, 2020, 2060, 2100, 2140]
    lut_payload = b"".join(struct.pack("<Q", item) for item in lut_values)
    array = _pack_array(
        "temperature_2m",
        dimensions=[4, 1],
        chunks=[1, 1],
        lut_offset=lut_offset,
        lut_size=len(lut_payload),
    )
    array_offset = 200
    root = _pack_root("root", [(array_offset, len(array))])
    root_offset = array_offset + len(array) + 32
    blob = bytearray(root_offset + len(root) + 24)
    blob[0:3] = OM_HEADER
    blob[lut_offset : lut_offset + len(lut_payload)] = lut_payload
    blob[array_offset : array_offset + len(array)] = array
    blob[root_offset : root_offset + len(root)] = root
    blob[-24:] = struct.pack("<2sBBIQQ", OM_TRAILER_MAGIC, 3, 0, 0, root_offset, len(root))
    return bytes(blob)


class _CliPlanRangeHandler(BaseHTTPRequestHandler):
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
            _CliPlanRangeHandler.plain_get_count += 1
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


class CliPlanOmRangeTests(unittest.TestCase):
    def setUp(self):
        _CliPlanRangeHandler.content = _sample_om_with_plain_lut()
        _CliPlanRangeHandler.range_headers = []
        _CliPlanRangeHandler.plain_get_count = 0
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _CliPlanRangeHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/sample.om"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def test_cli_plans_remote_om_data_ranges_for_variable_selection(self):
        result = subprocess.run(
            [
                sys.executable,
                "-m",
                "om_downloader.cli",
                "--plan-om-ranges-url",
                self.url,
                "--variable",
                "temperature_2m",
                "--selection",
                "1:3",
                "--selection",
                "0:1",
                "--lut-codec",
                "plain",
            ],
            cwd=Path.cwd(),
            text=True,
            capture_output=True,
            check=True,
        )

        payload = json.loads(result.stdout)
        self.assertEqual(payload["variable"], "temperature_2m")
        self.assertEqual(payload["selection_ranges"], [[1, 3], [0, 1]])
        self.assertEqual(payload["lut_byte_ranges"], [[100, 140]])
        self.assertEqual(payload["data_byte_ranges"], [[2020, 2100]])
        self.assertEqual(payload["lut_bytes_read"], 40)
        self.assertEqual(_CliPlanRangeHandler.plain_get_count, 0)
        self.assertIn("bytes=100-139", _CliPlanRangeHandler.range_headers)


if __name__ == "__main__":
    unittest.main()
