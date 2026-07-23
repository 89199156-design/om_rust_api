from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
import json
from typing import Any
from urllib.request import urlopen

from .coverage import CoveragePlan
from .metadata import OmRun
from .om_inventory import infer_pressure_levels_hpa


DEFAULT_OPENMETEO_BUCKET_URL = "https://openmeteo.s3.amazonaws.com"


@dataclass(frozen=True)
class OpenMeteoSpatialCatalog:
    model: str
    completed: bool
    reference_time_utc: datetime
    valid_times_utc: tuple[datetime, ...]
    variables: tuple[str, ...]
    last_modified_time_utc: datetime | None = None
    crs_wkt: str | None = None

    @property
    def available_variables(self) -> tuple[str, ...]:
        return tuple(sorted(self.variables))

    @property
    def max_forecast_hour(self) -> int:
        if not self.valid_times_utc:
            return 0
        latest = max(self.valid_times_utc)
        return int((latest - self.reference_time_utc).total_seconds() // 3600)


def _parse_utc(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def _payload_to_entries(payload: bytes | str) -> list[dict[str, Any]]:
    text = payload.decode("utf-8") if isinstance(payload, bytes) else payload
    raw = json.loads(text)
    if isinstance(raw, dict):
        return [raw]
    if isinstance(raw, list) and all(isinstance(item, dict) for item in raw):
        return raw
    raise ValueError("Open-Meteo spatial latest payload must be a JSON object or array")


def parse_openmeteo_spatial_latest(model: str, payload: bytes | str) -> OpenMeteoSpatialCatalog:
    entries = _payload_to_entries(payload)
    completed_entries = [entry for entry in entries if bool(entry.get("completed", False))]
    entry = completed_entries[0] if completed_entries else entries[0]
    reference_time = _parse_utc(str(entry["reference_time"]))
    valid_times = tuple(_parse_utc(str(item)) for item in entry.get("valid_times", []))
    variables = tuple(str(item) for item in entry.get("variables", []))
    last_modified = entry.get("last_modified_time")
    return OpenMeteoSpatialCatalog(
        model=model,
        completed=bool(entry.get("completed", False)),
        reference_time_utc=reference_time,
        valid_times_utc=valid_times,
        variables=variables,
        last_modified_time_utc=_parse_utc(str(last_modified)) if last_modified else None,
        crs_wkt=str(entry["crs_wkt"]) if "crs_wkt" in entry else None,
    )


def openmeteo_spatial_latest_url(
    bucket_url: str,
    model: str,
) -> str:
    return f"{bucket_url.rstrip('/')}/data_spatial/{model}/latest.json"


def _coerce_utc(value: datetime | str) -> datetime:
    if isinstance(value, datetime):
        parsed = value
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=timezone.utc)
        return parsed.astimezone(timezone.utc)
    return _parse_utc(value)


def _run_prefix(model: str, reference_time_utc: datetime | str) -> str:
    reference_time = _coerce_utc(reference_time_utc)
    return (
        f"data_spatial/{model}/"
        f"{reference_time:%Y/%m/%d}/"
        f"{reference_time:%H}00Z"
    )


def openmeteo_spatial_object_key(
    model: str,
    *,
    reference_time_utc: datetime | str,
    valid_time_utc: datetime | str,
) -> str:
    valid_time = _coerce_utc(valid_time_utc)
    return f"{_run_prefix(model, reference_time_utc)}/{valid_time:%Y-%m-%dT%H%M}.om"


def openmeteo_spatial_object_url(
    bucket_url: str,
    model: str,
    *,
    reference_time_utc: datetime | str,
    valid_time_utc: datetime | str,
) -> str:
    key = openmeteo_spatial_object_key(
        model,
        reference_time_utc=reference_time_utc,
        valid_time_utc=valid_time_utc,
    )
    return f"{bucket_url.rstrip('/')}/{key}"


def openmeteo_spatial_run_meta_url(
    bucket_url: str,
    model: str,
    *,
    reference_time_utc: datetime | str,
) -> str:
    return f"{bucket_url.rstrip('/')}/{_run_prefix(model, reference_time_utc)}/meta.json"


def load_openmeteo_spatial_latest(
    model: str,
    *,
    bucket_url: str = DEFAULT_OPENMETEO_BUCKET_URL,
    timeout: int = 30,
) -> OpenMeteoSpatialCatalog:
    with urlopen(openmeteo_spatial_latest_url(bucket_url, model), timeout=timeout) as response:
        payload = response.read()
    return parse_openmeteo_spatial_latest(model, payload)


def load_openmeteo_spatial_run(
    model: str,
    reference_time_utc: datetime | str,
    *,
    bucket_url: str = DEFAULT_OPENMETEO_BUCKET_URL,
    timeout: int = 30,
) -> OpenMeteoSpatialCatalog:
    reference_time = _coerce_utc(reference_time_utc)
    with urlopen(
        openmeteo_spatial_run_meta_url(
            bucket_url,
            model,
            reference_time_utc=reference_time,
        ),
        timeout=timeout,
    ) as response:
        payload = response.read()
    catalog = parse_openmeteo_spatial_latest(model, payload)
    if catalog.reference_time_utc != reference_time:
        raise ValueError(
            f"run metadata reference time mismatch for {model}: "
            f"expected {reference_time.isoformat()}, got {catalog.reference_time_utc.isoformat()}"
        )
    return catalog


def om_run_from_spatial_catalog(product_name: str, catalog: OpenMeteoSpatialCatalog) -> OmRun:
    return OmRun(
        run_id=catalog.reference_time_utc.strftime("%Y%m%d%H"),
        base_time_utc=catalog.reference_time_utc,
        max_forecast_hour=catalog.max_forecast_hour,
        variables=catalog.variables,
        pressure_levels_hpa=tuple(infer_pressure_levels_hpa(catalog.variables)),
        valid_times_utc=catalog.valid_times_utc,
    )


def _format_utc(value: datetime) -> str:
    return value.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def required_reference_times_for_coverage(
    latest_catalog: OpenMeteoSpatialCatalog,
    *,
    required_start_utc: datetime,
    run_cadence_hours: int,
) -> tuple[datetime, ...]:
    if run_cadence_hours <= 0:
        raise ValueError("run_cadence_hours must be positive")

    required_start = _coerce_utc(required_start_utc)
    cursor = latest_catalog.reference_time_utc
    reference_times = [cursor]
    while cursor > required_start:
        cursor = cursor - timedelta(hours=run_cadence_hours)
        reference_times.append(cursor)
    return tuple(sorted(reference_times))


def _fetch_url(url: str, timeout: int) -> bytes:
    with urlopen(url, timeout=timeout) as response:
        return response.read()


def discover_openmeteo_spatial_runs(
    product_name: str,
    latest_catalog: OpenMeteoSpatialCatalog,
    *,
    bucket_url: str = DEFAULT_OPENMETEO_BUCKET_URL,
    required_start_utc: datetime,
    run_cadence_hours: int,
    required_long_run_forecast_hour: int | None = None,
    max_additional_runs: int = 8,
    timeout: int = 30,
    fetch=None,
) -> list[OmRun]:
    if required_long_run_forecast_hour is not None and required_long_run_forecast_hour < 0:
        raise ValueError("required_long_run_forecast_hour must not be negative")
    if max_additional_runs < 0:
        raise ValueError("max_additional_runs must not be negative")

    fetch = fetch or (lambda url: _fetch_url(url, timeout))
    reference_times = list(required_reference_times_for_coverage(
        latest_catalog,
        required_start_utc=required_start_utc,
        run_cadence_hours=run_cadence_hours,
    ))
    catalogs: dict[datetime, OpenMeteoSpatialCatalog] = {}

    def load_reference_time(reference_time: datetime) -> OpenMeteoSpatialCatalog:
        if reference_time == latest_catalog.reference_time_utc:
            catalog = latest_catalog
        else:
            payload = fetch(
                openmeteo_spatial_run_meta_url(
                    bucket_url,
                    latest_catalog.model,
                    reference_time_utc=reference_time,
                )
            )
            catalog = parse_openmeteo_spatial_latest(latest_catalog.model, payload)
        if not catalog.completed:
            raise ValueError(
                f"Open-Meteo spatial run is not complete: "
                f"{latest_catalog.model} {reference_time.isoformat()}"
            )
        if catalog.reference_time_utc != reference_time:
            raise ValueError(
                f"run metadata reference time mismatch for {latest_catalog.model}: "
                f"expected {reference_time.isoformat()}, "
                f"got {catalog.reference_time_utc.isoformat()}"
            )
        catalogs[reference_time] = catalog
        return catalog

    for reference_time in reference_times:
        load_reference_time(reference_time)

    if required_long_run_forecast_hour is not None:
        probes = 0
        cursor = min(reference_times)
        while not any(
            catalog.max_forecast_hour >= required_long_run_forecast_hour
            for catalog in catalogs.values()
        ):
            if probes >= max_additional_runs:
                raise ValueError(
                    f"could not discover a {latest_catalog.model} run reaching forecast hour "
                    f"{required_long_run_forecast_hour}"
                )
            cursor -= timedelta(hours=run_cadence_hours)
            load_reference_time(cursor)
            probes += 1

    return [
        om_run_from_spatial_catalog(product_name, catalogs[reference_time])
        for reference_time in sorted(catalogs)
    ]


def coverage_object_records(
    plan: CoveragePlan,
    runs: list[OmRun],
    *,
    bucket_url: str = DEFAULT_OPENMETEO_BUCKET_URL,
    openmeteo_model: str,
) -> list[dict[str, Any]]:
    runs_by_id = {run.run_id: run for run in runs}
    records = []
    for slot in plan.slots:
        if slot.source_run not in runs_by_id:
            raise ValueError(f"coverage slot references unknown source run: {slot.source_run}")
        source_run = runs_by_id[slot.source_run]
        records.append(
            {
                "valid_time_utc": _format_utc(slot.valid_time_utc),
                "source_run": slot.source_run,
                "forecast_hour": slot.forecast_hour,
                "url": openmeteo_spatial_object_url(
                    bucket_url,
                    openmeteo_model,
                    reference_time_utc=source_run.base_time_utc,
                    valid_time_utc=slot.valid_time_utc,
                ),
            }
        )
    return records
