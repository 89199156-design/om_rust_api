#!/usr/bin/env python3
"""Reorder frozen NOAA GEFS float frames into Open-Meteo grid order."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path

import numpy as np


NY = 361
NX = 720


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def openmeteo_index(latitude: float, longitude: float) -> tuple[int, int]:
    y = int(round((latitude + 90.0) * 2.0))
    normalized = ((longitude + 180.0) % 360.0) - 180.0
    x = int(round((normalized + 180.0) * 2.0)) % NX
    if not (0 <= y < NY):
        raise ValueError(f"diagnostic latitude is outside GEFS grid: {latitude}")
    return y, x


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not args.source.is_absolute() or not args.output.is_absolute():
        raise ValueError("source and output must be absolute paths")
    if args.source == Path("/") or args.output == Path("/"):
        raise ValueError("refusing a filesystem root")
    metadata = json.loads((args.source / "metadata.json").read_text(encoding="utf-8"))
    if not isinstance(metadata, dict) or not metadata:
        raise ValueError("source metadata must be a non-empty JSON object")
    if args.output.exists() and any(args.output.iterdir()):
        raise ValueError(f"refusing non-empty output directory: {args.output}")
    args.output.mkdir(parents=True, exist_ok=True)

    result = {}
    for hour_text, source_record in sorted(metadata.items(), key=lambda item: int(item[0])):
        hour = int(hour_text)
        source_path = args.source / f"f{hour}.f32le"
        values = np.fromfile(source_path, dtype="<f4")
        if values.size != NY * NX:
            raise ValueError(f"unexpected GEFS frame size: {source_path}")
        # NOAA: north-to-south, 0..360°. Open-Meteo: south-to-north,
        # -180..180°. A half-width roll maps source lon 0 to destination lon 0.
        ordered = np.roll(values.reshape(NY, NX)[::-1, :], NX // 2, axis=1)
        output_path = args.output / source_path.name
        ordered.astype("<f4", copy=False).tofile(output_path)
        record = dict(source_record)
        record["raw_path"] = str(output_path)
        record["raw_bytes"] = output_path.stat().st_size
        record["raw_sha256"] = sha256(output_path)
        record["source_raw_sha256"] = source_record["raw_sha256"]
        record["grid_order"] = "openmeteo_south_to_north_-180_to_180"
        y, x = openmeteo_index(
            float(source_record["latitude"]),
            float(source_record["longitude"]),
        )
        actual = int(round(float(ordered[y, x])))
        expected = int(source_record["precipitation_probability"])
        if actual != expected:
            raise ValueError(
                f"normalized f{hour} diagnostic mismatch: expected {expected}, got {actual}"
            )
        result[hour_text] = record
    metadata_path = args.output / "metadata.json"
    metadata_path.write_text(
        json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(
        json.dumps(
            {
                "status": "complete",
                "source": str(args.source),
                "output": str(args.output),
                "metadata_sha256": sha256(metadata_path),
                "frames": {
                    hour: record["raw_sha256"] for hour, record in result.items()
                },
            },
            ensure_ascii=False,
            indent=2,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
