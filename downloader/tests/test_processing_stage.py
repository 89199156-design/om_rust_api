import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


def _write_ready(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True)
    path.write_text(json.dumps(payload), encoding="utf-8")


class ProcessingStageTests(unittest.TestCase):
    def test_cli_skips_gfs_stage_when_group_ready_is_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            raw_root = root / "om_raw"
            output_root = root / "build_status"

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--build-processing-stage",
                    "gfs013_surface",
                    "--raw-root",
                    str(raw_root),
                    "--output",
                    str(output_root),
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )
            payload = json.loads(result.stdout)
            status = json.loads(
                (output_root / "build_status" / "gfs013_surface" / "latest.json").read_text(
                    encoding="utf-8"
                )
            )

        self.assertEqual(payload["status"], "skipped")
        self.assertEqual(payload["reason"], "group ready missing")
        self.assertEqual(status["stage"], "gfs013_surface")
        self.assertEqual(status["required_group"], "gfs")

    def test_cli_records_ready_stage_as_pending_implementation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            raw_root = root / "om_raw"
            output_root = root / "build_status"
            run = "2026070712"
            _write_ready(
                raw_root / "groups" / "cams" / "current" / "ready_for_processing.json",
                {
                    "group": "cams",
                    "status": "complete",
                    "latest_complete_run": run,
                    "files": 390,
                    "bytes": 10891955,
                },
            )
            _write_ready(
                raw_root / "cams_global" / "current" / "ready_for_processing.json",
                {
                    "model": "cams_global",
                    "status": "complete",
                    "latest_complete_run": run,
                    "files": 390,
                    "bytes": 10891955,
                },
            )

            _write_ready(
                raw_root
                / "cams_global_greenhouse_gases"
                / "current"
                / "ready_for_processing.json",
                {
                    "model": "cams_global_greenhouse_gases",
                    "status": "complete",
                    "latest_complete_run": run,
                    "files": 390,
                    "bytes": 10891955,
                },
            )

            result = subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "om_downloader.cli",
                    "--build-processing-stage",
                    "cams_global",
                    "--raw-root",
                    str(raw_root),
                    "--output",
                    str(output_root),
                ],
                cwd=Path.cwd(),
                text=True,
                capture_output=True,
                check=True,
            )
            payload = json.loads(result.stdout)
            status = json.loads(
                (output_root / "build_status" / "cams_global" / "latest.json").read_text(
                    encoding="utf-8"
                )
            )

        self.assertEqual(payload["status"], "pending_implementation")
        self.assertEqual(status["status"], "pending_implementation")
        self.assertEqual(status["latest_complete_run"], run)
        self.assertEqual(
            status["required_products"],
            ["cams_global", "cams_global_greenhouse_gases"],
        )
        self.assertEqual(status["reason"], "processing stage is not implemented yet")


if __name__ == "__main__":
    unittest.main()
