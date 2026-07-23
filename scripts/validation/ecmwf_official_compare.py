#!/usr/bin/env python3
"""Strict frozen-run ECMWF IFS025 parity validation.

The official oracle is captured first with the minimum number of immutable
multi-location JSON POSTs allowed by the selected endpoint.  Public batches may
be statically assigned to independent named terminals, with every terminal kept
within its configured daily request weight.  Validation is then performed
serially: one complete local JSON POST for one point, followed immediately by a
strict comparison of that point's complete hourly and daily response.  The
first difference stops the run and leaves immutable diagnostic evidence.
"""

from __future__ import annotations

import argparse
import base64
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
import datetime as dt
from email.utils import parsedate_to_datetime
import hashlib
import json
import math
import os
from pathlib import Path
import random
import re
import socket
import subprocess
import sys
import time
from typing import Any, Callable
import urllib.error
import urllib.parse
import urllib.request
import uuid

try:
    from .ecmwf_variable_catalog import (
        CATALOG_SCHEMA_VERSION,
        DAILY_CATALOG_SHA256,
        DAILY_VARIABLES,
        HOURLY_CATALOG_SHA256,
        HOURLY_VARIABLES,
        OPEN_METEO_UPSTREAM_BASELINE,
        ROLLING_HOUR0_INHERITED_VARIABLES,
    )
except ImportError:  # Direct execution: python scripts/validation/ecmwf_official_compare.py
    from ecmwf_variable_catalog import (  # type: ignore[no-redef]
        CATALOG_SCHEMA_VERSION,
        DAILY_CATALOG_SHA256,
        DAILY_VARIABLES,
        HOURLY_CATALOG_SHA256,
        HOURLY_VARIABLES,
        OPEN_METEO_UPSTREAM_BASELINE,
        ROLLING_HOUR0_INHERITED_VARIABLES,
    )


SCHEMA_VERSION = 2
MODEL = "ecmwf_ifs025"
POINT_COUNT = 500
FORECAST_HOUR_START = 0
FORECAST_HOUR_END = 360
HOURLY_FRAMES = 361
DAILY_FRAMES = 15
COHORT_COUNTS = {
    "exact_native_grid": 200,
    "offgrid_nearest": 149,
    "offgrid_land": 151,
}
# Fixed anchors make crop edges, coasts/islands, and high terrain unavoidable;
# the remaining points are seeded random coverage.  Every point still uses the
# production land-cell selection and server-computed DEM elevation.
COVERAGE_ANCHORS: tuple[tuple[str, float, float, str], ...] = (
    ("exact_native_grid", 0.0, 70.0, "crop_boundary"),
    ("exact_native_grid", 0.0, 140.0, "crop_boundary"),
    ("exact_native_grid", 58.0, 70.0, "crop_boundary"),
    ("exact_native_grid", 58.0, 140.0, "crop_boundary"),
    ("exact_native_grid", 31.0, 88.0, "high_mountain"),
    ("exact_native_grid", 39.5, 75.0, "high_mountain"),
    ("exact_native_grid", 28.0, 87.0, "high_mountain"),
    ("exact_native_grid", 43.0, 87.5, "high_mountain"),
    ("exact_native_grid", 31.25, 121.5, "coastal"),
    ("exact_native_grid", 22.5, 114.0, "coastal"),
    ("exact_native_grid", 24.0, 121.0, "coastal_mountain"),
    ("exact_native_grid", 1.25, 103.75, "coastal"),
    ("offgrid_nearest", 0.001, 70.001, "crop_boundary"),
    ("offgrid_nearest", 0.001, 139.999, "crop_boundary"),
    ("offgrid_nearest", 57.999, 70.001, "crop_boundary"),
    ("offgrid_nearest", 57.999, 139.999, "crop_boundary"),
    ("offgrid_nearest", 31.2304, 121.4737, "coastal"),
    ("offgrid_nearest", 22.3193, 114.1694, "coastal"),
    ("offgrid_nearest", 10.8231, 106.6297, "coastal"),
    ("offgrid_nearest", 14.5995, 120.9842, "island_coastal"),
    ("offgrid_nearest", 27.9881, 86.925, "high_mountain"),
    ("offgrid_nearest", 35.8617, 76.5133, "high_mountain"),
    ("offgrid_nearest", 30.0668, 79.0193, "high_mountain"),
    ("offgrid_nearest", 23.47, 120.9575, "island_mountain"),
    ("offgrid_land", 18.2528, 109.5119, "island_coastal"),
    ("offgrid_land", 37.5665, 126.978, "coastal"),
    ("offgrid_land", 35.6762, 139.6503, "coastal"),
    ("offgrid_land", 29.652, 91.1721, "high_mountain"),
    ("offgrid_land", 43.222, 76.8512, "high_mountain"),
    ("offgrid_land", 38.5739, 70.0864, "high_mountain"),
    ("offgrid_land", 34.6, 70.2, "high_mountain"),
)
RUN_KEYS = ("latest_complete_run", "reference_time", "model_run", "run")
OFFICIAL_PROFILE_ORDER = ("land_dem",)
IGNORED_DYNAMIC_METADATA = ("generationtime_ms", "location_id")
ROLLING_HOUR0_VARIABLES = ROLLING_HOUR0_INHERITED_VARIABLES
USER_AGENT = "om-weather-server-ecmwf-strict-validation/2.0"


class ValidationError(RuntimeError):
    """The validation contract, an artifact, or an API response is invalid."""


@dataclass(frozen=True)
class HttpResult:
    status: int
    raw: bytes
    headers: dict[str, str]
    elapsed_seconds: float
    executor_id: str | None = None


class HttpRequestError(ValidationError):
    def __init__(self, message: str, result: HttpResult | None = None) -> None:
        super().__init__(message)
        self.result = result


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=True,
        sort_keys=True,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")


def pretty_bytes(value: Any) -> bytes:
    return (
        json.dumps(
            value,
            ensure_ascii=False,
            indent=2,
            sort_keys=True,
            allow_nan=False,
        ).encode("utf-8")
        + b"\n"
    )


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as exc:
        raise ValidationError(f"cannot hash {path}: {exc}") from exc
    return digest.hexdigest()


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z")


def read_json(path: Path) -> Any:
    try:
        raw = path.read_bytes()
        value = json.loads(raw)
    except (OSError, json.JSONDecodeError) as exc:
        raise ValidationError(f"cannot read JSON {path}: {exc}") from exc
    _assert_standard_json(value)
    return value


def _stage_bytes(path: Path, payload: bytes) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.parent / f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp"
    try:
        with temporary.open("xb") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    return temporary


def _fsync_directory(path: Path) -> None:
    flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError:
        return
    try:
        os.fsync(descriptor)
    except OSError:
        pass
    finally:
        os.close(descriptor)


def write_immutable_bytes(path: Path, payload: bytes) -> bool:
    """Create *path* atomically, or reuse it only when bytes are identical."""
    temporary = _stage_bytes(path, payload)
    try:
        try:
            os.link(temporary, path)
        except FileExistsError:
            try:
                existing = path.read_bytes()
            except OSError as exc:
                raise ValidationError(f"cannot verify immutable artifact {path}: {exc}") from exc
            if existing != payload:
                raise ValidationError(
                    f"refusing to overwrite immutable artifact with different bytes: {path}"
                )
            return False
        _fsync_directory(path.parent)
        return True
    finally:
        temporary.unlink(missing_ok=True)


def write_json_exclusive(path: Path, value: Any) -> None:
    if path.exists():
        raise ValidationError(f"refusing to overwrite immutable artifact: {path}")
    write_immutable_bytes(path, pretty_bytes(value))


def atomic_replace_bytes(path: Path, payload: bytes) -> None:
    temporary = _stage_bytes(path, payload)
    try:
        os.replace(temporary, path)
        _fsync_directory(path.parent)
    finally:
        temporary.unlink(missing_ok=True)


def atomic_commit_pair(
    response_path: Path,
    response: bytes,
    metadata_path: Path,
    metadata: bytes,
) -> None:
    if response_path.exists() or metadata_path.exists():
        if not response_path.is_file() or not metadata_path.is_file():
            raise ValidationError(f"incomplete immutable artifact pair: {response_path}")
        if response_path.read_bytes() != response or metadata_path.read_bytes() != metadata:
            raise ValidationError(f"refusing to replace immutable artifact pair: {response_path}")
        return
    response_temp = _stage_bytes(response_path, response)
    try:
        metadata_temp = _stage_bytes(metadata_path, metadata)
    except BaseException:
        response_temp.unlink(missing_ok=True)
        raise
    try:
        os.replace(response_temp, response_path)
        os.replace(metadata_temp, metadata_path)
        _fsync_directory(response_path.parent)
    finally:
        response_temp.unlink(missing_ok=True)
        metadata_temp.unlink(missing_ok=True)


def _with_self_hash(value: dict[str, Any], key: str) -> dict[str, Any]:
    result = dict(value)
    result.pop(key, None)
    result[key] = sha256_bytes(canonical_bytes(result))
    return result


def _verify_self_hash(value: Mapping[str, Any], key: str, description: str) -> str:
    unsigned = dict(value)
    claimed = unsigned.pop(key, None)
    actual = sha256_bytes(canonical_bytes(unsigned))
    if not isinstance(claimed, str) or claimed != actual:
        raise ValidationError(f"{description} {key} mismatch")
    return actual


def _assert_standard_json(value: Any, path: str = "$") -> None:
    if value is None or isinstance(value, (str, bool, int)):
        return
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValidationError(f"non-finite JSON number at {path}")
        return
    if isinstance(value, list):
        for index, item in enumerate(value):
            _assert_standard_json(item, f"{path}[{index}]")
        return
    if isinstance(value, dict):
        for key, item in value.items():
            if not isinstance(key, str):
                raise ValidationError(f"non-string JSON object key at {path}")
            _assert_standard_json(item, f"{path}.{key}")
        return
    raise ValidationError(f"non-JSON value at {path}: {type(value).__name__}")


def parse_run(value: str) -> dt.datetime:
    text = value.strip()
    if len(text) == 10 and text.isdigit():
        parsed = dt.datetime.strptime(text, "%Y%m%d%H").replace(tzinfo=dt.timezone.utc)
    else:
        if text.endswith("Z"):
            text = text[:-1] + "+00:00"
        try:
            parsed = dt.datetime.fromisoformat(text)
        except ValueError as exc:
            raise ValidationError(f"invalid run time {value!r}") from exc
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=dt.timezone.utc)
        parsed = parsed.astimezone(dt.timezone.utc)
    if parsed.minute or parsed.second or parsed.microsecond or parsed.hour != 0:
        raise ValidationError("the strict ECMWF daily contract requires an exact 00Z run")
    return parsed


def _parse_run_instant(value: str) -> dt.datetime:
    text = value.strip()
    if len(text) == 10 and text.isdigit():
        parsed = dt.datetime.strptime(text, "%Y%m%d%H").replace(
            tzinfo=dt.timezone.utc
        )
    else:
        if text.endswith("Z"):
            text = text[:-1] + "+00:00"
        try:
            parsed = dt.datetime.fromisoformat(text)
        except ValueError as exc:
            raise ValidationError(f"invalid model run time {value!r}") from exc
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=dt.timezone.utc)
        parsed = parsed.astimezone(dt.timezone.utc)
    if parsed.minute or parsed.second or parsed.microsecond:
        raise ValidationError("model run time must be aligned to an exact hour")
    return parsed


def format_run(value: dt.datetime) -> str:
    return value.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def query_run(value: str) -> str:
    return parse_run(value).strftime("%Y-%m-%dT%H:%M")


def _require_string_list(value: Any, description: str) -> list[str]:
    if (
        not isinstance(value, list)
        or not value
        or any(not isinstance(item, str) or not item.strip() for item in value)
        or len(value) != len(set(value))
    ):
        raise ValidationError(f"{description} must be a non-empty unique string list")
    return value


def load_config(path: Path) -> dict[str, Any]:
    config = read_json(path)
    if not isinstance(config, dict):
        raise ValidationError("validation config must be a JSON object")
    if config.get("schema_version") != SCHEMA_VERSION or config.get("model") != MODEL:
        raise ValidationError(f"unsupported validation config: {path}")
    serialized_keys = set()

    def visit(value: Any) -> None:
        if isinstance(value, dict):
            for key, item in value.items():
                serialized_keys.add(str(key).lower())
                visit(item)
        elif isinstance(value, list):
            for item in value:
                visit(item)

    visit(config)
    if "exemptions" in serialized_keys or any("tolerance" in key for key in serialized_keys):
        raise ValidationError("strict validation forbids tolerances and exemptions")
    variables = config.get("variables")
    if not isinstance(variables, dict):
        raise ValidationError("config variables must contain hourly and daily lists")
    hourly = _require_string_list(variables.get("hourly"), "variables.hourly")
    daily = _require_string_list(variables.get("daily"), "variables.daily")
    if hourly != list(HOURLY_VARIABLES):
        raise ValidationError(
            "variables.hourly must exactly match the canonical 197-field ECMWF catalog"
        )
    if daily != list(DAILY_VARIABLES):
        raise ValidationError(
            "variables.daily must exactly match the canonical 65-field ECMWF catalog"
        )
    if config.get("variable_catalog") != {
        "schema_version": CATALOG_SCHEMA_VERSION,
        "hourly_sha256": HOURLY_CATALOG_SHA256,
        "daily_sha256": DAILY_CATALOG_SHA256,
    }:
        raise ValidationError("variable_catalog fingerprints do not match the canonical module")
    if config.get("open_meteo_upstream_baseline") != OPEN_METEO_UPSTREAM_BASELINE:
        raise ValidationError(
            "open_meteo_upstream_baseline does not match the audited ECMWF source revision"
        )
    if "showers" in hourly or "showers_sum" in daily:
        raise ValidationError("source-unavailable IFS showers fields must be omitted, not exempted")
    if any("precipitation_probability" in variable for variable in [*hourly, *daily]):
        raise ValidationError(
            "source-unavailable IFS precipitation-probability fields must be omitted"
        )
    if config.get("rolling_hour0_inherited_variables") != list(ROLLING_HOUR0_VARIABLES):
        raise ValidationError(
            "rolling_hour0_inherited_variables must record the exact six live-series fields"
        )
    missing_hour0 = [variable for variable in ROLLING_HOUR0_VARIABLES if variable not in hourly]
    if missing_hour0:
        raise ValidationError(
            f"hourly contract is missing rolling hour-0 variables: {missing_hour0}"
        )
    dynamic = config.get("ignored_dynamic_metadata")
    if dynamic != list(IGNORED_DYNAMIC_METADATA):
        raise ValidationError(
            "ignored_dynamic_metadata must be exactly generationtime_ms and location_id"
        )
    crop = config.get("crop")
    if crop != {
        "longitude_min": 70.0,
        "longitude_max": 140.0,
        "latitude_min": 0.0,
        "latitude_max": 58.0,
    }:
        raise ValidationError("validation crop must be exactly 70..140E and 0..58N")
    grid = config.get("native_grid")
    if not isinstance(grid, dict) or grid.get("resolution_degrees") != 0.25:
        raise ValidationError("native ECMWF grid resolution must be 0.25 degrees")
    sampling = config.get("sampling")
    if not isinstance(sampling, dict):
        raise ValidationError("config has no sampling contract")
    expected_counts = COHORT_COUNTS
    if sampling.get("point_count") != POINT_COUNT or sampling.get("cohort_counts") != expected_counts:
        raise ValidationError(f"sampling contract must use exactly {POINT_COUNT} points: {expected_counts}")
    horizon = config.get("horizon")
    if horizon != {
        "forecast_hour_start": FORECAST_HOUR_START,
        "forecast_hour_end": FORECAST_HOUR_END,
        "hourly_frames": HOURLY_FRAMES,
        "complete_daily_frames": DAILY_FRAMES,
    }:
        raise ValidationError("horizon must be hourly 0..360 and 15 complete GMT days")
    official = config.get("official")
    if not isinstance(official, dict) or official.get("multi_location_limit") != 1000:
        raise ValidationError("official multi-location contract must record the source limit of 1000")
    if official.get("expected_request_count") != len(OFFICIAL_PROFILE_ORDER):
        raise ValidationError("official contract must use one POST per sampling semantic")
    request_options = config.get("request_options")
    if not isinstance(request_options, dict) or request_options.get("timezone") != "GMT":
        raise ValidationError("request_options.timezone must be GMT")
    storage = config.get("storage")
    if storage != {
        "production_evidence_roots": [
            "/data/om_validation_snapshots",
            "/data/validation/ecmwf",
        ]
    }:
        raise ValidationError("production validation evidence roots must stay on /data")
    return config


def require_production_evidence_path(
    path: Path,
    config: Mapping[str, Any],
    description: str,
    *,
    platform_name: str | None = None,
) -> None:
    """Keep generated production evidence off the Linux system disk."""
    if (platform_name or os.name) == "nt":
        return
    resolved = path.resolve()
    allowed = [
        Path(root).resolve()
        for root in config["storage"]["production_evidence_roots"]
    ]
    if not any(resolved == root or root in resolved.parents for root in allowed):
        raise ValidationError(
            f"{description} must be under a configured /data evidence root: {resolved}"
        )


def _point_profile(point: Mapping[str, Any]) -> str:
    if point.get("cell_selection") == "land" and point.get("elevation_mode") is None:
        return "land_dem"
    raise ValidationError(f"unsupported point sampling profile: {point}")


def _sample_points(seed: int, config: Mapping[str, Any]) -> list[dict[str, Any]]:
    crop = config["crop"]
    counts = config["sampling"]["cohort_counts"]
    rng = random.Random(seed)
    points: list[dict[str, Any]] = []
    seen: set[tuple[float, float]] = set()

    def add_point(kind: str, latitude: float, longitude: float, coverage: str) -> None:
        key = (latitude, longitude)
        if key in seen:
            raise AssertionError(f"duplicate deterministic validation point: {key}")
        if not (
            float(crop["latitude_min"]) <= latitude <= float(crop["latitude_max"])
            and float(crop["longitude_min"]) <= longitude <= float(crop["longitude_max"])
        ):
            raise AssertionError(f"validation point is outside the production crop: {key}")
        seen.add(key)
        points.append(
            {
                "id": f"p{len(points):04d}",
                "order": len(points),
                "kind": kind,
                "coverage_class": coverage,
                "cell_selection": "land",
                "elevation_mode": None,
                "latitude": latitude,
                "longitude": longitude,
            }
        )

    latitude_tick_min = int(round(float(crop["latitude_min"]) * 4))
    latitude_tick_max = int(round(float(crop["latitude_max"]) * 4))
    longitude_tick_min = int(round(float(crop["longitude_min"]) * 4))
    longitude_tick_max = int(round(float(crop["longitude_max"]) * 4))
    for kind, latitude, longitude, coverage in COVERAGE_ANCHORS:
        if kind == "exact_native_grid":
            add_point(kind, latitude, longitude, coverage)
    while len(points) < counts["exact_native_grid"]:
        latitude = rng.randrange(latitude_tick_min, latitude_tick_max + 1) / 4.0
        longitude = rng.randrange(longitude_tick_min, longitude_tick_max + 1) / 4.0
        key = (latitude, longitude)
        if key in seen:
            continue
        add_point("exact_native_grid", latitude, longitude, "seeded_general")

    for kind in ("offgrid_nearest", "offgrid_land"):
        target = len(points) + int(counts[kind])
        for anchor_kind, latitude, longitude, coverage in COVERAGE_ANCHORS:
            if anchor_kind == kind:
                add_point(kind, latitude, longitude, coverage)
        while len(points) < target:
            latitude = round(
                rng.uniform(float(crop["latitude_min"]), float(crop["latitude_max"])), 6
            )
            longitude = round(
                rng.uniform(float(crop["longitude_min"]), float(crop["longitude_max"])), 6
            )
            key = (latitude, longitude)
            if key in seen:
                continue
            if abs(latitude * 4 - round(latitude * 4)) < 1e-5:
                continue
            if abs(longitude * 4 - round(longitude * 4)) < 1e-5:
                continue
            add_point(kind, latitude, longitude, "seeded_general")
    if len(points) != POINT_COUNT:
        raise AssertionError("fixed point sampler did not produce 500 points")
    actual_counts = {
        kind: sum(point["kind"] == kind for point in points) for kind in COHORT_COUNTS
    }
    if actual_counts != counts:
        raise AssertionError(f"fixed point cohort counts changed: {actual_counts}")
    return points


def _hourly_axis(run: dt.datetime) -> list[str]:
    return [
        (run + dt.timedelta(hours=offset)).strftime("%Y-%m-%dT%H:%M")
        for offset in range(HOURLY_FRAMES)
    ]


def _daily_axis(run: dt.datetime) -> list[str]:
    return [(run.date() + dt.timedelta(days=offset)).isoformat() for offset in range(DAILY_FRAMES)]


def generate_plan(
    run: str,
    count: int = POINT_COUNT,
    seed: int = 20260723,
    forecast_hours: int = HOURLY_FRAMES,
    batch_size: int | None = None,
    config: dict[str, Any] | None = None,
    config_sha256: str | None = None,
) -> dict[str, Any]:
    """Build the one allowed deterministic production plan.

    ``batch_size`` is retained only to reject legacy fixed-GET batching.  The
    source supports 1000 POST locations, so all 500 pass/fail points use the
    production default land+DEM semantic in one customer-API POST.
    """
    if config is None or not isinstance(config_sha256, str):
        raise ValidationError("config and config_sha256 are required")
    run_time = parse_run(run)
    if count != POINT_COUNT:
        raise ValidationError(f"the production validation plan requires exactly {POINT_COUNT} points")
    if forecast_hours != HOURLY_FRAMES:
        raise ValidationError("the production validation plan requires forecast hours 0..360")
    if batch_size is not None:
        raise ValidationError("fixed GET batch sizes are forbidden; official capture uses one POST")
    points = _sample_points(seed, config)
    grouped = {
        profile: [point["id"] for point in points if _point_profile(point) == profile]
        for profile in OFFICIAL_PROFILE_ORDER
    }
    assigned: set[str] = set()
    for profile in OFFICIAL_PROFILE_ORDER:
        identifiers = set(grouped[profile])
        if assigned.intersection(identifiers):
            raise AssertionError("official profiles overlap")
        assigned.update(identifiers)
    plan = {
        "schema_version": SCHEMA_VERSION,
        "type": "ecmwf_ifs025_strict_validation_plan",
        "model": MODEL,
        "run": format_run(run_time),
        "seed": seed,
        "sample_count": POINT_COUNT,
        "crop": config["crop"],
        "native_grid": config["native_grid"],
        "hourly": {
            "variables": list(config["variables"]["hourly"]),
            "forecast_hours": HOURLY_FRAMES,
            "time": _hourly_axis(run_time),
        },
        "daily": {
            "variables": list(config["variables"]["daily"]),
            "forecast_days": DAILY_FRAMES,
            "time": _daily_axis(run_time),
        },
        "request_options": dict(config["request_options"]),
        "ignored_dynamic_metadata": list(IGNORED_DYNAMIC_METADATA),
        "rolling_hour0_inherited_variables": list(ROLLING_HOUR0_VARIABLES),
        "points": points,
        "official_profiles": [
            {"profile": profile, "point_ids": grouped[profile]}
            for profile in OFFICIAL_PROFILE_ORDER
        ],
        "official_expected_request_count": len(OFFICIAL_PROFILE_ORDER),
        "config_sha256": config_sha256,
        "open_meteo_upstream_baseline": config.get("open_meteo_upstream_baseline"),
        "variable_catalog": dict(config.get("variable_catalog", {})),
    }
    return _with_self_hash(plan, "plan_sha256")


def load_plan(path: Path, config: dict[str, Any], config_path: Path) -> dict[str, Any]:
    plan = read_json(path)
    if not isinstance(plan, dict):
        raise ValidationError("plan must contain a JSON object")
    _verify_self_hash(plan, "plan_sha256", "plan")
    config_hash = sha256_file(config_path)
    if plan.get("config_sha256") != config_hash:
        raise ValidationError("plan/config hash mismatch; regenerate the plan")
    expected = generate_plan(
        str(plan.get("run", "")),
        count=POINT_COUNT,
        seed=int(plan.get("seed", -1)),
        forecast_hours=HOURLY_FRAMES,
        config=config,
        config_sha256=config_hash,
    )
    if plan != expected:
        raise ValidationError("plan differs from the deterministic production contract")
    return plan


def point_map(plan: Mapping[str, Any]) -> dict[str, dict[str, Any]]:
    return {str(point["id"]): point for point in plan_points(plan)}


def plan_points(plan: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Return points in immutable comparison order.

    The embedded points are checked against a fresh deterministic
    reconstruction before they are trusted.
    """
    points = _sample_points(
        int(plan["seed"]),
        {
            "crop": plan["crop"],
            "sampling": {
                "cohort_counts": {
                    **COHORT_COUNTS,
                }
            },
        },
    )
    if plan.get("points") != points:
        raise ValidationError("plan point coordinates/order changed")
    expected_profiles = [
        {
            "profile": profile,
            "point_ids": [p["id"] for p in points if _point_profile(p) == profile],
        }
        for profile in OFFICIAL_PROFILE_ORDER
    ]
    if plan.get("official_profiles") != expected_profiles:
        raise ValidationError("plan official profile membership changed")
    return points


def normalize_endpoint(endpoint: str) -> str:
    parsed = urllib.parse.urlsplit(endpoint)
    if (
        parsed.scheme not in ("http", "https")
        or not parsed.netloc
        or parsed.query
        or parsed.fragment
        or parsed.username
        or parsed.password
    ):
        raise ValidationError(
            "endpoint must be an HTTP(S) URL without credentials, query parameters, or fragment"
        )
    return urllib.parse.urlunsplit(
        (parsed.scheme, parsed.netloc, parsed.path.rstrip("/"), "", "")
    )


def resolve_official_access(
    endpoint: str,
    *,
    allow_public_noncommercial: bool,
    api_key_environment: str,
    allow_loopback_mock: bool = False,
) -> tuple[str | None, str]:
    endpoint = normalize_endpoint(endpoint)
    host = (urllib.parse.urlsplit(endpoint).hostname or "").lower()
    if host in ("127.0.0.1", "::1", "localhost"):
        if not allow_loopback_mock:
            raise ValidationError("loopback official endpoints require --allow-loopback-mock")
        return None, "mock"
    if host == "api.open-meteo.com":
        if not allow_public_noncommercial:
            raise ValidationError(
                "the public endpoint requires --allow-public-noncommercial; commercial validation "
                "must use the customer API host"
            )
        return None, "public_noncommercial"
    if not host.startswith("customer-") or not host.endswith(".open-meteo.com"):
        raise ValidationError("official endpoint must be the public or customer ECMWF API")
    if host != "customer-api.open-meteo.com":
        raise ValidationError("commercial validation requires customer-api.open-meteo.com")
    api_key = os.environ.get(api_key_environment, "").strip()
    if not api_key:
        raise ValidationError(
            f"customer API access requires {api_key_environment} in the environment"
        )
    return api_key, "customer"


def _profile_points(plan: Mapping[str, Any], profile: str) -> list[dict[str, Any]]:
    points = point_map(plan)
    profiles = {item["profile"]: item["point_ids"] for item in plan["official_profiles"]}
    if profile not in profiles:
        raise ValidationError(f"plan has no official profile {profile}")
    result = [points[point_id] for point_id in profiles[profile]]
    if any(_point_profile(point) != profile for point in result):
        raise ValidationError(f"plan profile {profile} contains incompatible points")
    return result


def official_payload(
    plan: Mapping[str, Any],
    points: list[Mapping[str, Any]],
) -> dict[str, Any]:
    if not points:
        raise ValidationError("official request cannot contain zero points")
    profiles = {_point_profile(point) for point in points}
    if len(profiles) != 1:
        raise ValidationError("official request cannot mix sampling/elevation semantics")
    profile = next(iter(profiles))
    options = plan["request_options"]
    payload: dict[str, Any] = {
        "latitude": [point["latitude"] for point in points],
        "longitude": [point["longitude"] for point in points],
        "hourly": list(plan["hourly"]["variables"]),
        "daily": list(plan["daily"]["variables"]),
        "models": [MODEL],
        "start_hour": [plan["hourly"]["time"][0]],
        "end_hour": [plan["hourly"]["time"][-1]],
        "start_date": [plan["daily"]["time"][0]],
        "end_date": [plan["daily"]["time"][-1]],
        "timezone": [options["timezone"]],
        "timeformat": options.get("timeformat", "iso8601"),
        "temperature_unit": options.get("temperature_unit", "celsius"),
        "wind_speed_unit": options.get("wind_speed_unit", "ms"),
        "precipitation_unit": options.get("precipitation_unit", "mm"),
        "cell_selection": points[0]["cell_selection"],
    }
    if profile != "land_dem":
        raise ValidationError("pass/fail capture only supports default land+DEM semantics")
    _assert_standard_json(payload)
    return payload


def local_request(
    endpoint: str,
    plan: Mapping[str, Any],
    point: Mapping[str, Any],
) -> tuple[str, bytes, dict[str, Any]]:
    endpoint = normalize_endpoint(endpoint)
    payload_raw = canonical_bytes(official_payload(plan, [point]))
    identity = {
        "method": "POST",
        "endpoint": endpoint,
        "content_type": "application/json",
        "payload_bytes": len(payload_raw),
        "payload_sha256": sha256_bytes(payload_raw),
    }
    return endpoint, payload_raw, identity


def _public_locations_per_request(plan: Mapping[str, Any], config: Mapping[str, Any]) -> int:
    variable_count = len(plan["hourly"]["variables"]) + len(plan["daily"]["variables"])
    if variable_count <= 0:
        raise ValidationError("official request has no variables")
    days = DAILY_FRAMES
    per_location_weight = max(variable_count / 10.0, variable_count / 10.0 * days / 14.0)
    weight_limit = float(config["official"].get("public_request_weight_limit", 5000))
    by_weight = int(math.floor(weight_limit / per_location_weight))
    return max(1, min(int(config["official"]["multi_location_limit"]), by_weight))


def official_batches(
    plan: Mapping[str, Any],
    config: Mapping[str, Any],
    access_profile: str,
) -> list[dict[str, Any]]:
    if access_profile in ("customer", "mock"):
        locations_per_request = int(config["official"]["multi_location_limit"])
    elif access_profile == "public_noncommercial":
        locations_per_request = _public_locations_per_request(plan, config)
    else:
        raise ValidationError(f"unsupported official access profile: {access_profile}")
    batches: list[dict[str, Any]] = []
    for profile in OFFICIAL_PROFILE_ORDER:
        points = _profile_points(plan, profile)
        sentinel_id = points[0]["id"]
        offset = 0
        batch_number = 0
        while offset < len(points):
            if offset == 0:
                selected = points[:locations_per_request]
                request_ids = [point["id"] for point in selected]
            else:
                selected = points[offset : offset + locations_per_request - 1]
                request_ids = [sentinel_id, *[point["id"] for point in selected]]
            if not selected:
                raise ValidationError("official public weight limit leaves no room beside sentinel")
            batches.append(
                {
                    "batch_id": f"{profile}_{batch_number:03d}",
                    "profile": profile,
                    "point_ids": [point["id"] for point in selected],
                    "request_point_ids": request_ids,
                    "sentinel_point_id": sentinel_id,
                    "estimated_weight": round(
                        (len(plan["hourly"]["variables"]) + len(plan["daily"]["variables"]))
                        / 10.0
                        * DAILY_FRAMES
                        / 14.0
                        * len(request_ids),
                        6,
                    ),
                }
            )
            offset += len(selected)
            batch_number += 1
    if sorted(point_id for batch in batches for point_id in batch["point_ids"]) != sorted(
        point["id"] for point in plan_points(plan)
    ):
        raise AssertionError("official batching did not cover every point exactly once")
    return batches


def assign_public_batches(
    batches: list[dict[str, Any]],
    executor_ids: Iterable[str],
    daily_weight_limit: float,
) -> list[dict[str, Any]]:
    """Bind every public batch to one executor before any network request.

    Assignment is deterministic and load-balanced by accumulated request
    weight.  A retry remains on the assigned executor because the binding is
    stored in the batch and in every success/failure artifact.
    """
    if not math.isfinite(daily_weight_limit) or daily_weight_limit <= 0:
        raise ValidationError("public daily weight limit must be positive")
    ordered_ids = list(executor_ids)
    if not ordered_ids:
        raise ValidationError("public capture requires at least one named executor")
    if len(set(ordered_ids)) != len(ordered_ids):
        raise ValidationError("public executor ids must be unique")
    if any(
        not executor_id or any(character.isspace() for character in executor_id)
        for executor_id in ordered_ids
    ):
        raise ValidationError("public executor ids must be non-empty and contain no whitespace")

    accumulated = {executor_id: 0.0 for executor_id in ordered_ids}
    assigned: list[dict[str, Any]] = []
    for batch in batches:
        weight = float(batch["estimated_weight"])
        if not math.isfinite(weight) or weight <= 0 or weight > daily_weight_limit:
            raise ValidationError(
                f"official batch {batch['batch_id']} weight {weight:.3f} exceeds "
                f"the per-terminal daily limit {daily_weight_limit:.3f}"
            )
        eligible = [
            executor_id
            for executor_id in ordered_ids
            if accumulated[executor_id] + weight <= daily_weight_limit + 1e-9
        ]
        if not eligible:
            required = sum(float(item["estimated_weight"]) for item in batches)
            capacity = daily_weight_limit * len(ordered_ids)
            raise ValidationError(
                "configured public executors cannot hold the complete statically "
                f"assigned matrix ({required:.3f} request weight, {capacity:.3f} "
                "aggregate daily capacity)"
            )
        executor_id = min(
            eligible,
            key=lambda item: (accumulated[item], ordered_ids.index(item)),
        )
        accumulated[executor_id] += weight
        assigned.append({**batch, "executor_id": executor_id})
    return assigned


def public_executor_summary(
    batches: Iterable[Mapping[str, Any]],
) -> tuple[dict[str, list[str]], dict[str, float]]:
    batch_ids: dict[str, list[str]] = {}
    weights: dict[str, float] = {}
    for batch in batches:
        executor_id = batch.get("executor_id")
        if not isinstance(executor_id, str):
            continue
        batch_ids.setdefault(executor_id, []).append(str(batch["batch_id"]))
        weights[executor_id] = round(
            weights.get(executor_id, 0.0) + float(batch["estimated_weight"]),
            6,
        )
    return batch_ids, weights


def _batch_request_points(
    batch: Mapping[str, Any], points: Mapping[str, dict[str, Any]]
) -> list[dict[str, Any]]:
    request_ids = batch.get("request_point_ids")
    if not isinstance(request_ids, list) or not request_ids:
        raise ValidationError(f"official batch has no request points: {batch.get('batch_id')}")
    try:
        return [points[point_id] for point_id in request_ids]
    except KeyError as exc:
        raise ValidationError(f"official batch references an unknown point: {exc}") from exc


def _safe_headers(headers: Mapping[str, Any] | None) -> dict[str, str]:
    if not headers:
        return {}
    allowed = {
        "content-type",
        "content-length",
        "retry-after",
        "date",
        "server",
        "last-modified",
        "etag",
    }
    return {
        str(key).lower(): str(value)
        for key, value in headers.items()
        if str(key).lower() in allowed
    }


def _request_once(
    method: str,
    url: str,
    *,
    body: bytes | None,
    headers: Mapping[str, str],
    timeout: float,
) -> HttpResult:
    request = urllib.request.Request(url, data=body, method=method, headers=dict(headers))
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read()
            return HttpResult(
                status=int(response.status),
                raw=raw,
                headers=_safe_headers(response.headers),
                elapsed_seconds=time.monotonic() - started,
            )
    except urllib.error.HTTPError as exc:
        try:
            raw = exc.read()
        except OSError:
            raw = b""
        return HttpResult(
            status=int(exc.code),
            raw=raw,
            headers=_safe_headers(exc.headers),
            elapsed_seconds=time.monotonic() - started,
        )
    except (urllib.error.URLError, TimeoutError, socket.timeout, OSError) as exc:
        raise HttpRequestError(f"{method} transport failure for {url}: {type(exc).__name__}: {exc}") from exc


_SSH_HTTP_MARKER = b"__OM_HTTP_META__"
_SSH_DESTINATION = re.compile(r"^[A-Za-z0-9_.@:-]+$")


class SshHttpRequester:
    """Execute one official HTTP request through a named, independent terminal."""

    def __init__(
        self,
        executor_id: str,
        destination: str,
        *,
        ssh_executable: str = "ssh",
    ) -> None:
        if not executor_id or any(character.isspace() for character in executor_id):
            raise ValidationError("SSH executor id must be non-empty and contain no whitespace")
        if (
            not destination
            or destination.startswith("-")
            or _SSH_DESTINATION.fullmatch(destination) is None
        ):
            raise ValidationError(f"unsafe SSH destination: {destination}")
        self.executor_id = executor_id
        self.destination = destination
        self.ssh_executable = ssh_executable

    def __call__(
        self,
        method: str,
        url: str,
        *,
        body: bytes | None,
        headers: Mapping[str, str],
        timeout: float,
    ) -> HttpResult:
        envelope = {
            "method": method,
            "url": url,
            "body": base64.b64encode(body or b"").decode("ascii"),
            "has_body": body is not None,
            "headers": dict(headers),
            "timeout": timeout,
        }
        encoded = base64.b64encode(canonical_bytes(envelope)).decode("ascii")
        program = f"""\
import base64, gzip, hashlib, json, sys, time, urllib.error, urllib.request
request_data = json.loads(base64.b64decode({encoded!r}))
body = base64.b64decode(request_data["body"]) if request_data["has_body"] else None
request_headers = dict(request_data["headers"])
request_headers.setdefault("Accept-Encoding", "gzip")
request = urllib.request.Request(
    request_data["url"],
    data=body,
    method=request_data["method"],
    headers=request_headers,
)
started = time.monotonic()
try:
    with urllib.request.urlopen(request, timeout=float(request_data["timeout"])) as response:
        status = int(response.status)
        headers = dict(response.headers.items())
        raw = response.read()
except urllib.error.HTTPError as error:
    status = int(error.code)
    headers = dict(error.headers.items()) if error.headers else {{}}
    raw = error.read()
content_encoding = next(
    (value for key, value in headers.items() if key.lower() == "content-encoding"),
    "",
)
if "gzip" in content_encoding.lower():
    raw = gzip.decompress(raw)
meta = {{
    "status": status,
    "headers": headers,
    "elapsed_seconds": time.monotonic() - started,
    "response_bytes": len(raw),
    "response_sha256": hashlib.sha256(raw).hexdigest(),
}}
sys.stdout.buffer.write(b"\\n__OM_HTTP_META__" + json.dumps(meta, separators=(",", ":")).encode() + b"\\n" + raw)
"""
        started = time.monotonic()
        try:
            completed = subprocess.run(
                [
                    self.ssh_executable,
                    "-T",
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=15",
                    self.destination,
                    "python3",
                    "-",
                ],
                input=program.encode("utf-8"),
                capture_output=True,
                check=False,
                timeout=timeout + 45,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise HttpRequestError(
                f"SSH executor {self.executor_id} transport failure: "
                f"{type(exc).__name__}: {exc}"
            ) from exc
        if completed.returncode != 0:
            stderr = completed.stderr[-1000:].decode("utf-8", errors="replace")
            raise HttpRequestError(
                f"SSH executor {self.executor_id} failed with exit "
                f"{completed.returncode}: {stderr}"
            )
        marker = completed.stdout.rfind(b"\n" + _SSH_HTTP_MARKER)
        if marker < 0:
            raise HttpRequestError(
                f"SSH executor {self.executor_id} returned no HTTP metadata marker"
            )
        header_start = marker + 1 + len(_SSH_HTTP_MARKER)
        header_end = completed.stdout.find(b"\n", header_start)
        if header_end < 0:
            raise HttpRequestError(
                f"SSH executor {self.executor_id} returned truncated HTTP metadata"
            )
        try:
            meta = json.loads(completed.stdout[header_start:header_end])
        except json.JSONDecodeError as exc:
            raise HttpRequestError(
                f"SSH executor {self.executor_id} returned invalid HTTP metadata"
            ) from exc
        raw = completed.stdout[header_end + 1 :]
        if (
            int(meta.get("response_bytes", -1)) != len(raw)
            or meta.get("response_sha256") != sha256_bytes(raw)
        ):
            raise HttpRequestError(
                f"SSH executor {self.executor_id} response framing/hash mismatch"
            )
        return HttpResult(
            status=int(meta["status"]),
            raw=raw,
            headers=_safe_headers(meta.get("headers")),
            elapsed_seconds=float(meta.get("elapsed_seconds", time.monotonic() - started)),
            executor_id=self.executor_id,
        )


def bind_executor(
    executor_id: str,
    requester: Callable[..., HttpResult],
) -> Callable[..., HttpResult]:
    """Label a direct requester and reject responses attributed elsewhere."""

    def bound(
        method: str,
        url: str,
        *,
        body: bytes | None,
        headers: Mapping[str, str],
        timeout: float,
    ) -> HttpResult:
        try:
            result = requester(
                method,
                url,
                body=body,
                headers=headers,
                timeout=timeout,
            )
        except HttpRequestError as exc:
            if exc.result is None:
                raise
            if exc.result.executor_id not in (None, executor_id):
                raise ValidationError("HTTP failure was attributed to the wrong executor") from exc
            labeled = HttpResult(
                status=exc.result.status,
                raw=exc.result.raw,
                headers=exc.result.headers,
                elapsed_seconds=exc.result.elapsed_seconds,
                executor_id=executor_id,
            )
            raise HttpRequestError(str(exc), labeled) from exc
        if result.executor_id not in (None, executor_id):
            raise ValidationError("HTTP response was attributed to the wrong executor")
        return HttpResult(
            status=result.status,
            raw=result.raw,
            headers=result.headers,
            elapsed_seconds=result.elapsed_seconds,
            executor_id=executor_id,
        )

    return bound


def _retry_delay(result: HttpResult, retry_index: int) -> float:
    retry_after = result.headers.get("retry-after")
    if retry_after:
        try:
            return max(0.0, float(retry_after))
        except ValueError:
            try:
                parsed = dt.datetime.strptime(retry_after, "%a, %d %b %Y %H:%M:%S GMT").replace(
                    tzinfo=dt.timezone.utc
                )
                return max(0.0, (parsed - dt.datetime.now(dt.timezone.utc)).total_seconds())
            except ValueError:
                pass
    return min(60.0, 2.0**retry_index)


def _response_json(raw: bytes, description: str) -> Any:
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValidationError(f"{description} is not valid JSON: {exc}") from exc
    _assert_standard_json(value)
    return value


def normalize_response_rows(value: Any, expected_count: int) -> list[dict[str, Any]]:
    if isinstance(value, dict) and isinstance(value.get("points"), list):
        rows = value["points"]
    elif isinstance(value, list):
        rows = value
    elif isinstance(value, dict) and expected_count == 1:
        rows = [value]
    else:
        raise ValidationError(f"unexpected response shape for {expected_count} point(s)")
    if len(rows) != expected_count or any(not isinstance(row, dict) for row in rows):
        raise ValidationError(f"response contains {len(rows)} point(s), expected {expected_count}")
    for row in rows:
        if row.get("error") is True:
            raise ValidationError(f"API returned an error row: {row}")
    return rows


def validate_response_rows(
    rows: list[dict[str, Any]],
    plan: Mapping[str, Any],
    *,
    official_multi: bool,
) -> None:
    periods = (("hourly", plan["hourly"]), ("daily", plan["daily"]))
    for row_index, row in enumerate(rows):
        if official_multi and "location_id" in row:
            location_id = row["location_id"]
            if type(location_id) is not int or location_id != row_index:
                raise ValidationError(
                    f"official location_id mismatch at row {row_index}: {location_id!r}"
                )
        for period, contract in periods:
            values = row.get(period)
            units = row.get(f"{period}_units")
            if not isinstance(values, dict) or not isinstance(units, dict):
                raise ValidationError(f"row {row_index} has no valid {period}/{period}_units")
            expected_keys = {"time", *contract["variables"]}
            if set(values) != expected_keys or set(units) != expected_keys:
                raise ValidationError(
                    f"row {row_index} {period} key set differs from the request contract"
                )
            if values["time"] != contract["time"]:
                raise ValidationError(f"row {row_index} {period} time axis mismatch")
            frames = len(contract["time"])
            for variable in contract["variables"]:
                series = values[variable]
                if not isinstance(series, list) or len(series) != frames:
                    raise ValidationError(
                        f"row {row_index} {period}.{variable} must contain {frames} frames"
                    )


def _official_request_identity(
    endpoint: str,
    payload: Mapping[str, Any],
) -> tuple[bytes, dict[str, Any], str]:
    payload_raw = canonical_bytes(payload)
    identity = {
        "method": "POST",
        "endpoint": normalize_endpoint(endpoint),
        "content_type": "application/json",
        "payload_sha256": sha256_bytes(payload_raw),
    }
    return payload_raw, identity, sha256_bytes(canonical_bytes(identity))


def _probe_identity(value: Any, expected_run: str | None) -> dict[str, int]:
    if not isinstance(value, dict):
        raise ValidationError("ECMWF live source probe must contain a JSON object")
    keys = (
        "last_run_initialisation_time",
        "last_run_modification_time",
        "last_run_availability_time",
        "data_end_time",
    )
    identity: dict[str, int] = {}
    for key in keys:
        item = value.get(key)
        if type(item) is not int:
            raise ValidationError(f"ECMWF live source probe has no integer {key}")
        identity[key] = item
    if expected_run is None:
        return identity
    run_epoch = int(parse_run(expected_run).timestamp())
    if identity["last_run_initialisation_time"] != run_epoch:
        actual = dt.datetime.fromtimestamp(
            identity["last_run_initialisation_time"], tz=dt.timezone.utc
        ).isoformat()
        raise ValidationError(
            f"live Open-Meteo ECMWF run is {actual}, not the frozen 00Z run {expected_run}"
        )
    expected_end = run_epoch + FORECAST_HOUR_END * 3600
    if identity["data_end_time"] < expected_end:
        raise ValidationError(
            "live Open-Meteo ECMWF metadata does not cover forecast hour 360"
        )
    return identity


def _probe_paths(cache_dir: Path, label: str) -> tuple[Path, Path]:
    base = cache_dir / "official" / "source_probes"
    return base / f"{label}.json", base / f"{label}.meta.json"


def _read_source_probe(
    cache_dir: Path,
    label: str,
    probe_endpoint: str,
    expected_run: str | None,
) -> tuple[dict[str, Any], dict[str, int], bytes, dict[str, Any]]:
    raw_path, meta_path = _probe_paths(cache_dir, label)
    if not raw_path.is_file() or not meta_path.is_file():
        raise ValidationError(f"incomplete ECMWF source probe artifact: {label}")
    raw = raw_path.read_bytes()
    meta = read_json(meta_path)
    value = _response_json(raw, f"ECMWF source probe {label}")
    identity = _probe_identity(value, expected_run)
    request = {
        "method": "GET",
        "endpoint": normalize_endpoint(probe_endpoint),
    }
    request_hash = sha256_bytes(canonical_bytes(request))
    expected = {
        "schema_version": SCHEMA_VERSION,
        "type": "ecmwf_live_source_probe",
        "label": label,
        "request": request,
        "request_sha256": request_hash,
        "http_status": 200,
        "response_bytes": len(raw),
        "response_sha256": sha256_bytes(raw),
        "identity": identity,
    }
    if not isinstance(meta, dict) or any(meta.get(key) != item for key, item in expected.items()):
        raise ValidationError(f"ECMWF source probe metadata mismatch: {label}")
    return value, identity, raw, meta


def _capture_source_probe(
    *,
    cache_dir: Path,
    label: str,
    probe_endpoint: str,
    expected_run: str | None,
    timeout: float,
    requester: Callable[..., HttpResult],
) -> tuple[dict[str, Any], dict[str, int], bytes, dict[str, Any]]:
    probe_endpoint = normalize_endpoint(probe_endpoint)
    result = requester(
        "GET",
        probe_endpoint,
        body=None,
        headers={"Accept": "application/json", "User-Agent": USER_AGENT},
        timeout=timeout,
    )
    if result.status != 200:
        summary = result.raw[:512].decode("utf-8", errors="replace")
        raise HttpRequestError(
            f"ECMWF source probe {label} failed with HTTP {result.status}: {summary}",
            result,
        )
    value = _response_json(result.raw, f"ECMWF source probe {label}")
    identity = _probe_identity(value, expected_run)
    request = {"method": "GET", "endpoint": probe_endpoint}
    meta = {
        "schema_version": SCHEMA_VERSION,
        "type": "ecmwf_live_source_probe",
        "label": label,
        "captured_at": utc_now(),
        "request": request,
        "request_sha256": sha256_bytes(canonical_bytes(request)),
        "http_status": result.status,
        "response_headers": result.headers,
        "response_bytes": len(result.raw),
        "response_sha256": sha256_bytes(result.raw),
        "elapsed_seconds": round(result.elapsed_seconds, 6),
        "identity": identity,
    }
    raw_path, meta_path = _probe_paths(cache_dir, label)
    atomic_commit_pair(raw_path, result.raw, meta_path, pretty_bytes(meta))
    return _read_source_probe(cache_dir, label, probe_endpoint, expected_run)


def _next_resume_probe_label(cache_dir: Path, prefix: str = "resume_guard") -> str:
    base = cache_dir / "official" / "source_probes"
    existing = list(base.glob(f"{prefix}_*.meta.json")) if base.exists() else []
    return f"{prefix}_{len(existing):04d}"


def _spatial_probe_identity(value: Any, expected_run: str | None) -> dict[str, Any]:
    if not isinstance(value, dict) or value.get("completed") is not True:
        raise ValidationError("ECMWF spatial latest probe is not a completed catalog")
    reference = value.get("reference_time")
    try:
        normalized_reference = format_run(_parse_run_instant(str(reference)))
    except ValidationError as exc:
        raise ValidationError("ECMWF spatial latest probe has no valid 00Z reference_time") from exc
    if expected_run is not None and normalized_reference != expected_run:
        raise ValidationError(
            f"ECMWF spatial source run {normalized_reference} != frozen run {expected_run}"
        )
    valid_times = value.get("valid_times")
    variables = value.get("variables")
    if (
        not isinstance(valid_times, list)
        or not valid_times
        or not isinstance(variables, list)
        or not variables
        or any(not isinstance(item, str) for item in variables)
    ):
        raise ValidationError("ECMWF spatial latest probe lacks valid_times/variables")
    if expected_run is not None:
        expected_end = (
            parse_run(expected_run) + dt.timedelta(hours=FORECAST_HOUR_END)
        ).strftime("%Y-%m-%dT%H:%MZ")
        if expected_end not in valid_times:
            raise ValidationError("ECMWF spatial source does not include forecast hour 360")
    return {
        "reference_time": normalized_reference,
        "last_modified_time": value.get("last_modified_time"),
        "valid_times_sha256": sha256_bytes(canonical_bytes(valid_times)),
        "variables_sha256": sha256_bytes(canonical_bytes(sorted(set(variables)))),
        "valid_time_count": len(valid_times),
        "variable_count": len(set(variables)),
    }


def _read_spatial_probe(
    cache_dir: Path,
    label: str,
    endpoint: str,
    expected_run: str | None,
) -> tuple[dict[str, Any], dict[str, Any], bytes, dict[str, Any]]:
    raw_path, meta_path = _probe_paths(cache_dir, label)
    if not raw_path.is_file() or not meta_path.is_file():
        raise ValidationError(f"incomplete ECMWF spatial probe artifact: {label}")
    raw = raw_path.read_bytes()
    meta = read_json(meta_path)
    value = _response_json(raw, f"ECMWF spatial probe {label}")
    identity = _spatial_probe_identity(value, expected_run)
    request = {"method": "GET", "endpoint": normalize_endpoint(endpoint)}
    expected = {
        "schema_version": SCHEMA_VERSION,
        "type": "ecmwf_spatial_source_probe",
        "label": label,
        "request": request,
        "request_sha256": sha256_bytes(canonical_bytes(request)),
        "http_status": 200,
        "response_bytes": len(raw),
        "response_sha256": sha256_bytes(raw),
        "identity": identity,
    }
    if not isinstance(meta, dict) or any(meta.get(key) != item for key, item in expected.items()):
        raise ValidationError(f"ECMWF spatial probe metadata mismatch: {label}")
    return value, identity, raw, meta


def _capture_spatial_probe(
    *,
    cache_dir: Path,
    label: str,
    endpoint: str,
    expected_run: str | None,
    timeout: float,
    requester: Callable[..., HttpResult],
) -> tuple[dict[str, Any], dict[str, Any], bytes, dict[str, Any]]:
    endpoint = normalize_endpoint(endpoint)
    result = requester(
        "GET",
        endpoint,
        body=None,
        headers={"Accept": "application/json", "User-Agent": USER_AGENT},
        timeout=timeout,
    )
    if result.status != 200:
        raise HttpRequestError(
            f"ECMWF spatial probe {label} failed with HTTP {result.status}", result
        )
    value = _response_json(result.raw, f"ECMWF spatial probe {label}")
    identity = _spatial_probe_identity(value, expected_run)
    request = {"method": "GET", "endpoint": endpoint}
    meta = {
        "schema_version": SCHEMA_VERSION,
        "type": "ecmwf_spatial_source_probe",
        "label": label,
        "captured_at": utc_now(),
        "request": request,
        "request_sha256": sha256_bytes(canonical_bytes(request)),
        "http_status": 200,
        "response_headers": result.headers,
        "response_bytes": len(result.raw),
        "response_sha256": sha256_bytes(result.raw),
        "elapsed_seconds": round(result.elapsed_seconds, 6),
        "identity": identity,
    }
    raw_path, meta_path = _probe_paths(cache_dir, label)
    atomic_commit_pair(raw_path, result.raw, meta_path, pretty_bytes(meta))
    return _read_spatial_probe(cache_dir, label, endpoint, expected_run)


def _cache_paths(cache_dir: Path, batch_id: str) -> tuple[Path, Path]:
    base = cache_dir / "official"
    return base / f"{batch_id}.json", base / f"{batch_id}.meta.json"


def _cache_request_path(cache_dir: Path, batch_id: str) -> Path:
    return cache_dir / "official" / f"{batch_id}.request.json"


def _stable_row(value: Mapping[str, Any]) -> dict[str, Any]:
    return {key: item for key, item in value.items() if key not in IGNORED_DYNAMIC_METADATA}


def _sentinel_sha256(rows: list[dict[str, Any]]) -> str:
    if not rows:
        raise ValidationError("official response has no sentinel row")
    return sha256_bytes(canonical_bytes(_stable_row(rows[0])))


def _verify_cached_batch(
    cache_dir: Path,
    batch: Mapping[str, Any],
    endpoint: str,
    payload: Mapping[str, Any],
    plan: Mapping[str, Any],
    expected_source_identity: Mapping[str, int] | None = None,
) -> tuple[bytes, dict[str, Any], list[dict[str, Any]]] | None:
    raw_path, meta_path = _cache_paths(cache_dir, str(batch["batch_id"]))
    request_path = _cache_request_path(cache_dir, str(batch["batch_id"]))
    if not raw_path.exists() and not meta_path.exists():
        return None
    if not raw_path.is_file() or not meta_path.is_file():
        raise ValidationError(f"incomplete immutable official cache pair for {batch['batch_id']}")
    raw = raw_path.read_bytes()
    meta = read_json(meta_path)
    if not isinstance(meta, dict):
        raise ValidationError(f"invalid cache metadata for {batch['batch_id']}")
    payload_raw, identity, request_hash = _official_request_identity(endpoint, payload)
    expected = {
        "schema_version": SCHEMA_VERSION,
        "type": "official_ecmwf_post_response",
        "model": MODEL,
        "run": plan["run"],
        "batch_id": batch["batch_id"],
        "profile": batch["profile"],
        "point_ids": batch["point_ids"],
        "request_point_ids": batch["request_point_ids"],
        "point_count": len(batch["point_ids"]),
        "response_point_count": len(batch["request_point_ids"]),
        "sentinel_point_id": batch["sentinel_point_id"],
        "request": identity,
        "request_sha256": request_hash,
        "request_payload_sha256": sha256_bytes(payload_raw),
        "request_payload_file": request_path.relative_to(cache_dir).as_posix(),
        "http_status": 200,
        "response_bytes": len(raw),
        "response_sha256": sha256_bytes(raw),
    }
    if "executor_id" in batch:
        expected["executor_id"] = batch["executor_id"]
    mismatches = {
        key: {"expected": value, "actual": meta.get(key)}
        for key, value in expected.items()
        if meta.get(key) != value
    }
    if mismatches:
        raise ValidationError(f"official cache metadata mismatch for {batch['batch_id']}: {mismatches}")
    if not request_path.is_file() or request_path.read_bytes() != payload_raw:
        raise ValidationError(
            f"official raw request-body artifact mismatch for {batch['batch_id']}"
        )
    if expected_source_identity is not None and meta.get("source_identity") != dict(
        expected_source_identity
    ):
        raise ValidationError(
            f"official cache source identity mismatch for {batch['batch_id']}; use a new cache"
        )
    rows = normalize_response_rows(
        _response_json(raw, f"official cache {batch['batch_id']}"),
        len(batch["request_point_ids"]),
    )
    validate_response_rows(rows, plan, official_multi=True)
    if meta.get("sentinel_sha256") != _sentinel_sha256(rows):
        raise ValidationError(f"official sentinel hash mismatch for {batch['batch_id']}")
    return raw, meta, rows


def _failure_attempt_paths(cache_dir: Path, batch_id: str, attempt_index: int) -> tuple[Path, Path]:
    base = cache_dir / "official" / "failed_attempts"
    stem = f"{batch_id}.attempt{attempt_index:04d}"
    return base / f"{stem}.response", base / f"{stem}.meta.json"


def _existing_failure_attempts(
    cache_dir: Path,
    batch_id: str,
    request_sha256: str,
) -> list[dict[str, Any]]:
    base = cache_dir / "official" / "failed_attempts"
    if not base.exists():
        return []
    result: list[dict[str, Any]] = []
    for meta_path in sorted(base.glob(f"{batch_id}.attempt*.meta.json")):
        meta = read_json(meta_path)
        if not isinstance(meta, dict) or meta.get("request_sha256") != request_sha256:
            raise ValidationError(f"failed-attempt request identity mismatch: {meta_path}")
        request_file = meta.get("request_payload_file")
        request_path = cache_dir / str(request_file)
        if (
            not request_file
            or not request_path.is_file()
            or sha256_file(request_path) != meta.get("request_payload_sha256")
        ):
            raise ValidationError(f"failed-attempt request body is missing: {meta_path}")
        response_file = meta.get("response_file")
        if response_file is not None:
            response_path = cache_dir / str(response_file)
            if not response_path.is_file():
                raise ValidationError(f"failed-attempt response is missing: {response_path}")
            raw = response_path.read_bytes()
            if meta.get("response_sha256") != sha256_bytes(raw):
                raise ValidationError(f"failed-attempt response hash mismatch: {response_path}")
        result.append(meta)
    return result


def _persist_failed_attempt(
    cache_dir: Path,
    *,
    batch_id: str,
    attempt_index: int,
    request_identity: Mapping[str, Any],
    request_sha256: str,
    result: HttpResult | None,
    error: str | None,
    retry_decision: str,
    executor_id: str | None,
) -> dict[str, Any]:
    response_path, meta_path = _failure_attempt_paths(cache_dir, batch_id, attempt_index)
    response_relative: str | None = None
    response_hash: str | None = None
    if result is not None:
        response_relative = response_path.relative_to(cache_dir).as_posix()
        response_hash = sha256_bytes(result.raw)
        write_immutable_bytes(response_path, result.raw)
    meta = {
        "schema_version": SCHEMA_VERSION,
        "type": "official_ecmwf_failed_http_attempt",
        "batch_id": batch_id,
        "attempt_index": attempt_index,
        "attempted_at": utc_now(),
        "request": dict(request_identity),
        "request_sha256": request_sha256,
        "request_payload_file": _cache_request_path(cache_dir, batch_id)
        .relative_to(cache_dir)
        .as_posix(),
        "request_payload_sha256": request_identity["payload_sha256"],
        "http_status": result.status if result is not None else None,
        "response_file": response_relative,
        "response_bytes": len(result.raw) if result is not None else 0,
        "response_sha256": response_hash,
        "response_headers": result.headers if result is not None else {},
        "elapsed_seconds": round(result.elapsed_seconds, 6) if result is not None else None,
        "executor_id": executor_id,
        "transport_error": error,
        "retry_decision": retry_decision,
    }
    write_immutable_bytes(meta_path, pretty_bytes(meta))
    return meta


def _fetch_one_official_batch(
    *,
    plan: Mapping[str, Any],
    batch: Mapping[str, Any],
    points: list[dict[str, Any]],
    cache_dir: Path,
    endpoint: str,
    api_key: str | None,
    access_profile: str,
    source_identity: Mapping[str, int],
    source_probe_before_sha256: str,
    timeout: float,
    retries: int,
    consume_network_budget: Callable[[], None],
    requester: Callable[..., HttpResult],
    sleeper: Callable[[float], None],
) -> tuple[bytes, dict[str, Any], list[dict[str, Any]]]:
    payload = official_payload(plan, points)
    cached = _verify_cached_batch(
        cache_dir,
        batch,
        endpoint,
        payload,
        plan,
        expected_source_identity=source_identity,
    )
    if cached is not None:
        return cached
    payload_raw, request_identity, request_hash = _official_request_identity(endpoint, payload)
    request_path = _cache_request_path(cache_dir, str(batch["batch_id"]))
    write_immutable_bytes(request_path, payload_raw)
    prior_failures = _existing_failure_attempts(cache_dir, str(batch["batch_id"]), request_hash)
    failure_index = len(prior_failures)
    retry_index = 0
    while True:
        consume_network_budget()
        headers = {
            "Accept": "application/json",
            "Content-Type": "application/json",
            "User-Agent": USER_AGENT,
        }
        if api_key:
            headers["X-Api-Key"] = api_key
        try:
            result = requester(
                "POST",
                endpoint,
                body=payload_raw,
                headers=headers,
                timeout=timeout,
            )
        except HttpRequestError as exc:
            if exc.result is not None and "executor_id" in batch:
                if exc.result.executor_id != batch["executor_id"]:
                    raise ValidationError("failed HTTP result came from the wrong executor") from exc
            retry = retry_index < retries
            _persist_failed_attempt(
                cache_dir,
                batch_id=str(batch["batch_id"]),
                attempt_index=failure_index,
                request_identity=request_identity,
                request_sha256=request_hash,
                result=exc.result,
                error=str(exc),
                retry_decision="retry" if retry else "stop",
                executor_id=batch.get("executor_id"),
            )
            failure_index += 1
            if not retry:
                raise
            sleeper(min(60.0, 2.0**retry_index))
            retry_index += 1
            continue
        if result.status == 200:
            if "executor_id" in batch and result.executor_id != batch["executor_id"]:
                raise ValidationError("official response came from the wrong executor")
            rows = normalize_response_rows(
                _response_json(result.raw, f"official response {batch['batch_id']}"),
                len(points),
            )
            validate_response_rows(rows, plan, official_multi=True)
            all_failures = _existing_failure_attempts(
                cache_dir, str(batch["batch_id"]), request_hash
            )
            meta = {
                "schema_version": SCHEMA_VERSION,
                "type": "official_ecmwf_post_response",
                "model": MODEL,
                "run": plan["run"],
                "batch_id": batch["batch_id"],
                "profile": batch["profile"],
                "point_ids": batch["point_ids"],
                "request_point_ids": batch["request_point_ids"],
                "point_count": len(batch["point_ids"]),
                "response_point_count": len(points),
                "captured_at": utc_now(),
                "access_profile": access_profile,
                "executor_id": result.executor_id,
                "source_identity": dict(source_identity),
                "source_probe_before_sha256": source_probe_before_sha256,
                "request": request_identity,
                "request_sha256": request_hash,
                "request_payload_sha256": sha256_bytes(payload_raw),
                "request_payload_file": request_path.relative_to(cache_dir).as_posix(),
                "api_key_header_supplied": bool(api_key),
                "http_status": result.status,
                "response_headers": result.headers,
                "response_bytes": len(result.raw),
                "response_sha256": sha256_bytes(result.raw),
                "sentinel_point_id": batch["sentinel_point_id"],
                "sentinel_sha256": _sentinel_sha256(rows),
                "elapsed_seconds": round(result.elapsed_seconds, 6),
                "failed_attempts_before_success": [
                    {
                        "attempt_index": item["attempt_index"],
                        "http_status": item["http_status"],
                        "response_sha256": item["response_sha256"],
                        "transport_error": item["transport_error"],
                        "retry_decision": item["retry_decision"],
                    }
                    for item in all_failures
                ],
            }
            raw_path, meta_path = _cache_paths(cache_dir, str(batch["batch_id"]))
            atomic_commit_pair(raw_path, result.raw, meta_path, pretty_bytes(meta))
            verified = _verify_cached_batch(
                cache_dir,
                batch,
                endpoint,
                payload,
                plan,
                expected_source_identity=source_identity,
            )
            assert verified is not None
            return verified
        retry = (result.status == 429 or result.status >= 500) and retry_index < retries
        if "executor_id" in batch and result.executor_id != batch["executor_id"]:
            raise ValidationError("failed HTTP result came from the wrong executor")
        _persist_failed_attempt(
            cache_dir,
            batch_id=str(batch["batch_id"]),
            attempt_index=failure_index,
            request_identity=request_identity,
            request_sha256=request_hash,
            result=result,
            error=None,
            retry_decision="retry" if retry else "stop",
            executor_id=batch.get("executor_id"),
        )
        failure_index += 1
        if not retry:
            summary = result.raw[:512].decode("utf-8", errors="replace")
            raise HttpRequestError(
                f"official POST failed with HTTP {result.status} for {batch['batch_id']}: {summary}",
                result,
            )
        delay = _retry_delay(result, retry_index)
        # A 429 is never bypassed by switching hosts/IPs. Retry-After is obeyed.
        sleeper(delay)
        retry_index += 1


def _http_header_datetime(
    headers: Mapping[str, Any] | None,
    name: str,
    context: str,
) -> dt.datetime:
    value = headers.get(name) if isinstance(headers, Mapping) else None
    if not isinstance(value, str) or not value.strip():
        raise ValidationError(f"{context} has no HTTP {name} header")
    try:
        parsed = parsedate_to_datetime(value)
    except (TypeError, ValueError) as exc:
        raise ValidationError(f"{context} has an invalid HTTP {name} header") from exc
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed.astimezone(dt.timezone.utc)


def _http_datetime_text(value: dt.datetime) -> str:
    return value.astimezone(dt.timezone.utc).isoformat().replace("+00:00", "Z")


def _post_capture_transition_proof(
    *,
    cache_dir: Path,
    batches: list[dict[str, Any]],
    target_run: str,
    source_identity: Mapping[str, int],
    source_transition_identity: Mapping[str, int],
    source_transition_meta: Mapping[str, Any],
    source_transition_sha256: str,
    spatial_identity: Mapping[str, Any],
    spatial_transition_identity: Mapping[str, Any],
    spatial_transition_meta: Mapping[str, Any],
    spatial_transition_sha256: str,
) -> dict[str, Any]:
    target_epoch = int(parse_run(target_run).timestamp())
    if source_identity.get("last_run_initialisation_time") != target_epoch:
        raise ValidationError("transition proof source-before identity is not the target run")
    source_transition_epoch = source_transition_identity.get(
        "last_run_initialisation_time"
    )
    if type(source_transition_epoch) is not int or source_transition_epoch <= target_epoch:
        raise ValidationError("temporal source did not transition to a newer ECMWF run")
    if spatial_identity.get("reference_time") != target_run:
        raise ValidationError("transition proof spatial-before identity is not the target run")
    spatial_transition_run = spatial_transition_identity.get("reference_time")
    if (
        not isinstance(spatial_transition_run, str)
        or _parse_run_instant(spatial_transition_run) <= parse_run(target_run)
    ):
        raise ValidationError("spatial source did not transition to a newer ECMWF run")

    source_boundary = _http_header_datetime(
        source_transition_meta.get("response_headers"),
        "last-modified",
        "temporal transition probe",
    )
    spatial_boundary = _http_header_datetime(
        spatial_transition_meta.get("response_headers"),
        "last-modified",
        "spatial transition probe",
    )
    batch_dates: dict[str, str] = {}
    sentinel_by_profile: dict[str, str] = {}
    latest_batch_date: dt.datetime | None = None
    for batch in batches:
        _raw_path, meta_path = _cache_paths(cache_dir, str(batch["batch_id"]))
        meta = read_json(meta_path)
        if not isinstance(meta, dict):
            raise ValidationError(
                f"official batch metadata is invalid: {batch['batch_id']}"
            )
        captured = _http_header_datetime(
            meta.get("response_headers"),
            "date",
            f"official batch {batch['batch_id']}",
        )
        batch_dates[str(batch["batch_id"])] = _http_datetime_text(captured)
        latest_batch_date = (
            captured if latest_batch_date is None else max(latest_batch_date, captured)
        )
        profile = str(batch["profile"])
        sentinel = meta.get("sentinel_sha256")
        if not isinstance(sentinel, str):
            raise ValidationError(
                f"official batch has no sentinel proof: {batch['batch_id']}"
            )
        previous = sentinel_by_profile.setdefault(profile, sentinel)
        if previous != sentinel:
            raise ValidationError(
                f"official live sentinel changed between {profile} batches"
            )
    if latest_batch_date is None:
        raise ValidationError("transition proof has no official batch HTTP dates")
    if latest_batch_date >= source_boundary:
        raise ValidationError(
            "an official batch HTTP Date is not earlier than the temporal source transition"
        )
    if latest_batch_date >= spatial_boundary:
        raise ValidationError(
            "an official batch HTTP Date is not earlier than the spatial source transition"
        )
    return _with_self_hash(
        {
            "schema_version": SCHEMA_VERSION,
            "type": "ecmwf_post_capture_transition_proof",
            "acceptance_mode": "explicit_post_capture_transition",
            "target_run": target_run,
            "batch_http_dates": batch_dates,
            "max_batch_http_date": _http_datetime_text(latest_batch_date),
            "sentinel_sha256_by_profile": sentinel_by_profile,
            "source_before_identity": dict(source_identity),
            "source_transition_identity": dict(source_transition_identity),
            "source_transition_last_modified": _http_datetime_text(source_boundary),
            "source_transition_probe_sha256": source_transition_sha256,
            "spatial_before_identity": dict(spatial_identity),
            "spatial_transition_identity": dict(spatial_transition_identity),
            "spatial_transition_last_modified": _http_datetime_text(spatial_boundary),
            "spatial_transition_probe_sha256": spatial_transition_sha256,
        },
        "proof_sha256",
    )


def _validate_official_index(
    index: Mapping[str, Any],
    *,
    plan: Mapping[str, Any],
    plan_path: Path,
    endpoint: str,
    access_profile: str,
    batches: list[dict[str, Any]],
    source_identity: Mapping[str, int],
    source_probe_before_sha256: str,
    source_probe_after_sha256: str,
    spatial_identity: Mapping[str, Any],
    spatial_probe_before_sha256: str,
    spatial_probe_after_sha256: str,
    capture_mode: str = "same_source",
    source_transition_identity: Mapping[str, int] | None = None,
    spatial_transition_identity: Mapping[str, Any] | None = None,
    post_capture_transition_proof: Mapping[str, Any] | None = None,
) -> None:
    _verify_self_hash(index, "index_sha256", "official cache index")
    expected = {
        "schema_version": SCHEMA_VERSION,
        "type": "official_ecmwf_cache_index",
        "model": MODEL,
        "run": plan["run"],
        "plan_sha256": sha256_file(plan_path),
        "endpoint": normalize_endpoint(endpoint),
        "method": "POST",
        "access_profile": access_profile,
        "point_count": POINT_COUNT,
        "successful_request_count": len(batches),
        "theoretical_minimum_successful_requests": len(batches),
        "source_identity": dict(source_identity),
        "source_probe_before_sha256": source_probe_before_sha256,
        "source_probe_after_sha256": source_probe_after_sha256,
        "spatial_identity": dict(spatial_identity),
        "spatial_probe_before_sha256": spatial_probe_before_sha256,
        "spatial_probe_after_sha256": spatial_probe_after_sha256,
        "capture_mode": capture_mode,
    }
    if capture_mode == "post_capture_transition":
        if (
            source_transition_identity is None
            or spatial_transition_identity is None
            or post_capture_transition_proof is None
        ):
            raise ValidationError("official cache index has incomplete transition evidence")
        _verify_self_hash(
            post_capture_transition_proof,
            "proof_sha256",
            "post-capture transition proof",
        )
        expected.update(
            {
                "source_transition_identity": dict(source_transition_identity),
                "spatial_transition_identity": dict(spatial_transition_identity),
                "post_capture_transition_proof": dict(
                    post_capture_transition_proof
                ),
            }
        )
    elif capture_mode != "same_source":
        raise ValidationError(f"unsupported official capture mode: {capture_mode}")
    executor_batch_ids, executor_weights = public_executor_summary(batches)
    if executor_batch_ids:
        expected["public_executor_batch_ids"] = executor_batch_ids
        expected["public_executor_estimated_weight"] = executor_weights
    mismatches = {
        key: {"expected": value, "actual": index.get(key)}
        for key, value in expected.items()
        if index.get(key) != value
    }
    if mismatches:
        raise ValidationError(f"official cache index mismatch: {mismatches}")
    entries = index.get("entries")
    if not isinstance(entries, list) or [entry.get("batch_id") for entry in entries] != [
        batch["batch_id"] for batch in batches
    ]:
        raise ValidationError("official cache index batch order/coverage mismatch")
    sentinel_by_profile: dict[str, str] = {}
    for entry in entries:
        profile = entry.get("profile")
        sentinel_hash = entry.get("sentinel_sha256")
        if not isinstance(profile, str) or not isinstance(sentinel_hash, str):
            raise ValidationError("official cache index has no sentinel proof")
        previous = sentinel_by_profile.setdefault(profile, sentinel_hash)
        if previous != sentinel_hash:
            raise ValidationError(
                f"official live sentinel changed between {profile} batches"
            )
    if index.get("sentinel_sha256_by_profile") != sentinel_by_profile:
        raise ValidationError("official cache index sentinel attestation mismatch")


def fetch_official(
    plan: dict[str, Any],
    plan_path: Path,
    config: dict[str, Any],
    cache_dir: Path,
    endpoint: str,
    allow_network: bool,
    max_new_requests: int,
    delay_seconds: float,
    timeout: float,
    retries: int,
    api_key: str | None = None,
    access_profile: str = "customer",
    *,
    requester: Callable[..., HttpResult] = _request_once,
    public_executor_requesters: Mapping[str, Callable[..., HttpResult]] | None = None,
    sleeper: Callable[[float], None] = time.sleep,
    accept_proven_post_capture_transition: bool = False,
) -> dict[str, Any]:
    if max_new_requests < 0 or delay_seconds < 0 or timeout <= 0 or retries < 0:
        raise ValidationError("invalid official capture network settings")
    endpoint = normalize_endpoint(endpoint)
    probe_endpoint = normalize_endpoint(config["official"]["source_probe_endpoint"])
    spatial_probe_endpoint = normalize_endpoint(config["official"]["spatial_probe_endpoint"])
    batches = official_batches(plan, config, access_profile)
    estimated_total_weight = sum(float(batch["estimated_weight"]) for batch in batches)
    executor_requesters: dict[str, Callable[..., HttpResult]] = {}
    if access_profile == "public_noncommercial":
        supplied = (
            dict(public_executor_requesters)
            if public_executor_requesters is not None
            else {"local": requester}
        )
        batches = assign_public_batches(
            batches,
            supplied,
            float(config["official"].get("public_daily_weight_limit", 10000)),
        )
        executor_requesters = {
            executor_id: bind_executor(executor_id, executor_requester)
            for executor_id, executor_requester in supplied.items()
        }
    elif public_executor_requesters:
        raise ValidationError("public executors are valid only for public noncommercial capture")
    points = point_map(plan)
    missing = []
    for batch in batches:
        selected = _batch_request_points(batch, points)
        if _verify_cached_batch(
            cache_dir,
            batch,
            endpoint,
            official_payload(plan, selected),
            plan,
        ) is None:
            missing.append(batch)

    index_path = cache_dir / "official_index.json"
    before_paths = _probe_paths(cache_dir, "before")
    after_paths = _probe_paths(cache_dir, "after")
    spatial_before_paths = _probe_paths(cache_dir, "spatial_before")
    spatial_after_paths = _probe_paths(cache_dir, "spatial_after")
    if not missing and index_path.is_file():
        existing = read_json(index_path)
        if not isinstance(existing, dict):
            raise ValidationError("official cache index must contain an object")
        capture_mode = str(existing.get("capture_mode", ""))
        _before_value, before_identity, before_raw, _before_meta = _read_source_probe(
            cache_dir, "before", probe_endpoint, plan["run"]
        )
        _spatial_before_value, spatial_identity, spatial_before_raw, _spatial_before_meta = (
            _read_spatial_probe(
                cache_dir, "spatial_before", spatial_probe_endpoint, plan["run"]
            )
        )
        for batch in batches:
            selected = _batch_request_points(batch, points)
            if _verify_cached_batch(
                cache_dir,
                batch,
                endpoint,
                official_payload(plan, selected),
                plan,
                expected_source_identity=before_identity,
            ) is None:
                raise ValidationError("official cache index exists but a batch is missing")
        transition_proof: dict[str, Any] | None = None
        source_transition_identity: dict[str, int] | None = None
        spatial_transition_identity: dict[str, Any] | None = None
        if capture_mode == "same_source":
            _after_value, after_identity, after_raw, _after_meta = _read_source_probe(
                cache_dir, "after", probe_endpoint, plan["run"]
            )
            if before_identity != after_identity:
                raise ValidationError(
                    "cached ECMWF live source changed during official capture"
                )
            (
                _spatial_after_value,
                spatial_after_identity,
                spatial_after_raw,
                _spatial_after_meta,
            ) = _read_spatial_probe(
                cache_dir, "spatial_after", spatial_probe_endpoint, plan["run"]
            )
            if spatial_identity != spatial_after_identity:
                raise ValidationError(
                    "cached ECMWF spatial source changed during official capture"
                )
        elif capture_mode == "post_capture_transition":
            (
                _after_value,
                source_transition_identity,
                after_raw,
                after_meta,
            ) = _read_source_probe(
                cache_dir, "after_transition", probe_endpoint, None
            )
            (
                _spatial_after_value,
                spatial_transition_identity,
                spatial_after_raw,
                spatial_after_meta,
            ) = _read_spatial_probe(
                cache_dir, "spatial_after_transition", spatial_probe_endpoint, None
            )
            transition_proof = _post_capture_transition_proof(
                cache_dir=cache_dir,
                batches=batches,
                target_run=plan["run"],
                source_identity=before_identity,
                source_transition_identity=source_transition_identity,
                source_transition_meta=after_meta,
                source_transition_sha256=sha256_bytes(after_raw),
                spatial_identity=spatial_identity,
                spatial_transition_identity=spatial_transition_identity,
                spatial_transition_meta=spatial_after_meta,
                spatial_transition_sha256=sha256_bytes(spatial_after_raw),
            )
        else:
            raise ValidationError(
                f"unsupported official capture mode: {capture_mode}"
            )
        _validate_official_index(
            existing,
            plan=plan,
            plan_path=plan_path,
            endpoint=endpoint,
            access_profile=access_profile,
            batches=batches,
            source_identity=before_identity,
            source_probe_before_sha256=sha256_bytes(before_raw),
            source_probe_after_sha256=sha256_bytes(after_raw),
            spatial_identity=spatial_identity,
            spatial_probe_before_sha256=sha256_bytes(spatial_before_raw),
            spatial_probe_after_sha256=sha256_bytes(spatial_after_raw),
            capture_mode=capture_mode,
            source_transition_identity=source_transition_identity,
            spatial_transition_identity=spatial_transition_identity,
            post_capture_transition_proof=transition_proof,
        )
        return existing
    if not allow_network:
        raise ValidationError(
            f"official cache is incomplete (missing successful POSTs: {len(missing)}); "
            "networking is disabled by default"
        )
    if len(missing) > max_new_requests:
        raise ValidationError(
            f"at least {len(missing)} new POST(s) are needed, exceeding "
            f"--max-new-requests={max_new_requests}"
        )

    transition_finalize = (
        accept_proven_post_capture_transition
        and not missing
        and not index_path.exists()
        and all(path.is_file() for path in before_paths)
        and all(path.is_file() for path in spatial_before_paths)
    )
    if before_paths[0].exists() or before_paths[1].exists():
        _before_value, source_identity, before_raw, _before_meta = _read_source_probe(
            cache_dir, "before", probe_endpoint, plan["run"]
        )
        if not transition_finalize:
            guard_label = _next_resume_probe_label(cache_dir)
            _guard_value, guard_identity, _guard_raw, _guard_meta = _capture_source_probe(
                cache_dir=cache_dir,
                label=guard_label,
                probe_endpoint=probe_endpoint,
                expected_run=plan["run"],
                timeout=timeout,
                requester=requester,
            )
            if guard_identity != source_identity:
                raise ValidationError(
                    "live ECMWF source changed since the partial capture began; quarantine this cache"
                )
    else:
        _before_value, source_identity, before_raw, _before_meta = _capture_source_probe(
            cache_dir=cache_dir,
            label="before",
            probe_endpoint=probe_endpoint,
            expected_run=plan["run"],
            timeout=timeout,
            requester=requester,
        )
    source_probe_before_sha256 = sha256_bytes(before_raw)

    if spatial_before_paths[0].exists() or spatial_before_paths[1].exists():
        _spatial_before_value, spatial_identity, spatial_before_raw, _spatial_before_meta = (
            _read_spatial_probe(
                cache_dir, "spatial_before", spatial_probe_endpoint, plan["run"]
            )
        )
        if not transition_finalize:
            spatial_guard_label = _next_resume_probe_label(
                cache_dir, "spatial_resume_guard"
            )
            (
                _spatial_guard_value,
                spatial_guard_identity,
                _spatial_guard_raw,
                _spatial_guard_meta,
            ) = _capture_spatial_probe(
                cache_dir=cache_dir,
                label=spatial_guard_label,
                endpoint=spatial_probe_endpoint,
                expected_run=plan["run"],
                timeout=timeout,
                requester=requester,
            )
            if spatial_guard_identity != spatial_identity:
                raise ValidationError(
                    "ECMWF spatial source changed since partial capture began; quarantine this cache"
                )
    else:
        _spatial_before_value, spatial_identity, spatial_before_raw, _spatial_before_meta = (
            _capture_spatial_probe(
                cache_dir=cache_dir,
                label="spatial_before",
                endpoint=spatial_probe_endpoint,
                expected_run=plan["run"],
                timeout=timeout,
                requester=requester,
            )
        )
    spatial_probe_before_sha256 = sha256_bytes(spatial_before_raw)

    new_network_attempts = 0

    def consume_budget() -> None:
        nonlocal new_network_attempts
        if not allow_network:
            raise ValidationError("official networking is disabled")
        if new_network_attempts >= max_new_requests:
            raise ValidationError(
                f"official network-attempt budget exhausted at {new_network_attempts}; "
                "the immutable partial cache can be resumed"
            )
        new_network_attempts += 1

    captured = 0
    for batch in batches:
        selected = _batch_request_points(batch, points)
        existed = _verify_cached_batch(
            cache_dir,
            batch,
            endpoint,
            official_payload(plan, selected),
            plan,
            expected_source_identity=source_identity,
        ) is not None
        _fetch_one_official_batch(
            plan=plan,
            batch=batch,
            points=selected,
            cache_dir=cache_dir,
            endpoint=endpoint,
            api_key=api_key,
            access_profile=access_profile,
            source_identity=source_identity,
            source_probe_before_sha256=source_probe_before_sha256,
            timeout=timeout,
            retries=retries,
            consume_network_budget=consume_budget,
            requester=executor_requesters.get(str(batch.get("executor_id")), requester),
            sleeper=sleeper,
        )
        if not existed:
            captured += 1
            if captured < len(missing) and delay_seconds:
                sleeper(delay_seconds)

    capture_mode = "post_capture_transition" if transition_finalize else "same_source"
    transition_proof: dict[str, Any] | None = None
    source_transition_identity: dict[str, int] | None = None
    spatial_transition_identity: dict[str, Any] | None = None
    if transition_finalize:
        transition_paths = _probe_paths(cache_dir, "after_transition")
        if transition_paths[0].exists() or transition_paths[1].exists():
            (
                _after_value,
                source_transition_identity,
                after_raw,
                after_meta,
            ) = _read_source_probe(
                cache_dir, "after_transition", probe_endpoint, None
            )
        else:
            (
                _after_value,
                source_transition_identity,
                after_raw,
                after_meta,
            ) = _capture_source_probe(
                cache_dir=cache_dir,
                label="after_transition",
                probe_endpoint=probe_endpoint,
                expected_run=None,
                timeout=timeout,
                requester=requester,
            )
        spatial_transition_paths = _probe_paths(
            cache_dir, "spatial_after_transition"
        )
        if (
            spatial_transition_paths[0].exists()
            or spatial_transition_paths[1].exists()
        ):
            (
                _spatial_after_value,
                spatial_transition_identity,
                spatial_after_raw,
                spatial_after_meta,
            ) = _read_spatial_probe(
                cache_dir,
                "spatial_after_transition",
                spatial_probe_endpoint,
                None,
            )
        else:
            (
                _spatial_after_value,
                spatial_transition_identity,
                spatial_after_raw,
                spatial_after_meta,
            ) = _capture_spatial_probe(
                cache_dir=cache_dir,
                label="spatial_after_transition",
                endpoint=spatial_probe_endpoint,
                expected_run=None,
                timeout=timeout,
                requester=requester,
            )
        source_probe_after_sha256 = sha256_bytes(after_raw)
        spatial_probe_after_sha256 = sha256_bytes(spatial_after_raw)
        transition_proof = _post_capture_transition_proof(
            cache_dir=cache_dir,
            batches=batches,
            target_run=plan["run"],
            source_identity=source_identity,
            source_transition_identity=source_transition_identity,
            source_transition_meta=after_meta,
            source_transition_sha256=source_probe_after_sha256,
            spatial_identity=spatial_identity,
            spatial_transition_identity=spatial_transition_identity,
            spatial_transition_meta=spatial_after_meta,
            spatial_transition_sha256=spatial_probe_after_sha256,
        )
    else:
        if after_paths[0].exists() or after_paths[1].exists():
            _after_value, after_identity, after_raw, _after_meta = _read_source_probe(
                cache_dir, "after", probe_endpoint, plan["run"]
            )
        else:
            _after_value, after_identity, after_raw, _after_meta = _capture_source_probe(
                cache_dir=cache_dir,
                label="after",
                probe_endpoint=probe_endpoint,
                expected_run=plan["run"],
                timeout=timeout,
                requester=requester,
            )
        if after_identity != source_identity:
            raise ValidationError(
                "live ECMWF source changed between the before/after probes; official snapshot is invalid"
            )
        source_probe_after_sha256 = sha256_bytes(after_raw)
        if spatial_after_paths[0].exists() or spatial_after_paths[1].exists():
            (
                _spatial_after_value,
                spatial_after_identity,
                spatial_after_raw,
                _spatial_after_meta,
            ) = _read_spatial_probe(
                cache_dir, "spatial_after", spatial_probe_endpoint, plan["run"]
            )
        else:
            (
                _spatial_after_value,
                spatial_after_identity,
                spatial_after_raw,
                _spatial_after_meta,
            ) = _capture_spatial_probe(
                cache_dir=cache_dir,
                label="spatial_after",
                endpoint=spatial_probe_endpoint,
                expected_run=plan["run"],
                timeout=timeout,
                requester=requester,
            )
        if spatial_after_identity != spatial_identity:
            raise ValidationError(
                "ECMWF spatial source changed between before/after probes; snapshot is invalid"
            )
        spatial_probe_after_sha256 = sha256_bytes(spatial_after_raw)

    entries: list[dict[str, Any]] = []
    total_failed_attempts = 0
    for batch in batches:
        selected = _batch_request_points(batch, points)
        verified = _verify_cached_batch(
            cache_dir,
            batch,
            endpoint,
            official_payload(plan, selected),
            plan,
            expected_source_identity=source_identity,
        )
        assert verified is not None
        raw, meta, _rows = verified
        total_failed_attempts += len(meta.get("failed_attempts_before_success", []))
        raw_path, meta_path = _cache_paths(cache_dir, str(batch["batch_id"]))
        entries.append(
            {
                "batch_id": batch["batch_id"],
                "profile": batch["profile"],
                "point_ids": batch["point_ids"],
                "request_point_ids": batch["request_point_ids"],
                "point_count": len(batch["point_ids"]),
                "estimated_weight": batch["estimated_weight"],
                "executor_id": meta.get("executor_id"),
                "request_sha256": meta["request_sha256"],
                "request_payload_file": meta["request_payload_file"],
                "request_payload_sha256": meta["request_payload_sha256"],
                "response_file": raw_path.relative_to(cache_dir).as_posix(),
                "response_sha256": sha256_bytes(raw),
                "metadata_file": meta_path.relative_to(cache_dir).as_posix(),
                "metadata_sha256": sha256_file(meta_path),
                "sentinel_point_id": batch["sentinel_point_id"],
                "sentinel_sha256": meta["sentinel_sha256"],
                "response_http_date": meta.get("response_headers", {}).get("date"),
            }
        )
    after_probe_paths = (
        _probe_paths(cache_dir, "after_transition")
        if transition_finalize
        else after_paths
    )
    spatial_final_probe_paths = (
        _probe_paths(cache_dir, "spatial_after_transition")
        if transition_finalize
        else spatial_after_paths
    )
    index_value: dict[str, Any] = {
            "schema_version": SCHEMA_VERSION,
            "type": "official_ecmwf_cache_index",
            "model": MODEL,
            "run": plan["run"],
            "plan_sha256": sha256_file(plan_path),
            "endpoint": endpoint,
            "method": "POST",
            "access_profile": access_profile,
            "point_count": POINT_COUNT,
            "profile_count": len(OFFICIAL_PROFILE_ORDER),
            "successful_request_count": len(batches),
            "request_count": len(batches),
            "theoretical_minimum_successful_requests": len(batches),
            "failed_http_attempt_count": total_failed_attempts,
            "estimated_total_weight": round(estimated_total_weight, 6),
            "source_probe_endpoint": probe_endpoint,
            "source_identity": source_identity,
            "source_probe_before_file": before_paths[0].relative_to(cache_dir).as_posix(),
            "source_probe_before_sha256": source_probe_before_sha256,
            "source_probe_after_file": after_probe_paths[0]
            .relative_to(cache_dir)
            .as_posix(),
            "source_probe_after_sha256": source_probe_after_sha256,
            "spatial_probe_endpoint": spatial_probe_endpoint,
            "spatial_identity": spatial_identity,
            "spatial_probe_before_file": spatial_before_paths[0]
            .relative_to(cache_dir)
            .as_posix(),
            "spatial_probe_before_sha256": spatial_probe_before_sha256,
            "spatial_probe_after_file": spatial_final_probe_paths[0]
            .relative_to(cache_dir)
            .as_posix(),
            "spatial_probe_after_sha256": spatial_probe_after_sha256,
            "capture_mode": capture_mode,
            "sentinel_sha256_by_profile": {
                profile: next(
                    entry["sentinel_sha256"]
                    for entry in entries
                    if entry["profile"] == profile
                )
                for profile in OFFICIAL_PROFILE_ORDER
            },
            "completed_at": utc_now(),
            "public_executor_batch_ids": public_executor_summary(batches)[0],
            "public_executor_estimated_weight": public_executor_summary(batches)[1],
            "entries": entries,
    }
    if transition_finalize:
        assert source_transition_identity is not None
        assert spatial_transition_identity is not None
        assert transition_proof is not None
        index_value.update(
            {
                "source_transition_identity": source_transition_identity,
                "spatial_transition_identity": spatial_transition_identity,
                "post_capture_transition_proof": transition_proof,
            }
        )
    index = _with_self_hash(index_value, "index_sha256")
    if index_path.exists():
        existing = read_json(index_path)
        if not isinstance(existing, dict):
            raise ValidationError("official cache index must contain an object")
        _validate_official_index(
            existing,
            plan=plan,
            plan_path=plan_path,
            endpoint=endpoint,
            access_profile=access_profile,
            batches=batches,
            source_identity=source_identity,
            source_probe_before_sha256=source_probe_before_sha256,
            source_probe_after_sha256=source_probe_after_sha256,
            spatial_identity=spatial_identity,
            spatial_probe_before_sha256=spatial_probe_before_sha256,
            spatial_probe_after_sha256=spatial_probe_after_sha256,
            capture_mode=capture_mode,
            source_transition_identity=source_transition_identity,
            spatial_transition_identity=spatial_transition_identity,
            post_capture_transition_proof=transition_proof,
        )
        # All immutable entry hashes must also agree. Dynamic completion time is
        # retained from the first complete capture.
        if existing.get("entries") != entries:
            raise ValidationError("existing official cache index entry hashes changed")
        return existing
    write_immutable_bytes(index_path, pretty_bytes(index))
    _validate_official_index(
        index,
        plan=plan,
        plan_path=plan_path,
        endpoint=endpoint,
        access_profile=access_profile,
        batches=batches,
        source_identity=source_identity,
        source_probe_before_sha256=source_probe_before_sha256,
        source_probe_after_sha256=source_probe_after_sha256,
        spatial_identity=spatial_identity,
        spatial_probe_before_sha256=spatial_probe_before_sha256,
        spatial_probe_after_sha256=spatial_probe_after_sha256,
        capture_mode=capture_mode,
        source_transition_identity=source_transition_identity,
        spatial_transition_identity=spatial_transition_identity,
        post_capture_transition_proof=transition_proof,
    )
    return index


def _run_from_value(value: Any) -> str | None:
    if isinstance(value, int) and value > 1_000_000_000:
        return format_run(dt.datetime.fromtimestamp(value, tz=dt.timezone.utc))
    if not isinstance(value, str):
        return None
    try:
        return format_run(parse_run(value))
    except ValidationError:
        # Coverage IDs commonly embed YYYYMMDDHH without being a run field;
        # callers only pass values under explicit RUN_KEYS.
        return None


def _find_run(value: Any) -> str | None:
    if isinstance(value, dict):
        for wanted in RUN_KEYS:
            for key, item in value.items():
                if str(key).lower() == wanted:
                    parsed = _run_from_value(item)
                    if parsed is not None:
                        return parsed
        for item in value.values():
            found = _find_run(item)
            if found is not None:
                return found
    elif isinstance(value, list):
        for item in value:
            found = _find_run(item)
            if found is not None:
                return found
    return None


def _find_variable_inventory(value: Any, field: str) -> list[str]:
    if isinstance(value, dict):
        variables = value.get(field)
        if isinstance(variables, list) and all(isinstance(item, str) for item in variables):
            return sorted(set(variables))
        for item in value.values():
            found = _find_variable_inventory(item, field)
            if found:
                return found
    elif isinstance(value, list):
        for item in value:
            found = _find_variable_inventory(item, field)
            if found:
                return found
    return []


def _find_available_variables(value: Any) -> list[str]:
    return _find_variable_inventory(value, "available_variables")


def _find_list_field(value: Any, field: str) -> list[Any] | None:
    if isinstance(value, dict):
        candidate = value.get(field)
        if isinstance(candidate, list):
            return candidate
        for item in value.values():
            found = _find_list_field(item, field)
            if found is not None:
                return found
    elif isinstance(value, list):
        for item in value:
            found = _find_list_field(item, field)
            if found is not None:
                return found
    return None


def _boundary_stencil_evidence(catalog: Any, run: str) -> dict[str, Any]:
    target = parse_run(run)
    prior12 = target - dt.timedelta(hours=12)
    prior18 = target - dt.timedelta(hours=6)

    def compact(value: dt.datetime) -> str:
        return value.strftime("%Y%m%d%H")

    def valid_time(value: dt.datetime) -> str:
        return value.strftime("%Y-%m-%dT%H:%M:%SZ")

    visible_required = [
        (compact(prior12), valid_time(prior12), 0),
        (compact(prior12), valid_time(prior12 + dt.timedelta(hours=3)), 3),
        (compact(prior18), valid_time(prior18), 0),
        (compact(prior18), valid_time(prior18 + dt.timedelta(hours=3)), 3),
    ]
    hidden_required = [
        (compact(prior12), valid_time(prior12 + dt.timedelta(hours=6)), 6),
        (compact(prior12), valid_time(prior12 + dt.timedelta(hours=9)), 9),
        (compact(prior18), valid_time(prior18 + dt.timedelta(hours=6)), 6),
        (compact(prior18), valid_time(prior18 + dt.timedelta(hours=9)), 9),
    ]
    coverage_plan = _find_list_field(catalog, "coverage_plan")
    support_records = _find_list_field(catalog, "interpolation_support_records")
    if coverage_plan is None or support_records is None:
        raise ValidationError(
            "catalog lacks coverage_plan/interpolation_support_records boundary evidence"
        )
    visible_actual = {
        (item.get("source_run"), item.get("valid_time_utc"), item.get("forecast_hour"))
        for item in coverage_plan
        if isinstance(item, dict)
    }
    hidden_actual = {
        (item.get("source_run"), item.get("valid_time_utc"), item.get("forecast_hour"))
        for item in support_records
        if isinstance(item, dict)
        and item.get("hidden") is True
        and item.get("right_support") is True
        and item.get("support_kind") == "right_lookahead"
    }
    missing_visible = [item for item in visible_required if item not in visible_actual]
    missing_hidden = [item for item in hidden_required if item not in hidden_actual]
    if missing_visible or missing_hidden:
        raise ValidationError(
            "rolling interpolation boundary stencil is incomplete: "
            f"missing_visible={missing_visible}, missing_hidden={missing_hidden}"
        )
    evidence = {
        "prior12_visible": [list(item) for item in visible_required[:2]],
        "prior12_hidden_right": [list(item) for item in hidden_required[:2]],
        "prior18_visible": [list(item) for item in visible_required[2:]],
        "prior18_hidden_right": [list(item) for item in hidden_required[2:]],
    }
    evidence["sha256"] = sha256_bytes(canonical_bytes(evidence))
    return evidence


def create_freeze_attestation(
    run: str,
    release_manifest_path: Path,
    catalog_manifest_path: Path,
    output_path: Path,
    confirmed: bool,
    required_hourly_variables: Iterable[str] | None = None,
    required_daily_variables: Iterable[str] | None = None,
) -> dict[str, Any]:
    if not confirmed:
        raise ValidationError("--confirm-updates-frozen is required")
    expected_run = format_run(parse_run(run))
    release_manifest_path = release_manifest_path.resolve()
    catalog_manifest_path = catalog_manifest_path.resolve()
    release = read_json(release_manifest_path)
    catalog = read_json(catalog_manifest_path)
    release_run = _find_run(release)
    catalog_run = _find_run(catalog)
    if release_run != expected_run or catalog_run != expected_run:
        raise ValidationError(
            f"manifest run mismatch: expected={expected_run}, release={release_run}, "
            f"catalog={catalog_run}"
        )
    variables = _find_available_variables(catalog)
    if not variables:
        raise ValidationError("catalog manifest must expose a non-empty available_variables list")
    hourly_variables = _find_variable_inventory(catalog, "available_hourly_variables")
    daily_variables = _find_variable_inventory(catalog, "available_daily_variables")
    required_hourly = sorted(
        set(required_hourly_variables if required_hourly_variables is not None else HOURLY_VARIABLES)
    )
    required_daily = sorted(
        set(required_daily_variables if required_daily_variables is not None else DAILY_VARIABLES)
    )
    if hourly_variables != required_hourly or daily_variables != required_daily:
        raise ValidationError(
            "catalog hourly/daily inventories differ from the exact API contract"
        )
    required = sorted(set((*required_hourly, *required_daily)))
    if variables != required:
        missing = sorted(set(required) - set(variables))
        extra = sorted(set(variables) - set(required))
        raise ValidationError(
            "catalog available_variables differs from the exact API contract: "
            f"missing={missing[:20]}, extra={extra[:20]}"
        )
    boundary_stencil = _boundary_stencil_evidence(catalog, expected_run)
    attestation = _with_self_hash(
        {
            "schema_version": SCHEMA_VERSION,
            "type": "ecmwf_local_static_batch_freeze",
            "model": MODEL,
            "run": expected_run,
            "updates_frozen_confirmed": True,
            "created_at": utc_now(),
            "release_manifest": {
                "path": str(release_manifest_path),
                "sha256": sha256_file(release_manifest_path),
            },
            "catalog_manifest": {
                "path": str(catalog_manifest_path),
                "sha256": sha256_file(catalog_manifest_path),
                "available_variables_sha256": sha256_bytes(canonical_bytes(variables)),
                "available_variable_count": len(variables),
                "available_hourly_variables_sha256": sha256_bytes(
                    canonical_bytes(hourly_variables)
                ),
                "available_hourly_variable_count": len(hourly_variables),
                "available_daily_variables_sha256": sha256_bytes(
                    canonical_bytes(daily_variables)
                ),
                "available_daily_variable_count": len(daily_variables),
                "boundary_stencil": boundary_stencil,
            },
        },
        "attestation_sha256",
    )
    write_json_exclusive(output_path, attestation)
    return attestation


def verify_freeze(
    attestation_path: Path,
    plan: Mapping[str, Any],
    config: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    del config  # Compatibility with the previous harness signature.
    attestation = read_json(attestation_path)
    if not isinstance(attestation, dict):
        raise ValidationError("freeze attestation must contain a JSON object")
    _verify_self_hash(attestation, "attestation_sha256", "freeze attestation")
    if (
        attestation.get("schema_version") != SCHEMA_VERSION
        or attestation.get("model") != MODEL
        or attestation.get("run") != plan.get("run")
        or attestation.get("updates_frozen_confirmed") is not True
    ):
        raise ValidationError("freeze attestation does not match the validation plan")
    live_hashes: dict[str, str] = {}
    for key in ("release_manifest", "catalog_manifest"):
        item = attestation.get(key)
        if not isinstance(item, dict) or not isinstance(item.get("path"), str):
            raise ValidationError(f"freeze attestation has no {key} path")
        path = Path(item["path"])
        actual_hash = sha256_file(path)
        if actual_hash != item.get("sha256"):
            raise ValidationError(f"frozen {key} hash changed: {path}")
        manifest = read_json(path)
        if _find_run(manifest) != plan["run"]:
            raise ValidationError(f"frozen {key} run identity changed: {path}")
        live_hashes[key] = actual_hash
        if key == "catalog_manifest":
            variables = _find_available_variables(manifest)
            if not variables:
                raise ValidationError("frozen catalog lost available_variables")
            if sha256_bytes(canonical_bytes(variables)) != item.get(
                "available_variables_sha256"
            ):
                raise ValidationError("frozen catalog variable inventory changed")
            expected_variables = sorted(
                set((*plan["hourly"]["variables"], *plan["daily"]["variables"]))
            )
            if variables != expected_variables:
                raise ValidationError(
                    "frozen catalog available_variables no longer matches the plan contract"
                )
            for period in ("hourly", "daily"):
                period_variables = _find_variable_inventory(
                    manifest, f"available_{period}_variables"
                )
                expected_period_variables = sorted(set(plan[period]["variables"]))
                if period_variables != expected_period_variables:
                    raise ValidationError(
                        f"frozen catalog available_{period}_variables differs from the plan"
                    )
                if sha256_bytes(canonical_bytes(period_variables)) != item.get(
                    f"available_{period}_variables_sha256"
                ):
                    raise ValidationError(
                        f"frozen catalog {period} variable inventory changed"
                    )
            boundary_stencil = _boundary_stencil_evidence(manifest, plan["run"])
            if boundary_stencil != item.get("boundary_stencil"):
                raise ValidationError("frozen rolling boundary stencil changed")
    return {
        "attestation": attestation,
        "attestation_sha256": sha256_file(attestation_path),
        "release_manifest_sha256": live_hashes["release_manifest"],
        "catalog_manifest_sha256": live_hashes["catalog_manifest"],
    }


def load_official_reference(
    plan: Mapping[str, Any],
    plan_path: Path,
    config: Mapping[str, Any],
    cache_dir: Path,
) -> tuple[dict[str, dict[str, Any]], dict[str, Any]]:
    index_path = cache_dir / "official_index.json"
    index = read_json(index_path)
    if not isinstance(index, dict):
        raise ValidationError("official cache index must contain a JSON object")
    access_profile = str(index.get("access_profile", ""))
    batches = official_batches(plan, config, access_profile)
    probe_endpoint = normalize_endpoint(config["official"]["source_probe_endpoint"])
    _before_value, before_identity, before_raw, _before_meta = _read_source_probe(
        cache_dir, "before", probe_endpoint, plan["run"]
    )
    spatial_probe_endpoint = normalize_endpoint(config["official"]["spatial_probe_endpoint"])
    _spatial_before_value, spatial_identity, spatial_before_raw, _spatial_before_meta = (
        _read_spatial_probe(
            cache_dir, "spatial_before", spatial_probe_endpoint, plan["run"]
        )
    )
    capture_mode = str(index.get("capture_mode", ""))
    transition_proof: dict[str, Any] | None = None
    source_transition_identity: dict[str, int] | None = None
    spatial_transition_identity: dict[str, Any] | None = None
    if capture_mode == "same_source":
        _after_value, after_identity, after_raw, _after_meta = _read_source_probe(
            cache_dir, "after", probe_endpoint, plan["run"]
        )
        if before_identity != after_identity:
            raise ValidationError("official cache spans two live ECMWF source identities")
        (
            _spatial_after_value,
            spatial_after_identity,
            spatial_after_raw,
            _spatial_after_meta,
        ) = _read_spatial_probe(
            cache_dir, "spatial_after", spatial_probe_endpoint, plan["run"]
        )
        if spatial_identity != spatial_after_identity:
            raise ValidationError(
                "official cache spans two ECMWF spatial source identities"
            )
    elif capture_mode == "post_capture_transition":
        (
            _after_value,
            source_transition_identity,
            after_raw,
            after_meta,
        ) = _read_source_probe(
            cache_dir, "after_transition", probe_endpoint, None
        )
        (
            _spatial_after_value,
            spatial_transition_identity,
            spatial_after_raw,
            spatial_after_meta,
        ) = _read_spatial_probe(
            cache_dir, "spatial_after_transition", spatial_probe_endpoint, None
        )
        transition_proof = _post_capture_transition_proof(
            cache_dir=cache_dir,
            batches=batches,
            target_run=plan["run"],
            source_identity=before_identity,
            source_transition_identity=source_transition_identity,
            source_transition_meta=after_meta,
            source_transition_sha256=sha256_bytes(after_raw),
            spatial_identity=spatial_identity,
            spatial_transition_identity=spatial_transition_identity,
            spatial_transition_meta=spatial_after_meta,
            spatial_transition_sha256=sha256_bytes(spatial_after_raw),
        )
    else:
        raise ValidationError(f"unsupported official capture mode: {capture_mode}")
    _validate_official_index(
        index,
        plan=plan,
        plan_path=plan_path,
        endpoint=str(index.get("endpoint", "")),
        access_profile=access_profile,
        batches=batches,
        source_identity=before_identity,
        source_probe_before_sha256=sha256_bytes(before_raw),
        source_probe_after_sha256=sha256_bytes(after_raw),
        spatial_identity=spatial_identity,
        spatial_probe_before_sha256=sha256_bytes(spatial_before_raw),
        spatial_probe_after_sha256=sha256_bytes(spatial_after_raw),
        capture_mode=capture_mode,
        source_transition_identity=source_transition_identity,
        spatial_transition_identity=spatial_transition_identity,
        post_capture_transition_proof=transition_proof,
    )
    points = point_map(plan)
    result: dict[str, dict[str, Any]] = {}
    entries = index["entries"]
    for batch, entry in zip(batches, entries):
        if entry.get("batch_id") != batch["batch_id"]:
            raise ValidationError("official cache index batch order changed")
        selected = _batch_request_points(batch, points)
        verified = _verify_cached_batch(
            cache_dir,
            batch,
            index["endpoint"],
            official_payload(plan, selected),
            plan,
            expected_source_identity=before_identity,
        )
        if verified is None:
            raise ValidationError(f"official cache batch is missing: {batch['batch_id']}")
        raw, meta, rows = verified
        raw_path, meta_path = _cache_paths(cache_dir, str(batch["batch_id"]))
        if (
            entry.get("response_sha256") != sha256_bytes(raw)
            or entry.get("metadata_sha256") != sha256_file(meta_path)
            or meta.get("source_probe_before_sha256") != sha256_bytes(before_raw)
        ):
            raise ValidationError(f"official index/cache hash mismatch: {batch['batch_id']}")
        included_ids = set(batch["point_ids"])
        for point_id, row in zip(batch["request_point_ids"], rows):
            if point_id not in included_ids:
                if point_id != batch["sentinel_point_id"]:
                    raise ValidationError("official batch has an undeclared transport point")
                continue
            if point_id in result:
                raise ValidationError(f"official cache contains duplicate point {point_id}")
            result[point_id] = row
    if set(result) != set(points):
        raise ValidationError("official cache does not cover all 500 deterministic points")
    return result, index


def _json_kind(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, int):
        return "integer"
    if isinstance(value, float):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return type(value).__name__


def strictly_equal(expected: Any, actual: Any) -> bool:
    """JSON equality without Python's bool/int or int/float coercions."""
    kind = _json_kind(expected)
    if kind != _json_kind(actual):
        return False
    if kind == "number":
        if expected != actual:
            return False
        if expected == 0.0:
            return math.copysign(1.0, expected) == math.copysign(1.0, actual)
        return True
    if kind in {"null", "boolean", "integer", "string"}:
        return expected == actual
    if kind == "array":
        return len(expected) == len(actual) and all(
            strictly_equal(left, right) for left, right in zip(expected, actual)
        )
    if kind == "object":
        return set(expected) == set(actual) and all(
            strictly_equal(expected[key], actual[key]) for key in expected
        )
    return False


def _path_key(path: str, key: str) -> str:
    if key.replace("_", "").isalnum():
        return f"{path}.{key}"
    return f"{path}[{json.dumps(key, ensure_ascii=False)}]"


def _preferred_keys(path: str, keys: set[str], plan: Mapping[str, Any]) -> list[str]:
    if path == "$.hourly":
        order = ["time", *plan["hourly"]["variables"]]
    elif path == "$.hourly_units":
        order = ["time", *plan["hourly"]["variables"]]
    elif path == "$.daily":
        order = ["time", *plan["daily"]["variables"]]
    elif path == "$.daily_units":
        order = ["time", *plan["daily"]["variables"]]
    else:
        order = [
            "latitude",
            "longitude",
            "elevation",
            "utc_offset_seconds",
            "timezone",
            "timezone_abbreviation",
            "hourly_units",
            "hourly",
            "daily_units",
            "daily",
        ]
    selected = [key for key in order if key in keys]
    selected.extend(sorted(keys - set(selected)))
    return selected


def _difference(
    *,
    path: str,
    reason: str,
    official: Any = None,
    local: Any = None,
    official_present: bool = True,
    local_present: bool = True,
) -> dict[str, Any]:
    result: dict[str, Any] = {
        "path": path,
        "reason": reason,
        "official_present": official_present,
        "local_present": local_present,
    }
    if official_present:
        result["official"] = official
        result["official_json_type"] = _json_kind(official)
    if local_present:
        result["local"] = local
        result["local_json_type"] = _json_kind(local)
    return result


def first_json_difference(
    official: Any,
    local: Any,
    plan: Mapping[str, Any],
    path: str = "$",
) -> dict[str, Any] | None:
    if path == "$" and isinstance(official, dict) and isinstance(local, dict):
        official = _stable_row(official)
        local = _stable_row(local)
    official_kind = _json_kind(official)
    local_kind = _json_kind(local)
    if official_kind != local_kind:
        return _difference(path=path, reason="json_type", official=official, local=local)
    if official_kind == "object":
        official_keys = set(official)
        local_keys = set(local)
        missing = official_keys - local_keys
        if missing:
            key = _preferred_keys(path, missing, plan)[0]
            return _difference(
                path=_path_key(path, key),
                reason="missing_local_field",
                official=official[key],
                local_present=False,
            )
        extra = local_keys - official_keys
        if extra:
            key = _preferred_keys(path, extra, plan)[0]
            return _difference(
                path=_path_key(path, key),
                reason="extra_local_field",
                local=local[key],
                official_present=False,
            )
        for key in _preferred_keys(path, official_keys, plan):
            difference = first_json_difference(
                official[key], local[key], plan, _path_key(path, key)
            )
            if difference is not None:
                return difference
        return None
    if official_kind == "array":
        if len(official) != len(local):
            return _difference(
                path=path,
                reason="array_length",
                official=len(official),
                local=len(local),
            )
        for index, (official_item, local_item) in enumerate(zip(official, local)):
            difference = first_json_difference(
                official_item, local_item, plan, f"{path}[{index}]"
            )
            if difference is not None:
                return difference
        return None
    if not strictly_equal(official, local):
        return _difference(path=path, reason="json_value", official=official, local=local)
    return None


def _atomic_report(path: Path, report: dict[str, Any]) -> dict[str, Any]:
    value = _with_self_hash(report, "report_sha256")
    atomic_replace_bytes(path, pretty_bytes(value))
    return value


def _validation_contract(
    *,
    plan: Mapping[str, Any],
    plan_path: Path,
    config_path: Path,
    cache_dir: Path,
    freeze_path: Path,
    local_endpoint: str,
    official_index: Mapping[str, Any],
) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "model": MODEL,
        "run": plan["run"],
        "sample_count": POINT_COUNT,
        "point_order_sha256": sha256_bytes(
            canonical_bytes([point["id"] for point in plan_points(plan)])
        ),
        "plan_file": str(plan_path.resolve()),
        "plan_file_sha256": sha256_file(plan_path),
        "config_file": str(config_path.resolve()),
        "config_file_sha256": sha256_file(config_path),
        "official_index_file": str((cache_dir / "official_index.json").resolve()),
        "official_index_file_sha256": sha256_file(cache_dir / "official_index.json"),
        "official_index_sha256": official_index["index_sha256"],
        "freeze_attestation_file": str(freeze_path.resolve()),
        "freeze_attestation_file_sha256": sha256_file(freeze_path),
        "local_endpoint": normalize_endpoint(local_endpoint),
        "local_request_method": "POST",
        "local_request_content_type": "application/json",
        "local_parallelism": 1,
        "comparison": "strict_json_value_and_type",
        "ignored_dynamic_metadata": list(IGNORED_DYNAMIC_METADATA),
        "hourly_frames_per_variable": HOURLY_FRAMES,
        "daily_frames_per_variable": DAILY_FRAMES,
        "first_difference_stops": True,
    }


def _new_report(contract: Mapping[str, Any], contract_sha256: str) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "type": "ecmwf_strict_validation_report",
        "status": "running",
        "started_at": utc_now(),
        "updated_at": utc_now(),
        "finished_at": None,
        "resume_count": 0,
        "contract": dict(contract),
        "contract_sha256": contract_sha256,
        "points_completed": 0,
        "local_requests_completed": 0,
        "data_values_compared": 0,
        "hourly_values_compared": 0,
        "daily_values_compared": 0,
        "point_receipts": [],
        "failure": None,
    }


def _receipt_path(output_dir: Path, point: Mapping[str, Any]) -> Path:
    return output_dir / "receipts" / f"{point['order']:04d}_{point['id']}.receipt.json"


def _verify_receipt(
    path: Path,
    *,
    point: Mapping[str, Any],
    contract_sha256: str,
    output_dir: Path,
) -> dict[str, Any]:
    receipt = read_json(path)
    if not isinstance(receipt, dict):
        raise ValidationError(f"point receipt must contain an object: {path}")
    _verify_self_hash(receipt, "receipt_sha256", "point receipt")
    if receipt.get("contract_sha256") != contract_sha256 or receipt.get("point") != dict(point):
        raise ValidationError(f"point receipt contract/identity mismatch: {path}")
    for file_key, hash_key in (
        ("local_request_body_file", "local_request_body_sha256"),
        ("local_response_file", "local_response_sha256"),
        ("local_metadata_file", "local_metadata_sha256"),
    ):
        relative = Path(str(receipt.get(file_key, "")))
        if relative.is_absolute() or ".." in relative.parts:
            raise ValidationError(f"unsafe point receipt artifact path: {relative}")
        artifact = output_dir / relative
        if not artifact.is_file() or sha256_file(artifact) != receipt.get(hash_key):
            raise ValidationError(f"point receipt artifact hash mismatch: {artifact}")
    return receipt


def _replay_receipts(
    output_dir: Path,
    points: list[dict[str, Any]],
    contract_sha256: str,
) -> tuple[list[dict[str, Any]], dict[str, int]]:
    receipts: list[dict[str, Any]] = []
    totals = {
        "data_values_compared": 0,
        "hourly_values_compared": 0,
        "daily_values_compared": 0,
    }
    gap_seen = False
    for point in points:
        path = _receipt_path(output_dir, point)
        if not path.exists():
            gap_seen = True
            continue
        if gap_seen:
            raise ValidationError("point receipts are not a contiguous deterministic prefix")
        receipt = _verify_receipt(
            path,
            point=point,
            contract_sha256=contract_sha256,
            output_dir=output_dir,
        )
        receipts.append(receipt)
        for key in totals:
            totals[key] += int(receipt[key])
    return receipts, totals


def _attempt_paths(
    output_dir: Path, point: Mapping[str, Any], attempt_id: str
) -> tuple[Path, Path, Path]:
    base = output_dir / "attempts" / f"{point['order']:04d}_{point['id']}"
    return (
        base / f"{attempt_id}.request.json",
        base / f"{attempt_id}.response.json",
        base / f"{attempt_id}.meta.json",
    )


def _local_request_audit(endpoint: str, body: bytes) -> dict[str, Any]:
    return {
        "method": "POST",
        "endpoint": normalize_endpoint(endpoint),
        "content_type": "application/json",
        "payload_bytes": len(body),
        "payload_sha256": sha256_bytes(body),
    }


def run_validation(
    args: argparse.Namespace,
    config: dict[str, Any],
    config_path: Path,
    *,
    requester: Callable[..., HttpResult] = _request_once,
) -> dict[str, Any]:
    plan_path = Path(args.plan)
    cache_dir = Path(args.cache_dir)
    freeze_path = Path(args.freeze_attestation)
    output_dir = Path(args.output_dir)
    plan = load_plan(plan_path, config, config_path)
    points = plan_points(plan)
    verify_freeze(freeze_path, plan, config)
    official, official_index = load_official_reference(plan, plan_path, config, cache_dir)
    local_endpoint = normalize_endpoint(args.local_endpoint)
    contract = _validation_contract(
        plan=plan,
        plan_path=plan_path,
        config_path=config_path,
        cache_dir=cache_dir,
        freeze_path=freeze_path,
        local_endpoint=local_endpoint,
        official_index=official_index,
    )
    contract_sha256 = sha256_bytes(canonical_bytes(contract))
    output_dir.mkdir(parents=True, exist_ok=True)
    report_path = output_dir / "report.json"
    if report_path.exists():
        report = read_json(report_path)
        if not isinstance(report, dict):
            raise ValidationError("existing report must contain an object")
        _verify_self_hash(report, "report_sha256", "validation report")
        if report.get("contract_sha256") != contract_sha256:
            raise ValidationError("existing report contract differs; use a new output directory")
        if report.get("status") == "passed":
            return report
        report["resume_count"] = int(report.get("resume_count", 0)) + 1
    else:
        report = _new_report(contract, contract_sha256)
    receipts, totals = _replay_receipts(output_dir, points, contract_sha256)
    report["points_completed"] = len(receipts)
    report["local_requests_completed"] = len(receipts)
    report["point_receipts"] = [
        {
            "point_id": receipt["point"]["id"],
            "receipt_file": _receipt_path(output_dir, receipt["point"])
            .relative_to(output_dir)
            .as_posix(),
            "receipt_sha256": receipt["receipt_sha256"],
        }
        for receipt in receipts
    ]
    report.update(totals)
    report["status"] = "running"
    report["failure"] = None
    report["updated_at"] = utc_now()
    report = _atomic_report(report_path, report)

    max_local_requests = int(getattr(args, "max_local_requests", POINT_COUNT))
    remaining = POINT_COUNT - len(receipts)
    if max_local_requests < remaining:
        raise ValidationError(
            f"validation needs {remaining} serial local requests, exceeding "
            f"--max-local-requests={max_local_requests}"
        )
    timeout = float(getattr(args, "timeout", 300.0))
    if timeout <= 0:
        raise ValidationError("local timeout must be positive")

    requests_this_run = 0
    hourly_per_point = len(plan["hourly"]["variables"]) * HOURLY_FRAMES
    daily_per_point = len(plan["daily"]["variables"]) * DAILY_FRAMES
    for point in points[len(receipts) :]:
        pre_freeze = verify_freeze(freeze_path, plan, config)
        url, request_body, request_audit = local_request(local_endpoint, plan, point)
        requests_this_run += 1
        attempt_id = (
            dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
            + "_"
            + uuid.uuid4().hex
        )
        request_path, raw_path, meta_path = _attempt_paths(output_dir, point, attempt_id)
        write_immutable_bytes(request_path, request_body)
        request_audit["payload_file"] = request_path.relative_to(output_dir).as_posix()
        result: HttpResult | None = None
        transport_error: str | None = None
        difference: dict[str, Any] | None = None
        try:
            result = requester(
                "POST",
                url,
                body=request_body,
                headers={
                    "Accept": "application/json",
                    "Content-Type": "application/json",
                    "User-Agent": USER_AGENT,
                },
                timeout=timeout,
            )
            if result.status != 200:
                difference = {
                    "path": "$",
                    "reason": "local_http_status",
                    "official_present": False,
                    "local_present": True,
                    "local": result.status,
                    "local_json_type": "integer",
                }
            else:
                local_rows = normalize_response_rows(
                    _response_json(result.raw, f"local response for {point['id']}"), 1
                )
                difference = first_json_difference(
                    official[point["id"]], local_rows[0], plan
                )
        except (HttpRequestError, ValidationError) as exc:
            transport_error = f"{type(exc).__name__}: {exc}"
            if isinstance(exc, HttpRequestError) and exc.result is not None:
                result = exc.result
            difference = {
                "path": "$",
                "reason": "invalid_or_unavailable_local_response",
                "detail": transport_error,
            }
        # The post-point manifest check is deliberately after parsing and the
        # full strict comparison, including a failing comparison.
        post_freeze = verify_freeze(freeze_path, plan, config)
        response_raw = result.raw if result is not None else b""
        attempt_meta = {
            "schema_version": SCHEMA_VERSION,
            "type": "ecmwf_local_point_attempt",
            "point": point,
            "attempt_id": attempt_id,
            "captured_at": utc_now(),
            "request": request_audit,
            "response": {
                "http_status": result.status if result is not None else None,
                "headers": result.headers if result is not None else {},
                "bytes": len(response_raw),
                "sha256": sha256_bytes(response_raw),
                "elapsed_seconds": (
                    round(result.elapsed_seconds, 6) if result is not None else None
                ),
                "transport_error": transport_error,
            },
            "freeze_before": {
                key: pre_freeze[key]
                for key in (
                    "attestation_sha256",
                    "release_manifest_sha256",
                    "catalog_manifest_sha256",
                )
            },
            "freeze_after": {
                key: post_freeze[key]
                for key in (
                    "attestation_sha256",
                    "release_manifest_sha256",
                    "catalog_manifest_sha256",
                )
            },
            "comparison": {
                "strict": True,
                "ignored_dynamic_metadata": list(IGNORED_DYNAMIC_METADATA),
                "first_difference": difference,
            },
            "official_evidence": {
                "official_index_sha256": official_index["index_sha256"],
                "official_row_sha256": sha256_bytes(
                    canonical_bytes(_stable_row(official[point["id"]]))
                ),
            },
        }
        atomic_commit_pair(raw_path, response_raw, meta_path, pretty_bytes(attempt_meta))
        raw_relative = raw_path.relative_to(output_dir).as_posix()
        meta_relative = meta_path.relative_to(output_dir).as_posix()
        if difference is not None:
            report["status"] = "failed"
            report["updated_at"] = utc_now()
            report["finished_at"] = utc_now()
            report["failure"] = {
                "kind": "first_strict_difference",
                "point": point,
                "difference": difference,
                "request": request_audit,
                "local_request_body_file": request_path.relative_to(output_dir).as_posix(),
                "local_request_body_sha256": sha256_bytes(request_body),
                "local_response_file": raw_relative,
                "local_response_sha256": sha256_bytes(response_raw),
                "local_metadata_file": meta_relative,
                "local_metadata_sha256": sha256_file(meta_path),
                "official_row_sha256": attempt_meta["official_evidence"][
                    "official_row_sha256"
                ],
            }
            report["local_network_requests_this_run"] = requests_this_run
            return _atomic_report(report_path, report)

        receipt = _with_self_hash(
            {
                "schema_version": SCHEMA_VERSION,
                "type": "ecmwf_completed_point_receipt",
                "contract_sha256": contract_sha256,
                "completed_at": utc_now(),
                "point": point,
                "local_request_body_file": request_path.relative_to(output_dir).as_posix(),
                "local_request_body_sha256": sha256_bytes(request_body),
                "local_response_file": raw_relative,
                "local_response_sha256": sha256_bytes(response_raw),
                "local_metadata_file": meta_relative,
                "local_metadata_sha256": sha256_file(meta_path),
                "official_row_sha256": attempt_meta["official_evidence"][
                    "official_row_sha256"
                ],
                "local_row_sha256": sha256_bytes(
                    canonical_bytes(_stable_row(normalize_response_rows(
                        _response_json(response_raw, "completed local response"), 1
                    )[0]))
                ),
                "hourly_values_compared": hourly_per_point,
                "daily_values_compared": daily_per_point,
                "data_values_compared": hourly_per_point + daily_per_point,
                "freeze_before": attempt_meta["freeze_before"],
                "freeze_after": attempt_meta["freeze_after"],
            },
            "receipt_sha256",
        )
        receipt_path = _receipt_path(output_dir, point)
        write_immutable_bytes(receipt_path, pretty_bytes(receipt))
        report["points_completed"] += 1
        report["local_requests_completed"] += 1
        report["hourly_values_compared"] += hourly_per_point
        report["daily_values_compared"] += daily_per_point
        report["data_values_compared"] += hourly_per_point + daily_per_point
        report["point_receipts"].append(
            {
                "point_id": point["id"],
                "receipt_file": receipt_path.relative_to(output_dir).as_posix(),
                "receipt_sha256": receipt["receipt_sha256"],
            }
        )
        report["updated_at"] = utc_now()
        report["local_network_requests_this_run"] = requests_this_run
        report = _atomic_report(report_path, report)
        print(
            json.dumps(
                {
                    "point_order": point["order"],
                    "point_id": point["id"],
                    "status": "passed",
                    "points_completed": report["points_completed"],
                },
                ensure_ascii=False,
            ),
            flush=True,
        )

    verify_freeze(freeze_path, plan, config)
    expected_data = POINT_COUNT * (hourly_per_point + daily_per_point)
    if report["points_completed"] != POINT_COUNT or report["data_values_compared"] != expected_data:
        raise ValidationError("completed report counters do not satisfy the fixed validation contract")
    report["status"] = "passed"
    report["failure"] = None
    report["updated_at"] = utc_now()
    report["finished_at"] = utc_now()
    report["local_network_requests_this_run"] = requests_this_run
    return _atomic_report(report_path, report)


def default_config_path() -> Path:
    return Path(__file__).with_name("ecmwf_validation_config.json")


def parse_public_ssh_executors(
    specifications: Iterable[str],
) -> dict[str, Callable[..., HttpResult]]:
    requesters: dict[str, Callable[..., HttpResult]] = {}
    for specification in specifications:
        if "=" not in specification:
            raise ValidationError(
                "public SSH executor must use EXECUTOR_ID=SSH_DESTINATION"
            )
        executor_id, destination = specification.split("=", 1)
        if executor_id in requesters:
            raise ValidationError(f"duplicate public SSH executor id: {executor_id}")
        requesters[executor_id] = SshHttpRequester(executor_id, destination)
    return requesters


def build_public_executor_requesters(
    local_executor_id: str | None,
    ssh_specifications: Iterable[str],
) -> dict[str, Callable[..., HttpResult]]:
    requesters: dict[str, Callable[..., HttpResult]] = {}
    if local_executor_id is not None:
        if not local_executor_id or any(
            character.isspace() for character in local_executor_id
        ):
            raise ValidationError(
                "public local executor id must be non-empty and contain no whitespace"
            )
        requesters[local_executor_id] = _request_once
    for executor_id, requester in parse_public_ssh_executors(
        ssh_specifications
    ).items():
        if executor_id in requesters:
            raise ValidationError(f"duplicate public executor id: {executor_id}")
        requesters[executor_id] = requester
    return requesters


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", default=str(default_config_path()))
    subparsers = parser.add_subparsers(dest="command", required=True)

    plan_parser = subparsers.add_parser(
        "plan", help="create the fixed deterministic 500-point/00Z validation plan"
    )
    plan_parser.add_argument("--run", required=True)
    plan_parser.add_argument("--seed", type=int, default=20260723)
    plan_parser.add_argument("--output", required=True)

    freeze_parser = subparsers.add_parser(
        "freeze", help="attest the already-stopped local ECMWF static batch"
    )
    freeze_parser.add_argument("--run", required=True)
    freeze_parser.add_argument("--release-manifest", required=True)
    freeze_parser.add_argument("--catalog-manifest", required=True)
    freeze_parser.add_argument("--output", required=True)
    freeze_parser.add_argument("--confirm-updates-frozen", action="store_true")

    fetch_parser = subparsers.add_parser(
        "fetch-official", help="capture the frozen live /v1/ecmwf official oracle"
    )
    fetch_parser.add_argument("--plan", required=True)
    fetch_parser.add_argument("--cache-dir", required=True)
    fetch_parser.add_argument("--official-endpoint")
    fetch_parser.add_argument("--api-key-env", default="OPEN_METEO_API_KEY")
    fetch_parser.add_argument("--allow-network", action="store_true")
    fetch_parser.add_argument("--allow-public-noncommercial", action="store_true")
    fetch_parser.add_argument(
        "--accept-proven-post-capture-transition",
        action="store_true",
        help=(
            "explicitly finalize a complete immutable cache when every official "
            "HTTP Date predates independently timestamped temporal and spatial "
            "source transitions"
        ),
    )
    fetch_parser.add_argument("--allow-loopback-mock", action="store_true", help=argparse.SUPPRESS)
    fetch_parser.add_argument(
        "--public-ssh-executor",
        action="append",
        default=[],
        metavar="ID=DESTINATION",
        help=(
            "statically assign public request batches to named independent SSH "
            "terminals; may be repeated"
        ),
    )
    fetch_parser.add_argument(
        "--public-local-executor",
        metavar="ID",
        help=(
            "include the machine running this command as one statically assigned "
            "public executor"
        ),
    )
    fetch_parser.add_argument("--max-new-requests", type=int, default=0)
    fetch_parser.add_argument("--delay-seconds", type=float, default=1.0)
    fetch_parser.add_argument("--timeout", type=float, default=300.0)
    fetch_parser.add_argument("--retries", type=int, default=3)

    validate_parser = subparsers.add_parser(
        "validate", help="compare one complete local point at a time and stop on first difference"
    )
    validate_parser.add_argument("--plan", required=True)
    validate_parser.add_argument("--cache-dir", required=True)
    validate_parser.add_argument("--freeze-attestation", required=True)
    validate_parser.add_argument("--local-endpoint")
    validate_parser.add_argument("--output-dir", required=True)
    validate_parser.add_argument("--timeout", type=float, default=300.0)
    validate_parser.add_argument("--max-local-requests", type=int, default=POINT_COUNT)
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(list(argv) if argv is not None else None)
    config_path = Path(args.config)
    try:
        config = load_config(config_path)
        if args.command == "plan":
            require_production_evidence_path(Path(args.output), config, "plan output")
            plan = generate_plan(
                args.run,
                seed=args.seed,
                config=config,
                config_sha256=sha256_file(config_path),
            )
            write_json_exclusive(Path(args.output), plan)
            print(
                json.dumps(
                    {
                        "status": "created",
                        "output": args.output,
                        "points": POINT_COUNT,
                        "hourly_frames": HOURLY_FRAMES,
                        "daily_frames": DAILY_FRAMES,
                    }
                )
            )
            return 0
        if args.command == "freeze":
            require_production_evidence_path(Path(args.output), config, "freeze output")
            attestation = create_freeze_attestation(
                args.run,
                Path(args.release_manifest),
                Path(args.catalog_manifest),
                Path(args.output),
                args.confirm_updates_frozen,
            )
            print(
                json.dumps(
                    {"status": "created", "output": args.output, "run": attestation["run"]}
                )
            )
            return 0
        if args.command == "fetch-official":
            require_production_evidence_path(
                Path(args.cache_dir), config, "official cache directory"
            )
            plan_path = Path(args.plan)
            plan = load_plan(plan_path, config, config_path)
            endpoint = args.official_endpoint or config["official"]["endpoint"]
            api_key, access_profile = resolve_official_access(
                endpoint,
                allow_public_noncommercial=args.allow_public_noncommercial,
                api_key_environment=args.api_key_env,
                allow_loopback_mock=args.allow_loopback_mock,
            )
            public_requesters = build_public_executor_requesters(
                args.public_local_executor,
                args.public_ssh_executor,
            )
            index = fetch_official(
                plan,
                plan_path,
                config,
                Path(args.cache_dir),
                endpoint,
                args.allow_network,
                args.max_new_requests,
                args.delay_seconds,
                args.timeout,
                args.retries,
                api_key,
                access_profile,
                public_executor_requesters=public_requesters or None,
                accept_proven_post_capture_transition=(
                    args.accept_proven_post_capture_transition
                ),
            )
            print(
                json.dumps(
                    {
                        "status": "cached",
                        "points": index["point_count"],
                        "successful_requests": index["successful_request_count"],
                        "theoretical_minimum": index[
                            "theoretical_minimum_successful_requests"
                        ],
                        "source_identity": index["source_identity"],
                    }
                )
            )
            return 0
        if args.command == "validate":
            require_production_evidence_path(
                Path(args.output_dir), config, "validation output directory"
            )
            args.local_endpoint = args.local_endpoint or config["local"]["endpoint"]
            report = run_validation(args, config, config_path)
            print(
                json.dumps(
                    {
                        "status": report["status"],
                        "points_completed": report["points_completed"],
                        "report": str(Path(args.output_dir) / "report.json"),
                    }
                )
            )
            return 0 if report["status"] == "passed" else 1
        raise ValidationError(f"unsupported command: {args.command}")
    except (ValidationError, OSError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
