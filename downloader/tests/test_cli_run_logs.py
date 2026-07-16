import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

from om_downloader import cli


class CliRunLogTests(unittest.TestCase):
    def test_limited_jsonl_logs_are_split_and_old_files_are_removed(self):
        class FixedDatetime(datetime):
            @classmethod
            def now(cls, tz=None):
                value = cls(2026, 7, 10, 12, 0, 0, tzinfo=timezone.utc)
                return value.astimezone(tz) if tz else value.replace(tzinfo=None)

        with tempfile.TemporaryDirectory() as tmp:
            output_root = Path(tmp)
            log_dir = output_root / "logs"
            log_dir.mkdir(parents=True)
            old_log = log_dir / "test_log-2026-05-01.jsonl"
            old_log.write_text("{}\n", encoding="utf-8")
            retained_log = log_dir / "test_log-2026-06-01.jsonl"
            retained_log.write_text("{}\n", encoding="utf-8")

            with (
                patch.object(cli, "datetime", FixedDatetime),
                patch.object(cli, "APP_LOG_RETENTION_DAYS", 45),
                patch.object(cli, "APP_LOG_MAX_BYTES", 220),
            ):
                for index in range(8):
                    cli._append_limited_jsonl_log(
                        output_root,
                        "test_log",
                        {
                            "kind": "test",
                            "index": index,
                            "message": "x" * 40,
                        },
                    )

            written_logs = sorted(log_dir.glob("test_log-2026-07-10*.jsonl"))

            self.assertFalse(old_log.exists())
            self.assertTrue(retained_log.exists())
            self.assertGreater(len(written_logs), 1)
            for path in written_logs:
                self.assertLessEqual(path.stat().st_size, 220)
                for line in path.read_text(encoding="utf-8").splitlines():
                    record = json.loads(line)
                    self.assertEqual(record["logged_at_utc"], "2026-07-10T12:00:00Z")


if __name__ == "__main__":
    unittest.main()
