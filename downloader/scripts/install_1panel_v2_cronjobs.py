#!/usr/bin/env python3
"""Install weather OM jobs into 1Panel v2 agent cronjobs.

This script is intended to run on the 1Panel server as root.
It is idempotent: existing OM jobs are updated in place.
"""

from __future__ import annotations

import argparse
import datetime as dt
import shlex
import shutil
import sqlite3
from pathlib import Path


AGENT_DB = Path("/opt/1panel/db/agent.db")
LEGACY_DB = Path("/opt/1panel/db/1Panel.db")
APP_DIR = Path("/opt/1panel/apps/weather_om_downloader")
NATIVE_LIB = APP_DIR / "native" / "libom_turbopfor.so"
CONFIG = APP_DIR / "config" / "models.json"
PYTHON = Path("/usr/bin/python3")
PRODUCTS = (
    "gfs013_surface",
    "gfs025",
    "gfs_pressure_profile",
    "cams_global",
    "cams_global_greenhouse_gases",
)
OPENMETEO_GROUP_PRODUCTS = {
    "gfs": ("gfs013_surface", "gfs025", "gfs_pressure_profile"),
    "cams": ("cams_global", "cams_global_greenhouse_gases"),
}
REMOVED_PLACEHOLDER_TASKS = (
    "OM_BUILD_GFS013_SURFACE",
    "OM_BUILD_GFS_POINT_PACKAGE",
    "OM_BUILD_GFS_PRESSURE_PROFILE",
    "OM_BUILD_GFS_DERIVED",
    "OM_BUILD_CAMS_GLOBAL",
    "OM_CLEANUP",
)


def shell_path(path: Path) -> str:
    return str(path).replace("\\", "/")


def now_text() -> str:
    tz = dt.timezone(dt.timedelta(hours=8))
    return dt.datetime.now(tz).strftime("%Y-%m-%d %H:%M:%S.%f+08:00")


def backup(path: Path) -> Path:
    stamp = dt.datetime.utcnow().strftime("%Y%m%dT%H%M%SZ")
    target = path.with_name(f"{path.name}.bak.om_tasks.{stamp}")
    shutil.copy2(path, target)
    return target


def download_script(product: str) -> str:
    return "\n".join(
        [
            "#!/usr/bin/env bash",
            "set -euo pipefail",
            f"cd {shell_path(APP_DIR)}",
            f"export OM_TURBOPFOR_LIB={shell_path(NATIVE_LIB)}",
            "/usr/bin/python3 -m om_downloader.cli "
            f"--download-openmeteo-product {product} "
            f"--config {shell_path(CONFIG)} "
            "--output data "
            '--now "$(date -u +%Y-%m-%dT%H:00:00Z)"',
            "",
        ]
    )


def download_group_script(
    group: str,
    *,
    publish_root: Path | None = None,
    source_sync_task: str | None = None,
) -> str:
    publish_args = []
    if publish_root is not None:
        publish_args = [f"--publish-openmeteo-group-to {shell_path(publish_root)} "]
    command = (
        "/usr/bin/python3 -m om_downloader.cli "
        f"--download-openmeteo-group {group} "
        f"--config {shell_path(CONFIG)} "
        "--download-workers 6 "
        "--planning-workers 24 "
        "--range-workers 48 "
        "--object-fetch-mode auto "
        "--object-fetch-max-multiplier 1.5 "
        "--object-fetch-min-ranges 16 "
        "--object-range-merge-gap 16777216 "
        "--object-range-max-multiplier 1.5 "
        "--object-range-min-ranges 16 "
        "--object-range-max-bytes 8388608 "
        "--output data "
        + "".join(publish_args)
        + '--now "$(date -u +%Y-%m-%dT%H:00:00Z)"'
    )
    return "\n".join(
        [
            "#!/usr/bin/env bash",
            "set -euo pipefail",
            *(
                [
                    f"AGENT_DB={shell_path(AGENT_DB)}",
                    f"SOURCE_SYNC_TASK={shlex.quote(source_sync_task)}",
                    "source_sync_status() {",
                    f"  {shell_path(PYTHON)} - \"$AGENT_DB\" \"$SOURCE_SYNC_TASK\" <<'PY'",
                    "import sqlite3",
                    "import sys",
                    "with sqlite3.connect(sys.argv[1]) as connection:",
                    "    row = connection.execute('select status from cronjobs where name = ?', (sys.argv[2],)).fetchone()",
                    "print(row[0] if row else '')",
                    "PY",
                    "}",
                    'if [ "$(source_sync_status)" = "Enable" ]; then',
                    "  printf '%s\\n' '{\"status\":\"skipped\",\"reason\":\"upstream source sync task enabled; direct download skipped\",\"group\":\""
                    + group
                    + "\",\"source_sync_task\":\""
                    + source_sync_task
                    + "\"}'",
                    "  exit 0",
                    "fi",
                ]
                if source_sync_task
                else []
            ),
            "run_download() {",
            f"  cd {shell_path(APP_DIR)}",
            f"  export OM_TURBOPFOR_LIB={shell_path(NATIVE_LIB)}",
            f"  {command}",
            "}",
            'if [ "$(id -u)" -eq 0 ]; then',
            '  exec sudo -H -u ubuntu bash -lc "$(declare -f run_download); run_download"',
            "fi",
            "run_download",
            "",
        ]
    )


def downloader_tasks() -> list[tuple[str, str, str]]:
    return [
        ("OM_GFS_DOWNLOAD", "*/10 * * * *", download_group_script("gfs")),
        ("OM_CAMS_DOWNLOAD", "*/10 * * * *", download_group_script("cams")),
    ]


def api_publisher_tasks(*, raw_root: Path) -> list[tuple[str, str, str]]:
    return [
        ("OM_GFS_DOWNLOAD", "*/10 * * * *", download_group_script("gfs", publish_root=raw_root)),
        (
            "OM_CAMS_DOWNLOAD",
            "5,15,25,35,45,55 * * * *",
            download_group_script("cams", publish_root=raw_root),
        ),
    ]


def source_sync_task_script(
    *,
    group: str,
    source_host: str,
    source_root: Path,
    source_ssh_key: Path,
    source_known_hosts: Path,
    raw_root: Path,
) -> str:
    if group not in OPENMETEO_GROUP_PRODUCTS:
        raise ValueError(f"unknown group: {group}")
    arguments = [
        "--group", group,
        "--source-host", source_host,
        "--source-root", shell_path(source_root),
        "--raw-root", shell_path(raw_root),
        "--source-ssh-key", shell_path(source_ssh_key),
        "--source-known-hosts", shell_path(source_known_hosts),
        "--cleanup-grace-seconds", "300",
    ]
    command = " ".join(
        [
            "/usr/bin/env",
            "bash",
            shlex.quote(shell_path(APP_DIR / "scripts" / "sync_openmeteo_source_group.sh")),
        ]
        + [shlex.quote(argument) for argument in arguments]
    )
    return "\n".join(
        [
            "#!/usr/bin/env bash",
            "set -euo pipefail",
            'if [ "$(id -u)" -eq 0 ]; then',
            f"  exec sudo -H -u ubuntu {command}",
            "fi",
            f"exec {command}",
            "",
        ]
    )


def api_source_sync_tasks(
    *,
    source_host: str,
    source_root: Path,
    source_ssh_key: Path,
    source_known_hosts: Path,
    raw_root: Path,
) -> list[tuple[str, str, str]]:
    return [
        (
            "OM_GFS_DOWNLOAD",
            "*/10 * * * *",
            download_group_script(
                "gfs",
                publish_root=raw_root,
                source_sync_task="OM_GFS_SOURCE_SYNC",
            ),
        ),
        (
            "OM_CAMS_DOWNLOAD",
            "5,15,25,35,45,55 * * * *",
            download_group_script(
                "cams",
                publish_root=raw_root,
                source_sync_task="OM_CAMS_SOURCE_SYNC",
            ),
        ),
        (
            "OM_GFS_SOURCE_SYNC",
            "2-59/5 * * * *",
            source_sync_task_script(
                group="gfs",
                source_host=source_host,
                source_root=source_root,
                source_ssh_key=source_ssh_key,
                source_known_hosts=source_known_hosts,
                raw_root=raw_root,
            ),
        ),
        (
            "OM_CAMS_SOURCE_SYNC",
            "2-59/5 * * * *",
            source_sync_task_script(
                group="cams",
                source_host=source_host,
                source_root=source_root,
                source_ssh_key=source_ssh_key,
                source_known_hosts=source_known_hosts,
                raw_root=raw_root,
            ),
        ),
    ]


def ensure_cronjob_group(cur: sqlite3.Cursor, timestamp: str) -> int:
    row = cur.execute(
        "select id from groups where type = 'cronjob' and name = 'Default' order by id limit 1"
    ).fetchone()
    if row:
        return int(row[0])
    cur.execute(
        """
        insert into groups (created_at, updated_at, is_default, name, type)
        values (?, ?, 1, 'Default', 'cronjob')
        """,
        (timestamp, timestamp),
    )
    return int(cur.lastrowid)


def _cronjob_values(timestamp: str, name: str, spec: str, script: str, group_id: int) -> dict[str, object]:
    return {
        "updated_at": timestamp,
        "name": name,
        "type": "shell",
        "spec_custom": 0,
        "spec": spec,
        "executor": "bash",
        "command": "",
        "container_name": "",
        "script_mode": "input",
        "script": script,
        "user": "",
        "script_id": 0,
        "website": "",
        "app_id": "",
        "db_type": "",
        "db_name": "",
        "url": "",
        "is_dir": 0,
        "source_dir": "",
        "exclusion_rules": "",
        "source_account_ids": "",
        "download_account_id": 0,
        "retry_times": 0,
        "timeout": 21600,
        "retain_copies": 7,
        "status": "Enable",
        "entry_ids": "",
        "secret": "",
        "group_id": group_id,
        "snapshot_rule": "{}",
        "ignore_err": 0,
        "is_executing": 0,
        "config": "{}",
    }


def install_agent_jobs(tasks: list[tuple[str, str, str]], *, cleanup_names: tuple[str, ...] = ()) -> None:
    if not AGENT_DB.exists():
        raise FileNotFoundError(f"{AGENT_DB} does not exist")
    if not APP_DIR.exists():
        raise FileNotFoundError(f"{APP_DIR} does not exist")

    backup_path = backup(AGENT_DB)
    print(f"AGENT_DB_BACKUP={backup_path}")

    timestamp = now_text()
    con = sqlite3.connect(str(AGENT_DB))
    try:
        cur = con.cursor()
        cronjob_columns = {str(row[1]) for row in cur.execute("pragma table_info(cronjobs)")}
        group_id = ensure_cronjob_group(cur, timestamp)
        print(f"CRONJOB_GROUP_ID={group_id}")
        created: list[str] = []
        updated: list[str] = []
        for name in cleanup_names:
            cur.execute("delete from cronjobs where name = ?", (name,))
        for name, spec, script in tasks:
            row = cur.execute("select id from cronjobs where name = ?", (name,)).fetchone()
            values = _cronjob_values(timestamp, name, spec, script, group_id)
            if "args" in cronjob_columns:
                values["args"] = ""
            values = {key: value for key, value in values.items() if key in cronjob_columns}
            if row:
                assignments = ", ".join(f"{key}=:{key}" for key in values)
                values["id"] = row[0]
                cur.execute(f"update cronjobs set {assignments} where id=:id", values)
                updated.append(name)
            else:
                values["created_at"] = timestamp
                columns = ", ".join(values)
                placeholders = ", ".join(f":{key}" for key in values)
                cur.execute(f"insert into cronjobs ({columns}) values ({placeholders})", values)
                created.append(name)
        con.commit()

        print("CREATED=" + (",".join(created) or "-"))
        print("UPDATED=" + (",".join(updated) or "-"))
        if cleanup_names:
            print("CLEANUP_NAMES=" + ",".join(cleanup_names))
        for row in cur.execute(
            "select id,name,type,spec,status,entry_ids,timeout,retry_times "
            "from cronjobs where name like 'OM_%' order by id"
        ):
            print("|".join(str(item) for item in row))
    finally:
        con.close()


def clean_legacy_jobs(names: tuple[str, ...]) -> None:
    if not LEGACY_DB.exists():
        return
    con = sqlite3.connect(str(LEGACY_DB))
    try:
        count = con.execute(
            "select count(*) from cronjobs where name like 'om-download-%' or name like 'OM_%'"
        ).fetchone()[0]
        if count:
            backup_path = backup(LEGACY_DB)
            con.execute("delete from cronjobs where name like 'om-download-%' or name like 'OM_%'")
            con.commit()
            print(f"LEGACY_DB_CLEANED={count}")
            print(f"LEGACY_DB_BACKUP={backup_path}")
        else:
            print("LEGACY_DB_CLEANED=0")
    finally:
        con.close()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--role",
        choices=("downloader", "api-publisher", "api-source-sync"),
        default="downloader",
    )
    parser.add_argument("--source-host")
    parser.add_argument("--source-root")
    parser.add_argument("--source-ssh-key")
    parser.add_argument("--source-known-hosts")
    parser.add_argument("--raw-root", default="/data/om_raw")
    args = parser.parse_args(argv)

    legacy_download_task_names = (
        "om-download-gfs013-surface",
        "om-download-gfs025",
        "om-download-gfs-pressure-profile",
        "om-download-cams-global",
        "OM_GFS013_DOWNLOAD",
        "OM_GFS025_DOWNLOAD",
        "OM_GFS_PRESSURE_DOWNLOAD",
    )
    clean_legacy_jobs(legacy_download_task_names)
    if args.role == "downloader":
        install_agent_jobs(downloader_tasks(), cleanup_names=legacy_download_task_names)
    elif args.role == "api-publisher":
        install_agent_jobs(
            api_publisher_tasks(raw_root=Path(args.raw_root)),
            cleanup_names=("OM_MIRROR_SYNC",) + REMOVED_PLACEHOLDER_TASKS,
        )
    else:
        if not all((args.source_host, args.source_root, args.source_ssh_key, args.source_known_hosts)):
            parser.error(
                "--source-host, --source-root, --source-ssh-key, and --source-known-hosts are required "
                "with --role api-source-sync"
            )
        install_agent_jobs(
            api_source_sync_tasks(
                source_host=args.source_host,
                source_root=Path(args.source_root),
                source_ssh_key=Path(args.source_ssh_key),
                source_known_hosts=Path(args.source_known_hosts),
                raw_root=Path(args.raw_root),
            ),
            cleanup_names=("OM_MIRROR_SYNC",) + REMOVED_PLACEHOLDER_TASKS,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
