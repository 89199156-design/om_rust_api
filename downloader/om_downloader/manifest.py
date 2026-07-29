from __future__ import annotations

from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
from typing import Any

from .coverage import CoveragePlan
from .ecmwf_catalog import DAILY_VARIABLES as ECMWF_DAILY_VARIABLES
from .ecmwf_catalog import HOURLY_VARIABLES as ECMWF_HOURLY_VARIABLES
from .metadata import OmRun
from .model_config import ProductConfig
from .region import bounds_to_dict


def _format_utc(value: datetime) -> str:
    return value.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def _union_variables(runs: list[OmRun]) -> set[str]:
    result: set[str] = set()
    for run in runs:
        result.update(run.variables)
    return result


def _union_pressure_levels(runs: list[OmRun]) -> set[int]:
    result: set[int] = set()
    for run in runs:
        result.update(run.pressure_levels_hpa)
    return result


def _total_int(files: list[dict[str, Any]], key: str, fallback_key: str | None = None) -> int:
    total = 0
    for file_record in files:
        value = file_record.get(key)
        if value is None and fallback_key is not None:
            value = file_record.get(fallback_key, 0)
        total += int(value or 0)
    return total


def _sha256_map(files: list[dict[str, Any]]) -> dict[str, str]:
    return {
        str(file_record["path"]): str(file_record["sha256"])
        for file_record in files
        if "path" in file_record and "sha256" in file_record
    }


def product_config_fingerprint(product: ProductConfig) -> str:
    payload = {
        "download_product": product.download_product,
        "openmeteo_model": product.openmeteo_model,
        "required_variables": list(product.required_variables),
        "required_sparse_variables": list(product.required_sparse_variables),
        "required_initial_fallback_variables": list(
            product.required_initial_fallback_variables
        ),
        "interpolation_support_hours": product.interpolation_support_hours,
        "missing_variable_fallback_lookback_hours": (
            product.missing_variable_fallback_lookback_hours
        ),
        "missing_variable_fallback_context_hours": (
            product.missing_variable_fallback_context_hours
        ),
        "missing_variable_fallback_predecessor_runs": (
            product.missing_variable_fallback_predecessor_runs
        ),
        "optional_variables": list(product.optional_variables),
        "requested_pressure_levels_hpa": list(product.requested_pressure_levels_hpa),
        "requested_bounds": bounds_to_dict(product.requested_bounds),
        "bounds_padding_degrees": product.bounds_padding_degrees,
        "forecast_hour_start": product.forecast_hour_start,
        "forecast_hour_end": product.forecast_hour_end,
        "history_hours": product.history_hours,
        "timezone_anchors": list(product.timezone_anchors),
        "coverage_strategy": product.coverage_strategy,
    }
    encoded = json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def build_latest_manifest(
    product: ProductConfig,
    runs: list[OmRun],
    plan: CoveragePlan,
    files: list[dict[str, Any]],
    region_plan: dict[str, Any],
) -> dict[str, Any]:
    available_variables = _union_variables(runs)
    missing_required = sorted(
        (set(product.required_variables) | set(product.required_sparse_variables))
        - available_variables
    )
    missing_optional = sorted(set(product.optional_variables) - available_variables)
    available_levels = sorted(_union_pressure_levels(runs), reverse=True)
    requested_levels = set(product.requested_pressure_levels_hpa)
    missing_levels = sorted(requested_levels - set(available_levels), reverse=True)
    spatial_ranges = list(region_plan.get("spatial_ranges", []))
    complete = not missing_required and not missing_levels and bool(files) and bool(spatial_ranges)

    payload = {
        "model": product.name,
        "download_product": product.download_product,
        "coverage_id": f"{product.name}_{plan.latest_complete_run}_{len(plan.slots)}h",
        "config_fingerprint": product_config_fingerprint(product),
        "status": "complete" if complete else "incomplete",
        "generated_at": int(datetime.now(timezone.utc).timestamp()),
        "required_start_utc": _format_utc(plan.required_start_utc),
        "public_start_utc": _format_utc(plan.public_start_utc or plan.required_start_utc),
        "required_end_utc": _format_utc(plan.required_end_utc),
        "forecast_hour_start": product.forecast_hour_start,
        "forecast_hour_end": product.forecast_hour_end,
        "coverage_strategy": product.coverage_strategy,
        "latest_complete_run": plan.latest_complete_run,
        "valid_time_count": len(plan.slots),
        "timezone_anchors": list(product.timezone_anchors),
        "available_variables": sorted(available_variables),
        "missing_required_variables": missing_required,
        "required_sparse_variables": list(product.required_sparse_variables),
        "required_initial_fallback_variables": list(
            product.required_initial_fallback_variables
        ),
        "interpolation_support_hours": product.interpolation_support_hours,
        "missing_variable_fallback_lookback_hours": (
            product.missing_variable_fallback_lookback_hours
        ),
        "missing_variable_fallback_context_hours": (
            product.missing_variable_fallback_context_hours
        ),
        "missing_variable_fallback_predecessor_runs": (
            product.missing_variable_fallback_predecessor_runs
        ),
        "missing_optional_variables": missing_optional,
        "available_pressure_levels_hpa": available_levels,
        "missing_pressure_levels_hpa": missing_levels,
        "source_runs": sorted({slot.source_run for slot in plan.slots}),
        "coverage_plan": [
            {
                "valid_time_utc": _format_utc(slot.valid_time_utc),
                "source_run": slot.source_run,
                "forecast_hour": slot.forecast_hour,
            }
            for slot in plan.slots
        ],
        "files": files,
        "bytes": _total_int(files, "bytes"),
        "sha256": _sha256_map(files),
        "requested_bounds": region_plan.get("requested_bounds", bounds_to_dict(product.requested_bounds)),
        "padded_bounds": region_plan.get("padded_bounds"),
        "grid_bounds": region_plan.get("grid_bounds"),
        "spatial_ranges": spatial_ranges,
        "remote_content_length": _total_int(files, "remote_content_length", "bytes"),
        "downloaded_bytes": _total_int(files, "downloaded_bytes", "bytes"),
    }
    if product.name == "ecmwf_ifs025":
        payload["available_raw_variables"] = payload["available_variables"]
        payload["available_hourly_variables"] = list(ECMWF_HOURLY_VARIABLES)
        payload["available_daily_variables"] = list(ECMWF_DAILY_VARIABLES)
        payload["available_variables"] = sorted(
            set(ECMWF_HOURLY_VARIABLES) | set(ECMWF_DAILY_VARIABLES)
        )
    return payload


def atomic_write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_name(path.name + ".tmp")
    temp_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
    os.replace(temp_path, path)
