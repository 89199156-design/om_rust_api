from __future__ import annotations

import argparse
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, as_completed, wait
from contextlib import nullcontext, redirect_stdout
from copy import copy
from io import StringIO
from itertools import chain
import json
from datetime import datetime, timedelta, timezone
from math import gcd
from pathlib import Path, PurePosixPath
import shutil
import sys
import time
from typing import Any
from urllib.error import HTTPError
from urllib.request import urlopen

from .checksum import sha256_file
from .coverage import (
    build_complete_run_coverage_plan,
    build_product_coverage_plan,
    build_run_native_forecast_hour_coverage_plan,
    required_start_for_anchors,
)
from .locking import file_lock
from .manifest import atomic_write_json, build_latest_manifest, product_config_fingerprint
from .metadata import load_fixture_runs
from .model_config import ProductConfig, load_models
from .om_catalog import (
    DEFAULT_OPENMETEO_BUCKET_URL,
    OpenMeteoSpatialCatalog,
    coverage_object_records,
    discover_openmeteo_spatial_runs,
    load_openmeteo_spatial_latest,
    load_openmeteo_spatial_run,
    om_run_from_spatial_catalog,
    openmeteo_spatial_object_url,
)
from .om_inventory import OmInventory
from .om_product_download import (
    plan_region_for_array,
    plan_variable_range_bundle,
    selected_inventory_variables,
)
from .om_remote import HttpByteRangeSource, load_remote_om_inventory, load_remote_om_inventory_fast
from .om_remote_ranges import plan_remote_array_data_byte_ranges
from .region import bounds_to_dict, grid_spec_for_openmeteo_model, padded_bounds, regular_grid_ranges
from .processing_stage import build_processing_stage
from .mirror_sync import (
    GROUP_PRODUCT_SUMMARY_KEYS,
    activate_group_release,
    group_release_id,
    prune_expired_group_releases,
    retain_group_release_from_mirror,
    sync_from_manifest_path,
    sync_from_manifest_url,
    sync_group_from_mirror,
    sync_retained_group_releases_from_mirror,
)
from .http_range import ByteRange
from .store import write_fixture_om_file, write_http_range_file, write_om_coverage_bundle_file
from .static_assets import (
    OPENMETEO_STATIC_ASSETS,
    static_asset_manifest_record,
)
from .storage_guard import enforce_environment_storage_guard

OPENMETEO_GROUP_PRODUCTS = {
    "gfs": (
        "gfs013_surface",
        "gfs025",
        "gfs_pressure_profile",
        "ncep_gefs025",
        "ncep_gefs05",
    ),
    "cams": ("cams_global", "cams_global_greenhouse_gases"),
    "ecmwf": ("ecmwf_ifs025", "ecmwf_ifs025_ensemble"),
}
GROUPS_REQUIRING_MATCHING_RUNS = frozenset({"gfs"})
GFS_COMPLETE_RUN_RETENTION = 2
GFS_PARTIAL_RUN_RETENTION = 3
GFS_PARTIAL_FORECAST_HOUR_END = 5
GFS_TOTAL_RELEASE_RETENTION = GFS_COMPLETE_RUN_RETENTION + GFS_PARTIAL_RUN_RETENTION
CAMS_COMPLETE_RUN_RETENTION = 3
ECMWF_COMPLETE_RUN_RETENTION = GFS_COMPLETE_RUN_RETENTION
ECMWF_PARTIAL_RUN_RETENTION = GFS_PARTIAL_RUN_RETENTION
ECMWF_PARTIAL_FORECAST_HOUR_END = 6
ECMWF_TOTAL_RELEASE_RETENTION = (
    ECMWF_COMPLETE_RUN_RETENTION + ECMWF_PARTIAL_RUN_RETENTION
)

APP_LOG_RETENTION_DAYS = 45
APP_LOG_MAX_BYTES = 4 * 1024 * 1024


def _effective_group_retention(group: str, requested: int) -> int:
    if group == "gfs":
        return max(requested, GFS_TOTAL_RELEASE_RETENTION)
    if group == "cams":
        return max(requested, CAMS_COMPLETE_RUN_RETENTION)
    if group == "ecmwf":
        return max(requested, ECMWF_TOTAL_RELEASE_RETENTION)
    return requested


def _prepare_group_static_assets(
    group_name: str,
    *,
    output_root: Path,
    bucket_url: str,
) -> tuple[dict[str, dict[str, object]], list[dict[str, object]]]:
    del output_root
    models = OPENMETEO_GROUP_PRODUCTS.get(group_name, ())
    records: dict[str, dict[str, object]] = {}
    results: list[dict[str, object]] = []
    for model in models:
        spec = OPENMETEO_STATIC_ASSETS.get(model)
        if spec is None:
            continue
        record = static_asset_manifest_record(spec, bucket_url=bucket_url)
        records[model] = record
        results.append(
            {
                **record,
                "status": "external",
                "reason": "immutable model elevation is installed on the system disk",
            }
        )
    return records, results


def _group_static_assets_match(
    manifest: dict[str, Any] | None,
    expected: dict[str, dict[str, object]],
) -> bool:
    if not expected:
        return True
    if not manifest:
        return False
    return manifest.get("static_assets") == expected


def _parse_utc(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def _parse_product_reference_time(value: str) -> tuple[str, datetime]:
    try:
        product_name, timestamp = value.split("=", 1)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(
            "product reference time must use PRODUCT=ISO-8601"
        ) from exc
    product_name = product_name.strip()
    timestamp = timestamp.strip()
    if not product_name or not timestamp:
        raise argparse.ArgumentTypeError(
            "product reference time must use PRODUCT=ISO-8601"
        )
    try:
        reference_time = _as_utc(_parse_utc(timestamp))
    except ValueError as exc:
        raise argparse.ArgumentTypeError(
            f"invalid product reference time for {product_name}: {timestamp}"
        ) from exc
    return product_name, reference_time


def _frozen_group_product_reference_times(
    args: argparse.Namespace,
    parser: argparse.ArgumentParser,
    *,
    group_name: str,
    product_names: tuple[str, ...],
) -> dict[str, datetime]:
    entries = list(args.product_reference_time or [])
    if not entries:
        return {}
    if group_name != "ecmwf":
        parser.error("--product-reference-time is supported only for the ecmwf group")
    if args.reference_time:
        parser.error(
            "--reference-time and --product-reference-time cannot be used together"
        )
    references: dict[str, datetime] = {}
    for product_name, reference_time in entries:
        if product_name not in product_names:
            parser.error(
                f"product reference is not part of {group_name}: {product_name}"
            )
        if product_name in references:
            parser.error(f"duplicate product reference time: {product_name}")
        references[product_name] = reference_time
    missing = set(product_names) - set(references)
    if missing:
        parser.error(
            "frozen ECMWF group requires a reference time for every product: "
            + ", ".join(sorted(missing))
        )
    return references


def _as_utc(value: datetime) -> datetime:
    if value.tzinfo is None:
        return value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def _build_region_plan(product: ProductConfig) -> dict[str, Any]:
    grid = grid_spec_for_openmeteo_model(product.openmeteo_model)
    padded = padded_bounds(product.requested_bounds, product.bounds_padding_degrees, grid.bounds)
    spatial_range = regular_grid_ranges(grid, padded)
    return {
        "requested_bounds": bounds_to_dict(product.requested_bounds),
        "padded_bounds": bounds_to_dict(padded),
        "grid_bounds": bounds_to_dict(grid.bounds),
        "grid": grid.as_manifest(),
        "spatial_ranges": [spatial_range],
    }


def _parse_byte_range(value: str) -> ByteRange:
    try:
        start_text, end_text = value.split("-", 1)
        return ByteRange(int(start_text), int(end_text))
    except ValueError as exc:
        raise argparse.ArgumentTypeError("byte range must use start-end format") from exc


def _parse_selection_range(value: str) -> tuple[int, int]:
    try:
        start_text, end_text = value.split(":", 1)
        start = int(start_text)
        end = int(end_text)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("selection must use start:end format") from exc
    if start < 0 or end <= start:
        raise argparse.ArgumentTypeError("selection end must be greater than start")
    return start, end


def _inventory_as_json(inventory: OmInventory) -> dict[str, Any]:
    return {
        "available_variables": list(inventory.available_variables),
        "pressure_levels_hpa": inventory.pressure_levels_hpa,
        "variables": {
            name: {
                "path": item.path,
                "data_type": item.data_type,
                "compression": item.compression,
                "dimensions": list(item.dimensions),
                "chunks": list(item.chunks),
                "lut_offset": item.lut_offset,
                "lut_size": item.lut_size,
                "scale_factor": item.scale_factor,
                "add_offset": item.add_offset,
            }
            for name, item in sorted(inventory.arrays.items())
        },
    }


def _format_utc(value: datetime) -> str:
    return value.strftime("%Y-%m-%dT%H:%M:%SZ")


def _utc_now_text() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _manifest_entry_count(manifest: dict[str, Any]) -> int:
    return sum(
        len(file_record.get("entries") or [])
        for file_record in manifest.get("files") or []
        if isinstance(file_record, dict)
    )


def _append_run_summary(output_root: Path, payload: dict[str, Any]) -> None:
    _append_limited_jsonl_log(output_root, "om_run_summary", payload)


def _append_limited_jsonl_log(output_root: Path, prefix: str, payload: dict[str, Any]) -> None:
    now_utc = datetime.now(timezone.utc)
    record = {"logged_at_utc": _format_utc(now_utc), **payload}
    line = json.dumps(record, ensure_ascii=False, separators=(",", ":")) + "\n"
    line_bytes = len(line.encode("utf-8"))
    log_dir = output_root / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    _cleanup_limited_jsonl_logs(log_dir, prefix=prefix, now_utc=now_utc)
    path = _limited_jsonl_log_path(log_dir, prefix=prefix, now_utc=now_utc, next_bytes=line_bytes)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(line)


def _limited_jsonl_log_path(
    log_dir: Path,
    *,
    prefix: str,
    now_utc: datetime,
    next_bytes: int,
) -> Path:
    date_text = now_utc.strftime("%Y-%m-%d")
    base_path = log_dir / f"{prefix}-{date_text}.jsonl"
    if not base_path.exists() or base_path.stat().st_size + next_bytes <= APP_LOG_MAX_BYTES:
        return base_path
    index = 1
    while True:
        path = log_dir / f"{prefix}-{date_text}.{index}.jsonl"
        if not path.exists() or path.stat().st_size + next_bytes <= APP_LOG_MAX_BYTES:
            return path
        index += 1


def _cleanup_limited_jsonl_logs(log_dir: Path, *, prefix: str, now_utc: datetime) -> None:
    cutoff = now_utc.date().toordinal() - APP_LOG_RETENTION_DAYS
    for path in log_dir.glob(f"{prefix}-*.jsonl"):
        date_text = path.name.removeprefix(f"{prefix}-").split(".", 1)[0]
        try:
            file_date = datetime.strptime(date_text, "%Y-%m-%d").date()
        except ValueError:
            continue
        if file_date.toordinal() < cutoff:
            path.unlink(missing_ok=True)


def _append_product_run_summary(
    output_root: Path,
    *,
    product: ProductConfig,
    manifest: dict[str, Any],
    started_at_utc: str,
    started_monotonic: float,
    reused_existing: bool = False,
) -> None:
    _append_run_summary(
        output_root,
        {
            "kind": "product",
            "product": product.name,
            "status": manifest.get("status"),
            "coverage_id": manifest.get("coverage_id"),
            "latest_complete_run": manifest.get("latest_complete_run"),
            "required_start_utc": manifest.get("required_start_utc"),
            "required_end_utc": manifest.get("required_end_utc"),
            "valid_time_count": manifest.get("valid_time_count"),
            "files": len(manifest.get("files") or []),
            "entries": _manifest_entry_count(manifest),
            "bytes": int(manifest.get("bytes") or 0),
            "downloaded_bytes": int(manifest.get("downloaded_bytes") or 0),
            "reused_existing": reused_existing,
            "started_at_utc": started_at_utc,
            "finished_at_utc": _utc_now_text(),
            "duration_seconds": round(time.monotonic() - started_monotonic, 3),
        },
    )


def _error_is_remote_download_error(exc: BaseException) -> bool:
    message = str(exc).lower()
    markers = (
        "http ",
        "range request",
        "object request",
        "urlopen",
        "url error",
        "timed out",
        "timeout",
        "connection",
        "reset",
        "too many",
        "slowdown",
        "thrott",
        "429",
        "503",
    )
    return any(marker in message for marker in markers)


def _append_product_failure_summary(
    output_root: Path,
    *,
    product: ProductConfig,
    started_at_utc: str,
    started_monotonic: float,
    exc: BaseException,
    coverage_id: str | None = None,
    latest_complete_run: str | None = None,
) -> None:
    _append_run_summary(
        output_root,
        {
            "kind": "product",
            "product": product.name,
            "status": "failed",
            "coverage_id": coverage_id,
            "latest_complete_run": latest_complete_run,
            "files": 0,
            "entries": 0,
            "bytes": 0,
            "downloaded_bytes": 0,
            "reused_existing": False,
            "error_type": type(exc).__name__,
            "error_message": str(exc),
            "remote_error": _error_is_remote_download_error(exc),
            "started_at_utc": started_at_utc,
            "finished_at_utc": _utc_now_text(),
            "duration_seconds": round(time.monotonic() - started_monotonic, 3),
        },
    )


def _append_group_run_summary(
    output_root: Path,
    *,
    group_name: str,
    status: str,
    started_at_utc: str,
    started_monotonic: float,
    group_manifest: dict[str, Any] | None = None,
    product_runs: dict[str, str] | None = None,
    reason: str | None = None,
    cleared_paths: list[str] | None = None,
    exc: BaseException | None = None,
) -> None:
    manifest_products = {}
    if group_manifest is not None:
        manifest_products = group_manifest.get("product_manifests") or {}
    _append_run_summary(
        output_root,
        {
            "kind": "group",
            "group": group_name,
            "status": status,
            "reason": reason,
            "coverage_id": group_manifest.get("coverage_id") if group_manifest else None,
            "latest_complete_run": (
                group_manifest.get("latest_complete_run") if group_manifest else None
            ),
            "products": list(manifest_products),
            "product_runs": product_runs,
            "files": int(group_manifest.get("files") or 0) if group_manifest else 0,
            "bytes": int(group_manifest.get("bytes") or 0) if group_manifest else 0,
            "downloaded_bytes": (
                int(group_manifest.get("downloaded_bytes") or 0) if group_manifest else 0
            ),
            "product_manifests": manifest_products,
            "cleared_published_paths": cleared_paths or [],
            "error_type": type(exc).__name__ if exc else None,
            "error_message": str(exc) if exc else None,
            "remote_error": _error_is_remote_download_error(exc) if exc else False,
            "started_at_utc": started_at_utc,
            "finished_at_utc": _utc_now_text(),
            "duration_seconds": round(time.monotonic() - started_monotonic, 3),
        },
    )


def _forecast_hour_for_run(run: Any, valid_time: datetime) -> int | None:
    valid_time_utc = _as_utc(valid_time)
    if run.valid_times_utc:
        available = {_as_utc(item) for item in run.valid_times_utc}
        if valid_time_utc not in available:
            return None
    delta = valid_time_utc - _as_utc(run.base_time_utc)
    total_seconds = delta.total_seconds()
    if total_seconds < 0 or total_seconds % 3600 != 0:
        return None
    forecast_hour = int(total_seconds // 3600)
    if forecast_hour > run.max_forecast_hour:
        return None
    return forecast_hour


def _missing_variable_fallback_candidates(
    product: ProductConfig,
    runs: list[Any],
    *,
    primary_run: str,
    valid_time: datetime,
) -> list[tuple[str, datetime, int]]:
    """Return newest-first runs that may retain a missing rolling value.

    Some ECMWF variables disappear for an intermediate forecast-hour band and
    reappear on the 6-hour long-range cadence. Open-Meteo's rolling database
    keeps the last older-run value at those valid times. The ordinary coverage
    run list is intentionally small, so an explicitly configured lookback
    probes older cycle/object pairs until the same retained source can be
    captured.
    """
    valid_time_utc = _as_utc(valid_time)
    candidates: list[tuple[str, datetime, int]] = []
    seen: set[str] = set()
    for run in runs:
        if run.run_id == primary_run:
            continue
        forecast_hour = _forecast_hour_for_run(run, valid_time_utc)
        if forecast_hour is None:
            continue
        base_time = _as_utc(run.base_time_utc)
        candidates.append((run.run_id, base_time, forecast_hour))
        seen.add(run.run_id)

    lookback_hours = product.missing_variable_fallback_lookback_hours
    if lookback_hours > 0 and runs:
        cadence_hours = max(1, product.run_cadence_hours)
        oldest_base = min(_as_utc(run.base_time_utc) for run in runs)
        cursor = oldest_base - timedelta(hours=cadence_hours)
        cutoff = oldest_base - timedelta(hours=lookback_hours)
        while cursor >= cutoff:
            run_id = cursor.strftime("%Y%m%d%H")
            delta_seconds = (valid_time_utc - cursor).total_seconds()
            if (
                run_id not in seen
                and delta_seconds >= 0
                and delta_seconds % 3600 == 0
            ):
                forecast_hour = int(delta_seconds // 3600)
                if forecast_hour <= product.forecast_hour_end:
                    candidates.append((run_id, cursor, forecast_hour))
                    seen.add(run_id)
            cursor -= timedelta(hours=cadence_hours)

    return sorted(candidates, key=lambda item: item[1], reverse=True)


def _missing_variable_fallback_context_offsets(context_hours: int) -> tuple[int, ...]:
    """Return every regular three-hour axis offset inside the context window.

    ECMWF's retained long-range frames can be six-hourly, but Open-Meteo first
    regularizes each source run onto a three-hour axis with four-point Hermite
    interpolation. Capturing only the outer endpoints, or only the immediate
    six-hour neighbours, omits the A/D support needed for the same curve.
    """
    if context_hours <= 0:
        return ()
    distances = tuple(range(3, context_hours + 1, 3))
    return tuple(-distance for distance in reversed(distances)) + distances


def _with_interpolation_support_records(
    product: ProductConfig,
    plan: Any,
    runs: list[Any],
    object_records: list[dict[str, Any]],
    *,
    bucket_url: str,
) -> list[dict[str, Any]]:
    """Retain per-run context needed across a stitched ECMWF boundary.

    Open-Meteo first expands each individual IFS run from its native mixed
    3/6-hour cadence onto a regular 3-hour axis, and only then overlays newer
    runs. The public coverage therefore needs a small hidden window on both
    sides of every selected source-run span. In particular, an older long run
    that supplies the far tail needs left lookbehind before its first public
    frame; without it, the regularized boundary contains NaNs and changes
    Hermite values. Right lookahead remains necessary when a newer run takes
    over before an older source run ends.
    """
    records = []
    for record in object_records:
        enriched = dict(record)
        enriched.setdefault("coverage_source_run", record["source_run"])
        enriched.setdefault("coverage_forecast_hour", record["forecast_hour"])
        enriched.setdefault("interpolation_support", False)
        records.append(enriched)
    if product.interpolation_support_hours <= 0:
        return records

    runs_by_id = {run.run_id: run for run in runs}
    selected_spans: list[tuple[str, datetime, datetime]] = []
    current_run: str | None = None
    first_selected: datetime | None = None
    last_selected: datetime | None = None
    for slot in sorted(plan.slots, key=lambda item: _as_utc(item.valid_time_utc)):
        valid_time = _as_utc(slot.valid_time_utc)
        if current_run == slot.source_run:
            last_selected = valid_time
            continue
        if current_run is not None and first_selected is not None and last_selected is not None:
            selected_spans.append((current_run, first_selected, last_selected))
        current_run = slot.source_run
        first_selected = valid_time
        last_selected = valid_time
    if current_run is not None and first_selected is not None and last_selected is not None:
        selected_spans.append((current_run, first_selected, last_selected))
    existing = {
        (str(record["source_run"]), _parse_utc(str(record["valid_time_utc"])))
        for record in records
    }
    support: list[dict[str, Any]] = []
    for run_id, first_valid_time, last_valid_time in selected_spans:
        run = runs_by_id[run_id]
        support_start = first_valid_time - timedelta(
            hours=product.interpolation_support_hours
        )
        support_end = last_valid_time + timedelta(
            hours=product.interpolation_support_hours
        )
        for valid_time in sorted(_as_utc(value) for value in run.valid_times_utc):
            in_left_context = support_start <= valid_time < first_valid_time
            in_right_context = last_valid_time < valid_time <= support_end
            if not (in_left_context or in_right_context):
                continue
            if (run_id, valid_time) in existing:
                continue
            forecast_hour = _forecast_hour_for_run(run, valid_time)
            if (
                forecast_hour is None
                or forecast_hour < product.forecast_hour_start
                or forecast_hour > product.forecast_hour_end
            ):
                continue
            support.append(
                {
                    "valid_time_utc": _format_utc(valid_time),
                    "source_run": run_id,
                    "forecast_hour": forecast_hour,
                    "coverage_source_run": run_id,
                    "coverage_forecast_hour": forecast_hour,
                    "interpolation_support": True,
                    "url": openmeteo_spatial_object_url(
                        bucket_url,
                        product.openmeteo_model,
                        reference_time_utc=run.base_time_utc,
                        valid_time_utc=valid_time,
                    ),
                }
            )
            existing.add((run_id, valid_time))
    support.sort(key=lambda item: (item["source_run"], item["valid_time_utc"]))
    return records + support


def _rolling_time_series_object_records(
    product: ProductConfig,
    plan: Any,
    *,
    bucket_url: str,
) -> list[dict[str, Any]] | None:
    """Plan immutable regional reads from Open-Meteo's rolling time-series.

    The GEFS 0.5° rolling database contains the delayed 00Z extension that is
    intentionally absent from ``data_spatial``.  Those hidden frames are
    required as right-side Hermite control points at the public 16-day tail.
    The rolling database is used only for its own latest run.  Older retained
    GFS releases are immutable spatial runs and must fall back to their own
    ``data_spatial`` objects instead of relabelling values from a newer rolling
    batch.  A rolling database older than the selected run is still rejected.
    """
    meta_url = (
        f"{bucket_url.rstrip('/')}/data/{product.openmeteo_model}/static/meta.json"
    )
    with urlopen(meta_url, timeout=30) as response:
        meta = json.loads(response.read())
    chunk_time_length = int(meta.get("chunk_time_length") or 0)
    dt_seconds = int(meta.get("temporal_resolution_seconds") or 0)
    last_run_epoch = int(meta.get("last_run_initialisation_time") or 0)
    data_end_epoch = int(meta.get("data_end_time") or 0)
    if chunk_time_length <= 0 or dt_seconds <= 0:
        raise ValueError(
            f"rolling time-series metadata is incomplete: {product.openmeteo_model}"
        )
    latest_run_time = datetime.strptime(
        str(plan.latest_complete_run), "%Y%m%d%H"
    ).replace(tzinfo=timezone.utc)
    actual = datetime.fromtimestamp(last_run_epoch, tz=timezone.utc)
    if actual < latest_run_time:
        raise ValueError(
            "rolling time-series batch is older than selected spatial run: "
            f"{product.openmeteo_model} expected={plan.latest_complete_run} "
            f"actual={actual:%Y%m%d%H}"
        )
    if actual > latest_run_time:
        return None

    slots = sorted(plan.slots, key=lambda item: _as_utc(item.valid_time_utc))
    if not slots:
        raise ValueError(f"rolling time-series plan is empty: {product.name}")
    first = _as_utc(slots[0].valid_time_utc) - timedelta(seconds=dt_seconds)
    # One extra regular frame is enough: the final public hourly interval has
    # B at -3h, C at the latest spatial frame and D at +3h.
    last = _as_utc(slots[-1].valid_time_utc) + timedelta(seconds=dt_seconds)
    if int(last.timestamp()) > data_end_epoch:
        raise ValueError(
            f"rolling time-series does not include interpolation lookahead: "
            f"{product.openmeteo_model} required={_format_utc(last)}"
        )

    public_times = {_as_utc(slot.valid_time_utc) for slot in slots}
    aliases_by_chunk: dict[int, list[dict[str, Any]]] = {}
    cursor = first
    while cursor <= last:
        timestamp = int(cursor.timestamp())
        if timestamp % dt_seconds != 0:
            raise ValueError("rolling time-series timestamps are not aligned")
        absolute_index = timestamp // dt_seconds
        chunk_index, native_time_index = divmod(
            absolute_index, chunk_time_length
        )
        forecast_hour = int((cursor - latest_run_time).total_seconds() // 3600)
        aliases_by_chunk.setdefault(chunk_index, []).append(
            {
                "valid_time_utc": _format_utc(cursor),
                "source_run": str(plan.latest_complete_run),
                "forecast_hour": forecast_hour,
                "coverage_source_run": str(plan.latest_complete_run),
                "coverage_forecast_hour": forecast_hour,
                "interpolation_support": cursor not in public_times,
                "native_time_index": native_time_index,
            }
        )
        cursor += timedelta(seconds=dt_seconds)

    records = []
    variable = product.required_variables[0]
    for chunk_index, aliases in sorted(aliases_by_chunk.items()):
        records.append(
            {
                **aliases[0],
                "rolling_time_series": True,
                "rolling_variable": variable,
                "rolling_time_range": [
                    min(int(item["native_time_index"]) for item in aliases),
                    max(int(item["native_time_index"]) for item in aliases) + 1,
                ],
                "rolling_aliases": aliases,
                "url": (
                    f"{bucket_url.rstrip('/')}/data/{product.openmeteo_model}/"
                    f"{variable}/chunk_{chunk_index}.om"
                ),
            }
        )
    return records


def _product_coverage_object_records(
    product: ProductConfig,
    plan: Any,
    runs: list[Any],
    *,
    bucket_url: str,
) -> list[dict[str, Any]]:
    if product.source_mode == "rolling_time_series":
        rolling_records = _rolling_time_series_object_records(
            product,
            plan,
            bucket_url=bucket_url,
        )
        if rolling_records is not None:
            return rolling_records

    records = coverage_object_records(
        plan,
        runs,
        bucket_url=bucket_url,
        openmeteo_model=product.openmeteo_model,
    )
    return _with_interpolation_support_records(
        product,
        plan,
        runs,
        records,
        bucket_url=bucket_url,
    )


def _read_json_if_exists(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def _build_product_plan(
    product: ProductConfig,
    *,
    now_utc: datetime,
    bucket_url: str,
    reference_time_utc: datetime | None = None,
) -> tuple[Any, list[Any], Any]:
    latest_catalog = (
        load_openmeteo_spatial_run(
            product.openmeteo_model,
            reference_time_utc,
            bucket_url=bucket_url,
        )
        if reference_time_utc is not None
        else load_openmeteo_spatial_latest(
            product.openmeteo_model,
            bucket_url=bucket_url,
        )
    )
    return _build_product_plan_from_catalog(
        product,
        latest_catalog,
        now_utc=now_utc,
        bucket_url=bucket_url,
    )


def _build_product_plan_from_catalog(
    product: ProductConfig,
    latest_catalog: OpenMeteoSpatialCatalog,
    *,
    now_utc: datetime,
    bucket_url: str,
) -> tuple[Any, list[Any], Any]:
    if not latest_catalog.completed:
        raise ValueError(
            f"Open-Meteo spatial run is not complete: "
            f"{product.openmeteo_model} {latest_catalog.reference_time_utc.isoformat()}"
        )
    required_start = required_start_for_anchors(
        now_utc,
        product.timezone_anchors,
    ) - timedelta(hours=product.history_hours)
    required_long_run_forecast_hour = (
        product.forecast_hour_end
        if product.coverage_strategy == "latest_with_long_run_tail"
        else None
    )
    runs = discover_openmeteo_spatial_runs(
        product.name,
        latest_catalog,
        bucket_url=bucket_url,
        required_start_utc=required_start,
        run_cadence_hours=product.run_cadence_hours,
        required_long_run_forecast_hour=required_long_run_forecast_hour,
    )
    return latest_catalog, runs, build_product_coverage_plan(product, runs, now_utc)


def _coverage_id_for_plan(product: ProductConfig, plan: Any) -> str:
    return f"{product.name}_{plan.latest_complete_run}_{len(plan.slots)}h"


def _safe_manifest_file_path(output_root: Path, product: ProductConfig, file_record: dict[str, Any]) -> Path | None:
    raw_path = str(file_record.get("path") or "")
    if not raw_path or raw_path.endswith(".tmp"):
        return None
    pure = PurePosixPath(raw_path)
    if pure.is_absolute() or ".." in pure.parts:
        return None
    return output_root / "published" / product.name / Path(*pure.parts)


def _manifest_matches_plan(
    manifest: dict[str, Any] | None,
    plan: Any,
    product: ProductConfig,
    output_root: Path,
) -> bool:
    if not _manifest_identity_matches_plan(manifest, plan, product):
        return False
    files = manifest.get("files") if manifest else None
    if not isinstance(files, list) or not files:
        return False
    total_bytes = 0
    total_downloaded_bytes = 0
    for file_record in files:
        if not isinstance(file_record, dict):
            return False
        path = _safe_manifest_file_path(output_root, product, file_record)
        if path is None or not path.exists() or path.name.endswith(".tmp"):
            return False
        if int(file_record.get("bytes") or -1) != path.stat().st_size:
            return False
        if str(file_record.get("sha256") or "") != sha256_file(path):
            return False
        entries = file_record.get("entries")
        if file_record.get("kind") == "om_coverage_bundle" and (not isinstance(entries, list) or not entries):
            return False
        file_bytes = int(file_record.get("bytes") or 0)
        downloaded_bytes = int(file_record.get("downloaded_bytes") or 0)
        if file_bytes <= 0 or downloaded_bytes < file_bytes:
            return False
        total_bytes += file_bytes
        total_downloaded_bytes += downloaded_bytes
    if int(manifest.get("bytes") or 0) != total_bytes:
        return False
    if int(manifest.get("downloaded_bytes") or 0) < total_downloaded_bytes:
        return False
    return True


def _manifest_identity_matches_plan(
    manifest: dict[str, Any] | None,
    plan: Any,
    product: ProductConfig,
) -> bool:
    if not manifest:
        return False
    coverage_id = _coverage_id_for_plan(product, plan)
    if manifest.get("status") != "complete":
        return False
    if manifest.get("model") != product.name:
        return False
    if manifest.get("coverage_id") != coverage_id:
        return False
    if manifest.get("config_fingerprint") != product_config_fingerprint(product):
        return False
    if manifest.get("latest_complete_run") != plan.latest_complete_run:
        return False
    if manifest.get("required_start_utc") != _format_utc(plan.required_start_utc):
        return False
    if manifest.get("public_start_utc") != _format_utc(plan.public_start_utc or plan.required_start_utc):
        return False
    if manifest.get("required_end_utc") != _format_utc(plan.required_end_utc):
        return False
    if manifest.get("valid_time_count") != len(plan.slots):
        return False
    files = manifest.get("files")
    if not isinstance(files, list) or not files:
        return False
    for file_record in files:
        if not isinstance(file_record, dict):
            return False
        entries = file_record.get("entries")
        if file_record.get("kind") == "om_coverage_bundle" and (not isinstance(entries, list) or not entries):
            return False
        file_bytes = int(file_record.get("bytes") or 0)
        downloaded_bytes = int(file_record.get("downloaded_bytes") or 0)
        if file_bytes <= 0 or downloaded_bytes < file_bytes:
            return False
    if int(manifest.get("bytes") or 0) <= 0:
        return False
    if int(manifest.get("downloaded_bytes") or 0) < int(manifest.get("bytes") or 0):
        return False
    return True


def _manifest_with_reuse_flags(manifest: dict[str, Any]) -> dict[str, Any]:
    payload = json.loads(json.dumps(manifest))
    for file_record in payload.get("files") or []:
        if isinstance(file_record, dict):
            file_record["reused_existing"] = True
    return payload


def _plan_requires_initial_fallback(plan: Any) -> bool:
    return any(
        slot.source_run == plan.latest_complete_run and int(slot.forecast_hour) == 0
        for slot in plan.slots
    )


def _repair_non_applicable_initial_fallback_manifest(
    manifest: dict[str, Any] | None,
    plan: Any,
    product: ProductConfig,
    output_root: Path,
) -> dict[str, Any] | None:
    """Reuse a valid bundle rejected only by an inapplicable f000 fallback gate."""
    if (
        not manifest
        or manifest.get("status") != "incomplete"
        or _plan_requires_initial_fallback(plan)
        or set(manifest.get("missing_initial_fallback_variables") or ())
        != set(product.required_initial_fallback_variables)
        or manifest.get("missing_bundle_required_variables")
        or manifest.get("missing_pressure_levels_hpa")
    ):
        return manifest
    repaired = json.loads(json.dumps(manifest))
    repaired["initial_fallback_requirement_applies"] = False
    repaired["missing_initial_fallback_variables"] = []
    repaired["status"] = "complete"
    if not _manifest_matches_plan(repaired, plan, product, output_root):
        return manifest
    atomic_write_json(
        output_root / "published" / product.name / "latest.json",
        repaired,
    )
    return repaired


def _clear_group_published_data(
    output_root: Path,
    *,
    group_name: str,
    product_names: list[str],
) -> list[str]:
    published_root = output_root / "published"
    targets = [published_root / product_name for product_name in product_names]
    targets.append(published_root / "groups" / group_name)
    cleared: list[str] = []
    resolved_root = published_root.resolve(strict=False)
    for target in targets:
        resolved_target = target.resolve(strict=False)
        try:
            resolved_target.relative_to(resolved_root)
        except ValueError as exc:
            raise ValueError(f"refusing to clear path outside published root: {target}") from exc
        if resolved_target == resolved_root:
            raise ValueError(f"refusing to clear published root: {target}")
        if target.exists():
            shutil.rmtree(target)
            cleared.append(str(target))
    return cleared


def _api_group_ready_matches(
    group_manifest: dict[str, Any] | None,
    *,
    api_root: Path,
    group_name: str,
) -> bool:
    if not group_manifest or group_manifest.get("status") != "complete":
        return False
    ready_path = api_root / "groups" / group_name / "current" / "ready_for_processing.json"
    ready = _read_json_if_exists(ready_path)
    if not ready or ready.get("status") != "complete":
        return False
    if ready.get("latest_complete_run") != group_manifest.get("latest_complete_run"):
        return False
    group_products = group_manifest.get("product_manifests") or {}
    ready_products = ready.get("product_manifests") or {}
    if not isinstance(group_products, dict) or not isinstance(ready_products, dict):
        return False
    for product, group_summary in group_products.items():
        if not isinstance(group_summary, dict):
            return False
        ready_summary = ready_products.get(product)
        if not isinstance(ready_summary, dict):
            return False
        for key in GROUP_PRODUCT_SUMMARY_KEYS:
            if ready_summary.get(key) != group_summary.get(key):
                return False
        product_ready = _read_json_if_exists(
            api_root / product / "current" / "ready_for_processing.json"
        )
        if not product_ready or product_ready.get("coverage_id") != group_summary.get("coverage_id"):
            return False
    return True


def _api_product_current_matches_manifest(
    manifest: dict[str, Any] | None,
    *,
    api_root: Path,
    product_name: str,
) -> bool:
    if not manifest:
        return False
    current_root = api_root / product_name / "current"
    ready = _read_json_if_exists(current_root / "ready_for_processing.json")
    current_manifest = _read_json_if_exists(current_root / "latest.json")
    if not ready or not current_manifest:
        return False
    if ready.get("coverage_id") != manifest.get("coverage_id"):
        return False
    for key in (
        "model",
        "status",
        "coverage_id",
        "config_fingerprint",
        "latest_complete_run",
        "required_start_utc",
        "required_end_utc",
        "valid_time_count",
        "bytes",
        "downloaded_bytes",
    ):
        if current_manifest.get(key) != manifest.get(key):
            return False
    current_files = current_manifest.get("files")
    manifest_files = manifest.get("files")
    return isinstance(current_files, list) and isinstance(manifest_files, list) and len(current_files) == len(manifest_files)


def _published_group_identity_matches_plan(
    group_manifest: dict[str, Any] | None,
    *,
    group_name: str,
    products: list[ProductConfig],
    plan_by_product: dict[str, tuple[Any, list[Any], Any]],
    output_root: Path,
) -> bool:
    if not group_manifest:
        return False
    if group_manifest.get("group") != group_name or group_manifest.get("status") != "complete":
        return False
    product_summaries = group_manifest.get("product_manifests") or {}
    if not isinstance(product_summaries, dict):
        return False
    for product in products:
        if product.name not in product_summaries:
            return False
        _, _, plan = plan_by_product[product.name]
        manifest = _read_json_if_exists(output_root / "published" / product.name / "latest.json")
        if not _manifest_identity_matches_plan(manifest, plan, product):
            return False
        summary = product_summaries.get(product.name)
        if not isinstance(summary, dict):
            return False
        for manifest_key, summary_key in (
            ("coverage_id", "coverage_id"),
            ("latest_complete_run", "latest_complete_run"),
            ("required_start_utc", "required_start_utc"),
            ("required_end_utc", "required_end_utc"),
            ("valid_time_count", "valid_time_count"),
            ("bytes", "bytes"),
            ("downloaded_bytes", "downloaded_bytes"),
        ):
            if manifest and summary.get(summary_key) != manifest.get(manifest_key):
                return False
    expected_runs = {
        plan.latest_complete_run for _latest_catalog, _runs, plan in plan_by_product.values()
    }
    expected_group_run = next(iter(expected_runs)) if len(expected_runs) == 1 else None
    return group_manifest.get("latest_complete_run") == expected_group_run


def _clear_group_download_payloads(
    output_root: Path,
    *,
    product_names: list[str],
) -> list[str]:
    published_root = output_root / "published"
    targets: list[Path] = []
    for product_name in product_names:
        product_root = published_root / product_name
        targets.extend([product_root / "coverages", product_root / ".incoming"])
    cleared: list[str] = []
    resolved_root = published_root.resolve(strict=False)
    for target in targets:
        resolved_target = target.resolve(strict=False)
        try:
            resolved_target.relative_to(resolved_root)
        except ValueError as exc:
            raise ValueError(f"refusing to clear path outside published root: {target}") from exc
        if target.exists():
            shutil.rmtree(target)
            cleared.append(str(target))
    return cleared


def _log_download_stage(
    stage: str,
    *,
    product: ProductConfig,
    coverage_id: str,
    planned_entries: int = 0,
    planned_ranges: int = 0,
    downloaded_ranges: int = 0,
    written_bytes: int = 0,
    active_workers: int = 0,
    range_workers: int = 0,
    planning_workers: int = 0,
    variable_plan_workers: int = 0,
    reused_existing: bool = False,
) -> None:
    stage_text = {
        "planning": "开始分析下载范围",
        "manifest": "完成文件并生成清单",
        "reused": "复用已有完整文件",
    }.get(stage, stage)
    product_text = {
        "gfs013_surface": "GFS 0.13°地面层",
        "gfs025": "GFS 0.25°地面层",
        "gfs_pressure_profile": "GFS 气压层",
        "ncep_gefs025": "GEFS 0.25°降水概率",
        "ncep_gefs05": "GEFS 0.5°降水概率",
        "ecmwf_ifs025": "ECMWF IFS 0.25°",
        "ecmwf_ifs025_ensemble": "ECMWF IFS 0.25°集合降水概率",
        "cams_global": "CAMS 全球空气质量",
        "cams_global_greenhouse_gases": "CAMS 温室气体",
    }.get(product.name, product.name)
    print(
        f"阶段：{stage_text}｜产品：{product_text}｜批次：{coverage_id}",
        file=sys.stderr,
        flush=True,
    )


def _download_openmeteo_product(
    product: ProductConfig,
    *,
    now_utc: datetime,
    output_root: Path,
    bucket_url: str,
    lut_codec: str,
    download_workers: int,
    planning_workers: int | None,
    range_workers: int | None,
    range_io_merge_gap: int,
    range_io_size_max: int | None,
    object_fetch_mode: str = "range",
    object_fetch_max_multiplier: float = 3.0,
    object_fetch_min_ranges: int = 16,
    object_range_merge_gap: int = 16 * 1024 * 1024,
    object_range_max_multiplier: float = 2.0,
    object_range_min_ranges: int = 16,
    object_range_max_bytes: int | None = None,
    reference_time_utc: datetime | None = None,
    plan_data: tuple[Any, list[Any], Any] | None = None,
) -> dict[str, Any]:
    run_started_at_utc = _utc_now_text()
    run_started_monotonic = time.monotonic()
    if download_workers < 1:
        raise ValueError("download_workers must be at least 1")
    if planning_workers is None:
        planning_workers = download_workers
    if range_workers is None:
        range_workers = download_workers
    if planning_workers < 1:
        raise ValueError("planning_workers must be at least 1")
    if range_workers < 1:
        raise ValueError("range_workers must be at least 1")
    if range_io_merge_gap < 0:
        raise ValueError("range_io_merge_gap must be non-negative")
    variable_plan_workers = max(1, range_workers // max(1, planning_workers))
    if plan_data is None:
        _, runs, plan = _build_product_plan(
            product,
            now_utc=now_utc,
            bucket_url=bucket_url,
            reference_time_utc=reference_time_utc,
        )
    else:
        _, runs, plan = plan_data
    coverage_id = _coverage_id_for_plan(product, plan)
    existing_manifest_path = output_root / "published" / product.name / "latest.json"
    existing_manifest = _read_json_if_exists(existing_manifest_path)
    existing_manifest = _repair_non_applicable_initial_fallback_manifest(
        existing_manifest,
        plan,
        product,
        output_root,
    )
    if _manifest_matches_plan(existing_manifest, plan, product, output_root):
        reused_manifest = _manifest_with_reuse_flags(existing_manifest)
        atomic_write_json(existing_manifest_path, reused_manifest)
        _log_download_stage(
            "reused",
            product=product,
            coverage_id=coverage_id,
            planned_entries=sum(len(file_record.get("entries") or []) for file_record in reused_manifest.get("files") or []),
            planned_ranges=sum(
                len(entry.get("byte_ranges") or [])
                for file_record in reused_manifest.get("files") or []
                for entry in file_record.get("entries") or []
            ),
            written_bytes=int(reused_manifest.get("bytes") or 0),
            range_workers=range_workers,
            planning_workers=planning_workers,
            variable_plan_workers=variable_plan_workers,
            reused_existing=True,
        )
        _append_product_run_summary(
            output_root,
            product=product,
            manifest=reused_manifest,
            started_at_utc=run_started_at_utc,
            started_monotonic=run_started_monotonic,
            reused_existing=True,
        )
        return reused_manifest

    object_records = _product_coverage_object_records(
        product,
        plan,
        runs,
        bucket_url=bucket_url,
    )
    region_plan: dict[str, Any] | None = None
    missing_object_required_variables = []
    wanted_variables = tuple(
        dict.fromkeys(
            list(product.required_variables)
            + list(product.required_sparse_variables)
            + list(product.optional_variables)
        )
    )
    runs_by_id = {run.run_id: run for run in runs}
    fallback_inventory_cache: dict[str, tuple[HttpByteRangeSource, int, OmInventory]] = {}

    def inventory_for_url(url: str, wanted: tuple[str, ...]) -> tuple[HttpByteRangeSource, int, OmInventory]:
        cached = fallback_inventory_cache.get(url)
        if cached is not None:
            return cached
        source = HttpByteRangeSource(url)
        remote_content_length = source.content_length()
        inventory = load_remote_om_inventory_fast(
            source,
            wanted,
            metadata_workers=planning_workers,
        )
        cached = (source, remote_content_length, inventory)
        fallback_inventory_cache[url] = cached
        return cached

    def plan_object_entries(object_record: dict[str, Any]) -> dict[str, Any]:
        if object_record.get("rolling_time_series"):
            source = HttpByteRangeSource(object_record["url"])
            remote_content_length = source.content_length()
            inventory = load_remote_om_inventory(source)
            array = inventory.arrays.get("")
            if array is None:
                raise ValueError(
                    f"rolling OM chunk has no root array: {object_record['url']}"
                )
            if len(array.dimensions) != 3 or len(array.chunks) != 3:
                raise ValueError(
                    f"rolling OM chunk is not a three-dimensional time series: "
                    f"{object_record['url']}"
                )
            object_region_plan, spatial_ranges = plan_region_for_array(
                product, array
            )
            time_range = tuple(int(value) for value in object_record["rolling_time_range"])
            bundle = plan_variable_range_bundle(
                source,
                array,
                selection_ranges=spatial_ranges + (time_range,),
                lut_codec=lut_codec,
                lut_workers=planning_workers,
                io_size_merge=range_io_merge_gap,
                io_size_max=range_io_size_max,
            )
            bundle["variable"] = str(object_record["rolling_variable"])
            bundle["path"] = ""
            bundle["manifest_selection_ranges"] = [
                list(item) for item in spatial_ranges
            ]
            return {
                "object_record": object_record,
                "region_plan": object_region_plan,
                "missing_required_variables": [],
                "entries": [
                    {
                        "object_record": object_record,
                        "bundle": bundle,
                        "source_url": object_record["url"],
                        "remote_content_length": remote_content_length,
                    }
                ],
            }
        source, remote_content_length, inventory = inventory_for_url(
            object_record["url"],
            wanted_variables,
        )
        coverage_forecast_hour = int(
            object_record.get("coverage_forecast_hour", object_record["forecast_hour"])
        )
        forced_fallback = (
            set(product.required_initial_fallback_variables)
            if coverage_forecast_hour == 0
            and not bool(object_record.get("interpolation_support"))
            else set()
        )
        missing_for_object = sorted(
            (set(product.required_variables) - set(inventory.arrays)) | forced_fallback
        )
        entries = []
        object_region_plan = None

        def plan_inventory_variable(
            variable: str,
            variable_source: HttpByteRangeSource,
            variable_inventory: OmInventory,
            variable_object_record: dict[str, Any],
            variable_source_url: str,
            variable_remote_content_length: int,
        ) -> tuple[dict[str, Any], dict[str, Any]]:
            array = variable_inventory.arrays[variable]
            current_region_plan, selection_ranges = plan_region_for_array(product, array)
            return current_region_plan, {
                "object_record": variable_object_record,
                "bundle": plan_variable_range_bundle(
                    variable_source,
                    array,
                    selection_ranges=selection_ranges,
                    lut_codec=lut_codec,
                    lut_workers=1,
                    io_size_merge=range_io_merge_gap,
                    io_size_max=range_io_size_max,
                ),
                "source_url": variable_source_url,
                "remote_content_length": variable_remote_content_length,
            }

        def append_planned_variables(
            variables: tuple[str, ...],
            variable_source: HttpByteRangeSource,
            variable_inventory: OmInventory,
            variable_object_record: dict[str, Any],
            variable_source_url: str,
            variable_remote_content_length: int,
        ) -> None:
            nonlocal object_region_plan
            if not variables:
                return
            workers = min(variable_plan_workers, len(variables))
            if workers <= 1:
                for variable in variables:
                    current_region_plan, entry = plan_inventory_variable(
                        variable,
                        variable_source,
                        variable_inventory,
                        variable_object_record,
                        variable_source_url,
                        variable_remote_content_length,
                    )
                    if object_region_plan is None:
                        object_region_plan = current_region_plan
                    entries.append(entry)
                return

            completed: dict[int, tuple[dict[str, Any], dict[str, Any]]] = {}
            with ThreadPoolExecutor(max_workers=workers) as executor:
                future_orders = {
                    executor.submit(
                        plan_inventory_variable,
                        variable,
                        variable_source,
                        variable_inventory,
                        variable_object_record,
                        variable_source_url,
                        variable_remote_content_length,
                    ): order
                    for order, variable in enumerate(variables)
                }
                for future in as_completed(future_orders):
                    completed[future_orders[future]] = future.result()
            for order in range(len(variables)):
                current_region_plan, entry = completed[order]
                if object_region_plan is None:
                    object_region_plan = current_region_plan
                entries.append(entry)

        append_planned_variables(
            tuple(
                variable
                for variable in selected_inventory_variables(product, inventory)
                if variable not in forced_fallback
            ),
            source,
            inventory,
            object_record,
            object_record["url"],
            remote_content_length,
        )
        if missing_for_object:
            remaining_missing = set(missing_for_object)
            predecessor_support_missing: set[str] = set()
            valid_time = _parse_utc(str(object_record["valid_time_utc"]))
            primary_run = str(object_record["source_run"])
            fallback_runs = _missing_variable_fallback_candidates(
                product,
                runs,
                primary_run=primary_run,
                valid_time=valid_time,
            )

            def append_fallback_source_variables(
                variables: tuple[str, ...],
                fallback_source: Any,
                fallback_inventory: Any,
                fallback_run_id: str,
                fallback_base_time: datetime,
                fallback_forecast_hour: int,
                fallback_url: str,
                fallback_content_length: int | None,
                *,
                interpolation_support: bool,
            ) -> None:
                if not variables:
                    return
                fallback_object_record = {
                    "valid_time_utc": object_record["valid_time_utc"],
                    "source_run": fallback_run_id,
                    "forecast_hour": fallback_forecast_hour,
                    "coverage_source_run": object_record.get(
                        "coverage_source_run", primary_run
                    ),
                    "coverage_forecast_hour": coverage_forecast_hour,
                    "interpolation_support": interpolation_support,
                }
                append_planned_variables(
                    variables,
                    fallback_source,
                    fallback_inventory,
                    fallback_object_record,
                    fallback_url,
                    fallback_content_length,
                )
                context_hours = product.missing_variable_fallback_context_hours
                if context_hours <= 0:
                    return
                for context_offset in _missing_variable_fallback_context_offsets(
                    context_hours
                ):
                    context_valid_time = valid_time + timedelta(hours=context_offset)
                    context_forecast_hour = fallback_forecast_hour + context_offset
                    context_coverage_forecast_hour = (
                        coverage_forecast_hour + context_offset
                    )
                    if (
                        context_forecast_hour < 0
                        or context_forecast_hour > product.forecast_hour_end
                        or context_coverage_forecast_hour < 0
                    ):
                        continue
                    context_url = openmeteo_spatial_object_url(
                        bucket_url,
                        product.openmeteo_model,
                        reference_time_utc=fallback_base_time,
                        valid_time_utc=context_valid_time,
                    )
                    try:
                        (
                            context_source,
                            context_content_length,
                            context_inventory,
                        ) = inventory_for_url(context_url, variables)
                    except Exception:
                        continue
                    context_variables = tuple(
                        variable
                        for variable in variables
                        if variable in context_inventory.arrays
                    )
                    context_object_record = {
                        "valid_time_utc": _format_utc(context_valid_time),
                        "source_run": fallback_run_id,
                        "forecast_hour": context_forecast_hour,
                        "coverage_source_run": object_record.get(
                            "coverage_source_run", primary_run
                        ),
                        "coverage_forecast_hour": context_coverage_forecast_hour,
                        "interpolation_support": True,
                    }
                    append_planned_variables(
                        context_variables,
                        context_source,
                        context_inventory,
                        context_object_record,
                        context_url,
                        context_content_length,
                    )

            variable_order = tuple(
                dict.fromkeys(
                    tuple(product.required_variables)
                    + tuple(product.required_initial_fallback_variables)
                )
            )
            for fallback_run_id, fallback_base_time, fallback_forecast_hour in fallback_runs:
                if not remaining_missing and not predecessor_support_missing:
                    break
                fallback_wanted_variables = tuple(
                    variable
                    for variable in variable_order
                    if variable in remaining_missing
                    or variable in predecessor_support_missing
                )
                fallback_url = openmeteo_spatial_object_url(
                    bucket_url,
                    product.openmeteo_model,
                    reference_time_utc=fallback_base_time,
                    valid_time_utc=valid_time,
                )
                try:
                    _fallback_source, fallback_content_length, fallback_inventory = inventory_for_url(
                        fallback_url,
                        fallback_wanted_variables,
                    )
                except Exception:
                    continue
                fallback_variables = tuple(
                    variable
                    for variable in variable_order
                    if variable in remaining_missing and variable in fallback_inventory.arrays
                )
                predecessor_variables = tuple(
                    variable
                    for variable in variable_order
                    if variable in predecessor_support_missing
                    and variable in fallback_inventory.arrays
                )
                append_fallback_source_variables(
                    fallback_variables,
                    _fallback_source,
                    fallback_inventory,
                    fallback_run_id,
                    fallback_base_time,
                    fallback_forecast_hour,
                    fallback_url,
                    fallback_content_length,
                    interpolation_support=bool(
                        object_record.get("interpolation_support")
                    ),
                )
                append_fallback_source_variables(
                    predecessor_variables,
                    _fallback_source,
                    fallback_inventory,
                    fallback_run_id,
                    fallback_base_time,
                    fallback_forecast_hour,
                    fallback_url,
                    fallback_content_length,
                    interpolation_support=True,
                )
                remaining_missing.difference_update(fallback_variables)
                predecessor_support_missing.difference_update(predecessor_variables)
                if product.missing_variable_fallback_predecessor_runs > 0:
                    predecessor_support_missing.update(fallback_variables)
                missing_for_object = sorted(remaining_missing)
        return {
            "object_record": object_record,
            "region_plan": object_region_plan,
            "missing_required_variables": missing_for_object,
            "entries": entries,
        }

    def consume_object_result(result: dict[str, Any]):
        nonlocal region_plan
        if region_plan is None and result["region_plan"] is not None:
            region_plan = result["region_plan"]
        missing_for_object = result["missing_required_variables"]
        if missing_for_object:
            object_record = result["object_record"]
            missing_object_required_variables.append(
                {
                    "valid_time_utc": object_record["valid_time_utc"],
                    "source_run": object_record["source_run"],
                    "missing_required_variables": missing_for_object,
                }
            )
        yield from result["entries"]

    def iter_planned_entries():
        if planning_workers == 1:
            for object_record in object_records:
                yield from consume_object_result(plan_object_entries(object_record))
            return

        pending = set()
        future_orders = {}
        completed: dict[int, dict[str, Any]] = {}
        next_order = 0

        def yield_ready_objects():
            nonlocal next_order
            while next_order in completed:
                result = completed.pop(next_order)
                yield from consume_object_result(result)
                next_order += 1

        with ThreadPoolExecutor(max_workers=planning_workers) as executor:
            for order, object_record in enumerate(object_records):
                future = executor.submit(plan_object_entries, object_record)
                pending.add(future)
                future_orders[future] = order
                if len(pending) >= planning_workers:
                    done, pending = wait(pending, return_when=FIRST_COMPLETED)
                    for future in done:
                        completed[future_orders.pop(future)] = future.result()
                    yield from yield_ready_objects()
            while pending:
                done, pending = wait(pending, return_when=FIRST_COMPLETED)
                for future in done:
                    completed[future_orders.pop(future)] = future.result()
                yield from yield_ready_objects()

    def iter_unique_planned_entries():
        seen: set[tuple[Any, ...]] = set()
        for entry in iter_planned_entries():
            record = entry["object_record"]
            key = (
                entry["bundle"]["variable"],
                record["source_run"],
                record["valid_time_utc"],
                int(record["forecast_hour"]),
                record.get("coverage_source_run"),
                int(record.get("coverage_forecast_hour", record["forecast_hour"])),
            )
            if key in seen:
                continue
            seen.add(key)
            yield entry

    files = []
    planned_entries = iter_unique_planned_entries()
    try:
        first_entry = next(planned_entries)
    except StopIteration:
        first_entry = None
    if first_entry is not None:
        _log_download_stage(
            "planning",
            product=product,
            coverage_id=coverage_id,
            active_workers=planning_workers,
            range_workers=range_workers,
            planning_workers=planning_workers,
            variable_plan_workers=variable_plan_workers,
        )
        files.append(
            write_om_coverage_bundle_file(
                output_root,
                product.name,
                coverage_id,
                chain((first_entry,), planned_entries),
                download_workers=download_workers,
                range_workers=range_workers,
                progress_context={
                    "product": product.name,
                    "coverage_id": coverage_id,
                    "progress_interval_seconds": 60,
                    "range_workers": range_workers,
                    "planning_workers": planning_workers,
                    "variable_plan_workers": variable_plan_workers,
                },
                object_fetch_mode=object_fetch_mode,
                object_fetch_max_multiplier=object_fetch_max_multiplier,
                object_fetch_min_ranges=object_fetch_min_ranges,
                object_range_merge_gap=object_range_merge_gap,
                object_range_max_multiplier=object_range_max_multiplier,
                object_range_min_ranges=object_range_min_ranges,
                object_range_max_bytes=object_range_max_bytes,
            )
        )
    if (
        object_records
        and object_records[0].get("rolling_time_series")
        and files
    ):
        base_entries = {
            str(entry.get("source_url")): entry
            for entry in files[0].get("entries") or []
        }
        aliases = []
        for object_record in object_records:
            base = base_entries.get(str(object_record["url"]))
            if base is None:
                raise ValueError(
                    f"rolling bundle entry is missing: {object_record['url']}"
                )
            for alias in object_record["rolling_aliases"]:
                expanded = dict(base)
                expanded.update(alias)
                aliases.append(expanded)
        aliases.sort(
            key=lambda entry: (
                str(entry["valid_time_utc"]),
                str(entry["source_run"]),
            )
        )
        files[0]["entries"] = aliases
    if region_plan is None:
        region_plan = _build_region_plan(product)
    manifest = build_latest_manifest(product, runs, plan, files, region_plan)
    catalog_missing_required_variables = manifest["missing_required_variables"]
    manifest["missing_object_required_variables"] = missing_object_required_variables
    required_variables = set(product.required_variables) | set(product.required_sparse_variables)
    downloaded_required_variables = {
        str(entry.get("variable"))
        for file_record in files
        for entry in file_record.get("entries") or []
        if entry.get("variable") in required_variables
    }
    missing_bundle_required_variables = sorted(required_variables - downloaded_required_variables)
    manifest["catalog_missing_required_variables"] = catalog_missing_required_variables
    manifest["missing_required_variables"] = missing_bundle_required_variables
    manifest["missing_bundle_required_variables"] = missing_bundle_required_variables
    initial_substitutions = {
        str(entry.get("variable")): {
            "valid_time_utc": entry.get("valid_time_utc"),
            "coverage_source_run": entry.get("coverage_source_run"),
            "coverage_forecast_hour": entry.get("coverage_forecast_hour"),
            "source_run": entry.get("source_run"),
            "forecast_hour": entry.get("forecast_hour"),
        }
        for file_record in files
        for entry in file_record.get("entries") or []
        if entry.get("variable") in product.required_initial_fallback_variables
        and entry.get("coverage_source_run") == plan.latest_complete_run
        and int(entry.get("coverage_forecast_hour", -1)) == 0
        and entry.get("source_run") != plan.latest_complete_run
        and int(entry.get("forecast_hour", -1)) == product.run_cadence_hours
    }
    initial_fallback_requirement_applies = _plan_requires_initial_fallback(plan)
    missing_initial_fallback_variables = (
        sorted(set(product.required_initial_fallback_variables) - set(initial_substitutions))
        if initial_fallback_requirement_applies
        else []
    )
    manifest["initial_fallback_requirement_applies"] = (
        initial_fallback_requirement_applies
    )
    manifest["initial_frame_substitutions"] = initial_substitutions
    manifest["missing_initial_fallback_variables"] = missing_initial_fallback_variables
    manifest["interpolation_support_entries"] = sum(
        1
        for file_record in files
        for entry in file_record.get("entries") or []
        if entry.get("interpolation_support")
    )
    support_records = {
        (
            str(entry.get("coverage_source_run") or entry.get("source_run")),
            str(entry.get("valid_time_utc")),
            int(entry.get("coverage_forecast_hour", entry.get("forecast_hour", -1))),
        )
        for file_record in files
        for entry in file_record.get("entries") or []
        if entry.get("interpolation_support")
    }
    manifest["interpolation_support_records"] = [
        {
            "source_run": source_run,
            "valid_time_utc": valid_time_utc,
            "forecast_hour": forecast_hour,
            "hidden": True,
            "right_support": True,
            "support_kind": "right_lookahead",
        }
        for source_run, valid_time_utc, forecast_hour in sorted(support_records)
    ]
    if (
        missing_bundle_required_variables
        or missing_initial_fallback_variables
        or manifest["missing_pressure_levels_hpa"]
    ):
        manifest["status"] = "incomplete"
    elif files and manifest["spatial_ranges"]:
        manifest["status"] = "complete"
    atomic_write_json(output_root / "published" / product.name / "latest.json", manifest)
    _log_download_stage(
        "manifest",
        product=product,
        coverage_id=coverage_id,
        planned_entries=sum(len(file_record.get("entries") or []) for file_record in files),
        planned_ranges=sum(
            len(entry.get("byte_ranges") or [])
            for file_record in files
            for entry in file_record.get("entries") or []
        ),
        downloaded_ranges=sum(
            len(entry.get("byte_ranges") or [])
            for file_record in files
            for entry in file_record.get("entries") or []
        ),
        written_bytes=int(manifest.get("bytes") or 0),
        range_workers=range_workers,
        planning_workers=planning_workers,
    )
    _append_product_run_summary(
        output_root,
        product=product,
        manifest=manifest,
        started_at_utc=run_started_at_utc,
        started_monotonic=run_started_monotonic,
    )
    return manifest


def _write_group_manifest(
    output_root: Path,
    group_name: str,
    product_manifests: dict[str, dict[str, Any]],
    static_assets: dict[str, dict[str, object]] | None = None,
) -> dict[str, Any]:
    latest_runs = {manifest.get("latest_complete_run") for manifest in product_manifests.values()}
    runs_are_available = all(isinstance(run, str) and run for run in latest_runs)
    runs_are_coherent = (
        len(latest_runs) == 1 if group_name in GROUPS_REQUIRING_MATCHING_RUNS else True
    )
    complete = (
        bool(product_manifests)
        and all(manifest.get("status") == "complete" for manifest in product_manifests.values())
        and runs_are_available
        and runs_are_coherent
    )
    group_latest_run = max(latest_runs) if runs_are_available and runs_are_coherent else None
    total_files = sum(len(manifest.get("files", [])) for manifest in product_manifests.values())
    total_bytes = sum(int(manifest.get("bytes") or 0) for manifest in product_manifests.values())
    total_downloaded_bytes = sum(
        int(manifest.get("downloaded_bytes") or 0) for manifest in product_manifests.values()
    )
    static_asset_bytes = sum(
        int(record.get("bytes") or 0) for record in (static_assets or {}).values()
    )
    payload = {
        "group": group_name,
        "status": "complete" if complete else "incomplete",
        "reason": None if complete else "required product manifests are incomplete or incoherent",
        "generated_at": int(datetime.now().timestamp()),
        "latest_complete_run": group_latest_run,
        "products": list(product_manifests),
        "files": total_files,
        "bytes": total_bytes,
        "downloaded_bytes": total_downloaded_bytes,
        "static_assets": static_assets or {},
        "static_asset_files": len(static_assets or {}),
        "static_asset_bytes": static_asset_bytes,
        "product_manifests": {
            name: {
                "coverage_id": manifest.get("coverage_id"),
                "status": manifest.get("status"),
                "latest_complete_run": manifest.get("latest_complete_run"),
                "required_start_utc": manifest.get("required_start_utc"),
                "public_start_utc": manifest.get("public_start_utc"),
                "required_end_utc": manifest.get("required_end_utc"),
                "valid_time_count": manifest.get("valid_time_count"),
                "files": len(manifest.get("files", [])),
                "bytes": manifest.get("bytes"),
                "downloaded_bytes": manifest.get("downloaded_bytes"),
                "path": f"../{name}/latest.json",
            }
            for name, manifest in product_manifests.items()
        },
    }
    group_root = output_root / "published" / "groups" / group_name
    atomic_write_json(group_root / "latest.json", payload)
    if complete:
        release_id = group_release_id(payload)
        archived = json.loads(json.dumps(payload))
        archived["release_id"] = release_id
        atomic_write_json(group_root / "releases" / f"{release_id}.json", archived)
    return payload


def _reported_group_status(
    *, already_complete: bool, group_manifest: dict[str, Any]
) -> tuple[str, str | None]:
    status = str(group_manifest["status"])
    if already_complete and status == "complete":
        return "skipped", "group already complete"
    return status, None


def _download_openmeteo_group_release(
    args: argparse.Namespace,
    parser: argparse.ArgumentParser,
    *,
    plan_by_product_override: dict[str, tuple[Any, list[Any], Any]] | None = None,
    preserve_published: bool = False,
) -> int:
    if not args.config:
        parser.error("--config is required with --download-openmeteo-group")
    if not args.now:
        parser.error("--now is required with --download-openmeteo-group")
    if args.download_openmeteo_group not in OPENMETEO_GROUP_PRODUCTS:
        parser.error(f"unknown Open-Meteo group: {args.download_openmeteo_group}")

    group_name = args.download_openmeteo_group
    config = load_models(Path(args.config))
    product_names = OPENMETEO_GROUP_PRODUCTS[group_name]
    product_reference_times = _frozen_group_product_reference_times(
        args,
        parser,
        group_name=group_name,
        product_names=product_names,
    )
    missing_products = [name for name in product_names if name not in config.products]
    if missing_products:
        parser.error(f"group {group_name} missing products in config: {', '.join(missing_products)}")

    now_utc = _parse_utc(args.now)
    output_root = Path(args.output)
    publish_root = Path(args.publish_openmeteo_group_to) if args.publish_openmeteo_group_to else None
    self_publish = (
        publish_root is not None
        and publish_root.resolve(strict=False)
        == (output_root / "published").resolve(strict=False)
    )
    group_started_at_utc = _utc_now_text()
    group_started_monotonic = time.monotonic()
    runs_by_product: dict[str, str] | None = None
    static_assets: dict[str, dict[str, object]] = {}
    static_asset_results: list[dict[str, object]] = []
    try:
        # 1Panel serializes each row, and the two production rows query
        # agent.db before entering this command.  Group downloads therefore do
        # not create filesystem locks that can survive service stop/start.
        with nullcontext():
            products = [config.products[name] for name in product_names]
            static_assets, static_asset_results = _prepare_group_static_assets(
                group_name,
                output_root=output_root,
                bucket_url=args.openmeteo_bucket_url,
            )
            plan_by_product = plan_by_product_override or {
                product.name: _build_product_plan(
                    product,
                    now_utc=now_utc,
                    bucket_url=args.openmeteo_bucket_url,
                    reference_time_utc=(
                        product_reference_times.get(product.name)
                        or (
                            _parse_utc(args.reference_time)
                            if args.reference_time
                            else None
                        )
                    ),
                )
                for product in products
            }
            runs_by_product = {
                name: plan.latest_complete_run
                for name, (_, _, plan) in plan_by_product.items()
            }
            existing_group_manifest = _read_json_if_exists(
                output_root / "published" / "groups" / group_name / "latest.json"
            )
            if (
                publish_root is not None
                and _published_group_identity_matches_plan(
                    existing_group_manifest,
                    group_name=group_name,
                    products=products,
                    plan_by_product=plan_by_product,
                    output_root=output_root,
                )
                and _api_group_ready_matches(
                    existing_group_manifest,
                    api_root=publish_root,
                    group_name=group_name,
                )
                and _group_static_assets_match(existing_group_manifest, static_assets)
            ):
                publish_result = sync_group_from_mirror(
                    group_name,
                    output_root / "published",
                    publish_root,
                    retain_complete_releases=args.retain_complete_releases,
                )
                cleared_payloads = []
                if not self_publish:
                    cleared_payloads = _clear_group_download_payloads(
                        output_root,
                        product_names=product_names,
                    )
                _append_group_run_summary(
                    output_root,
                    group_name=group_name,
                    status="skipped",
                    reason="api group already current",
                    group_manifest=existing_group_manifest,
                    product_runs=runs_by_product,
                    cleared_paths=cleared_payloads,
                    started_at_utc=group_started_at_utc,
                    started_monotonic=group_started_monotonic,
                )
                print(
                    json.dumps(
                        {
                            "group": group_name,
                            "status": "skipped",
                            "reason": "api group already current",
                            "latest_complete_run": existing_group_manifest.get("latest_complete_run")
                            if existing_group_manifest
                            else None,
                            "published_to": str(publish_root),
                            "publish_result": publish_result,
                            "cleared_download_payload_paths": cleared_payloads,
                            "static_assets": static_asset_results,
                        },
                        ensure_ascii=False,
                    )
                )
                return 0

            product_manifests: dict[str, dict[str, Any]] = {}
            already_complete = True
            changed_product_names: list[str] = []
            for product in products:
                _, _, plan = plan_by_product[product.name]
                manifest = _read_json_if_exists(
                    output_root / "published" / product.name / "latest.json"
                )
                manifest_has_payload = _manifest_matches_plan(manifest, plan, product, output_root)
                api_current_has_payload = (
                    publish_root is not None
                    and _manifest_identity_matches_plan(manifest, plan, product)
                    and _api_product_current_matches_manifest(
                        manifest,
                        api_root=publish_root,
                        product_name=product.name,
                    )
                )
                if manifest_has_payload or api_current_has_payload:
                    product_manifests[product.name] = manifest
                    continue
                already_complete = False
                changed_product_names.append(product.name)
            cleared_paths: list[str] = []
            if not already_complete:
                if not self_publish and not preserve_published:
                    cleared_paths = _clear_group_published_data(
                        output_root,
                        group_name=group_name,
                        product_names=changed_product_names,
                    )
                for product in products:
                    if product.name in product_manifests:
                        continue
                    product_started_at_utc = _utc_now_text()
                    product_started_monotonic = time.monotonic()
                    _, _, product_plan = plan_by_product[product.name]
                    try:
                        product_manifests[product.name] = _download_openmeteo_product(
                            product,
                            now_utc=now_utc,
                            output_root=output_root,
                            bucket_url=args.openmeteo_bucket_url,
                            lut_codec=args.lut_codec,
                            download_workers=args.download_workers,
                            planning_workers=args.planning_workers,
                            range_workers=args.range_workers,
                            range_io_merge_gap=args.range_io_merge_gap,
                            range_io_size_max=args.range_io_size_max,
                            object_fetch_mode=args.object_fetch_mode,
                            object_fetch_max_multiplier=args.object_fetch_max_multiplier,
                            object_fetch_min_ranges=args.object_fetch_min_ranges,
                            object_range_merge_gap=args.object_range_merge_gap,
                            object_range_max_multiplier=args.object_range_max_multiplier,
                            object_range_min_ranges=args.object_range_min_ranges,
                            object_range_max_bytes=args.object_range_max_bytes,
                            plan_data=plan_by_product[product.name],
                        )
                    except Exception as exc:
                        _append_product_failure_summary(
                            output_root,
                            product=product,
                            coverage_id=_coverage_id_for_plan(product, product_plan),
                            latest_complete_run=product_plan.latest_complete_run,
                            started_at_utc=product_started_at_utc,
                            started_monotonic=product_started_monotonic,
                            exc=exc,
                        )
                        raise

            group_manifest = _write_group_manifest(
                output_root,
                group_name,
                product_manifests,
                static_assets,
            )
            group_status, group_reason = _reported_group_status(
                already_complete=already_complete,
                group_manifest=group_manifest,
            )
            publish_result = None
            cleared_download_payloads: list[str] = []
            if publish_root is not None and group_manifest.get("status") == "complete":
                publish_result = sync_group_from_mirror(
                    group_name,
                    output_root / "published",
                    publish_root,
                    retain_complete_releases=args.retain_complete_releases,
                )
                if not _api_group_ready_matches(
                    group_manifest,
                    api_root=publish_root,
                    group_name=group_name,
                ):
                    raise ValueError(f"published API group is not ready after sync: {group_name}")
                if not self_publish:
                    cleared_download_payloads = _clear_group_download_payloads(
                        output_root,
                        product_names=product_names,
                    )
                    cleared_paths.extend(cleared_download_payloads)
            _append_group_run_summary(
                output_root,
                group_name=group_name,
                status=group_status,
                reason=group_reason,
                group_manifest=group_manifest,
                product_runs=runs_by_product,
                cleared_paths=cleared_paths,
                started_at_utc=group_started_at_utc,
                started_monotonic=group_started_monotonic,
            )
            print(
                json.dumps(
                    {
                        "group": group_name,
                        "status": group_status,
                        "reason": group_reason,
                        "latest_complete_run": group_manifest["latest_complete_run"],
                        "products": product_names,
                        "cleared_published_paths": cleared_paths,
                        "publish_result": publish_result,
                        "cleared_download_payload_paths": cleared_download_payloads,
                        "static_assets": static_asset_results,
                    },
                    ensure_ascii=False,
                )
            )
            return 0
    except Exception as exc:
        _append_group_run_summary(
            output_root,
            group_name=group_name,
            status="failed",
            reason="group download failed",
            product_runs=runs_by_product,
            started_at_utc=group_started_at_utc,
            started_monotonic=group_started_monotonic,
            exc=exc,
        )
        print(
            json.dumps(
                {
                    "group": group_name,
                    "status": "failed",
                    "reason": "group download failed",
                    "error_type": type(exc).__name__,
                    "error_message": str(exc),
                    "remote_error": _error_is_remote_download_error(exc),
                },
                ensure_ascii=False,
            ),
            file=sys.stderr,
        )
        raise


def _group_release_payload_is_available(
    api_root: Path,
    group_name: str,
    manifest: dict[str, Any],
) -> bool:
    if manifest.get("group") != group_name or manifest.get("status") != "complete":
        return False
    group_run = str(manifest.get("latest_complete_run") or "")
    summaries = manifest.get("product_manifests")
    if not group_run or not isinstance(summaries, dict):
        return False
    for product_name in OPENMETEO_GROUP_PRODUCTS[group_name]:
        summary = summaries.get(product_name)
        if not isinstance(summary, dict):
            return False
        product_run = str(summary.get("latest_complete_run") or "")
        if not product_run:
            return False
        if group_name in GROUPS_REQUIRING_MATCHING_RUNS and product_run != group_run:
            return False
        coverage_id = str(summary.get("coverage_id") or "")
        coverage_root = api_root / product_name / "coverages" / coverage_id
        product_manifest = _read_json_if_exists(coverage_root / "latest.json")
        if not product_manifest or product_manifest.get("status") != "complete":
            return False
        if product_manifest.get("coverage_id") != coverage_id:
            return False
        if product_manifest.get("latest_complete_run") != product_run:
            return False
        files = product_manifest.get("files")
        if not isinstance(files, list) or not files:
            return False
        for file_record in files:
            if not isinstance(file_record, dict):
                return False
            relative_path = PurePosixPath(str(file_record.get("path") or ""))
            if relative_path.is_absolute() or ".." in relative_path.parts:
                return False
            if len(relative_path.parts) < 2 or relative_path.parts[:2] != ("coverages", coverage_id):
                return False
            payload_path = api_root / product_name / Path(*relative_path.parts)
            if not payload_path.is_file():
                return False
            if payload_path.stat().st_size != int(file_record.get("bytes") or -1):
                return False
    return True


def _available_group_release_candidates(
    api_root: Path,
    group_name: str,
) -> dict[str, list[dict[str, Any]]]:
    manifests: list[dict[str, Any]] = []
    current = _read_json_if_exists(
        api_root / "groups" / group_name / "current" / "ready_for_processing.json"
    )
    if current:
        manifests.append(current)
    release_root = api_root / "groups" / group_name / "releases"
    if release_root.exists():
        for release_path in sorted(release_root.glob("*.json")):
            release = _read_json_if_exists(release_path)
            if release:
                manifests.append(release)
    available: dict[str, list[dict[str, Any]]] = {}
    for manifest in manifests:
        run = str(manifest.get("latest_complete_run") or "")
        if run and _group_release_payload_is_available(api_root, group_name, manifest):
            available.setdefault(run, []).append(manifest)
    return available


def _available_group_releases(
    api_root: Path,
    group_name: str,
) -> dict[str, dict[str, Any]]:
    return {
        run: candidates[-1]
        for run, candidates in _available_group_release_candidates(api_root, group_name).items()
    }


def _group_release_matches_plans(
    manifest: dict[str, Any],
    products: list[ProductConfig],
    plan_by_product: dict[str, tuple[Any, list[Any], Any]],
) -> bool:
    summaries = manifest.get("product_manifests")
    if not isinstance(summaries, dict):
        return False
    for product in products:
        summary = summaries.get(product.name)
        if not isinstance(summary, dict):
            return False
        _catalog, _runs, plan = plan_by_product[product.name]
        if summary.get("coverage_id") != _coverage_id_for_plan(product, plan):
            return False
        if summary.get("latest_complete_run") != plan.latest_complete_run:
            return False
        if summary.get("required_start_utc") != _format_utc(plan.required_start_utc):
            return False
        if summary.get("required_end_utc") != _format_utc(plan.required_end_utc):
            return False
        if summary.get("valid_time_count") != len(plan.slots):
            return False
    return True


def _matching_group_releases(
    api_root: Path,
    group_name: str,
    products: list[ProductConfig],
    plans_by_run: dict[str, dict[str, tuple[Any, list[Any], Any]]],
) -> dict[str, dict[str, Any]]:
    matches: dict[str, dict[str, Any]] = {}
    candidates_by_run = _available_group_release_candidates(api_root, group_name)
    for run, plans in plans_by_run.items():
        for candidate in reversed(candidates_by_run.get(run, [])):
            if _group_release_matches_plans(candidate, products, plans):
                matches[run] = candidate
                break
    return matches


def _complete_run_plan_data(
    product: ProductConfig,
    catalog: OpenMeteoSpatialCatalog,
) -> tuple[OpenMeteoSpatialCatalog, list[Any], Any]:
    if not catalog.completed:
        raise ValueError(f"Open-Meteo run is not complete: {product.name}")
    run = om_run_from_spatial_catalog(product.name, catalog)
    plan = build_complete_run_coverage_plan(product, run)
    return catalog, [run], plan


def _discover_recent_complete_group_plans(
    products: list[ProductConfig],
    *,
    bucket_url: str,
    count: int,
    latest_reference_time_utc: datetime | None = None,
) -> list[tuple[str, dict[str, tuple[Any, list[Any], Any]]]]:
    if count < 1:
        raise ValueError("complete group run count must be positive")
    if latest_reference_time_utc is None:
        latest_catalogs = {
            product.name: load_openmeteo_spatial_latest(
                product.openmeteo_model,
                bucket_url=bucket_url,
            )
            for product in products
        }
    else:
        frozen_reference = _as_utc(latest_reference_time_utc)
        latest_catalogs = {
            product.name: load_openmeteo_spatial_run(
                product.openmeteo_model,
                frozen_reference,
                bucket_url=bucket_url,
            )
            for product in products
        }
    cadence_step = products[0].run_cadence_hours
    for product in products[1:]:
        cadence_step = gcd(cadence_step, product.run_cadence_hours)
    if cadence_step <= 0:
        raise ValueError("group run cadence must be positive")

    candidate = min(catalog.reference_time_utc for catalog in latest_catalogs.values())
    discovered: list[tuple[str, dict[str, tuple[Any, list[Any], Any]]]] = []
    max_probes = max(24, count * 8)
    for _probe in range(max_probes):
        catalogs: dict[str, OpenMeteoSpatialCatalog] = {}
        missing_candidate = False
        for product in products:
            latest = latest_catalogs[product.name]
            age_seconds = (latest.reference_time_utc - candidate).total_seconds()
            if (
                age_seconds < 0
                or age_seconds % 3600 != 0
                or int(age_seconds // 3600) % product.run_cadence_hours != 0
            ):
                missing_candidate = True
                break
            try:
                catalog = (
                    latest
                    if latest.reference_time_utc == candidate
                    else load_openmeteo_spatial_run(
                        product.openmeteo_model,
                        candidate,
                        bucket_url=bucket_url,
                    )
                )
            except HTTPError as exc:
                if exc.code != 404:
                    raise
                missing_candidate = True
                break
            if (
                not catalog.completed
                or catalog.reference_time_utc != candidate
                or catalog.max_forecast_hour < product.forecast_hour_end
            ):
                missing_candidate = True
                break
            catalogs[product.name] = catalog
        if not missing_candidate and len(catalogs) == len(products):
            run_id = candidate.strftime("%Y%m%d%H")
            try:
                plans = {
                    product.name: _complete_run_plan_data(product, catalogs[product.name])
                    for product in products
                }
            except ValueError:
                plans = None
            if plans is not None:
                discovered.append((run_id, plans))
            if len(discovered) == count:
                return discovered
        candidate -= timedelta(hours=cadence_step)
    raise ValueError(f"could not discover {count} recent complete coherent group runs")


def _discover_recent_gfs_retention_plans(
    products: list[ProductConfig],
    *,
    bucket_url: str,
    latest_reference_time_utc: datetime | None = None,
) -> list[tuple[str, dict[str, tuple[Any, list[Any], Any]]]]:
    discovered = _discover_recent_complete_group_plans(
        products,
        bucket_url=bucket_url,
        count=GFS_TOTAL_RELEASE_RETENTION,
        latest_reference_time_utc=latest_reference_time_utc,
    )
    product_by_name = {product.name: product for product in products}
    ranked: list[tuple[str, dict[str, tuple[Any, list[Any], Any]]]] = []
    for rank, (run_id, plans) in enumerate(discovered):
        if rank < GFS_COMPLETE_RUN_RETENTION:
            ranked.append((run_id, plans))
            continue
        partial_plans: dict[str, tuple[Any, list[Any], Any]] = {}
        for product_name, (catalog, runs, _full_plan) in plans.items():
            if len(runs) != 1:
                raise ValueError(f"expected one source run for {product_name}/{run_id}")
            partial_plans[product_name] = (
                catalog,
                runs,
                build_run_native_forecast_hour_coverage_plan(
                    product_by_name[product_name],
                    runs[0],
                    forecast_hour_end=GFS_PARTIAL_FORECAST_HOUR_END,
                ),
            )
        ranked.append((run_id, partial_plans))
    return ranked


def _parse_exact_gfs_run_id(run_id: str) -> datetime:
    if len(run_id) != 10 or not run_id.isdigit():
        raise ValueError("GFS run must use YYYYMMDDHH format")
    try:
        reference_time = datetime.strptime(run_id, "%Y%m%d%H").replace(tzinfo=timezone.utc)
    except ValueError as exc:
        raise ValueError(f"invalid GFS run: {run_id}") from exc
    if reference_time.strftime("%Y%m%d%H") != run_id or reference_time.hour % 6 != 0:
        raise ValueError(f"GFS run must be one of the 00/06/12/18 UTC cycles: {run_id}")
    return reference_time


def _build_exact_gfs_short_run_plans(
    products: list[ProductConfig],
    *,
    run_id: str,
    bucket_url: str,
) -> dict[str, tuple[Any, list[Any], Any]]:
    reference_time = _parse_exact_gfs_run_id(run_id)
    catalogs_by_model: dict[str, OpenMeteoSpatialCatalog] = {}
    plans: dict[str, tuple[Any, list[Any], Any]] = {}
    for product in products:
        if product.forecast_hour_end < GFS_PARTIAL_FORECAST_HOUR_END:
            raise ValueError(
                f"GFS product cannot retain f000..f{GFS_PARTIAL_FORECAST_HOUR_END:03d}: "
                f"{product.name} ends at f{product.forecast_hour_end:03d}"
            )
        catalog = catalogs_by_model.get(product.openmeteo_model)
        if catalog is None:
            catalog = load_openmeteo_spatial_run(
                product.openmeteo_model,
                reference_time,
                bucket_url=bucket_url,
            )
            catalogs_by_model[product.openmeteo_model] = catalog
        if not catalog.completed:
            raise ValueError(f"Open-Meteo GFS run is not complete: {product.name}/{run_id}")
        if catalog.reference_time_utc != reference_time:
            raise ValueError(
                f"Open-Meteo GFS run mismatch for {product.name}: "
                f"expected {run_id}, got {catalog.reference_time_utc:%Y%m%d%H}"
            )
        run = om_run_from_spatial_catalog(product.name, catalog)
        plan = build_run_native_forecast_hour_coverage_plan(
            product,
            run,
            forecast_hour_end=GFS_PARTIAL_FORECAST_HOUR_END,
        )
        expected_hours = tuple(
            sorted(
                int((valid_time - reference_time).total_seconds() // 3600)
                for valid_time in catalog.valid_times_utc
                if getattr(product, "forecast_hour_start", 0)
                <= int((valid_time - reference_time).total_seconds() // 3600)
                <= GFS_PARTIAL_FORECAST_HOUR_END
            )
        )
        actual_hours = tuple(slot.forecast_hour for slot in plan.slots)
        source_runs = {slot.source_run for slot in plan.slots}
        if (
            plan.latest_complete_run != run_id
            or actual_hours != expected_hours
            or source_runs != {run_id}
        ):
            raise ValueError(
                f"exact GFS short-run plan is invalid for {product.name}/{run_id}: "
                f"hours={actual_hours}, source_runs={sorted(source_runs)}"
            )
        plans[product.name] = (catalog, [run], plan)
    return plans


def _current_group_marker_state(
    output_root: Path,
    group: str,
) -> dict[str, bytes | None]:
    relative_paths = [
        Path("groups") / group / "current" / "latest.json",
        Path("groups") / group / "current" / "ready_for_processing.json",
    ]
    relative_paths.extend(
        Path(product) / "current" / marker
        for product in OPENMETEO_GROUP_PRODUCTS[group]
        for marker in ("latest.json", "ready_for_processing.json")
    )
    state: dict[str, bytes | None] = {}
    for relative_path in relative_paths:
        path = output_root / relative_path
        if path.exists() and not path.is_file():
            raise ValueError(f"current marker is not a regular file: {path}")
        state[relative_path.as_posix()] = path.read_bytes() if path.exists() else None
    return state


def _paths_overlap(left: Path, right: Path) -> bool:
    resolved_left = left.resolve(strict=False)
    resolved_right = right.resolve(strict=False)
    for child, parent in (
        (resolved_left, resolved_right),
        (resolved_right, resolved_left),
    ):
        try:
            child.relative_to(parent)
        except ValueError:
            continue
        return True
    return False


def _discover_recent_complete_product_plans(
    product: ProductConfig,
    *,
    bucket_url: str,
    count: int,
) -> list[tuple[str, tuple[Any, list[Any], Any]]]:
    if count < 1:
        raise ValueError("complete product run count must be positive")
    if product.run_cadence_hours <= 0:
        raise ValueError(f"product run cadence must be positive: {product.name}")

    latest = load_openmeteo_spatial_latest(
        product.openmeteo_model,
        bucket_url=bucket_url,
    )
    candidate = latest.reference_time_utc
    discovered: list[tuple[str, tuple[Any, list[Any], Any]]] = []
    max_probes = max(24, count * 8)
    for _probe in range(max_probes):
        try:
            catalog = (
                latest
                if candidate == latest.reference_time_utc
                else load_openmeteo_spatial_run(
                    product.openmeteo_model,
                    candidate,
                    bucket_url=bucket_url,
                )
            )
        except HTTPError as exc:
            if exc.code != 404:
                raise
            catalog = None
        if (
            catalog is not None
            and catalog.completed
            and catalog.reference_time_utc == candidate
            and catalog.max_forecast_hour >= product.forecast_hour_end
        ):
            try:
                plan_data = _complete_run_plan_data(product, catalog)
            except ValueError:
                plan_data = None
            if plan_data is not None:
                run_id = candidate.strftime("%Y%m%d%H")
                discovered.append((run_id, plan_data))
                if len(discovered) == count:
                    return discovered
        candidate -= timedelta(hours=product.run_cadence_hours)
    raise ValueError(
        f"could not discover {count} recent complete runs for CAMS product {product.name}"
    )


def _discover_recent_complete_cams_ranked_plans(
    products: list[ProductConfig],
    *,
    bucket_url: str,
    count: int,
) -> list[tuple[str, dict[str, tuple[Any, list[Any], Any]]]]:
    product_plans = {
        product.name: _discover_recent_complete_product_plans(
            product,
            bucket_url=bucket_url,
            count=count,
        )
        for product in products
    }
    ranked: list[tuple[str, dict[str, tuple[Any, list[Any], Any]]]] = []
    for rank in range(count):
        plans = {
            product.name: product_plans[product.name][rank][1]
            for product in products
        }
        group_run = max(product_plans[product.name][rank][0] for product in products)
        ranked.append((group_run, plans))
    return ranked


def _discover_recent_ecmwf_product_retention_plans(
    product: ProductConfig,
    *,
    now_utc: datetime,
    bucket_url: str,
    count: int = ECMWF_TOTAL_RELEASE_RETENTION,
) -> list[tuple[str, tuple[Any, list[Any], Any]]]:
    """Discover the latest two 00Z/12Z long runs and their three predecessors."""
    if count != ECMWF_TOTAL_RELEASE_RETENTION:
        raise ValueError(
            f"ECMWF retention count must be {ECMWF_TOTAL_RELEASE_RETENTION}"
        )
    latest = load_openmeteo_spatial_latest(
        product.openmeteo_model,
        bucket_url=bucket_url,
    )
    candidate = latest.reference_time_utc
    discovered: list[tuple[str, tuple[Any, list[Any], Any]]] = []
    complete_runs: list[datetime] = []
    required_partial_runs: set[datetime] = set()
    max_probes = max(24, count * 8)
    for _probe in range(max_probes):
        try:
            catalog = (
                latest
                if candidate == latest.reference_time_utc
                else load_openmeteo_spatial_run(
                    product.openmeteo_model,
                    candidate,
                    bucket_url=bucket_url,
                )
            )
        except HTTPError as exc:
            if exc.code != 404:
                raise
            catalog = None
        if (
            catalog is not None
            and catalog.completed
            and catalog.reference_time_utc == candidate
        ):
            if (
                len(complete_runs) < ECMWF_COMPLETE_RUN_RETENTION
                and candidate.hour in (0, 12)
                and catalog.max_forecast_hour >= product.forecast_hour_end
            ):
                plan = _build_product_plan(
                    product,
                    now_utc=now_utc,
                    bucket_url=bucket_url,
                    reference_time_utc=candidate,
                )
                complete_runs.append(candidate)
                run_id = candidate.strftime("%Y%m%d%H")
                discovered.append((run_id, plan))
                if len(complete_runs) == ECMWF_COMPLETE_RUN_RETENTION:
                    previous_complete = complete_runs[-1]
                    required_partial_runs = {
                        previous_complete - timedelta(hours=offset)
                        for offset in (6, 12, 18)
                    }
            elif candidate in required_partial_runs:
                run = om_run_from_spatial_catalog(product.name, catalog)
                plan = (
                    catalog,
                    [run],
                    build_run_native_forecast_hour_coverage_plan(
                        product,
                        run,
                        forecast_hour_end=ECMWF_PARTIAL_FORECAST_HOUR_END,
                    ),
                )
                run_id = candidate.strftime("%Y%m%d%H")
                discovered.append((run_id, plan))
            if len(discovered) == count:
                return discovered
        candidate -= timedelta(hours=product.run_cadence_hours)
    raise ValueError(
        f"could not discover {count} recent complete ECMWF releases for {product.name}"
    )


def _discover_recent_ecmwf_retention_plans(
    products: list[ProductConfig],
    *,
    now_utc: datetime,
    bucket_url: str,
    count: int = ECMWF_TOTAL_RELEASE_RETENTION,
) -> list[tuple[str, dict[str, tuple[Any, list[Any], Any]]]]:
    """Rank independently published deterministic and ensemble ECMWF cycles."""
    if not products:
        raise ValueError("ECMWF retention requires at least one product")
    product_plans = {
        product.name: _discover_recent_ecmwf_product_retention_plans(
            product,
            now_utc=now_utc,
            bucket_url=bucket_url,
            count=count,
        )
        for product in products
    }
    ranked: list[tuple[str, dict[str, tuple[Any, list[Any], Any]]]] = []
    for rank in range(count):
        plans = {
            product.name: product_plans[product.name][rank][1]
            for product in products
        }
        group_run = max(product_plans[product.name][rank][0] for product in products)
        ranked.append((group_run, plans))
    return ranked


def _reconcile_ecmwf_retention_window(
    args: argparse.Namespace,
    parser: argparse.ArgumentParser,
) -> int:
    if not args.config:
        parser.error("--config is required with --download-openmeteo-group")
    if args.retain_complete_releases != ECMWF_TOTAL_RELEASE_RETENTION:
        parser.error(
            "ECMWF requires --retain-complete-releases "
            f"{ECMWF_TOTAL_RELEASE_RETENTION}"
        )

    config = load_models(Path(args.config))
    products = [
        config.products[name] for name in OPENMETEO_GROUP_PRODUCTS["ecmwf"]
    ]
    now_utc = _parse_utc(args.now)
    target_plans = _discover_recent_ecmwf_retention_plans(
        products,
        now_utc=now_utc,
        bucket_url=args.openmeteo_bucket_url,
    )
    source_root = Path(args.output) / "published"
    api_root = (
        Path(args.publish_openmeteo_group_to)
        if args.publish_openmeteo_group_to
        else source_root
    )
    target_runs = [run_id for run_id, _plans in target_plans]
    plans_by_run = dict(target_plans)
    pre_pruned_source_paths = prune_expired_group_releases(
        source_root,
        "ecmwf",
        retain_complete_releases=ECMWF_TOTAL_RELEASE_RETENTION,
        preserve_current=True,
        retain_runs=target_runs,
    )
    pre_pruned_raw_paths: list[str] = []
    if source_root.resolve(strict=False) != api_root.resolve(strict=False):
        pre_pruned_raw_paths = prune_expired_group_releases(
            api_root,
            "ecmwf",
            retain_complete_releases=ECMWF_TOTAL_RELEASE_RETENTION,
            preserve_current=True,
            retain_runs=target_runs,
        )

    available = _matching_group_releases(
        api_root,
        "ecmwf",
        products,
        plans_by_run,
    )
    missing_runs = [run_id for run_id in target_runs if run_id not in available]
    download_results: list[dict[str, Any]] = []
    retain_results: list[dict[str, Any]] = []
    download_args = copy(args)
    download_args.publish_openmeteo_group_to = None

    for run_id in reversed(target_runs):
        if run_id not in missing_runs:
            continue
        stdout = StringIO()
        with redirect_stdout(stdout):
            result = _download_openmeteo_group_release(
                download_args,
                parser,
                plan_by_product_override=plans_by_run[run_id],
                preserve_published=True,
            )
        if result != 0:
            return result
        lines = [line for line in stdout.getvalue().splitlines() if line.strip()]
        if lines:
            download_results.append(json.loads(lines[-1]))
        retain_results.append(
            retain_group_release_from_mirror("ecmwf", source_root, api_root)
        )
        if source_root.resolve(strict=False) != api_root.resolve(strict=False):
            _clear_group_download_payloads(
                Path(args.output),
                product_names=[product.name for product in products],
            )

    available = _matching_group_releases(
        api_root,
        "ecmwf",
        products,
        plans_by_run,
    )
    absent_after_download = [run_id for run_id in target_runs if run_id not in available]
    if absent_after_download:
        raise ValueError(
            "ECMWF retention window is incomplete after download: "
            + ", ".join(absent_after_download)
        )

    newest_run = target_runs[0]
    current = _read_json_if_exists(
        api_root / "groups" / "ecmwf" / "current" / "ready_for_processing.json"
    )
    activation = None
    if (
        not current
        or current.get("latest_complete_run") != newest_run
        or not _group_release_matches_plans(
            current,
            products,
            plans_by_run[newest_run],
        )
    ):
        activation = activate_group_release(
            api_root,
            "ecmwf",
            available[newest_run],
        )
    pruned = prune_expired_group_releases(
        api_root,
        "ecmwf",
        retain_complete_releases=ECMWF_TOTAL_RELEASE_RETENTION,
        retain_runs=target_runs,
    )
    print(
        json.dumps(
            {
                "group": "ecmwf",
                "status": "complete" if missing_runs else "skipped",
                "reason": (
                    None
                    if missing_runs
                    else "target retention window already complete"
                ),
                "latest_complete_run": newest_run,
                "retained_complete_runs": target_runs[:ECMWF_COMPLETE_RUN_RETENTION],
                "retained_partial_runs": target_runs[ECMWF_COMPLETE_RUN_RETENTION:],
                "partial_forecast_hour_end": ECMWF_PARTIAL_FORECAST_HOUR_END,
                "downloaded_missing_runs": list(reversed(missing_runs)),
                "download_results": download_results,
                "retain_results": retain_results,
                "pre_pruned_source_paths": pre_pruned_source_paths,
                "pre_pruned_raw_paths": pre_pruned_raw_paths,
                "activation": activation,
                "pruned_raw_paths": pruned,
            },
            ensure_ascii=False,
        )
    )
    return 0


def _reconcile_cams_complete_runs(
    args: argparse.Namespace,
    parser: argparse.ArgumentParser,
) -> int:
    if not args.config:
        parser.error("--config is required with --download-openmeteo-group")
    if args.retain_complete_releases != CAMS_COMPLETE_RUN_RETENTION:
        parser.error(
            f"CAMS requires --retain-complete-releases {CAMS_COMPLETE_RUN_RETENTION}"
        )

    config = load_models(Path(args.config))
    products = [config.products[name] for name in OPENMETEO_GROUP_PRODUCTS["cams"]]
    target_plans = _discover_recent_complete_cams_ranked_plans(
        products,
        bucket_url=args.openmeteo_bucket_url,
        count=CAMS_COMPLETE_RUN_RETENTION,
    )
    api_root = Path(args.publish_openmeteo_group_to)
    target_runs = [run_id for run_id, _plans in target_plans]
    plans_by_run = dict(target_plans)
    pre_pruned_raw_paths = prune_expired_group_releases(
        api_root,
        "cams",
        retain_complete_releases=CAMS_COMPLETE_RUN_RETENTION,
        preserve_current=True,
    )
    all_available = _available_group_releases(api_root, "cams")
    available = {
        run_id: manifest
        for run_id, manifest in all_available.items()
        if run_id in plans_by_run
        and _group_release_matches_plans(manifest, products, plans_by_run[run_id])
    }
    missing_runs = [run_id for run_id in target_runs if run_id not in available]
    download_results: list[dict[str, Any]] = []

    for run_id in reversed(target_runs):
        if run_id not in missing_runs:
            continue
        stdout = StringIO()
        with redirect_stdout(stdout):
            result = _download_openmeteo_group_release(
                args,
                parser,
                plan_by_product_override=plans_by_run[run_id],
            )
        if result != 0:
            return result
        lines = [line for line in stdout.getvalue().splitlines() if line.strip()]
        if lines:
            download_result = json.loads(lines[-1])
            download_results.append(download_result)
            if (
                download_result.get("status") == "skipped"
                and download_result.get("reason") == "group already running"
            ):
                print(json.dumps(download_result, ensure_ascii=False))
                return 0

    all_available = _available_group_releases(api_root, "cams")
    available = {
        run_id: manifest
        for run_id, manifest in all_available.items()
        if run_id in plans_by_run
        and _group_release_matches_plans(manifest, products, plans_by_run[run_id])
    }
    absent_after_download = [run_id for run_id in target_runs if run_id not in available]
    if absent_after_download:
        raise ValueError(
            "CAMS retention window is incomplete after download: "
            + ", ".join(absent_after_download)
        )

    newest_run = target_runs[0]
    current = _read_json_if_exists(
        api_root / "groups" / "cams" / "current" / "ready_for_processing.json"
    )
    activation = None
    if not current or current.get("latest_complete_run") != newest_run:
        activation = activate_group_release(api_root, "cams", available[newest_run])
    pruned = prune_expired_group_releases(
        api_root,
        "cams",
        retain_complete_releases=CAMS_COMPLETE_RUN_RETENTION,
    )
    print(
        json.dumps(
            {
                "group": "cams",
                "status": "complete" if missing_runs else "skipped",
                "reason": None if missing_runs else "three most recent complete runs already retained",
                "latest_complete_run": newest_run,
                "retained_complete_runs": target_runs,
                "downloaded_missing_runs": list(reversed(missing_runs)),
                "download_results": download_results,
                "activation": activation,
                "pre_pruned_raw_paths": pre_pruned_raw_paths,
                "pruned_raw_paths": pruned,
            },
            ensure_ascii=False,
        )
    )
    return 0


def _reconcile_gfs_retention_window(
    args: argparse.Namespace,
    parser: argparse.ArgumentParser,
) -> int:
    if not args.config:
        parser.error("--config is required with --download-openmeteo-group")

    defer_activation = bool(getattr(args, "defer_openmeteo_gfs_activation", False))
    config = load_models(Path(args.config))
    products = [config.products[name] for name in OPENMETEO_GROUP_PRODUCTS["gfs"]]
    target_plans = _discover_recent_gfs_retention_plans(
        products,
        bucket_url=args.openmeteo_bucket_url,
        latest_reference_time_utc=(
            _parse_utc(args.reference_time)
            if getattr(args, "reference_time", None)
            else None
        ),
    )
    source_root = Path(args.output) / "published"
    api_root = (
        Path(args.publish_openmeteo_group_to)
        if args.publish_openmeteo_group_to
        else source_root
    )
    if defer_activation and not args.publish_openmeteo_group_to:
        parser.error(
            "--defer-openmeteo-gfs-activation requires "
            "--publish-openmeteo-group-to for an API publisher"
        )
    target_runs = [run_id for run_id, _plans in target_plans]
    plans_by_run = dict(target_plans)
    pre_pruned_source_paths = prune_expired_group_releases(
        source_root,
        "gfs",
        retain_complete_releases=GFS_TOTAL_RELEASE_RETENTION,
        preserve_current=True,
    )
    pre_pruned_raw_paths: list[str] = []
    if source_root.resolve(strict=False) != api_root.resolve(strict=False):
        pre_pruned_raw_paths = prune_expired_group_releases(
            api_root,
            "gfs",
            retain_complete_releases=GFS_TOTAL_RELEASE_RETENTION,
            preserve_current=True,
        )

    available = _matching_group_releases(
        api_root,
        "gfs",
        products,
        plans_by_run,
    )
    missing_runs = [run_id for run_id in target_runs if run_id not in available]
    download_results: list[dict[str, Any]] = []
    retain_results: list[dict[str, Any]] = []
    download_args = copy(args)
    download_args.publish_openmeteo_group_to = None

    for run_id in reversed(target_runs):
        if run_id not in missing_runs:
            continue
        stdout = StringIO()
        with redirect_stdout(stdout):
            result = _download_openmeteo_group_release(
                download_args,
                parser,
                plan_by_product_override=plans_by_run[run_id],
                preserve_published=True,
            )
        if result != 0:
            return result
        lines = [line for line in stdout.getvalue().splitlines() if line.strip()]
        if lines:
            download_result = json.loads(lines[-1])
            download_results.append(download_result)
            if (
                download_result.get("status") == "skipped"
                and download_result.get("reason") == "group already running"
            ):
                print(json.dumps(download_result, ensure_ascii=False))
                return 0
        retain_results.append(
            retain_group_release_from_mirror("gfs", source_root, api_root)
        )
        if source_root.resolve(strict=False) != api_root.resolve(strict=False):
            _clear_group_download_payloads(
                Path(args.output),
                product_names=[product.name for product in products],
            )

    available = _matching_group_releases(
        api_root,
        "gfs",
        products,
        plans_by_run,
    )
    absent_after_download = [run_id for run_id in target_runs if run_id not in available]
    if absent_after_download:
        raise ValueError(
            "GFS retention window is incomplete after download: "
            + ", ".join(absent_after_download)
        )

    newest_run = target_runs[0]
    current = _read_json_if_exists(
        api_root / "groups" / "gfs" / "current" / "ready_for_processing.json"
    )
    activation = None
    if not defer_activation and (
        not current
        or current.get("latest_complete_run") != newest_run
        or not _group_release_matches_plans(current, products, plans_by_run[newest_run])
    ):
        activation = activate_group_release(api_root, "gfs", available[newest_run])
    pruned = prune_expired_group_releases(
        api_root,
        "gfs",
        retain_complete_releases=GFS_TOTAL_RELEASE_RETENTION,
        preserve_current=defer_activation,
    )
    print(
        json.dumps(
            {
                "group": "gfs",
                "status": "complete" if missing_runs else "skipped",
                "reason": None if missing_runs else "target retention window already complete",
                "latest_complete_run": newest_run,
                "retained_complete_runs": target_runs[:GFS_COMPLETE_RUN_RETENTION],
                "retained_partial_runs": target_runs[GFS_COMPLETE_RUN_RETENTION:],
                "partial_forecast_hour_end": GFS_PARTIAL_FORECAST_HOUR_END,
                "downloaded_missing_runs": list(reversed(missing_runs)),
                "download_results": download_results,
                "retain_results": retain_results,
                "pre_pruned_source_paths": pre_pruned_source_paths,
                "pre_pruned_raw_paths": pre_pruned_raw_paths,
                "activation": activation,
                "activation_deferred": defer_activation,
                "pruned_raw_paths": pruned,
            },
            ensure_ascii=False,
        )
    )
    return 0


def _recover_openmeteo_gfs_short_run(
    args: argparse.Namespace,
    parser: argparse.ArgumentParser,
) -> int:
    if not args.config:
        parser.error("--config is required with --recover-openmeteo-gfs-short-run")
    if not args.retain_openmeteo_group_to:
        parser.error(
            "--retain-openmeteo-group-to is required with "
            "--recover-openmeteo-gfs-short-run"
        )

    run_id = args.recover_openmeteo_gfs_short_run
    try:
        reference_time = _parse_exact_gfs_run_id(run_id)
    except ValueError as exc:
        parser.error(str(exc))
    api_root = Path(args.retain_openmeteo_group_to).resolve(strict=False)
    recovery_root = (Path(args.output) / "recovery" / "gfs" / run_id).resolve(
        strict=False
    )
    if _paths_overlap(api_root, recovery_root):
        parser.error(
            "--retain-openmeteo-group-to must not overlap the generated recovery "
            f"staging root: {recovery_root}"
        )

    config = load_models(Path(args.config))
    missing_products = [
        name for name in OPENMETEO_GROUP_PRODUCTS["gfs"] if name not in config.products
    ]
    if missing_products:
        parser.error(f"group gfs missing products in config: {', '.join(missing_products)}")
    products = [config.products[name] for name in OPENMETEO_GROUP_PRODUCTS["gfs"]]
    plans = _build_exact_gfs_short_run_plans(
        products,
        run_id=run_id,
        bucket_url=args.openmeteo_bucket_url,
    )

    download_args = copy(args)
    download_args.download_openmeteo_group = "gfs"
    download_args.now = _format_utc(reference_time)
    download_args.output = str(recovery_root)
    download_args.publish_openmeteo_group_to = None
    source_root = recovery_root / "published"
    lock_path = Path(args.output) / "locks" / f"gfs_short_run_recovery_{run_id}.lock"

    with file_lock(lock_path):
        markers_before = _current_group_marker_state(api_root, "gfs")
        existing = _matching_group_releases(
            api_root,
            "gfs",
            products,
            {run_id: plans},
        )
        if run_id in existing:
            markers_after = _current_group_marker_state(api_root, "gfs")
            if markers_after != markers_before:
                raise ValueError("GFS current markers changed while checking retained releases")
            cleared_paths = _clear_group_download_payloads(
                recovery_root,
                product_names=[product.name for product in products],
            )
            print(
                json.dumps(
                    {
                        "group": "gfs",
                        "status": "skipped",
                        "reason": "exact short-run release already retained",
                        "latest_complete_run": run_id,
                        "forecast_hour_start": 0,
                        "forecast_hour_end": GFS_PARTIAL_FORECAST_HOUR_END,
                        "retained_to": str(api_root),
                        "current_markers_unchanged": True,
                        "activated": False,
                        "cleared_recovery_payload_paths": cleared_paths,
                    },
                    ensure_ascii=False,
                )
            )
            return 0

        stdout = StringIO()
        with redirect_stdout(stdout):
            result = _download_openmeteo_group_release(
                download_args,
                parser,
                plan_by_product_override=plans,
                preserve_published=True,
            )
        if result != 0:
            return result
        output_lines = [line for line in stdout.getvalue().splitlines() if line.strip()]
        download_result = json.loads(output_lines[-1]) if output_lines else None

        source_group_manifest = _read_json_if_exists(
            source_root / "groups" / "gfs" / "latest.json"
        )
        source_products_are_valid = all(
            _manifest_matches_plan(
                _read_json_if_exists(source_root / product.name / "latest.json"),
                plans[product.name][2],
                product,
                recovery_root,
            )
            for product in products
        )
        if (
            not source_group_manifest
            or source_group_manifest.get("status") != "complete"
            or not _group_release_matches_plans(
                source_group_manifest,
                products,
                plans,
            )
            or not source_products_are_valid
        ):
            raise ValueError(f"recovered GFS source release failed validation: {run_id}")
        retain_result = retain_group_release_from_mirror("gfs", source_root, api_root)
        retained = _matching_group_releases(
            api_root,
            "gfs",
            products,
            {run_id: plans},
        )
        if run_id not in retained:
            raise ValueError(f"retained GFS release failed validation: {run_id}")
        markers_after = _current_group_marker_state(api_root, "gfs")
        if markers_after != markers_before:
            raise ValueError(
                "GFS current markers changed during retained-release recovery; "
                "the recovery command never activates releases"
            )
        cleared_paths = _clear_group_download_payloads(
            recovery_root,
            product_names=[product.name for product in products],
        )

    print(
        json.dumps(
            {
                "group": "gfs",
                "status": "complete",
                "latest_complete_run": run_id,
                "forecast_hour_start": 0,
                "forecast_hour_end": GFS_PARTIAL_FORECAST_HOUR_END,
                "download_result": download_result,
                "retain_result": retain_result,
                "retained_to": str(api_root),
                "current_markers_unchanged": True,
                "activated": False,
                "cleared_recovery_payload_paths": cleared_paths,
            },
            ensure_ascii=False,
        )
    )
    return 0


def _download_openmeteo_group(args: argparse.Namespace, parser: argparse.ArgumentParser) -> int:
    if args.reference_time and args.download_openmeteo_group == "cams":
        parser.error(
            "--reference-time is supported by direct product/group downloads and "
            "frozen GFS retention; CAMS retention reconciliation selects its own "
            "run window"
        )
    if args.download_openmeteo_group == "gfs":
        return _reconcile_gfs_retention_window(args, parser)
    if (
        args.download_openmeteo_group == "ecmwf"
        and not args.reference_time
        and not args.product_reference_time
    ):
        effective_args = copy(args)
        effective_args.retain_complete_releases = _effective_group_retention(
            "ecmwf",
            args.retain_complete_releases,
        )
        return _reconcile_ecmwf_retention_window(effective_args, parser)
    if args.download_openmeteo_group != "cams":
        return _download_openmeteo_group_release(args, parser)
    effective_args = args
    if not args.publish_openmeteo_group_to:
        effective_args = copy(args)
        effective_args.publish_openmeteo_group_to = str(Path(args.output) / "published")
    return _reconcile_cams_complete_runs(effective_args, parser)


def _catalog_as_json(catalog: OpenMeteoSpatialCatalog, *, bucket_url: str) -> dict[str, Any]:
    first_valid = min(catalog.valid_times_utc) if catalog.valid_times_utc else None
    last_valid = max(catalog.valid_times_utc) if catalog.valid_times_utc else None
    return {
        "model": catalog.model,
        "completed": catalog.completed,
        "reference_time": _format_utc(catalog.reference_time_utc),
        "last_modified_time": _format_utc(catalog.last_modified_time_utc)
        if catalog.last_modified_time_utc
        else None,
        "valid_time_count": len(catalog.valid_times_utc),
        "first_valid_time": _format_utc(first_valid) if first_valid else None,
        "last_valid_time": _format_utc(last_valid) if last_valid else None,
        "max_forecast_hour": catalog.max_forecast_hour,
        "variable_count": len(catalog.variables),
        "variables": list(catalog.available_variables),
        "first_object_url": openmeteo_spatial_object_url(
            bucket_url,
            catalog.model,
            reference_time_utc=catalog.reference_time_utc,
            valid_time_utc=first_valid,
        )
        if first_valid
        else None,
        "last_object_url": openmeteo_spatial_object_url(
            bucket_url,
            catalog.model,
            reference_time_utc=catalog.reference_time_utc,
            valid_time_utc=last_valid,
        )
        if last_valid
        else None,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--inspect-openmeteo-model")
    parser.add_argument("--inspect-product-catalog")
    parser.add_argument("--download-openmeteo-product")
    parser.add_argument("--download-openmeteo-group")
    parser.add_argument("--recover-openmeteo-gfs-short-run")
    parser.add_argument("--retain-openmeteo-group-to")
    parser.add_argument("--build-processing-stage")
    parser.add_argument("--sync-from-manifest-url")
    parser.add_argument("--sync-from-manifest-path")
    parser.add_argument("--sync-openmeteo-group-from-source")
    parser.add_argument("--sync-openmeteo-group-releases-from-source")
    parser.add_argument("--print-openmeteo-group-release-id")
    parser.add_argument("--publish-openmeteo-group-to")
    parser.add_argument(
        "--defer-openmeteo-gfs-activation",
        action="store_true",
        help=(
            "retain the newest GFS source window without replacing API current; "
            "the production native materializer publishes current after validation"
        ),
    )
    parser.add_argument("--openmeteo-bucket-url", default=DEFAULT_OPENMETEO_BUCKET_URL)
    parser.add_argument("--inspect-om-url")
    parser.add_argument("--plan-om-ranges-url")
    parser.add_argument("--variable")
    parser.add_argument("--selection", action="append", type=_parse_selection_range, default=[])
    parser.add_argument("--lut-codec", choices=("turbopfor", "plain"), default="turbopfor")
    parser.add_argument("--download-workers", type=int, default=1)
    parser.add_argument("--planning-workers", type=int, default=None)
    parser.add_argument("--range-workers", type=int, default=None)
    parser.add_argument("--range-io-merge-gap", type=int, default=64 * 1024)
    parser.add_argument("--range-io-size-max", type=int, default=None)
    parser.add_argument("--object-fetch-mode", choices=("range", "auto", "prefix"), default="range")
    parser.add_argument("--object-fetch-max-multiplier", type=float, default=3.0)
    parser.add_argument("--object-fetch-min-ranges", type=int, default=16)
    parser.add_argument("--object-range-merge-gap", type=int, default=16 * 1024 * 1024)
    parser.add_argument("--object-range-max-multiplier", type=float, default=2.0)
    parser.add_argument("--object-range-min-ranges", type=int, default=16)
    parser.add_argument("--object-range-max-bytes", type=int, default=None)
    parser.add_argument("--config")
    parser.add_argument("--model")
    parser.add_argument("--metadata")
    parser.add_argument("--output", default="data")
    parser.add_argument("--mirror-root")
    parser.add_argument("--source-stage-root")
    parser.add_argument("--cleanup-grace-seconds", type=int, default=300)
    parser.add_argument("--retain-complete-releases", type=int, default=3)
    parser.add_argument("--raw-root")
    parser.add_argument("--now")
    parser.add_argument(
        "--reference-time",
        help=(
            "freeze Open-Meteo discovery to an exact completed run reference time "
            "(ISO-8601, for example 2026-07-18T18:00:00Z)"
        ),
    )
    parser.add_argument(
        "--product-reference-time",
        action="append",
        type=_parse_product_reference_time,
        default=[],
        metavar="PRODUCT=ISO-8601",
        help=(
            "freeze each independently published ECMWF product to an exact "
            "completed run; repeat once for every product in the group"
        ),
    )
    parser.add_argument("--source-url")
    parser.add_argument("--byte-range", action="append", type=_parse_byte_range, default=[])
    args = parser.parse_args(argv)
    if args.product_reference_time and not args.download_openmeteo_group:
        parser.error(
            "--product-reference-time requires --download-openmeteo-group ecmwf"
        )

    mutating_command = any(
        (
            args.download_openmeteo_product,
            args.download_openmeteo_group,
            args.recover_openmeteo_gfs_short_run,
            args.retain_openmeteo_group_to,
            args.build_processing_stage,
            args.sync_from_manifest_url,
            args.sync_from_manifest_path,
            args.sync_openmeteo_group_from_source,
            args.sync_openmeteo_group_releases_from_source,
        )
    )
    if mutating_command:
        guarded_paths = {Path(args.output)}
        if args.publish_openmeteo_group_to:
            guarded_paths.add(Path(args.publish_openmeteo_group_to))
        if args.raw_root:
            guarded_paths.add(Path(args.raw_root))
        for guarded_path in sorted(guarded_paths, key=str):
            enforce_environment_storage_guard(guarded_path)

    if args.inspect_openmeteo_model:
        catalog = (
            load_openmeteo_spatial_run(
                args.inspect_openmeteo_model,
                _parse_utc(args.reference_time),
                bucket_url=args.openmeteo_bucket_url,
            )
            if args.reference_time
            else load_openmeteo_spatial_latest(
                args.inspect_openmeteo_model,
                bucket_url=args.openmeteo_bucket_url,
            )
        )
        print(
            json.dumps(
                _catalog_as_json(catalog, bucket_url=args.openmeteo_bucket_url),
                ensure_ascii=False,
            )
        )
        return 0

    if args.inspect_product_catalog:
        if not args.config:
            parser.error("--config is required with --inspect-product-catalog")
        config = load_models(Path(args.config))
        if args.inspect_product_catalog not in config.products:
            parser.error(f"product not found in config: {args.inspect_product_catalog}")
        product = config.products[args.inspect_product_catalog]
        catalog = (
            load_openmeteo_spatial_run(
                product.openmeteo_model,
                _parse_utc(args.reference_time),
                bucket_url=args.openmeteo_bucket_url,
            )
            if args.reference_time
            else load_openmeteo_spatial_latest(
                product.openmeteo_model,
                bucket_url=args.openmeteo_bucket_url,
            )
        )
        available = set(catalog.variables)
        missing_required = sorted(
            (set(product.required_variables) | set(product.required_sparse_variables))
            - available
        )
        missing_optional = sorted(set(product.optional_variables) - available)
        payload = {
            "product": product.name,
            "openmeteo_model": product.openmeteo_model,
            "reference_time": _format_utc(catalog.reference_time_utc),
            "max_forecast_hour": catalog.max_forecast_hour,
            "variable_count": len(catalog.variables),
            "missing_required_variables": missing_required,
            "missing_optional_variables": missing_optional,
        }
        if args.now:
            _catalog, runs, plan = _build_product_plan_from_catalog(
                product,
                catalog,
                now_utc=_parse_utc(args.now),
                bucket_url=args.openmeteo_bucket_url,
            )
            object_records = coverage_object_records(
                plan,
                runs,
                bucket_url=args.openmeteo_bucket_url,
                openmeteo_model=product.openmeteo_model,
            )
            payload.update(
                {
                    "required_start_utc": _format_utc(plan.required_start_utc),
                    "required_end_utc": _format_utc(plan.required_end_utc),
                    "latest_complete_run": plan.latest_complete_run,
                    "valid_time_count": len(plan.slots),
                    "source_runs": sorted({slot.source_run for slot in plan.slots}),
                    "object_count": len(object_records),
                    "first_object_url": object_records[0]["url"] if object_records else None,
                    "last_object_url": object_records[-1]["url"] if object_records else None,
                }
            )
        print(
            json.dumps(
                payload,
                ensure_ascii=False,
            )
        )
        return 0

    if args.inspect_om_url:
        inventory = load_remote_om_inventory(HttpByteRangeSource(args.inspect_om_url))
        print(json.dumps(_inventory_as_json(inventory), ensure_ascii=False))
        return 0

    if args.plan_om_ranges_url:
        if not args.variable:
            parser.error("--variable is required with --plan-om-ranges-url")
        if not args.selection:
            parser.error("at least one --selection is required with --plan-om-ranges-url")
        source = HttpByteRangeSource(args.plan_om_ranges_url)
        inventory = load_remote_om_inventory(source)
        if args.variable not in inventory.arrays:
            parser.error(f"variable not found in remote OM metadata: {args.variable}")
        plan = plan_remote_array_data_byte_ranges(
            source,
            inventory.arrays[args.variable],
            selection_ranges=tuple(args.selection),
            lut_codec=args.lut_codec,
            io_size_merge=args.range_io_merge_gap,
        )
        print(
            json.dumps(
                {
                    "variable": args.variable,
                    "selection_ranges": [list(item) for item in args.selection],
                    "lut_byte_ranges": [list(item) for item in plan.lut_byte_ranges],
                    "data_byte_ranges": [list(item) for item in plan.data_byte_ranges],
                    "lut_bytes_read": plan.lut_bytes_read,
                },
                ensure_ascii=False,
            )
        )
        return 0

    if args.sync_from_manifest_url:
        result = sync_from_manifest_url(args.sync_from_manifest_url, Path(args.output))
        print(json.dumps(result, ensure_ascii=False))
        return 0

    if args.sync_from_manifest_path:
        result = sync_from_manifest_path(Path(args.sync_from_manifest_path), Path(args.output))
        print(json.dumps(result, ensure_ascii=False))
        return 0

    if args.print_openmeteo_group_release_id:
        source_root = args.source_stage_root or args.mirror_root
        if not source_root:
            parser.error(
                "--source-stage-root or --mirror-root is required with "
                "--print-openmeteo-group-release-id"
            )
        group_manifest_path = (
            Path(source_root)
            / "groups"
            / args.print_openmeteo_group_release_id
            / "latest.json"
        )
        group_manifest = json.loads(group_manifest_path.read_text(encoding="utf-8"))
        print(
            json.dumps(
                {
                    "group": args.print_openmeteo_group_release_id,
                    "release_id": group_release_id(group_manifest),
                },
                ensure_ascii=False,
            )
        )
        return 0

    if args.sync_openmeteo_group_from_source:
        source_root = args.source_stage_root or args.mirror_root
        if not source_root:
            parser.error(
                "--source-stage-root or --mirror-root is required with "
                "--sync-openmeteo-group-from-source"
            )
        group = args.sync_openmeteo_group_from_source
        result = sync_group_from_mirror(
            group,
            Path(source_root),
            Path(args.output),
            cleanup_grace_seconds=args.cleanup_grace_seconds,
            retain_complete_releases=_effective_group_retention(
                group,
                args.retain_complete_releases,
            ),
        )
        print(json.dumps(result, ensure_ascii=False))
        return 0

    if args.sync_openmeteo_group_releases_from_source:
        source_root = args.source_stage_root or args.mirror_root
        if not source_root:
            parser.error(
                "--source-stage-root or --mirror-root is required with "
                "--sync-openmeteo-group-releases-from-source"
            )
        group = args.sync_openmeteo_group_releases_from_source
        result = sync_retained_group_releases_from_mirror(
            group,
            Path(source_root),
            Path(args.output),
            retain_complete_releases=_effective_group_retention(
                group,
                args.retain_complete_releases,
            ),
            activate_current=not (
                group == "gfs" and args.defer_openmeteo_gfs_activation
            ),
        )
        print(json.dumps(result, ensure_ascii=False))
        return 0

    if args.build_processing_stage:
        if not args.raw_root:
            parser.error("--raw-root is required with --build-processing-stage")
        result = build_processing_stage(
            args.build_processing_stage,
            raw_root=Path(args.raw_root),
            output_root=Path(args.output),
        )
        print(json.dumps(result, ensure_ascii=False))
        return 0

    if args.recover_openmeteo_gfs_short_run:
        return _recover_openmeteo_gfs_short_run(args, parser)

    if args.download_openmeteo_group:
        return _download_openmeteo_group(args, parser)

    if args.download_openmeteo_product:
        if not args.config:
            parser.error("--config is required with --download-openmeteo-product")
        if not args.now:
            parser.error("--now is required with --download-openmeteo-product")
        config = load_models(Path(args.config))
        if args.download_openmeteo_product not in config.products:
            parser.error(f"product not found in config: {args.download_openmeteo_product}")
        product = config.products[args.download_openmeteo_product]
        now_utc = _parse_utc(args.now)
        output_root = Path(args.output)
        product_started_at_utc = _utc_now_text()
        product_started_monotonic = time.monotonic()
        with file_lock(output_root / "locks" / f"{product.name}.lock"):
            try:
                manifest = _download_openmeteo_product(
                    product,
                    now_utc=now_utc,
                    output_root=output_root,
                    bucket_url=args.openmeteo_bucket_url,
                    lut_codec=args.lut_codec,
                    download_workers=args.download_workers,
                    planning_workers=args.planning_workers,
                    range_workers=args.range_workers,
                    range_io_merge_gap=args.range_io_merge_gap,
                    range_io_size_max=args.range_io_size_max,
                    object_fetch_mode=args.object_fetch_mode,
                    object_fetch_max_multiplier=args.object_fetch_max_multiplier,
                    object_fetch_min_ranges=args.object_fetch_min_ranges,
                    object_range_merge_gap=args.object_range_merge_gap,
                    object_range_max_multiplier=args.object_range_max_multiplier,
                    object_range_min_ranges=args.object_range_min_ranges,
                    object_range_max_bytes=args.object_range_max_bytes,
                    reference_time_utc=(
                        _parse_utc(args.reference_time) if args.reference_time else None
                    ),
                )
            except Exception as exc:
                _append_product_failure_summary(
                    output_root,
                    product=product,
                    started_at_utc=product_started_at_utc,
                    started_monotonic=product_started_monotonic,
                    exc=exc,
                )
                raise
            print(
                json.dumps(
                    {
                        "coverage_id": manifest["coverage_id"],
                        "status": manifest["status"],
                        "files": len(manifest.get("files", [])),
                    },
                    ensure_ascii=False,
                )
            )
        return 0

    for required_arg in ("config", "model", "metadata", "now"):
        if not getattr(args, required_arg):
            parser.error(f"--{required_arg.replace('_', '-')} is required")

    if bool(args.source_url) != bool(args.byte_range):
        parser.error("--source-url and at least one --byte-range must be provided together")

    config = load_models(Path(args.config))
    product = config.products[args.model]
    runs = load_fixture_runs(Path(args.metadata))
    plan = build_product_coverage_plan(product, runs, _parse_utc(args.now))
    coverage_id = f"{product.name}_{plan.latest_complete_run}_{len(plan.slots)}h"
    output_root = Path(args.output)
    region_plan = _build_region_plan(product)
    if args.source_url:
        file_record = write_http_range_file(
            output_root,
            product.name,
            coverage_id,
            args.source_url,
            args.byte_range,
        )
    else:
        file_record = write_fixture_om_file(output_root, product.name, coverage_id)
    manifest = build_latest_manifest(product, runs, plan, [file_record], region_plan)
    atomic_write_json(output_root / "published" / product.name / "latest.json", manifest)
    print(json.dumps({"coverage_id": manifest["coverage_id"], "status": manifest["status"]}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
