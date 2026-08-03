#!/usr/bin/env python3
"""Render long production-task output as a compact Chinese 1Panel report."""

from __future__ import annotations

import argparse
import json
import os
import queue
import re
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path


RETURN_CODE_PREFIX = "\x1eWEATHER_TASK_RC="
SKIP_REASON_PREFIX = "\x1eWEATHER_TASK_SKIP="
TARGET_RUN_PREFIX = "\x1eWEATHER_TASK_TARGET_RUN="
RUN_PATTERN = re.compile(r"(?<!\d)(20\d{8})(?!\d)")


PRODUCT_NAMES = {
    "gfs013_surface": "GFS 0.13°地面层",
    "ncep_gfs013": "GFS 0.13°地面层",
    "gfs013": "GFS 0.13°地面层",
    "gfs025_pressure": "GFS 0.25°气压层",
    "gfs_pressure_profile": "GFS 0.25°气压层",
    "pressure_profile": "GFS 0.25°气压层",
    "gfs025_surface": "GFS 0.25°地面层",
    "gfs025": "GFS 0.25°地面层",
    "ncep_gfs025": "GFS 0.25°地面层",
    "cams_global_greenhouse_gases": "CAMS 温室气体",
    "greenhouse": "CAMS 温室气体",
    "cams_global": "CAMS 全球空气质量",
}

FORECAST_HOUR_PATTERN = re.compile(r"\bforecastHour\s+(\d+)\b", re.IGNORECASE)
HORIZON_PATTERNS = (
    re.compile(r"\bhorizon=(\d+)\b", re.IGNORECASE),
    re.compile(r"--max-forecast-hour\s+(\d+)\b", re.IGNORECASE),
)


def utc_text() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")


def directory_bytes(roots: list[Path]) -> int:
    total = 0
    for root in roots:
        if not root.exists():
            continue
        for directory, _subdirectories, filenames in os.walk(root, followlinks=False):
            for filename in filenames:
                try:
                    total += (Path(directory) / filename).stat().st_size
                except (FileNotFoundError, PermissionError, OSError):
                    continue
    return total


def structured_payload(line: str) -> dict[str, object]:
    offset = line.find("{")
    if offset < 0:
        return {}
    try:
        value = json.loads(line[offset:])
    except (json.JSONDecodeError, TypeError):
        return {}
    return value if isinstance(value, dict) else {}


def infer_state(line: str, *, default_stage: str) -> tuple[str, str | None]:
    lowered = line.lower()
    payload = structured_payload(line)
    product = str(payload.get("product") or "").lower()
    stage = str(payload.get("stage") or "").lower()
    combined = " ".join((lowered, product, stage))

    product_text = next(
        (label for marker, label in PRODUCT_NAMES.items() if marker in combined),
        None,
    )
    if "startup cleanup" in combined:
        stage_text = "启动前清理"
    elif "cleanup after task" in combined:
        stage_text = "任务后清理"
    elif "cleanup" in combined or "清理" in combined:
        stage_text = "清理异常残留"
    elif "probe" in combined or "ready " in combined or "official run" in combined:
        stage_text = "检查最新批次"
    elif "seed" in combined or "staging" in combined and "reuse" in combined:
        stage_text = "准备历史批次"
    elif "validat" in combined or "verify" in combined or "校验" in combined:
        stage_text = "校验数据"
    elif "webp" in combined or " layer" in combined or "render" in combined:
        stage_text = "生成 WebP"
    elif "publish" in combined or "activate" in combined or "manifest" in combined:
        stage_text = "发布数据"
    elif "convert" in combined or "writing" in combined or "write " in combined:
        stage_text = "生成 OM 文件"
    elif "planning" in combined or " plan" in combined:
        stage_text = "分析下载范围"
    elif "download" in combined or product_text:
        if product_text:
            stage_text = f"下载 {product_text}"
        elif default_stage.startswith("下载 "):
            # Open-Meteo's per-frame lines only contain ``forecastHour``. Keep
            # the product selected by the preceding input-group line instead
            # of degrading the report back to the generic download stage.
            stage_text = default_stage
        else:
            stage_text = "下载原始数据"
    else:
        stage_text = default_stage

    run_match = RUN_PATTERN.search(line)
    return stage_text, run_match.group(1) if run_match else None


def report_progress(
    *,
    task: str,
    default_stage: str,
    watch_roots: list[Path],
    log_file: Path,
    interval_seconds: float,
    input_stream=sys.stdin,
) -> int:
    log_file.parent.mkdir(parents=True, exist_ok=True)
    stage = default_stage
    run = "-"
    target_run = "-"
    forecast_hour: int | None = None
    max_forecast_hour: int | None = None
    skipped_reason: str | None = None
    metadata_error: str | None = None
    return_code: int | None = None
    frame_events = 0
    retry_events = 0
    last_size = directory_bytes(watch_roots)
    last_report_at = time.monotonic()
    print(f"{utc_text()}｜开始｜任务：{task}｜阶段：{stage}", flush=True)

    # Read the child output on a separate thread so the reporting clock keeps
    # advancing while a downloader is quiet.  Iterating over ``input_stream``
    # directly blocks until the next line and used to suppress all one-minute
    # progress messages during long CAMS/GFS network transfers.
    input_events: queue.Queue[tuple[str, object]] = queue.Queue()

    def read_input() -> None:
        try:
            for input_line in input_stream:
                input_events.put(("line", input_line))
        except BaseException as exc:  # Preserve reader failures in the caller.
            input_events.put(("error", exc))
        finally:
            input_events.put(("eof", None))

    threading.Thread(target=read_input, name="task-progress-input", daemon=True).start()

    def emit_progress(now: float) -> None:
        nonlocal frame_events, last_size, last_report_at, retry_events
        current_size = directory_bytes(watch_roots)
        growth = current_size - last_size
        frame_text = ""
        if forecast_hour is not None:
            frame_text = f"｜当前时效：f{forecast_hour:03d}"
            if max_forecast_hour is not None:
                frame_text += f"/f{max_forecast_hour:03d}"
        print(
            f"{utc_text()}｜进度｜任务：{task}｜阶段：{stage}"
            f"｜目标批次：{target_run}｜当前批次：{run}{frame_text}"
            f"｜本周期时效请求日志：{frame_events}｜下载重试告警：{retry_events}"
            f"｜staging 文件表观大小变化：{growth / 1024 / 1024:+.1f} MiB"
            "（含硬链接/生成文件，非网速、非实际磁盘增量）",
            flush=True,
        )
        frame_events = 0
        retry_events = 0
        last_size = current_size
        last_report_at = now

    with log_file.open("a", encoding="utf-8") as raw_log:
        while True:
            remaining = max(interval_seconds - (time.monotonic() - last_report_at), 0.0)
            try:
                event, value = input_events.get(timeout=remaining)
            except queue.Empty:
                emit_progress(time.monotonic())
                continue

            if event == "eof":
                break
            if event == "error":
                if not isinstance(value, BaseException):
                    raise RuntimeError("task progress input reader failed")
                raise value
            line = str(value)
            if line.startswith(RETURN_CODE_PREFIX):
                try:
                    return_code = int(line[len(RETURN_CODE_PREFIX) :].strip())
                except ValueError:
                    return_code = 1
                continue
            if line.startswith(TARGET_RUN_PREFIX):
                candidate = line[len(TARGET_RUN_PREFIX) :].strip()
                if not RUN_PATTERN.fullmatch(candidate):
                    metadata_error = f"无效目标批次：{candidate!r}"
                elif target_run not in ("-", candidate):
                    metadata_error = f"目标批次冲突：{target_run} 与 {candidate}"
                else:
                    target_run = candidate
                continue
            if line.startswith(SKIP_REASON_PREFIX):
                skipped_reason = line[len(SKIP_REASON_PREFIX) :].strip() or "没有待处理批次"
                continue
            raw_log.write(line)
            raw_log.flush()
            inferred_stage, inferred_run = infer_state(line, default_stage=stage)
            if inferred_run and inferred_run != run:
                forecast_hour = None
                max_forecast_hour = None
            if inferred_stage != stage and inferred_stage.startswith("下载 "):
                forecast_hour = None
            stage = inferred_stage
            if inferred_run:
                run = inferred_run
            for horizon_pattern in HORIZON_PATTERNS:
                horizon_match = horizon_pattern.search(line)
                if horizon_match:
                    max_forecast_hour = int(horizon_match.group(1))
                    break
            forecast_hour_match = FORECAST_HOUR_PATTERN.search(line)
            if forecast_hour_match:
                forecast_hour = int(forecast_hour_match.group(1))
                frame_events += 1
            if "download failed, retry" in line.lower():
                retry_events += 1

            now = time.monotonic()
            if now - last_report_at < interval_seconds:
                continue
            emit_progress(now)

    if return_code is None:
        return_code = 1
    if target_run != "-" and skipped_reason is not None:
        metadata_error = metadata_error or "目标批次与跳过原因同时出现"
    if return_code == 0 and target_run == "-" and skipped_reason is None:
        metadata_error = metadata_error or "成功记录缺少目标批次或跳过原因"
    if metadata_error and return_code == 0:
        return_code = 1
    if return_code != 0:
        metadata_text = f"｜日志元数据错误：{metadata_error}" if metadata_error else ""
        print(
            f"{utc_text()}｜失败｜任务：{task}｜阶段：{stage}"
            f"｜目标批次：{target_run}｜当前观察批次：{run}"
            f"｜退出码：{return_code}{metadata_text}｜详细日志：{log_file}",
            flush=True,
        )
    elif skipped_reason is not None:
        print(f"{utc_text()}｜跳过｜任务：{task}｜原因：{skipped_reason}", flush=True)
    else:
        print(
            f"{utc_text()}｜完成｜任务：{task}｜目标批次：{target_run}"
            f"｜最后处理批次：{target_run}",
            flush=True,
        )
    return return_code


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--task", required=True)
    parser.add_argument("--default-stage", default="检查最新批次")
    parser.add_argument("--watch-root", action="append", default=[])
    parser.add_argument("--log-file", type=Path, required=True)
    parser.add_argument("--interval-seconds", type=float, default=60.0)
    args = parser.parse_args(argv)
    if args.interval_seconds <= 0:
        parser.error("--interval-seconds must be positive")
    return report_progress(
        task=args.task,
        default_stage=args.default_stage,
        watch_roots=[Path(value) for value in args.watch_root],
        log_file=args.log_file,
        interval_seconds=args.interval_seconds,
    )


if __name__ == "__main__":
    raise SystemExit(main())
