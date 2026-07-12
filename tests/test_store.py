import tempfile
import time
import threading
from pathlib import Path
import unittest

from om_downloader import store
from om_downloader.http_range import ByteRange


def _coverage_entry(variable, source_url, byte_range):
    return {
        "object_record": {
            "valid_time_utc": "2026-07-08T00:00:00Z",
            "source_run": "2026070800",
            "forecast_hour": 0,
        },
        "source_url": source_url,
        "bundle": {
            "variable": variable,
            "path": variable,
            "selection_ranges": [[0, 1], [0, 1]],
            "array": {
                "data_type": 20,
                "compression": 0,
                "dimensions": [1, 1],
                "chunks": [1, 1],
                "lut_offset": 0,
                "lut_size": 16,
                "scale_factor": 1.0,
                "add_offset": 0.0,
            },
            "lut_byte_ranges": [],
            "data_byte_ranges": [[byte_range.start, byte_range.end + 1]],
            "lut_bytes_read": 0,
            "byte_ranges": [byte_range],
        },
    }


class StoreTests(unittest.TestCase):
    def test_coverage_bundle_downloads_entries_concurrently_and_writes_one_file(self):
        events = []
        original_fetch = store.fetch_byte_range_with_retry

        def fake_fetch_byte_range_with_retry(source_url, byte_range, **_kwargs):
            events.append(f"fetch:{source_url}:{byte_range.start}")
            return source_url[-1:].encode("ascii") * byte_range.length

        entries = [
            _coverage_entry("temperature_2m", "source-a", ByteRange(0, 2)),
            _coverage_entry("pressure_msl", "source-b", ByteRange(10, 13)),
            _coverage_entry("cloud_cover", "source-c", ByteRange(20, 24)),
        ]
        for entry in entries:
            entry["remote_content_length"] = 100

        store.fetch_byte_range_with_retry = fake_fetch_byte_range_with_retry
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                record = store.write_om_coverage_bundle_file(
                    root,
                    "gfs025",
                    "gfs025_2026070800_3h",
                    iter(entries),
                    download_workers=3,
                )
                output = root / "published" / "gfs025" / record["path"]
                payload = output.read_bytes()
                file_count = len(
                    list(
                        (
                            root
                            / "published"
                            / "gfs025"
                            / "coverages"
                            / "gfs025_2026070800_3h"
                        ).glob("*.omranges")
                    )
                )
        finally:
            store.fetch_byte_range_with_retry = original_fetch

        self.assertEqual(record["path"], "coverages/gfs025_2026070800_3h/gfs025.omranges")
        self.assertEqual(file_count, 1)
        self.assertEqual(payload, b"aaabbbbccccc")
        self.assertEqual([entry["bundle_offset"] for entry in record["entries"]], [0, 3, 7])
        self.assertEqual([entry["bundle_bytes"] for entry in record["entries"]], [3, 4, 5])

    def test_coverage_bundle_streams_entries_without_waiting_for_full_plan(self):
        events = []
        original_fetch = store.fetch_byte_range_with_retry

        def fake_fetch_byte_range_with_retry(source_url, byte_range, **_kwargs):
            events.append(f"copy:{source_url}")
            return source_url[-1:].encode("ascii") * byte_range.length

        def iter_entries():
            events.append("yield:first")
            entry = _coverage_entry("temperature_2m", "source-a", ByteRange(0, 2))
            entry["remote_content_length"] = 100
            yield entry
            events.append("yield:second")
            entry = _coverage_entry("pressure_msl", "source-b", ByteRange(10, 13))
            entry["remote_content_length"] = 100
            yield entry

        store.fetch_byte_range_with_retry = fake_fetch_byte_range_with_retry
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                record = store.write_om_coverage_bundle_file(
                    root,
                    "gfs025",
                    "gfs025_2026070800_2h",
                    iter_entries(),
                )
                output = root / "published" / "gfs025" / record["path"]
                payload = output.read_bytes()
        finally:
            store.fetch_byte_range_with_retry = original_fetch

        self.assertLess(events.index("copy:source-a"), events.index("yield:second"))
        self.assertEqual(payload, b"aaabbbb")
        self.assertEqual(record["bytes"], 7)
        self.assertEqual([entry["bundle_offset"] for entry in record["entries"]], [0, 3])
        self.assertEqual([entry["bundle_bytes"] for entry in record["entries"]], [3, 4])

    def test_coverage_bundle_downloads_ranges_inside_one_entry_concurrently(self):
        active = 0
        max_active = 0
        lock = threading.Lock()
        original_fetch = store.fetch_byte_range_with_retry

        def fake_fetch_byte_range_with_retry(source_url, byte_range, **_kwargs):
            nonlocal active, max_active
            with lock:
                active += 1
                max_active = max(max_active, active)
            try:
                time.sleep(0.05)
                return source_url[-1:].encode("ascii") * byte_range.length
            finally:
                with lock:
                    active -= 1

        entry = _coverage_entry("temperature_2m", "source-a", ByteRange(0, 2))
        entry["remote_content_length"] = 100
        entry["bundle"]["data_byte_ranges"] = [[0, 3], [10, 13], [20, 23]]
        entry["bundle"]["byte_ranges"] = [
            ByteRange(0, 2),
            ByteRange(10, 12),
            ByteRange(20, 22),
        ]

        store.fetch_byte_range_with_retry = fake_fetch_byte_range_with_retry
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                record = store.write_om_coverage_bundle_file(
                    root,
                    "gfs025",
                    "gfs025_2026070800_1h",
                    iter([entry]),
                    download_workers=1,
                    range_workers=3,
                )
        finally:
            store.fetch_byte_range_with_retry = original_fetch

        self.assertGreaterEqual(max_active, 2)
        self.assertEqual(record["bytes"], 9)
        self.assertEqual(record["entries"][0]["bundle_offset"], 0)
        self.assertEqual(record["entries"][0]["bundle_bytes"], 9)

    def test_coverage_bundle_can_stream_object_prefix_and_crop_locally(self):
        events = []
        original_fetch = store.fetch_byte_range_with_retry
        original_prefix = store.fetch_http_prefix_to_file

        def fake_fetch_byte_range_with_retry(*_args, **_kwargs):
            raise AssertionError("range fetch should not be used when object prefix is cheaper")

        def fake_fetch_http_prefix_to_file(source_url, output_path, byte_count, **_kwargs):
            events.append((source_url, byte_count))
            output_path.write_bytes(bytes(range(byte_count)))
            return byte_count

        first = _coverage_entry("temperature_2m", "same-source", ByteRange(10, 12))
        second = _coverage_entry("pressure_msl", "same-source", ByteRange(20, 21))
        for entry in (first, second):
            entry["remote_content_length"] = 100

        store.fetch_byte_range_with_retry = fake_fetch_byte_range_with_retry
        store.fetch_http_prefix_to_file = fake_fetch_http_prefix_to_file
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                record = store.write_om_coverage_bundle_file(
                    root,
                    "gfs025",
                    "gfs025_2026070800_2h",
                    iter([first, second]),
                    object_fetch_mode="prefix",
                    object_fetch_max_multiplier=5.0,
                    object_fetch_min_ranges=2,
                )
                output = root / "published" / "gfs025" / record["path"]
                payload = output.read_bytes()
        finally:
            store.fetch_byte_range_with_retry = original_fetch
            store.fetch_http_prefix_to_file = original_prefix

        self.assertEqual(events, [("same-source", 22)])
        self.assertEqual(payload, bytes([10, 11, 12, 20, 21]))
        self.assertEqual(record["bytes"], 5)
        self.assertEqual(record["downloaded_bytes"], 22)
        self.assertEqual([entry["bundle_offset"] for entry in record["entries"]], [0, 3])
        self.assertEqual([entry["bundle_bytes"] for entry in record["entries"]], [3, 2])

    def test_coverage_bundle_merges_source_object_ranges_and_crops_locally(self):
        fetched_ranges = []
        original_fetch = store.fetch_byte_range_with_retry
        original_prefix = store.fetch_http_prefix_to_file

        def fake_fetch_byte_range_with_retry(source_url, byte_range, **_kwargs):
            fetched_ranges.append((source_url, byte_range.as_manifest()))
            return bytes(range(byte_range.start, byte_range.end + 1))

        def fake_fetch_http_prefix_to_file(*_args, **_kwargs):
            raise AssertionError("prefix fetch should not be used by auto merge mode")

        first = _coverage_entry("temperature_2m", "same-source", ByteRange(10, 12))
        second = _coverage_entry("pressure_msl", "same-source", ByteRange(15, 16))
        for entry in (first, second):
            entry["remote_content_length"] = 100

        store.fetch_byte_range_with_retry = fake_fetch_byte_range_with_retry
        store.fetch_http_prefix_to_file = fake_fetch_http_prefix_to_file
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                record = store.write_om_coverage_bundle_file(
                    root,
                    "gfs025",
                    "gfs025_2026070800_2h",
                    iter([first, second]),
                    object_fetch_mode="auto",
                    object_range_merge_gap=4,
                    object_range_max_multiplier=2.0,
                    object_range_min_ranges=2,
                )
                output = root / "published" / "gfs025" / record["path"]
                payload = output.read_bytes()
        finally:
            store.fetch_byte_range_with_retry = original_fetch
            store.fetch_http_prefix_to_file = original_prefix

        self.assertEqual(fetched_ranges, [("same-source", [10, 16])])
        self.assertEqual(payload, bytes([10, 11, 12, 15, 16]))
        self.assertEqual(record["bytes"], 5)
        self.assertEqual(record["downloaded_bytes"], 7)
        self.assertEqual([entry["bundle_offset"] for entry in record["entries"]], [0, 3])
        self.assertEqual([entry["bundle_bytes"] for entry in record["entries"]], [3, 2])

    def test_coverage_bundle_does_not_merge_ranges_when_extra_bytes_are_too_high(self):
        fetched_ranges = []
        original_fetch = store.fetch_byte_range_with_retry

        def fake_fetch_byte_range_with_retry(source_url, byte_range, **_kwargs):
            fetched_ranges.append((source_url, byte_range.as_manifest()))
            return source_url[-1:].encode("ascii") * byte_range.length

        first = _coverage_entry("temperature_2m", "source-a", ByteRange(10, 12))
        second = _coverage_entry("pressure_msl", "source-a", ByteRange(90, 92))
        for entry in (first, second):
            entry["remote_content_length"] = 100

        store.fetch_byte_range_with_retry = fake_fetch_byte_range_with_retry
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                record = store.write_om_coverage_bundle_file(
                    root,
                    "gfs025",
                    "gfs025_2026070800_2h",
                    iter([first, second]),
                    object_fetch_mode="auto",
                    object_range_merge_gap=100,
                    object_range_max_multiplier=2.0,
                    object_range_min_ranges=2,
                )
                output = root / "published" / "gfs025" / record["path"]
                payload = output.read_bytes()
        finally:
            store.fetch_byte_range_with_retry = original_fetch

        self.assertEqual(fetched_ranges, [("source-a", [10, 12]), ("source-a", [90, 92])])
        self.assertEqual(payload, b"aaaaaa")
        self.assertEqual(record["bytes"], 6)
        self.assertEqual(record["downloaded_bytes"], 6)

    def test_coverage_bundle_does_not_merge_ranges_beyond_max_bytes(self):
        fetched_ranges = []
        original_fetch = store.fetch_byte_range_with_retry

        def fake_fetch_byte_range_with_retry(source_url, byte_range, **_kwargs):
            fetched_ranges.append((source_url, byte_range.as_manifest()))
            return source_url[-1:].encode("ascii") * byte_range.length

        first = _coverage_entry("temperature_2m", "source-a", ByteRange(10, 12))
        second = _coverage_entry("pressure_msl", "source-a", ByteRange(15, 16))
        for entry in (first, second):
            entry["remote_content_length"] = 100

        store.fetch_byte_range_with_retry = fake_fetch_byte_range_with_retry
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                record = store.write_om_coverage_bundle_file(
                    root,
                    "gfs025",
                    "gfs025_2026070800_2h",
                    iter([first, second]),
                    object_fetch_mode="auto",
                    object_range_merge_gap=4,
                    object_range_max_multiplier=2.0,
                    object_range_min_ranges=2,
                    object_range_max_bytes=6,
                )
                output = root / "published" / "gfs025" / record["path"]
                payload = output.read_bytes()
        finally:
            store.fetch_byte_range_with_retry = original_fetch

        self.assertEqual(fetched_ranges, [("source-a", [10, 12]), ("source-a", [15, 16])])
        self.assertEqual(payload, b"aaaaa")
        self.assertEqual(record["bytes"], 5)
        self.assertEqual(record["downloaded_bytes"], 5)

    def test_coverage_bundle_auto_writes_later_completed_entry_before_slow_first(self):
        original_fetch = store.fetch_byte_range_with_retry
        original_progress_log = store._progress_log
        second_range_returned = threading.Event()
        later_entry_written_before_first = threading.Event()

        first = _coverage_entry("temperature_2m", "slow-source", ByteRange(0, 2))
        second = _coverage_entry("pressure_msl", "fast-source", ByteRange(10, 12))
        for entry in (first, second):
            entry["remote_content_length"] = 100

        def fake_fetch_byte_range_with_retry(source_url, byte_range, **_kwargs):
            if source_url == "fast-source":
                second_range_returned.set()
                return b"bbb"

            self.assertTrue(second_range_returned.wait(timeout=1.0))
            self.assertTrue(
                later_entry_written_before_first.wait(timeout=1.0),
                "later completed entry was buffered behind slow first entry",
            )
            return b"aaa"

        def fake_progress_log(context, payload, *, force=False):
            if payload.get("stage") == "writing" and int(payload.get("written_bytes", 0)) >= 3:
                later_entry_written_before_first.set()
            return original_progress_log(context, payload, force=force)

        store.fetch_byte_range_with_retry = fake_fetch_byte_range_with_retry
        store._progress_log = fake_progress_log
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                record = store.write_om_coverage_bundle_file(
                    root,
                    "gfs025",
                    "gfs025_2026070800_2h",
                    iter([first, second]),
                    download_workers=2,
                    range_workers=2,
                    object_fetch_mode="auto",
                    object_range_merge_gap=0,
                    object_range_max_multiplier=1.0,
                    object_range_min_ranges=2,
                    progress_context={"progress_interval_seconds": 0.0},
                )
                output = root / "published" / "gfs025" / record["path"]
                payload = output.read_bytes()
        finally:
            store.fetch_byte_range_with_retry = original_fetch
            store._progress_log = original_progress_log

        self.assertTrue(later_entry_written_before_first.is_set())
        self.assertEqual(payload, b"aaabbb")
        self.assertEqual([entry["bundle_offset"] for entry in record["entries"]], [0, 3])
        self.assertEqual([entry["bundle_bytes"] for entry in record["entries"]], [3, 3])

    def test_coverage_bundle_keeps_range_fetch_when_object_prefix_is_too_expensive(self):
        fetched_ranges = []
        original_fetch = store.fetch_byte_range_with_retry
        original_prefix = store.fetch_http_prefix_to_file

        def fake_fetch_byte_range_with_retry(source_url, byte_range, **_kwargs):
            fetched_ranges.append((source_url, byte_range.as_manifest()))
            return source_url[-1:].encode("ascii") * byte_range.length

        def fake_fetch_http_prefix_to_file(*_args, **_kwargs):
            raise AssertionError("object prefix should not be used when multiplier is too high")

        entry = _coverage_entry("temperature_2m", "source-a", ByteRange(90, 92))
        entry["remote_content_length"] = 100

        store.fetch_byte_range_with_retry = fake_fetch_byte_range_with_retry
        store.fetch_http_prefix_to_file = fake_fetch_http_prefix_to_file
        try:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                record = store.write_om_coverage_bundle_file(
                    root,
                    "gfs025",
                    "gfs025_2026070800_1h",
                    iter([entry]),
                    object_fetch_mode="auto",
                    object_fetch_max_multiplier=5.0,
                    object_fetch_min_ranges=1,
                )
                output = root / "published" / "gfs025" / record["path"]
                payload = output.read_bytes()
        finally:
            store.fetch_byte_range_with_retry = original_fetch
            store.fetch_http_prefix_to_file = original_prefix

        self.assertEqual(fetched_ranges, [("source-a", [90, 92])])
        self.assertEqual(payload, b"aaa")
        self.assertEqual(record["bytes"], 3)
        self.assertEqual(record["downloaded_bytes"], 3)


if __name__ == "__main__":
    unittest.main()
