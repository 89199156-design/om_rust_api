#!/usr/bin/env python3
"""Snapshot once, then compare 100 points against the official APIs.

The official response for each model is captured by one multi-location POST.
Validation then requests the local API one point at a time and stops at the
first difference.  Successful point receipts are immutable and resumable, so
diagnosis and fixes never consume the official API quota again.

Only the official/local field intersection is compared.  GFS and ECMWF hourly
and daily fields are compared directly.  Open-Meteo does not expose CAMS daily
fields or Chinese AQI fields, so local-only derived outputs are intentionally
outside this official parity run.
"""

from __future__ import annotations

import argparse
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
PRESSURE_LEVELS = (
    10, 50, 100, 150, 200, 250, 300, 350, 400, 450, 500, 550,
    600, 650, 700, 750, 800, 850, 900, 925, 950, 975, 1000,
)
GFS_PRESSURE_TYPES = (
    "temperature",
    "relative_humidity",
    "dew_point",
    "cloud_cover",
    "wind_speed",
    "wind_direction",
    "geopotential_height",
    "vertical_velocity",
)
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
GFS_HOURLY = GFS_SURFACE + tuple(
    f"{kind}_{level}hPa" for level in PRESSURE_LEVELS for kind in GFS_PRESSURE_TYPES
)
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
        "official_endpoint": "https://customer-api.open-meteo.com/v1/gfs",
        "local_path": "/v1/gfs",
        "model_parameter": ("models", ["gfs_global"]),
        "forecast_days": 16,
        "official_hourly": GFS_HOURLY,
        "local_hourly": GFS_HOURLY,
        "daily": GFS_DAILY,
    },
    "ec": {
        "official_endpoint": "https://customer-api.open-meteo.com/v1/ecmwf",
        "local_path": "/v1/ecmwf",
        "model_parameter": ("models", ["ecmwf_ifs025"]),
        "forecast_days": 15,
        "official_hourly": tuple(ECMWF_HOURLY),
        "local_hourly": tuple(ECMWF_HOURLY),
        "daily": tuple(ECMWF_DAILY),
    },
    "cams": {
        "official_endpoint": "https://customer-air-quality-api.open-meteo.com/v1/air-quality",
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
                raise ValidationError(
                    f"{method} {url} returned HTTP {exc.code}: "
                    f"{raw[:1000].decode('utf-8', errors='replace')}"
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
    api_key: str,
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
    endpoint = MODEL_SPECS[model]["official_endpoint"]
    headers = {
        "Accept": "application/json",
        "Content-Type": "application/json",
        "User-Agent": USER_AGENT,
        "X-Api-Key": api_key,
    }
    raw, response_headers, elapsed = request_json(
        "POST",
        endpoint,
        body=payload_raw,
        headers=headers,
        timeout=timeout,
        retries=retries,
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
        "api_key_transport": "X-Api-Key header",
        "api_key_persisted": False,
    }
    write_once(metadata_path, pretty_bytes(metadata))
    return metadata


def local_url(base: str, model: str, point: dict[str, Any]) -> str:
    spec = MODEL_SPECS[model]
    params: dict[str, Any] = {
        "latitude": f"{point['latitude']:.4f}",
        "longitude": f"{point['longitude']:.4f}",
        "hourly": ",".join(spec["local_hourly"]),
        "daily": ",".join(spec["daily"]),
        "forecast_days": str(spec["forecast_days"]),
        "timezone": "GMT",
        "timeformat": "iso8601",
        "cell_selection": "nearest",
    }
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


def first_direct_difference(
    model: str, official: dict[str, Any], local: dict[str, Any]
) -> tuple[dict[str, Any] | None, int, int]:
    spec = MODEL_SPECS[model]
    hourly_count = 0
    daily_count = 0
    for period, variables in (
        ("hourly", spec["official_hourly"]),
        ("daily", spec["daily"] if model != "cams" else ()),
    ):
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
        if official_period.get("time") != local_period.get("time"):
            return (
                {
                    "period": period,
                    "variable": "time",
                    "reason": "time_axis",
                    "official": official_period.get("time"),
                    "local": local_period.get("time"),
                },
                hourly_count,
                daily_count,
            )
        times = official_period["time"]
        for variable in variables:
            official_values = official_period.get(variable)
            local_values = local_period.get(variable)
            if official_values != local_values:
                if not isinstance(official_values, list) or not isinstance(local_values, list):
                    index = None
                else:
                    index = next(
                        (
                            offset
                            for offset, pair in enumerate(zip(official_values, local_values))
                            if pair[0] != pair[1]
                        ),
                        min(len(official_values), len(local_values)),
                    )
                return (
                    {
                        "period": period,
                        "variable": variable,
                        "reason": "json_value",
                        "index": index,
                        "time": times[index] if isinstance(index, int) and index < len(times) else None,
                        "official": (
                            official_values[index]
                            if isinstance(official_values, list)
                            and isinstance(index, int)
                            and index < len(official_values)
                            else official_values
                        ),
                        "local": (
                            local_values[index]
                            if isinstance(local_values, list)
                            and isinstance(index, int)
                            and index < len(local_values)
                            else local_values
                        ),
                    },
                    hourly_count,
                    daily_count,
                )
            if period == "hourly":
                hourly_count += len(official_values)
            else:
                daily_count += len(official_values)
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
    report_path = output / model / "report.json"
    report = {
        "schema_version": SCHEMA_VERSION,
        "type": "official_100_point_comparison",
        "model": model,
        "status": "running",
        "official_snapshot_sha256": metadata["response_sha256"],
        "official_requests": metadata["official_request_count"],
        "points_total": POINT_COUNT,
        "points_completed": 0,
        "hourly_values_compared": 0,
        "daily_values_compared": 0,
        "comparison": "strict_official_json_values_for_field_intersection",
        "failure": None,
    }
    points = sample_points()
    receipts = output / model / "receipts"
    receipts.mkdir(parents=True, exist_ok=True)
    for point, official in zip(points, official_rows):
        receipt_path = receipts / f"{point['order']:03d}_{point['id']}.json"
        if receipt_path.exists():
            receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
            report["points_completed"] += 1
            report["hourly_values_compared"] += receipt["hourly_values_compared"]
            report["daily_values_compared"] += receipt["daily_values_compared"]
            continue
        url = local_url(local_base, model, point)
        raw, headers, elapsed = request_json(
            "GET",
            url,
            body=None,
            headers={"Accept": "application/json", "User-Agent": USER_AGENT},
            timeout=timeout,
            retries=retries,
        )
        local_path = output / model / "local" / f"{point['order']:03d}_{point['id']}.json"
        write_once(local_path, raw)
        local = normalize_rows(json.loads(raw), 1)[0]
        difference, hourly_count, daily_count = first_direct_difference(model, official, local)
        if difference is not None:
            failure = {
                "point": point,
                "difference": difference,
                "local_response_file": str(local_path),
                "local_response_sha256": sha256_bytes(raw),
            }
            report["status"] = "failed"
            report["failure"] = failure
            write_json(report_path, report)
            raise ValidationError(
                f"{model} stopped at {point['id']}: {json.dumps(difference, ensure_ascii=False)}"
            )
        receipt = {
            "schema_version": SCHEMA_VERSION,
            "model": model,
            "point": point,
            "official_row_sha256": sha256_bytes(canonical_bytes(official)),
            "local_response_sha256": sha256_bytes(raw),
            "local_elapsed_seconds": round(elapsed, 6),
            "local_response_headers": headers,
            "hourly_values_compared": hourly_count,
            "daily_values_compared": daily_count,
            "status": "passed",
        }
        write_once(receipt_path, pretty_bytes(receipt))
        report["points_completed"] += 1
        report["hourly_values_compared"] += hourly_count
        report["daily_values_compared"] += daily_count
        write_json(report_path, report)
    report["status"] = "passed"
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
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    models = [value.strip() for value in args.models.split(",") if value.strip()]
    invalid = set(models) - set(MODEL_SPECS)
    if invalid:
        raise ValidationError(f"unknown models: {sorted(invalid)}")
    args.output.mkdir(parents=True, exist_ok=True)
    manifest_path = args.output / "manifest.json"
    manifest = {
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
    write_once(manifest_path, pretty_bytes(manifest))
    if args.command in {"capture", "run"}:
        api_key = os.environ.get(args.api_key_env, "").strip()
        if not api_key:
            raise ValidationError(
                f"commercial official capture requires {args.api_key_env}; "
                "the public non-commercial API is intentionally not used"
            )
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
        for model in models:
            report = validate_model(
                model, args.output, args.local_base, args.timeout, args.retries
            )
            print(json.dumps(report, ensure_ascii=False), flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ValidationError as exc:
        print(f"validation error: {exc}", file=sys.stderr)
        raise SystemExit(1)
