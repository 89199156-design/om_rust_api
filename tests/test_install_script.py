from pathlib import Path
import unittest

from scripts.install_1panel_v2_cronjobs import (
    REMOVED_PLACEHOLDER_TASKS,
    mirror_sync_tasks,
)


class InstallScriptTests(unittest.TestCase):
    def test_install_script_contains_server_deploy_steps_without_system_scheduler(self):
        script = Path("scripts/install_from_zip.sh")
        content = script.read_text(encoding="utf-8")

        self.assertIn("set -euo pipefail", content)
        self.assertIn("/opt/1panel/apps/weather_om_downloader", content)
        self.assertIn("unzip", content)
        self.assertIn("scripts/build_turbopfor_decoder.sh", content)
        self.assertNotIn("python3 -m unittest discover -s tests -p", content)
        self.assertIn("native_decoder_ok", content)
        self.assertIn("--inspect-product-catalog gfs025", content)
        self.assertIn("--inspect-product-catalog cams_global", content)
        self.assertNotIn("crontab", content)
        self.assertNotIn("systemctl", content)
        self.assertNotIn("systemd", content)

    def test_1panel_v2_cronjob_installer_uses_role_based_names(self):
        content = Path("scripts/install_1panel_v2_cronjobs.py").read_text(encoding="utf-8")

        self.assertIn("--role", content)
        self.assertIn("--source-host", content)
        self.assertNotIn("43.162.112.201", content)
        self.assertIn("downloader", content)
        self.assertIn("mirror-sync", content)
        self.assertIn("api-publisher", content)
        self.assertIn("OM_GFS_DOWNLOAD", content)
        self.assertIn("OM_CAMS_DOWNLOAD", content)
        self.assertIn("*/10 * * * *", content)
        self.assertIn("--download-openmeteo-group {group}", content)
        self.assertIn("--download-workers 6", content)
        self.assertIn("--planning-workers 24", content)
        self.assertIn("--range-workers 48", content)
        self.assertIn("--object-fetch-mode auto", content)
        self.assertIn("--object-fetch-max-multiplier 1.5", content)
        self.assertIn("--object-fetch-min-ranges 16", content)
        self.assertIn("--object-range-merge-gap 16777216", content)
        self.assertIn("--object-range-max-multiplier 1.5", content)
        self.assertIn("--object-range-min-ranges 16", content)
        self.assertIn("--object-range-max-bytes 8388608", content)
        self.assertIn("sudo -H -u ubuntu", content)
        self.assertNotIn("--range-io-size-max 4194304", content)
        self.assertIn('download_group_script("gfs")', content)
        self.assertIn('download_group_script("cams")', content)
        self.assertIn("OM_MIRROR_SYNC", content)
        removed_names = (
            "OM_BUILD_GFS013_SURFACE",
            "OM_BUILD_GFS_POINT_PACKAGE",
            "OM_BUILD_GFS_PRESSURE_PROFILE",
            "OM_BUILD_GFS_DERIVED",
            "OM_BUILD_CAMS_GLOBAL",
            "OM_CLEANUP",
        )
        self.assertEqual(REMOVED_PLACEHOLDER_TASKS, removed_names)
        self.assertEqual(
            [name for name, _spec, _script in mirror_sync_tasks(
                source_host="ubuntu@example.com",
                mirror_root=Path("/tmp/mirror"),
                raw_root=Path("/tmp/raw"),
            )],
            ["OM_MIRROR_SYNC"],
        )
        self.assertIn("cleanup_names=REMOVED_PLACEHOLDER_TASKS", content)
        self.assertIn("--sync-openmeteo-group-from-source", content)
        self.assertIn("groups/{group}/latest.json", content)
        self.assertIn("for group, products in OPENMETEO_GROUP_PRODUCTS.items()", content)
        self.assertIn("/home/ubuntu/.ssh/id_ed25519", content)
        self.assertIn("StrictHostKeyChecking=accept-new", content)
        self.assertIn("rsync -a --whole-file", content)
        self.assertNotIn("rsync -az", content)
        self.assertIn("weather_om_mirror_sync.lock", content)
        self.assertIn("flock -n 9", content)
        self.assertIn("sync_product_files_from_manifest", content)
        self.assertIn("group_needs_sync", content)
        self.assertIn("--sync-openmeteo-group-from-source", content)
        self.assertIn("--mirror-root", content)
        self.assertNotIn("--write-om-http-status", content)
        self.assertNotIn("--build-status-root /data/build_status", content)
        self.assertEqual(
            mirror_sync_tasks(
                source_host="ubuntu@example.com",
                mirror_root=Path("/tmp/mirror"),
                raw_root=Path("/tmp/raw"),
            )[0][1],
            "5,15,25,35,45,55 * * * *",
        )
        self.assertNotIn('if pull_remote_file "{product}/latest.json"; then', content)
        self.assertIn('GROUP_READY=1', content)
        self.assertIn('if ! pull_remote_file "{product}/latest.json"; then', content)
        self.assertIn('if ! sync_product_files_from_manifest "$MANIFEST"; then', content)
        self.assertIn('if pull_remote_file "groups/{group}/latest.json"; then', content)
        self.assertIn("PurePosixPath(product) / str(item.get('path', ''))", content)
        self.assertIn("--files-from", content)
        self.assertIn(
            'rsync -a --whole-file --partial --timeout=180 --files-from="$file_list"',
            content,
        )
        self.assertNotIn('for rel in "${FILES[@]}"', content)
        self.assertIn('manifest_status_is_complete "$GROUP_MANIFEST"', content)
        self.assertIn('group_needs_sync "$GROUP_MANIFEST" "$RAW/groups/{group}/current/ready_for_processing.json"', content)
        self.assertIn("manifest.get('files') is not None", content)
        self.assertIn("manifest.get('bytes') is not None", content)
        self.assertNotIn('rsync -az --partial --delete --timeout=180 -e "$SSH_CMD" "$SOURCE_HOST:$SOURCE/" "$MIRROR/"', content)


if __name__ == "__main__":
    unittest.main()
