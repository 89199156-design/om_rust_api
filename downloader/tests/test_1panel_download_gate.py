from pathlib import Path
from contextlib import closing
import sqlite3
import tempfile
import unittest

from scripts.check_1panel_download_tasks import TASKS, decision


class DownloadTaskGateTests(unittest.TestCase):
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
        temporary, path = self.database({TASKS[0]: 1, TASKS[1]: 0})
        with temporary:
            self.assertEqual(decision(path, TASKS[0])[0], "run")

    def test_running_peer_skips_without_starting_download(self):
        temporary, path = self.database({TASKS[0]: 1, TASKS[1]: 1})
        with temporary:
            action, reason = decision(path, TASKS[0])
            self.assertEqual(action, "skip")
            self.assertIn(TASKS[1], reason)

    def test_missing_task_is_configuration_error(self):
        temporary, path = self.database({TASKS[0]: 1})
        with temporary, self.assertRaisesRegex(RuntimeError, TASKS[1]):
            decision(path, TASKS[0])


if __name__ == "__main__":
    unittest.main()
