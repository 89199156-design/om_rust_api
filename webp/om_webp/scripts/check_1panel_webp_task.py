#!/usr/bin/env python3
"""Check 1Panel state before starting a WebP production task."""

from __future__ import annotations

import argparse
from contextlib import closing
import sqlite3
import sys
from pathlib import Path


TASKS = ("OM_GFS_WEBP_BUILD", "OM_CAMS_WEBP_BUILD")
ACTIVE_RECORD_STATUSES = ("Running", "Waiting")


def decision(database: Path, current_task: str) -> tuple[str, str]:
    if current_task not in TASKS:
        raise ValueError(f"unsupported WebP task: {current_task}")
    with closing(sqlite3.connect(database)) as connection:
        row = connection.execute(
            "select id, is_executing from cronjobs where name = ?", (current_task,)
        ).fetchone()
        if row is None:
            raise RuntimeError(f"1Panel WebP task is missing: {current_task}")
        task_id, is_executing = int(row[0]), int(row[1] or 0)
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
    return "run", "本任务没有上一执行实例"


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
