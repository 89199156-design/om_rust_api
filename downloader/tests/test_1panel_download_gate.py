from pathlib import Path
from contextlib import closing
import sqlite3
import tempfile
import unittest

from scripts.check_1panel_download_tasks import ALL_PRODUCTION_TASKS, TASKS, decision
from scripts.install_1panel_v2_cronjobs import (
    _cronjob_values,
    _existing_cronjob_values,
    api_publisher_tasks,
)


class DownloadTaskGateTests(unittest.TestCase):
    def test_ecmwf_production_task_tracks_latest_run_and_is_enabled(self):
        tasks = {name: script for name, _spec, script in api_publisher_tasks(raw_root=Path("/data/om_raw"))}
        script = tasks["OM_ECMWF_DOWNLOAD"]
        self.assertIn("--download-openmeteo-group ecmwf", script)
        self.assertIn("--output /data/om_downloader", script)
        self.assertIn("--publish-openmeteo-group-to /data/om_raw", script)
        self.assertIn("--retain-complete-releases 5", script)
        self.assertNotIn("--reference-time", script)
        self.assertNotIn("OM_ECMWF_REFERENCE_TIME", script)
        payload = _cronjob_values("now", "OM_ECMWF_DOWNLOAD", "spec", script, 1)
        self.assertEqual(payload["status"], "Enable")

    def test_reinstall_preserves_existing_enabled_or_disabled_status(self):
        payload = _existing_cronjob_values(
            {"status": "Disable", "spec": "20 14 * * *", "entry_ids": "1", "is_executing": 1}
        )
        self.assertEqual(payload, {"spec": "20 14 * * *"})

    def database(self, states: dict[str, int]) -> tuple[tempfile.TemporaryDirectory, Path]:
        temporary = tempfile.TemporaryDirectory()
        path = Path(temporary.name) / "agent.db"
        with closing(sqlite3.connect(path)) as connection:
            connection.execute("create table cronjobs (name text primary key, is_executing integer)")
            connection.executemany(
                "insert into cronjobs (name, is_executing) values (?, ?)", states.items()
            )
            connection.commit()
        return temporary, path

    def test_current_1panel_row_does_not_block_its_own_invocation(self):
        states = {name: 0 for name in ALL_PRODUCTION_TASKS}
        states[TASKS[0]] = 1
        temporary, path = self.database(states)
        with temporary:
            self.assertEqual(decision(path, TASKS[0])[0], "run")

    def test_running_peer_skips_without_starting_download(self):
        states = {name: 0 for name in ALL_PRODUCTION_TASKS}
        states[TASKS[0]] = states[TASKS[1]] = 1
        temporary, path = self.database(states)
        with temporary:
            action, reason = decision(path, TASKS[0])
            self.assertEqual(action, "skip")
            self.assertIn(TASKS[1], reason)

    def test_running_webp_task_skips_without_starting_download(self):
        states = {name: 0 for name in ALL_PRODUCTION_TASKS}
        states[TASKS[0]] = states[ALL_PRODUCTION_TASKS[-1]] = 1
        temporary, path = self.database(states)
        with temporary:
            action, reason = decision(path, TASKS[0])
            self.assertEqual(action, "skip")
            self.assertIn(ALL_PRODUCTION_TASKS[-1], reason)

    def test_missing_task_is_configuration_error(self):
        states = {name: 0 for name in ALL_PRODUCTION_TASKS}
        states.pop(TASKS[1])
        temporary, path = self.database(states)
        with temporary, self.assertRaisesRegex(RuntimeError, TASKS[1]):
            decision(path, TASKS[0])


if __name__ == "__main__":
    unittest.main()
