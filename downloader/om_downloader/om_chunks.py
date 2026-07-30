from __future__ import annotations

import math
from itertools import product


def _validate_ranges(
    dimensions: tuple[int, ...],
    chunks: tuple[int, ...],
    selection_ranges: tuple[tuple[int, int], ...],
) -> None:
    if not (len(dimensions) == len(chunks) == len(selection_ranges)):
        raise ValueError("dimensions, chunks, and selection_ranges must have the same length")
    for dimension, chunk, selected in zip(dimensions, chunks, selection_ranges):
        start, end = selected
        if dimension <= 0 or chunk <= 0:
            raise ValueError("dimensions and chunks must be positive")
        if start < 0 or end > dimension or start >= end:
            raise ValueError("selection range is outside dimensions")


def _chunk_counts(dimensions: tuple[int, ...], chunks: tuple[int, ...]) -> tuple[int, ...]:
    return tuple(math.ceil(dimension / chunk) for dimension, chunk in zip(dimensions, chunks))


def _flatten_chunk_index(coordinates: tuple[int, ...], counts: tuple[int, ...]) -> int:
    index = 0
    for coordinate, count in zip(coordinates, counts):
        if coordinate < 0 or coordinate >= count:
            raise ValueError("chunk coordinate outside chunk counts")
        index = index * count + coordinate
    return index


def chunk_indices_for_selection(
    *,
    dimensions: tuple[int, ...],
    chunks: tuple[int, ...],
    selection_ranges: tuple[tuple[int, int], ...],
) -> list[int]:
    _validate_ranges(dimensions, chunks, selection_ranges)
    counts = _chunk_counts(dimensions, chunks)
    chunk_ranges = []
    for selected, chunk in zip(selection_ranges, chunks):
        start, end = selected
        chunk_start = start // chunk
        chunk_end = (end - 1) // chunk + 1
        chunk_ranges.append(range(chunk_start, chunk_end))
    indices = [
        _flatten_chunk_index(tuple(coordinates), counts)
        for coordinates in product(*chunk_ranges)
    ]
    return sorted(indices)


def _group_contiguous(indices: list[int]) -> list[tuple[int, int]]:
    if not indices:
        return []
    groups: list[tuple[int, int]] = []
    start = indices[0]
    previous = indices[0]
    for item in indices[1:]:
        if item == previous + 1:
            previous = item
            continue
        groups.append((start, previous + 1))
        start = item
        previous = item
    groups.append((start, previous + 1))
    return groups


def chunk_index_ranges_for_selection(
    *,
    dimensions: tuple[int, ...],
    chunks: tuple[int, ...],
    selection_ranges: tuple[tuple[int, int], ...],
) -> list[tuple[int, int]]:
    return _group_contiguous(
        chunk_indices_for_selection(
            dimensions=dimensions,
            chunks=chunks,
            selection_ranges=selection_ranges,
        )
    )
