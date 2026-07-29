from __future__ import annotations

from dataclasses import dataclass
import hashlib
import os
from pathlib import Path, PurePosixPath
import shutil
from typing import BinaryIO, Callable
from urllib.request import urlopen


@dataclass(frozen=True)
class StaticAssetSpec:
    model: str
    relative_path: PurePosixPath
    bucket_key: PurePosixPath
    bytes: int
    sha256: str


ECMWF_IFS025_HSURF = StaticAssetSpec(
    model="ecmwf_ifs025",
    relative_path=PurePosixPath("static/ecmwf_ifs025/HSURF.om"),
    bucket_key=PurePosixPath("data/ecmwf_ifs025/static/HSURF.om"),
    bytes=433_648,
    sha256="935d56ba000b438b61504fbc271bfaa8f70db2acb541d58d5b466a24d294a9fb",
)

NCEP_GEFS025_HSURF = StaticAssetSpec(
    model="ncep_gefs025",
    relative_path=PurePosixPath("static/ncep_gefs025/HSURF.om"),
    bucket_key=PurePosixPath("data/ncep_gefs025/static/HSURF.om"),
    bytes=408_440,
    sha256="09f589bd34aad27f538a5c0cb01c5dbd4114acc27d6b6f24524fd9451ce56f62",
)

NCEP_GEFS05_HSURF = StaticAssetSpec(
    model="ncep_gefs05",
    relative_path=PurePosixPath("static/ncep_gefs05/HSURF.om"),
    bucket_key=PurePosixPath("data/ncep_gefs05/static/HSURF.om"),
    bytes=118_088,
    sha256="aeb5313b8f402b9098a268b476b480ed065593eb718c358ad01fada0f0f6ddcb",
)

ECMWF_IFS025_ENSEMBLE_HSURF = StaticAssetSpec(
    model="ecmwf_ifs025_ensemble",
    relative_path=PurePosixPath("static/ecmwf_ifs025_ensemble/HSURF.om"),
    bucket_key=PurePosixPath("data/ecmwf_ifs025_ensemble/static/HSURF.om"),
    bytes=433_744,
    sha256="c137997ccae161da2f79bd4ae3e00e03aa0a94cf351d5fd69b88a03191c1e2be",
)

OPENMETEO_STATIC_ASSETS = {
    ECMWF_IFS025_HSURF.model: ECMWF_IFS025_HSURF,
    NCEP_GEFS025_HSURF.model: NCEP_GEFS025_HSURF,
    NCEP_GEFS05_HSURF.model: NCEP_GEFS05_HSURF,
    ECMWF_IFS025_ENSEMBLE_HSURF.model: ECMWF_IFS025_ENSEMBLE_HSURF,
}


def static_asset_path(data_root: Path, spec: StaticAssetSpec) -> Path:
    return data_root / Path(*spec.relative_path.parts)


def static_asset_url(bucket_url: str, spec: StaticAssetSpec) -> str:
    return f"{bucket_url.rstrip('/')}/{spec.bucket_key.as_posix()}"


def _sha256_stream(handle: BinaryIO) -> tuple[int, str]:
    digest = hashlib.sha256()
    size = 0
    while True:
        block = handle.read(1024 * 1024)
        if not block:
            break
        size += len(block)
        digest.update(block)
    return size, digest.hexdigest()


def verify_static_asset(path: Path, spec: StaticAssetSpec) -> bool:
    if not path.is_file() or path.stat().st_size != spec.bytes:
        return False
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            block = handle.read(1024 * 1024)
            if not block:
                break
            digest.update(block)
    return digest.hexdigest() == spec.sha256


def static_asset_manifest_record(
    spec: StaticAssetSpec,
    *,
    bucket_url: str,
) -> dict[str, object]:
    return {
        "model": spec.model,
        "path": spec.relative_path.as_posix(),
        "source_url": static_asset_url(bucket_url, spec),
        "bytes": spec.bytes,
        "sha256": spec.sha256,
        "storage": "external_env",
        "environment": "OM_MODEL_STATIC_ROOT",
    }


def ensure_static_asset(
    data_root: Path,
    spec: StaticAssetSpec,
    *,
    bucket_url: str,
    timeout: int = 60,
    opener: Callable[..., object] = urlopen,
) -> dict[str, object]:
    """Download an immutable model asset and atomically promote it after verification."""
    target = static_asset_path(data_root, spec)
    record = static_asset_manifest_record(spec, bucket_url=bucket_url)
    if verify_static_asset(target, spec):
        return {**record, "status": "reused"}

    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(target.name + ".download.tmp")
    temporary.unlink(missing_ok=True)
    try:
        with opener(static_asset_url(bucket_url, spec), timeout=timeout) as response:
            with temporary.open("wb") as output:
                shutil.copyfileobj(response, output, length=1024 * 1024)
                output.flush()
                os.fsync(output.fileno())
        if not verify_static_asset(temporary, spec):
            with temporary.open("rb") as handle:
                actual_size, actual_sha256 = _sha256_stream(handle)
            raise ValueError(
                f"static asset verification failed for {spec.model}: "
                f"expected {spec.bytes}/{spec.sha256}, "
                f"got {actual_size}/{actual_sha256}"
            )
        os.replace(temporary, target)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise
    return {**record, "status": "downloaded"}


def install_static_asset(
    source_root: Path,
    destination_root: Path,
    spec: StaticAssetSpec,
    *,
    bucket_url: str,
) -> dict[str, object]:
    """Atomically copy an already verified asset into another data root."""
    source = static_asset_path(source_root, spec)
    if not verify_static_asset(source, spec):
        raise ValueError(f"verified static asset source is missing or corrupt: {source}")
    target = static_asset_path(destination_root, spec)
    record = static_asset_manifest_record(spec, bucket_url=bucket_url)
    if verify_static_asset(target, spec):
        return {**record, "status": "reused"}

    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(target.name + ".install.tmp")
    temporary.unlink(missing_ok=True)
    try:
        shutil.copyfile(source, temporary)
        if not verify_static_asset(temporary, spec):
            raise ValueError(f"copied static asset failed verification: {temporary}")
        os.replace(temporary, target)
    except Exception:
        temporary.unlink(missing_ok=True)
        raise
    return {**record, "status": "installed"}
