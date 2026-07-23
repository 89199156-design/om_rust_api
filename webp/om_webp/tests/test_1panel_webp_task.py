from contextlib import closing
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from check_1panel_webp_task import ALL_PRODUCTION_TASKS, DOWNLOAD_TASKS, TASKS, decision
from install_1panel_jobs import existing_job_values, values


class WebpTaskGateTests(unittest.TestCase):
    def test_run_scope_exports_external_static_roots(self):
        content = (
            Path(__file__).resolve().parents[1] / "scripts" / "run_scope.sh"
        ).read_text(encoding="utf-8")

        self.assertIn(
            'dem_root="${OM_DEM_ROOT:-/opt/1panel/apps/weather_om_api/static}"',
            content,
        )
        self.assertIn(
            'model_static_root="${OM_MODEL_STATIC_ROOT:-/opt/1panel/apps/weather_om_api}"',
            content,
        )
        self.assertIn('export OM_DEM_ROOT="$dem_root"', content)
        self.assertIn('export OM_MODEL_STATIC_ROOT="$model_static_root"', content)
        self.assertIn("default_workers=1", content)
        self.assertIn('workers="${OM_WEBP_WORKERS:-$default_workers}"', content)

    def test_ecmwf_task_is_initially_disabled_and_uses_canonical_scope(self):
        payload = values("now", "OM_ECMWF_WEBP_BUILD", "ecmwf_ifs025", 1)
        self.assertEqual(payload["status"], "Disable")
        self.assertIn("run_scope.sh ecmwf_ifs025", payload["script"])

    def test_existing_job_update_preserves_scheduler_runtime_fields(self):
        payload = existing_job_values(
            {"spec": "5 * * * *", "status": "Disable", "entry_ids": "1", "is_executing": 1}
        )
        self.assertEqual(payload, {"spec": "5 * * * *"})

    def database(
        self,
        *,
        is_executing: int,
        active_records: int,
        executing_peer: str | None = None,
    ) -> tuple[tempfile.TemporaryDirectory, Path]:
        temporary = tempfile.TemporaryDirectory()
        path = Path(temporary.name) / "agent.db"
        with closing(sqlite3.connect(path)) as connection:
            connection.execute(
                "create table cronjobs (id integer primary key, name text, is_executing integer)"
            )
            connection.execute(
                "create table job_records (cronjob_id integer, status text)"
            )
            connection.executemany(
                "insert into cronjobs (id, name, is_executing) values (?, ?, ?)",
                [
                    (
                        index,
                        name,
                        is_executing if name == TASKS[0] else int(name == executing_peer),
                    )
                    for index, name in enumerate(ALL_PRODUCTION_TASKS, start=1)
                ],
            )
            connection.executemany(
                "insert into job_records (cronjob_id, status) values (?, 'Running')",
                [(ALL_PRODUCTION_TASKS.index(TASKS[0]) + 1,)] * active_records,
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

    def test_running_download_skips_webp(self):
        temporary, path = self.database(
            is_executing=1,
            active_records=1,
            executing_peer=DOWNLOAD_TASKS[0],
        )
        with temporary:
            action, reason = decision(path, TASKS[0])
            self.assertEqual(action, "skip")
            self.assertIn(DOWNLOAD_TASKS[0], reason)

    def test_running_peer_webp_skips_webp(self):
        temporary, path = self.database(
            is_executing=1,
            active_records=1,
            executing_peer=TASKS[1],
        )
        with temporary:
            action, reason = decision(path, TASKS[0])
            self.assertEqual(action, "skip")
            self.assertIn(TASKS[1], reason)


if __name__ == "__main__":
    unittest.main()
