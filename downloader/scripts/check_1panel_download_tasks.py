#!/usr/bin/env python3
"""Decide whether one of the two 1Panel OM download tasks may start."""

from __future__ import annotations

import argparse
from contextlib import closing
import sqlite3
import sys
from pathlib import Path


TASKS = ("OM_GFS_DOWNLOAD", "OM_CAMS_DOWNLOAD")


def decision(database: Path, current_task: str) -> tuple[str, str]:
    if current_task not in TASKS:
        raise ValueError(f"unsupported download task: {current_task}")

    peer_task = TASKS[1] if current_task == TASKS[0] else TASKS[0]
    with closing(sqlite3.connect(database)) as connection:
        rows = {
            str(name): int(is_executing or 0)
            for name, is_executing in connection.execute(
                "select name, is_executing from cronjobs where name in (?, ?)", TASKS
            )
        }

    missing = [name for name in TASKS if name not in rows]
    if missing:
        raise RuntimeError(f"1Panel download task is missing: {', '.join(missing)}")

    # 1Panel marks the current row as executing before invoking its script, so
    # that row represents this invocation.  A running peer is the only conflict.
    if rows[peer_task] == 1:
        return "skip", f"{peer_task} 正在执行"
    return "run", "两个下载任务均无其他执行实例"


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
