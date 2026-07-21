from contextlib import closing
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from check_1panel_webp_task import TASKS, decision


class WebpTaskGateTests(unittest.TestCase):
    def database(self, *, is_executing: int, active_records: int) -> tuple[tempfile.TemporaryDirectory, Path]:
        temporary = tempfile.TemporaryDirectory()
        path = Path(temporary.name) / "agent.db"
        with closing(sqlite3.connect(path)) as connection:
            connection.execute(
                "create table cronjobs (id integer primary key, name text, is_executing integer)"
            )
            connection.execute(
                "create table job_records (cronjob_id integer, status text)"
            )
            connection.execute(
                "insert into cronjobs (id, name, is_executing) values (1, ?, ?)",
                (TASKS[0], is_executing),
            )
            connection.executemany(
                "insert into job_records (cronjob_id, status) values (1, 'Running')",
                [()] * active_records,
            )
            connection.commit()
        return temporary, path

    def test_current_invocation_is_allowed(self):
        temporary, path = self.database(is_executing=1, active_records=1)
        with temporary:
            self.assertEqual(decision(path, TASKS[0])[0], "run")

    def test_older_active_instance_skips(self):
        temporary, path = self.database(is_executing=1, active_records=2)
        with temporary:
            self.assertEqual(decision(path, TASKS[0])[0], "skip")


if __name__ == "__main__":
    unittest.main()
