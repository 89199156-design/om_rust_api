from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from typing import Iterable

from .om_chunks import chunk_index_ranges_for_selection
from .om_inventory import OmArrayInfo
from .om_lut import (
    LUT_CHUNK_COUNT,
    calculate_lut_chunk_length,
    chunk_index_ranges_to_lut_byte_ranges,
    decode_plain_u64_lut,
    decode_turbopfor_u64_lut,
)
from .om_remote import ByteRangeSource


@dataclass(frozen=True)
class RemoteArrayByteRangePlan:
    lut_byte_ranges: list[tuple[int, int]]
    data_byte_ranges: list[tuple[int, int]]
    lut_bytes_read: int


def _number_of_chunks(array: OmArrayInfo) -> int:
    count = 1
    for dimension, chunk in zip(array.dimensions, array.chunks):
        count *= (dimension + chunk - 1) // chunk
    return count


def _chunk_index_ranges(
    array: OmArrayInfo,
    selection_ranges: tuple[tuple[int, int], ...],
) -> list[tuple[int, int]]:
    return chunk_index_ranges_for_selection(
        dimensions=array.dimensions,
        chunks=array.chunks,
        selection_ranges=selection_ranges,
    )


def _merge_ranges(ranges: Iterable[tuple[int, int]], *, merge_gap: int) -> list[tuple[int, int]]:
    sorted_ranges = sorted(ranges)
    if not sorted_ranges:
        return []
    merged = [sorted_ranges[0]]
    for start, end in sorted_ranges[1:]:
        current_start, current_end = merged[-1]
        if start < current_start or end < start:
            raise ValueError("range is invalid")
        if start - current_end <= merge_gap:
            merged[-1] = (current_start, max(current_end, end))
            continue
        merged.append((start, end))
    return merged


def _split_ranges(ranges: list[tuple[int, int]], *, io_size_max: int | None) -> list[tuple[int, int]]:
    if io_size_max is None:
        return ranges
    if io_size_max <= 0:
        raise ValueError("io_size_max must be positive")
    split = []
    for start, end in ranges:
        cursor = start
        while cursor < end:
            next_end = min(end, cursor + io_size_max)
            split.append((cursor, next_end))
            cursor = next_end
    return split


def _decode_lut_chunk(payload: bytes, *, count: int, lut_codec: str, lut_decoder=None) -> list[int]:
    if lut_codec == "plain":
        return decode_plain_u64_lut(payload, count=count)
    if lut_codec == "turbopfor":
        return decode_turbopfor_u64_lut(payload, count=count, decoder=lut_decoder)
    raise ValueError(f"unsupported LUT codec: {lut_codec}")


def _read_needed_lut_offsets(
    source: ByteRangeSource,
    array: OmArrayInfo,
    *,
    chunk_ranges: list[tuple[int, int]],
    lut_codec: str,
    lut_decoder=None,
    lut_workers: int = 1,
) -> tuple[dict[int, int], list[tuple[int, int]], int]:
    if array.lut_offset is None or array.lut_size is None:
        raise ValueError("array metadata is missing LUT offset/size")

    number_of_chunks = _number_of_chunks(array)
    lut_chunk_length = calculate_lut_chunk_length(
        number_of_chunks=number_of_chunks,
        lut_size=array.lut_size,
    )
    needed_lut_chunks = set()
    for start, end in chunk_ranges:
        needed_lut_chunks.update(range(start // LUT_CHUNK_COUNT, end // LUT_CHUNK_COUNT + 1))

    chunk_specs = []
    for lut_chunk_index in sorted(needed_lut_chunks):
        entry_start = lut_chunk_index * LUT_CHUNK_COUNT
        entry_count = min(LUT_CHUNK_COUNT, number_of_chunks + 1 - entry_start)
        if entry_count <= 0:
            continue
        byte_start = array.lut_offset + lut_chunk_index * lut_chunk_length
        byte_end = byte_start + lut_chunk_length
        chunk_specs.append((lut_chunk_index, entry_start, entry_count, byte_start, byte_end))

    lut_byte_ranges = _merge_ranges(
        ((byte_start, byte_end) for _index, _entry_start, _entry_count, byte_start, byte_end in chunk_specs),
        merge_gap=0,
    )

    def read_lut_range(item: tuple[int, int]) -> tuple[tuple[int, int], bytes]:
        start, end = item
        return item, source.read_range(start, end)

    if lut_workers > 1 and len(lut_byte_ranges) > 1:
        with ThreadPoolExecutor(max_workers=min(lut_workers, len(lut_byte_ranges))) as executor:
            payload_by_range = dict(executor.map(read_lut_range, lut_byte_ranges))
    else:
        payload_by_range = dict(read_lut_range(item) for item in lut_byte_ranges)

    offset_entries: dict[int, int] = {}
    bytes_read = sum(len(payload) for payload in payload_by_range.values())
    for _lut_chunk_index, entry_start, entry_count, byte_start, byte_end in chunk_specs:
        payload = None
        for range_start, range_end in lut_byte_ranges:
            if range_start <= byte_start and byte_end <= range_end:
                merged_payload = payload_by_range[(range_start, range_end)]
                payload = merged_payload[byte_start - range_start : byte_end - range_start]
                break
        if payload is None:
            raise ValueError("merged LUT byte ranges do not cover requested chunk")
        if len(payload) != lut_chunk_length:
            raise ValueError("byte range source returned incomplete LUT chunk")
        values = _decode_lut_chunk(
            payload,
            count=entry_count,
            lut_codec=lut_codec,
            lut_decoder=lut_decoder,
        )
        for local_index, value in enumerate(values):
            offset_entries[entry_start + local_index] = int(value)

    return offset_entries, lut_byte_ranges, bytes_read


def _data_ranges_from_offset_entries(
    offset_entries: dict[int, int],
    chunk_ranges: list[tuple[int, int]],
) -> list[tuple[int, int]]:
    ranges = []
    for start, end in chunk_ranges:
        for chunk_index in range(start, end):
            if chunk_index not in offset_entries or chunk_index + 1 not in offset_entries:
                raise ValueError("decoded LUT offsets do not cover selected chunk")
            data_start = offset_entries[chunk_index]
            data_end = offset_entries[chunk_index + 1]
            if data_end < data_start:
                raise ValueError("LUT offsets are not monotonic")
            if data_end > data_start:
                ranges.append((data_start, data_end))
    return ranges


def plan_remote_array_data_byte_ranges(
    source: ByteRangeSource,
    array: OmArrayInfo,
    *,
    selection_ranges: tuple[tuple[int, int], ...],
    lut_codec: str = "turbopfor",
    lut_decoder=None,
    lut_workers: int = 1,
    io_size_merge: int = 64 * 1024,
    io_size_max: int | None = None,
) -> RemoteArrayByteRangePlan:
    if io_size_merge < 0:
        raise ValueError("io_size_merge must be non-negative")

    chunk_ranges = _chunk_index_ranges(array, selection_ranges)
    offset_entries, lut_byte_ranges, lut_bytes_read = _read_needed_lut_offsets(
        source,
        array,
        chunk_ranges=chunk_ranges,
        lut_codec=lut_codec,
        lut_decoder=lut_decoder,
        lut_workers=lut_workers,
    )
    data_ranges = _data_ranges_from_offset_entries(offset_entries, chunk_ranges)
    data_ranges = _merge_ranges(data_ranges, merge_gap=io_size_merge)
    data_ranges = _split_ranges(data_ranges, io_size_max=io_size_max)
    return RemoteArrayByteRangePlan(
        lut_byte_ranges=lut_byte_ranges,
        data_byte_ranges=data_ranges,
        lut_bytes_read=lut_bytes_read,
    )
