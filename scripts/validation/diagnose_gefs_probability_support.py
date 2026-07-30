#!/usr/bin/env python3
"""Reconstruct delayed GEFS precipitation-probability support frames.

This diagnostic reads only the APCP GRIB message from each NOAA GEFS member,
then applies Open-Meteo's documented probability threshold of 0.1 mm/hour.
It never calls an Open-Meteo API endpoint.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import re
import urllib.request

import eccodes
import numpy as np


NOAA_ROOT = "https://noaa-gefs-pds.s3.amazonaws.com"
APCP_PATTERN = re.compile(
    r"^(?P<record>\d+):(?P<offset>\d+):.*:APCP:surface:"
    r"(?P<start>\d+)-(?P<end>\d+) hour acc fcst:"
)


def member_name(member: int, run_hour: int, forecast_hour: int) -> str:
    prefix = "gec00" if member == 0 else f"gep{member:02d}"
    return (
        f"{prefix}.t{run_hour:02d}z.pgrb2a.0p50."
        f"f{forecast_hour:03d}"
    )


def object_url(run: str, run_hour: int, member: int, forecast_hour: int) -> str:
    name = member_name(member, run_hour, forecast_hour)
    return (
        f"{NOAA_ROOT}/gefs.{run}/{run_hour:02d}/atmos/pgrb2ap5/{name}"
    )


def read_url(url: str, *, byte_range: tuple[int, int] | None = None) -> bytes:
    headers = {}
    if byte_range is not None:
        headers["Range"] = f"bytes={byte_range[0]}-{byte_range[1]}"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read()


def apcp_range(index_text: str) -> tuple[int, int, int]:
    lines = index_text.splitlines()
    for index, line in enumerate(lines):
        match = APCP_PATTERN.match(line)
        if match is None:
            continue
        if index + 1 >= len(lines):
            raise ValueError("APCP is the final GRIB index record")
        next_offset = int(lines[index + 1].split(":", 2)[1])
        start = int(match.group("offset"))
        duration = int(match.group("end")) - int(match.group("start"))
        if next_offset <= start or duration <= 0:
            raise ValueError("invalid APCP byte range or accumulation duration")
        return start, next_offset - 1, duration
    raise ValueError("APCP record is missing from GEFS index")


def read_member(
    run: str,
    run_hour: int,
    member: int,
    forecast_hour: int,
) -> tuple[np.ndarray, np.ndarray, np.ndarray, int]:
    url = object_url(run, run_hour, member, forecast_hour)
    index_text = read_url(url + ".idx").decode("utf-8")
    start, end, duration = apcp_range(index_text)
    message = read_url(url, byte_range=(start, end))
    grib = eccodes.codes_new_from_message(message)
    if grib is None:
        raise ValueError(f"failed to decode APCP GRIB: {url}")
    try:
        values = np.asarray(eccodes.codes_get_values(grib), dtype=np.float32)
        latitudes = np.asarray(
            eccodes.codes_get_array(grib, "latitudes"), dtype=np.float32
        )
        longitudes = np.asarray(
            eccodes.codes_get_array(grib, "longitudes"), dtype=np.float32
        )
    finally:
        eccodes.codes_release(grib)
    return values, latitudes, longitudes, duration


def reconstruct(
    run: str,
    run_hour: int,
    forecast_hour: int,
    workers: int,
) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    with ThreadPoolExecutor(max_workers=workers) as executor:
        frames = list(
            executor.map(
                lambda member: read_member(
                    run, run_hour, member, forecast_hour
                ),
                range(31),
            )
        )
    values, latitudes, longitudes, duration = frames[0]
    probability_count = np.zeros(values.shape, dtype=np.uint8)
    threshold = np.float32(0.1 * duration)
    for member_values, member_latitudes, member_longitudes, member_duration in frames:
        if (
            member_duration != duration
            or member_values.shape != values.shape
            or not np.array_equal(member_latitudes, latitudes)
            or not np.array_equal(member_longitudes, longitudes)
        ):
            raise ValueError("GEFS member grids or accumulation windows differ")
        probability_count += member_values >= threshold
    probability = probability_count.astype(np.float32) * np.float32(100.0 / 31.0)
    return to_openmeteo_gefs05_order(probability, latitudes, longitudes)


def to_openmeteo_gefs05_order(
    probability: np.ndarray,
    latitudes: np.ndarray,
    longitudes: np.ndarray,
) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Convert NOAA GRIB scan order to Open-Meteo's GEFS 0.5° grid order.

    NOAA's pgrb2a messages scan rows north-to-south and columns 0..360°.
    Open-Meteo's ncep_gefs05 array is south-to-north and -180..180°.
    Sort from the decoded coordinates instead of relying on an undocumented
    fixed flip/roll so a future upstream scan-order change fails visibly.
    """
    expected = 361 * 720
    if (
        probability.size != expected
        or latitudes.size != expected
        or longitudes.size != expected
    ):
        raise ValueError("GEFS 0.5° frame does not have a 361x720 grid")
    values = probability.reshape(361, 720)
    latitude_grid = latitudes.reshape(361, 720)
    longitude_grid = longitudes.reshape(361, 720)
    if not np.allclose(latitude_grid, latitude_grid[:, :1], atol=1e-6):
        raise ValueError("GEFS latitude is not constant across each row")
    normalized_longitude = (
        (longitude_grid[0].astype(np.float64) + 180.0) % 360.0
    ) - 180.0
    if not np.allclose(
        ((longitude_grid.astype(np.float64) + 180.0) % 360.0) - 180.0,
        normalized_longitude[None, :],
        atol=1e-6,
    ):
        raise ValueError("GEFS longitude columns differ between rows")
    latitude_order = np.argsort(latitude_grid[:, 0], kind="stable")
    longitude_order = np.argsort(normalized_longitude, kind="stable")
    ordered_values = values[np.ix_(latitude_order, longitude_order)].reshape(-1)
    ordered_latitudes = latitude_grid[
        np.ix_(latitude_order, longitude_order)
    ].reshape(-1)
    ordered_longitudes = np.broadcast_to(
        normalized_longitude[longitude_order][None, :],
        (361, 720),
    ).reshape(-1)
    if (
        not np.isclose(ordered_latitudes[0], -90.0)
        or not np.isclose(ordered_latitudes[-1], 90.0)
        or not np.isclose(ordered_longitudes[0], -180.0)
        or not np.isclose(ordered_longitudes[-1], 179.5)
    ):
        raise ValueError("normalized GEFS grid bounds do not match Open-Meteo")
    return ordered_values, ordered_latitudes, ordered_longitudes


def nearest_index(
    latitudes: np.ndarray,
    longitudes: np.ndarray,
    latitude: float,
    longitude: float,
) -> int:
    longitude = longitude % 360.0
    distance = np.square(latitudes - latitude) + np.square(longitudes - longitude)
    return int(np.argmin(distance))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--run", required=True, help="YYYYMMDD")
    parser.add_argument("--run-hour", type=int, default=0)
    parser.add_argument("--forecast-hours", required=True)
    parser.add_argument("--latitude", type=float, default=44.0)
    parser.add_argument("--longitude", type=float, default=78.0)
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--output")
    parser.add_argument("--raw-output-dir")
    args = parser.parse_args()

    output = {}
    raw_output_dir = Path(args.raw_output_dir) if args.raw_output_dir else None
    if raw_output_dir is not None:
        raw_output_dir.mkdir(parents=True, exist_ok=True)
    for forecast_hour in [int(value) for value in args.forecast_hours.split(",")]:
        probability, latitudes, longitudes = reconstruct(
            args.run,
            args.run_hour,
            forecast_hour,
            args.workers,
        )
        index = nearest_index(
            latitudes,
            longitudes,
            args.latitude,
            args.longitude,
        )
        output[str(forecast_hour)] = {
            "latitude": float(latitudes[index]),
            "longitude": float(longitudes[index]),
            "precipitation_probability_unquantized": float(probability[index]),
            "precipitation_probability": int(round(float(probability[index]))),
        }
        if raw_output_dir is not None:
            raw_path = raw_output_dir / f"f{forecast_hour:03d}.f32le"
            quantized = np.rint(probability).astype("<f4", copy=False)
            raw_path.write_bytes(quantized.tobytes(order="C"))
            output[str(forecast_hour)]["raw_path"] = str(raw_path)
            output[str(forecast_hour)]["raw_bytes"] = raw_path.stat().st_size
            output[str(forecast_hour)]["raw_sha256"] = hashlib.sha256(
                raw_path.read_bytes()
            ).hexdigest()
    encoded = json.dumps(output, ensure_ascii=False, indent=2, sort_keys=True)
    if args.output:
        Path(args.output).write_text(encoded + "\n", encoding="utf-8")
    print(encoded)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
