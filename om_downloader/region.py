from __future__ import annotations

from dataclasses import dataclass
import math
from typing import Any

from .model_config import Bounds


OPENMETEO_REGULAR_GRIDS = {
    "ncep_gfs05": {
        "nx": 720,
        "ny": 361,
        "lon_min": -180.0,
        "lat_min": -90.0,
        "dx": 0.5,
        "dy": 0.5,
    },
    "ncep_gfs013": {
        "nx": 3072,
        "ny": 1536,
        "lon_min": -180.0,
        "lat_min": -0.11714935 * (1536 - 1) / 2,
        "dx": 360 / 3072,
        "dy": 0.11714935,
    },
    "ncep_gfs025": {
        "nx": 1440,
        "ny": 721,
        "lon_min": -180.0,
        "lat_min": -90.0,
        "dx": 0.25,
        "dy": 0.25,
    },
    "cams_global": {
        "nx": 900,
        "ny": 451,
        "lon_min": -180.0,
        "lat_min": -90.0,
        "dx": 0.4,
        "dy": 0.4,
    },
    "cams_global_greenhouse_gases": {
        "nx": 3600,
        "ny": 1801,
        "lon_min": -180.0,
        "lat_min": -90.0,
        "dx": 0.1,
        "dy": 0.1,
    },
}


@dataclass(frozen=True)
class GridSpec:
    grid_type: str
    nx: int
    ny: int
    lon_min: float
    lat_min: float
    dx: float
    dy: float

    @property
    def bounds(self) -> Bounds:
        return Bounds(
            lon_min=self.lon_min,
            lat_min=self.lat_min,
            lon_max=self.lon_min + self.dx * (self.nx - 1),
            lat_max=self.lat_min + self.dy * (self.ny - 1),
        )

    def as_manifest(self) -> dict[str, Any]:
        return {
            "grid_type": self.grid_type,
            "nx": self.nx,
            "ny": self.ny,
            "lon_min": self.lon_min,
            "lat_min": self.lat_min,
            "dx": self.dx,
            "dy": self.dy,
        }


def bounds_to_dict(bounds: Bounds) -> dict[str, float]:
    return {
        "lon_min": bounds.lon_min,
        "lat_min": bounds.lat_min,
        "lon_max": bounds.lon_max,
        "lat_max": bounds.lat_max,
    }


def grid_spec_for_openmeteo_model(model: str, *, dimensions: tuple[int, ...] | None = None) -> GridSpec:
    if model not in OPENMETEO_REGULAR_GRIDS:
        raise ValueError(f"unsupported Open-Meteo regular grid model: {model}")
    raw = OPENMETEO_REGULAR_GRIDS[model]
    grid = GridSpec(grid_type="regular_latlon", **raw)
    if dimensions is not None:
        if len(dimensions) < 2:
            raise ValueError("OM array dimensions must include y and x axes")
        expected = (grid.ny, grid.nx)
        actual = (int(dimensions[0]), int(dimensions[1]))
        if actual != expected:
            raise ValueError(
                f"OM dimensions {actual} do not match configured grid {expected} for {model}"
            )
    return grid


def padded_bounds(requested: Bounds, padding_degrees: float, grid_bounds: Bounds) -> Bounds:
    return Bounds(
        lon_min=max(grid_bounds.lon_min, requested.lon_min - padding_degrees),
        lat_min=max(grid_bounds.lat_min, requested.lat_min - padding_degrees),
        lon_max=min(grid_bounds.lon_max, requested.lon_max + padding_degrees),
        lat_max=min(grid_bounds.lat_max, requested.lat_max + padding_degrees),
    )


def regular_grid_ranges(grid: GridSpec, bounds: Bounds) -> dict[str, Any]:
    if grid.grid_type != "regular_latlon":
        raise ValueError(f"unsupported regular grid type: {grid.grid_type}")

    x1 = max(0, math.floor((bounds.lon_min - grid.lon_min) / grid.dx))
    x2 = min(grid.nx, math.ceil((bounds.lon_max - grid.lon_min) / grid.dx) + 1)
    y1 = max(0, math.floor((bounds.lat_min - grid.lat_min) / grid.dy))
    y2 = min(grid.ny, math.ceil((bounds.lat_max - grid.lat_min) / grid.dy) + 1)

    if x1 >= x2 or y1 >= y2:
        raise ValueError("requested bounds do not intersect grid")

    location_count = (x2 - x1) * (y2 - y1)
    is_global = location_count >= grid.nx * grid.ny
    if is_global:
        raise ValueError("region planner selected the full grid; refusing global OM download")

    return {
        "grid_type": grid.grid_type,
        "x_range": [x1, x2],
        "y_range": [y1, y2],
        "location_count": location_count,
        "is_global": is_global,
    }
