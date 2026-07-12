import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class CliFixtureTests(unittest.TestCase):
    def test_cli_writes_complete_gfs025_manifest(self):
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
            ]
            result = subprocess.run(cmd, cwd=Path.cwd(), text=True, capture_output=True, check=True)
            self.assertIn("coverage_id", result.stdout)

            manifest_path = out / "published" / "gfs025" / "latest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            self.assertEqual(manifest["status"], "complete")
            self.assertEqual(manifest["coverage_id"], "gfs025_2026070806_399h")
            self.assertEqual(manifest["required_start_utc"], "2026-07-07T16:00:00Z")
            self.assertEqual(manifest["latest_complete_run"], "2026070806")
            self.assertEqual(manifest["timezone_anchors"], [8, 6])
            self.assertEqual(manifest["spatial_ranges"][0]["x_range"], [992, 1289])
            self.assertGreater(manifest["downloaded_bytes"], 0)
            self.assertEqual(manifest["bytes"], manifest["downloaded_bytes"])


if __name__ == "__main__":
    unittest.main()
