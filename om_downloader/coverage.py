from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone

from .metadata import OmRun
from .model_config import ProductConfig


@dataclass(frozen=True)
class CoverageSlot:
    valid_time_utc: datetime
    source_run: str
    forecast_hour: int


@dataclass(frozen=True)
class CoveragePlan:
    product: str
    required_start_utc: datetime
    required_end_utc: datetime
    latest_complete_run: str
    slots: tuple[CoverageSlot, ...]
    public_start_utc: datetime | None = None


def _as_utc(value: datetime) -> datetime:
    if value.tzinfo is None:
        return value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def _local_midnight_as_utc(now_utc: datetime, utc_offset_hours: int) -> datetime:
    local = _as_utc(now_utc).astimezone(timezone(timedelta(hours=utc_offset_hours)))
    local_midnight = local.replace(hour=0, minute=0, second=0, microsecond=0)
    return local_midnight.astimezone(timezone.utc)


def required_start_for_anchors(now_utc: datetime, timezone_anchors: tuple[int, ...]) -> datetime:
    if not timezone_anchors:
        raise ValueError("timezone_anchors must not be empty")
    candidates = [_local_midnight_as_utc(now_utc, offset) for offset in timezone_anchors]
    return min(candidates)


def _forecast_hour_for(run: OmRun, valid_time: datetime) -> int | None:
    valid_time_utc = _as_utc(valid_time)
    if run.valid_times_utc:
        available = {_as_utc(item) for item in run.valid_times_utc}
        if valid_time_utc not in available:
            return None
    delta = _as_utc(valid_time) - _as_utc(run.base_time_utc)
    total_seconds = delta.total_seconds()
    if total_seconds < 0 or total_seconds % 3600 != 0:
        return None
    hour = int(total_seconds // 3600)
    if hour > run.max_forecast_hour:
        return None
    return hour


def _target_valid_times(
    runs: list[OmRun],
    *,
    required_start: datetime,
    required_end: datetime,
) -> list[datetime]:
    if any(run.valid_times_utc for run in runs):
        values = {
            _as_utc(valid_time)
            for run in runs
            for valid_time in run.valid_times_utc
            if required_start <= _as_utc(valid_time) <= required_end
        }
        if not values:
            raise ValueError("no actual OM valid times cover requested window")
        return sorted(values)

    values = []
    cursor = required_start
    while cursor <= required_end:
        values.append(cursor)
        cursor += timedelta(hours=1)
    return values


def build_coverage_plan(product: ProductConfig, runs: list[OmRun], now_utc: datetime) -> CoveragePlan:
    if not runs:
        raise ValueError("no OM runs available")

    sorted_runs = sorted(runs, key=lambda item: _as_utc(item.base_time_utc))
    latest = sorted_runs[-1]
    forecast_hour_end = min(product.forecast_hour_end, latest.max_forecast_hour)
    public_start = required_start_for_anchors(now_utc, product.timezone_anchors)
    required_start = public_start - timedelta(hours=product.history_hours)
    required_end = _as_utc(latest.base_time_utc) + timedelta(hours=forecast_hour_end)

    slots: list[CoverageSlot] = []
    for cursor in _target_valid_times(
        sorted_runs,
        required_start=required_start,
        required_end=required_end,
    ):
        candidates: list[tuple[datetime, str, int]] = []
        for run in sorted_runs:
            forecast_hour = _forecast_hour_for(run, cursor)
            if forecast_hour is not None:
                candidates.append((_as_utc(run.base_time_utc), run.run_id, forecast_hour))
        if not candidates:
            raise ValueError(f"no source run covers valid_time={cursor.isoformat()}")
        _base_time, source_run, forecast_hour = sorted(candidates)[-1]
        slots.append(CoverageSlot(cursor, source_run, forecast_hour))

    return CoveragePlan(
        product=product.name,
        required_start_utc=required_start,
        required_end_utc=required_end,
        latest_complete_run=latest.run_id,
        slots=tuple(slots),
        public_start_utc=public_start,
    )


def build_complete_run_coverage_plan(product: ProductConfig, run: OmRun) -> CoveragePlan:
    """Build a coverage containing only one complete model run."""
    base_time = _as_utc(run.base_time_utc)
    forecast_hour_end = min(product.forecast_hour_end, run.max_forecast_hour)
    required_end = base_time + timedelta(hours=forecast_hour_end)
    valid_times = (
        sorted(
            _as_utc(valid_time)
            for valid_time in run.valid_times_utc
            if base_time <= _as_utc(valid_time) <= required_end
        )
        if run.valid_times_utc
        else [base_time + timedelta(hours=hour) for hour in range(forecast_hour_end + 1)]
    )
    if not valid_times or valid_times[0] != base_time or valid_times[-1] != required_end:
        raise ValueError(f"run {run.run_id} does not contain the requested complete window")

    slots = tuple(
        CoverageSlot(
            valid_time_utc=valid_time,
            source_run=run.run_id,
            forecast_hour=int((valid_time - base_time).total_seconds() // 3600),
        )
        for valid_time in valid_times
    )
    return CoveragePlan(
        product=product.name,
        required_start_utc=base_time,
        required_end_utc=required_end,
        latest_complete_run=run.run_id,
        slots=slots,
        public_start_utc=base_time,
    )


def build_run_forecast_hour_coverage_plan(
    product: ProductConfig,
    run: OmRun,
    *,
    forecast_hour_end: int,
) -> CoveragePlan:
    """Build a single-run coverage limited to an inclusive forecast-hour end."""
    if forecast_hour_end < 0:
        raise ValueError("forecast_hour_end must not be negative")
    base_time = _as_utc(run.base_time_utc)
    effective_end = min(product.forecast_hour_end, run.max_forecast_hour, forecast_hour_end)
    required_end = base_time + timedelta(hours=effective_end)
    valid_times = (
        sorted(
            _as_utc(valid_time)
            for valid_time in run.valid_times_utc
            if base_time <= _as_utc(valid_time) <= required_end
        )
        if run.valid_times_utc
        else [base_time + timedelta(hours=hour) for hour in range(effective_end + 1)]
    )
    expected_times = [base_time + timedelta(hours=hour) for hour in range(effective_end + 1)]
    if valid_times != expected_times:
        raise ValueError(
            f"run {run.run_id} does not contain every hour through forecast hour {effective_end}"
        )
    slots = tuple(
        CoverageSlot(
            valid_time_utc=valid_time,
            source_run=run.run_id,
            forecast_hour=int((valid_time - base_time).total_seconds() // 3600),
        )
        for valid_time in valid_times
    )
    return CoveragePlan(
        product=product.name,
        required_start_utc=base_time,
        required_end_utc=required_end,
        latest_complete_run=run.run_id,
        slots=slots,
        public_start_utc=base_time,
    )
