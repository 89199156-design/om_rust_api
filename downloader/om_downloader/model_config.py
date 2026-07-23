from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class Bounds:
    lon_min: float
    lat_min: float
    lon_max: float
    lat_max: float


@dataclass(frozen=True)
class ProductConfig:
    name: str
    download_product: str
    openmeteo_model: str
    forecast_hour_end: int
    run_cadence_hours: int
    timezone_anchors: tuple[int, ...]
    requested_bounds: Bounds
    bounds_padding_degrees: float
    required_variables: tuple[str, ...]
    optional_variables: tuple[str, ...]
    requested_pressure_levels_hpa: tuple[int, ...]
    history_hours: int = 0
    coverage_strategy: str = "latest_run"
    required_sparse_variables: tuple[str, ...] = ()
    required_initial_fallback_variables: tuple[str, ...] = ()
    interpolation_support_hours: int = 0
    missing_variable_fallback_lookback_hours: int = 0


@dataclass(frozen=True)
class ModelsConfig:
    version: int
    products: dict[str, ProductConfig]


def _require(data: dict[str, Any], key: str) -> Any:
    if key not in data:
        raise ValueError(f"product config missing {key}")
    return data[key]


def _require_product_keys(product_name: str, raw: dict[str, Any]) -> None:
    required_keys = (
        "download_product",
        "forecast_hour_end",
        "run_cadence_hours",
        "timezone_anchors",
        "requested_bounds",
        "bounds_padding_degrees",
        "required_variables",
    )
    missing = [key for key in required_keys if key not in raw]
    if missing:
        joined = ", ".join(missing)
        raise ValueError(f"product {product_name} config missing keys: {joined}")


def _as_str_tuple(value: Any, key: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise ValueError(f"{key} must be a list of strings")
    return tuple(value)


def _as_int_tuple(value: Any, key: str) -> tuple[int, ...]:
    if not isinstance(value, list) or not all(isinstance(item, int) for item in value):
        raise ValueError(f"{key} must be a list of integers")
    return tuple(value)


def _expand_pressure_variables(prefixes: tuple[str, ...], levels: tuple[int, ...]) -> tuple[str, ...]:
    return tuple(f"{prefix}_{level}hPa" for prefix in prefixes for level in levels)


def _as_bounds(value: Any, key: str) -> Bounds:
    if not isinstance(value, dict):
        raise ValueError(f"{key} must be an object")
    return Bounds(
        lon_min=float(_require(value, "lon_min")),
        lat_min=float(_require(value, "lat_min")),
        lon_max=float(_require(value, "lon_max")),
        lat_max=float(_require(value, "lat_max")),
    )


def load_models(path: Path) -> ModelsConfig:
    data = json.loads(path.read_text(encoding="utf-8"))
    products_raw = data.get("products")
    if not isinstance(products_raw, dict):
        raise ValueError("models config missing products")

    products: dict[str, ProductConfig] = {}
    for name, raw in products_raw.items():
        if not isinstance(raw, dict):
            raise ValueError(f"product {name} must be an object")
        _require_product_keys(name, raw)
        requested_levels = _as_int_tuple(
            raw.get("requested_pressure_levels_hpa", []),
            "requested_pressure_levels_hpa",
        )
        required_variables = _as_str_tuple(raw["required_variables"], "required_variables")
        required_variables += _expand_pressure_variables(
            _as_str_tuple(
                raw.get("required_pressure_variable_prefixes", []),
                "required_pressure_variable_prefixes",
            ),
            requested_levels,
        )
        optional_variables = _as_str_tuple(raw.get("optional_variables", []), "optional_variables")
        optional_variables += _expand_pressure_variables(
            _as_str_tuple(
                raw.get("optional_pressure_variable_prefixes", []),
                "optional_pressure_variable_prefixes",
            ),
            requested_levels,
        )
        required_sparse_variables = _as_str_tuple(
            raw.get("required_sparse_variables", []),
            "required_sparse_variables",
        )
        required_initial_fallback_variables = _as_str_tuple(
            raw.get("required_initial_fallback_variables", []),
            "required_initial_fallback_variables",
        )
        interpolation_support_hours = int(raw.get("interpolation_support_hours", 0))
        if interpolation_support_hours < 0:
            raise ValueError(f"product {name} interpolation_support_hours must not be negative")
        missing_variable_fallback_lookback_hours = int(
            raw.get("missing_variable_fallback_lookback_hours", 0)
        )
        if missing_variable_fallback_lookback_hours < 0:
            raise ValueError(
                f"product {name} missing_variable_fallback_lookback_hours must not be negative"
            )
        coverage_strategy = str(raw.get("coverage_strategy", "latest_run"))
        if coverage_strategy not in ("latest_run", "latest_with_long_run_tail"):
            raise ValueError(
                f"product {name} has unsupported coverage_strategy: {coverage_strategy}"
            )
        products[name] = ProductConfig(
            name=name,
            download_product=str(raw["download_product"]),
            openmeteo_model=str(raw.get("openmeteo_model", name)),
            forecast_hour_end=int(raw["forecast_hour_end"]),
            run_cadence_hours=int(raw["run_cadence_hours"]),
            timezone_anchors=_as_int_tuple(raw["timezone_anchors"], "timezone_anchors"),
            requested_bounds=_as_bounds(raw["requested_bounds"], "requested_bounds"),
            bounds_padding_degrees=float(raw["bounds_padding_degrees"]),
            required_variables=required_variables,
            optional_variables=optional_variables,
            requested_pressure_levels_hpa=requested_levels,
            history_hours=int(raw.get("history_hours", 0)),
            coverage_strategy=coverage_strategy,
            required_sparse_variables=required_sparse_variables,
            required_initial_fallback_variables=required_initial_fallback_variables,
            interpolation_support_hours=interpolation_support_hours,
            missing_variable_fallback_lookback_hours=(
                missing_variable_fallback_lookback_hours
            ),
        )
    return ModelsConfig(version=int(data.get("version", 1)), products=products)
