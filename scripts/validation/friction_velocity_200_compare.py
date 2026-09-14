#!/usr/bin/env python3
"""Freeze and compare GFS friction_velocity at the standard 200 validation points.

The public Open-Meteo API does not expose this locally added native GFS field.
The reference endpoint therefore uses the pinned, unmodified Open-Meteo Swift
engine on a read-only view where the friction-velocity OM directory is exposed
under the API-visible ``wind_gusts_10m`` name. Both variables use the same GFS
unit and bounded Hermite interpolation. FlatBuffers preserves the pre-JSON float
values, which are rounded to the production field's three-decimal API contract.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
from pathlib import Path
import struct
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
REFERENCE_ALIAS_FIELD = "wind_gusts_10m"


def point_url(
    base: str,
    point: dict[str, Any],
    field: str = FIELD,
    *,
    timeformat: str = "iso8601",
    response_format: str | None = None,
    start_hour: str | None = None,
    end_hour: str | None = None,
) -> str:
    params = {
        "latitude": format(float(point["latitude"]), ".9g"),
        "longitude": format(float(point["longitude"]), ".9g"),
        "hourly": field,
        "forecast_days": "16",
        "timezone": "GMT",
        "timeformat": timeformat,
        "cell_selection": "nearest",
        "wind_speed_unit": "ms",
    }
    if response_format is not None:
        params["format"] = response_format
    if start_hour is not None or end_hour is not None:
        if not start_hour or not end_hour:
            raise ValidationError("start_hour and end_hour must be provided together")
        params["start_hour"] = start_hour
        params["end_hour"] = end_hour
    return base.rstrip("/") + "/v1/gfs?" + urllib.parse.urlencode(params, safe=",")


def reference_point_url(base: str, point: dict[str, Any], field: str) -> str:
    return point_url(
        base,
        point,
        field,
        timeformat="unixtime",
        response_format="flatbuffers",
    )


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


def load_flatbuffers_sdk(sdk_path: Path) -> tuple[Any, Any, Any]:
    if not sdk_path.is_dir():
        raise ValidationError(f"Open-Meteo FlatBuffers SDK path is missing: {sdk_path}")
    sdk_path_text = str(sdk_path.resolve())
    if sdk_path_text not in sys.path:
        sys.path.insert(0, sdk_path_text)
    try:
        from openmeteo_sdk.Unit import Unit
        from openmeteo_sdk.Variable import Variable
        from openmeteo_sdk.WeatherApiResponse import WeatherApiResponse
    except (ImportError, ModuleNotFoundError) as exc:
        raise ValidationError(
            f"cannot import the pinned Open-Meteo FlatBuffers SDK from {sdk_path}"
        ) from exc
    return WeatherApiResponse, Variable, Unit


def flatbuffers_projection(raw: bytes, sdk_path: Path) -> dict[str, Any]:
    if len(raw) < 8:
        raise ValidationError("friction reference FlatBuffers response is truncated")
    message_size = struct.unpack_from("<I", raw, 0)[0]
    if message_size != len(raw) - 4:
        raise ValidationError(
            "friction reference must contain exactly one size-prefixed FlatBuffer"
        )
    WeatherApiResponse, Variable, Unit = load_flatbuffers_sdk(sdk_path)
    response = WeatherApiResponse.GetRootAs(raw, 4)
    hourly = response.Hourly()
    if hourly is None or hourly.VariablesLength() != 1:
        raise ValidationError("friction reference FlatBuffer has an invalid hourly section")
    variable = hourly.Variables(0)
    if variable is None or variable.Variable() != Variable.wind_gusts:
        raise ValidationError("friction reference FlatBuffer did not return the alias field")
    if variable.Unit() != Unit.metre_per_second:
        raise ValidationError("friction reference FlatBuffer has an unexpected unit")
    interval = int(hourly.Interval())
    start = int(hourly.Time())
    end = int(hourly.TimeEnd())
    values = [
        None if not math.isfinite(value) else float(f"{value:.3f}")
        for value in (float(variable.Values(index)) for index in range(variable.ValuesLength()))
    ]
    if interval <= 0 or end <= start or start + interval * len(values) != end:
        raise ValidationError("friction reference FlatBuffer has an invalid time axis")
    times = [
        dt.datetime.fromtimestamp(start + index * interval, dt.timezone.utc).strftime(
            "%Y-%m-%dT%H:%M"
        )
        for index in range(len(values))
    ]
    return {
        "hourly_units": {FIELD: "m/s"},
        "hourly": {"time": times, FIELD: values},
    }


def first_difference(reference: dict[str, Any], local: dict[str, Any]) -> dict[str, Any] | None:
    if reference["hourly_units"] != local["hourly_units"]:
        return {
            "reason": "hourly_units",
            "reference": reference["hourly_units"],
            "local": local["hourly_units"],
        }
    reference_hourly = reference["hourly"]
    local_hourly = local["hourly"]
    reference_times = reference_hourly["time"]
    local_times = local_hourly["time"]
    if not local_times or len(local_times) > len(reference_times):
        return {
            "reason": "time_axis_length",
            "reference": len(reference_times),
            "local": len(local_times),
        }
    for index, (reference_time, local_time) in enumerate(zip(reference_times, local_times)):
        if reference_time != local_time:
            return {
                "reason": "json_value",
                "variable": "time",
                "index": index,
                "time": reference_time,
                "reference": reference_time,
                "local": local_time,
            }
    reference_values = reference_hourly[FIELD]
    local_values = local_hourly[FIELD]
    if len(local_values) != len(local_times):
        return {
            "reason": "local_value_axis_length",
            "reference": len(local_times),
            "local": len(local_values),
        }
    for index, (reference_value, local_value) in enumerate(
        zip(reference_values, local_values)
    ):
        if reference_value != local_value:
            return {
                "reason": "json_value",
                "variable": FIELD,
                "index": index,
                "time": reference_times[index],
                "reference": reference_value,
                "local": local_value,
            }
    if any(value is not None for value in reference_values[len(local_values) :]):
        index = len(local_values) + next(
            offset
            for offset, value in enumerate(reference_values[len(local_values) :])
            if value is not None
        )
        return {
            "reason": "finite_reference_value_after_local_model_end",
            "variable": FIELD,
            "index": index,
            "time": reference_times[index],
            "reference": reference_values[index],
            "local": None,
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
    sdk_path: Path,
    reference_query_field: str,
) -> dict[str, Any]:
    points = sample_points(model="gfs")
    rows: list[dict[str, Any]] = []
    requests: list[dict[str, Any]] = []
    with ProductionSshApiClient(ssh_host, timeout, retries) as client:
        for index, point in enumerate(points):
            url = reference_point_url(reference_base, point, reference_query_field)
            raw, _headers, _elapsed = client.request(url, headers={})
            rows.append(flatbuffers_projection(raw, sdk_path))
            requests.append({"point_index": index, "point": point, "url": url})
    response_raw = pretty_bytes(rows)
    request_raw = pretty_bytes(requests)
    write_once(output / "reference" / "request.json", request_raw)
    write_once(output / "reference" / "response.json", response_raw)
    metadata = {
        "schema_version": SCHEMA_VERSION,
        "type": "gfs_friction_velocity_reference_snapshot",
        "field": FIELD,
        "reference_query_field": reference_query_field,
        "reference_response_format": "size_prefixed_flatbuffers",
        "reference_value_decimals": 3,
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
        "reference_null_tail_exempted": 0,
        "failure": None,
    }
    write_once(output / "local" / "report.initial.json", pretty_bytes(report))
    with ProductionSshApiClient(ssh_host, timeout, retries) as client:
        for index, (point, reference) in enumerate(zip(points, references)):
            reference_times = reference["hourly"]["time"]
            url = point_url(
                local_base,
                point,
                start_hour=reference_times[0],
                end_hour=reference_times[-1],
            )
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
            values = len(local["hourly"][FIELD])
            report["points_completed"] += 1
            report["values_compared"] += values
            report["reference_null_tail_exempted"] += len(
                reference["hourly"][FIELD]
            ) - values
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
    capture.add_argument("--flatbuffers-sdk-path", type=Path, required=True)
    capture.add_argument("--reference-query-field", default=REFERENCE_ALIAS_FIELD)
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
                args.flatbuffers_sdk_path,
                args.reference_query_field,
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
