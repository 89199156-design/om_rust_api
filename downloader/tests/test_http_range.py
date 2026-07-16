import tempfile
import time
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from om_downloader.http_range import (
    ByteRange,
    RangeFetchRequest,
    download_byte_ranges,
    download_ranges_concurrently,
    fetch_http_prefix_to_file,
    probe_http_object,
)


class _RangeHandler(BaseHTTPRequestHandler):
    content = b""
    range_headers: list[str] = []
    fail_once_ranges: set[str] = set()
    failed_ranges: set[str] = set()
    fail_always_ranges: set[str] = set()
    head_failures_remaining = 0
    head_count = 0
    active_requests = 0
    max_active_requests = 0
    request_lock = threading.Lock()
    response_delay = 0.0

    def log_message(self, _format, *_args):
        return

    def do_HEAD(self):
        type(self).head_count += 1
        if type(self).head_failures_remaining:
            type(self).head_failures_remaining -= 1
            self.send_response(503)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(self.content)))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

    def do_GET(self):
        range_header = self.headers.get("Range")
        if not range_header:
            self.send_response(200)
            self.send_header("Content-Length", str(len(self.content)))
            self.end_headers()
            self.wfile.write(self.content)
            return

        type(self).range_headers.append(range_header)
        with type(self).request_lock:
            type(self).active_requests += 1
            type(self).max_active_requests = max(
                type(self).max_active_requests,
                type(self).active_requests,
            )
        try:
            if self.response_delay:
                time.sleep(self.response_delay)
        finally:
            with type(self).request_lock:
                type(self).active_requests -= 1
        if range_header in self.fail_always_ranges:
            self.close_connection = True
            return
        if range_header in self.fail_once_ranges and range_header not in self.failed_ranges:
            self.failed_ranges.add(range_header)
            self.close_connection = True
            return

        unit, spec = range_header.split("=", 1)
        self.assertEqual(unit, "bytes")
        start_text, end_text = spec.split("-", 1)
        start = int(start_text)
        end = int(end_text)
        payload = self.content[start : end + 1]
        self.send_response(206)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Content-Range", f"bytes {start}-{end}/{len(self.content)}")
        self.end_headers()
        self.wfile.write(payload)

    def assertEqual(self, left, right):
        if left != right:
            raise AssertionError(f"{left!r} != {right!r}")


class HttpRangeTests(unittest.TestCase):
    def setUp(self):
        _RangeHandler.content = bytes(range(64))
        _RangeHandler.range_headers = []
        _RangeHandler.fail_once_ranges = set()
        _RangeHandler.failed_ranges = set()
        _RangeHandler.fail_always_ranges = set()
        _RangeHandler.head_failures_remaining = 0
        _RangeHandler.head_count = 0
        _RangeHandler.active_requests = 0
        _RangeHandler.max_active_requests = 0
        _RangeHandler.response_delay = 0.0
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _RangeHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/sample.om"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def test_probe_http_object_reads_head_metadata(self):
        info = probe_http_object(self.url)
        self.assertEqual(info.content_length, 64)
        self.assertTrue(info.accept_ranges)

    def test_probe_http_object_retries_transient_head_503(self):
        _RangeHandler.head_failures_remaining = 1

        info = probe_http_object(self.url)

        self.assertEqual(info.content_length, 64)
        self.assertEqual(_RangeHandler.head_count, 2)

    def test_download_byte_ranges_uses_range_requests(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "partial.om"
            record = download_byte_ranges(
                self.url,
                [ByteRange(2, 5), ByteRange(10, 13)],
                output,
                relative_to=Path(tmp),
            )

            self.assertEqual(output.read_bytes(), bytes([2, 3, 4, 5, 10, 11, 12, 13]))
            self.assertEqual(_RangeHandler.range_headers, ["bytes=2-5", "bytes=10-13"])
            self.assertEqual(record["path"], "partial.om")
            self.assertEqual(record["bytes"], 8)
            self.assertEqual(record["downloaded_bytes"], 8)
            self.assertEqual(record["remote_content_length"], 64)
            self.assertEqual(record["source_url"], self.url)
            self.assertEqual(record["byte_ranges"], [[2, 5], [10, 13]])
            self.assertEqual(len(record["sha256"]), 64)

    def test_fetch_http_prefix_to_file_uses_plain_get_and_writes_prefix(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "prefix.om"

            written = fetch_http_prefix_to_file(
                self.url,
                output,
                12,
                expected_content_length=64,
            )

            self.assertEqual(written, 12)
            self.assertEqual(output.read_bytes(), bytes(range(12)))
            self.assertEqual(_RangeHandler.range_headers, [])

    def test_download_byte_ranges_retries_transient_connection_reset(self):
        _RangeHandler.fail_once_ranges = {"bytes=2-5"}
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "partial.om"
            record = download_byte_ranges(
                self.url,
                [ByteRange(2, 5)],
                output,
                relative_to=Path(tmp),
            )

            self.assertEqual(output.read_bytes(), bytes([2, 3, 4, 5]))
            self.assertEqual(_RangeHandler.range_headers, ["bytes=2-5", "bytes=2-5"])
            self.assertEqual(record["downloaded_bytes"], 4)

    def test_download_byte_ranges_keeps_existing_file_when_later_range_fails(self):
        _RangeHandler.fail_always_ranges = {"bytes=10-13"}
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "partial.om"
            output.write_bytes(b"old-complete")

            with self.assertRaises(ValueError):
                download_byte_ranges(
                    self.url,
                    [ByteRange(2, 5), ByteRange(10, 13)],
                    output,
                    relative_to=Path(tmp),
                )

            self.assertEqual(output.read_bytes(), b"old-complete")
            self.assertFalse((Path(tmp) / "partial.om.tmp").exists())

    def test_download_byte_ranges_refuses_full_object_by_default(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "global.om"
            with self.assertRaises(ValueError) as ctx:
                download_byte_ranges(self.url, [ByteRange(0, 63)], output)
            self.assertIn("refusing full-object download", str(ctx.exception))

    def test_download_ranges_concurrently_fetches_multiple_ranges_in_parallel(self):
        _RangeHandler.response_delay = 0.15

        results = download_ranges_concurrently(
            [
                RangeFetchRequest(self.url, ByteRange(0, 7), remote_content_length=64),
                RangeFetchRequest(self.url, ByteRange(8, 15), remote_content_length=64),
                RangeFetchRequest(self.url, ByteRange(16, 23), remote_content_length=64),
            ],
            max_workers=3,
        )

        self.assertEqual([item.payload for item in results], [bytes(range(0, 8)), bytes(range(8, 16)), bytes(range(16, 24))])
        self.assertGreaterEqual(_RangeHandler.max_active_requests, 2)


if __name__ == "__main__":
    unittest.main()
