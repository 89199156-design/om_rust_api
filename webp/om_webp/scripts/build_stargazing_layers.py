#!/usr/bin/env python3
"""Compose hourly stargazing-score rasters from already published products.

This is a low-cost derived product.  It reads the current weather and CAMS WebP
frames plus the immutable monthly artificial-skyglow cache; it never invokes a
light-pollution propagation solver.
"""

from __future__ import annotations

import argparse
import contextlib
import datetime as dt
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import tempfile
import time
from typing import Iterator

import numpy as np
from PIL import Image

try:  # Linux production lock; tests also run on Windows.
    import fcntl
except ImportError:  # pragma: no cover - Windows only
    fcntl = None


SCHEMA_VERSION = 1
SCORER_VERSION = "stargazing-score-v2"
MAX_CAMS_TIME_DELTA_SECONDS = 90 * 60
NATURAL_BACKGROUND_MICROCD_M2 = 174.0
MICROCD_M2_PER_NANOLAMBERT = 3.18309886184
REFERENCE_EXTINCTION_MAG_PER_AIRMASS = 0.20
REQUIRED_WEATHER_LAYERS = (
    "cloud_total_1",
    "t2m",
    "d2m",
    "r2",
    "wind",
    "gust",
    "vis",
    "tp",
    "thunderstorm_code",
)

MODELS = {
    "gfs": {
        "pointer": "gfs.json",
        "product": "gfs013_surface",
        "output": "stargazing_gfs",
        "manifest": "stargazing_gfs_data.json",
        "display": "GFS+CAMS",
    },
    "ec9": {
        "pointer": "ecmwf_ifs9km.json",
        "product": "ecmwf_ifs9km",
        "output": "stargazing_ec9",
        "manifest": "stargazing_ec9_data.json",
        "display": "EC9+CAMS",
    },
}


def plain_json(path: Path) -> dict[str, object]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise RuntimeError(f"JSON object required: {path}")
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(8 * 1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


@contextlib.contextmanager
def exclusive_lock(path: Path) -> Iterator[None]:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as handle:
        if fcntl is not None:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            if fcntl is not None:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def pointer_product(webp_root: Path, pointer_name: str, product: str) -> tuple[dict[str, object], Path, dict[str, object]]:
    pointer = plain_json(webp_root / "current" / pointer_name)
    if pointer.get("status") != "complete":
        raise RuntimeError(f"incomplete input pointer: {pointer_name}")
    release = Path(str(pointer.get("path") or ""))
    product_root = release / product
    candidates = list(product_root.glob("*_data.json"))
    if len(candidates) != 1:
        raise RuntimeError(f"exactly one product manifest required: {product_root}")
    return pointer, product_root, plain_json(candidates[0])


def load_inputs(webp_root: Path, light_cache_manifest: Path, model: str) -> dict[str, object]:
    spec = MODELS[model]
    weather_pointer, weather_root, weather_manifest = pointer_product(
        webp_root, str(spec["pointer"]), str(spec["product"])
    )
    cams_pointer, cams_root, cams_manifest = pointer_product(webp_root, "cams.json", "cams_global")
    light_manifest = plain_json(light_cache_manifest)
    if light_manifest.get("kind") != "stargazing-monthly-artificial-skyglow-cache":
        raise RuntimeError("invalid light cache manifest kind")
    light_file = light_cache_manifest.parent / str(light_manifest.get("cacheFile") or "")
    if not light_file.is_file() or sha256_file(light_file) != light_manifest.get("cacheSha256"):
        raise RuntimeError("light cache identity validation failed")
    weather_layers = weather_manifest.get("layers")
    if not isinstance(weather_layers, dict):
        raise RuntimeError("weather manifest layers are missing")
    missing = sorted(set(REQUIRED_WEATHER_LAYERS) - set(weather_layers))
    if missing:
        raise RuntimeError(f"weather input layers are missing: {','.join(missing)}")
    cams_layers = cams_manifest.get("layers")
    if not isinstance(cams_layers, dict) or "aerosol_optical_depth" not in cams_layers:
        raise RuntimeError("CAMS AOD input is missing")
    with np.load(light_file, allow_pickle=False) as cache:
        light_values = np.asarray(cache["values_microcd_m2"], dtype=np.float32)
    if light_values.shape[0] != 12:
        raise RuntimeError("light cache must contain twelve months")
    return {
        "weatherPointer": weather_pointer,
        "weatherRoot": weather_root,
        "weatherManifest": weather_manifest,
        "camsPointer": cams_pointer,
        "camsRoot": cams_root,
        "camsManifest": cams_manifest,
        "lightManifest": light_manifest,
        "lightValues": light_values,
    }


def _sample_bounds(grid: dict[str, object]) -> dict[str, float]:
    raw = grid.get("sample_bounds")
    if not isinstance(raw, dict):
        raise RuntimeError("grid sample_bounds are missing")
    return {name: float(raw[name]) for name in ("lon_min", "lat_min", "lon_max", "lat_max")}


def grid_axes(grid: dict[str, object]) -> tuple[np.ndarray, np.ndarray]:
    width, height = int(grid["width"]), int(grid["height"])
    bounds = _sample_bounds(grid)
    return (
        np.linspace(bounds["lon_min"], bounds["lon_max"], width, dtype=np.float64),
        np.linspace(bounds["lat_max"], bounds["lat_min"], height, dtype=np.float64),
    )


def decode_scalar(path: Path, scale: float, vmin: float) -> np.ndarray:
    with Image.open(path) as image:
        rgba = np.asarray(image.convert("RGBA"), dtype=np.uint8)
    raw = (rgba[..., 0].astype(np.uint16) << 8) | rgba[..., 1].astype(np.uint16)
    values = raw.astype(np.float32) / np.float32(scale) + np.float32(vmin)
    values[rgba[..., 3] == 0] = np.nan
    return values


def decode_wind_speed(path: Path) -> np.ndarray:
    with Image.open(path) as image:
        rgba = np.asarray(image.convert("RGBA"), dtype=np.uint8)
    u12 = (rgba[..., 0].astype(np.uint16) << 4) | (rgba[..., 1].astype(np.uint16) >> 4)
    v12 = ((rgba[..., 1].astype(np.uint16) & 0x0F) << 8) | rgba[..., 2].astype(np.uint16)
    u = (u12.astype(np.float32) - 1000.0) * 0.1
    v = (v12.astype(np.float32) - 1000.0) * 0.1
    speed = np.sqrt(u * u + v * v)
    speed[rgba[..., 3] == 0] = np.nan
    return speed


def frame_path(root: Path, manifest: dict[str, object], layer: str, timestamp: int) -> Path:
    pattern = str(manifest.get("file_pattern") or "{timestamp}_{batch}.webp")
    name = pattern.replace("{timestamp}", str(timestamp)).replace("{batch}", str(int(manifest["batch"])))
    return root / layer / name


def layer_contract(manifest: dict[str, object], layer: str) -> tuple[float, float]:
    layers = manifest["layers"]
    assert isinstance(layers, dict)
    value = layers[layer]
    assert isinstance(value, dict)
    return float(value["scale"]), float(value["vmin"])


def read_scalar(root: Path, manifest: dict[str, object], layer: str, timestamp: int) -> np.ndarray:
    scale, vmin = layer_contract(manifest, layer)
    return decode_scalar(frame_path(root, manifest, layer, timestamp), scale, vmin)


def resample_bilinear(values: np.ndarray, source_grid: dict[str, object], target_grid: dict[str, object]) -> np.ndarray:
    source_lon, source_lat = grid_axes(source_grid)
    target_lon, target_lat = grid_axes(target_grid)
    if values.shape != (source_lat.size, source_lon.size):
        raise RuntimeError("source raster shape does not match its manifest")
    x = (target_lon - source_lon[0]) / (source_lon[-1] - source_lon[0]) * (source_lon.size - 1)
    y = (source_lat[0] - target_lat) / (source_lat[0] - source_lat[-1]) * (source_lat.size - 1)
    valid_x = (x >= 0.0) & (x <= source_lon.size - 1)
    valid_y = (y >= 0.0) & (y <= source_lat.size - 1)
    x = np.clip(x, 0.0, source_lon.size - 1)
    y = np.clip(y, 0.0, source_lat.size - 1)
    x0 = np.floor(x).astype(np.int32)
    y0 = np.floor(y).astype(np.int32)
    x1 = np.minimum(x0 + 1, source_lon.size - 1)
    y1 = np.minimum(y0 + 1, source_lat.size - 1)
    fx = (x - x0)[None, :]
    fy = (y - y0)[:, None]
    samples = (
        (values[y0[:, None], x0[None, :]], (1.0 - fx) * (1.0 - fy)),
        (values[y0[:, None], x1[None, :]], fx * (1.0 - fy)),
        (values[y1[:, None], x0[None, :]], (1.0 - fx) * fy),
        (values[y1[:, None], x1[None, :]], fx * fy),
    )
    weighted = np.zeros((target_lat.size, target_lon.size), dtype=np.float64)
    weights = np.zeros_like(weighted)
    for sample, weight in samples:
        finite = np.isfinite(sample)
        weighted += np.where(finite, sample * weight, 0.0)
        weights += np.where(finite, weight, 0.0)
    output = np.full_like(weighted, np.nan, dtype=np.float32)
    valid = (weights > 0.0) & valid_y[:, None] & valid_x[None, :]
    output[valid] = (weighted[valid] / weights[valid]).astype(np.float32)
    return output


def _degrees(value: np.ndarray | float) -> np.ndarray | float:
    return np.deg2rad(value)


def _wrap_degrees(value: np.ndarray | float) -> np.ndarray | float:
    return np.mod(value, 360.0)


def celestial_geometry(timestamp: int, latitudes: np.ndarray, longitudes: np.ndarray) -> tuple[np.ndarray, np.ndarray, float]:
    """Return geometric Sun altitude, Moon altitude, and lunar synodic phase.

    The solar terms are the standard low-order apparent ecliptic solution.  The
    lunar solution includes the principal longitude/latitude perturbations from
    the compact Meeus/Schlyter series.  This is appropriate for a 9-13 km map
    decision layer; point details remain backed by the app's native ephemeris.
    """
    jd = timestamp / 86400.0 + 2440587.5
    d = jd - 2451543.5
    sun_mean_long = _wrap_degrees(280.460 + 0.9856474 * d)
    sun_mean_anomaly = _wrap_degrees(357.528 + 0.9856003 * d)
    sun_lon = _wrap_degrees(
        sun_mean_long + 1.915 * math.sin(math.radians(sun_mean_anomaly))
        + 0.020 * math.sin(math.radians(2.0 * sun_mean_anomaly))
    )
    obliquity = 23.4393 - 3.563e-7 * d
    sun_ra = math.degrees(math.atan2(
        math.cos(math.radians(obliquity)) * math.sin(math.radians(sun_lon)),
        math.cos(math.radians(sun_lon)),
    ))
    sun_dec = math.degrees(math.asin(
        math.sin(math.radians(obliquity)) * math.sin(math.radians(sun_lon))
    ))

    node = _wrap_degrees(125.1228 - 0.0529538083 * d)
    inclination = 5.1454
    periapsis = _wrap_degrees(318.0634 + 0.1643573223 * d)
    eccentricity = 0.054900
    anomaly = _wrap_degrees(115.3654 + 13.0649929509 * d)
    anomaly_r = math.radians(anomaly)
    eccentric_anomaly = anomaly + math.degrees(
        eccentricity * math.sin(anomaly_r) * (1.0 + eccentricity * math.cos(anomaly_r))
    )
    ex = math.cos(math.radians(eccentric_anomaly)) - eccentricity
    ey = math.sqrt(1.0 - eccentricity * eccentricity) * math.sin(math.radians(eccentric_anomaly))
    true_anomaly = math.degrees(math.atan2(ey, ex))
    radius = math.sqrt(ex * ex + ey * ey)
    orbital_lon = true_anomaly + periapsis
    node_r = math.radians(node)
    orbital_lon_r = math.radians(orbital_lon)
    inclination_r = math.radians(inclination)
    orbital_x = radius * (
        math.cos(node_r) * math.cos(orbital_lon_r)
        - math.sin(node_r) * math.sin(orbital_lon_r) * math.cos(inclination_r)
    )
    orbital_y = radius * (
        math.sin(node_r) * math.cos(orbital_lon_r)
        + math.cos(node_r) * math.sin(orbital_lon_r) * math.cos(inclination_r)
    )
    orbital_z = radius * math.sin(orbital_lon_r) * math.sin(inclination_r)
    moon_lon = float(_wrap_degrees(math.degrees(math.atan2(orbital_y, orbital_x))))
    moon_lat = math.degrees(math.atan2(orbital_z, math.sqrt(orbital_x * orbital_x + orbital_y * orbital_y)))
    moon_mean_long = _wrap_degrees(node + periapsis + anomaly)
    elongation = _wrap_degrees(moon_mean_long - sun_lon)
    argument_latitude = _wrap_degrees(moon_mean_long - node)
    moon_lon += (
        -1.274 * math.sin(math.radians(anomaly - 2.0 * elongation))
        + 0.658 * math.sin(math.radians(2.0 * elongation))
        - 0.186 * math.sin(math.radians(sun_mean_anomaly))
        - 0.059 * math.sin(math.radians(2.0 * anomaly - 2.0 * elongation))
        - 0.057 * math.sin(math.radians(anomaly - 2.0 * elongation + sun_mean_anomaly))
        + 0.053 * math.sin(math.radians(anomaly + 2.0 * elongation))
        + 0.046 * math.sin(math.radians(2.0 * elongation - sun_mean_anomaly))
        + 0.041 * math.sin(math.radians(anomaly - sun_mean_anomaly))
        - 0.035 * math.sin(math.radians(elongation))
        - 0.031 * math.sin(math.radians(anomaly + sun_mean_anomaly))
        - 0.015 * math.sin(math.radians(2.0 * argument_latitude - 2.0 * elongation))
        + 0.011 * math.sin(math.radians(anomaly - 4.0 * elongation))
    )
    moon_lat += (
        -0.173 * math.sin(math.radians(argument_latitude - 2.0 * elongation))
        - 0.055 * math.sin(math.radians(anomaly - argument_latitude - 2.0 * elongation))
        - 0.046 * math.sin(math.radians(anomaly + argument_latitude - 2.0 * elongation))
        + 0.033 * math.sin(math.radians(argument_latitude + 2.0 * elongation))
        + 0.017 * math.sin(math.radians(2.0 * anomaly + argument_latitude))
    )
    moon_x = radius * math.cos(math.radians(moon_lon)) * math.cos(math.radians(moon_lat))
    moon_y = radius * math.sin(math.radians(moon_lon)) * math.cos(math.radians(moon_lat))
    moon_z = radius * math.sin(math.radians(moon_lat))
    moon_eq_x = moon_x
    moon_eq_y = moon_y * math.cos(math.radians(obliquity)) - moon_z * math.sin(math.radians(obliquity))
    moon_eq_z = moon_y * math.sin(math.radians(obliquity)) + moon_z * math.cos(math.radians(obliquity))
    moon_ra = math.degrees(math.atan2(moon_eq_y, moon_eq_x))
    moon_dec = math.degrees(math.atan2(moon_eq_z, math.sqrt(moon_eq_x ** 2 + moon_eq_y ** 2)))

    centuries = (jd - 2451545.0) / 36525.0
    gmst = _wrap_degrees(
        280.46061837 + 360.98564736629 * (jd - 2451545.0)
        + 0.000387933 * centuries * centuries
        - centuries * centuries * centuries / 38710000.0
    )
    lat_r = _degrees(latitudes)[:, None]
    local_sidereal = _wrap_degrees(gmst + longitudes)[None, :]

    def altitude(ra: float, dec: float) -> np.ndarray:
        hour_angle = _degrees((local_sidereal - ra + 180.0) % 360.0 - 180.0)
        dec_r = math.radians(dec)
        return np.rad2deg(np.arcsin(
            np.sin(lat_r) * math.sin(dec_r)
            + np.cos(lat_r) * math.cos(dec_r) * np.cos(hour_angle)
        ))

    phase = float(_wrap_degrees(moon_lon - sun_lon))
    return altitude(sun_ra, sun_dec), altitude(moon_ra, moon_dec), phase


def _linear_factor(value: np.ndarray, start: float, end: float, at_start: float, at_end: float) -> np.ndarray:
    ratio = np.clip((value - start) / (end - start), 0.0, 1.0)
    return at_start + (at_end - at_start) * ratio


def moonlight_microcd_m2(moon_elevation: np.ndarray, phase_degrees: float) -> np.ndarray:
    phase_angle = abs(180.0 - phase_degrees)
    zenith_distance = np.clip(90.0 - moon_elevation, 0.0, 89.9)
    rho = np.maximum(zenith_distance, 0.25)
    scattering = 10.0 ** 5.36 * (1.06 + np.cos(np.deg2rad(rho)) ** 2.0) + 10.0 ** (6.15 - rho / 40.0)
    illuminance = 10.0 ** (-0.4 * (3.84 + 0.026 * phase_angle + 4.0e-9 * phase_angle ** 4.0))
    moon_airmass = 1.0 / np.sqrt(1.0 - 0.96 * np.sin(np.deg2rad(zenith_distance)) ** 2.0)
    nano_lambert = (
        scattering * illuminance
        * 10.0 ** (-0.4 * REFERENCE_EXTINCTION_MAG_PER_AIRMASS * moon_airmass)
        * (1.0 - 10.0 ** (-0.4 * REFERENCE_EXTINCTION_MAG_PER_AIRMASS))
    )
    result = np.maximum(nano_lambert * MICROCD_M2_PER_NANOLAMBERT, 0.0)
    return np.where(moon_elevation > 0.0, result, 0.0)


def score_frame(
    *, artificial: np.ndarray, sun_elevation: np.ndarray, moon_elevation: np.ndarray,
    moon_phase: float, cloud: np.ndarray, visibility_m: np.ndarray, humidity: np.ndarray,
    temperature: np.ndarray, dew_point: np.ndarray, wind: np.ndarray, gust: np.ndarray,
    precipitation: np.ndarray, thunder: np.ndarray, aod: np.ndarray,
) -> tuple[np.ndarray, np.ndarray]:
    required = (artificial, cloud, visibility_m, humidity, temperature, dew_point, wind, gust, precipitation, thunder, aod)
    valid = np.logical_and.reduce([np.isfinite(value) for value in required])
    moonlight = moonlight_microcd_m2(moon_elevation, moon_phase)
    total_light = NATURAL_BACKGROUND_MICROCD_M2 + artificial + moonlight
    nano_lambert = total_light / MICROCD_M2_PER_NANOLAMBERT
    magnitude = (20.7233 - np.log(nano_lambert / 34.08)) / 0.92104
    darkness = np.clip((magnitude - 16.5) / (22.0 - 16.5), 0.0, 1.0)
    twilight = np.where(
        sun_elevation <= -18.0,
        1.0,
        np.where(sun_elevation < -12.0, (-sun_elevation - 12.0) / 6.0, 0.0),
    )
    light_score = darkness * twilight
    visibility_km = visibility_m / 1000.0
    dew_spread = temperature - dew_point
    cloud_factor = (1.0 - np.clip(cloud, 0.0, 100.0) / 100.0) ** 1.35
    visibility_factor = _linear_factor(visibility_km, 2.0, 25.0, 0.10, 1.0)
    humidity_factor = _linear_factor(humidity, 75.0, 100.0, 1.0, 0.35)
    dew_factor = _linear_factor(dew_spread, 0.0, 6.0, 0.35, 1.0)
    max_wind = np.maximum(wind, gust)
    wind_factor = np.where(
        max_wind <= 5.0,
        1.0,
        np.where(
            max_wind <= 10.0,
            _linear_factor(max_wind, 5.0, 10.0, 1.0, 0.70),
            np.where(
                max_wind <= 15.0,
                _linear_factor(max_wind, 10.0, 15.0, 0.70, 0.25),
                _linear_factor(max_wind, 15.0, 20.0, 0.25, 0.05),
            ),
        ),
    )
    aerosol_factor = _linear_factor(aod, 0.10, 0.80, 1.0, 0.35)
    weather_score = cloud_factor * np.sqrt(
        visibility_factor * humidity_factor * dew_factor
    ) * np.sqrt(wind_factor) * aerosol_factor ** 0.35
    hard_gate = (precipitation >= 0.05) | (thunder >= 95.0) | (visibility_m < 1000.0)
    valid &= np.isfinite(sun_elevation) & np.isfinite(moon_elevation)
    score = np.floor(100.0 * light_score * weather_score)
    score = np.where(valid & ~hard_gate, np.clip(score, 0.0, 100.0), 0.0).astype(np.uint8)
    return score, valid


def encode_score(path: Path, score: np.ndarray, valid: np.ndarray) -> None:
    rgba = np.zeros((*score.shape, 4), dtype=np.uint8)
    rgba[..., 1] = score
    rgba[..., 3] = np.where(valid, 255, 0).astype(np.uint8)
    Image.fromarray(rgba, "RGBA").save(path, format="WEBP", lossless=True, method=4)


def nearest_timestamp(timestamp: int, candidates: list[int]) -> int | None:
    if not candidates:
        return None
    value = min(candidates, key=lambda candidate: abs(candidate - timestamp))
    return value if abs(value - timestamp) <= MAX_CAMS_TIME_DELTA_SECONDS else None


def light_grid_from_manifest(manifest: dict[str, object]) -> dict[str, object]:
    raw = manifest["grid"]
    assert isinstance(raw, dict)
    sample = raw["sampleBounds"]
    display = raw["displayBounds"]
    assert isinstance(sample, dict) and isinstance(display, dict)
    return {
        "width": raw["width"], "height": raw["height"], "dx": raw["dx"], "dy": raw["dy"],
        "sample_bounds": {"lon_min": sample["lonMin"], "lat_min": sample["latMin"], "lon_max": sample["lonMax"], "lat_max": sample["latMax"]},
        "display_bounds": {"lon_min": display["lonMin"], "lat_min": display["latMin"], "lon_max": display["lonMax"], "lat_max": display["latMax"]},
        "row_order": "north_to_south",
    }


def input_identity(inputs: dict[str, object], model: str) -> str:
    light = inputs["lightManifest"]
    assert isinstance(light, dict)
    payload = {
        "schema": SCHEMA_VERSION,
        "scorer": SCORER_VERSION,
        "builderSha256": sha256_file(Path(__file__)),
        "model": model,
        "weather": inputs["weatherPointer"],
        "cams": inputs["camsPointer"],
        "lightRevision": light["publicationRevision"],
        "lightCache": light["cacheSha256"],
    }
    return hashlib.sha256(json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def build_model(webp_root: Path, public_root: Path, light_cache_manifest: Path, model: str, keep: int) -> dict[str, object]:
    spec = MODELS[model]
    inputs = load_inputs(webp_root, light_cache_manifest, model)
    identity = input_identity(inputs, model)
    current_path = webp_root / "current" / f"{spec['output']}.json"
    if current_path.is_file():
        current = plain_json(current_path)
        if current.get("status") == "complete" and current.get("inputIdentity") == identity:
            return {"model": model, "status": "unchanged", "releaseId": current["release_id"]}

    weather_manifest = inputs["weatherManifest"]
    cams_manifest = inputs["camsManifest"]
    light_manifest = inputs["lightManifest"]
    assert isinstance(weather_manifest, dict) and isinstance(cams_manifest, dict) and isinstance(light_manifest, dict)
    weather_grid = weather_manifest["grid"]
    cams_grid = cams_manifest["grid"]
    assert isinstance(weather_grid, dict) and isinstance(cams_grid, dict)
    light_values = inputs["lightValues"]
    assert isinstance(light_values, np.ndarray)
    light_grid = light_grid_from_manifest(light_manifest)
    weather_times = [int(value) for value in weather_manifest["files"]]
    cams_times = [int(value) for value in cams_manifest["files"]]
    aligned = [(timestamp, nearest_timestamp(timestamp, cams_times)) for timestamp in weather_times]
    aligned = [(weather, cams) for weather, cams in aligned if cams is not None]
    if not aligned:
        raise RuntimeError(f"{spec['display']} has no common hourly timestamps")

    release_id = f"{spec['output']}-{identity[:16]}"
    releases = webp_root / "releases"
    releases.mkdir(parents=True, exist_ok=True)
    final_release = releases / release_id
    staging = Path(tempfile.mkdtemp(prefix=f".{release_id}.", dir=releases))
    output_product = staging / str(spec["output"])
    score_root = output_product / "score"
    score_root.mkdir(parents=True)
    weather_root = inputs["weatherRoot"]
    cams_root = inputs["camsRoot"]
    assert isinstance(weather_root, Path) and isinstance(cams_root, Path)
    target_lon, target_lat = grid_axes(weather_grid)
    light_by_month = [resample_bilinear(values, light_grid, weather_grid) for values in light_values]
    batch = int(weather_manifest["batch"])
    try:
        for index, (timestamp, cams_timestamp) in enumerate(aligned, start=1):
            values = {
                layer: (
                    decode_wind_speed(frame_path(weather_root, weather_manifest, layer, timestamp))
                    if layer == "wind" else read_scalar(weather_root, weather_manifest, layer, timestamp)
                )
                for layer in REQUIRED_WEATHER_LAYERS
            }
            aod_source = read_scalar(cams_root, cams_manifest, "aerosol_optical_depth", int(cams_timestamp))
            aod = resample_bilinear(aod_source, cams_grid, weather_grid)
            month = dt.datetime.fromtimestamp(timestamp, tz=dt.timezone(dt.timedelta(hours=8))).month
            sun_alt, moon_alt, moon_phase = celestial_geometry(timestamp, target_lat, target_lon)
            score, valid = score_frame(
                artificial=light_by_month[month - 1], sun_elevation=sun_alt, moon_elevation=moon_alt,
                moon_phase=moon_phase, cloud=values["cloud_total_1"], visibility_m=values["vis"],
                humidity=values["r2"], temperature=values["t2m"], dew_point=values["d2m"],
                wind=values["wind"], gust=values["gust"], precipitation=values["tp"],
                thunder=values["thunderstorm_code"], aod=aod,
            )
            encode_score(score_root / f"{timestamp}_{batch}.webp", score, valid)
            if index == 1 or index % 12 == 0 or index == len(aligned):
                print(f"STARGAZING_PROGRESS model={model} frame={index}/{len(aligned)} timestamp={timestamp}", flush=True)

        manifest = {
            "generated_at": int(time.time()),
            "source": str(spec["display"]),
            "source_run": inputs["weatherPointer"].get("run"),
            "source_release_id": inputs["weatherPointer"].get("release_id"),
            "cams_run": inputs["camsPointer"].get("run"),
            "cams_release_id": inputs["camsPointer"].get("release_id"),
            "light_pollution_publication_revision": light_manifest["publicationRevision"],
            "scorer_version": SCORER_VERSION,
            "builder_sha256": sha256_file(Path(__file__)),
            "input_identity": identity,
            "batch": batch,
            "frame_count": len(aligned),
            "frame_step_seconds": 3600,
            "file_pattern": "{timestamp}_{batch}.webp",
            "files": [timestamp for timestamp, _ in aligned],
            "grid": weather_grid,
            "layers": {"score": {"subdir": "score", "unit": "score", "encoding": "scalar", "scale": 1.0, "vmin": 0.0, "range": [0.0, 100.0]}},
            "coverage_policy": "weather grid intersected with CAMS and monthly light-pollution valid cells",
            "score_components": ["monthly artificial skyglow", "natural sky background", "moonlight", "twilight", "cloud", "visibility", "humidity", "dew-point spread", "wind", "precipitation", "thunderstorm", "CAMS AOD"],
            "data_attribution": [
                {"source": str(spec["display"]), "modified": True},
                {"source": "CAMS global forecast", "license": "Copernicus Products Licence", "modified": True},
                {"source": "first-order app light-pollution 2025", "publicationRevision": light_manifest["publicationRevision"], "modified": True},
                {"source": "Krisciunas and Schaefer (1991) scattered moonlight model", "doi": "10.1086/132921", "modified": True},
            ],
        }
        (output_product / str(spec["manifest"])).write_text(
            json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        if final_release.exists():
            shutil.rmtree(final_release)
        os.replace(staging, final_release)
        staging = None
        public_root.mkdir(parents=True, exist_ok=True)
        link = public_root / str(spec["output"])
        temp_link = public_root / f".{spec['output']}.{os.getpid()}.tmp"
        temp_link.unlink(missing_ok=True)
        os.symlink(final_release / str(spec["output"]), temp_link)
        os.replace(temp_link, link)
        pointer = {
            "schemaVersion": SCHEMA_VERSION,
            "status": "complete",
            "scope": spec["output"],
            "release_id": release_id,
            "path": str(final_release),
            "run": inputs["weatherPointer"].get("run"),
            "inputIdentity": identity,
        }
        current_path.parent.mkdir(parents=True, exist_ok=True)
        current_temp = current_path.with_name(f".{current_path.name}.{os.getpid()}.tmp")
        current_temp.write_text(json.dumps(pointer, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        os.replace(current_temp, current_path)
        old = sorted(
            [path for path in releases.glob(f"{spec['output']}-*") if path != final_release],
            key=lambda path: path.stat().st_mtime,
            reverse=True,
        )
        for path in old[max(0, keep - 1):]:
            shutil.rmtree(path)
        return {"model": model, "status": "published", "releaseId": release_id, "frames": len(aligned)}
    finally:
        if staging is not None:
            shutil.rmtree(staging, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--webp-root", type=Path, default=Path("/opt/1panel/apps/weather_om_webp/data"))
    parser.add_argument("--public-root", type=Path, default=Path("/opt/1panel/apps/weather/data"))
    parser.add_argument("--light-cache-manifest", type=Path, default=Path("/opt/1panel/apps/weather_om_webp/static/stargazing-light/manifest.json"))
    parser.add_argument("--model", choices=("gfs", "ec9", "all"), default="all")
    parser.add_argument("--keep-releases", type=int, default=2)
    parser.add_argument("--lock-path", type=Path)
    args = parser.parse_args()
    models = tuple(MODELS) if args.model == "all" else (args.model,)
    lock_path = args.lock_path or args.webp_root / ".stargazing.lock"
    with exclusive_lock(lock_path):
        results = [build_model(args.webp_root, args.public_root, args.light_cache_manifest, model, args.keep_releases) for model in models]
    print(json.dumps({"results": results}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
