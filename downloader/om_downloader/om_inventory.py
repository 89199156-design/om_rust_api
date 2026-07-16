from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re

from .om_format import collect_array_variables, parse_om_file


PRESSURE_LEVEL_PATTERN = re.compile(r"(?:^|_)(\d{2,4})hpa(?:_|$)", re.IGNORECASE)


@dataclass(frozen=True)
class OmArrayInfo:
    name: str
    path: str
    data_type: int
    compression: int
    dimensions: tuple[int, ...]
    chunks: tuple[int, ...]
    lut_offset: int | None
    lut_size: int | None
    scale_factor: float | None
    add_offset: float | None


@dataclass(frozen=True)
class OmInventory:
    arrays: dict[str, OmArrayInfo]
    pressure_levels_hpa: list[int]

    @property
    def available_variables(self) -> tuple[str, ...]:
        return tuple(sorted(self.arrays))


def infer_pressure_levels_hpa(variable_names: list[str] | tuple[str, ...]) -> list[int]:
    levels = set()
    for name in variable_names:
        match = PRESSURE_LEVEL_PATTERN.search(name)
        if match:
            levels.add(int(match.group(1)))
    return sorted(levels, reverse=True)


def inventory_from_root(root) -> OmInventory:
    raw_arrays = collect_array_variables(root)
    arrays = {
        name: OmArrayInfo(
            name=name,
            path=str(payload["path"]),
            data_type=int(payload["data_type"]),
            compression=int(payload["compression"]),
            dimensions=tuple(int(item) for item in payload["dimensions"]),
            chunks=tuple(int(item) for item in payload["chunks"]),
            lut_offset=int(payload["lut_offset"]) if payload["lut_offset"] is not None else None,
            lut_size=int(payload["lut_size"]) if payload["lut_size"] is not None else None,
            scale_factor=float(payload["scale_factor"]) if payload["scale_factor"] is not None else None,
            add_offset=float(payload["add_offset"]) if payload["add_offset"] is not None else None,
        )
        for name, payload in raw_arrays.items()
    }
    return OmInventory(
        arrays=arrays,
        pressure_levels_hpa=infer_pressure_levels_hpa(tuple(arrays)),
    )


def load_om_inventory(path: Path) -> OmInventory:
    root = parse_om_file(path.read_bytes())
    return inventory_from_root(root)
