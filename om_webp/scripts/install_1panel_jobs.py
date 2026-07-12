#!/usr/bin/env python3
from __future__ import annotations

import datetime as dt
import shutil
import sqlite3
from pathlib import Path

AGENT_DB = Path("/opt/1panel/db/agent.db")
APP_DIR = Path("/opt/1panel/apps/weather_om_webp")


def now_text() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%d %H:%M:%S")


def ensure_group(cur: sqlite3.Cursor, timestamp: str) -> int:
    row = cur.execute(
        "select id from groups where type='cronjob' order by is_default desc,id limit 1"
    ).fetchone()
    if row:
        return int(row[0])
    cur.execute(
        "insert into groups (created_at,updated_at,is_default,name,type) values (?,?,1,'Default','cronjob')",
        (timestamp, timestamp),
    )
    return int(cur.lastrowid)


def values(timestamp: str, name: str, scope: str, group_id: int) -> dict[str, object]:
    script = "\n".join(
        [
            "#!/usr/bin/env bash",
            "set -euo pipefail",
            f"exec /usr/bin/env bash {APP_DIR}/scripts/run_scope.sh {scope}",
        ]
    )
    return {
        "updated_at": timestamp,
        "name": name,
        "type": "shell",
        "spec_custom": 0,
        "spec": "*/5 * * * *",
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


def main() -> int:
    if not AGENT_DB.exists():
        raise FileNotFoundError(AGENT_DB)
    backup = AGENT_DB.with_name(
        f"agent.db.backup_om_webp_{dt.datetime.now(dt.timezone.utc):%Y%m%dT%H%M%SZ}"
    )
    shutil.copy2(AGENT_DB, backup)
    timestamp = now_text()
    con = sqlite3.connect(AGENT_DB)
    try:
        cur = con.cursor()
        columns = {str(row[1]) for row in cur.execute("pragma table_info(cronjobs)")}
        group_id = ensure_group(cur, timestamp)
        for name, scope in (("OM_GFS_WEBP_BUILD", "gfs"), ("OM_CAMS_WEBP_BUILD", "cams")):
            payload = values(timestamp, name, scope, group_id)
            if "args" in columns:
                payload["args"] = ""
            payload = {key: value for key, value in payload.items() if key in columns}
            row = cur.execute("select id from cronjobs where name=?", (name,)).fetchone()
            if row:
                payload["id"] = int(row[0])
                assignments = ",".join(f"{key}=:{key}" for key in payload if key != "id")
                cur.execute(f"update cronjobs set {assignments} where id=:id", payload)
                print(f"UPDATED={name}")
            else:
                payload["created_at"] = timestamp
                names = ",".join(payload)
                placeholders = ",".join(f":{key}" for key in payload)
                cur.execute(f"insert into cronjobs ({names}) values ({placeholders})", payload)
                print(f"CREATED={name}")
        con.commit()
        print(f"AGENT_DB_BACKUP={backup}")
    finally:
        con.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
