from __future__ import annotations

from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import json
import os
from pathlib import Path
import sys
import time
from typing import Iterable

from .checksum import sha256_file
from .http_range import (
    ByteRange,
    copy_byte_range_to_file,
    download_byte_ranges,
    fetch_byte_range_with_retry,
    fetch_http_prefix_to_file,
)
from .storage_guard import enforce_environment_storage_guard


def write_fixture_om_file(output_root: Path, model: str, coverage_id: str) -> dict:
    directory = output_root / "published" / model / "coverages" / coverage_id
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f"{model}_{coverage_id}.om"
    path.write_bytes(f"{model}:{coverage_id}\n".encode("utf-8"))
    relative_path = path.relative_to(output_root / "published" / model)
    size = path.stat().st_size
    return {
        "path": str(relative_path).replace("\\", "/"),
        "bytes": size,
        "sha256": sha256_file(path),
        "remote_content_length": size,
        "downloaded_bytes": size,
    }


def write_http_range_file(
    output_root: Path,
    model: str,
    coverage_id: str,
    source_url: str,
    ranges: Iterable[ByteRange],
) -> dict:
    directory = output_root / "published" / model / "coverages" / coverage_id
    path = directory / f"{model}_{coverage_id}.om"
    ranges = tuple(ranges)
    enforce_environment_storage_guard(
        path,
        additional_bytes=sum(item.length for item in ranges),
    )
    return download_byte_ranges(
        source_url,
        ranges,
        path,
        relative_to=output_root / "published" / model,
    )


def _safe_utc_path(value: str) -> str:
    return (
        value.replace(":", "")
        .replace("-", "")
        .replace("+", "")
        .replace(".", "")
        .replace("Z", "Z")
    )


def write_om_range_bundle_file(
    output_root: Path,
    model: str,
    coverage_id: str,
    object_record: dict,
    bundle: dict,
    source_url: str,
) -> dict:
    valid_time = _safe_utc_path(str(object_record["valid_time_utc"]))
    source_run = str(object_record["source_run"])
    variable = str(bundle["variable"])
    directory = output_root / "published" / model / "coverages" / coverage_id / "objects" / source_run / valid_time
    path = directory / f"{variable}.omranges"
    byte_ranges = tuple(bundle["byte_ranges"])
    expected_bytes = sum(item.length for item in byte_ranges)
    base = output_root / "published" / model
    if path.exists() and path.stat().st_size == expected_bytes:
        relative_path = path.relative_to(base)
        file_record = {
            "path": str(relative_path).replace("\\", "/"),
            "bytes": path.stat().st_size,
            "sha256": sha256_file(path),
            "source_url": source_url,
            "byte_ranges": [item.as_manifest() for item in byte_ranges],
            "remote_content_length": None,
            "downloaded_bytes": expected_bytes,
            "reused_existing": True,
        }
    else:
        enforce_environment_storage_guard(path, additional_bytes=expected_bytes)
        file_record = download_byte_ranges(
            source_url,
            byte_ranges,
            path,
            relative_to=base,
        )
        file_record["reused_existing"] = False
    file_record.update(
        {
            "kind": "om_range_bundle",
            "variable": variable,
            "valid_time_utc": object_record["valid_time_utc"],
            "source_run": object_record["source_run"],
            "forecast_hour": object_record["forecast_hour"],
            "selection_ranges": bundle["selection_ranges"],
            "array": bundle["array"],
            "lut_byte_ranges": bundle["lut_byte_ranges"],
            "data_byte_ranges": bundle["data_byte_ranges"],
            "lut_bytes_read": bundle["lut_bytes_read"],
        }
    )
    return file_record


def _coverage_bundle_entry(
    object_record: dict,
    bundle: dict,
    source_url: str,
    *,
    bundle_offset: int,
    bundle_bytes: int,
) -> dict:
    byte_ranges = tuple(bundle["byte_ranges"])
    return {
        "variable": bundle["variable"],
        "variable_path": bundle["path"],
        "valid_time_utc": object_record["valid_time_utc"],
        "source_run": object_record["source_run"],
        "forecast_hour": object_record["forecast_hour"],
        "coverage_source_run": object_record.get(
            "coverage_source_run", object_record["source_run"]
        ),
        "coverage_forecast_hour": object_record.get(
            "coverage_forecast_hour", object_record["forecast_hour"]
        ),
        "interpolation_support": bool(object_record.get("interpolation_support")),
        "source_url": source_url,
        "selection_ranges": bundle["selection_ranges"],
        "array": bundle["array"],
        "lut_byte_ranges": bundle["lut_byte_ranges"],
        "data_byte_ranges": bundle["data_byte_ranges"],
        "lut_bytes_read": bundle["lut_bytes_read"],
        "byte_ranges": [item.as_manifest() for item in byte_ranges],
        "bundle_offset": bundle_offset,
        "bundle_bytes": bundle_bytes,
    }


def _prepare_coverage_bundle_entry(entry: dict, *, bundle_offset: int) -> tuple[dict, int]:
    object_record = entry["object_record"]
    bundle = entry["bundle"]
    source_url = entry["source_url"]
    byte_ranges = tuple(bundle["byte_ranges"])
    bundle_bytes = sum(item.length for item in byte_ranges)
    prepared = {
        "source_url": source_url,
        "remote_content_length": entry.get("remote_content_length"),
        "byte_ranges": byte_ranges,
        "manifest": _coverage_bundle_entry(
            object_record,
            bundle,
            source_url,
            bundle_offset=bundle_offset,
            bundle_bytes=bundle_bytes,
        ),
    }
    return prepared, bundle_offset + bundle_bytes


def _coverage_bundle_file_record(
    path: Path,
    *,
    base: Path,
    manifest_entries: list[dict],
    downloaded_bytes: int,
    reused_existing: bool,
) -> dict:
    relative_path = path.relative_to(base)
    return {
        "kind": "om_coverage_bundle",
        "path": str(relative_path).replace("\\", "/"),
        "bytes": path.stat().st_size,
        "sha256": sha256_file(path),
        "entries": manifest_entries,
        "remote_content_length": None,
        "downloaded_bytes": downloaded_bytes,
        "reused_existing": reused_existing,
    }


def _write_prepared_coverage_bundle(path: Path, prepared: list[dict]) -> None:
    temp_path = path.with_name(path.name + ".tmp")
    enforce_environment_storage_guard(
        path,
        additional_bytes=sum(
            byte_range.length
            for item in prepared
            for byte_range in item["byte_ranges"]
        ),
    )
    try:
        with temp_path.open("wb") as file_obj:
            for item in prepared:
                for byte_range in item["byte_ranges"]:
                    copy_byte_range_to_file(item["source_url"], byte_range, file_obj)
        os.replace(temp_path, path)
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise


def _download_prepared_range(
    sequence: int,
    range_index: int,
    item: dict,
    byte_range: ByteRange,
    *,
    timeout: int,
) -> dict:
    payload = fetch_byte_range_with_retry(
        item["source_url"],
        byte_range,
        timeout=timeout,
        remote_content_length=item.get("remote_content_length"),
    )
    if len(payload) != byte_range.length:
        raise ValueError(
            f"downloaded range payload is {len(payload)} bytes, expected {byte_range.length}"
        )
    return {
        "sequence": sequence,
        "range_index": range_index,
        "payload": payload,
    }


def _progress_log(context: dict | None, payload: dict, *, force: bool = False) -> float:
    if not context:
        return time.monotonic()
    now = time.monotonic()
    last_emit = float(context.get("_last_emit", 0.0))
    interval = float(context.get("progress_interval_seconds", 10.0))
    if not force and now - last_emit < interval:
        return last_emit
    context["_last_emit"] = now
    elapsed = max(now - float(context.get("_started_at", now)), 0.001)
    written_bytes = int(payload.get("written_bytes", 0))
    remote_downloaded_bytes = int(payload.get("remote_downloaded_bytes", written_bytes))
    last_bytes = int(context.get("_last_progress_bytes", 0))
    last_remote_bytes = int(context.get("_last_progress_remote_bytes", 0))
    last_time = float(context.get("_last_progress_time", now))
    current_elapsed = max(now - last_time, 0.001)
    current_mib_s = max(written_bytes - last_bytes, 0) / current_elapsed / 1024 / 1024
    remote_current_mib_s = (
        max(remote_downloaded_bytes - last_remote_bytes, 0) / current_elapsed / 1024 / 1024
    )
    context["_last_progress_bytes"] = written_bytes
    context["_last_progress_remote_bytes"] = remote_downloaded_bytes
    context["_last_progress_time"] = now
    stage_text = {
        "planning": "分析下载范围",
        "downloading": "下载文件",
        "writing": "写入 OM 文件",
    }.get(str(payload.get("stage") or ""), str(payload.get("stage") or "处理中"))
    product_text = {
        "gfs013_surface": "GFS 0.13°地面层",
        "gfs025": "GFS 0.25°地面层",
        "gfs_pressure_profile": "GFS 气压层",
        "ncep_gefs025": "GEFS 0.25°降水概率",
        "ncep_gefs05": "GEFS 0.5°降水概率",
        "ecmwf_ifs025": "ECMWF IFS 0.25°",
        "ecmwf_ifs025_ensemble": "ECMWF IFS 0.25°集合降水概率",
        "cams_global": "CAMS 全球空气质量",
        "cams_global_greenhouse_gases": "CAMS 温室气体",
    }.get(str(context.get("product") or ""), str(context.get("product") or "未知产品"))
    interval_growth = max(remote_downloaded_bytes - last_remote_bytes, 0)
    print(
        "｜".join(
            [
                f"阶段：{stage_text}",
                f"产品：{product_text}",
                f"批次：{context.get('coverage_id') or '-'}",
                f"近一分钟增长：{interval_growth / 1024 / 1024:.1f} MiB",
                f"速度：{remote_current_mib_s:.2f} MiB/s",
                f"累计下载：{remote_downloaded_bytes / 1024 / 1024:.1f} MiB",
            ]
        ),
        file=sys.stderr,
        flush=True,
    )
    return now


def _same_source_url(prepared: list[dict]) -> str | None:
    urls = {str(item["source_url"]) for item in prepared}
    if len(urls) != 1:
        return None
    return next(iter(urls))


def _source_prefix_bytes(
    prepared: list[dict],
    *,
    object_fetch_mode: str,
    object_fetch_max_multiplier: float,
    object_fetch_min_ranges: int,
) -> int | None:
    if object_fetch_mode != "prefix":
        return None
    if object_fetch_max_multiplier <= 0:
        raise ValueError("object_fetch_max_multiplier must be positive")
    if object_fetch_min_ranges < 1:
        raise ValueError("object_fetch_min_ranges must be at least 1")
    if not prepared or _same_source_url(prepared) is None:
        return None

    all_ranges = [byte_range for item in prepared for byte_range in item["byte_ranges"]]
    if not all_ranges:
        return None
    remote_lengths = {
        int(item["remote_content_length"])
        for item in prepared
        if item.get("remote_content_length") is not None
    }
    if len(remote_lengths) != 1:
        return None

    needed_bytes = sum(item.length for item in all_ranges)
    prefix_bytes = max(item.end for item in all_ranges) + 1
    remote_content_length = next(iter(remote_lengths))
    if prefix_bytes > remote_content_length:
        return None
    if object_fetch_mode == "prefix":
        return prefix_bytes
    return None


def _merged_object_range_requests(
    prepared: list[dict],
    *,
    merge_gap: int,
    max_multiplier: float,
    min_ranges: int,
    max_bytes: int | None,
) -> list[dict] | None:
    if merge_gap < 0:
        raise ValueError("object_range_merge_gap must be non-negative")
    if max_multiplier <= 0:
        raise ValueError("object_range_max_multiplier must be positive")
    if min_ranges < 1:
        raise ValueError("object_range_min_ranges must be at least 1")
    if max_bytes is not None and max_bytes <= 0:
        raise ValueError("object_range_max_bytes must be positive")
    source_url = _same_source_url(prepared)
    if source_url is None:
        return None

    remote_lengths = {
        int(item["remote_content_length"])
        for item in prepared
        if item.get("remote_content_length") is not None
    }
    remote_content_length = next(iter(remote_lengths)) if len(remote_lengths) == 1 else None
    segments = []
    for fallback_sequence, item in enumerate(prepared):
        sequence = int(item.get("sequence", fallback_sequence))
        for range_index, byte_range in enumerate(item["byte_ranges"]):
            if remote_content_length is not None and byte_range.end >= remote_content_length:
                raise ValueError("byte range exceeds remote content length")
            segments.append(
                {
                    "sequence": sequence,
                    "range_index": range_index,
                    "byte_range": byte_range,
                }
            )
    if len(segments) < min_ranges:
        return None

    merged: list[dict] = []
    current: dict | None = None
    for segment in sorted(
        segments,
        key=lambda item: (item["byte_range"].start, item["byte_range"].end),
    ):
        byte_range = segment["byte_range"]
        if current is None:
            current = {
                "byte_range": ByteRange(byte_range.start, byte_range.end),
                "segments": [segment],
                "needed_bytes": byte_range.length,
            }
            continue
        current_range = current["byte_range"]
        candidate_end = max(current_range.end, byte_range.end)
        candidate_range = ByteRange(current_range.start, candidate_end)
        candidate_needed = int(current["needed_bytes"]) + byte_range.length
        gap = byte_range.start - current_range.end - 1
        can_merge = (
            gap <= merge_gap
            and candidate_range.length <= candidate_needed * max_multiplier
            and (max_bytes is None or candidate_range.length <= max_bytes)
        )
        if can_merge:
            current["byte_range"] = candidate_range
            current["segments"].append(segment)
            current["needed_bytes"] = candidate_needed
        else:
            merged.append(current)
            current = {
                "byte_range": ByteRange(byte_range.start, byte_range.end),
                "segments": [segment],
                "needed_bytes": byte_range.length,
            }
    if current is not None:
        merged.append(current)
    if len(merged) >= len(segments):
        return None
    for item in merged:
        item["source_url"] = source_url
        item["remote_content_length"] = remote_content_length
    return merged


def _download_merged_object_range(
    item: dict,
    *,
    timeout: int,
) -> dict:
    byte_range = item["byte_range"]
    payload = fetch_byte_range_with_retry(
        item["source_url"],
        byte_range,
        timeout=timeout,
        remote_content_length=item.get("remote_content_length"),
    )
    if len(payload) != byte_range.length:
        raise ValueError(
            f"downloaded range payload is {len(payload)} bytes, expected {byte_range.length}"
        )
    return {
        "byte_range": byte_range,
        "payload": payload,
        "segments": item["segments"],
    }


def _download_prepared_group_ranges(
    file_obj,
    path: Path,
    prepared: list[dict],
    *,
    range_workers: int,
    timeout: int,
    progress_context: dict | None,
    counters: dict[str, int],
    object_range_merge_gap: int,
    object_range_max_multiplier: float,
    object_range_min_ranges: int,
    object_range_max_bytes: int | None,
    enable_object_range_merge: bool,
) -> None:
    if not prepared:
        return
    states: dict[int, dict] = {
        sequence: {
            "item": item,
            "payloads": [b""] * len(item["byte_ranges"]),
            "remaining": len(item["byte_ranges"]),
        }
        for sequence, item in enumerate(prepared)
    }
    with ThreadPoolExecutor(max_workers=range_workers) as executor:
        merged_requests = None
        if enable_object_range_merge:
            merged_requests = _merged_object_range_requests(
                prepared,
                merge_gap=object_range_merge_gap,
                max_multiplier=object_range_max_multiplier,
                min_ranges=object_range_min_ranges,
                max_bytes=object_range_max_bytes,
            )
        futures = {}
        if merged_requests is not None:
            for item in merged_requests:
                futures[executor.submit(_download_merged_object_range, item, timeout=timeout)] = item
        else:
            for sequence, item in enumerate(prepared):
                if not item["byte_ranges"]:
                    continue
                for range_index, byte_range in enumerate(item["byte_ranges"]):
                    futures[
                        executor.submit(
                            _download_prepared_range,
                            sequence,
                            range_index,
                            item,
                            byte_range,
                            timeout=timeout,
                        )
                    ] = (sequence, range_index)
        while futures:
            done, _pending = wait(futures, return_when=FIRST_COMPLETED)
            for future in done:
                futures.pop(future, None)
                result = future.result()
                payload = result["payload"]
                counters["remote_downloaded_bytes"] += len(payload)
                counters["downloaded_http_ranges"] += 1
                if "segments" in result:
                    source_start = int(result["byte_range"].start)
                    for segment in result["segments"]:
                        byte_range = segment["byte_range"]
                        start = byte_range.start - source_start
                        end = start + byte_range.length
                        segment_payload = payload[start:end]
                        if len(segment_payload) != byte_range.length:
                            raise ValueError(
                                f"merged range slice is {len(segment_payload)} bytes, expected {byte_range.length}"
                            )
                        state = states[int(segment["sequence"])]
                        state["payloads"][int(segment["range_index"])] = segment_payload
                        state["remaining"] -= 1
                        counters["downloaded_ranges"] += 1
                else:
                    counters["downloaded_ranges"] += 1
                    state = states[int(result["sequence"])]
                    state["payloads"][int(result["range_index"])] = payload
                    state["remaining"] -= 1
            _progress_log(
                progress_context,
                {
                    "stage": "downloading",
                    **counters,
                    "active_workers": len(futures),
                    "range_workers": range_workers,
                    "queue_size": 0,
                    "pending_futures": len(futures),
                },
            )

    for sequence in range(len(prepared)):
        state = states[sequence]
        if state["remaining"] != 0:
            raise ValueError("range downloads did not complete for prepared entry")
        payload = b"".join(state["payloads"])
        expected = int(state["item"]["manifest"]["bundle_bytes"])
        if len(payload) != expected:
            raise ValueError(f"downloaded entry payload is {len(payload)} bytes, expected {expected}")
        enforce_environment_storage_guard(path, additional_bytes=len(payload))
        file_obj.write(payload)
        counters["written_bytes"] += len(payload)
        counters["manifest_entries"].append(state["item"]["manifest"])
        _progress_log(
            progress_context,
            {
                "stage": "writing",
                **counters,
                "active_workers": 0,
                "range_workers": range_workers,
                "queue_size": 0,
                "pending_futures": 0,
            },
        )


def _download_prepared_group_prefix(
    file_obj,
    path: Path,
    prepared: list[dict],
    *,
    prefix_bytes: int,
    group_index: int,
    timeout: int,
    progress_context: dict | None,
    counters: dict[str, int],
) -> None:
    source_url = _same_source_url(prepared)
    if source_url is None:
        raise ValueError("object prefix download requires one source URL")
    remote_content_length = prepared[0].get("remote_content_length")
    source_temp = path.with_name(f".{path.name}.source-{group_index}.tmp")
    try:
        enforce_environment_storage_guard(path, additional_bytes=prefix_bytes)
        fetched = fetch_http_prefix_to_file(
            source_url,
            source_temp,
            prefix_bytes,
            timeout=timeout,
            expected_content_length=remote_content_length,
        )
        counters["downloaded_http_ranges"] += 1
        counters["downloaded_ranges"] += sum(len(item["byte_ranges"]) for item in prepared)
        counters["remote_downloaded_bytes"] += fetched
        _progress_log(
            progress_context,
            {
                "stage": "downloading",
                **counters,
                "active_workers": 1,
                "queue_size": 0,
                "pending_futures": 0,
            },
        )
        with source_temp.open("rb") as source_file:
            for item in prepared:
                entry_bytes = 0
                for byte_range in item["byte_ranges"]:
                    source_file.seek(byte_range.start)
                    payload = source_file.read(byte_range.length)
                    if len(payload) != byte_range.length:
                        raise ValueError(
                            f"local prefix read returned {len(payload)} bytes, expected {byte_range.length}"
                        )
                    enforce_environment_storage_guard(path, additional_bytes=len(payload))
                    file_obj.write(payload)
                    entry_bytes += len(payload)
                expected = int(item["manifest"]["bundle_bytes"])
                if entry_bytes != expected:
                    raise ValueError(
                        f"downloaded entry payload is {entry_bytes} bytes, expected {expected}"
                    )
                counters["written_bytes"] += entry_bytes
                counters["manifest_entries"].append(item["manifest"])
                _progress_log(
                    progress_context,
                    {
                        "stage": "writing",
                        **counters,
                        "active_workers": 0,
                        "queue_size": 0,
                        "pending_futures": 0,
                    },
                )
    finally:
        source_temp.unlink(missing_ok=True)


def _write_prepared_coverage_bundle_merged_streaming(
    path: Path,
    entries: Iterable[dict],
    *,
    download_workers: int,
    range_workers: int | None,
    timeout: int,
    progress_context: dict | None,
    object_range_merge_gap: int,
    object_range_max_multiplier: float,
    object_range_min_ranges: int,
    object_range_max_bytes: int | None,
) -> tuple[list[dict], int, int]:
    if download_workers < 1:
        raise ValueError("download_workers must be at least 1")
    if range_workers is None:
        range_workers = download_workers
    if range_workers < 1:
        raise ValueError("range_workers must be at least 1")

    temp_path = path.with_name(path.name + ".tmp")
    offset = 0
    next_sequence = 0
    states: dict[int, dict] = {}
    completed_ready: dict[int, dict] = {}
    manifest_by_sequence: dict[int, dict] = {}
    pending = {}
    max_pending_http = max(1, range_workers)
    counters: dict[str, int] = {
        "planned_entries": 0,
        "planned_ranges": 0,
        "downloaded_ranges": 0,
        "planned_http_ranges": 0,
        "downloaded_http_ranges": 0,
        "written_bytes": 0,
        "remote_downloaded_bytes": 0,
    }

    if progress_context is not None:
        progress_context.setdefault("_started_at", time.monotonic())
        progress_context.setdefault("range_workers", range_workers)

    def submit_prepared_item(item: dict, sequence: int) -> None:
        byte_ranges = list(item["byte_ranges"])
        states[sequence] = {
            "item": item,
            "payloads": [b""] * len(byte_ranges),
            "remaining": len(byte_ranges),
        }
        if not byte_ranges:
            completed_ready[sequence] = {
                "sequence": sequence,
                "item": item,
                "payload": b"",
            }

    def submit_group(executor: ThreadPoolExecutor, raw_group: list[dict]) -> None:
        nonlocal offset, next_sequence
        if not raw_group:
            return
        prepared: list[dict] = []
        for entry in raw_group:
            item, offset = _prepare_coverage_bundle_entry(entry, bundle_offset=offset)
            item["sequence"] = next_sequence
            submit_prepared_item(item, next_sequence)
            prepared.append(item)
            next_sequence += 1

        group_ranges = sum(len(item["byte_ranges"]) for item in prepared)
        merged_requests = _merged_object_range_requests(
            prepared,
            merge_gap=object_range_merge_gap,
            max_multiplier=object_range_max_multiplier,
            min_ranges=object_range_min_ranges,
            max_bytes=object_range_max_bytes,
        )
        counters["planned_entries"] += len(prepared)
        counters["planned_ranges"] += group_ranges
        counters["planned_http_ranges"] += (
            len(merged_requests) if merged_requests is not None else group_ranges
        )

        if merged_requests is not None:
            for request in merged_requests:
                pending[
                    executor.submit(
                        _download_merged_object_range,
                        request,
                        timeout=timeout,
                    )
                ] = request
        else:
            for item in prepared:
                sequence = int(item["sequence"])
                for range_index, byte_range in enumerate(item["byte_ranges"]):
                    pending[
                        executor.submit(
                            _download_prepared_range,
                            sequence,
                            range_index,
                            item,
                            byte_range,
                            timeout=timeout,
                        )
                    ] = (sequence, range_index)

        _progress_log(
            progress_context,
            {
                "stage": "planning",
                **counters,
                "active_workers": len(pending),
                "range_workers": range_workers,
                "queue_size": len(completed_ready),
                "pending_futures": len(pending),
            },
        )

    def collect_done(done) -> None:
        for future in done:
            pending.pop(future, None)
            result = future.result()
            payload = result["payload"]
            counters["remote_downloaded_bytes"] += len(payload)
            counters["downloaded_http_ranges"] += 1
            if "segments" in result:
                source_start = int(result["byte_range"].start)
                for segment in result["segments"]:
                    byte_range = segment["byte_range"]
                    start = byte_range.start - source_start
                    end = start + byte_range.length
                    segment_payload = payload[start:end]
                    if len(segment_payload) != byte_range.length:
                        raise ValueError(
                            f"merged range slice is {len(segment_payload)} bytes, expected {byte_range.length}"
                        )
                    sequence = int(segment["sequence"])
                    state = states[sequence]
                    state["payloads"][int(segment["range_index"])] = segment_payload
                    state["remaining"] -= 1
                    counters["downloaded_ranges"] += 1
                    if state["remaining"] == 0:
                        mark_sequence_complete(sequence, state)
            else:
                sequence = int(result["sequence"])
                state = states[sequence]
                state["payloads"][int(result["range_index"])] = payload
                state["remaining"] -= 1
                counters["downloaded_ranges"] += 1
                if state["remaining"] == 0:
                    mark_sequence_complete(sequence, state)
        _progress_log(
            progress_context,
            {
                "stage": "downloading",
                **counters,
                "active_workers": len(pending),
                "range_workers": range_workers,
                "queue_size": len(completed_ready),
                "pending_futures": len(pending),
            },
        )

    def mark_sequence_complete(sequence: int, state: dict) -> None:
        payload = b"".join(state["payloads"])
        expected = int(state["item"]["manifest"]["bundle_bytes"])
        if len(payload) != expected:
            raise ValueError(f"downloaded entry payload is {len(payload)} bytes, expected {expected}")
        completed_ready[sequence] = {
            "sequence": sequence,
            "item": state["item"],
            "payload": payload,
        }
        states.pop(sequence, None)

    def write_ready(file_obj) -> None:
        ready_sequences = sorted(completed_ready)
        for sequence in ready_sequences:
            result = completed_ready.pop(sequence)
            payload = result["payload"]
            manifest = result["item"]["manifest"]
            bundle_offset = int(manifest["bundle_offset"])
            expected = int(manifest["bundle_bytes"])
            if len(payload) != expected:
                raise ValueError(f"downloaded entry payload is {len(payload)} bytes, expected {expected}")
            file_obj.seek(bundle_offset)
            enforce_environment_storage_guard(path, additional_bytes=len(payload))
            file_obj.write(payload)
            counters["written_bytes"] += len(payload)
            manifest_by_sequence[sequence] = manifest
            _progress_log(
                progress_context,
                {
                    "stage": "writing",
                    **counters,
                    "active_workers": len(pending),
                    "range_workers": range_workers,
                    "queue_size": len(completed_ready),
                    "pending_futures": len(pending),
                },
            )

    try:
        with temp_path.open("wb") as file_obj:
            with ThreadPoolExecutor(max_workers=range_workers) as executor:
                entry_iter = iter(entries)
                entries_exhausted = False
                group: list[dict] = []
                current_url: str | None = None

                while True:
                    while not entries_exhausted and len(pending) < max_pending_http:
                        try:
                            entry = next(entry_iter)
                        except StopIteration:
                            entries_exhausted = True
                            break
                        entry_url = str(entry["source_url"])
                        if group and entry_url != current_url:
                            submit_group(executor, group)
                            group = []
                            write_ready(file_obj)
                            if len(pending) >= max_pending_http:
                                current_url = entry_url
                                group.append(entry)
                                break
                        current_url = entry_url
                        group.append(entry)

                    if entries_exhausted and group:
                        submit_group(executor, group)
                        group = []

                    write_ready(file_obj)
                    if pending:
                        done, _ = wait(pending, return_when=FIRST_COMPLETED)
                        collect_done(done)
                        write_ready(file_obj)
                        continue
                    if entries_exhausted:
                        break

                while pending:
                    done, _ = wait(pending, return_when=FIRST_COMPLETED)
                    collect_done(done)
                    write_ready(file_obj)

            write_ready(file_obj)
            file_obj.truncate(offset)

        if not manifest_by_sequence:
            raise ValueError("at least one OM coverage bundle entry is required")
        missing_sequences = [sequence for sequence in range(next_sequence) if sequence not in manifest_by_sequence]
        if missing_sequences:
            raise ValueError(f"missing written bundle entries: {missing_sequences[:10]}")
        manifest_entries = [manifest_by_sequence[sequence] for sequence in range(next_sequence)]
        written_bytes = int(counters["written_bytes"])
        if written_bytes != offset:
            raise ValueError(f"written bundle size is {written_bytes}, expected {offset}")
        if temp_path.stat().st_size != offset:
            raise ValueError(f"temporary bundle size is {temp_path.stat().st_size}, expected {offset}")
        os.replace(temp_path, path)
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise

    _progress_log(
        progress_context,
        {
            "stage": "writing",
            **counters,
            "active_workers": 0,
            "range_workers": range_workers,
            "queue_size": 0,
            "pending_futures": 0,
        },
        force=True,
    )
    return manifest_entries, offset, int(counters["remote_downloaded_bytes"])


def _write_prepared_coverage_bundle_adaptive(
    path: Path,
    entries: Iterable[dict],
    *,
    download_workers: int,
    range_workers: int | None,
    timeout: int,
    progress_context: dict | None,
    object_fetch_mode: str,
    object_fetch_max_multiplier: float,
    object_fetch_min_ranges: int,
    object_range_merge_gap: int,
    object_range_max_multiplier: float,
    object_range_min_ranges: int,
    object_range_max_bytes: int | None,
) -> tuple[list[dict], int, int]:
    if object_fetch_mode == "auto":
        return _write_prepared_coverage_bundle_merged_streaming(
            path,
            entries,
            download_workers=download_workers,
            range_workers=range_workers,
            timeout=timeout,
            progress_context=progress_context,
            object_range_merge_gap=object_range_merge_gap,
            object_range_max_multiplier=object_range_max_multiplier,
            object_range_min_ranges=object_range_min_ranges,
            object_range_max_bytes=object_range_max_bytes,
        )

    if download_workers < 1:
        raise ValueError("download_workers must be at least 1")
    if range_workers is None:
        range_workers = download_workers
    if range_workers < 1:
        raise ValueError("range_workers must be at least 1")
    if object_fetch_mode not in {"auto", "prefix"}:
        raise ValueError("object_fetch_mode must be range, auto, or prefix")

    temp_path = path.with_name(path.name + ".tmp")
    offset = 0
    counters: dict[str, int] = {
        "planned_entries": 0,
        "planned_ranges": 0,
        "downloaded_ranges": 0,
        "planned_http_ranges": 0,
        "downloaded_http_ranges": 0,
        "written_bytes": 0,
        "remote_downloaded_bytes": 0,
        "manifest_entries": [],  # type: ignore[dict-item]
    }
    if progress_context is not None:
        progress_context.setdefault("_started_at", time.monotonic())
        progress_context.setdefault("range_workers", range_workers)

    def process_group(file_obj, raw_group: list[dict], group_index: int) -> None:
        nonlocal offset
        prepared: list[dict] = []
        for entry in raw_group:
            item, offset = _prepare_coverage_bundle_entry(entry, bundle_offset=offset)
            prepared.append(item)
        counters["planned_entries"] += len(prepared)
        group_ranges = sum(len(item["byte_ranges"]) for item in prepared)
        counters["planned_ranges"] += group_ranges
        prefix_bytes = _source_prefix_bytes(
            prepared,
            object_fetch_mode=object_fetch_mode,
            object_fetch_max_multiplier=object_fetch_max_multiplier,
            object_fetch_min_ranges=object_fetch_min_ranges,
        )
        merged_requests = None
        if prefix_bytes is None and object_fetch_mode == "auto":
            merged_requests = _merged_object_range_requests(
                prepared,
                merge_gap=object_range_merge_gap,
                max_multiplier=object_range_max_multiplier,
                min_ranges=object_range_min_ranges,
                max_bytes=object_range_max_bytes,
            )
        if prefix_bytes is not None:
            counters["planned_http_ranges"] += 1
        elif merged_requests is not None:
            counters["planned_http_ranges"] += len(merged_requests)
        else:
            counters["planned_http_ranges"] += group_ranges
        _progress_log(
            progress_context,
            {
                "stage": "planning",
                **counters,
                "active_workers": 0,
                "range_workers": range_workers,
                "queue_size": 0,
                "pending_futures": 0,
            },
        )
        if prefix_bytes is not None:
            _download_prepared_group_prefix(
                file_obj,
                path,
                prepared,
                prefix_bytes=prefix_bytes,
                group_index=group_index,
                timeout=timeout,
                progress_context=progress_context,
                counters=counters,
            )
            return
        _download_prepared_group_ranges(
            file_obj,
            path,
            prepared,
            range_workers=range_workers,
            timeout=timeout,
            progress_context=progress_context,
            counters=counters,
            object_range_merge_gap=object_range_merge_gap,
            object_range_max_multiplier=object_range_max_multiplier,
            object_range_min_ranges=object_range_min_ranges,
            object_range_max_bytes=object_range_max_bytes,
            enable_object_range_merge=object_fetch_mode == "auto",
        )

    try:
        with temp_path.open("wb") as file_obj:
            group: list[dict] = []
            current_url: str | None = None
            group_index = 0
            for entry in entries:
                entry_url = str(entry["source_url"])
                if group and entry_url != current_url:
                    process_group(file_obj, group, group_index)
                    group_index += 1
                    group = []
                current_url = entry_url
                group.append(entry)
            if group:
                process_group(file_obj, group, group_index)
        manifest_entries = counters["manifest_entries"]
        if not manifest_entries:
            raise ValueError("at least one OM coverage bundle entry is required")
        written_bytes = int(counters["written_bytes"])
        if written_bytes != offset:
            raise ValueError(f"written bundle size is {written_bytes}, expected {offset}")
        os.replace(temp_path, path)
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise

    _progress_log(
        progress_context,
        {
            "stage": "writing",
            **counters,
            "active_workers": 0,
            "range_workers": range_workers,
            "queue_size": 0,
            "pending_futures": 0,
        },
        force=True,
    )
    return manifest_entries, offset, int(counters["remote_downloaded_bytes"])


def _write_prepared_coverage_bundle_streaming(
    path: Path,
    entries: Iterable[dict],
    *,
    download_workers: int,
    range_workers: int | None,
    timeout: int,
    progress_context: dict | None,
) -> tuple[list[dict], int]:
    if download_workers < 1:
        raise ValueError("download_workers must be at least 1")
    if range_workers is None:
        range_workers = download_workers
    if range_workers < 1:
        raise ValueError("range_workers must be at least 1")

    temp_path = path.with_name(path.name + ".tmp")
    manifest_entries: list[dict] = []
    offset = 0
    planned_entries = 0
    planned_ranges = 0
    downloaded_ranges = 0
    written_bytes = 0
    next_sequence_to_write = 0
    completed_by_sequence: dict[int, dict] = {}
    entry_states: dict[int, dict] = {}
    pending = {}
    max_pending_ranges = max(1, range_workers * 4)

    if progress_context is not None:
        progress_context.setdefault("_started_at", time.monotonic())
        progress_context.setdefault("range_workers", range_workers)

    def write_ready(file_obj) -> None:
        nonlocal next_sequence_to_write, written_bytes
        while next_sequence_to_write in completed_by_sequence:
            result = completed_by_sequence.pop(next_sequence_to_write)
            enforce_environment_storage_guard(
                path,
                additional_bytes=len(result["payload"]),
            )
            file_obj.write(result["payload"])
            written_bytes += len(result["payload"])
            manifest_entries.append(result["item"]["manifest"])
            next_sequence_to_write += 1
            _progress_log(
                progress_context,
                {
                    "stage": "writing",
                    "planned_entries": planned_entries,
                    "planned_ranges": planned_ranges,
                    "downloaded_ranges": downloaded_ranges,
                    "written_bytes": written_bytes,
                    "active_workers": len(pending),
                    "range_workers": range_workers,
                    "queue_size": len(completed_by_sequence),
                    "pending_futures": len(pending),
                },
            )

    def collect_done(done) -> None:
        nonlocal downloaded_ranges
        for future in done:
            pending.pop(future, None)
            result = future.result()
            downloaded_ranges += 1
            sequence = int(result["sequence"])
            state = entry_states[sequence]
            state["payloads"][int(result["range_index"])] = result["payload"]
            state["remaining"] -= 1
            if state["remaining"] == 0:
                payloads = state["payloads"]
                payload = b"".join(payloads)
                expected = int(state["item"]["manifest"]["bundle_bytes"])
                if len(payload) != expected:
                    raise ValueError(
                        f"downloaded entry payload is {len(payload)} bytes, expected {expected}"
                    )
                completed_by_sequence[sequence] = {
                    "sequence": sequence,
                    "item": state["item"],
                    "payload": payload,
                }
                entry_states.pop(sequence, None)
        _progress_log(
            progress_context,
            {
                "stage": "downloading",
                "planned_entries": planned_entries,
                "planned_ranges": planned_ranges,
                "downloaded_ranges": downloaded_ranges,
                "written_bytes": written_bytes,
                "active_workers": len(pending),
                "range_workers": range_workers,
                "queue_size": len(completed_by_sequence),
                "pending_futures": len(pending),
            },
        )

    try:
        with temp_path.open("wb") as file_obj:
            entry_iter = iter(entries)
            entries_exhausted = False
            with ThreadPoolExecutor(max_workers=range_workers) as executor:
                sequence = 0

                def submit_entry(entry: dict) -> None:
                    nonlocal offset, planned_entries, planned_ranges, sequence
                    item, offset = _prepare_coverage_bundle_entry(entry, bundle_offset=offset)
                    byte_ranges = list(item["byte_ranges"])
                    planned_entries += 1
                    planned_ranges += len(byte_ranges)
                    entry_states[sequence] = {
                        "item": item,
                        "payloads": [b""] * len(byte_ranges),
                        "remaining": len(byte_ranges),
                    }
                    if not byte_ranges:
                        completed_by_sequence[sequence] = {
                            "sequence": sequence,
                            "item": item,
                            "payload": b"",
                        }
                    for range_index, byte_range in enumerate(byte_ranges):
                        pending[
                            executor.submit(
                                _download_prepared_range,
                                sequence,
                                range_index,
                                item,
                                byte_range,
                                timeout=timeout,
                            )
                        ] = (sequence, range_index)
                    sequence += 1

                while True:
                    while (
                        not entries_exhausted
                        and len(pending) < max_pending_ranges
                    ):
                        try:
                            entry = next(entry_iter)
                        except StopIteration:
                            entries_exhausted = True
                            break
                        submit_entry(entry)
                        _progress_log(
                            progress_context,
                            {
                                "stage": "planning",
                                "planned_entries": planned_entries,
                                "planned_ranges": planned_ranges,
                                "downloaded_ranges": downloaded_ranges,
                                "written_bytes": written_bytes,
                                "active_workers": len(pending),
                                "range_workers": range_workers,
                                "queue_size": len(completed_by_sequence),
                                "pending_futures": len(pending),
                            },
                        )

                    write_ready(file_obj)
                    if pending:
                        done, _ = wait(pending, return_when=FIRST_COMPLETED)
                        collect_done(done)
                        write_ready(file_obj)
                        continue
                    if entries_exhausted:
                        break

                while pending:
                    done, _pending = wait(pending, return_when=FIRST_COMPLETED)
                    collect_done(done)
                    write_ready(file_obj)

            write_ready(file_obj)
        if not manifest_entries:
            raise ValueError("at least one OM coverage bundle entry is required")
        if written_bytes != offset:
            raise ValueError(f"written bundle size is {written_bytes}, expected {offset}")
        os.replace(temp_path, path)
    except Exception:
        temp_path.unlink(missing_ok=True)
        raise

    _progress_log(
        progress_context,
        {
            "stage": "writing",
            "planned_entries": planned_entries,
            "planned_ranges": planned_ranges,
            "downloaded_ranges": downloaded_ranges,
            "written_bytes": written_bytes,
            "active_workers": 0,
            "range_workers": range_workers,
            "queue_size": 0,
            "pending_futures": 0,
        },
        force=True,
    )
    return manifest_entries, offset


def write_om_coverage_bundle_file(
    output_root: Path,
    model: str,
    coverage_id: str,
    entries: Iterable[dict],
    *,
    download_workers: int = 1,
    range_workers: int | None = None,
    timeout: int = 90,
    progress_context: dict | None = None,
    object_fetch_mode: str = "range",
    object_fetch_max_multiplier: float = 3.0,
    object_fetch_min_ranges: int = 16,
    object_range_merge_gap: int = 16 * 1024 * 1024,
    object_range_max_multiplier: float = 2.0,
    object_range_min_ranges: int = 16,
    object_range_max_bytes: int | None = None,
) -> dict:
    directory = output_root / "published" / model / "coverages" / coverage_id
    path = directory / f"{model}.omranges"
    base = output_root / "published" / model

    directory.mkdir(parents=True, exist_ok=True)
    if object_fetch_mode == "range":
        manifest_entries, offset = _write_prepared_coverage_bundle_streaming(
            path,
            entries,
            download_workers=download_workers,
            range_workers=range_workers,
            timeout=timeout,
            progress_context=progress_context,
        )
        downloaded_bytes = offset
    else:
        manifest_entries, offset, downloaded_bytes = _write_prepared_coverage_bundle_adaptive(
            path,
            entries,
            download_workers=download_workers,
            range_workers=range_workers,
            timeout=timeout,
            progress_context=progress_context,
            object_fetch_mode=object_fetch_mode,
            object_fetch_max_multiplier=object_fetch_max_multiplier,
            object_fetch_min_ranges=object_fetch_min_ranges,
            object_range_merge_gap=object_range_merge_gap,
            object_range_max_multiplier=object_range_max_multiplier,
            object_range_min_ranges=object_range_min_ranges,
            object_range_max_bytes=object_range_max_bytes,
        )

    return _coverage_bundle_file_record(
        path,
        base=base,
        manifest_entries=manifest_entries,
        downloaded_bytes=downloaded_bytes,
        reused_existing=False,
    )
