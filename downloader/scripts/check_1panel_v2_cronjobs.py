#!/usr/bin/env python3
"""Check 1Panel v2 cronjob rows in agent.db."""

from __future__ import annotations

import argparse
import sqlite3
from pathlib import Path


AGENT_DB = Path("/opt/1panel/db/agent.db")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--expect", action="append", default=[])
    parser.add_argument("--require-entry-ids", action="store_true")
    args = parser.parse_args(argv)

    con = sqlite3.connect(str(AGENT_DB))
    try:
        rows = con.execute(
            "select name,type,spec,status,entry_ids,timeout,retry_times "
            "from cronjobs where name like 'OM_%' order by name"
        ).fetchall()
    finally:
        con.close()

    by_name = {row[0]: row for row in rows}
    print(f"COUNT={len(rows)}")
    for row in rows:
        print("|".join(str(item) for item in row))

    missing = [name for name in args.expect if name not in by_name]
    if missing:
        print("MISSING=" + ",".join(missing))
        return 2
    if args.require_entry_ids:
        empty = [name for name in args.expect if not str(by_name[name][4])]
        if empty:
            print("EMPTY_ENTRY_IDS=" + ",".join(empty))
            return 3
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
