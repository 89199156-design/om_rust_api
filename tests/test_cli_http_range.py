import json
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


class _CliRangeHandler(BaseHTTPRequestHandler):
    content = bytes(range(64))
    range_headers: list[str] = []

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
            self.send_response(500)
            self.end_headers()
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


class CliHttpRangeTests(unittest.TestCase):
    def setUp(self):
        _CliRangeHandler.range_headers = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _CliRangeHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/sample.om"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def test_cli_downloads_explicit_http_byte_ranges_into_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            cmd = [
                sys.executable,
                "-m",
                "om_downloader.cli",
                "--config",
                "config/models.json",
                "--model",
                "gfs025",
                "--metadata",
                "fixtures/metadata/gfs025.json",
                "--output",
                str(out),
                "--now",
                "2026-07-08T14:00:00Z",
                "--source-url",
                self.url,
                "--byte-range",
                "2-5",
                "--byte-range",
                "10-13",
            ]
            subprocess.run(cmd, cwd=Path.cwd(), text=True, capture_output=True, check=True)

            manifest = json.loads((out / "published" / "gfs025" / "latest.json").read_text(encoding="utf-8"))
            self.assertEqual(manifest["status"], "complete")
            self.assertEqual(_CliRangeHandler.range_headers, ["bytes=2-5", "bytes=10-13"])
            self.assertEqual(manifest["bytes"], 8)
            self.assertEqual(manifest["downloaded_bytes"], 8)
            self.assertEqual(manifest["remote_content_length"], 64)
            self.assertEqual(manifest["files"][0]["source_url"], self.url)
            self.assertEqual(manifest["files"][0]["byte_ranges"], [[2, 5], [10, 13]])


if __name__ == "__main__":
    unittest.main()
