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
        self.assertEqual(content.count("default_workers=2"), 3)
        self.assertNotIn("default_workers=1", content)
        self.assertIn('workers="${OM_WEBP_WORKERS:-$default_workers}"', content)
        self.assertEqual(content.count('default_memory_max="1536M"'), 2)
        self.assertIn('default_memory_max="1792M"', content)
        self.assertIn(
            'memory_max="${OM_WEBP_MEMORY_MAX:-$default_memory_max}"',
            content,
        )
        self.assertIn('cpu_quota="${OM_WEBP_CPU_QUOTA:-150%}"', content)
        self.assertIn('--property="MemoryMax=$memory_max"', content)
        self.assertIn('--property="MemorySwapMax=0"', content)
        self.assertIn('--property="CPUQuota=$cpu_quota"', content)
        self.assertIn('--property="LimitNOFILE=$minimum_open_files"', content)
        self.assertIn('WebP production memory guard requires root and systemd-run', content)
        self.assertIn(
            'minimum_open_files="${OM_WEBP_MIN_OPEN_FILES:-65536}"',
            content,
        )
        self.assertIn('ulimit -Sn "$minimum_open_files"', content)
        self.assertIn(
            'OM_DATA_ROOT must be an absolute read-only source path',
            content,
        )
        self.assertIn(
            'reporter="${OM_TASK_PROGRESS_REPORTER:-$app_dir/scripts/task_progress_reporter.py}"',
            content,
        )
        self.assertIn('payload.get("latest_complete_run")', content)
        self.assertIn("WEATHER_TASK_TARGET_RUN", content)
        self.assertNotIn("weather_om_downloader", content)

    def test_installer_accepts_verified_bind_mount_on_output_device(self):
        content = (
            Path(__file__).resolve().parents[1] / "scripts" / "install_om_webp.sh"
        ).read_text(encoding="utf-8")

        self.assertIn('data_ancestor="$DATA_DIR"', content)
        self.assertIn(
            '"$(stat -c %d "$data_ancestor")" != "$(stat -c %d "$strict_real")"',
            content,
        )
        self.assertIn(
            "WebP data path is not on the strict data filesystem",
            content,
        )
        self.assertIn('OM_BUILD_REVISION="$SOURCE_REVISION" cargo build --release', content)

    def test_standalone_tasks_are_disabled_and_do_not_render(self):
        for name, scope in (
            ("OM_GFS_WEBP_BUILD", "gfs"),
            ("OM_CAMS_WEBP_BUILD", "cams"),
            ("OM_ECMWF_WEBP_BUILD", "ecmwf_ifs025"),
        ):
            payload = values("now", name, scope, 1)
            self.assertEqual(payload["status"], "Disable")
            self.assertIn("WebP由对应下载任务连续生成", payload["script"])
            self.assertNotIn("run_scope.sh", payload["script"])

    def test_existing_job_update_forces_disabled_status(self):
        payload = existing_job_values(
            {"spec": "5 * * * *", "status": "Disable", "entry_ids": "1", "is_executing": 1}
        )
        self.assertEqual(payload, {"spec": "5 * * * *", "status": "Disable"})

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
