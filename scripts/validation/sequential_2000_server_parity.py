#!/usr/bin/env python3
"""Strict sequential Shanghai/Singapore parity gate for 2,000 regional points."""

from __future__ import annotations

import argparse
import contextlib
from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import random
import sys
import time
from typing import Any

VALIDATION_ROOT = Path(__file__).resolve().parent
if str(VALIDATION_ROOT) not in sys.path:
    sys.path.insert(0, str(VALIDATION_ROOT))

from ecmwf_variable_catalog import DAILY_VARIABLES as ECMWF_DAILY
from ecmwf_variable_catalog import HOURLY_VARIABLES as ECMWF_HOURLY
from official_200_point_compare import (
    CAMS_RAW,
    GFS_DAILY,
    GFS_HOURLY,
    ProductionSshApiClient,
    ValidationError,
    first_period_difference,
    local_url,
    normalize_rows,
    request_json,
)


POINT_COUNT = 2000
GRID_POINT_COUNT = 1000
PRESSURE_LEVELS = (
    1000, 975, 950, 925, 900, 850, 800, 750, 700, 650, 600, 550, 500,
    450, 400, 350, 300, 250, 200, 150, 100, 50,
)
GFS_PRESSURE_FAMILIES = (
    "temperature",
    "relative_humidity",
    "dew_point",
    "cloud_cover",
    "wind_speed",
    "wind_direction",
    "geopotential_height",
    "vertical_velocity",
)
GFS_ALL_HOURLY = tuple(
    dict.fromkeys(
        (
            *GFS_HOURLY,
            *(
                f"{family}_{level}hPa"
                for family in GFS_PRESSURE_FAMILIES
                for level in PRESSURE_LEVELS
            ),
        )
    )
)
CAMS_CHINESE_AQI = (
    "chinese_aqi",
    "chinese_aqi_pm2_5",
    "chinese_aqi_pm10",
    "chinese_aqi_no2",
    "chinese_aqi_nitrogen_dioxide",
    "chinese_aqi_o3",
    "chinese_aqi_ozone",
    "chinese_aqi_so2",
    "chinese_aqi_sulphur_dioxide",
    "chinese_aqi_co",
    "chinese_aqi_carbon_monoxide",
)
CAMS_ALL_HOURLY = tuple(dict.fromkeys((*CAMS_RAW, *CAMS_CHINESE_AQI)))
CAMS_DAILY = (
    *CAMS_CHINESE_AQI,
    "pm2_5_mean",
    "pm10_mean",
    "nitrogen_dioxide_mean",
    "ozone_maximum_8h_mean",
    "sulphur_dioxide_mean",
    "carbon_monoxide_mean",
)
VARIABLES: dict[str, dict[str, tuple[str, ...]]] = {
    "gfs": {"hourly": GFS_ALL_HOURLY, "daily": tuple(GFS_DAILY)},
    "ec": {"hourly": tuple(ECMWF_HOURLY), "daily": tuple(ECMWF_DAILY)},
    "cams": {"hourly": CAMS_ALL_HOURLY, "daily": CAMS_DAILY},
}
HORIZONS = {"gfs": 384, "ec": 360, "cams": 120}


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def atomic_write_json(path: Path, payload: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp.{os.getpid()}")
    temporary.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, path)


def parse_run(value: str) -> datetime:
    parsed = datetime.strptime(value, "%Y%m%d%H").replace(tzinfo=timezone.utc)
    if parsed.strftime("%Y%m%d%H") != value:
        raise ValueError(f"invalid run: {value}")
    return parsed


def comparison_ranges(model: str, run: str) -> tuple[tuple[str, str], tuple[str, str]]:
    start = parse_run(run)
    end = start + timedelta(hours=HORIZONS[model])
    hourly = (
        start.strftime("%Y-%m-%dT%H:00"),
        end.strftime("%Y-%m-%dT%H:00"),
    )
    daily = (start.strftime("%Y-%m-%d"), end.strftime("%Y-%m-%d"))
    return hourly, daily


def sample_points(seed: int) -> list[dict[str, Any]]:
    rng = random.Random(seed)
    common_grid = [
        (float(latitude), float(longitude))
        for latitude in range(2, 59, 2)
        for longitude in range(72, 141, 2)
    ]
    if len(common_grid) < GRID_POINT_COUNT:
        raise RuntimeError("common exact-grid pool is too small")
    rng.shuffle(common_grid)
    exact = common_grid[:GRID_POINT_COUNT]
    offgrid: list[tuple[float, float]] = []
    seen: set[tuple[float, float]] = set()
    while len(offgrid) < POINT_COUNT - GRID_POINT_COUNT:
        candidate = (
            round(rng.uniform(0.1, 57.9), 6),
            round(rng.uniform(70.1, 139.9), 6),
        )
        if candidate in seen:
            continue
        seen.add(candidate)
        offgrid.append(candidate)

    points: list[dict[str, Any]] = []
    for index in range(GRID_POINT_COUNT):
        for kind, values in (
            ("exact_common_grid", exact[index]),
            ("offgrid_uniform", offgrid[index]),
        ):
            latitude, longitude = values
            points.append(
                {
                    "id": f"p{len(points):04d}",
                    "order": len(points),
                    "kind": kind,
                    "latitude": latitude,
                    "longitude": longitude,
                }
            )
    return points


def chunks(values: tuple[str, ...], size: int) -> list[tuple[str, ...]]:
    return [
        values[index : index + size]
        for index in range(0, len(values), size)
    ]


def fetch(
    base: str,
    url: str,
    timeout: float,
    retries: int,
    ssh_client: ProductionSshApiClient | None = None,
) -> tuple[dict[str, Any], dict[str, Any]]:
    resolved_url = url.replace("__BASE__", base.rstrip("/"), 1)
    headers = {
        "Accept": "application/json",
        "Cache-Control": "no-cache",
        "User-Agent": "weather-server-sequential-2000-parity/1.0",
    }
    started = time.monotonic()
    if ssh_client:
        raw, _headers, _elapsed = ssh_client.request(resolved_url, headers)
        transport = f"production_ssh:{ssh_client.ssh_host}"
    else:
        raw, _headers, _elapsed = request_json(
            "GET",
            resolved_url,
            body=None,
            headers=headers,
            timeout=timeout,
            retries=retries,
        )
        transport = "direct_http"
    wall_elapsed = time.monotonic() - started
    return normalize_rows(json.loads(raw), 1)[0], {
        "transport": transport,
        "response_bytes": len(raw),
        "api_elapsed_seconds": float(_elapsed),
        "wall_elapsed_seconds": wall_elapsed,
    }


def compare_request(
    *,
    shanghai_url: str,
    singapore_url: str,
    model: str,
    point: dict[str, Any],
    hourly: tuple[str, ...],
    daily: tuple[str, ...],
    hourly_time_range: tuple[str, str],
    daily_time_range: tuple[str, str],
    timeout: float,
    retries: int,
    shanghai_ssh_client: ProductionSshApiClient | None = None,
    singapore_ssh_client: ProductionSshApiClient | None = None,
) -> tuple[dict[str, Any] | None, int, dict[str, dict[str, Any]]]:
    template = local_url(
        "__BASE__",
        model,
        point,
        hourly=hourly,
        daily=daily,
        hourly_time_range=hourly_time_range if hourly else None,
        daily_time_range=daily_time_range if daily else None,
    )
    shanghai, shanghai_performance = fetch(
        shanghai_url, template, timeout, retries, shanghai_ssh_client
    )
    singapore, singapore_performance = fetch(
        singapore_url, template, timeout, retries, singapore_ssh_client
    )
    performance = {
        "shanghai": shanghai_performance,
        "singapore": singapore_performance,
    }
    values_compared = 0
    for period, variables in (("hourly", hourly), ("daily", daily)):
        if not variables:
            continue
        difference, hourly_values, daily_values = first_period_difference(
            period,
            variables,
            shanghai,
            singapore,
        )
        values_compared += hourly_values + daily_values
        if difference is not None:
            return difference, values_compared, performance
    return None, values_compared, performance


def record_request_performance(
    checkpoint: dict[str, Any],
    request_performance: dict[str, dict[str, Any]],
) -> None:
    aggregate = checkpoint.setdefault("performance", {})
    for server in ("shanghai", "singapore"):
        current = request_performance[server]
        server_aggregate = aggregate.setdefault(
            server,
            {
                "transport": current["transport"],
                "requests": 0,
                "response_bytes": 0,
                "api_elapsed_seconds_total": 0.0,
                "wall_elapsed_seconds_total": 0.0,
                "api_elapsed_seconds_max": 0.0,
                "wall_elapsed_seconds_max": 0.0,
            },
        )
        if server_aggregate["transport"] != current["transport"]:
            raise ValueError(f"{server} transport changed during parity validation")
        server_aggregate["requests"] += 1
        server_aggregate["response_bytes"] += current["response_bytes"]
        for metric in ("api_elapsed_seconds", "wall_elapsed_seconds"):
            value = float(current[metric])
            server_aggregate[f"{metric}_total"] += value
            server_aggregate[f"{metric}_max"] = max(
                server_aggregate[f"{metric}_max"], value
            )


def summarized_performance(checkpoint: dict[str, Any]) -> dict[str, Any]:
    summary = json.loads(json.dumps(checkpoint.get("performance", {})))
    for server_aggregate in summary.values():
        requests = int(server_aggregate.get("requests", 0))
        if requests <= 0:
            continue
        for metric in ("api_elapsed_seconds", "wall_elapsed_seconds"):
            server_aggregate[f"{metric}_average"] = (
                server_aggregate[f"{metric}_total"] / requests
            )
    return summary


def paired_variable_groups(
    hourly: tuple[str, ...], daily: tuple[str, ...], size: int
) -> list[tuple[tuple[str, ...], tuple[str, ...]]]:
    hourly_groups = chunks(hourly, size)
    daily_groups = chunks(daily, size)
    return [
        (
            hourly_groups[index] if index < len(hourly_groups) else (),
            daily_groups[index] if index < len(daily_groups) else (),
        )
        for index in range(max(len(hourly_groups), len(daily_groups)))
    ]


def checkpoint_contract(args: argparse.Namespace, points: list[dict[str, Any]]) -> dict[str, Any]:
    contract = {
        "version": 4,
        "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "shanghai_url": args.shanghai_url.rstrip("/"),
        "singapore_url": args.singapore_url.rstrip("/"),
        "shanghai_ssh_host": args.shanghai_ssh_host.strip() or None,
        "singapore_ssh_host": args.singapore_ssh_host.strip() or None,
        "runs": {
            "gfs": args.gfs_run,
            "ec": args.ecmwf_run,
            "cams": args.cams_run,
        },
        "point_count": len(points),
        "grid_point_count": sum(point["kind"] == "exact_common_grid" for point in points),
        "points_sha256": hashlib.sha256(canonical_bytes(points)).hexdigest(),
        "field_chunk_size": args.field_chunk_size,
        "variables_sha256": hashlib.sha256(canonical_bytes(VARIABLES)).hexdigest(),
    }
    if args.identity_report:
        identity_path = Path(args.identity_report)
        contract["identity_report_sha256"] = hashlib.sha256(
            identity_path.read_bytes()
        ).hexdigest()
    return contract


def validate_identity_report(
    path: Path,
    expected_runs: dict[str, str],
) -> dict[str, Any]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("status") != "matched":
        raise ValueError("batch identity report is not matched")
    models = payload.get("models")
    if not isinstance(models, dict):
        raise ValueError("batch identity report is missing models")
    for model, expected_run in expected_runs.items():
        entry = models.get(model)
        if not isinstance(entry, dict):
            raise ValueError(f"batch identity report is missing model {model}")
        if entry.get("expected_run") != expected_run:
            raise ValueError(
                f"batch identity expected run differs for {model}: "
                f"{entry.get('expected_run')} != {expected_run}"
            )
        shanghai = entry.get("shanghai")
        singapore = entry.get("singapore")
        if not isinstance(shanghai, dict) or not isinstance(singapore, dict):
            raise ValueError(f"batch identity is missing server data for {model}")
        for server_name, server in (
            ("shanghai", shanghai),
            ("singapore", singapore),
        ):
            if server.get("latest_complete_run") != expected_run:
                raise ValueError(
                    f"{server_name} latest run differs for {model}: "
                    f"{server.get('latest_complete_run')} != {expected_run}"
                )
            source_runs = server.get("source_runs")
            horizons = server.get("source_run_max_forecast_hours")
            if (
                not isinstance(source_runs, list)
                or not source_runs
                or not all(isinstance(value, str) for value in source_runs)
                or not isinstance(horizons, dict)
                or set(horizons) != set(source_runs)
                or not all(
                    isinstance(value, int) and value >= 0
                    for value in horizons.values()
                )
            ):
                raise ValueError(
                    f"{server_name} source-run contract is invalid for {model}"
                )
            products = server.get("products")
            if not isinstance(products, dict) or not products:
                raise ValueError(
                    f"{server_name} product identity is missing for {model}"
                )
        if shanghai["source_runs"] != singapore["source_runs"]:
            raise ValueError(f"source runs differ between servers for {model}")
        if (
            shanghai["source_run_max_forecast_hours"]
            != singapore["source_run_max_forecast_hours"]
        ):
            raise ValueError(f"source horizons differ between servers for {model}")
        if shanghai["products"] != singapore["products"]:
            raise ValueError(f"product source identities differ for {model}")
    return payload


def run_comparison(
    *,
    args: argparse.Namespace,
    points: list[dict[str, Any]],
    runs: dict[str, str],
    checkpoint: dict[str, Any],
    checkpoint_path: Path,
    output_path: Path,
    shanghai_ssh_client: ProductionSshApiClient | None,
    singapore_ssh_client: ProductionSshApiClient | None,
) -> int:
    start_index = int(checkpoint.get("points_completed", 0))
    for point_index in range(start_index, len(points)):
        point = points[point_index]
        for model in ("gfs", "ec", "cams"):
            hourly_range, daily_range = comparison_ranges(model, runs[model])
            for hourly_group, daily_group in paired_variable_groups(
                VARIABLES[model]["hourly"],
                VARIABLES[model]["daily"],
                args.field_chunk_size,
            ):
                difference, value_count, request_performance = compare_request(
                    shanghai_url=args.shanghai_url,
                    singapore_url=args.singapore_url,
                    model=model,
                    point=point,
                    hourly=hourly_group,
                    daily=daily_group,
                    hourly_time_range=hourly_range,
                    daily_time_range=daily_range,
                    timeout=args.timeout,
                    retries=args.retries,
                    shanghai_ssh_client=shanghai_ssh_client,
                    singapore_ssh_client=singapore_ssh_client,
                )
                checkpoint["values_compared"] += value_count
                record_request_performance(checkpoint, request_performance)
                if difference is not None:
                    checkpoint.update(
                        {
                            "status": "failed",
                            "failed_point": point,
                            "failed_model": model,
                            "failed_period": difference.get("period"),
                            "failed_variables": {
                                "hourly": list(hourly_group),
                                "daily": list(daily_group),
                            },
                            "difference": difference,
                            "performance_summary": summarized_performance(checkpoint),
                            "failed_at": datetime.now(timezone.utc).isoformat(),
                        }
                    )
                    atomic_write_json(checkpoint_path, checkpoint)
                    atomic_write_json(output_path, checkpoint)
                    print(json.dumps(checkpoint, ensure_ascii=False))
                    return 1
                if args.request_pause:
                    time.sleep(args.request_pause)
        checkpoint["points_completed"] = point_index + 1
        checkpoint["last_completed_point"] = point
        atomic_write_json(checkpoint_path, checkpoint)
        if checkpoint["points_completed"] % args.progress_every == 0:
            print(
                json.dumps(
                    {
                        "status": "running",
                        "points_completed": checkpoint["points_completed"],
                        "points_total": len(points),
                        "values_compared": checkpoint["values_compared"],
                        "performance_summary": summarized_performance(checkpoint),
                    },
                    ensure_ascii=False,
                ),
                flush=True,
            )

    checkpoint.update(
        {
            "status": "passed",
            "passed": True,
            "completed_at": datetime.now(timezone.utc).isoformat(),
            "points_completed": len(points),
            "performance_summary": summarized_performance(checkpoint),
        }
    )
    atomic_write_json(checkpoint_path, checkpoint)
    atomic_write_json(output_path, checkpoint)
    print(json.dumps(checkpoint, ensure_ascii=False))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shanghai-url", required=True)
    parser.add_argument("--singapore-url", required=True)
    parser.add_argument(
        "--shanghai-ssh-host",
        default="",
        help="configured SSH alias for the real Shanghai loopback production API",
    )
    parser.add_argument(
        "--singapore-ssh-host",
        default="",
        help="configured SSH alias for the real Singapore loopback production API",
    )
    parser.add_argument("--gfs-run", required=True)
    parser.add_argument("--ecmwf-run", required=True)
    parser.add_argument("--cams-run", required=True)
    parser.add_argument("--output-report", required=True)
    parser.add_argument("--checkpoint-report")
    parser.add_argument(
        "--identity-report",
        help=(
            "normalized, independently collected server marker identity report; "
            "required for the full 2,000-point acceptance gate"
        ),
    )
    parser.add_argument("--seed", type=int, default=20260730)
    parser.add_argument("--field-chunk-size", type=int, default=256)
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("--retries", type=int, default=3)
    parser.add_argument("--request-pause", type=float, default=0.0)
    parser.add_argument("--progress-every", type=int, default=10)
    parser.add_argument("--allow-reduced-points", type=int)
    args = parser.parse_args()
    if args.field_chunk_size <= 0 or args.progress_every <= 0:
        parser.error("chunk and progress sizes must be positive")
    if args.request_pause < 0:
        parser.error("request pause must not be negative")
    for value in (args.gfs_run, args.ecmwf_run, args.cams_run):
        parse_run(value)
    if args.shanghai_ssh_host and not args.shanghai_url.startswith(
        ("http://127.0.0.1", "http://localhost")
    ):
        parser.error("--shanghai-ssh-host requires a loopback --shanghai-url")
    if args.singapore_ssh_host and not args.singapore_url.startswith(
        ("http://127.0.0.1", "http://localhost")
    ):
        parser.error("--singapore-ssh-host requires a loopback --singapore-url")

    points = sample_points(args.seed)
    if args.allow_reduced_points is not None:
        if not 1 <= args.allow_reduced_points <= POINT_COUNT:
            parser.error("--allow-reduced-points must be between 1 and 2000")
        points = points[: args.allow_reduced_points]
    elif len(points) != POINT_COUNT:
        parser.error("acceptance mode requires exactly 2,000 points")
    if len(points) == POINT_COUNT and not args.identity_report:
        parser.error("acceptance mode requires --identity-report")

    output_path = Path(args.output_report)
    checkpoint_path = (
        Path(args.checkpoint_report)
        if args.checkpoint_report
        else Path(str(output_path) + ".checkpoint.json")
    )
    runs = {"gfs": args.gfs_run, "ec": args.ecmwf_run, "cams": args.cams_run}
    identity = (
        validate_identity_report(Path(args.identity_report), runs)
        if args.identity_report
        else None
    )
    contract = checkpoint_contract(args, points)
    contract_sha256 = hashlib.sha256(canonical_bytes(contract)).hexdigest()
    checkpoint: dict[str, Any] = {
        "contract": contract,
        "contract_sha256": contract_sha256,
        "status": "running",
        "points_completed": 0,
        "values_compared": 0,
        "performance": {},
        "started_at": datetime.now(timezone.utc).isoformat(),
        "batch_identity": identity,
    }
    if checkpoint_path.exists():
        saved = json.loads(checkpoint_path.read_text(encoding="utf-8"))
        if saved.get("contract_sha256") != contract_sha256:
            parser.error("checkpoint does not match this exact comparison contract")
        checkpoint.update(saved)

    with contextlib.ExitStack() as transports:
        shanghai_ssh_client = (
            transports.enter_context(
                ProductionSshApiClient(
                    args.shanghai_ssh_host.strip(), args.timeout, args.retries
                )
            )
            if args.shanghai_ssh_host.strip()
            else None
        )
        singapore_ssh_client = (
            transports.enter_context(
                ProductionSshApiClient(
                    args.singapore_ssh_host.strip(), args.timeout, args.retries
                )
            )
            if args.singapore_ssh_host.strip()
            else None
        )
        return run_comparison(
            args=args,
            points=points,
            runs=runs,
            checkpoint=checkpoint,
            checkpoint_path=checkpoint_path,
            output_path=output_path,
            shanghai_ssh_client=shanghai_ssh_client,
            singapore_ssh_client=singapore_ssh_client,
        )


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, ValidationError, json.JSONDecodeError) as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(2)
