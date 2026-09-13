#!/usr/bin/env python3
"""Freeze and compare GFS friction_velocity at the standard 200 validation points.

The public Open-Meteo API does not expose this locally added native GFS field.
The reference endpoint must therefore be a read-only server built from the pinned
Open-Meteo Swift source and mounted on the exact immutable production run.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
import urllib.parse
from typing import Any

VALIDATION_ROOT = Path(__file__).resolve().parent
if str(VALIDATION_ROOT) not in sys.path:
    sys.path.insert(0, str(VALIDATION_ROOT))

from official_200_point_compare import (  # noqa: E402
    POINT_COUNT,
    ProductionSshApiClient,
    ValidationError,
    canonical_bytes,
    normalize_rows,
    pretty_bytes,
    sample_points,
    sha256_bytes,
    sha256_file,
    write_once,
)

SCHEMA_VERSION = 1
FIELD = "friction_velocity"


def point_url(base: str, point: dict[str, Any]) -> str:
    params = {
        "latitude": format(float(point["latitude"]), ".9g"),
        "longitude": format(float(point["longitude"]), ".9g"),
        "hourly": FIELD,
        "forecast_days": "16",
        "timezone": "GMT",
        "timeformat": "iso8601",
        "cell_selection": "nearest",
        "wind_speed_unit": "ms",
    }
    return base.rstrip("/") + "/v1/gfs?" + urllib.parse.urlencode(params, safe=",")


def projection(raw: bytes) -> dict[str, Any]:
    row = normalize_rows(json.loads(raw), 1)[0]
    hourly = row.get("hourly")
    units = row.get("hourly_units")
    if not isinstance(hourly, dict) or not isinstance(units, dict):
        raise ValidationError("friction reference response is missing hourly data or units")
    times = hourly.get("time")
    values = hourly.get(FIELD)
    if (
        not isinstance(times, list)
        or not isinstance(values, list)
        or len(times) != len(values)
        or len(set(times)) != len(times)
        or not times
    ):
        raise ValidationError("friction reference response has an invalid value axis")
    if units.get(FIELD) != "m/s":
        raise ValidationError(f"unexpected friction_velocity unit: {units.get(FIELD)!r}")
    return {"hourly_units": {FIELD: units[FIELD]}, "hourly": {"time": times, FIELD: values}}


def first_difference(reference: dict[str, Any], local: dict[str, Any]) -> dict[str, Any] | None:
    if reference["hourly_units"] != local["hourly_units"]:
        return {
            "reason": "hourly_units",
            "reference": reference["hourly_units"],
            "local": local["hourly_units"],
        }
    reference_hourly = reference["hourly"]
    local_hourly = local["hourly"]
    for key in ("time", FIELD):
        reference_values = reference_hourly[key]
        local_values = local_hourly[key]
        if reference_values == local_values:
            continue
        index = next(
            (
                offset
                for offset, pair in enumerate(zip(reference_values, local_values))
                if pair[0] != pair[1]
            ),
            min(len(reference_values), len(local_values)),
        )
        return {
            "reason": "json_value",
            "variable": key,
            "index": index,
            "time": (
                reference_hourly["time"][index]
                if index < len(reference_hourly["time"])
                else None
            ),
            "reference": reference_values[index] if index < len(reference_values) else None,
            "local": local_values[index] if index < len(local_values) else None,
        }
    return None


def capture_reference(
    output: Path,
    ssh_host: str,
    reference_base: str,
    expected_run: str,
    reference_image: str,
    timeout: float,
    retries: int,
) -> dict[str, Any]:
    points = sample_points(model="gfs")
    rows: list[dict[str, Any]] = []
    requests: list[dict[str, Any]] = []
    with ProductionSshApiClient(ssh_host, timeout, retries) as client:
        for index, point in enumerate(points):
            url = point_url(reference_base, point)
            raw, _headers, _elapsed = client.request(url, headers={})
            rows.append(projection(raw))
            requests.append({"point_index": index, "point": point, "url": url})
    response_raw = pretty_bytes(rows)
    request_raw = pretty_bytes(requests)
    write_once(output / "reference" / "request.json", request_raw)
    write_once(output / "reference" / "response.json", response_raw)
    metadata = {
        "schema_version": SCHEMA_VERSION,
        "type": "gfs_friction_velocity_reference_snapshot",
        "field": FIELD,
        "expected_run": expected_run,
        "reference_image": reference_image,
        "reference_transport": f"production_ssh:{ssh_host}",
        "reference_base": reference_base,
        "points": POINT_COUNT,
        "point_plan_sha256": sha256_bytes(canonical_bytes(points)),
        "request_sha256": sha256_bytes(request_raw),
        "response_sha256": sha256_bytes(response_raw),
    }
    write_once(output / "reference" / "metadata.json", pretty_bytes(metadata))
    return metadata


def validate_local(
    output: Path,
    ssh_host: str,
    local_base: str,
    timeout: float,
    retries: int,
) -> dict[str, Any]:
    metadata_path = output / "reference" / "metadata.json"
    response_path = output / "reference" / "response.json"
    if not metadata_path.exists() or not response_path.exists():
        raise ValidationError("immutable friction reference snapshot is missing")
    metadata = json.loads(metadata_path.read_bytes())
    if sha256_file(response_path) != metadata.get("response_sha256"):
        raise ValidationError("friction reference snapshot hash mismatch")
    references = json.loads(response_path.read_bytes())
    points = sample_points(model="gfs")
    if len(references) != POINT_COUNT:
        raise ValidationError("friction reference snapshot does not contain 200 points")
    report: dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "type": "gfs_friction_velocity_200_point_comparison",
        "status": "running",
        "field": FIELD,
        "expected_run": metadata["expected_run"],
        "reference_image": metadata["reference_image"],
        "reference_response_sha256": metadata["response_sha256"],
        "points_total": POINT_COUNT,
        "points_completed": 0,
        "values_compared": 0,
        "failure": None,
    }
    write_once(output / "local" / "report.initial.json", pretty_bytes(report))
    with ProductionSshApiClient(ssh_host, timeout, retries) as client:
        for index, (point, reference) in enumerate(zip(points, references)):
            url = point_url(local_base, point)
            raw, _headers, _elapsed = client.request(url, headers={})
            local = projection(raw)
            response_file = output / "local" / f"point-{index:03d}.response.json"
            write_once(response_file, pretty_bytes(local))
            difference = first_difference(reference, local)
            if difference is not None:
                report["status"] = "failed"
                report["failure"] = {"point_index": index, "point": point, **difference}
                write_once(output / "local" / "report.json", pretty_bytes(report))
                return report
            values = len(reference["hourly"][FIELD])
            report["points_completed"] += 1
            report["values_compared"] += values
            write_once(
                output / "local" / f"point-{index:03d}.receipt.json",
                pretty_bytes(
                    {
                        "point_index": index,
                        "point": point,
                        "values_compared": values,
                        "reference_projection_sha256": sha256_bytes(canonical_bytes(reference)),
                        "local_projection_sha256": sha256_bytes(canonical_bytes(local)),
                    }
                ),
            )
    report["status"] = "passed"
    write_once(output / "local" / "report.json", pretty_bytes(report))
    return report


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    subparsers = result.add_subparsers(dest="command", required=True)
    capture = subparsers.add_parser("capture")
    capture.add_argument("--output", type=Path, required=True)
    capture.add_argument("--ssh-host", required=True)
    capture.add_argument("--reference-base", default="http://127.0.0.1:18080")
    capture.add_argument("--expected-run", required=True)
    capture.add_argument("--reference-image", required=True)
    validate = subparsers.add_parser("validate")
    validate.add_argument("--output", type=Path, required=True)
    validate.add_argument("--ssh-host", required=True)
    validate.add_argument("--local-base", default="http://127.0.0.1:8088")
    for command in (capture, validate):
        command.add_argument("--timeout", type=float, default=120.0)
        command.add_argument("--retries", type=int, default=2)
    return result


def main() -> int:
    args = parser().parse_args()
    try:
        if args.command == "capture":
            report = capture_reference(
                args.output,
                args.ssh_host,
                args.reference_base,
                args.expected_run,
                args.reference_image,
                args.timeout,
                args.retries,
            )
        else:
            report = validate_local(
                args.output,
                args.ssh_host,
                args.local_base,
                args.timeout,
                args.retries,
            )
    except ValidationError as exc:
        print(json.dumps({"status": "failed", "error": str(exc)}, ensure_ascii=False))
        return 1
    print(json.dumps(report, ensure_ascii=False, sort_keys=True))
    return 0 if report.get("status") in {None, "passed"} else 1


if __name__ == "__main__":
    raise SystemExit(main())
