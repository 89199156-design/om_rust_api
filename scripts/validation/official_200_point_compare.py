#!/usr/bin/env python3
"""Snapshot once, then compare 300 points against the official APIs.

The official response for each model is captured by bounded multi-location POSTs.
Validation then requests the local API one point at a time and stops at the
first difference.  Successful point receipts are immutable and resumable, so
diagnosis and fixes never consume the official API quota again.

Only the requested official/local surface-field intersection is compared. GFS
and ECMWF surface hourly and daily fields are compared directly. Pressure-level
fields are excluded because this validation targets the public point forecast
contract rather than each server's pressure-level inventory. Open-Meteo does
not expose CAMS daily fields or Chinese AQI fields, so local-only derived
outputs are intentionally outside this official parity run.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import datetime as dt
import gzip
import hashlib
import json
import math
import os
from pathlib import Path
import random
import shlex
import subprocess
import sys
import time
from typing import Any
import urllib.error
import urllib.parse
import urllib.request

VALIDATION_ROOT = Path(__file__).resolve().parent
if str(VALIDATION_ROOT) not in sys.path:
    sys.path.insert(0, str(VALIDATION_ROOT))

try:
    from .ecmwf_variable_catalog import DAILY_VARIABLES as ECMWF_DAILY
    from .ecmwf_variable_catalog import HOURLY_VARIABLES as ECMWF_HOURLY
except ImportError:
    from ecmwf_variable_catalog import DAILY_VARIABLES as ECMWF_DAILY
    from ecmwf_variable_catalog import HOURLY_VARIABLES as ECMWF_HOURLY


SCHEMA_VERSION = 2
POINT_COUNT = 300
OFFICIAL_BATCH_SIZE = 100
USER_AGENT = "om-weather-server-official-300-point-validation/1.0"
# A single-point response with every public surface field is small, while each
# HTTP round trip makes the API repeat model lookup and time-axis work.  Keep
# hourly and daily requests separate, but normally fit each period into one
# request.  The resource guard before every request remains the throttle.
DEFAULT_FIELD_CHUNK_SIZE = 96
DEFAULT_REQUEST_DELAY_SECONDS = 0.0
DEFAULT_POINT_DELAY_SECONDS = 0.0
DEFAULT_MIN_AVAILABLE_MEMORY_MIB = 768.0
DEFAULT_MAX_IO_FULL_PRESSURE_AVG10 = 10.0
DEFAULT_RESOURCE_WAIT_TIMEOUT_SECONDS = 900.0
DEFAULT_RESOURCE_POLL_SECONDS = 5.0
DEFAULT_MAX_LOCAL_OM_API_PROCESSES = 1
GFS_SURFACE = (
    "temperature_2m",
    "apparent_temperature",
    "relative_humidity_2m",
    "dew_point_2m",
    "wet_bulb_temperature_2m",
    "surface_temperature",
    "soil_temperature_0_to_10cm",
    "soil_temperature_0_to_7cm",
    "soil_temperature_7_to_28cm",
    "soil_temperature_28_to_100cm",
    "soil_temperature_100_to_255cm",
    "soil_temperature_10_to_40cm",
    "soil_temperature_40_to_100cm",
    "soil_temperature_100_to_200cm",
    "soil_moisture_0_to_7cm",
    "soil_moisture_7_to_28cm",
    "soil_moisture_28_to_100cm",
    "soil_moisture_100_to_255cm",
    "soil_moisture_0_to_10cm",
    "soil_moisture_10_to_40cm",
    "soil_moisture_40_to_100cm",
    "soil_moisture_100_to_200cm",
    "pressure_msl",
    "surface_pressure",
    "visibility",
    "weather_code",
    "is_day",
    "precipitation",
    "precipitation_probability",
    "rain",
    "showers",
    "snowfall",
    "snowfall_water_equivalent",
    "snow_depth",
    "cloud_cover",
    "cloud_cover_low",
    "cloud_cover_mid",
    "cloud_cover_high",
    "freezing_level_height",
    "temperature_80m",
    "temperature_100m",
    "temperature_120m",
    "wind_speed_10m",
    "wind_direction_10m",
    "wind_gusts_10m",
    "wind_speed_80m",
    "wind_direction_80m",
    "wind_speed_100m",
    "wind_direction_100m",
    "wind_speed_120m",
    "wind_direction_120m",
    "cape",
    "uv_index",
    "uv_index_clear_sky",
    "sunshine_duration",
    "et0_fao_evapotranspiration",
    "vapour_pressure_deficit",
)
GFS_HOURLY = GFS_SURFACE
ECMWF_SURFACE_HOURLY = tuple(variable for variable in ECMWF_HOURLY if "hPa" not in variable)
GFS_DAILY = (
    "temperature_2m_max",
    "temperature_2m_min",
    "temperature_2m_mean",
    "apparent_temperature_max",
    "apparent_temperature_min",
    "apparent_temperature_mean",
    "precipitation_sum",
    "precipitation_probability_max",
    "precipitation_probability_min",
    "precipitation_probability_mean",
    "rain_sum",
    "showers_sum",
    "snowfall_sum",
    "snowfall_water_equivalent_sum",
    "weather_code",
    "wind_speed_10m_max",
    "wind_speed_10m_min",
    "wind_speed_10m_mean",
    "wind_gusts_10m_max",
    "wind_gusts_10m_min",
    "wind_gusts_10m_mean",
    "wind_direction_10m_dominant",
    "precipitation_hours",
    "visibility_max",
    "visibility_min",
    "visibility_mean",
    "pressure_msl_max",
    "pressure_msl_min",
    "pressure_msl_mean",
    "surface_pressure_max",
    "surface_pressure_min",
    "surface_pressure_mean",
    "cloud_cover_max",
    "cloud_cover_min",
    "cloud_cover_mean",
    "dew_point_2m_max",
    "dew_point_2m_min",
    "dew_point_2m_mean",
    "relative_humidity_2m_max",
    "relative_humidity_2m_min",
    "relative_humidity_2m_mean",
    "snow_depth_max",
    "snow_depth_min",
    "snow_depth_mean",
    "uv_index_max",
    "uv_index_clear_sky_max",
)
CAMS_RAW = (
    "aerosol_optical_depth",
    "pm2_5",
    "pm10",
    "dust",
    "carbon_monoxide",
    "nitrogen_dioxide",
    "ozone",
    "sulphur_dioxide",
)
CAMS_OFFICIAL_DERIVED = (
    "european_aqi",
    "european_aqi_pm2_5",
    "european_aqi_pm10",
    "european_aqi_no2",
    "european_aqi_o3",
    "european_aqi_so2",
    "us_aqi",
    "us_aqi_pm2_5",
    "us_aqi_pm10",
    "us_aqi_no2",
    "us_aqi_o3",
    "us_aqi_so2",
    "us_aqi_co",
)
CAMS_HOURLY_OFFICIAL = CAMS_RAW + CAMS_OFFICIAL_DERIVED
# The local CAMS endpoint intentionally publishes the raw concentration fields
# plus its own Chinese AQI derivatives. European/US AQI fields exist only in
# the official snapshot, while Chinese AQI exists only locally, so neither
# derived family belongs to the strict official/local field intersection.
CAMS_HOURLY_LOCAL = CAMS_RAW
CAMS_DAILY: tuple[str, ...] = ()

MODEL_SPECS: dict[str, dict[str, Any]] = {
    "gfs": {
        "public_endpoint": "https://api.open-meteo.com/v1/gfs",
        "customer_endpoint": "https://customer-api.open-meteo.com/v1/gfs",
        "local_path": "/v1/gfs",
        "model_parameter": ("models", ["gfs_global"]),
        "forecast_days": 16,
        "official_hourly": GFS_HOURLY,
        "local_hourly": GFS_HOURLY,
        "daily": GFS_DAILY,
    },
    "ec": {
        "public_endpoint": "https://api.open-meteo.com/v1/ecmwf",
        "customer_endpoint": "https://customer-api.open-meteo.com/v1/ecmwf",
        "local_path": "/v1/ecmwf",
        "model_parameter": ("models", ["ecmwf_ifs025"]),
        "forecast_days": 15,
        "official_hourly": ECMWF_SURFACE_HOURLY,
        "local_hourly": ECMWF_SURFACE_HOURLY,
        "daily": tuple(ECMWF_DAILY),
    },
    "cams": {
        "public_endpoint": "https://air-quality-api.open-meteo.com/v1/air-quality",
        "customer_endpoint": (
            "https://customer-air-quality-api.open-meteo.com/v1/air-quality"
        ),
        "local_path": "/v1/cams",
        "model_parameter": ("domains", "cams_global"),
        "forecast_days": 5,
        "official_hourly": CAMS_HOURLY_OFFICIAL,
        "local_hourly": CAMS_HOURLY_LOCAL,
        "daily": CAMS_DAILY,
    },
}

AQI_BP = (0.0, 50.0, 100.0, 150.0, 200.0, 300.0, 400.0, 500.0)
CHINESE_BREAKPOINTS = {
    "pm2_5": ((0.0, 30.0, 60.0, 115.0, 150.0, 250.0, 350.0, 500.0), AQI_BP, 500.0, 0),
    "pm10": ((0.0, 50.0, 120.0, 250.0, 350.0, 420.0, 500.0, 600.0), AQI_BP, 500.0, 0),
    "nitrogen_dioxide": (
        (0.0, 100.0, 200.0, 700.0, 1200.0, 2340.0, 3090.0, 3840.0),
        AQI_BP,
        500.0,
        0,
    ),
    "ozone": (
        (0.0, 160.0, 200.0, 300.0, 400.0, 800.0, 1000.0, 1200.0),
        AQI_BP,
        500.0,
        0,
    ),
    "sulphur_dioxide": ((0.0, 150.0, 500.0, 650.0, 800.0), AQI_BP[:5], 200.0, 0),
    "carbon_monoxide": (
        (0.0, 5.0, 10.0, 35.0, 60.0, 90.0, 120.0, 150.0),
        AQI_BP,
        500.0,
        1,
    ),
}
CHINESE_DAILY_BREAKPOINTS = {
    "pm2_5": CHINESE_BREAKPOINTS["pm2_5"],
    "pm10": CHINESE_BREAKPOINTS["pm10"],
    "nitrogen_dioxide": (
        (0.0, 40.0, 80.0, 180.0, 280.0, 565.0, 750.0, 940.0),
        AQI_BP,
        500.0,
        0,
    ),
    "ozone": ((0.0, 100.0, 160.0, 215.0, 265.0, 800.0), AQI_BP[:6], 300.0, 0),
    "sulphur_dioxide": (
        (0.0, 50.0, 150.0, 475.0, 800.0, 1600.0, 2100.0, 2620.0),
        AQI_BP,
        500.0,
        0,
    ),
    "carbon_monoxide": (
        (0.0, 2.0, 4.0, 14.0, 24.0, 36.0, 48.0, 60.0),
        AQI_BP,
        500.0,
        1,
    ),
}


class ValidationError(RuntimeError):
    pass


def chunks(values: tuple[str, ...], size: int) -> list[tuple[str, ...]]:
    if size <= 0:
        raise ValidationError("field chunk size must be positive")
    return [values[index : index + size] for index in range(0, len(values), size)]


def request_plan(model: str, field_chunk_size: int) -> list[dict[str, Any]]:
    spec = MODEL_SPECS[model]
    hourly_groups = chunks(tuple(spec["local_hourly"]), field_chunk_size)
    daily_groups = chunks(
        tuple(spec["daily"]) if model != "cams" else (), field_chunk_size
    )
    plan: list[dict[str, Any]] = []
    # The local API supports hourly and daily fields in one request. Pair their
    # chunks so snapshot setup, file descriptors and decoder caches are reused.
    # Comparison order remains hourly then daily, preserving first-difference
    # determinism.
    for index in range(max(len(hourly_groups), len(daily_groups))):
        plan.append(
            {
                "hourly": (
                    hourly_groups[index] if index < len(hourly_groups) else ()
                ),
                "daily": daily_groups[index] if index < len(daily_groups) else (),
            }
        )
    return plan


def attempt_id_now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")


def update_progress_estimate(
    report: dict[str, Any],
    *,
    started_monotonic: float,
    request_units_completed: int,
    request_units_total: int,
    timed_request_units_completed: int,
) -> None:
    elapsed = max(0.0, time.monotonic() - started_monotonic)
    report["elapsed_seconds"] = round(elapsed, 3)
    report["request_units_completed"] = request_units_completed
    report["request_units_total"] = request_units_total
    report["request_units_reused"] = (
        request_units_completed - timed_request_units_completed
    )
    report["request_units_executed_this_attempt"] = timed_request_units_completed
    if timed_request_units_completed <= 0:
        report["estimated_remaining_seconds"] = None
        report["estimated_finish_at"] = None
        return
    remaining_units = max(0, request_units_total - request_units_completed)
    remaining_seconds = elapsed * remaining_units / timed_request_units_completed
    report["estimated_remaining_seconds"] = round(remaining_seconds, 3)
    report["estimated_finish_at"] = (
        dt.datetime.now(dt.timezone.utc) + dt.timedelta(seconds=remaining_seconds)
    ).isoformat()


def linux_available_memory_mib() -> float | None:
    try:
        for line in Path("/proc/meminfo").read_text(encoding="ascii").splitlines():
            if line.startswith("MemAvailable:"):
                return float(line.split()[1]) / 1024.0
    except (OSError, ValueError, IndexError):
        return None
    return None


def linux_io_full_pressure_avg10() -> float | None:
    try:
        for line in Path("/proc/pressure/io").read_text(encoding="ascii").splitlines():
            if not line.startswith("full "):
                continue
            for item in line.split()[1:]:
                key, value = item.split("=", 1)
                if key == "avg10":
                    return float(value)
    except (OSError, ValueError):
        return None
    return None


def local_om_api_process_count() -> int | None:
    proc = Path("/proc")
    if not proc.is_dir():
        return None
    count = 0
    try:
        entries = tuple(proc.iterdir())
    except OSError:
        return None
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            if entry.joinpath("comm").read_text(encoding="ascii").strip() != "om-api":
                continue
            status = entry.joinpath("status").read_text(encoding="ascii")
        except OSError:
            continue
        if "\nState:\tZ" not in f"\n{status}":
            count += 1
    return count


def is_loopback_url(url: str) -> bool:
    hostname = urllib.parse.urlsplit(url).hostname
    return hostname in {"127.0.0.1", "::1", "localhost"}


def wait_for_safe_local_resources(
    *,
    local_base: str,
    min_available_memory_mib: float,
    max_io_full_pressure_avg10: float,
    max_local_om_api_processes: int,
    wait_timeout_seconds: float,
    poll_seconds: float,
) -> dict[str, float | int | None]:
    deadline = time.monotonic() + wait_timeout_seconds
    while True:
        available_memory_mib = linux_available_memory_mib()
        io_full_pressure_avg10 = linux_io_full_pressure_avg10()
        om_api_processes = (
            local_om_api_process_count() if is_loopback_url(local_base) else None
        )
        if (
            om_api_processes is not None
            and om_api_processes > max_local_om_api_processes
        ):
            raise ValidationError(
                "refusing local validation with "
                f"{om_api_processes} om-api processes; maximum is "
                f"{max_local_om_api_processes}"
            )
        memory_safe = (
            available_memory_mib is None
            or available_memory_mib >= min_available_memory_mib
        )
        io_safe = (
            io_full_pressure_avg10 is None
            or io_full_pressure_avg10 <= max_io_full_pressure_avg10
        )
        snapshot: dict[str, float | int | None] = {
            "available_memory_mib": (
                None
                if available_memory_mib is None
                else round(available_memory_mib, 3)
            ),
            "io_full_pressure_avg10": io_full_pressure_avg10,
            "local_om_api_processes": om_api_processes,
        }
        if memory_safe and io_safe:
            return snapshot
        if time.monotonic() >= deadline:
            raise ValidationError(
                "local validation resource guard timed out: "
                f"{json.dumps(snapshot, ensure_ascii=False)}"
            )
        print(
            json.dumps({"event": "resource_wait", **snapshot}, ensure_ascii=False),
            flush=True,
        )
        time.sleep(poll_seconds)


@contextlib.contextmanager
def validation_lock(output: Path):
    lock_path = output / ".official-300-validation.lock"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    handle = lock_path.open("a+", encoding="utf-8")
    fcntl_module = None
    try:
        try:
            import fcntl as fcntl_module

            fcntl_module.flock(
                handle.fileno(), fcntl_module.LOCK_EX | fcntl_module.LOCK_NB
            )
        except ImportError:
            pass
        except BlockingIOError as exc:
            raise ValidationError(
                f"another validator holds {lock_path}; concurrent validation is forbidden"
            ) from exc
        handle.seek(0)
        handle.truncate()
        handle.write(f"{os.getpid()}\n")
        handle.flush()
        yield
    finally:
        try:
            if fcntl_module is not None:
                fcntl_module.flock(handle.fileno(), fcntl_module.LOCK_UN)
        finally:
            handle.close()


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(
        value, ensure_ascii=True, sort_keys=True, separators=(",", ":"), allow_nan=False
    ).encode("utf-8")


def pretty_bytes(value: Any) -> bytes:
    return (
        json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2, allow_nan=False)
        + "\n"
    ).encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path, chunk_size: int = 1024 * 1024) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(chunk_size):
            digest.update(chunk)
    return digest.hexdigest()


def iter_json_array_file(
    path: Path,
    expected: int,
    chunk_size: int = 1024 * 1024,
):
    """Yield a top-level JSON array one item at a time with bounded memory."""
    decoder = json.JSONDecoder()
    buffer = ""
    position = 0
    count = 0
    state = "start"

    with path.open("r", encoding="utf-8") as handle:
        eof = False
        while not eof:
            chunk = handle.read(chunk_size)
            eof = chunk == ""
            buffer = buffer[position:] + chunk
            position = 0

            while True:
                while position < len(buffer) and buffer[position].isspace():
                    position += 1
                if state == "start":
                    if position >= len(buffer):
                        break
                    if buffer[position] != "[":
                        raise ValidationError(
                            f"official snapshot must be a top-level array: {path}"
                        )
                    state = "first_or_end"
                    position += 1
                    continue

                if position >= len(buffer):
                    break
                if state == "first_or_end" and buffer[position] == "]":
                    state = "done"
                    position += 1
                    break
                if state == "separator_or_end":
                    if buffer[position] == "]":
                        state = "done"
                        position += 1
                        break
                    if buffer[position] != ",":
                        raise ValidationError(
                            f"invalid official snapshot array separator: {path}"
                        )
                    state = "value"
                    position += 1
                    continue
                if state not in {"first_or_end", "value"}:
                    raise ValidationError(f"invalid official snapshot state: {path}")
                try:
                    value, end = decoder.raw_decode(buffer, position)
                except json.JSONDecodeError:
                    if eof:
                        raise ValidationError(
                            f"invalid or truncated official snapshot: {path}"
                        )
                    break
                if not isinstance(value, dict):
                    raise ValidationError(
                        f"official snapshot row {count} is not an object: {path}"
                    )
                count += 1
                position = end
                state = "separator_or_end"
                yield value

            if state == "done":
                if buffer[position:].strip():
                    raise ValidationError(
                        f"unexpected content after official snapshot array: {path}"
                    )
                if not eof and handle.read().strip():
                    raise ValidationError(
                        f"unexpected content after official snapshot array: {path}"
                    )
                break

    if state != "done":
        raise ValidationError(f"invalid or truncated official snapshot: {path}")
    if count != expected:
        raise ValidationError(
            f"response row count/type mismatch: expected={expected}, actual={count}"
        )


def write_once(path: Path, value: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        if path.read_bytes() != value:
            raise ValidationError(f"immutable artifact differs: {path}")
        return
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_bytes(value)
    temporary.replace(path)


def write_json(path: Path, value: Any, *, immutable: bool = False) -> None:
    raw = pretty_bytes(value)
    if immutable:
        write_once(path, raw)
    else:
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
        temporary.write_bytes(raw)
        temporary.replace(path)


def validation_manifest(models: list[str]) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "type": "official_300_point_validation_manifest",
        "models": models,
        "point_count_per_model": POINT_COUNT,
        "random_seed": 20260729,
        "sampling_cohorts": {
            "random_exact_common_native_grid": 100,
            "random_offgrid_near_native_grid": 100,
            "random_offgrid_uniform_crop": 100,
        },
        "cell_selection": "nearest",
        "points": sample_points(),
        "official_capture_policy": "bounded_multi_location_posts_per_model_then_immutable_reuse",
        "first_difference_stops": True,
        "gfs_precipitation_probability_daily": [
            "precipitation_probability_max",
            "precipitation_probability_min",
            "precipitation_probability_mean",
        ],
        "ec_precipitation_probability_daily": [
            "precipitation_probability_max",
            "precipitation_probability_min",
            "precipitation_probability_mean",
        ],
        "comparison_scope": "official/local field intersection only",
        "excluded_local_only_outputs": [
            "Chinese AQI / aqi_cn",
            "CAMS daily aggregations",
            "model pressure levels or derived outputs absent from the official response",
        ],
    }


def ensure_validation_manifest(path: Path, requested_models: list[str]) -> None:
    if not path.exists():
        write_once(path, pretty_bytes(validation_manifest(requested_models)))
        return
    try:
        existing = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as exc:
        raise ValidationError(f"invalid immutable manifest: {path}") from exc
    existing_models = existing.get("models")
    if (
        not isinstance(existing_models, list)
        or not existing_models
        or any(
            not isinstance(model, str) or model not in MODEL_SPECS
            for model in existing_models
        )
    ):
        raise ValidationError(f"invalid model list in immutable manifest: {path}")
    missing = set(requested_models) - set(existing_models)
    if missing:
        raise ValidationError(
            f"requested models absent from immutable manifest: {sorted(missing)}"
        )
    write_once(path, pretty_bytes(validation_manifest(existing_models)))


def sample_points(seed: int = 20260729) -> list[dict[str, Any]]:
    randomizer = random.Random(seed)
    common_grid = [
        (float(latitude), float(longitude))
        for latitude in range(2, 58, 2)
        for longitude in range(72, 140, 2)
    ]
    randomizer.shuffle(common_grid)
    points: list[dict[str, Any]] = []
    for latitude, longitude in common_grid[:100]:
        points.append(
            {
                "id": f"p{len(points):03d}",
                "order": len(points),
                "latitude": latitude,
                "longitude": longitude,
                "kind": "random_exact_common_native_grid",
            }
        )
    for latitude, longitude in common_grid[100:200]:
        latitude += randomizer.uniform(0.031, 0.179)
        longitude += randomizer.uniform(0.031, 0.179)
        points.append(
            {
                "id": f"p{len(points):03d}",
                "order": len(points),
                "latitude": round(latitude, 4),
                "longitude": round(longitude, 4),
                "kind": "random_offgrid_near_native_grid",
            }
        )
    while len(points) < POINT_COUNT:
        latitude = randomizer.uniform(0.1, 57.9)
        longitude = randomizer.uniform(70.1, 139.9)
        index = len(points)
        points.append(
            {
                "id": f"p{index:03d}",
                "order": index,
                "latitude": round(latitude, 4),
                "longitude": round(longitude, 4),
                "kind": "random_offgrid_uniform_crop",
            }
        )
    return points


def normalize_rows(value: Any, expected: int) -> list[dict[str, Any]]:
    rows = value if isinstance(value, list) else [value]
    if len(rows) != expected or any(not isinstance(row, dict) for row in rows):
        raise ValidationError(
            f"response row count/type mismatch: expected={expected}, actual={len(rows)}"
        )
    return rows


def request_json(
    method: str,
    url: str,
    *,
    body: bytes | None,
    headers: dict[str, str],
    timeout: float,
    retries: int,
    redact: tuple[str, ...] = (),
) -> tuple[bytes, dict[str, str], float]:
    attempt = 0
    while True:
        request = urllib.request.Request(url, data=body, headers=headers, method=method)
        started = time.monotonic()
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                raw = response.read()
                elapsed = time.monotonic() - started
                response_headers = {
                    key.lower(): value
                    for key, value in response.headers.items()
                    if key.lower() in {"date", "content-type", "content-length", "x-ratelimit-remaining"}
                }
                return raw, response_headers, elapsed
        except urllib.error.HTTPError as exc:
            raw = exc.read()
            transient = exc.code in {408, 425, 429, 500, 502, 503, 504}
            if not transient or attempt >= retries:
                response_text = raw[:1000].decode("utf-8", errors="replace")
                for secret in redact:
                    if secret:
                        response_text = response_text.replace(secret, "[REDACTED]")
                raise ValidationError(
                    f"{method} {url} returned HTTP {exc.code}: "
                    f"{response_text}"
                ) from exc
        except (urllib.error.URLError, TimeoutError) as exc:
            if attempt >= retries:
                raise ValidationError(f"{method} {url} failed: {exc}") from exc
        time.sleep(min(30.0, 2.0**attempt))
        attempt += 1


def request_json_via_ssh(
    ssh_host: str,
    method: str,
    url: str,
    *,
    body: bytes | None,
    headers: dict[str, str],
    timeout: float,
    retries: int,
) -> tuple[bytes, dict[str, str], float]:
    """Send one API request through a configured production SSH host.

    Official POST capture can use independent free-tier source IPs, while
    local GET validation can reach the real loopback-only production service.
    Neither mode copies the validation program, snapshot, or credentials to
    the server.
    """
    if method not in {"GET", "POST"}:
        raise ValidationError(f"SSH API request does not support {method}")
    if (method == "POST") != (body is not None):
        raise ValidationError("SSH POST requires a body and SSH GET forbids one")
    if not ssh_host or any(character.isspace() for character in ssh_host):
        raise ValidationError(f"invalid SSH host alias: {ssh_host!r}")
    curl_headers = " ".join(
        f"-H {shlex.quote(f'{key}: {value}')}" for key, value in headers.items()
    )
    retry_delay = min(30, max(1, int(timeout // 30)))
    body_argument = "--data-binary @- " if body is not None else ""
    curl_command = (
        "curl --silent --show-error --fail-with-body "
        f"--max-time {max(1, int(timeout))} --retry {max(0, retries)} "
        f"--retry-delay {retry_delay} --retry-all-errors -X {method} "
        f"{curl_headers} {body_argument}{shlex.quote(url)}"
    )
    # Compress before crossing the SSH link. Forecast JSON compresses very
    # well, while sending it uncompressed can make a low-bandwidth server hit
    # curl's transfer timeout even after the official endpoint has answered.
    remote_command = "bash -o pipefail -c " + shlex.quote(
        f"{curl_command} | gzip -1 -c"
    )
    started = time.monotonic()
    try:
        completed = subprocess.run(
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                f"ConnectTimeout={min(30, max(5, int(timeout)))}",
                ssh_host,
                remote_command,
            ],
            input=body,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=timeout * (retries + 1) + 60,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise ValidationError(f"{method} {url} through SSH host {ssh_host} failed: {exc}") from exc
    elapsed = time.monotonic() - started
    if completed.returncode != 0:
        message = completed.stderr[:1000].decode("utf-8", errors="replace")
        raise ValidationError(
            f"{method} {url} through SSH host {ssh_host} failed with exit "
            f"{completed.returncode}: {message}"
        )
    try:
        raw = gzip.decompress(completed.stdout)
    except OSError as exc:
        raise ValidationError(
            f"{method} {url} through SSH host {ssh_host} returned invalid gzip data"
        ) from exc
    return raw, {}, elapsed


PRODUCTION_SSH_HELPER = r"""
import base64
import gzip
import hashlib
import json
import sys
import time
import urllib.error
import urllib.request

for line in sys.stdin.buffer:
    started = time.monotonic()
    try:
        request_spec = json.loads(line)
        retries = int(request_spec["retries"])
        attempt = 0
        while True:
            try:
                request = urllib.request.Request(
                    request_spec["url"],
                    headers=request_spec["headers"],
                    method="GET",
                )
                with urllib.request.urlopen(
                    request,
                    timeout=float(request_spec["timeout"]),
                ) as response:
                    raw = response.read()
                break
            except (urllib.error.URLError, TimeoutError):
                if attempt >= retries:
                    raise
                time.sleep(min(5.0, 2.0**attempt))
                attempt += 1
        projection = request_spec.get("projection")
        if projection is None:
            output = {
                "ok": True,
                "body": base64.b64encode(gzip.compress(raw, compresslevel=1)).decode("ascii"),
                "content_encoding": "gzip+base64",
                "elapsed": time.monotonic() - started,
            }
        else:
            payload = json.loads(raw)
            if isinstance(payload, list):
                if len(payload) != 1:
                    raise ValueError("projection requires exactly one response row")
                payload = payload[0]
            if not isinstance(payload, dict):
                raise ValueError("projection response row is not an object")
            projected = {}
            projection_valid = True
            value_count = 0
            for period in ("hourly", "daily"):
                variables = projection.get(period, [])
                if not variables:
                    continue
                period_payload = payload.get(period)
                if not isinstance(period_payload, dict):
                    projected[period] = {"__missing_period__": True}
                    projection_valid = False
                    continue
                projected_period = {}
                times = period_payload.get("time")
                if "time" in period_payload:
                    projected_period["time"] = times
                else:
                    projected_period["__missing_time__"] = True
                try:
                    duplicate_time = (
                        isinstance(times, list)
                        and len(set(times)) != len(times)
                    )
                except TypeError:
                    duplicate_time = True
                if not isinstance(times, list) or duplicate_time:
                    projection_valid = False
                for variable in variables:
                    if variable in period_payload:
                        values = period_payload[variable]
                        projected_period[variable] = values
                    else:
                        projected_period[f"__missing__:{variable}"] = True
                        values = None
                    if (
                        not isinstance(values, list)
                        or not isinstance(times, list)
                        or len(values) != len(times)
                    ):
                        projection_valid = False
                    else:
                        value_count += len(values)
                projected[period] = projected_period
            canonical_projection = json.dumps(
                projected,
                ensure_ascii=True,
                sort_keys=True,
                separators=(",", ":"),
                allow_nan=False,
            ).encode("utf-8")
            output = {
                "ok": True,
                "projection_sha256": hashlib.sha256(canonical_projection).hexdigest(),
                "projection_valid": projection_valid,
                "value_count": value_count,
                "source_response_bytes": len(raw),
                "content_encoding": "sha256-json-projection-v1",
                "elapsed": time.monotonic() - started,
            }
    except Exception as exc:
        output = {
            "ok": False,
            "error": f"{type(exc).__name__}: {exc}",
            "elapsed": time.monotonic() - started,
        }
    sys.stdout.write(json.dumps(output, separators=(",", ":")) + "\n")
    sys.stdout.flush()
"""


class ProductionSshApiClient:
    """Keep one SSH session open while querying a loopback production API."""

    def __init__(self, ssh_host: str, timeout: float, retries: int) -> None:
        if not ssh_host or any(character.isspace() for character in ssh_host):
            raise ValidationError(f"invalid SSH host alias: {ssh_host!r}")
        self.ssh_host = ssh_host
        self.timeout = timeout
        self.retries = retries
        self.process: subprocess.Popen[bytes] | None = None

    def __enter__(self) -> "ProductionSshApiClient":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def start(self) -> None:
        if self.process is not None:
            return
        command = "python3 -u -c " + shlex.quote(PRODUCTION_SSH_HELPER)
        self.process = subprocess.Popen(
            [
                "ssh",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                f"ConnectTimeout={min(30, max(5, int(self.timeout)))}",
                self.ssh_host,
                command,
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    def request(
        self,
        url: str,
        headers: dict[str, str],
    ) -> tuple[bytes, dict[str, str], float]:
        response, _response_bytes = self._exchange(
            {
                "url": url,
                "headers": headers,
                "timeout": self.timeout,
                "retries": self.retries,
            }
        )
        if response.get("content_encoding") != "gzip+base64":
            raise ValidationError(
                f"production SSH API transport to {self.ssh_host} returned "
                "an unsupported body encoding"
            )
        try:
            compressed = base64.b64decode(response["body"], validate=True)
            raw = gzip.decompress(compressed)
        except (KeyError, ValueError, OSError) as exc:
            raise ValidationError(
                f"production SSH API transport to {self.ssh_host} returned invalid body"
            ) from exc
        return raw, {}, float(response["elapsed"])

    def request_projection_digest(
        self,
        url: str,
        headers: dict[str, str],
        projection: dict[str, tuple[str, ...]],
    ) -> dict[str, Any]:
        response, response_bytes = self._exchange(
            {
                "url": url,
                "headers": headers,
                "timeout": self.timeout,
                "retries": self.retries,
                "projection": {
                    period: list(variables)
                    for period, variables in projection.items()
                    if variables
                },
            }
        )
        if response.get("content_encoding") != "sha256-json-projection-v1":
            raise ValidationError(
                f"production SSH API transport to {self.ssh_host} returned "
                "an unsupported projection encoding"
            )
        try:
            digest = str(response["projection_sha256"])
            if len(digest) != 64 or any(
                character not in "0123456789abcdef" for character in digest
            ):
                raise ValueError("invalid projection digest")
            projection_valid = response["projection_valid"]
            if not isinstance(projection_valid, bool):
                raise TypeError("projection validity is not boolean")
            value_count = int(response["value_count"])
            source_response_bytes = int(response["source_response_bytes"])
            if value_count < 0 or source_response_bytes < 0:
                raise ValueError("projection counters must not be negative")
            return {
                "projection_sha256": digest,
                "projection_valid": projection_valid,
                "value_count": value_count,
                "source_response_bytes": source_response_bytes,
                "transport_response_bytes": response_bytes,
                "elapsed": float(response["elapsed"]),
            }
        except (KeyError, TypeError, ValueError) as exc:
            raise ValidationError(
                f"production SSH API transport to {self.ssh_host} returned "
                "an invalid projection digest"
            ) from exc

    def _exchange(self, request: dict[str, Any]) -> tuple[dict[str, Any], int]:
        self.start()
        assert self.process is not None
        if self.process.stdin is None or self.process.stdout is None:
            raise ValidationError("production SSH API process has no pipes")
        request_spec = canonical_bytes(request)
        try:
            self.process.stdin.write(request_spec + b"\n")
            self.process.stdin.flush()
            response_line = self.process.stdout.readline()
        except (BrokenPipeError, OSError) as exc:
            raise ValidationError(
                f"production SSH API transport to {self.ssh_host} failed: {exc}"
            ) from exc
        if not response_line:
            return_code = self.process.poll()
            raise ValidationError(
                f"production SSH API transport to {self.ssh_host} ended "
                f"unexpectedly with exit {return_code}"
            )
        try:
            response = json.loads(response_line)
        except json.JSONDecodeError as exc:
            raise ValidationError(
                f"production SSH API transport to {self.ssh_host} returned invalid JSON"
            ) from exc
        if not response.get("ok"):
            raise ValidationError(
                f"GET {request.get('url')} through SSH host {self.ssh_host} failed: "
                f"{response.get('error', 'unknown remote error')}"
            )
        return response, len(response_line)

    def close(self) -> None:
        process = self.process
        self.process = None
        if process is None:
            return
        if process.stdin is not None:
            process.stdin.close()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.terminate()
            process.wait(timeout=10)


def response_time_axis_signature(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """Return and validate the common model time axes for one point batch."""
    signature: dict[str, Any] | None = None
    for row in rows:
        current: dict[str, Any] = {}
        for period in ("hourly", "daily"):
            values = row.get(period, {}).get("time", [])
            if not isinstance(values, list):
                raise ValidationError(f"official {period} time axis is invalid")
            current[period] = {
                "count": len(values),
                "first": values[0] if values else None,
                "last": values[-1] if values else None,
            }
        if signature is None:
            signature = current
        elif signature != current:
            raise ValidationError("official response contains mixed time axes")
    return signature or {
        "hourly": {"count": 0, "first": None, "last": None},
        "daily": {"count": 0, "first": None, "last": None},
    }


def official_payload(model: str, points: list[dict[str, Any]]) -> dict[str, Any]:
    spec = MODEL_SPECS[model]
    payload: dict[str, Any] = {
        "latitude": [point["latitude"] for point in points],
        "longitude": [point["longitude"] for point in points],
        "hourly": list(spec["official_hourly"]),
        "forecast_days": spec["forecast_days"],
        "timezone": ["GMT"],
        "timeformat": "iso8601",
        "cell_selection": "nearest",
    }
    if model != "cams":
        payload["daily"] = list(spec["daily"])
        payload["temperature_unit"] = "celsius"
        payload["wind_speed_unit"] = "ms"
        payload["precipitation_unit"] = "mm"
    name, value = spec["model_parameter"]
    payload[name] = value
    return payload


def capture_official(
    model: str,
    output: Path,
    api_key: str | None,
    timeout: float,
    retries: int,
    request_delay_seconds: float = 0.0,
    ssh_hosts: tuple[str, ...] = (),
) -> dict[str, Any]:
    model_dir = output / model / "official"
    response_path = model_dir / "response.json"
    metadata_path = model_dir / "metadata.json"
    request_path = model_dir / "request.json"
    if response_path.exists() and metadata_path.exists() and request_path.exists():
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
        raw = response_path.read_bytes()
        if metadata.get("response_sha256") != sha256_bytes(raw):
            raise ValidationError(f"official snapshot hash mismatch: {response_path}")
        batch_artifacts = metadata.get("batches")
        if not isinstance(batch_artifacts, list) or len(batch_artifacts) != metadata.get(
            "official_request_count"
        ):
            raise ValidationError(f"official batch metadata is invalid: {metadata_path}")
        for batch_index, artifact in enumerate(batch_artifacts):
            if not isinstance(artifact, dict):
                raise ValidationError(f"official batch metadata is invalid: {metadata_path}")
            request_batch_path = model_dir / f"request-{batch_index:03d}.json"
            response_batch_path = model_dir / f"response-{batch_index:03d}.json"
            if artifact.get("request_file") != request_batch_path.name or artifact.get(
                "response_file"
            ) != response_batch_path.name:
                raise ValidationError(f"official batch filenames are invalid: {metadata_path}")
            try:
                persisted_request = json.loads(
                    request_batch_path.read_text(encoding="utf-8")
                )
            except (OSError, json.JSONDecodeError) as exc:
                raise ValidationError(
                    f"official batch request is invalid: {request_batch_path}"
                ) from exc
            if artifact.get("request_sha256") != sha256_bytes(
                canonical_bytes(persisted_request)
            ):
                raise ValidationError(
                    f"official batch request hash mismatch: {request_batch_path}"
                )
            try:
                persisted_response = response_batch_path.read_bytes()
            except OSError as exc:
                raise ValidationError(
                    f"official batch response is unavailable: {response_batch_path}"
                ) from exc
            if artifact.get("response_sha256") != sha256_bytes(persisted_response):
                raise ValidationError(
                    f"official batch response hash mismatch: {response_batch_path}"
                )
        return metadata

    points = sample_points()
    api_key = (api_key or "").strip()
    endpoint_key = "customer_endpoint" if api_key else "public_endpoint"
    endpoint = MODEL_SPECS[model][endpoint_key]
    headers = {
        "Accept": "application/json",
        "Content-Type": "application/json",
        "User-Agent": USER_AGENT,
    }
    payloads: list[dict[str, Any]] = []
    rows: list[dict[str, Any]] = []
    batch_artifacts: list[dict[str, Any]] = []
    batch_time_axis: dict[str, Any] | None = None
    total_elapsed = 0.0
    last_network_request_at: float | None = None
    for batch_index, start in enumerate(range(0, len(points), OFFICIAL_BATCH_SIZE)):
        batch_points = points[start : start + OFFICIAL_BATCH_SIZE]
        payload = official_payload(model, batch_points)
        payloads.append(payload)
        payload_raw = canonical_bytes(payload)
        wire_payload = {**payload, **({"apikey": api_key} if api_key else {})}
        batch_request_path = model_dir / f"request-{batch_index:03d}.json"
        batch_response_path = model_dir / f"response-{batch_index:03d}.json"
        request_exists = batch_request_path.exists()
        response_exists = batch_response_path.exists()
        if request_exists != response_exists:
            raise ValidationError(
                "incomplete immutable official batch artifact pair: "
                f"{batch_request_path}, {batch_response_path}"
            )
        resumed_from_disk = request_exists
        request_exit = "persisted"
        if resumed_from_disk:
            try:
                persisted_payload = json.loads(
                    batch_request_path.read_text(encoding="utf-8")
                )
            except (OSError, json.JSONDecodeError) as exc:
                raise ValidationError(
                    f"official batch request is invalid: {batch_request_path}"
                ) from exc
            if canonical_bytes(persisted_payload) != payload_raw:
                raise ValidationError(
                    f"immutable official batch request differs: {batch_request_path}"
                )
            raw = batch_response_path.read_bytes()
            response_headers: dict[str, str] = {}
            elapsed = 0.0
        else:
            if last_network_request_at is not None and request_delay_seconds > 0:
                remaining_delay = request_delay_seconds - (
                    time.monotonic() - last_network_request_at
                )
                if remaining_delay > 0:
                    time.sleep(remaining_delay)
            ssh_host = ssh_hosts[batch_index % len(ssh_hosts)] if ssh_hosts else None
            if ssh_host:
                if api_key:
                    raise ValidationError(
                        "SSH-routed official capture is restricted to the keyless public API"
                    )
                raw, response_headers, elapsed = request_json_via_ssh(
                    ssh_host,
                    "POST",
                    endpoint,
                    body=canonical_bytes(wire_payload),
                    headers=headers,
                    timeout=timeout,
                    retries=retries,
                )
                request_exit = f"ssh:{ssh_host}"
            else:
                raw, response_headers, elapsed = request_json(
                    "POST",
                    endpoint,
                    body=canonical_bytes(wire_payload),
                    headers=headers,
                    timeout=timeout,
                    retries=retries,
                    redact=(api_key,) if api_key else (),
                )
                request_exit = "local"
            last_network_request_at = time.monotonic()
        try:
            batch_rows = normalize_rows(json.loads(raw), len(batch_points))
        except (json.JSONDecodeError, TypeError) as exc:
            raise ValidationError(
                f"official {model} batch {batch_index} response is not valid JSON"
            ) from exc
        current_time_axis = response_time_axis_signature(batch_rows)
        if batch_time_axis is None:
            batch_time_axis = current_time_axis
        elif batch_time_axis != current_time_axis:
            raise ValidationError(
                f"official {model} batches do not belong to the same time axis"
            )
        write_once(batch_request_path, pretty_bytes(payload))
        write_once(batch_response_path, raw)
        rows.extend(batch_rows)
        total_elapsed += elapsed
        batch_artifacts.append(
            {
                "batch_index": batch_index,
                "point_offset": start,
                "point_count": len(batch_points),
                "request_file": batch_request_path.name,
                "request_sha256": sha256_bytes(payload_raw),
                "response_file": batch_response_path.name,
                "response_sha256": sha256_bytes(raw),
                "response_bytes": len(raw),
                "elapsed_seconds": round(elapsed, 6),
                "response_headers": response_headers,
                "request_exit": request_exit,
                "resumed_from_disk": resumed_from_disk,
                "time_axis": current_time_axis,
            }
        )
    if len(rows) != POINT_COUNT:
        raise ValidationError(
            f"official {model} merged response row count mismatch: "
            f"expected={POINT_COUNT}, actual={len(rows)}"
        )
    request_snapshot = {
        "batch_size": OFFICIAL_BATCH_SIZE,
        "batches": payloads,
    }
    request_snapshot_raw = canonical_bytes(request_snapshot)
    response_snapshot_raw = canonical_bytes(rows)
    write_once(request_path, pretty_bytes(request_snapshot))
    write_once(response_path, response_snapshot_raw)
    metadata = {
        "schema_version": SCHEMA_VERSION,
        "type": "official_multi_location_snapshot",
        "model": model,
        "captured_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "endpoint": endpoint,
        "method": "POST",
        "official_request_count": len(batch_artifacts),
        "official_batch_size": OFFICIAL_BATCH_SIZE,
        "point_count": len(rows),
        "request_sha256": sha256_bytes(request_snapshot_raw),
        "response_sha256": sha256_bytes(response_snapshot_raw),
        "response_bytes": len(response_snapshot_raw),
        "elapsed_seconds": round(total_elapsed, 6),
        "time_axis": batch_time_axis,
        "batches": batch_artifacts,
        "api_access_tier": "customer_commercial" if api_key else "public_noncommercial",
        "api_key_transport": (
            "POST JSON apikey field (excluded from request snapshot)"
            if api_key
            else "none"
        ),
        "api_key_persisted": False,
    }
    write_once(metadata_path, pretty_bytes(metadata))
    return metadata


def local_url(
    base: str,
    model: str,
    point: dict[str, Any],
    *,
    hourly: tuple[str, ...] | None = None,
    daily: tuple[str, ...] | None = None,
    hourly_time_range: tuple[str, str] | None = None,
    daily_time_range: tuple[str, str] | None = None,
) -> str:
    spec = MODEL_SPECS[model]
    if hourly is None and daily is None:
        hourly = tuple(spec["local_hourly"])
        daily = tuple(spec["daily"]) if model != "cams" else ()
    params: dict[str, Any] = {
        "latitude": f"{point['latitude']:.4f}",
        "longitude": f"{point['longitude']:.4f}",
        "timezone": "GMT",
        "timeformat": "iso8601",
        "cell_selection": "nearest",
    }
    if hourly_time_range is None and daily_time_range is None:
        params["forecast_days"] = str(spec["forecast_days"])
    else:
        if hourly:
            if hourly_time_range is None:
                raise ValidationError("hourly fields require an hourly time range")
            params["start_hour"], params["end_hour"] = hourly_time_range
        if daily:
            if daily_time_range is None:
                raise ValidationError("daily fields require a daily time range")
            params["start_date"], params["end_date"] = daily_time_range
    if hourly:
        params["hourly"] = ",".join(hourly)
    if daily:
        params["daily"] = ",".join(daily)
    if model != "cams":
        params["temperature_unit"] = "celsius"
        params["wind_speed_unit"] = "ms"
        params["precipitation_unit"] = "mm"
    return (
        base.rstrip("/")
        + spec["local_path"]
        + "?"
        + urllib.parse.urlencode(params, safe=",")
    )


def first_period_difference(
    period: str,
    variables: tuple[str, ...],
    official: dict[str, Any],
    local: dict[str, Any],
) -> tuple[dict[str, Any] | None, int, int]:
    hourly_count = 0
    daily_count = 0
    if not variables:
        return None, hourly_count, daily_count
    official_period = official.get(period)
    local_period = local.get(period)
    if not isinstance(official_period, dict) or not isinstance(local_period, dict):
        return (
            {
                "period": period,
                "reason": "missing_period",
                "official_present": isinstance(official_period, dict),
                "local_present": isinstance(local_period, dict),
            },
            hourly_count,
            daily_count,
        )
    official_times = official_period.get("time")
    local_times = local_period.get("time")
    if (
        not isinstance(official_times, list)
        or not isinstance(local_times, list)
        or len(set(official_times)) != len(official_times)
        or len(set(local_times)) != len(local_times)
    ):
        return (
            {
                "period": period,
                "variable": "time",
                "reason": "invalid_time_axis",
                "official": official_times,
                "local": local_times,
            },
            hourly_count,
            daily_count,
        )
    local_index_by_time = {
        time_value: index for index, time_value in enumerate(local_times)
    }
    if period == "daily":
        # The final official aggregate day is intentionally out of scope: its
        # source hourly window may be incomplete at snapshot time.
        candidate_indices = list(range(max(0, len(official_times) - 1)))
    else:
        candidate_indices = list(range(len(official_times)))

    comparable_indices = [
        index
        for index in candidate_indices
        if official_times[index] in local_index_by_time
    ]
    missing_indices = [
        index
        for index in candidate_indices
        if official_times[index] not in local_index_by_time
    ]
    if missing_indices and period == "hourly":
        first_missing = missing_indices[0]
        permitted_tail = (
            bool(comparable_indices)
            and missing_indices == list(range(first_missing, len(official_times)))
            and first_missing == comparable_indices[-1] + 1
        )
        if permitted_tail:
            missing_indices = []
    if missing_indices:
        index = missing_indices[0]
        time_value = official_times[index]
        return (
            {
                "period": period,
                "variable": "time",
                "reason": "missing_official_time",
                "index": index,
                "time": time_value,
                "local_start": local_times[0] if local_times else None,
                "local_end": local_times[-1] if local_times else None,
            },
            hourly_count,
            daily_count,
        )
    if period == "hourly" and not comparable_indices:
        return (
            {
                "period": period,
                "variable": "time",
                "reason": "no_common_model_time",
                "local_start": local_times[0] if local_times else None,
                "local_end": local_times[-1] if local_times else None,
            },
            hourly_count,
            daily_count,
        )
    for variable in variables:
        official_values = official_period.get(variable)
        local_values = local_period.get(variable)
        if (
            not isinstance(official_values, list)
            or not isinstance(local_values, list)
            or len(official_values) != len(official_times)
            or len(local_values) != len(local_times)
        ):
            return (
                {
                    "period": period,
                    "variable": variable,
                    "reason": "invalid_value_axis",
                    "official_values": len(official_values)
                    if isinstance(official_values, list)
                    else None,
                    "official_times": len(official_times),
                    "local_values": len(local_values)
                    if isinstance(local_values, list)
                    else None,
                    "local_times": len(local_times),
                },
                hourly_count,
                daily_count,
            )
        index = next(
            (
                official_index
                for official_index in comparable_indices
                for time_value in (official_times[official_index],)
                if official_values[official_index]
                != local_values[local_index_by_time[time_value]]
            ),
            None,
        )
        if index is not None:
            local_index = local_index_by_time[official_times[index]]
            return (
                {
                    "period": period,
                    "variable": variable,
                    "reason": "json_value",
                    "index": index,
                    "time": official_times[index],
                    "official": official_values[index],
                    "local": local_values[local_index],
                },
                hourly_count,
                daily_count,
            )
        if period == "hourly":
            hourly_count += len(comparable_indices)
        else:
            daily_count += len(comparable_indices)
    return None, hourly_count, daily_count


def first_direct_difference(
    model: str, official: dict[str, Any], local: dict[str, Any]
) -> tuple[dict[str, Any] | None, int, int]:
    spec = MODEL_SPECS[model]
    hourly_count = 0
    daily_count = 0
    for period, variables in (
        ("hourly", tuple(spec["official_hourly"])),
        ("daily", tuple(spec["daily"]) if model != "cams" else ()),
    ):
        difference, hourly_part, daily_part = first_period_difference(
            period, variables, official, local
        )
        hourly_count += hourly_part
        daily_count += daily_part
        if difference is not None:
            return difference, hourly_count, daily_count
    return None, hourly_count, daily_count


def round_ties_even(value: float, decimals: int) -> float:
    return round(value, decimals)


def iaqi(value: Any, contract: tuple[Any, Any, float, int]) -> int | None:
    if value is None:
        return None
    concentration_breakpoints, aqi_breakpoints, upper_limit, decimals = contract
    concentration = round_ties_even(max(0.0, float(value)), decimals)
    if concentration > concentration_breakpoints[-1]:
        return int(min(upper_limit, aqi_breakpoints[-1]))
    for index in range(1, len(concentration_breakpoints)):
        if concentration <= concentration_breakpoints[index]:
            low = concentration_breakpoints[index - 1]
            high = concentration_breakpoints[index]
            result = aqi_breakpoints[index - 1] + (
                (aqi_breakpoints[index] - aqi_breakpoints[index - 1])
                * (concentration - low)
                / (high - low)
            )
            return int(min(upper_limit, math.ceil(result)))
    return int(min(upper_limit, aqi_breakpoints[-1]))


def chinese_hourly_expected(official: dict[str, Any]) -> dict[str, list[Any]]:
    hourly = official["hourly"]
    result: dict[str, list[Any]] = {variable: [] for variable in CAMS_CHINESE}
    for index in range(len(hourly["time"])):
        values = {
            "pm2_5": iaqi(hourly["pm2_5"][index], CHINESE_BREAKPOINTS["pm2_5"]),
            "pm10": iaqi(hourly["pm10"][index], CHINESE_BREAKPOINTS["pm10"]),
            "nitrogen_dioxide": iaqi(
                hourly["nitrogen_dioxide"][index], CHINESE_BREAKPOINTS["nitrogen_dioxide"]
            ),
            "ozone": iaqi(hourly["ozone"][index], CHINESE_BREAKPOINTS["ozone"]),
            "sulphur_dioxide": iaqi(
                hourly["sulphur_dioxide"][index], CHINESE_BREAKPOINTS["sulphur_dioxide"]
            ),
            "carbon_monoxide": iaqi(
                (
                    None
                    if hourly["carbon_monoxide"][index] is None
                    else hourly["carbon_monoxide"][index] / 1000.0
                ),
                CHINESE_BREAKPOINTS["carbon_monoxide"],
            ),
        }
        result["chinese_aqi"].append(
            max(values.values()) if all(item is not None for item in values.values()) else None
        )
        result["chinese_aqi_pm2_5"].append(values["pm2_5"])
        result["chinese_aqi_pm10"].append(values["pm10"])
        result["chinese_aqi_no2"].append(values["nitrogen_dioxide"])
        result["chinese_aqi_o3"].append(values["ozone"])
        result["chinese_aqi_so2"].append(values["sulphur_dioxide"])
        result["chinese_aqi_co"].append(values["carbon_monoxide"])
    return result


def _mean(values: list[Any]) -> float | None:
    if len(values) != 24 or any(value is None for value in values):
        return None
    return sum(float(value) for value in values) / 24.0


def cams_daily_expected(official: dict[str, Any], dates: list[str]) -> dict[str, list[Any]]:
    hourly = official["hourly"]
    index_by_time = {value: index for index, value in enumerate(hourly["time"])}
    result: dict[str, list[Any]] = {variable: [] for variable in CAMS_DAILY}
    for date_text in dates:
        china_midnight = dt.datetime.fromisoformat(date_text).replace(tzinfo=dt.timezone.utc)
        start = china_midnight - dt.timedelta(hours=8)
        times = [
            (start + dt.timedelta(hours=hour)).strftime("%Y-%m-%dT%H:%M")
            for hour in range(25)
        ]
        indices = [index_by_time.get(value) for value in times]
        means: dict[str, float | None] = {}
        for variable in (
            "pm2_5",
            "pm10",
            "nitrogen_dioxide",
            "sulphur_dioxide",
            "carbon_monoxide",
        ):
            samples = [
                hourly[variable][index] if index is not None else None
                for index in indices[:24]
            ]
            means[variable] = _mean(samples)
        ozone_windows: list[float] = []
        for end in range(8, 25):
            window = [
                hourly["ozone"][indices[offset]]
                if indices[offset] is not None
                else None
                for offset in range(end - 7, end + 1)
            ]
            if any(value is None for value in window):
                ozone_windows = []
                break
            ozone_windows.append(sum(float(value) for value in window) / 8.0)
        means["ozone"] = max(ozone_windows) if ozone_windows else None
        aqi_values = {
            variable: iaqi(
                (
                    None
                    if means[variable] is None
                    else means[variable] / 1000.0
                    if variable == "carbon_monoxide"
                    else means[variable]
                ),
                CHINESE_DAILY_BREAKPOINTS[variable],
            )
            for variable in CHINESE_DAILY_BREAKPOINTS
        }
        result["chinese_aqi"].append(
            max(aqi_values.values())
            if all(value is not None for value in aqi_values.values())
            else None
        )
        result["chinese_aqi_pm2_5"].append(aqi_values["pm2_5"])
        result["chinese_aqi_pm10"].append(aqi_values["pm10"])
        result["chinese_aqi_no2"].append(aqi_values["nitrogen_dioxide"])
        result["chinese_aqi_o3"].append(aqi_values["ozone"])
        result["chinese_aqi_so2"].append(aqi_values["sulphur_dioxide"])
        result["chinese_aqi_co"].append(aqi_values["carbon_monoxide"])
        result["pm2_5_mean"].append(
            None if means["pm2_5"] is None else round_ties_even(means["pm2_5"], 0)
        )
        result["pm10_mean"].append(
            None if means["pm10"] is None else round_ties_even(means["pm10"], 0)
        )
        result["nitrogen_dioxide_mean"].append(
            None
            if means["nitrogen_dioxide"] is None
            else round_ties_even(means["nitrogen_dioxide"], 0)
        )
        result["ozone_maximum_8h_mean"].append(
            None if means["ozone"] is None else round_ties_even(means["ozone"], 0)
        )
        result["sulphur_dioxide_mean"].append(
            None
            if means["sulphur_dioxide"] is None
            else round_ties_even(means["sulphur_dioxide"], 0)
        )
        result["carbon_monoxide_mean"].append(
            None
            if means["carbon_monoxide"] is None
            else round_ties_even(means["carbon_monoxide"] / 1000.0, 1)
        )
    return result


def first_cams_derived_difference(
    official: dict[str, Any], local: dict[str, Any]
) -> tuple[dict[str, Any] | None, int, int]:
    hourly_expected = chinese_hourly_expected(official)
    local_hourly = local.get("hourly", {})
    compared_hourly = 0
    for variable, expected in hourly_expected.items():
        actual = local_hourly.get(variable)
        if expected != actual:
            index = next(
                (
                    offset
                    for offset, pair in enumerate(zip(expected, actual or []))
                    if pair[0] != pair[1]
                ),
                min(len(expected), len(actual or [])),
            )
            return (
                {
                    "period": "hourly",
                    "variable": variable,
                    "reason": "derived_from_official_raw",
                    "index": index,
                    "time": local_hourly.get("time", [None] * (index + 1))[index],
                    "official_derived": expected[index] if index < len(expected) else None,
                    "local": actual[index] if actual and index < len(actual) else None,
                },
                compared_hourly,
                0,
            )
        compared_hourly += len(expected)
    local_daily = local.get("daily", {})
    dates = local_daily.get("time", [])
    expected_daily = cams_daily_expected(official, dates)
    compared_daily = 0
    for variable, expected in expected_daily.items():
        actual = local_daily.get(variable)
        if expected != actual:
            index = next(
                (
                    offset
                    for offset, pair in enumerate(zip(expected, actual or []))
                    if pair[0] != pair[1]
                ),
                min(len(expected), len(actual or [])),
            )
            return (
                {
                    "period": "daily",
                    "variable": variable,
                    "reason": "derived_from_official_hourly_raw",
                    "index": index,
                    "time": dates[index] if index < len(dates) else None,
                    "official_derived": expected[index] if index < len(expected) else None,
                    "local": actual[index] if actual and index < len(actual) else None,
                    "note": "Open-Meteo has no CAMS daily/Chinese AQI response field",
                },
                compared_hourly,
                compared_daily,
            )
        compared_daily += len(expected)
    return None, compared_hourly, compared_daily


def validate_model(
    model: str,
    output: Path,
    local_base: str,
    timeout: float,
    retries: int,
    point_delay_seconds: float,
    field_chunk_size: int = DEFAULT_FIELD_CHUNK_SIZE,
    request_delay_seconds: float = DEFAULT_REQUEST_DELAY_SECONDS,
    min_available_memory_mib: float = DEFAULT_MIN_AVAILABLE_MEMORY_MIB,
    max_io_full_pressure_avg10: float = DEFAULT_MAX_IO_FULL_PRESSURE_AVG10,
    resource_wait_timeout_seconds: float = DEFAULT_RESOURCE_WAIT_TIMEOUT_SECONDS,
    resource_poll_seconds: float = DEFAULT_RESOURCE_POLL_SECONDS,
    max_local_om_api_processes: int = DEFAULT_MAX_LOCAL_OM_API_PROCESSES,
    attempt_id: str | None = None,
    point_limit: int = POINT_COUNT,
    local_ssh_client: ProductionSshApiClient | None = None,
    official_snapshot_root: Path | None = None,
) -> dict[str, Any]:
    if point_limit > POINT_COUNT:
        raise ValidationError(
            f"point limit exceeds immutable plan: {point_limit} > {POINT_COUNT}"
        )
    snapshot_root = official_snapshot_root or output
    official_path = snapshot_root / model / "official" / "response.json"
    metadata_path = snapshot_root / model / "official" / "metadata.json"
    if not official_path.exists() or not metadata_path.exists():
        raise ValidationError(f"official {model} snapshot is missing; run capture first")
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    if sha256_file(official_path) != metadata["response_sha256"]:
        raise ValidationError(f"official {model} snapshot hash mismatch")
    plan = request_plan(model, field_chunk_size)
    if not plan:
        raise ValidationError(f"{model} has no local fields to validate")
    attempt_id = attempt_id or attempt_id_now()
    report_path = output / model / "report.json"
    report = {
        "schema_version": SCHEMA_VERSION,
        "type": "official_300_point_comparison",
        "model": model,
        "status": "running",
        "official_snapshot_sha256": metadata["response_sha256"],
        "official_snapshot_root": str(snapshot_root.resolve()),
        "official_requests": metadata["official_request_count"],
        "points_total": POINT_COUNT,
        "points_target": point_limit,
        "points_completed": 0,
        "local_requests_completed": 0,
        "local_requests_per_point": len(plan),
        "hourly_values_compared": 0,
        "daily_values_compared": 0,
        "hourly_values_exempted_after_raw_model_end": 0,
        "daily_values_exempted_on_final_day": 0,
        "comparison": "strict_common_raw_model_axis_official_json_values",
        "time_scope_policy": {
            "hourly": "ignore_only_consecutive_official_tail_after_local_raw_model_end",
            "daily": "exclude_final_official_day",
        },
        "local_request_mode": "paired_hourly_daily_chunks",
        "local_request_transport": (
            f"production_ssh:{local_ssh_client.ssh_host}"
            if local_ssh_client
            else "direct_http"
        ),
        "concurrency": 1,
        "first_difference_order": "point_then_period_then_field_then_time",
        "field_chunk_size": field_chunk_size,
        "failure": None,
        "current_point": None,
        "current_request": None,
        "current_point_hourly_values_compared": 0,
        "current_point_daily_values_compared": 0,
        "current_point_hourly_values_exempted_after_raw_model_end": 0,
        "current_point_daily_values_exempted_on_final_day": 0,
        "attempt_id": attempt_id,
        "point_delay_seconds": point_delay_seconds,
        "request_delay_seconds": request_delay_seconds,
        "resource_guard": {
            "min_available_memory_mib": min_available_memory_mib,
            "max_io_full_pressure_avg10": max_io_full_pressure_avg10,
            "resource_wait_timeout_seconds": resource_wait_timeout_seconds,
            "resource_poll_seconds": resource_poll_seconds,
            "max_local_om_api_processes": max_local_om_api_processes,
        },
        "started_at": dt.datetime.now(dt.timezone.utc).isoformat(),
    }
    started_monotonic = time.monotonic()
    request_units_total = point_limit * len(plan)
    request_units_completed = 0
    timed_request_units_completed = 0
    update_progress_estimate(
        report,
        started_monotonic=started_monotonic,
        request_units_completed=request_units_completed,
        request_units_total=request_units_total,
        timed_request_units_completed=timed_request_units_completed,
    )
    points = sample_points()
    # Every repair attempt must prove the complete prefix again from point 0.
    # Keep prior receipts immutable for audit, but never use them to skip a
    # point in a later attempt.
    receipts = output / model / "receipts" / "attempts" / attempt_id
    receipts.mkdir(parents=True, exist_ok=True)
    official_rows = iter_json_array_file(official_path, POINT_COUNT)
    for point in points[:point_limit]:
        try:
            official = next(official_rows)
        except StopIteration as exc:
            raise ValidationError(
                f"official {model} snapshot ended before {point['id']}"
            ) from exc
        receipt_path = receipts / f"{point['order']:03d}_{point['id']}.json"
        if receipt_path.exists():
            receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
            report["points_completed"] += 1
            report["local_requests_completed"] += receipt.get(
                "local_request_count", len(plan)
            )
            report["hourly_values_compared"] += receipt["hourly_values_compared"]
            report["daily_values_compared"] += receipt["daily_values_compared"]
            report["hourly_values_exempted_after_raw_model_end"] += receipt.get(
                "hourly_values_exempted_after_raw_model_end", 0
            )
            report["daily_values_exempted_on_final_day"] += receipt.get(
                "daily_values_exempted_on_final_day", 0
            )
            request_units_completed += len(plan)
            update_progress_estimate(
                report,
                started_monotonic=started_monotonic,
                request_units_completed=request_units_completed,
                request_units_total=request_units_total,
                timed_request_units_completed=timed_request_units_completed,
            )
            continue
        report["current_point"] = point
        report["current_point_hourly_values_compared"] = 0
        report["current_point_daily_values_compared"] = 0
        report["current_point_hourly_values_exempted_after_raw_model_end"] = 0
        report["current_point_daily_values_exempted_on_final_day"] = 0
        write_json(report_path, report)
        print(
            json.dumps(
                {
                    "model": model,
                    "event": "point_started",
                    "point": point,
                    "points_completed": report["points_completed"],
                    "points_total": POINT_COUNT,
                },
                ensure_ascii=False,
            ),
            flush=True,
        )
        hourly_count = 0
        daily_count = 0
        hourly_exempted = 0
        daily_exempted = 0
        local_elapsed_seconds = 0.0
        response_parts: list[dict[str, Any]] = []
        for request_index, request_part in enumerate(plan):
            hourly_variables = request_part["hourly"]
            daily_variables = request_part["daily"]
            periods = [
                (period, variables)
                for period, variables in (
                    ("hourly", hourly_variables),
                    ("daily", daily_variables),
                )
                if variables
            ]
            time_ranges: dict[str, tuple[str, str]] = {}
            for period, _variables in periods:
                official_period = official.get(period)
                official_times = (
                    official_period.get("time")
                    if isinstance(official_period, dict)
                    else None
                )
                if not isinstance(official_times, list) or not official_times:
                    raise ValidationError(
                        f"official {model} point {point['id']} has no {period} time axis"
                    )
                time_ranges[period] = (
                    str(official_times[0]),
                    str(official_times[-1]),
                )
            if local_ssh_client:
                resource_snapshot = {
                    "transport": "production_ssh",
                    "ssh_host": local_ssh_client.ssh_host,
                    "remote_loopback": local_base,
                }
            else:
                resource_snapshot = wait_for_safe_local_resources(
                    local_base=local_base,
                    min_available_memory_mib=min_available_memory_mib,
                    max_io_full_pressure_avg10=max_io_full_pressure_avg10,
                    max_local_om_api_processes=max_local_om_api_processes,
                    wait_timeout_seconds=resource_wait_timeout_seconds,
                    poll_seconds=resource_poll_seconds,
                )
            url = local_url(
                local_base,
                model,
                point,
                hourly=hourly_variables,
                daily=daily_variables,
                hourly_time_range=time_ranges.get("hourly"),
                daily_time_range=time_ranges.get("daily"),
            )
            report["current_request"] = {
                "index": request_index,
                "count": len(plan),
                "periods": [period for period, _variables in periods],
                "variables": {
                    period: list(variables) for period, variables in periods
                },
                "time_ranges": {
                    period: list(time_range)
                    for period, time_range in time_ranges.items()
                },
                "local_url": url,
                "resources": resource_snapshot,
            }
            write_json(report_path, report)
            print(
                json.dumps(
                    {
                        "model": model,
                        "event": "request_started",
                        "point": point,
                        "index": request_index,
                        "count": len(plan),
                        "periods": [period for period, _variables in periods],
                        "variable_counts": {
                            period: len(variables)
                            for period, variables in periods
                        },
                        "time_ranges": report["current_request"]["time_ranges"],
                        "resources": resource_snapshot,
                    },
                    ensure_ascii=False,
                ),
                flush=True,
            )
            request_headers = {"Accept": "application/json", "User-Agent": USER_AGENT}
            if local_ssh_client:
                raw, headers, elapsed = local_ssh_client.request(url, request_headers)
            else:
                raw, headers, elapsed = request_json(
                    "GET",
                    url,
                    body=None,
                    headers=request_headers,
                    timeout=timeout,
                    retries=retries,
                )
            local_path = (
                output
                / model
                / "local"
                / "attempts"
                / attempt_id
                / f"{point['order']:03d}_{point['id']}"
                / (
                    f"{request_index:03d}_"
                    + "_".join(period for period, _variables in periods)
                    + ".json"
                )
            )
            write_once(local_path, raw)
            local = normalize_rows(json.loads(raw), 1)[0]
            difference = None
            part_comparison: dict[str, Any] = {}
            for period, variables in periods:
                difference, hourly_part, daily_part = first_period_difference(
                    period, variables, official, local
                )
                hourly_count += hourly_part
                daily_count += daily_part
                if difference is None:
                    official_period = official.get(period, {})
                    official_times = official_period.get("time", [])
                    compared = hourly_part if period == "hourly" else daily_part
                    exempted = len(variables) * len(official_times) - compared
                    part_comparison[period] = {
                        "values_compared": compared,
                        "values_exempted": exempted,
                    }
                    if period == "hourly":
                        hourly_exempted += exempted
                    else:
                        daily_exempted += exempted
                if difference is not None:
                    break
            report["current_point_hourly_values_compared"] = hourly_count
            report["current_point_daily_values_compared"] = daily_count
            report[
                "current_point_hourly_values_exempted_after_raw_model_end"
            ] = hourly_exempted
            report["current_point_daily_values_exempted_on_final_day"] = (
                daily_exempted
            )
            local_elapsed_seconds += elapsed
            part_metadata = {
                "index": request_index,
                "periods": [period for period, _variables in periods],
                "variables": {
                    period: list(variables) for period, variables in periods
                },
                "local_response_file": str(local_path),
                "local_response_sha256": sha256_bytes(raw),
                "local_elapsed_seconds": round(elapsed, 6),
                "local_response_headers": headers,
                "resources_before_request": resource_snapshot,
                "comparison": part_comparison,
            }
            response_parts.append(part_metadata)
            report["local_requests_completed"] += 1
            request_units_completed += 1
            timed_request_units_completed += 1
            update_progress_estimate(
                report,
                started_monotonic=started_monotonic,
                request_units_completed=request_units_completed,
                request_units_total=request_units_total,
                timed_request_units_completed=timed_request_units_completed,
            )
            if difference is not None:
                failure = {
                    "point": point,
                    "request": part_metadata,
                    "difference": difference,
                }
                report["status"] = "failed"
                report["failure"] = failure
                write_json(report_path, report)
                raise ValidationError(
                    f"{model} stopped at {point['id']}: "
                    f"{json.dumps(difference, ensure_ascii=False)}"
                )
            report["current_request"] = None
            write_json(report_path, report)
            print(
                json.dumps(
                    {
                        "model": model,
                        "event": "request_passed",
                        "point": point,
                        "request_index": request_index,
                        "request_count": len(plan),
                        "periods": [period for period, _variables in periods],
                        "variable_counts": {
                            period: len(variables)
                            for period, variables in periods
                        },
                        "local_elapsed_seconds": round(elapsed, 6),
                        "elapsed_seconds": report["elapsed_seconds"],
                        "estimated_remaining_seconds": report[
                            "estimated_remaining_seconds"
                        ],
                        "estimated_finish_at": report["estimated_finish_at"],
                    },
                    ensure_ascii=False,
                ),
                flush=True,
            )
            if request_delay_seconds > 0 and request_index + 1 < len(plan):
                time.sleep(request_delay_seconds)
        receipt = {
            "schema_version": SCHEMA_VERSION,
            "model": model,
            "point": point,
            "official_row_sha256": sha256_bytes(canonical_bytes(official)),
            "local_response_sha256": sha256_bytes(
                canonical_bytes(
                    [part["local_response_sha256"] for part in response_parts]
                )
            ),
            "local_response_parts": response_parts,
            "local_request_count": len(response_parts),
            "local_elapsed_seconds": round(local_elapsed_seconds, 6),
            "hourly_values_compared": hourly_count,
            "daily_values_compared": daily_count,
            "hourly_values_exempted_after_raw_model_end": hourly_exempted,
            "daily_values_exempted_on_final_day": daily_exempted,
            "status": "passed",
        }
        write_once(receipt_path, pretty_bytes(receipt))
        report["points_completed"] += 1
        report["hourly_values_compared"] += hourly_count
        report["daily_values_compared"] += daily_count
        report["hourly_values_exempted_after_raw_model_end"] += hourly_exempted
        report["daily_values_exempted_on_final_day"] += daily_exempted
        report["current_point"] = None
        report["current_request"] = None
        report["current_point_hourly_values_compared"] = 0
        report["current_point_daily_values_compared"] = 0
        report["current_point_hourly_values_exempted_after_raw_model_end"] = 0
        report["current_point_daily_values_exempted_on_final_day"] = 0
        write_json(report_path, report)
        print(
            json.dumps(
                {
                    "model": model,
                    "event": "point_passed",
                    "point": point,
                    "points_completed": report["points_completed"],
                    "points_total": POINT_COUNT,
                    "local_request_count": len(response_parts),
                    "local_elapsed_seconds": round(local_elapsed_seconds, 6),
                    "elapsed_seconds": report["elapsed_seconds"],
                    "estimated_remaining_seconds": report[
                        "estimated_remaining_seconds"
                    ],
                    "estimated_finish_at": report["estimated_finish_at"],
                },
                ensure_ascii=False,
            ),
            flush=True,
        )
        if point_delay_seconds > 0 and report["points_completed"] < point_limit:
            time.sleep(point_delay_seconds)
    if point_limit == POINT_COUNT:
        sentinel = object()
        if next(official_rows, sentinel) is not sentinel:
            raise ValidationError(
                f"response row count/type mismatch: expected={POINT_COUNT}, actual>={POINT_COUNT + 1}"
            )
    report["status"] = "passed" if point_limit == POINT_COUNT else "partial"
    report["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
    write_json(report_path, report)
    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("capture", "validate", "run"))
    parser.add_argument("--models", default="gfs,ec,cams")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--official-snapshot-root",
        type=Path,
        help=(
            "existing immutable official capture root used by validate; reports, "
            "receipts and local responses are still written under --output"
        ),
    )
    parser.add_argument("--local-base", default="http://127.0.0.1:8088")
    parser.add_argument(
        "--local-ssh-host",
        default="",
        help=(
            "configured SSH alias that executes local API GETs against "
            "--local-base on the real production server"
        ),
    )
    parser.add_argument("--api-key-env", default="OPEN_METEO_API_KEY")
    parser.add_argument("--timeout", type=float, default=900.0)
    parser.add_argument("--retries", type=int, default=2)
    parser.add_argument(
        "--official-request-delay-seconds",
        type=float,
        default=0.0,
        help="pause between new official snapshot batches from the same process",
    )
    parser.add_argument(
        "--official-ssh-hosts",
        default="",
        help=(
            "comma-separated configured SSH aliases used round-robin as "
            "independent public API exits"
        ),
    )
    parser.add_argument(
        "--point-delay-seconds",
        type=float,
        default=DEFAULT_POINT_DELAY_SECONDS,
        help=(
            "pause between successful local points so validation does not "
            "starve production monitoring or SSH"
        ),
    )
    parser.add_argument(
        "--field-chunk-size",
        type=int,
        default=DEFAULT_FIELD_CHUNK_SIZE,
        help="maximum hourly or daily fields per local API request",
    )
    parser.add_argument(
        "--request-delay-seconds",
        type=float,
        default=DEFAULT_REQUEST_DELAY_SECONDS,
        help="pause between field-chunk requests for the same point",
    )
    parser.add_argument(
        "--min-available-memory-mib",
        type=float,
        default=DEFAULT_MIN_AVAILABLE_MEMORY_MIB,
        help="wait before the next local request when Linux MemAvailable is lower",
    )
    parser.add_argument(
        "--max-io-full-pressure-avg10",
        type=float,
        default=DEFAULT_MAX_IO_FULL_PRESSURE_AVG10,
        help="wait while Linux full I/O PSI avg10 exceeds this percentage",
    )
    parser.add_argument(
        "--resource-wait-timeout-seconds",
        type=float,
        default=DEFAULT_RESOURCE_WAIT_TIMEOUT_SECONDS,
    )
    parser.add_argument(
        "--resource-poll-seconds",
        type=float,
        default=DEFAULT_RESOURCE_POLL_SECONDS,
    )
    parser.add_argument(
        "--max-local-om-api-processes",
        type=int,
        default=DEFAULT_MAX_LOCAL_OM_API_PROCESSES,
        help="refuse loopback validation when too many om-api processes exist",
    )
    parser.add_argument(
        "--point-limit",
        type=int,
        default=POINT_COUNT,
        help="validate only the first N points for a partial smoke run (maximum 300)",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    models = [value.strip() for value in args.models.split(",") if value.strip()]
    invalid = set(models) - set(MODEL_SPECS)
    if invalid:
        raise ValidationError(f"unknown models: {sorted(invalid)}")
    non_negative = {
        "--point-delay-seconds": args.point_delay_seconds,
        "--request-delay-seconds": args.request_delay_seconds,
        "--official-request-delay-seconds": args.official_request_delay_seconds,
        "--min-available-memory-mib": args.min_available_memory_mib,
        "--max-io-full-pressure-avg10": args.max_io_full_pressure_avg10,
        "--resource-wait-timeout-seconds": args.resource_wait_timeout_seconds,
    }
    invalid_non_negative = [
        name for name, value in non_negative.items() if value < 0
    ]
    if invalid_non_negative:
        raise ValidationError(
            f"{', '.join(invalid_non_negative)} must be non-negative"
        )
    positive = {
        "--field-chunk-size": args.field_chunk_size,
        "--resource-poll-seconds": args.resource_poll_seconds,
        "--max-local-om-api-processes": args.max_local_om_api_processes,
        "--point-limit": args.point_limit,
    }
    invalid_positive = [name for name, value in positive.items() if value <= 0]
    if invalid_positive:
        raise ValidationError(f"{', '.join(invalid_positive)} must be positive")
    if args.point_limit > POINT_COUNT:
        raise ValidationError(
            f"--point-limit must not exceed the {POINT_COUNT}-point immutable plan"
        )
    local_ssh_host = args.local_ssh_host.strip() or None
    if local_ssh_host and not is_loopback_url(args.local_base):
        raise ValidationError("--local-ssh-host requires a loopback --local-base")
    if args.official_snapshot_root and args.command != "validate":
        raise ValidationError(
            "--official-snapshot-root is only valid with the validate command"
        )
    args.output.mkdir(parents=True, exist_ok=True)
    manifest_path = args.output / "manifest.json"
    ensure_validation_manifest(manifest_path, models)
    official_snapshot_root = args.official_snapshot_root or args.output
    if args.official_snapshot_root:
        ensure_validation_manifest(
            official_snapshot_root / "manifest.json", models
        )
    if args.command in {"capture", "run"}:
        api_key = os.environ.get(args.api_key_env, "").strip() or None
        official_ssh_hosts = tuple(
            value.strip()
            for value in args.official_ssh_hosts.split(",")
            if value.strip()
        )
        for model_index, model in enumerate(models):
            if official_ssh_hosts:
                rotation = (model_index * 2) % len(official_ssh_hosts)
                model_ssh_hosts = (
                    official_ssh_hosts[rotation:] + official_ssh_hosts[:rotation]
                )
            else:
                model_ssh_hosts = ()
            metadata = capture_official(
                model,
                args.output,
                api_key,
                args.timeout,
                args.retries,
                args.official_request_delay_seconds,
                model_ssh_hosts,
            )
            print(
                json.dumps(
                    {
                        "model": model,
                        "official_snapshot": metadata["response_sha256"],
                        "official_requests": metadata["official_request_count"],
                    },
                    ensure_ascii=False,
                ),
                flush=True,
            )
    if args.command in {"validate", "run"}:
        attempt_id = attempt_id_now()
        transport_context = (
            ProductionSshApiClient(local_ssh_host, args.timeout, args.retries)
            if local_ssh_host
            else contextlib.nullcontext(None)
        )
        with transport_context as local_ssh_client:
            with validation_lock(args.output):
                for model in models:
                    report = validate_model(
                        model,
                        args.output,
                        args.local_base,
                        args.timeout,
                        args.retries,
                        args.point_delay_seconds,
                        args.field_chunk_size,
                        args.request_delay_seconds,
                        args.min_available_memory_mib,
                        args.max_io_full_pressure_avg10,
                        args.resource_wait_timeout_seconds,
                        args.resource_poll_seconds,
                        args.max_local_om_api_processes,
                        attempt_id,
                        args.point_limit,
                        local_ssh_client,
                        official_snapshot_root,
                    )
                    print(json.dumps(report, ensure_ascii=False), flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ValidationError as exc:
        print(f"validation error: {exc}", file=sys.stderr)
        raise SystemExit(1)
