#!/usr/bin/env python3
"""Render long production-task output as a compact Chinese 1Panel report."""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import time
from datetime import datetime, timezone
from pathlib import Path


RETURN_CODE_PREFIX = "\x1eWEATHER_TASK_RC="
RUN_PATTERN = re.compile(r"(?<!\d)(20\d{8})(?!\d)")
PRODUCT_NAMES = {
    "gfs013_surface": "GFS 0.13°地面层",
    "ncep_gfs013": "GFS 0.13°地面层",
    "gfs013": "GFS 0.13°地面层",
    "gfs025": "GFS 0.25°地面层",
    "ncep_gfs025": "GFS 0.25°地面层",
    "gfs_pressure_profile": "GFS 气压层",
    "pressure_profile": "GFS 气压层",
    "cams_global_greenhouse_gases": "CAMS 温室气体",
    "greenhouse": "CAMS 温室气体",
    "cams_global": "CAMS 全球空气质量",
}


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


def infer_state(line: str, *, default_stage: str) -> tuple[str, str | None, bool]:
    lowered = line.lower()
    payload = structured_payload(line)
    product = str(payload.get("product") or "").lower()
    stage = str(payload.get("stage") or "").lower()
    combined = " ".join((lowered, product, stage))
    product_text = next(
        (label for marker, label in PRODUCT_NAMES.items() if marker in combined),
        None,
    )
    if "probe" in combined or "ready " in combined or "official run" in combined:
        stage_text = "检查最新批次"
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
        stage_text = f"下载 {product_text}" if product_text else "下载原始数据"
    else:
        stage_text = default_stage
    run_match = RUN_PATTERN.search(line)
    skipped = any(
        marker in lowered
        for marker in (" skip", "skipped", "already running", "not_ready", "not ready")
    )
    return stage_text, run_match.group(1) if run_match else None, skipped


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
    skipped = False
    return_code: int | None = None
    last_size = directory_bytes(watch_roots)
    last_report_at = time.monotonic()
    print(f"{utc_text()}｜开始｜任务：{task}｜阶段：{stage}", flush=True)
    with log_file.open("a", encoding="utf-8") as raw_log:
        for line in input_stream:
            if line.startswith(RETURN_CODE_PREFIX):
                try:
                    return_code = int(line[len(RETURN_CODE_PREFIX) :].strip())
                except ValueError:
                    return_code = 1
                continue
            raw_log.write(line)
            raw_log.flush()
            inferred_stage, inferred_run, line_skipped = infer_state(
                line, default_stage=stage
            )
            stage = inferred_stage
            if inferred_run:
                run = inferred_run
            skipped = skipped or line_skipped
            now = time.monotonic()
            if now - last_report_at < interval_seconds:
                continue
            current_size = directory_bytes(watch_roots)
            elapsed = max(now - last_report_at, 0.001)
            growth = max(current_size - last_size, 0)
            print(
                f"{utc_text()}｜进度｜任务：{task}｜阶段：{stage}｜批次：{run}"
                f"｜近一分钟增长：{growth / 1024 / 1024:.1f} MiB"
                f"｜速度：{growth / elapsed / 1024 / 1024:.2f} MiB/s",
                flush=True,
            )
            last_size = current_size
            last_report_at = now
    if return_code is None:
        return_code = 1
    if return_code != 0:
        print(
            f"{utc_text()}｜失败｜任务：{task}｜阶段：{stage}｜批次：{run}"
            f"｜退出码：{return_code}｜详细日志：{log_file}",
            flush=True,
        )
    elif skipped:
        print(f"{utc_text()}｜跳过｜任务：{task}｜原因：已有任务运行或没有新批次", flush=True)
    else:
        print(f"{utc_text()}｜完成｜任务：{task}｜批次：{run}", flush=True)
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
