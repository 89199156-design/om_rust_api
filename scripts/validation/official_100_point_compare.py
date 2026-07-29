#!/usr/bin/env python3
"""Snapshot once, then compare 100 points against the official APIs.

The official response for each model is captured by one multi-location POST.
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
import contextlib
import datetime as dt
import hashlib
import json
import math
import os
from pathlib import Path
import random
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


SCHEMA_VERSION = 1
POINT_COUNT = 100
USER_AGENT = "om-weather-server-official-100-point-validation/1.0"
DEFAULT_FIELD_CHUNK_SIZE = 12
DEFAULT_REQUEST_DELAY_SECONDS = 0.5
DEFAULT_POINT_DELAY_SECONDS = 2.0
DEFAULT_MIN_AVAILABLE_MEMORY_MIB = 768.0
DEFAULT_MAX_IO_FULL_PRESSURE_AVG10 = 10.0
DEFAULT_RESOURCE_WAIT_TIMEOUT_SECONDS = 900.0
DEFAULT_RESOURCE_POLL_SECONDS = 5.0
DEFAULT_MAX_LOCAL_OM_API_PROCESSES = 2
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
CAMS_HOURLY_LOCAL = CAMS_HOURLY_OFFICIAL
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
    plan: list[dict[str, Any]] = []
    for period, variables in (
        ("hourly", tuple(spec["local_hourly"])),
        ("daily", tuple(spec["daily"]) if model != "cams" else ()),
    ):
        for group in chunks(variables, field_chunk_size):
            plan.append({"period": period, "variables": group})
    return plan


def attempt_id_now() -> str:
    return dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")


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
    lock_path = output / ".official-100-validation.lock"
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
        "type": "official_100_point_validation_manifest",
        "models": models,
        "point_count_per_model": POINT_COUNT,
        "random_seed": 20260729,
        "sampling_cohorts": {
            "random_exact_common_native_grid": 35,
            "random_offgrid_near_native_grid": 35,
            "random_offgrid_uniform_crop": 30,
        },
        "cell_selection": "nearest",
        "points": sample_points(),
        "official_capture_policy": "one_multi_location_post_per_model_then_immutable_reuse",
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
    for latitude, longitude in common_grid[:35]:
        points.append(
            {
                "id": f"p{len(points):03d}",
                "order": len(points),
                "latitude": latitude,
                "longitude": longitude,
                "kind": "random_exact_common_native_grid",
            }
        )
    for latitude, longitude in common_grid[35:70]:
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
        return metadata

    points = sample_points()
    payload = official_payload(model, points)
    payload_raw = canonical_bytes(payload)
    api_key = (api_key or "").strip()
    wire_payload = {**payload, **({"apikey": api_key} if api_key else {})}
    wire_payload_raw = canonical_bytes(wire_payload)
    endpoint_key = "customer_endpoint" if api_key else "public_endpoint"
    endpoint = MODEL_SPECS[model][endpoint_key]
    headers = {
        "Accept": "application/json",
        "Content-Type": "application/json",
        "User-Agent": USER_AGENT,
    }
    raw, response_headers, elapsed = request_json(
        "POST",
        endpoint,
        body=wire_payload_raw,
        headers=headers,
        timeout=timeout,
        retries=retries,
        redact=(api_key,) if api_key else (),
    )
    try:
        rows = normalize_rows(json.loads(raw), POINT_COUNT)
    except (json.JSONDecodeError, TypeError) as exc:
        raise ValidationError(f"official {model} response is not valid JSON") from exc
    write_once(request_path, pretty_bytes(payload))
    write_once(response_path, raw)
    metadata = {
        "schema_version": SCHEMA_VERSION,
        "type": "official_multi_location_snapshot",
        "model": model,
        "captured_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "endpoint": endpoint,
        "method": "POST",
        "official_request_count": 1,
        "point_count": len(rows),
        "request_sha256": sha256_bytes(payload_raw),
        "response_sha256": sha256_bytes(raw),
        "response_bytes": len(raw),
        "elapsed_seconds": round(elapsed, 6),
        "response_headers": response_headers,
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
) -> str:
    spec = MODEL_SPECS[model]
    if hourly is None and daily is None:
        hourly = tuple(spec["local_hourly"])
        daily = tuple(spec["daily"]) if model != "cams" else ()
    params: dict[str, Any] = {
        "latitude": f"{point['latitude']:.4f}",
        "longitude": f"{point['longitude']:.4f}",
        "forecast_days": str(spec["forecast_days"]),
        "timezone": "GMT",
        "timeformat": "iso8601",
        "cell_selection": "nearest",
    }
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
    missing_time = next(
        (
            (index, time_value)
            for index, time_value in enumerate(official_times)
            if time_value not in local_index_by_time
        ),
        None,
    )
    if missing_time is not None:
        index, time_value = missing_time
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
                for official_index, time_value in enumerate(official_times)
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
            hourly_count += len(official_values)
        else:
            daily_count += len(official_values)
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
) -> dict[str, Any]:
    official_path = output / model / "official" / "response.json"
    metadata_path = output / model / "official" / "metadata.json"
    if not official_path.exists() or not metadata_path.exists():
        raise ValidationError(f"official {model} snapshot is missing; run capture first")
    official_raw = official_path.read_bytes()
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    if sha256_bytes(official_raw) != metadata["response_sha256"]:
        raise ValidationError(f"official {model} snapshot hash mismatch")
    official_rows = normalize_rows(json.loads(official_raw), POINT_COUNT)
    plan = request_plan(model, field_chunk_size)
    if not plan:
        raise ValidationError(f"{model} has no local fields to validate")
    attempt_id = attempt_id or attempt_id_now()
    report_path = output / model / "report.json"
    report = {
        "schema_version": SCHEMA_VERSION,
        "type": "official_100_point_comparison",
        "model": model,
        "status": "running",
        "official_snapshot_sha256": metadata["response_sha256"],
        "official_requests": metadata["official_request_count"],
        "points_total": POINT_COUNT,
        "points_target": point_limit,
        "points_completed": 0,
        "local_requests_completed": 0,
        "local_requests_per_point": len(plan),
        "hourly_values_compared": 0,
        "daily_values_compared": 0,
        "comparison": "strict_official_json_values_for_field_intersection",
        "local_request_mode": "sequential_field_chunks",
        "field_chunk_size": field_chunk_size,
        "failure": None,
        "current_point": None,
        "current_request": None,
        "current_point_hourly_values_compared": 0,
        "current_point_daily_values_compared": 0,
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
    points = sample_points()
    receipts = output / model / "receipts"
    receipts.mkdir(parents=True, exist_ok=True)
    for point, official in zip(points[:point_limit], official_rows[:point_limit]):
        receipt_path = receipts / f"{point['order']:03d}_{point['id']}.json"
        if receipt_path.exists():
            receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
            report["points_completed"] += 1
            report["local_requests_completed"] += receipt.get(
                "local_request_count", len(plan)
            )
            report["hourly_values_compared"] += receipt["hourly_values_compared"]
            report["daily_values_compared"] += receipt["daily_values_compared"]
            continue
        report["current_point"] = point
        report["current_point_hourly_values_compared"] = 0
        report["current_point_daily_values_compared"] = 0
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
        local_elapsed_seconds = 0.0
        response_parts: list[dict[str, Any]] = []
        for request_index, request_part in enumerate(plan):
            period = request_part["period"]
            variables = request_part["variables"]
            resource_snapshot = wait_for_safe_local_resources(
                local_base=local_base,
                min_available_memory_mib=min_available_memory_mib,
                max_io_full_pressure_avg10=max_io_full_pressure_avg10,
                max_local_om_api_processes=max_local_om_api_processes,
                wait_timeout_seconds=resource_wait_timeout_seconds,
                poll_seconds=resource_poll_seconds,
            )
            report["current_request"] = {
                "index": request_index,
                "count": len(plan),
                "period": period,
                "variables": list(variables),
                "resources": resource_snapshot,
            }
            write_json(report_path, report)
            print(
                json.dumps(
                    {
                        "model": model,
                        "event": "request_started",
                        "point": point,
                        **report["current_request"],
                    },
                    ensure_ascii=False,
                ),
                flush=True,
            )
            url = local_url(
                local_base,
                model,
                point,
                hourly=variables if period == "hourly" else (),
                daily=variables if period == "daily" else (),
            )
            raw, headers, elapsed = request_json(
                "GET",
                url,
                body=None,
                headers={"Accept": "application/json", "User-Agent": USER_AGENT},
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
                / f"{request_index:03d}_{period}.json"
            )
            write_once(local_path, raw)
            local = normalize_rows(json.loads(raw), 1)[0]
            difference, hourly_part, daily_part = first_period_difference(
                period, variables, official, local
            )
            hourly_count += hourly_part
            daily_count += daily_part
            report["current_point_hourly_values_compared"] = hourly_count
            report["current_point_daily_values_compared"] = daily_count
            local_elapsed_seconds += elapsed
            part_metadata = {
                "index": request_index,
                "period": period,
                "variables": list(variables),
                "local_response_file": str(local_path),
                "local_response_sha256": sha256_bytes(raw),
                "local_elapsed_seconds": round(elapsed, 6),
                "local_response_headers": headers,
                "resources_before_request": resource_snapshot,
            }
            response_parts.append(part_metadata)
            report["local_requests_completed"] += 1
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
                        "period": period,
                        "variables": list(variables),
                        "local_elapsed_seconds": round(elapsed, 6),
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
            "status": "passed",
        }
        write_once(receipt_path, pretty_bytes(receipt))
        report["points_completed"] += 1
        report["hourly_values_compared"] += hourly_count
        report["daily_values_compared"] += daily_count
        report["current_point"] = None
        report["current_request"] = None
        report["current_point_hourly_values_compared"] = 0
        report["current_point_daily_values_compared"] = 0
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
                },
                ensure_ascii=False,
            ),
            flush=True,
        )
        if point_delay_seconds > 0 and report["points_completed"] < point_limit:
            time.sleep(point_delay_seconds)
    report["status"] = "passed" if point_limit == POINT_COUNT else "partial"
    report["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
    write_json(report_path, report)
    return report


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("capture", "validate", "run"))
    parser.add_argument("--models", default="gfs,ec,cams")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--local-base", default="http://127.0.0.1:8088")
    parser.add_argument("--api-key-env", default="OPEN_METEO_API_KEY")
    parser.add_argument("--timeout", type=float, default=900.0)
    parser.add_argument("--retries", type=int, default=2)
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
        help="validate only the first N points for a partial smoke run (maximum 100)",
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
    args.output.mkdir(parents=True, exist_ok=True)
    manifest_path = args.output / "manifest.json"
    ensure_validation_manifest(manifest_path, models)
    if args.command in {"capture", "run"}:
        api_key = os.environ.get(args.api_key_env, "").strip() or None
        for model in models:
            metadata = capture_official(
                model, args.output, api_key, args.timeout, args.retries
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
                )
                print(json.dumps(report, ensure_ascii=False), flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ValidationError as exc:
        print(f"validation error: {exc}", file=sys.stderr)
        raise SystemExit(1)
