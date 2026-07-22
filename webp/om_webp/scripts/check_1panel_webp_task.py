#!/usr/bin/env python3
"""Check 1Panel state before starting a WebP production task."""

from __future__ import annotations

import argparse
from contextlib import closing
import sqlite3
import sys
from pathlib import Path


DOWNLOAD_TASKS = ("OM_GFS_DOWNLOAD", "OM_CAMS_DOWNLOAD")
TASKS = ("OM_GFS_WEBP_BUILD", "OM_CAMS_WEBP_BUILD")
ALL_PRODUCTION_TASKS = DOWNLOAD_TASKS + TASKS
ACTIVE_RECORD_STATUSES = ("Running", "Waiting")


def decision(database: Path, current_task: str) -> tuple[str, str]:
    if current_task not in TASKS:
        raise ValueError(f"unsupported WebP task: {current_task}")
    with closing(sqlite3.connect(database)) as connection:
        rows = {
            str(name): (int(task_id), int(is_executing or 0))
            for task_id, name, is_executing in connection.execute(
                "select id, name, is_executing from cronjobs "
                "where name in (?, ?, ?, ?)",
                ALL_PRODUCTION_TASKS,
            )
        }
        missing = [name for name in ALL_PRODUCTION_TASKS if name not in rows]
        if missing:
            raise RuntimeError(
                f"1Panel production task is missing: {', '.join(missing)}"
            )
        task_id, is_executing = rows[current_task]
        active_records = int(
            connection.execute(
                "select count(*) from job_records where cronjob_id = ? and status in (?, ?)",
                (task_id, *ACTIVE_RECORD_STATUSES),
            ).fetchone()[0]
        )

    # is_executing=1 is the invocation currently running this check.  More
    # than one active record means an older instance is still present.
    if is_executing == 1 and active_records > 1:
        return "skip", "本任务已有上一实例仍在执行"
    conflicts = [
        name
        for name in ALL_PRODUCTION_TASKS
        if name != current_task and rows[name][1] == 1
    ]
    if conflicts:
        return "skip", f"{', '.join(conflicts)} 正在执行"
    return "run", "其他下载与 WebP 任务均未执行"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--database", type=Path, default=Path("/opt/1panel/db/agent.db"))
    parser.add_argument("--current-task", choices=TASKS, required=True)
    args = parser.parse_args()
    try:
        action, reason = decision(args.database, args.current_task)
    except Exception as error:
        print(f"1Panel WebP 任务状态检查失败：{error}", file=sys.stderr)
        return 2
    print(f"{action}|{reason}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
