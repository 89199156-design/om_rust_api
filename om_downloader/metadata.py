from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
import json
from pathlib import Path


@dataclass(frozen=True)
class OmRun:
    run_id: str
    base_time_utc: datetime
    max_forecast_hour: int
    variables: tuple[str, ...]
    pressure_levels_hpa: tuple[int, ...]
    valid_times_utc: tuple[datetime, ...] = ()


def parse_utc_datetime(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def load_fixture_runs(path: Path) -> list[OmRun]:
    data = json.loads(path.read_text(encoding="utf-8"))
    runs = []
    for item in data["runs"]:
        runs.append(
            OmRun(
                run_id=str(item["run_id"]),
                base_time_utc=parse_utc_datetime(str(item["base_time_utc"])),
                max_forecast_hour=int(item["max_forecast_hour"]),
                variables=tuple(str(v) for v in item.get("variables", [])),
                pressure_levels_hpa=tuple(int(v) for v in item.get("pressure_levels_hpa", [])),
                valid_times_utc=tuple(
                    parse_utc_datetime(str(value))
                    for value in item.get("valid_times_utc", [])
                ),
            )
        )
    return runs
