from __future__ import annotations

from .om_chunks import chunk_index_ranges_for_selection
from .om_inventory import OmArrayInfo
from .om_lut import (
    calculate_lut_chunk_length,
    chunk_index_ranges_to_lut_byte_ranges,
    data_byte_ranges_from_lut_offsets,
)


def _number_of_chunks(array: OmArrayInfo) -> int:
    count = 1
    for dimension, chunk in zip(array.dimensions, array.chunks):
        count *= (dimension + chunk - 1) // chunk
    return count


def _chunk_index_ranges(array: OmArrayInfo, selection_ranges: tuple[tuple[int, int], ...]) -> list[tuple[int, int]]:
    return chunk_index_ranges_for_selection(
        dimensions=array.dimensions,
        chunks=array.chunks,
        selection_ranges=selection_ranges,
    )


def plan_array_lut_byte_ranges(
    array: OmArrayInfo,
    *,
    selection_ranges: tuple[tuple[int, int], ...],
) -> list[tuple[int, int]]:
    if array.lut_offset is None or array.lut_size is None:
        raise ValueError("array metadata is missing LUT offset/size")
    chunk_ranges = _chunk_index_ranges(array, selection_ranges)
    lut_chunk_length = calculate_lut_chunk_length(
        number_of_chunks=_number_of_chunks(array),
        lut_size=array.lut_size,
    )
    return chunk_index_ranges_to_lut_byte_ranges(
        chunk_index_ranges=chunk_ranges,
        lut_offset=array.lut_offset,
        lut_chunk_length=lut_chunk_length,
    )


def plan_array_data_byte_ranges(
    array: OmArrayInfo,
    *,
    selection_ranges: tuple[tuple[int, int], ...],
    lut_offsets: list[int] | None,
    io_size_merge: int = 512,
    io_size_max: int | None = 64 * 1024,
) -> list[tuple[int, int]]:
    if lut_offsets is None:
        raise ValueError("decoded LUT offsets are required to plan OM data byte ranges")
    chunk_ranges = _chunk_index_ranges(array, selection_ranges)
    return data_byte_ranges_from_lut_offsets(
        lut_offsets=lut_offsets,
        chunk_index_ranges=chunk_ranges,
        io_size_merge=io_size_merge,
        io_size_max=io_size_max,
    )
