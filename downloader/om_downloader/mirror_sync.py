from __future__ import annotations

from datetime import datetime, timezone
import errno
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
from typing import Any
from urllib.parse import urljoin
from urllib.request import urlopen

from .checksum import sha256_file

OPENMETEO_GROUP_PRODUCTS = {
    "gfs": ("gfs013_surface", "gfs025", "gfs_pressure_profile"),
    "cams": ("cams_global", "cams_global_greenhouse_gases"),
}
MINIMUM_GROUP_PRODUCTS = {
    "gfs": ("gfs013_surface", "gfs025", "gfs_pressure_profile"),
    "cams": ("cams_global", "cams_global_greenhouse_gases"),
}
GROUPS_REQUIRING_MATCHING_RUNS = frozenset({"gfs"})
DEFAULT_COMPLETE_RELEASE_RETENTION = 3
GROUP_PRODUCT_SUMMARY_KEYS = (
    "coverage_id",
    "latest_complete_run",
    "required_start_utc",
    "public_start_utc",
    "required_end_utc",
    "valid_time_count",
    "files",
    "bytes",
    "downloaded_bytes",
)


def _fetch_json(url: str, *, timeout: int) -> dict[str, Any]:
    with urlopen(url, timeout=timeout) as response:
        payload = response.read()
    parsed = json.loads(payload.decode("utf-8"))
    if not isinstance(parsed, dict):
        raise ValueError("remote manifest must be a JSON object")
    return parsed


def _load_json(path: Path) -> dict[str, Any]:
    parsed = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(parsed, dict):
        raise ValueError("manifest must be a JSON object")
    return parsed


def _safe_relative_path(path: str) -> PurePosixPath:
    posix_path = PurePosixPath(path)
    if posix_path.is_absolute() or ".." in posix_path.parts:
        raise ValueError(f"unsafe manifest file path: {path}")
    return posix_path


def _atomic_write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_name(path.name + ".tmp")
    temp_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
    os.replace(temp_path, path)


def _download_file(url: str, path: Path, *, timeout: int) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_name(path.name + ".tmp")
    with urlopen(url, timeout=timeout) as response, temp_path.open("wb") as file_obj:
        shutil.copyfileobj(response, file_obj)
    os.replace(temp_path, path)


def _copy_file(source: Path, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path = path.with_name(path.name + ".tmp")
    shutil.copy2(source, temp_path)
    os.replace(temp_path, path)


def _format_now_utc() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def _format_utc(value: datetime) -> str:
    return value.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def _parse_utc(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError(f"timestamp must include timezone: {value}")
    return parsed.astimezone(timezone.utc)


def _products_from_group_manifest(group_manifest: dict[str, Any], group: str) -> tuple[str, ...]:
    summaries = group_manifest.get("product_manifests")
    if not isinstance(summaries, dict):
        raise ValueError(f"group manifest has invalid product summaries: {group}")
    allowed = OPENMETEO_GROUP_PRODUCTS[group]
    unexpected = sorted(set(summaries) - set(allowed))
    if unexpected:
        raise ValueError(f"group manifest contains unknown products: {', '.join(unexpected)}")
    missing = [product for product in MINIMUM_GROUP_PRODUCTS[group] if product not in summaries]
    if missing:
        raise ValueError(f"group manifest missing products: {', '.join(missing)}")
    return tuple(product for product in allowed if product in summaries)


def group_release_id(group_manifest: dict[str, Any]) -> str:
    group = str(group_manifest.get("group") or "")
    if group not in OPENMETEO_GROUP_PRODUCTS:
        raise ValueError(f"unknown Open-Meteo group: {group}")
    if not _group_manifest_is_complete(group_manifest, group):
        raise ValueError(f"group manifest is not complete: {group}")
    product_manifests = group_manifest["product_manifests"]
    products = _products_from_group_manifest(group_manifest, group)
    summary = {
        product: {
            key: product_manifests.get(product, {}).get(key)
            for key in GROUP_PRODUCT_SUMMARY_KEYS
        }
        for product in products
    }
    encoded = json.dumps(
        {"group": group, "products": summary},
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    return f"{group}-{hashlib.sha256(encoded).hexdigest()[:16]}"


def _prepare_manifest_stage(
    manifest: dict[str, Any],
    output_root: Path,
    *,
    source_label_key: str,
    source_label: str,
    fetch_file,
) -> dict[str, Any]:
    model = str(manifest.get("model", ""))
    coverage_id = str(manifest.get("coverage_id", ""))
    if not model or not coverage_id:
        raise ValueError("manifest must include model and coverage_id")
    if manifest.get("status") != "complete":
        return {
            "status": "skipped",
            "reason": f"manifest status is {manifest.get('status')}",
            "model": model,
            "coverage_id": coverage_id,
        }

    files = manifest.get("files")
    if not isinstance(files, list) or not files:
        raise ValueError("complete manifest must include files")

    model_root = output_root / model
    incoming_root = model_root / ".incoming" / coverage_id
    coverage_root = model_root / "coverages" / coverage_id
    if incoming_root.exists():
        shutil.rmtree(incoming_root)
    incoming_root.mkdir(parents=True, exist_ok=True)

    try:
        for file_record in files:
            if not isinstance(file_record, dict):
                raise ValueError("manifest file record must be an object")
            relative_path = _safe_relative_path(str(file_record["path"]))
            if len(relative_path.parts) < 2 or relative_path.parts[0] != "coverages" or relative_path.parts[1] != coverage_id:
                raise ValueError(f"manifest file path does not belong to coverage {coverage_id}: {relative_path}")
            inside_coverage = PurePosixPath(*relative_path.parts[2:])
            destination = incoming_root / Path(*inside_coverage.parts)
            fetch_file(relative_path, destination)
            expected_size = int(file_record.get("bytes", 0))
            if expected_size and destination.stat().st_size != expected_size:
                raise ValueError(f"downloaded size mismatch for {relative_path}")
            expected_sha = str(file_record.get("sha256", ""))
            if expected_sha and sha256_file(destination) != expected_sha:
                raise ValueError(f"sha256 mismatch for {relative_path}")

        _atomic_write_json(incoming_root / "latest.json", manifest)
    except Exception:
        shutil.rmtree(incoming_root, ignore_errors=True)
        raise
    ready = {
        "model": model,
        "coverage_id": coverage_id,
        "status": manifest.get("status"),
        "latest_complete_run": manifest.get("latest_complete_run"),
        "files": len(files),
        "bytes": manifest.get("bytes"),
        source_label_key: source_label,
        "coverage_dir": str(coverage_root),
        "synced_at": _format_now_utc(),
    }
    return {
        "status": "staged",
        "model": model,
        "coverage_id": coverage_id,
        "manifest": manifest,
        "incoming_root": incoming_root,
        "coverage_root": coverage_root,
        "current_root": model_root / "current",
        "ready": ready,
        "files": len(files),
    }


def _manifest_stage_bytes(manifest: dict[str, Any]) -> int:
    files = manifest.get("files")
    if not isinstance(files, list) or not files:
        raise ValueError("complete manifest must include files")
    total = 0
    for file_record in files:
        if not isinstance(file_record, dict):
            raise ValueError("manifest file record must be an object")
        size = int(file_record.get("bytes", 0))
        if size < 0:
            raise ValueError("manifest file size must not be negative")
        total += size
    return total


def _ensure_stage_capacity(output_root: Path, manifests: list[dict[str, Any]]) -> None:
    output_root.mkdir(parents=True, exist_ok=True)
    required_bytes = sum(_manifest_stage_bytes(manifest) for manifest in manifests)
    available_bytes = shutil.disk_usage(output_root).free
    if available_bytes < required_bytes:
        raise OSError(
            errno.ENOSPC,
            f"insufficient free space for atomic group staging: need {required_bytes} bytes, "
            f"available {available_bytes} bytes",
        )


def _remove_stage(stage: dict[str, Any]) -> None:
    incoming_root = stage.get("incoming_root")
    if isinstance(incoming_root, Path):
        shutil.rmtree(incoming_root, ignore_errors=True)


def _promote_manifest_stage(stage: dict[str, Any]) -> dict[str, Any]:
    if stage.get("status") != "staged":
        return stage
    coverage_root = stage["coverage_root"]
    incoming_root = stage["incoming_root"]
    if coverage_root.exists():
        shutil.rmtree(coverage_root)
    coverage_root.parent.mkdir(parents=True, exist_ok=True)
    os.replace(incoming_root, coverage_root)

    current_root = stage["current_root"]
    _atomic_write_json(current_root / "latest.json", stage["manifest"])
    _atomic_write_json(current_root / "ready_for_processing.json", stage["ready"])
    return {
        "status": "synced",
        "model": stage["model"],
        "coverage_id": stage["coverage_id"],
        "files": stage["files"],
    }


def _sync_manifest(
    manifest: dict[str, Any],
    output_root: Path,
    *,
    source_label_key: str,
    source_label: str,
    fetch_file,
) -> dict[str, Any]:
    stage = _prepare_manifest_stage(
        manifest,
        output_root,
        source_label_key=source_label_key,
        source_label=source_label,
        fetch_file=fetch_file,
    )
    return _promote_manifest_stage(stage)


def sync_from_manifest_url(manifest_url: str, output_root: Path, *, timeout: int = 30) -> dict[str, Any]:
    manifest = _fetch_json(manifest_url, timeout=timeout)

    def fetch_file(relative_path: PurePosixPath, destination: Path) -> None:
        _download_file(urljoin(manifest_url, str(relative_path)), destination, timeout=timeout)

    return _sync_manifest(
        manifest,
        output_root,
        source_label_key="source_manifest_url",
        source_label=manifest_url,
        fetch_file=fetch_file,
    )


def sync_from_manifest_path(manifest_path: Path, output_root: Path) -> dict[str, Any]:
    manifest_path = manifest_path.resolve()
    source_root = manifest_path.parent
    manifest = _load_json(manifest_path)

    def fetch_file(relative_path: PurePosixPath, destination: Path) -> None:
        source = source_root / Path(*relative_path.parts)
        _copy_file(source, destination)

    return _sync_manifest(
        manifest,
        output_root,
        source_label_key="source_manifest_path",
        source_label=str(manifest_path),
        fetch_file=fetch_file,
    )


def _group_manifest_is_complete(group_manifest: dict[str, Any], group: str) -> bool:
    if group_manifest.get("group") != group or group_manifest.get("status") != "complete":
        return False
    latest_complete_run = group_manifest.get("latest_complete_run")
    if not isinstance(latest_complete_run, str) or not latest_complete_run:
        return False
    try:
        products = _products_from_group_manifest(group_manifest, group)
    except ValueError:
        return False
    summaries = group_manifest["product_manifests"]
    product_runs = []
    for product in products:
        summary = summaries.get(product)
        if not isinstance(summary, dict) or summary.get("status") != "complete":
            return False
        product_run = summary.get("latest_complete_run")
        if not isinstance(product_run, str) or not product_run:
            return False
        product_runs.append(product_run)
    return (
        not group in GROUPS_REQUIRING_MATCHING_RUNS
        or all(product_run == latest_complete_run for product_run in product_runs)
    )


def _local_group_matches(group_manifest: dict[str, Any], output_root: Path, group: str) -> bool:
    local_path = output_root / "groups" / group / "current" / "ready_for_processing.json"
    if not local_path.exists():
        return False
    local = _load_json(local_path)
    if local.get("status") != "complete":
        return False
    if local.get("latest_complete_run") != group_manifest.get("latest_complete_run"):
        return False
    remote_products = group_manifest.get("product_manifests") or {}
    local_products = local.get("product_manifests") or {}
    if not isinstance(remote_products, dict) or not isinstance(local_products, dict):
        return False
    for product, remote_summary in remote_products.items():
        if not isinstance(remote_summary, dict):
            return False
        local_summary = local_products.get(product)
        if not isinstance(local_summary, dict):
            return False
        for key in GROUP_PRODUCT_SUMMARY_KEYS:
            if remote_summary.get(key) != local_summary.get(key):
                return False
        remote_coverage = remote_summary.get("coverage_id")
        product_ready_path = output_root / product / "current" / "ready_for_processing.json"
        if not product_ready_path.exists():
            return False
        product_ready = _load_json(product_ready_path)
        if product_ready.get("coverage_id") != remote_coverage:
            return False
    return True


def _load_product_manifest_for_group(
    mirror_root: Path,
    group_manifest: dict[str, Any],
    product: str,
) -> dict[str, Any]:
    summary = (group_manifest.get("product_manifests") or {}).get(product)
    if not isinstance(summary, dict):
        raise ValueError(f"group manifest missing product summary: {product}")
    coverage_id = str(summary.get("coverage_id") or "")
    manifest_path = mirror_root / product / "coverages" / coverage_id / "latest.json"
    if not manifest_path.exists():
        manifest_path = mirror_root / product / "latest.json"
    manifest = _load_json(manifest_path)
    if manifest.get("model") != product:
        raise ValueError(f"product manifest model mismatch for {product}")
    if manifest.get("status") != "complete":
        raise ValueError(f"product manifest is not complete: {product}")
    if manifest.get("coverage_id") != summary.get("coverage_id"):
        raise ValueError(f"product coverage mismatch for {product}")
    if manifest.get("latest_complete_run") != summary.get("latest_complete_run"):
        raise ValueError(f"product run mismatch for {product}")
    for manifest_key, summary_key in (
        ("required_start_utc", "required_start_utc"),
        ("required_end_utc", "required_end_utc"),
        ("valid_time_count", "valid_time_count"),
        ("bytes", "bytes"),
        ("downloaded_bytes", "downloaded_bytes"),
    ):
        if summary.get(summary_key) is not None and manifest.get(manifest_key) != summary.get(summary_key):
            raise ValueError(f"product summary {summary_key} mismatch for {product}")
    return manifest


def _manifest_value_for_summary(manifest: dict[str, Any], key: str) -> Any:
    if key == "files":
        return len(manifest.get("files") or [])
    return manifest.get(key)


def _local_product_matches_summary(output_root: Path, product: str, summary: dict[str, Any]) -> bool:
    ready_path = output_root / product / "current" / "ready_for_processing.json"
    manifest_path = output_root / product / "current" / "latest.json"
    if not ready_path.exists() or not manifest_path.exists():
        return False
    ready = _load_json(ready_path)
    manifest = _load_json(manifest_path)
    if ready.get("coverage_id") != summary.get("coverage_id"):
        return False
    if manifest.get("model") != product:
        return False
    if manifest.get("status") != "complete":
        return False
    for key in GROUP_PRODUCT_SUMMARY_KEYS:
        if summary.get(key) is not None and _manifest_value_for_summary(manifest, key) != summary.get(key):
            return False
    return True


def _stage_product_manifest_from_mirror(
    mirror_root: Path,
    output_root: Path,
    product: str,
    manifest: dict[str, Any],
) -> dict[str, Any]:
    source_root = mirror_root / product

    def fetch_file(relative_path: PurePosixPath, destination: Path) -> None:
        source = source_root / Path(*relative_path.parts)
        _copy_file(source, destination)

    return _prepare_manifest_stage(
        manifest,
        output_root,
        source_label_key="source_mirror_root",
        source_label=str(mirror_root),
        fetch_file=fetch_file,
    )


def _write_group_ready(
    group: str,
    group_manifest: dict[str, Any],
    output_root: Path,
    mirror_root: Path,
) -> None:
    payload = json.loads(json.dumps(group_manifest))
    payload["source_mirror_root"] = str(mirror_root)
    payload["synced_at"] = _format_now_utc()
    payload["release_id"] = group_release_id(group_manifest)
    current_root = output_root / "groups" / group / "current"
    _atomic_write_json(current_root / "latest.json", payload)
    _atomic_write_json(current_root / "ready_for_processing.json", payload)


def _release_root(output_root: Path, group: str) -> Path:
    return output_root / "groups" / group / "releases"


def _release_path(output_root: Path, group: str, release_id: str) -> Path:
    prefix = f"{group}-"
    digest = release_id.removeprefix(prefix)
    if not digest or release_id == digest or any(char not in "0123456789abcdef" for char in digest):
        raise ValueError(f"unsafe group release id: {release_id}")
    return _release_root(output_root, group) / f"{release_id}.json"


def _write_group_release(
    output_root: Path,
    group: str,
    group_manifest: dict[str, Any],
    mirror_root: Path,
    *,
    now_utc: datetime,
) -> None:
    payload = json.loads(json.dumps(group_manifest))
    payload["release_id"] = group_release_id(group_manifest)
    payload["source_mirror_root"] = str(mirror_root)
    payload["synced_at"] = _format_utc(now_utc)
    _atomic_write_json(
        _release_path(output_root, group, payload["release_id"]),
        payload,
    )


def _archive_current_group_release(
    output_root: Path,
    group: str,
    *,
    now_utc: datetime,
) -> None:
    """Preserve the old atomic group before publishing its replacement."""
    current_path = output_root / "groups" / group / "current" / "ready_for_processing.json"
    if not current_path.exists():
        return
    payload = _load_json(current_path)
    if not _group_manifest_is_complete(payload, group):
        return
    payload["release_id"] = group_release_id(payload)
    payload.setdefault("synced_at", _format_utc(now_utc))
    _atomic_write_json(_release_path(output_root, group, payload["release_id"]), payload)


def _group_release_sort_key(payload: dict[str, Any]) -> tuple[str, str, str]:
    return (
        str(payload.get("latest_complete_run") or ""),
        str(payload.get("synced_at") or ""),
        str(payload.get("release_id") or ""),
    )


def _load_complete_group_releases(
    output_root: Path,
    group: str,
) -> list[tuple[Path, dict[str, Any]]]:
    releases: list[tuple[Path, dict[str, Any]]] = []
    root = _release_root(output_root, group)
    if not root.exists():
        return releases
    for path in root.glob("*.json"):
        payload = _load_json(path)
        if _group_manifest_is_complete(payload, group):
            releases.append((path, payload))
    releases.sort(key=lambda item: _group_release_sort_key(item[1]), reverse=True)
    return releases


def activate_group_release(
    output_root: Path,
    group: str,
    release: dict[str, Any],
    *,
    now_utc: datetime | None = None,
) -> dict[str, Any]:
    """Atomically make an already retained complete release current again."""
    if not _group_manifest_is_complete(release, group):
        raise ValueError(f"group release is not complete: {group}")
    now = (now_utc or datetime.now(timezone.utc)).astimezone(timezone.utc)
    products = _products_from_group_manifest(release, group)
    summaries = release["product_manifests"]
    manifests: dict[str, dict[str, Any]] = {}

    for product in products:
        summary = summaries[product]
        coverage_id = str(summary.get("coverage_id") or "")
        manifest_path = output_root / product / "coverages" / coverage_id / "latest.json"
        if not manifest_path.exists():
            raise ValueError(f"retained coverage manifest is missing: {product}/{coverage_id}")
        manifest = _load_json(manifest_path)
        if manifest.get("model") != product or manifest.get("status") != "complete":
            raise ValueError(f"retained coverage manifest is invalid: {product}/{coverage_id}")
        for key in GROUP_PRODUCT_SUMMARY_KEYS:
            if summary.get(key) is not None and _manifest_value_for_summary(manifest, key) != summary.get(key):
                raise ValueError(f"retained coverage summary mismatch: {product}/{coverage_id}/{key}")
        for file_record in manifest.get("files") or []:
            relative_path = _safe_relative_path(str(file_record.get("path") or ""))
            path = output_root / product / Path(*relative_path.parts)
            if not path.is_file() or path.stat().st_size != int(file_record.get("bytes") or -1):
                raise ValueError(f"retained coverage payload is missing: {path}")
        manifests[product] = manifest

    _archive_current_group_release(output_root, group, now_utc=now)
    for product, manifest in manifests.items():
        coverage_id = str(manifest["coverage_id"])
        current_root = output_root / product / "current"
        _atomic_write_json(current_root / "latest.json", manifest)
        _atomic_write_json(
            current_root / "ready_for_processing.json",
            {
                "model": product,
                "coverage_id": coverage_id,
                "status": "complete",
                "latest_complete_run": manifest.get("latest_complete_run"),
                "files": len(manifest.get("files") or []),
                "bytes": manifest.get("bytes"),
                "coverage_dir": str(output_root / product / "coverages" / coverage_id),
                "activated_at": _format_utc(now),
            },
        )

    payload = json.loads(json.dumps(release))
    payload["release_id"] = group_release_id(payload)
    payload["activated_at"] = _format_utc(now)
    current_root = output_root / "groups" / group / "current"
    _atomic_write_json(current_root / "latest.json", payload)
    _atomic_write_json(current_root / "ready_for_processing.json", payload)
    return {
        "status": "activated",
        "group": group,
        "latest_complete_run": payload.get("latest_complete_run"),
        "release_id": payload["release_id"],
    }


def _local_coverage_matches_manifest(
    output_root: Path,
    product: str,
    manifest: dict[str, Any],
) -> bool:
    coverage_id = str(manifest.get("coverage_id") or "")
    if not coverage_id or manifest.get("model") != product or manifest.get("status") != "complete":
        return False
    local_manifest_path = output_root / product / "coverages" / coverage_id / "latest.json"
    if not local_manifest_path.exists():
        return False
    local_manifest = _load_json(local_manifest_path)
    for key in (
        "model",
        "status",
        "coverage_id",
        "config_fingerprint",
        "latest_complete_run",
        "required_start_utc",
        "public_start_utc",
        "required_end_utc",
        "valid_time_count",
        "bytes",
        "downloaded_bytes",
    ):
        if local_manifest.get(key) != manifest.get(key):
            return False
    files = manifest.get("files")
    if not isinstance(files, list) or not files:
        return False
    for file_record in files:
        if not isinstance(file_record, dict):
            return False
        relative_path = _safe_relative_path(str(file_record.get("path") or ""))
        path = output_root / product / Path(*relative_path.parts)
        if not path.is_file() or path.stat().st_size != int(file_record.get("bytes") or -1):
            return False
        expected_sha = str(file_record.get("sha256") or "")
        if expected_sha and sha256_file(path) != expected_sha:
            return False
    return True


def _promote_coverage_stage_without_current(stage: dict[str, Any]) -> dict[str, Any]:
    if stage.get("status") != "staged":
        return stage
    coverage_root = stage["coverage_root"]
    incoming_root = stage["incoming_root"]
    if coverage_root.exists():
        shutil.rmtree(coverage_root)
    coverage_root.parent.mkdir(parents=True, exist_ok=True)
    os.replace(incoming_root, coverage_root)
    return {
        "status": "synced",
        "model": stage["model"],
        "coverage_id": stage["coverage_id"],
        "files": stage["files"],
    }


def retain_group_release_from_mirror(
    group: str,
    mirror_root: Path,
    output_root: Path,
    *,
    now_utc: datetime | None = None,
) -> dict[str, Any]:
    """Retain one complete release without changing the current group."""
    if group not in OPENMETEO_GROUP_PRODUCTS:
        raise ValueError(f"unknown Open-Meteo group: {group}")
    mirror_root = mirror_root.resolve()
    output_root = output_root.resolve()
    now = (now_utc or datetime.now(timezone.utc)).astimezone(timezone.utc)
    group_manifest = _load_json(mirror_root / "groups" / group / "latest.json")
    if not _group_manifest_is_complete(group_manifest, group):
        raise ValueError(f"source group release is not complete: {group}")

    manifests_to_stage: list[tuple[str, dict[str, Any]]] = []
    for product in _products_from_group_manifest(group_manifest, group):
        manifest = _load_product_manifest_for_group(mirror_root, group_manifest, product)
        if not _local_coverage_matches_manifest(output_root, product, manifest):
            manifests_to_stage.append((product, manifest))

    _ensure_stage_capacity(output_root, [manifest for _product, manifest in manifests_to_stage])
    staged: list[dict[str, Any]] = []
    try:
        for product, manifest in manifests_to_stage:
            staged.append(
                _stage_product_manifest_from_mirror(
                    mirror_root,
                    output_root,
                    product,
                    manifest,
                )
            )
    except Exception:
        for stage in staged:
            _remove_stage(stage)
        raise

    promoted = [_promote_coverage_stage_without_current(stage) for stage in staged]
    _write_group_release(output_root, group, group_manifest, mirror_root, now_utc=now)
    return {
        "status": "retained" if promoted else "skipped",
        "reason": None if promoted else "release payloads already retained",
        "group": group,
        "latest_complete_run": group_manifest.get("latest_complete_run"),
        "release_id": group_release_id(group_manifest),
        "synced_coverages": len(promoted),
    }


def sync_retained_group_releases_from_mirror(
    group: str,
    mirror_root: Path,
    output_root: Path,
    *,
    retain_complete_releases: int = DEFAULT_COMPLETE_RELEASE_RETENTION,
    now_utc: datetime | None = None,
) -> dict[str, Any]:
    if group not in OPENMETEO_GROUP_PRODUCTS:
        raise ValueError(f"unknown Open-Meteo group: {group}")
    if retain_complete_releases < DEFAULT_COMPLETE_RELEASE_RETENTION:
        raise ValueError(
            f"retain_complete_releases must be at least {DEFAULT_COMPLETE_RELEASE_RETENTION}"
        )
    mirror_root = mirror_root.resolve()
    output_root = output_root.resolve()
    now = (now_utc or datetime.now(timezone.utc)).astimezone(timezone.utc)

    source_releases = _load_complete_group_releases(mirror_root, group)
    desired: list[tuple[Path, dict[str, Any]]] = []
    desired_runs: set[str] = set()
    for release in source_releases:
        run = str(release[1].get("latest_complete_run") or "")
        if not run or run in desired_runs:
            continue
        desired_runs.add(run)
        desired.append(release)
        if len(desired) == retain_complete_releases:
            break
    if len(desired) < retain_complete_releases:
        return {
            "status": "skipped",
            "reason": f"source has only {len(desired)} complete distinct releases",
            "group": group,
        }

    release_manifests: list[tuple[dict[str, Any], dict[str, dict[str, Any]]]] = []
    manifests_to_stage: list[tuple[str, dict[str, Any]]] = []
    seen_coverages: set[tuple[str, str]] = set()
    for _path, release in reversed(desired):
        products = _products_from_group_manifest(release, group)
        manifests: dict[str, dict[str, Any]] = {}
        for product in products:
            manifest = _load_product_manifest_for_group(mirror_root, release, product)
            manifests[product] = manifest
            key = (product, str(manifest.get("coverage_id") or ""))
            if key in seen_coverages:
                continue
            seen_coverages.add(key)
            if not _local_coverage_matches_manifest(output_root, product, manifest):
                manifests_to_stage.append((product, manifest))
        release_manifests.append((release, manifests))

    _ensure_stage_capacity(output_root, [manifest for _product, manifest in manifests_to_stage])
    staged: list[dict[str, Any]] = []
    try:
        for product, manifest in manifests_to_stage:
            staged.append(
                _stage_product_manifest_from_mirror(
                    mirror_root,
                    output_root,
                    product,
                    manifest,
                )
            )
    except Exception:
        for stage in staged:
            _remove_stage(stage)
        raise

    promoted = [_promote_coverage_stage_without_current(stage) for stage in staged]
    for release, _manifests in release_manifests:
        _write_group_release(output_root, group, release, mirror_root, now_utc=now)

    newest_release = desired[0][1]
    activation = activate_group_release(output_root, group, newest_release, now_utc=now)
    pruned = prune_expired_group_releases(
        output_root,
        group,
        now_utc=now,
        retain_complete_releases=retain_complete_releases,
    )
    return {
        "status": "synced" if promoted else "skipped",
        "reason": None
        if promoted
        else f"{retain_complete_releases} most recent complete runs already retained",
        "group": group,
        "latest_complete_run": newest_release.get("latest_complete_run"),
        "retained_complete_runs": [
            str(release.get("latest_complete_run")) for _path, release in desired
        ],
        "synced_coverages": len(promoted),
        "activation": activation,
        "pruned_raw_paths": pruned,
    }


def _referenced_group_coverages(
    group: str,
    releases: list[dict[str, Any]],
) -> dict[str, set[str]]:
    referenced = {product: set() for product in OPENMETEO_GROUP_PRODUCTS[group]}
    for release in releases:
        manifests = release.get("product_manifests")
        if not isinstance(manifests, dict):
            continue
        for product in OPENMETEO_GROUP_PRODUCTS[group]:
            summary = manifests.get(product)
            if not isinstance(summary, dict):
                continue
            coverage_id = str(summary.get("coverage_id") or "")
            if coverage_id:
                referenced[product].add(coverage_id)
    return referenced


def prune_expired_group_releases(
    output_root: Path,
    group: str,
    *,
    now_utc: datetime | None = None,
    retain_complete_releases: int = DEFAULT_COMPLETE_RELEASE_RETENTION,
    preserve_current: bool = False,
) -> list[str]:
    if group not in OPENMETEO_GROUP_PRODUCTS:
        raise ValueError(f"unknown Open-Meteo group: {group}")
    if retain_complete_releases < DEFAULT_COMPLETE_RELEASE_RETENTION:
        raise ValueError(
            f"retain_complete_releases must be at least {DEFAULT_COMPLETE_RELEASE_RETENTION}"
        )
    del now_utc

    # Legacy cleanup entries are intentionally discarded without deleting their
    # coverages. Daily AQI uses retained complete releases as its history window.
    legacy_cleanup_path = output_root / "groups" / group / "cleanup.json"
    try:
        legacy_cleanup_path.unlink()
    except FileNotFoundError:
        pass

    releases = _load_complete_group_releases(output_root, group)
    current_path = output_root / "groups" / group / "current" / "ready_for_processing.json"
    current_release_id: str | None = None
    if current_path.exists():
        current = _load_json(current_path)
        current_release_id = group_release_id(current)
        if all(payload.get("release_id") != current_release_id for _, payload in releases):
            payload = json.loads(json.dumps(current))
            payload["release_id"] = current_release_id
            _atomic_write_json(_release_path(output_root, group, current_release_id), payload)
            releases = _load_complete_group_releases(output_root, group)

    retained_releases: list[tuple[Path, dict[str, Any]]] = []
    expired_releases: list[tuple[Path, dict[str, Any]]] = []
    retained_runs: set[str] = set()
    for release in releases:
        payload = release[1]
        run = str(payload.get("latest_complete_run") or "")
        release_id = str(payload.get("release_id") or group_release_id(payload))
        if preserve_current and release_id == current_release_id:
            retained_releases.append(release)
            retained_runs.add(run)
            continue
        if run in retained_runs or len(retained_releases) >= retain_complete_releases:
            expired_releases.append(release)
            continue
        retained_runs.add(run)
        retained_releases.append(release)
    referenced = _referenced_group_coverages(
        group,
        [payload for _, payload in retained_releases],
    )
    removed: list[str] = []
    for product in OPENMETEO_GROUP_PRODUCTS[group]:
        coverages_root = output_root / product / "coverages"
        if not coverages_root.exists():
            continue
        resolved_root = coverages_root.resolve(strict=False)
        for coverage_path in coverages_root.iterdir():
            if not coverage_path.is_dir() or coverage_path.name in referenced[product]:
                continue
            resolved_path = coverage_path.resolve(strict=False)
            try:
                resolved_path.relative_to(resolved_root)
            except ValueError as exc:
                raise ValueError(
                    f"refusing to prune path outside coverages root: {coverage_path}"
                ) from exc
            shutil.rmtree(coverage_path)
            removed.append(str(coverage_path))

    for release_path, _ in expired_releases:
        release_path.unlink()
    return removed


def _prune_product_coverages(root: Path, product: str, keep_coverage_id: str) -> list[str]:
    product_root = root / product
    coverages_root = product_root / "coverages"
    pruned: list[str] = []
    if not coverages_root.exists():
        return pruned
    resolved_coverages = coverages_root.resolve(strict=False)
    for child in coverages_root.iterdir():
        if not child.is_dir() or child.name == keep_coverage_id:
            continue
        resolved_child = child.resolve(strict=False)
        try:
            resolved_child.relative_to(resolved_coverages)
        except ValueError as exc:
            raise ValueError(f"refusing to prune path outside coverages root: {child}") from exc
        shutil.rmtree(child)
        pruned.append(str(child))
    incoming_root = product_root / ".incoming"
    if incoming_root.exists():
        shutil.rmtree(incoming_root)
        pruned.append(str(incoming_root))
    return pruned


def _prune_group_coverages(
    group_manifest: dict[str, Any],
    output_root: Path,
    mirror_root: Path,
    products: tuple[str, ...],
) -> dict[str, list[str]]:
    product_manifests = group_manifest.get("product_manifests") or {}
    pruned = {"raw": [], "mirror": []}
    for product in products:
        summary = product_manifests.get(product)
        if not isinstance(summary, dict):
            continue
        keep_coverage_id = str(summary.get("coverage_id") or "")
        if not keep_coverage_id:
            continue
        pruned["raw"].extend(_prune_product_coverages(output_root, product, keep_coverage_id))
        pruned["mirror"].extend(_prune_product_coverages(mirror_root, product, keep_coverage_id))
    return pruned


def sync_group_from_mirror(
    group: str,
    mirror_root: Path,
    output_root: Path,
    *,
    cleanup_grace_seconds: int = 300,
    retain_complete_releases: int = DEFAULT_COMPLETE_RELEASE_RETENTION,
    now_utc: datetime | None = None,
) -> dict[str, Any]:
    if group not in OPENMETEO_GROUP_PRODUCTS:
        raise ValueError(f"unknown Open-Meteo group: {group}")
    if cleanup_grace_seconds < 0:
        raise ValueError("cleanup_grace_seconds must not be negative")
    if retain_complete_releases < DEFAULT_COMPLETE_RELEASE_RETENTION:
        raise ValueError(
            f"retain_complete_releases must be at least {DEFAULT_COMPLETE_RELEASE_RETENTION}"
        )
    now = (now_utc or datetime.now(timezone.utc)).astimezone(timezone.utc)
    mirror_root = mirror_root.resolve()
    group_manifest_path = mirror_root / "groups" / group / "latest.json"
    group_manifest = _load_json(group_manifest_path)
    if not _group_manifest_is_complete(group_manifest, group):
        return {
            "status": "skipped",
            "reason": f"group manifest status is {group_manifest.get('status')}",
            "group": group,
        }
    if _local_group_matches(group_manifest, output_root, group):
        _write_group_release(output_root, group, group_manifest, mirror_root, now_utc=now)
        pruned = prune_expired_group_releases(
            output_root,
            group,
            now_utc=now,
            retain_complete_releases=retain_complete_releases,
        )
        return {
            "status": "skipped",
            "reason": "local group already current",
            "group": group,
            "latest_complete_run": group_manifest.get("latest_complete_run"),
            "pruned_raw_paths": pruned,
            "pruned_mirror_paths": [],
        }

    products = _products_from_group_manifest(group_manifest, group)

    group_summaries = group_manifest.get("product_manifests") or {}
    manifests_to_stage: list[dict[str, Any]] = []
    for product in products:
        summary = group_summaries[product]
        if not _local_product_matches_summary(output_root, product, summary):
            manifests_to_stage.append(
                _load_product_manifest_for_group(mirror_root, group_manifest, product)
            )
    _ensure_stage_capacity(output_root, manifests_to_stage)

    staged = []
    promoted = []
    manifests_by_product = {str(manifest["model"]): manifest for manifest in manifests_to_stage}
    try:
        for product in products:
            summary = group_summaries[product]
            manifest = manifests_by_product.get(product)
            if manifest is None:
                promoted.append(
                    {
                        "status": "skipped",
                        "model": product,
                        "coverage_id": summary.get("coverage_id"),
                        "files": summary.get("files"),
                    }
                )
                continue
            staged.append(_stage_product_manifest_from_mirror(mirror_root, output_root, product, manifest))
    except Exception:
        for stage in staged:
            _remove_stage(stage)
        raise
    promoted.extend(_promote_manifest_stage(stage) for stage in staged)
    _archive_current_group_release(output_root, group, now_utc=now)
    _write_group_ready(group, group_manifest, output_root, mirror_root)
    _write_group_release(output_root, group, group_manifest, mirror_root, now_utc=now)
    pruned = prune_expired_group_releases(
        output_root,
        group,
        now_utc=now,
        retain_complete_releases=retain_complete_releases,
    )
    return {
        "status": "synced",
        "group": group,
        "latest_complete_run": group_manifest.get("latest_complete_run"),
        "products": len(promoted),
        "files": sum(int(item.get("files") or 0) for item in promoted),
        "bytes": group_manifest.get("bytes"),
        "pruned_raw_paths": pruned,
        "pruned_mirror_paths": [],
    }
