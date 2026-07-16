from __future__ import annotations

from typing import Any, Iterable

from .http_range import ByteRange
from .model_config import ProductConfig
from .om_inventory import OmArrayInfo, OmInventory
from .om_remote import ByteRangeSource
from .om_remote_ranges import plan_remote_array_data_byte_ranges
from .region import bounds_to_dict, grid_spec_for_openmeteo_model, padded_bounds, regular_grid_ranges


def selected_inventory_variables(product: ProductConfig, inventory: OmInventory) -> tuple[str, ...]:
    available = set(inventory.arrays)
    ordered = list(product.required_variables) + list(product.optional_variables)
    selected: list[str] = []
    seen: set[str] = set()
    for name in ordered:
        if name in available and name not in seen:
            selected.append(name)
            seen.add(name)
    return tuple(selected)


def plan_region_for_array(product: ProductConfig, array: OmArrayInfo) -> tuple[dict[str, Any], tuple[tuple[int, int], ...]]:
    grid = grid_spec_for_openmeteo_model(product.openmeteo_model, dimensions=array.dimensions)
    padded = padded_bounds(product.requested_bounds, product.bounds_padding_degrees, grid.bounds)
    spatial_range = regular_grid_ranges(grid, padded)
    selection_ranges = (
        tuple(spatial_range["y_range"]),
        tuple(spatial_range["x_range"]),
    )
    return (
        {
            "requested_bounds": bounds_to_dict(product.requested_bounds),
            "padded_bounds": bounds_to_dict(padded),
            "grid_bounds": bounds_to_dict(grid.bounds),
            "grid": grid.as_manifest(),
            "spatial_ranges": [spatial_range],
        },
        selection_ranges,
    )


def _merge_half_open_ranges(ranges: Iterable[tuple[int, int]]) -> list[tuple[int, int]]:
    sorted_ranges = sorted(ranges)
    if not sorted_ranges:
        return []
    merged = [sorted_ranges[0]]
    for start, end in sorted_ranges[1:]:
        if end <= start:
            raise ValueError("byte range end must be greater than start")
        current_start, current_end = merged[-1]
        if start <= current_end:
            merged[-1] = (current_start, max(current_end, end))
            continue
        merged.append((start, end))
    return merged


def _inclusive_byte_ranges(ranges: Iterable[tuple[int, int]]) -> list[ByteRange]:
    return [ByteRange(start, end - 1) for start, end in _merge_half_open_ranges(ranges)]


def plan_variable_range_bundle(
    source: ByteRangeSource,
    array: OmArrayInfo,
    *,
    selection_ranges: tuple[tuple[int, int], ...],
    lut_codec: str = "turbopfor",
    lut_workers: int = 1,
    io_size_merge: int = 64 * 1024,
    io_size_max: int | None = None,
) -> dict[str, Any]:
    plan = plan_remote_array_data_byte_ranges(
        source,
        array,
        selection_ranges=selection_ranges,
        lut_codec=lut_codec,
        lut_workers=lut_workers,
        io_size_merge=io_size_merge,
        io_size_max=io_size_max,
    )
    byte_ranges = _inclusive_byte_ranges(plan.lut_byte_ranges + plan.data_byte_ranges)
    return {
        "variable": array.name,
        "path": array.path,
        "selection_ranges": [list(item) for item in selection_ranges],
        "array": {
            "data_type": array.data_type,
            "compression": array.compression,
            "dimensions": list(array.dimensions),
            "chunks": list(array.chunks),
            "lut_offset": array.lut_offset,
            "lut_size": array.lut_size,
            "scale_factor": array.scale_factor,
            "add_offset": array.add_offset,
        },
        "lut_byte_ranges": [list(item) for item in plan.lut_byte_ranges],
        "data_byte_ranges": [list(item) for item in plan.data_byte_ranges],
        "lut_bytes_read": plan.lut_bytes_read,
        "byte_ranges": byte_ranges,
    }
