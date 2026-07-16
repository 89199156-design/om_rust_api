import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from om_downloader.om_remote import (
    HttpByteRangeSource,
    load_remote_om_inventory,
    load_remote_om_inventory_fast,
    read_remote_om_root,
)
from tests.test_om_format import _sample_om_file, _sample_om_file_with_scalar_metadata


class _FakeByteRangeSource:
    def __init__(self, data):
        self.data = data
        self.requests = []

    def content_length(self):
        return len(self.data)

    def read_range(self, start, end):
        if start < 0 or end <= start or end > len(self.data):
            raise AssertionError(f"invalid range: {start}-{end}")
        self.requests.append((start, end))
        return self.data[start:end]


class _OmRangeHandler(BaseHTTPRequestHandler):
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
        if not range_header:
            _OmRangeHandler.plain_get_count += 1
            self.send_response(200)
            self.send_header("Content-Length", str(len(self.content)))
            self.end_headers()
            self.wfile.write(self.content)
            return

        self.range_headers.append(range_header)
        unit, spec = range_header.split("=", 1)
        if unit != "bytes":
            raise AssertionError(f"unexpected range unit: {unit}")
        start_text, end_text = spec.split("-", 1)
        start = int(start_text)
        end = int(end_text)
        payload = self.content[start : end + 1]
        self.send_response(206)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Content-Range", f"bytes {start}-{end}/{len(self.content)}")
        self.end_headers()
        self.wfile.write(payload)


class OmRemoteTests(unittest.TestCase):
    def test_read_remote_om_root_fetches_metadata_ranges_only(self):
        data = _sample_om_file()
        source = _FakeByteRangeSource(data)

        root = read_remote_om_root(source)

        self.assertEqual(root.name, "root")
        self.assertEqual(sorted(root.children), ["relative_humidity_2m", "temperature_2m"])
        self.assertNotIn((0, len(data)), source.requests)
        self.assertIn((0, 40), source.requests)
        self.assertIn((len(data) - 24, len(data)), source.requests)
        self.assertTrue(all(end - start < len(data) for start, end in source.requests))

    def test_load_remote_om_inventory_uses_actual_remote_metadata(self):
        source = _FakeByteRangeSource(_sample_om_file())

        inventory = load_remote_om_inventory(source)

        self.assertEqual(
            inventory.available_variables,
            ("relative_humidity_2m", "temperature_2m"),
        )
        self.assertEqual(inventory.arrays["temperature_2m"].dimensions, (721, 1440, 385))
        self.assertEqual(inventory.pressure_levels_hpa, [])

    def test_http_byte_range_source_reads_remote_inventory_with_range_requests(self):
        _OmRangeHandler.content = _sample_om_file()
        _OmRangeHandler.range_headers = []
        _OmRangeHandler.plain_get_count = 0
        server = ThreadingHTTPServer(("127.0.0.1", 0), _OmRangeHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_address[1]}/sample.om"
            inventory = load_remote_om_inventory(HttpByteRangeSource(url))
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

        self.assertEqual(inventory.available_variables, ("relative_humidity_2m", "temperature_2m"))
        self.assertIn("bytes=0-39", _OmRangeHandler.range_headers)
        self.assertEqual(_OmRangeHandler.plain_get_count, 0)
        self.assertFalse(
            any(header == f"bytes=0-{len(_OmRangeHandler.content) - 1}" for header in _OmRangeHandler.range_headers)
        )

    def test_load_remote_om_inventory_fast_reads_only_wanted_array_metadata(self):
        data = _sample_om_file_with_scalar_metadata()
        source = _FakeByteRangeSource(data)

        inventory = load_remote_om_inventory_fast(source, {"temperature_2m"})

        self.assertEqual(tuple(inventory.arrays), ("temperature_2m",))
        self.assertEqual(inventory.arrays["temperature_2m"].dimensions, (721, 1440, 385))
        self.assertFalse(any(start == 256 for start, _end in source.requests))


if __name__ == "__main__":
    unittest.main()
