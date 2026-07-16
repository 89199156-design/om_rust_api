#!/usr/bin/env python3
import json
import sqlite3


def main() -> None:
    connection = sqlite3.connect("/opt/1panel/db/agent.db")
    connection.row_factory = sqlite3.Row
    rows = connection.execute(
        "select id,name,spec,status,is_executing "
        "from cronjobs where name like 'OM_%' order by id"
    ).fetchall()
    payload = []
    for row in rows:
        item = dict(row)
        record = connection.execute(
            "select start_time,interval,status,records from job_records "
            "where cronjob_id=? order by id desc limit 1",
            (row["id"],),
        ).fetchone()
        if record:
            item["last_record"] = {
                "start_time": record["start_time"],
                "interval": record["interval"],
                "status": record["status"],
                "records_tail": (record["records"] or "")[-500:],
            }
        payload.append(item)
    print(json.dumps(payload, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
