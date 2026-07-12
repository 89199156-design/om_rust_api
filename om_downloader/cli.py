from __future__ import annotations

import argparse
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, as_completed, wait
from contextlib import redirect_stdout
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

from .checksum import sha256_file
from .coverage import (
    build_complete_run_coverage_plan,
    build_coverage_plan,
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
    sync_from_manifest_path,
    sync_from_manifest_url,
    sync_group_from_mirror,
    sync_retained_group_releases_from_mirror,
)
from .http_range import ByteRange
from .store import write_fixture_om_file, write_http_range_file, write_om_coverage_bundle_file

OPENMETEO_GROUP_PRODUCTS = {
    "gfs": ("gfs013_surface", "gfs025", "gfs_pressure_profile"),
    "cams": ("cams_global", "cams_global_greenhouse_gases"),
}
GROUPS_REQUIRING_MATCHING_RUNS = frozenset({"gfs", "cams"})

APP_LOG_RETENTION_DAYS = 45
APP_LOG_MAX_BYTES = 4 * 1024 * 1024


def _parse_utc(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


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


def _read_json_if_exists(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def _build_product_plan(
    product: ProductConfig,
    *,
    now_utc: datetime,
    bucket_url: str,
) -> tuple[Any, list[Any], Any]:
    latest_catalog = load_openmeteo_spatial_latest(
        product.openmeteo_model,
        bucket_url=bucket_url,
    )
    required_start = required_start_for_anchors(now_utc, product.timezone_anchors)
    runs = discover_openmeteo_spatial_runs(
        product.name,
        latest_catalog,
        bucket_url=bucket_url,
        required_start_utc=required_start,
        run_cadence_hours=product.run_cadence_hours,
    )
    return latest_catalog, runs, build_coverage_plan(product, runs, now_utc)


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
    print(
        json.dumps(
            {
                "stage": stage,
                "product": product.name,
                "coverage_id": coverage_id,
                "planned_entries": planned_entries,
                "planned_ranges": planned_ranges,
                "downloaded_ranges": downloaded_ranges,
                "written_bytes": written_bytes,
                "elapsed_seconds": 0.0,
                "average_mib_s": 0.0,
                "current_mib_s": 0.0,
                "active_workers": active_workers,
                "range_workers": range_workers,
                "planning_workers": planning_workers,
                "variable_plan_workers": variable_plan_workers,
                "queue_size": 0,
                "pending_futures": 0,
                "reused_existing": reused_existing,
            },
            ensure_ascii=False,
        ),
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
        _, runs, plan = _build_product_plan(product, now_utc=now_utc, bucket_url=bucket_url)
    else:
        _, runs, plan = plan_data
    coverage_id = _coverage_id_for_plan(product, plan)
    existing_manifest_path = output_root / "published" / product.name / "latest.json"
    existing_manifest = _read_json_if_exists(existing_manifest_path)
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

    object_records = coverage_object_records(
        plan,
        runs,
        bucket_url=bucket_url,
        openmeteo_model=product.openmeteo_model,
    )
    region_plan: dict[str, Any] | None = None
    missing_object_required_variables = []
    wanted_variables = tuple(dict.fromkeys(list(product.required_variables) + list(product.optional_variables)))
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
        source, remote_content_length, inventory = inventory_for_url(
            object_record["url"],
            wanted_variables,
        )
        missing_for_object = sorted(set(product.required_variables) - set(inventory.arrays))
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
            selected_inventory_variables(product, inventory),
            source,
            inventory,
            object_record,
            object_record["url"],
            remote_content_length,
        )
        if missing_for_object:
            remaining_missing = set(missing_for_object)
            valid_time = _parse_utc(str(object_record["valid_time_utc"]))
            primary_run = str(object_record["source_run"])
            fallback_runs = sorted(
                (
                    run
                    for run in runs
                    if run.run_id != primary_run
                    and _forecast_hour_for_run(run, valid_time) is not None
                ),
                key=lambda item: _as_utc(item.base_time_utc),
                reverse=True,
            )
            for fallback_run in fallback_runs:
                if not remaining_missing:
                    break
                fallback_forecast_hour = _forecast_hour_for_run(fallback_run, valid_time)
                if fallback_forecast_hour is None:
                    continue
                fallback_url = openmeteo_spatial_object_url(
                    bucket_url,
                    product.openmeteo_model,
                    reference_time_utc=fallback_run.base_time_utc,
                    valid_time_utc=valid_time,
                )
                try:
                    _fallback_source, fallback_content_length, fallback_inventory = inventory_for_url(
                        fallback_url,
                        tuple(remaining_missing),
                    )
                except Exception:
                    continue
                fallback_object_record = {
                    "valid_time_utc": object_record["valid_time_utc"],
                    "source_run": fallback_run.run_id,
                    "forecast_hour": fallback_forecast_hour,
                }
                fallback_variables = tuple(
                    variable
                    for variable in product.required_variables
                    if variable in remaining_missing and variable in fallback_inventory.arrays
                )
                append_planned_variables(
                    fallback_variables,
                    _fallback_source,
                    fallback_inventory,
                    fallback_object_record,
                    fallback_url,
                    fallback_content_length,
                )
                for variable in fallback_variables:
                    remaining_missing.remove(variable)
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

    files = []
    planned_entries = iter_planned_entries()
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
                    "progress_interval_seconds": 10,
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
    if region_plan is None:
        region_plan = _build_region_plan(product)
    manifest = build_latest_manifest(product, runs, plan, files, region_plan)
    catalog_missing_required_variables = manifest["missing_required_variables"]
    manifest["missing_object_required_variables"] = missing_object_required_variables
    required_variables = set(product.required_variables)
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
    if missing_bundle_required_variables or manifest["missing_pressure_levels_hpa"]:
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
    atomic_write_json(output_root / "published" / "groups" / group_name / "latest.json", payload)
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
    try:
        with file_lock(output_root / "locks" / "openmeteo_download.lock"), file_lock(
            output_root / "locks" / f"{group_name}.lock"
        ):
            products = [config.products[name] for name in product_names]
            plan_by_product = plan_by_product_override or {
                product.name: _build_product_plan(
                    product,
                    now_utc=now_utc,
                    bucket_url=args.openmeteo_bucket_url,
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
                if not self_publish:
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

            group_manifest = _write_group_manifest(output_root, group_name, product_manifests)
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
                    },
                    ensure_ascii=False,
                )
            )
            return 0
    except RuntimeError as exc:
        if "already running" not in str(exc):
            raise
        _append_group_run_summary(
            output_root,
            group_name=group_name,
            status="skipped",
            reason="group already running",
            product_runs=runs_by_product,
            started_at_utc=group_started_at_utc,
            started_monotonic=group_started_monotonic,
        )
        print(
            json.dumps(
                {
                    "group": group_name,
                    "status": "skipped",
                    "reason": "group already running",
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
        if not isinstance(summary, dict) or summary.get("latest_complete_run") != group_run:
            return False
        coverage_id = str(summary.get("coverage_id") or "")
        coverage_root = api_root / product_name / "coverages" / coverage_id
        product_manifest = _read_json_if_exists(coverage_root / "latest.json")
        if not product_manifest or product_manifest.get("status") != "complete":
            return False
        if product_manifest.get("coverage_id") != coverage_id:
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


def _available_group_releases(
    api_root: Path,
    group_name: str,
) -> dict[str, dict[str, Any]]:
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
    available: dict[str, dict[str, Any]] = {}
    for manifest in manifests:
        run = str(manifest.get("latest_complete_run") or "")
        if run and _group_release_payload_is_available(api_root, group_name, manifest):
            available[run] = manifest
    return available


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
) -> list[tuple[str, dict[str, tuple[Any, list[Any], Any]]]]:
    if count < 1:
        raise ValueError("complete group run count must be positive")
    latest_catalogs = {
        product.name: load_openmeteo_spatial_latest(
            product.openmeteo_model,
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
    raise ValueError(f"could not discover {count} recent complete coherent CAMS runs")


def _reconcile_cams_complete_runs(
    args: argparse.Namespace,
    parser: argparse.ArgumentParser,
) -> int:
    if not args.config:
        parser.error("--config is required with --download-openmeteo-group")
    if args.retain_complete_releases != 3:
        parser.error("CAMS requires --retain-complete-releases 3")

    config = load_models(Path(args.config))
    products = [config.products[name] for name in OPENMETEO_GROUP_PRODUCTS["cams"]]
    target_plans = _discover_recent_complete_group_plans(
        products,
        bucket_url=args.openmeteo_bucket_url,
        count=3,
    )
    api_root = Path(args.publish_openmeteo_group_to)
    prune_expired_group_releases(api_root, "cams", retain_complete_releases=3)
    target_runs = [run_id for run_id, _plans in target_plans]
    plans_by_run = dict(target_plans)
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
            download_results.append(json.loads(lines[-1]))

    prune_expired_group_releases(api_root, "cams", retain_complete_releases=3)
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
    pruned = prune_expired_group_releases(api_root, "cams", retain_complete_releases=3)
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
                "pruned_raw_paths": pruned,
            },
            ensure_ascii=False,
        )
    )
    return 0


def _download_openmeteo_group(args: argparse.Namespace, parser: argparse.ArgumentParser) -> int:
    if args.download_openmeteo_group != "cams":
        return _download_openmeteo_group_release(args, parser)
    effective_args = args
    if not args.publish_openmeteo_group_to:
        effective_args = copy(args)
        effective_args.publish_openmeteo_group_to = str(Path(args.output) / "published")
    output_root = Path(args.output)
    try:
        with file_lock(output_root / "locks" / "cams_reconcile.lock"):
            return _reconcile_cams_complete_runs(effective_args, parser)
    except RuntimeError as exc:
        if "already running" not in str(exc):
            raise
        print(
            json.dumps(
                {
                    "group": "cams",
                    "status": "skipped",
                    "reason": "CAMS reconciliation already running",
                },
                ensure_ascii=False,
            )
        )
        return 0


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
    parser.add_argument("--build-processing-stage")
    parser.add_argument("--sync-from-manifest-url")
    parser.add_argument("--sync-from-manifest-path")
    parser.add_argument("--sync-openmeteo-group-from-source")
    parser.add_argument("--sync-openmeteo-group-releases-from-source")
    parser.add_argument("--print-openmeteo-group-release-id")
    parser.add_argument("--publish-openmeteo-group-to")
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
    parser.add_argument("--source-url")
    parser.add_argument("--byte-range", action="append", type=_parse_byte_range, default=[])
    args = parser.parse_args(argv)

    if args.inspect_openmeteo_model:
        catalog = load_openmeteo_spatial_latest(
            args.inspect_openmeteo_model,
            bucket_url=args.openmeteo_bucket_url,
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
        catalog = load_openmeteo_spatial_latest(
            product.openmeteo_model,
            bucket_url=args.openmeteo_bucket_url,
        )
        available = set(catalog.variables)
        missing_required = sorted(set(product.required_variables) - available)
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
            required_start = required_start_for_anchors(_parse_utc(args.now), product.timezone_anchors)
            runs = discover_openmeteo_spatial_runs(
                product.name,
                catalog,
                bucket_url=args.openmeteo_bucket_url,
                required_start_utc=required_start,
                run_cadence_hours=product.run_cadence_hours,
            )
            plan = build_coverage_plan(product, runs, _parse_utc(args.now))
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
        result = sync_group_from_mirror(
            args.sync_openmeteo_group_from_source,
            Path(source_root),
            Path(args.output),
            cleanup_grace_seconds=args.cleanup_grace_seconds,
            retain_complete_releases=args.retain_complete_releases,
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
        result = sync_retained_group_releases_from_mirror(
            args.sync_openmeteo_group_releases_from_source,
            Path(source_root),
            Path(args.output),
            retain_complete_releases=args.retain_complete_releases,
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
    plan = build_coverage_plan(product, runs, _parse_utc(args.now))
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
