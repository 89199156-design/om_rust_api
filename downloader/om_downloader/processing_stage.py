from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
import json
from pathlib import Path
from typing import Any

from .locking import file_lock
from .manifest import atomic_write_json


@dataclass(frozen=True)
class ProcessingStage:
    name: str
    group: str | None
    products: tuple[str, ...]


STAGES: dict[str, ProcessingStage] = {
    "gfs013_surface": ProcessingStage("gfs013_surface", "gfs", ("gfs013_surface",)),
    "gfs_point_package": ProcessingStage(
        "gfs_point_package", "gfs", ("gfs013_surface", "gfs025")
    ),
    "gfs_pressure_profile": ProcessingStage(
        "gfs_pressure_profile", "gfs", ("gfs_pressure_profile",)
    ),
    "gfs_derived": ProcessingStage(
        "gfs_derived", "gfs", ("gfs013_surface", "gfs025", "gfs_pressure_profile")
    ),
    "cams_global": ProcessingStage(
        "cams_global",
        "cams",
        ("cams_global", "cams_global_greenhouse_gases"),
    ),
    "cleanup": ProcessingStage("cleanup", None, ()),
}


def _utc_timestamp() -> int:
    return int(datetime.now(timezone.utc).timestamp())


def _read_json(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def _ready_manifest_is_complete(manifest: dict[str, Any] | None) -> bool:
    if not manifest:
        return False
    return (
        manifest.get("status") == "complete"
        and manifest.get("latest_complete_run")
        and manifest.get("files") is not None
        and manifest.get("bytes") is not None
    )


def _status_path(output_root: Path, stage: str) -> Path:
    return output_root / "build_status" / stage / "latest.json"


def _write_stage_status(output_root: Path, stage: str, payload: dict[str, Any]) -> dict[str, Any]:
    atomic_write_json(_status_path(output_root, stage), payload)
    return payload


def _base_payload(stage: ProcessingStage) -> dict[str, Any]:
    return {
        "stage": stage.name,
        "generated_at": _utc_timestamp(),
        "required_group": stage.group,
        "required_products": list(stage.products),
    }


def build_processing_stage(stage_name: str, *, raw_root: Path, output_root: Path) -> dict[str, Any]:
    if stage_name not in STAGES:
        raise ValueError(f"unknown processing stage: {stage_name}")
    stage = STAGES[stage_name]

    try:
        with file_lock(output_root / "locks" / f"{stage.name}.lock"):
            return _build_processing_stage_locked(stage, raw_root=raw_root, output_root=output_root)
    except RuntimeError as exc:
        if "already running" not in str(exc):
            raise
        payload = _base_payload(stage)
        payload.update({"status": "skipped", "reason": "stage already running"})
        return _write_stage_status(output_root, stage.name, payload)


def _build_processing_stage_locked(
    stage: ProcessingStage, *, raw_root: Path, output_root: Path
) -> dict[str, Any]:
    payload = _base_payload(stage)

    group_manifest: dict[str, Any] | None = None
    if stage.group:
        group_path = raw_root / "groups" / stage.group / "current" / "ready_for_processing.json"
        group_manifest = _read_json(group_path)
        payload["group_ready_path"] = str(group_path)
        if group_manifest is None:
            payload.update({"status": "skipped", "reason": "group ready missing"})
            return _write_stage_status(output_root, stage.name, payload)
        if not _ready_manifest_is_complete(group_manifest):
            payload.update({"status": "skipped", "reason": "group ready incomplete"})
            return _write_stage_status(output_root, stage.name, payload)
        payload["latest_complete_run"] = group_manifest["latest_complete_run"]

    product_ready_paths: dict[str, str] = {}
    for product in stage.products:
        product_path = raw_root / product / "current" / "ready_for_processing.json"
        product_ready_paths[product] = str(product_path)
        product_manifest = _read_json(product_path)
        if product_manifest is None:
            payload.update(
                {
                    "status": "skipped",
                    "reason": "product ready missing",
                    "product": product,
                    "product_ready_paths": product_ready_paths,
                }
            )
            return _write_stage_status(output_root, stage.name, payload)
        if not _ready_manifest_is_complete(product_manifest):
            payload.update(
                {
                    "status": "skipped",
                    "reason": "product ready incomplete",
                    "product": product,
                    "product_ready_paths": product_ready_paths,
                }
            )
            return _write_stage_status(output_root, stage.name, payload)
        if group_manifest and product_manifest.get("latest_complete_run") != group_manifest.get(
            "latest_complete_run"
        ):
            payload.update(
                {
                    "status": "skipped",
                    "reason": "product run mismatch",
                    "product": product,
                    "product_ready_paths": product_ready_paths,
                    "product_latest_complete_run": product_manifest.get("latest_complete_run"),
                    "group_latest_complete_run": group_manifest.get("latest_complete_run"),
                }
            )
            return _write_stage_status(output_root, stage.name, payload)

    payload.update(
        {
            "status": "pending_implementation",
            "reason": "processing stage is not implemented yet",
            "product_ready_paths": product_ready_paths,
        }
    )
    return _write_stage_status(output_root, stage.name, payload)
