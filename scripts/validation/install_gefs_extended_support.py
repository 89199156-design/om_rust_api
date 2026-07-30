#!/usr/bin/env python3
"""Install frozen NOAA GEFS extension frames as a new immutable coverage.

This tool never contacts Open-Meteo or NOAA.  It consumes full-grid float32
probability frames produced by ``diagnose_gefs_probability_support.py``, copies
the selected ncep_gefs05 bundle, appends those frames, and publishes a new
coverage/release.  The active group marker changes only with ``--activate``.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import shutil
import sys
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
DOWNLOADER_ROOT = REPO_ROOT / "downloader"
if str(DOWNLOADER_ROOT) not in sys.path:
    sys.path.insert(0, str(DOWNLOADER_ROOT))

from om_downloader.mirror_sync import GROUP_PRODUCT_SUMMARY_KEYS, group_release_id


PRODUCT = "ncep_gefs05"
VARIABLE = "precipitation_probability"
GRID_NY = 361
GRID_NX = 720
FRAME_BYTES = GRID_NY * GRID_NX * 4
DEFAULT_SUPPORT_HOURS = (402, 408, 414, 420)


def _load_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"expected JSON object: {path}")
    return value


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _atomic_write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    with temp.open("w", encoding="utf-8", newline="\n") as output:
        json.dump(value, output, ensure_ascii=False, indent=2, sort_keys=False)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    os.replace(temp, path)


def _parse_run(value: str) -> datetime:
    return datetime.strptime(value, "%Y%m%d%H").replace(tzinfo=timezone.utc)


def _format_utc(value: datetime) -> str:
    return value.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def _summary(manifest: dict[str, Any], old_summary: dict[str, Any]) -> dict[str, Any]:
    result = copy.deepcopy(old_summary)
    for key in GROUP_PRODUCT_SUMMARY_KEYS:
        result[key] = len(manifest["files"]) if key == "files" else manifest.get(key)
    result["status"] = "complete"
    return result


def _validate_inputs(
    data_root: Path,
    support_root: Path,
    expected_release_id: str,
    current_run: str,
    support_run: str,
    support_hours: tuple[int, ...],
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any], dict[str, Any]]:
    if not data_root.is_absolute() or not support_root.is_absolute():
        raise ValueError("data and support roots must be absolute paths")
    if data_root == Path("/") or support_root == Path("/"):
        raise ValueError("refusing a filesystem root")
    _parse_run(current_run)
    _parse_run(support_run)
    if not support_hours or len(set(support_hours)) != len(support_hours):
        raise ValueError("support hours must be non-empty and unique")
    if any(hour < 0 or hour % 6 != 0 for hour in support_hours):
        raise ValueError("GEFS extended support hours must be non-negative 6-hour steps")

    current_path = data_root / "groups/gfs/current/ready_for_processing.json"
    current = _load_json(current_path)
    if current.get("status") != "complete":
        raise ValueError("GFS current marker is not complete")
    if current.get("release_id") != expected_release_id:
        raise ValueError(
            f"GFS release changed: expected {expected_release_id}, "
            f"got {current.get('release_id')}"
        )
    if current.get("latest_complete_run") != current_run:
        raise ValueError(
            f"GFS run changed: expected {current_run}, "
            f"got {current.get('latest_complete_run')}"
        )

    release_path = data_root / "groups/gfs/releases" / f"{expected_release_id}.json"
    release = _load_json(release_path)
    if (
        release.get("release_id") != expected_release_id
        or release.get("latest_complete_run") != current_run
        or release.get("status") != "complete"
    ):
        raise ValueError("selected GFS release does not match the active marker")
    if group_release_id(release) != expected_release_id:
        raise ValueError("selected GFS release identity is invalid")

    product_summary = release.get("product_manifests", {}).get(PRODUCT)
    if not isinstance(product_summary, dict):
        raise ValueError(f"release has no {PRODUCT} summary")
    source_coverage_id = str(product_summary.get("coverage_id") or "")
    source_coverage = data_root / PRODUCT / "coverages" / source_coverage_id
    source_manifest = _load_json(source_coverage / "latest.json")
    if (
        source_manifest.get("model") != PRODUCT
        or source_manifest.get("status") != "complete"
        or source_manifest.get("coverage_id") != source_coverage_id
        or source_manifest.get("latest_complete_run") != current_run
    ):
        raise ValueError("source GEFS product manifest does not match the release")
    files = source_manifest.get("files")
    if not isinstance(files, list) or len(files) != 1:
        raise ValueError("source GEFS coverage must contain exactly one bundle")
    source_file = files[0]
    source_bundle = data_root / PRODUCT / str(source_file.get("path") or "")
    if not source_bundle.is_file():
        raise ValueError(f"source GEFS bundle is missing: {source_bundle}")
    if source_bundle.stat().st_size != int(source_file.get("bytes") or -1):
        raise ValueError("source GEFS bundle size mismatch")
    if _sha256(source_bundle) != source_file.get("sha256"):
        raise ValueError("source GEFS bundle checksum mismatch")

    metadata = _load_json(support_root / "metadata.json")
    for hour in support_hours:
        record = metadata.get(str(hour))
        frame = support_root / f"f{hour}.f32le"
        if not isinstance(record, dict) or not frame.is_file():
            raise ValueError(f"missing support metadata/frame for f{hour}")
        if frame.stat().st_size != FRAME_BYTES:
            raise ValueError(f"support frame f{hour} has unexpected size")
        if int(record.get("raw_bytes") or -1) != FRAME_BYTES:
            raise ValueError(f"support metadata size mismatch for f{hour}")
        if _sha256(frame) != record.get("raw_sha256"):
            raise ValueError(f"support frame checksum mismatch for f{hour}")
    return current, release, source_manifest, metadata


def _build_candidate(
    data_root: Path,
    support_root: Path,
    current: dict[str, Any],
    release: dict[str, Any],
    source_manifest: dict[str, Any],
    metadata: dict[str, Any],
    current_run: str,
    support_run: str,
    support_hours: tuple[int, ...],
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any], Path]:
    source_file = source_manifest["files"][0]
    source_bundle = data_root / PRODUCT / source_file["path"]
    identity = {
        "source_coverage_id": source_manifest["coverage_id"],
        "support_run": support_run,
        "support_frames": {
            str(hour): metadata[str(hour)]["raw_sha256"] for hour in support_hours
        },
        "schema": "gefs-extended-support-v1",
    }
    suffix = hashlib.sha256(
        json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()[:16]
    coverage_id = (
        f"{PRODUCT}_{current_run}_"
        f"{int(source_manifest.get('valid_time_count') or 0) + len(support_hours)}h_"
        f"extended_{suffix}"
    )
    coverage_root = data_root / PRODUCT / "coverages" / coverage_id
    relative_bundle = f"coverages/{coverage_id}/{PRODUCT}.omranges"

    manifest = copy.deepcopy(source_manifest)
    manifest["coverage_id"] = coverage_id
    manifest["generated_at"] = int(time.time())
    manifest["config_fingerprint"] = hashlib.sha256(
        json.dumps(
            {
                "source": source_manifest.get("config_fingerprint"),
                "extended_support": identity,
            },
            sort_keys=True,
            separators=(",", ":"),
        ).encode()
    ).hexdigest()
    manifest["source_runs"] = sorted(
        set(manifest.get("source_runs") or []) | {support_run}
    )
    manifest["interpolation_support_hours"] = max(
        int(manifest.get("interpolation_support_hours") or 0),
        max(
            0,
            int(
                (
                    _parse_run(support_run)
                    + timedelta(hours=max(support_hours))
                    - datetime.fromisoformat(
                        str(manifest["required_end_utc"]).replace("Z", "+00:00")
                    )
                ).total_seconds()
                // 3600
            ),
        ),
    )

    file_record = copy.deepcopy(source_file)
    file_record["path"] = relative_bundle
    entries = file_record.get("entries")
    if not isinstance(entries, list):
        raise ValueError("source bundle has no entry list")
    bundle_offset = source_bundle.stat().st_size
    support_records: list[dict[str, Any]] = []
    support_base = _parse_run(support_run)
    coverage_base = _parse_run(current_run)
    for hour in support_hours:
        valid = support_base + timedelta(hours=hour)
        coverage_hour = int((valid - coverage_base).total_seconds() // 3600)
        frame_record = metadata[str(hour)]
        source_descriptor = (
            "https://nomads.ncep.noaa.gov/pub/data/nccf/com/gens/prod/"
            f"gefs.{support_run[:8]}/{support_run[8:10]}/atmos/pgrb2ap5/"
            f"#APCP-f{hour:03d}-31-members"
        )
        entry = {
            "variable": VARIABLE,
            "variable_path": VARIABLE,
            "valid_time_utc": _format_utc(valid),
            "source_run": support_run,
            "forecast_hour": hour,
            "coverage_source_run": current_run,
            "coverage_forecast_hour": coverage_hour,
            "interpolation_support": True,
            "source_url": source_descriptor,
            "selection_ranges": [[0, GRID_NY], [0, GRID_NX]],
            "array": {
                "data_type": 20,
                "compression": 4,
                "dimensions": [GRID_NY, GRID_NX],
                "chunks": [GRID_NY, GRID_NX],
                "lut_offset": None,
                "lut_size": None,
                "scale_factor": 1.0,
                "add_offset": 0.0,
            },
            "lut_byte_ranges": [],
            "data_byte_ranges": [[0, FRAME_BYTES]],
            "lut_bytes_read": 0,
            "byte_ranges": [[0, FRAME_BYTES - 1]],
            "bundle_offset": bundle_offset,
            "bundle_bytes": FRAME_BYTES,
        }
        entries.append(entry)
        support_records.append(
            {
                "valid_time_utc": entry["valid_time_utc"],
                "source_run": support_run,
                "forecast_hour": hour,
                "coverage_source_run": current_run,
                "coverage_forecast_hour": coverage_hour,
                "variable": VARIABLE,
                "raw_bytes": FRAME_BYTES,
                "raw_sha256": frame_record["raw_sha256"],
                "source_url": source_descriptor,
            }
        )
        bundle_offset += FRAME_BYTES

    manifest["interpolation_support_entries"] = int(
        manifest.get("interpolation_support_entries") or 0
    ) + len(support_records)
    manifest["interpolation_support_records"] = list(
        manifest.get("interpolation_support_records") or []
    ) + support_records
    manifest["extended_support"] = {
        "schema": "gefs-extended-support-v1",
        "source_coverage_id": source_manifest["coverage_id"],
        "source_run": support_run,
        "hours": list(support_hours),
        "metadata_sha256": _sha256(support_root / "metadata.json"),
        "diagnostic_program": "scripts/validation/diagnose_gefs_probability_support.py",
        "frames": identity["support_frames"],
    }

    file_record["bytes"] = bundle_offset
    manifest["files"] = [file_record]
    manifest["bytes"] = bundle_offset
    manifest["downloaded_bytes"] = bundle_offset
    manifest["remote_content_length"] = bundle_offset
    manifest["sha256"] = {}

    candidate_release = copy.deepcopy(release)
    old_summary = candidate_release["product_manifests"][PRODUCT]
    new_summary = _summary(manifest, old_summary)
    candidate_release["product_manifests"][PRODUCT] = new_summary
    candidate_release["bytes"] = sum(
        int(item.get("bytes") or 0)
        for item in candidate_release["product_manifests"].values()
    )
    candidate_release["downloaded_bytes"] = sum(
        int(item.get("downloaded_bytes") or 0)
        for item in candidate_release["product_manifests"].values()
    )
    candidate_release["release_id"] = group_release_id(candidate_release)

    candidate_current = copy.deepcopy(current)
    delta = bundle_offset - int(source_manifest.get("downloaded_bytes") or 0)
    candidate_current["product_manifests"][PRODUCT] = new_summary
    candidate_current["release_id"] = candidate_release["release_id"]
    candidate_current["downloaded_bytes"] = int(
        candidate_current.get("downloaded_bytes") or 0
    ) + delta
    candidate_current["extended_support"] = {
        "product": PRODUCT,
        "coverage_id": coverage_id,
        "source_release_id": release["release_id"],
        "support_run": support_run,
        "support_hours": list(support_hours),
    }
    return manifest, candidate_release, candidate_current, coverage_root


def _prepare_coverage(
    data_root: Path,
    support_root: Path,
    source_manifest: dict[str, Any],
    manifest: dict[str, Any],
    coverage_root: Path,
    support_hours: tuple[int, ...],
) -> None:
    if coverage_root.exists():
        existing = _load_json(coverage_root / "latest.json")
        if existing.get("extended_support") != manifest.get("extended_support"):
            raise ValueError(f"immutable candidate coverage already differs: {coverage_root}")
        bundle = coverage_root / f"{PRODUCT}.omranges"
        if _sha256(bundle) != existing["files"][0]["sha256"]:
            raise ValueError("existing candidate bundle checksum mismatch")
        manifest.clear()
        manifest.update(existing)
        return

    incoming = (
        data_root
        / PRODUCT
        / ".incoming"
        / f"{coverage_root.name}.{os.getpid()}"
    )
    if incoming.exists():
        shutil.rmtree(incoming)
    incoming.mkdir(parents=True)
    destination = incoming / f"{PRODUCT}.omranges"
    source_bundle = data_root / PRODUCT / source_manifest["files"][0]["path"]
    try:
        with source_bundle.open("rb") as source, destination.open("wb") as output:
            shutil.copyfileobj(source, output, length=1024 * 1024)
            for hour in support_hours:
                with (support_root / f"f{hour}.f32le").open("rb") as frame:
                    shutil.copyfileobj(frame, output, length=1024 * 1024)
            output.flush()
            os.fsync(output.fileno())
        bundle_sha = _sha256(destination)
        manifest["files"][0]["sha256"] = bundle_sha
        manifest["sha256"] = {manifest["files"][0]["path"]: bundle_sha}
        _atomic_write_json(incoming / "latest.json", manifest)
        coverage_root.parent.mkdir(parents=True, exist_ok=True)
        os.replace(incoming, coverage_root)
    finally:
        if incoming.exists():
            shutil.rmtree(incoming)


def _activate(
    data_root: Path,
    support_root: Path,
    expected_release_id: str,
    release: dict[str, Any],
    current: dict[str, Any],
) -> Path:
    current_root = data_root / "groups/gfs/current"
    current_path = current_root / "ready_for_processing.json"
    if _load_json(current_path).get("release_id") != expected_release_id:
        raise ValueError("active release changed before activation")
    backup_root = support_root / "installation-backup" / (
        datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    )
    backup_root.mkdir(parents=True, exist_ok=False)
    shutil.copy2(current_path, backup_root / "ready_for_processing.json")
    current_latest = current_root / "latest.json"
    if current_latest.exists():
        shutil.copy2(current_latest, backup_root / "latest.json")

    release_path = (
        data_root / "groups/gfs/releases" / f"{release['release_id']}.json"
    )
    if release_path.exists():
        existing = _load_json(release_path)
        if existing != release:
            raise ValueError(f"immutable release already differs: {release_path}")
    else:
        _atomic_write_json(release_path, release)
    _atomic_write_json(current_latest, release)
    _atomic_write_json(current_path, current)
    _atomic_write_json(
        backup_root / "activation.json",
        {
            "activated_at": _format_utc(datetime.now(timezone.utc)),
            "previous_release_id": expected_release_id,
            "new_release_id": release["release_id"],
            "current_ready_path": str(current_path),
        },
    )
    return backup_root


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-root", type=Path, required=True)
    parser.add_argument("--support-root", type=Path, required=True)
    parser.add_argument("--expected-release-id", required=True)
    parser.add_argument("--current-run", required=True)
    parser.add_argument("--support-run", required=True)
    parser.add_argument(
        "--support-hours",
        type=int,
        nargs="+",
        default=list(DEFAULT_SUPPORT_HOURS),
    )
    parser.add_argument(
        "--activate",
        action="store_true",
        help="atomically select the prepared release after backing up current markers",
    )
    args = parser.parse_args()
    support_hours = tuple(sorted(args.support_hours))
    current, release, source_manifest, metadata = _validate_inputs(
        args.data_root,
        args.support_root,
        args.expected_release_id,
        args.current_run,
        args.support_run,
        support_hours,
    )
    manifest, candidate_release, candidate_current, coverage_root = _build_candidate(
        args.data_root,
        args.support_root,
        current,
        release,
        source_manifest,
        metadata,
        args.current_run,
        args.support_run,
        support_hours,
    )
    _prepare_coverage(
        args.data_root,
        args.support_root,
        source_manifest,
        manifest,
        coverage_root,
        support_hours,
    )
    candidate_state_root = (
        args.support_root
        / "installation-candidates"
        / candidate_release["release_id"]
    )
    _atomic_write_json(candidate_state_root / "release.json", candidate_release)
    _atomic_write_json(
        candidate_state_root / "ready_for_processing.json",
        candidate_current,
    )

    result: dict[str, Any] = {
        "status": "prepared",
        "coverage_id": manifest["coverage_id"],
        "coverage_path": str(coverage_root),
        "coverage_bytes": manifest["bytes"],
        "coverage_sha256": manifest["files"][0]["sha256"],
        "source_release_id": args.expected_release_id,
        "release_id": candidate_release["release_id"],
        "release_path": str(candidate_state_root / "release.json"),
        "candidate_current_path": str(
            candidate_state_root / "ready_for_processing.json"
        ),
        "activated": False,
    }
    if args.activate:
        backup_root = _activate(
            args.data_root,
            args.support_root,
            args.expected_release_id,
            candidate_release,
            candidate_current,
        )
        result["status"] = "activated"
        result["activated"] = True
        result["backup_root"] = str(backup_root)
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
