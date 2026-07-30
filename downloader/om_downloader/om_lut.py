from __future__ import annotations

import math
import struct
from typing import Iterable

from .om_native import load_default_turbopfor_decoder


LUT_CHUNK_COUNT = 64


def calculate_lut_chunk_length(*, number_of_chunks: int, lut_size: int) -> int:
    if number_of_chunks < 0:
        raise ValueError("number_of_chunks must be non-negative")
    if lut_size <= 0:
        raise ValueError("lut_size must be positive")
    lut_chunk_count = math.ceil((number_of_chunks + 1) / LUT_CHUNK_COUNT)
    if lut_size % lut_chunk_count != 0:
        raise ValueError("lut_size is not divisible by compressed LUT chunk count")
    return lut_size // lut_chunk_count


def chunk_index_ranges_to_lut_byte_ranges(
    *,
    chunk_index_ranges: Iterable[tuple[int, int]],
    lut_offset: int,
    lut_chunk_length: int,
) -> list[tuple[int, int]]:
    if lut_offset < 0 or lut_chunk_length <= 0:
        raise ValueError("lut_offset and lut_chunk_length are invalid")

    ranges: list[tuple[int, int]] = []
    for start, end in chunk_index_ranges:
        if start < 0 or end <= start:
            raise ValueError("chunk index range is invalid")
        # Include chunk end offset, so chunk [start, end) needs LUT entries start..end.
        first_lut_chunk = start // LUT_CHUNK_COUNT
        last_lut_chunk = end // LUT_CHUNK_COUNT
        ranges.append(
            (
                lut_offset + first_lut_chunk * lut_chunk_length,
                lut_offset + (last_lut_chunk + 1) * lut_chunk_length,
            )
        )
    return _merge_ranges(ranges, merge_gap=0)


def decode_plain_u64_lut(payload: bytes, *, count: int) -> list[int]:
    expected = count * 8
    if len(payload) < expected:
        raise ValueError("plain LUT payload is shorter than requested count")
    return list(struct.unpack("<" + "Q" * count, payload[:expected]))


def decode_turbopfor_u64_lut(_payload: bytes, *, count: int, decoder=None) -> list[int]:
    if count <= 0:
        return []
    decoder = decoder or load_default_turbopfor_decoder()
    values, _consumed = decoder.decode_u64_delta0(_payload, count=count)
    return values


def _merge_ranges(ranges: Iterable[tuple[int, int]], *, merge_gap: int) -> list[tuple[int, int]]:
    sorted_ranges = sorted(ranges)
    if not sorted_ranges:
        return []
    merged: list[tuple[int, int]] = []
    current_start, current_end = sorted_ranges[0]
    for start, end in sorted_ranges[1:]:
        if start < current_start or end < start:
            raise ValueError("range is invalid")
        if start - current_end <= merge_gap:
            current_end = max(current_end, end)
            continue
        merged.append((current_start, current_end))
        current_start, current_end = start, end
    merged.append((current_start, current_end))
    return merged


def data_byte_ranges_from_lut_offsets(
    *,
    lut_offsets: list[int],
    chunk_index_ranges: Iterable[tuple[int, int]],
    io_size_merge: int = 0,
    io_size_max: int | None = None,
) -> list[tuple[int, int]]:
    if io_size_merge < 0:
        raise ValueError("io_size_merge must be non-negative")
    ranges: list[tuple[int, int]] = []
    for start, end in chunk_index_ranges:
        if start < 0 or end <= start:
            raise ValueError("chunk index range is invalid")
        if end >= len(lut_offsets):
            raise ValueError("chunk index range requires LUT offsets outside payload")
        data_start = lut_offsets[start]
        data_end = lut_offsets[end]
        if data_end < data_start:
            raise ValueError("LUT offsets are not monotonic")
        if data_end > data_start:
            ranges.append((data_start, data_end))

    merged = _merge_ranges(ranges, merge_gap=io_size_merge)
    if io_size_max is None:
        return merged

    split_ranges: list[tuple[int, int]] = []
    for start, end in merged:
        cursor = start
        while cursor < end:
            next_end = min(end, cursor + io_size_max)
            split_ranges.append((cursor, next_end))
            cursor = next_end
    return split_ranges
