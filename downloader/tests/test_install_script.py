from contextlib import closing, redirect_stdout
import io
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import scripts.install_1panel_v2_cronjobs as cronjob_installer
from scripts.install_1panel_v2_cronjobs import (
    REMOVED_PLACEHOLDER_TASKS,
    _existing_cronjob_values,
    api_publisher_tasks,
    api_source_sync_tasks,
    downloader_tasks,
)


class InstallScriptTests(unittest.TestCase):
    def test_legacy_database_without_cronjobs_table_is_ignored(self):
        with tempfile.TemporaryDirectory() as directory:
            database = Path(directory) / "1Panel.db"
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("create table settings (name text primary key)")
                connection.commit()

            output = io.StringIO()
            with (
                patch.object(cronjob_installer, "LEGACY_DB", database),
                redirect_stdout(output),
            ):
                cronjob_installer.clean_legacy_jobs(("OM_GFS_DOWNLOAD",))

            self.assertEqual(output.getvalue().strip(), "LEGACY_DB_CLEANED=0")

    def test_existing_job_update_preserves_scheduler_runtime_fields(self):
        values = _existing_cronjob_values(
            {"spec": "0 * * * *", "entry_ids": "1", "is_executing": 1}
        )
        self.assertEqual(values, {"spec": "0 * * * *"})

    def test_install_script_contains_server_deploy_steps_without_system_scheduler(self):
        script = Path("scripts/install_from_zip.sh")
        content = script.read_text(encoding="utf-8")

        self.assertIn("set -euo pipefail", content)
        self.assertIn("/opt/1panel/apps/weather_om_downloader", content)
        self.assertIn("unzip", content)
        self.assertIn("scripts/build_turbopfor_decoder.sh", content)
        self.assertIn('ln -s -- "$DOWNLOAD_ROOT" "$INSTALL_DIR/data"', content)
        self.assertIn(
            'if [ -f "$BACKUP_DIR/native/libom_turbopfor.so" ]; then',
            content,
        )
        self.assertIn("reused native decoder:", content)
        self.assertNotIn("python3 -m unittest discover -s tests -p", content)
        self.assertIn("native_decoder_ok", content)
        self.assertIn("--inspect-product-catalog gfs025", content)
        self.assertIn("--inspect-product-catalog cams_global", content)
        self.assertIn("--inspect-product-catalog ecmwf_ifs025", content)
        self.assertNotIn("crontab", content)
        self.assertNotIn("systemctl", content)
        self.assertNotIn("systemd", content)

    def test_1panel_v2_cronjob_installer_uses_role_based_names(self):
        content = Path("scripts/install_1panel_v2_cronjobs.py").read_text(encoding="utf-8")

        self.assertIn("--role", content)
        self.assertIn("--source-host", content)
        self.assertNotIn("43.162.112.201", content)
        self.assertIn("downloader", content)
        self.assertIn("api-publisher", content)
        self.assertIn("api-source-sync", content)
        self.assertIn("--download-workers 6", content)
        self.assertIn("--planning-workers 24", content)
        self.assertIn("--range-workers 48", content)
        self.assertIn("--object-fetch-mode auto", content)
        self.assertIn("--object-range-max-bytes 8388608", content)
        self.assertNotIn("--range-io-size-max 4194304", content)

        publisher_tasks = api_publisher_tasks(raw_root=Path("/tmp/raw"))
        self.assertEqual(
            [(name, spec) for name, spec, _script in publisher_tasks],
            [
                (
                    "OM_GFS_DOWNLOAD",
                    "0 * * * *&&20 * * * *&&40 * * * *",
                ),
                (
                    "OM_CAMS_DOWNLOAD",
                    "10 * * * *&&30 * * * *&&50 * * * *",
                ),
                ("OM_ECMWF_DOWNLOAD", "3 * * * *"),
            ],
        )
        for name, _spec, script in publisher_tasks:
            self.assertIn("check_1panel_download_tasks.py", script)
            self.assertIn(f"--current-task {name}", script)
            self.assertNotIn("flock", script)
        publisher_scripts = {name: script for name, _spec, script in publisher_tasks}
        self.assertIn(
            "--defer-openmeteo-gfs-activation",
            publisher_scripts["OM_GFS_DOWNLOAD"],
        )
        self.assertIn(
            "materialize_openmeteo_gfs.sh --raw-root /tmp/raw",
            publisher_scripts["OM_GFS_DOWNLOAD"],
        )
        self.assertIn(
            "weather_om_webp/scripts/run_scope.sh gfs",
            publisher_scripts["OM_GFS_DOWNLOAD"],
        )
        self.assertIn(
            "weather_om_webp/scripts/run_scope.sh cams",
            publisher_scripts["OM_CAMS_DOWNLOAD"],
        )
        self.assertIn(
            "weather_om_webp/scripts/run_scope.sh ecmwf_ifs025",
            publisher_scripts["OM_ECMWF_DOWNLOAD"],
        )
        self.assertLess(
            publisher_scripts["OM_GFS_DOWNLOAD"].index(
                "materialize_openmeteo_gfs.sh --raw-root /tmp/raw"
            ),
            publisher_scripts["OM_GFS_DOWNLOAD"].index(
                "weather_om_webp/scripts/run_scope.sh gfs"
            ),
        )
        for script in publisher_scripts.values():
            self.assertIn('pgrep -u "$(id -u)" -x om-api', script)
            self.assertIn('kill -HUP "${api_pids[0]}"', script)
            self.assertLess(
                script.index('kill -HUP "${api_pids[0]}"'),
                script.index("weather_om_webp/scripts/run_scope.sh"),
            )
        self.assertIn(
            '--now "$(date -u +%Y-%m-%dT%H:00:00Z)" || return $?',
            publisher_scripts["OM_GFS_DOWNLOAD"],
        )
        self.assertNotIn(
            "materialize_openmeteo_gfs.sh",
            publisher_scripts["OM_CAMS_DOWNLOAD"],
        )
        for _name, _spec, script in downloader_tasks():
            self.assertNotIn("materialize_openmeteo_gfs.sh", script)
            self.assertNotIn("--defer-openmeteo-gfs-activation", script)
            self.assertNotIn("weather_om_webp/scripts/run_scope.sh", script)

        removed_names = (
            "OM_BUILD_GFS013_SURFACE",
            "OM_BUILD_GFS_POINT_PACKAGE",
            "OM_BUILD_GFS_PRESSURE_PROFILE",
            "OM_BUILD_GFS_DERIVED",
            "OM_BUILD_CAMS_GLOBAL",
            "OM_CLEANUP",
        )
        self.assertEqual(REMOVED_PLACEHOLDER_TASKS, removed_names)

        tasks = api_source_sync_tasks(
            source_host="ubuntu@example.com",
            source_root=Path("/tmp/source"),
            source_ssh_key=Path("/tmp/id_ed25519"),
            source_known_hosts=Path("/tmp/known_hosts"),
            raw_root=Path("/tmp/raw"),
        )
        self.assertEqual(
            [name for name, _spec, _script in tasks],
            [
                "OM_GFS_DOWNLOAD",
                "OM_CAMS_DOWNLOAD",
                "OM_GFS_SOURCE_SYNC",
                "OM_CAMS_SOURCE_SYNC",
                "OM_ECMWF_DOWNLOAD",
            ],
        )
        self.assertEqual(
            [spec for _name, spec, _script in tasks],
            [
                "*/10 * * * *",
                "5,15,25,35,45,55 * * * *",
                "2-59/5 * * * *",
                "2-59/5 * * * *",
                "3 * * * *",
            ],
        )
        scripts = {name: script for name, _spec, script in tasks}
        self.assertIn("SOURCE_SYNC_TASK=OM_GFS_SOURCE_SYNC", scripts["OM_GFS_DOWNLOAD"])
        self.assertIn("SOURCE_SYNC_TASK=OM_CAMS_SOURCE_SYNC", scripts["OM_CAMS_DOWNLOAD"])
        self.assertIn(
            "上游同步任务已启用",
            scripts["OM_GFS_DOWNLOAD"],
        )
        self.assertIn("task_progress_reporter.py", scripts["OM_GFS_DOWNLOAD"])
        self.assertIn("--task 'GFS 下载'", scripts["OM_GFS_DOWNLOAD"])
        self.assertIn("--task 'CAMS 下载'", scripts["OM_CAMS_DOWNLOAD"])
        self.assertIn("--group gfs", scripts["OM_GFS_SOURCE_SYNC"])
        self.assertIn("--group cams", scripts["OM_CAMS_SOURCE_SYNC"])
        self.assertIn("--source-host ubuntu@example.com", scripts["OM_GFS_SOURCE_SYNC"])
        self.assertIn("--source-root /tmp/source", scripts["OM_GFS_SOURCE_SYNC"])
        self.assertIn("--source-ssh-key /tmp/id_ed25519", scripts["OM_GFS_SOURCE_SYNC"])
        self.assertIn("--source-known-hosts /tmp/known_hosts", scripts["OM_GFS_SOURCE_SYNC"])
        self.assertIn("--raw-root /tmp/raw", scripts["OM_GFS_SOURCE_SYNC"])
        self.assertIn("sudo -H -u ubuntu", scripts["OM_GFS_SOURCE_SYNC"])
        self.assertIn("sudo -H -u ubuntu", scripts["OM_CAMS_SOURCE_SYNC"])
        self.assertIn("--defer-gfs-activation", scripts["OM_GFS_SOURCE_SYNC"])
        self.assertNotIn("--defer-gfs-activation", scripts["OM_CAMS_SOURCE_SYNC"])

        sync_content = Path("scripts/sync_openmeteo_source_group.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("rsync -a --whole-file", sync_content)
        self.assertNotIn("rsync -az", sync_content)
        self.assertIn("flock -n 9", sync_content)
        self.assertIn('--files-from="$PAYLOAD_LIST"', sync_content)
        self.assertIn("--partial-dir=.rsync-partial", sync_content)
        self.assertIn("--source-ssh-key", sync_content)
        self.assertIn("--source-known-hosts", sync_content)
        self.assertIn("source_reconciliation_running", sync_content)
        self.assertIn("source publication changed during synchronization", sync_content)
        self.assertIn("gfs) RETENTION=5", sync_content)
        self.assertIn("manifest_status=0", sync_content)
        self.assertIn("payload_status=0", sync_content)
        self.assertIn('if [ "$manifest_status" -eq 23 ]', sync_content)
        self.assertIn('if [ "$payload_status" -eq 23 ]', sync_content)
        self.assertIn("--defer-openmeteo-gfs-activation", sync_content)
        self.assertIn("materialize_openmeteo_gfs.sh", sync_content)
        self.assertIn('pgrep -u "$(id -u)" -x om-api', sync_content)
        self.assertIn('kill -HUP "${api_pids[0]}"', sync_content)

        materializer_content = Path("scripts/materialize_openmeteo_gfs.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("flock -n 9", materializer_content)
        self.assertIn("build-and-publish", materializer_content)
        self.assertIn("OM_MODEL_STATIC_ROOT", materializer_content)
        self.assertIn("--model-static-root", materializer_content)
        self.assertIn("fixed model elevation checksum mismatch", materializer_content)
        self.assertNotIn('$RAW_ROOT/static/ncep_gfs013/HSURF.om', materializer_content)
        self.assertNotIn("2026072018", materializer_content)


if __name__ == "__main__":
    unittest.main()
