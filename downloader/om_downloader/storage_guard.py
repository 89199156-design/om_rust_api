from __future__ import annotations

from collections.abc import Mapping
import errno
import os
from pathlib import Path
import shutil
from typing import Any


STRICT_DATA_ROOT_ENV = "OM_STRICT_DATA_ROOT"
MINIMUM_FREE_BYTES_ENV = "OM_DATA_MIN_FREE_BYTES"
DEFAULT_MINIMUM_FREE_BYTES = 10 * 1024 * 1024 * 1024
MINIMUM_ALLOWED_RESERVE_BYTES = 512 * 1024 * 1024


def _existing_ancestor(path: Path) -> Path:
    candidate = path
    while not candidate.exists():
        parent = candidate.parent
        if parent == candidate:
            raise ValueError(f"storage path has no existing ancestor: {path}")
        candidate = parent
    return candidate


def _device_id(path: Path) -> int:
    return path.stat().st_dev


def configured_minimum_free_bytes(
    environment: Mapping[str, str] | None = None,
) -> int:
    environment = os.environ if environment is None else environment
    raw = environment.get(MINIMUM_FREE_BYTES_ENV, "").strip()
    if not raw:
        return DEFAULT_MINIMUM_FREE_BYTES
    try:
        value = int(raw)
    except ValueError as exc:
        raise ValueError(f"{MINIMUM_FREE_BYTES_ENV} must be an integer") from exc
    if value < MINIMUM_ALLOWED_RESERVE_BYTES:
        raise ValueError(
            f"{MINIMUM_FREE_BYTES_ENV} must be at least "
            f"{MINIMUM_ALLOWED_RESERVE_BYTES} bytes"
        )
    return value


def require_strict_data_path(
    path: Path,
    *,
    required_root: Path,
    minimum_free_bytes: int,
    additional_bytes: int = 0,
) -> dict[str, Any]:
    if additional_bytes < 0:
        raise ValueError("additional_bytes must not be negative")
    if not required_root.is_absolute() or not path.is_absolute():
        raise ValueError("strict data root and guarded paths must be absolute")
    if not required_root.is_dir() or not os.path.ismount(required_root):
        raise ValueError(f"strict data root is not a mounted directory: {required_root}")

    resolved_root = required_root.resolve(strict=True)
    resolved_path = path.resolve(strict=False)
    try:
        resolved_path.relative_to(resolved_root)
    except ValueError as exc:
        raise ValueError(
            f"storage path escapes strict data root {resolved_root}: {resolved_path}"
        ) from exc

    existing = _existing_ancestor(path).resolve(strict=True)
    root_device = _device_id(resolved_root)
    if _device_id(existing) != root_device:
        raise ValueError(
            f"storage path is not on strict data device {resolved_root}: {existing}"
        )
    filesystem_root = Path(resolved_root.anchor)
    if _device_id(filesystem_root) == root_device:
        raise ValueError(
            f"strict data root shares the system filesystem device: {resolved_root}"
        )

    available = shutil.disk_usage(resolved_root).free
    required_available = minimum_free_bytes + additional_bytes
    if available < required_available:
        raise OSError(
            errno.ENOSPC,
            "insufficient strict data-disk capacity: "
            f"path={resolved_path} available={available} "
            f"additional={additional_bytes} reserve={minimum_free_bytes}",
        )
    return {
        "path": str(resolved_path),
        "data_root": str(resolved_root),
        "device": root_device,
        "available_bytes": available,
        "additional_bytes": additional_bytes,
        "minimum_free_bytes": minimum_free_bytes,
    }


def enforce_environment_storage_guard(
    path: Path,
    *,
    additional_bytes: int = 0,
    environment: Mapping[str, str] | None = None,
) -> dict[str, Any] | None:
    environment = os.environ if environment is None else environment
    root_text = environment.get(STRICT_DATA_ROOT_ENV, "").strip()
    if not root_text:
        return None
    return require_strict_data_path(
        path,
        required_root=Path(root_text),
        minimum_free_bytes=configured_minimum_free_bytes(environment),
        additional_bytes=additional_bytes,
    )
