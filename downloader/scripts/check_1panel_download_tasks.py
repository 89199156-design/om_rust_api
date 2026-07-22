#!/usr/bin/env python3
"""Decide whether one of the two 1Panel OM download tasks may start."""

from __future__ import annotations

import argparse
from contextlib import closing
import sqlite3
import sys
from pathlib import Path


TASKS = ("OM_GFS_DOWNLOAD", "OM_CAMS_DOWNLOAD")
WEBP_TASKS = ("OM_GFS_WEBP_BUILD", "OM_CAMS_WEBP_BUILD")
ALL_PRODUCTION_TASKS = TASKS + WEBP_TASKS


def decision(database: Path, current_task: str) -> tuple[str, str]:
    if current_task not in TASKS:
        raise ValueError(f"unsupported download task: {current_task}")

    with closing(sqlite3.connect(database)) as connection:
        rows = {
            str(name): int(is_executing or 0)
            for name, is_executing in connection.execute(
                "select name, is_executing from cronjobs where name in (?, ?, ?, ?)",
                ALL_PRODUCTION_TASKS,
            )
        }

    missing = [name for name in ALL_PRODUCTION_TASKS if name not in rows]
    if missing:
        raise RuntimeError(f"1Panel production task is missing: {', '.join(missing)}")

    # 1Panel marks the current row as executing before invoking its script, so
    # that row represents this invocation. All other production jobs share the
    # native data tree and must be idle while a download publishes atomically.
    conflicts = [
        name
        for name in ALL_PRODUCTION_TASKS
        if name != current_task and rows[name] == 1
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
        print(f"1Panel 任务状态检查失败：{error}", file=sys.stderr)
        return 2
    print(f"{action}|{reason}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
