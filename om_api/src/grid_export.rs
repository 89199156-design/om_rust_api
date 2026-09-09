use crate::official::OfficialDecoder;
use crate::query::{
    is_supported_hourly_grid_variable, read_variable_grid_series, round_variable_output_value,
    unit_for_variable, with_weather_model, WeatherModel,
};
use crate::snapshot::OmDataSnapshot;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Timelike, Utc};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const GRID_FILE_MAGIC: &[u8; 8] = b"WGRID1\0\0";
const GRID_FILE_PREFIX_BYTES: usize = 12;
const EC9_OUTPUT_STEP: f64 = 360.0 / 4_608.0;
const EC9_OUTPUT_LEFT: f64 = 70.0;
const EC9_OUTPUT_RIGHT: f64 = 140.0;
const EC9_OUTPUT_BOTTOM: f64 = 0.0;
const EC9_OUTPUT_TOP: f64 = 58.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridModel {
    Gfs,
    EcmwfIfs025,
    EcmwfIfs9km,
    Cams,
}

impl GridModel {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gfs" | "ncep_gfs" | "ncep_gfs013" => Ok(Self::Gfs),
            "ec25" | "ecmwf" | "ecmwf_ifs025" => Ok(Self::EcmwfIfs025),
            "ec9" | "ecmwf_ifs9km" | "ecmwf_ifs_9km" => Ok(Self::EcmwfIfs9km),
            "cams" | "cams_global" => Ok(Self::Cams),
            _ => bail!("unsupported grid model: {value}"),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Gfs => "gfs",
            Self::EcmwfIfs025 => "ec25",
            Self::EcmwfIfs9km => "ec9",
            Self::Cams => "cams",
        }
    }

    pub fn group(self) -> &'static str {
        match self {
            Self::Gfs => "gfs",
            Self::EcmwfIfs025 => "ecmwf",
            Self::EcmwfIfs9km => "ecmwf_ifs9km",
            Self::Cams => "cams",
        }
    }

    fn weather_model(self) -> WeatherModel {
        match self {
            Self::Gfs | Self::Cams => WeatherModel::Gfs,
            Self::EcmwfIfs025 => WeatherModel::EcmwfIfs025,
            Self::EcmwfIfs9km => WeatherModel::EcmwfIfs9km,
        }
    }

    fn primary_product(self) -> &'static str {
        match self {
            Self::Gfs => "gfs013_surface",
            Self::EcmwfIfs025 => "ecmwf_ifs025",
            Self::EcmwfIfs9km => "ecmwf_ifs9km",
            Self::Cams => "cams_global",
        }
    }

    fn products(self) -> &'static [&'static str] {
        match self {
            Self::Gfs => &[
                "gfs013_surface",
                "gfs025",
                "gfs_pressure_profile",
                "ncep_gefs025",
                "ncep_gefs05",
            ],
            Self::EcmwfIfs025 => &["ecmwf_ifs025", "ecmwf_ifs025_ensemble"],
            Self::EcmwfIfs9km => &["ecmwf_ifs9km"],
            Self::Cams => &["cams_global", "cams_global_greenhouse_gases"],
        }
    }
}

#[derive(Debug, Clone)]
pub struct GridReleaseIdentity {
    pub model_run: String,
    pub coverage_id: String,
}

#[derive(Debug, Clone, Copy)]
pub struct RequestedBounds {
    pub west: f64,
    pub east: f64,
    pub south: f64,
    pub north: f64,
}

impl RequestedBounds {
    pub fn validate(self) -> Result<Self> {
        if !self.west.is_finite()
            || !self.east.is_finite()
            || !self.south.is_finite()
            || !self.north.is_finite()
            || self.west >= self.east
            || self.south >= self.north
            || self.west < -180.0
            || self.east > 180.0
            || self.south < -90.0
            || self.north > 90.0
        {
            bail!("invalid grid bounds");
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GridBounds {
    pub west: f64,
    pub east: f64,
    pub south: f64,
    pub north: f64,
}

#[derive(Debug, Clone)]
pub struct OutputGrid {
    pub latitudes: Vec<f64>,
    pub longitudes: Vec<f64>,
    pub full_height: usize,
    pub full_width: usize,
    pub row_start: usize,
    pub column_start: usize,
    pub requested_bounds: Option<RequestedBounds>,
}

impl OutputGrid {
    pub fn height(&self) -> usize {
        self.latitudes.len()
    }

    pub fn width(&self) -> usize {
        self.longitudes.len()
    }

    pub fn point_count(&self) -> Result<usize> {
        self.height()
            .checked_mul(self.width())
            .context("grid point count overflow")
    }

    pub fn bounds(&self) -> Result<GridBounds> {
        Ok(GridBounds {
            west: *self.longitudes.first().context("grid has no longitude")?,
            east: *self.longitudes.last().context("grid has no longitude")?,
            south: *self.latitudes.last().context("grid has no latitude")?,
            north: *self.latitudes.first().context("grid has no latitude")?,
        })
    }
}

#[derive(Debug)]
pub struct EncodedGridFile {
    pub bytes: Vec<u8>,
    pub payload_sha256: String,
    pub point_count: usize,
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn regular_native_grid(snapshot: &OmDataSnapshot, model: GridModel) -> Result<OutputGrid> {
    let product = snapshot.require_product(model.primary_product())?;
    let grid = product
        .entries
        .values()
        .find_map(|entry| entry.native_grid.as_ref())
        .context("native product has no grid metadata")?;
    if grid.grid_type.as_deref() != Some("regional_regular_lat_lon")
        || grid.nx == 0
        || grid.ny == 0
        || !grid.dx.is_finite()
        || !grid.dy.is_finite()
        || grid.dx <= 0.0
        || grid.dy <= 0.0
    {
        bail!("native product does not have a valid regular regional grid");
    }
    let longitudes: Vec<f64> = if let (Some(full_nx), Some(x0)) = (grid.full_nx, grid.x0) {
        let dx = 360.0_f32 / full_nx as f32;
        (0..grid.nx)
            .map(|x| round6((-180.0_f32 + (x0 + x) as f32 * dx) as f64))
            .collect()
    } else {
        (0..grid.nx)
            .map(|x| round6(grid.lon_min + x as f64 * grid.dx))
            .collect()
    };
    let latitudes: Vec<f64> = if let (Some(full_ny), Some(y0)) = (grid.full_ny, grid.y0) {
        let (lat_origin, dy) = if full_ny == 1_536 {
            let dy = 0.11714935_f32;
            (-dy * (full_ny as f32 - 1.0) / 2.0, dy)
        } else {
            (-90.0_f32, 180.0_f32 / (full_ny as f32 - 1.0))
        };
        (0..grid.ny)
            .rev()
            .map(|y| round6((lat_origin + (y0 + y) as f32 * dy) as f64))
            .collect()
    } else {
        (0..grid.ny)
            .rev()
            .map(|y| round6(grid.lat_min + y as f64 * grid.dy))
            .collect()
    };
    Ok(OutputGrid {
        full_height: latitudes.len(),
        full_width: longitudes.len(),
        latitudes,
        longitudes,
        row_start: 0,
        column_start: 0,
        requested_bounds: None,
    })
}

fn ec9_regular_grid() -> Result<OutputGrid> {
    let x0 = (((EC9_OUTPUT_LEFT + 180.0) / EC9_OUTPUT_STEP) - 1e-9)
        .ceil()
        .max(0.0) as usize;
    let x1 = (((EC9_OUTPUT_RIGHT + 180.0) / EC9_OUTPUT_STEP) + 1e-9)
        .floor()
        .min(4_607.0) as usize;
    let y0 = (((EC9_OUTPUT_BOTTOM + 90.0) / EC9_OUTPUT_STEP) - 1e-9)
        .ceil()
        .max(0.0) as usize;
    let y1 = (((EC9_OUTPUT_TOP + 90.0) / EC9_OUTPUT_STEP) + 1e-9)
        .floor()
        .min(2_304.0) as usize;
    if x0 > x1 || y0 > y1 {
        bail!("EC9 output region does not overlap its regular grid");
    }
    let longitudes = (x0..=x1)
        .map(|x| round6(-180.0 + x as f64 * EC9_OUTPUT_STEP))
        .collect::<Vec<_>>();
    let latitudes = (y0..=y1)
        .rev()
        .map(|y| round6(-90.0 + y as f64 * EC9_OUTPUT_STEP))
        .collect::<Vec<_>>();
    Ok(OutputGrid {
        full_height: latitudes.len(),
        full_width: longitudes.len(),
        latitudes,
        longitudes,
        row_start: 0,
        column_start: 0,
        requested_bounds: None,
    })
}

pub fn output_grid(
    snapshot: &OmDataSnapshot,
    model: GridModel,
    requested: Option<RequestedBounds>,
) -> Result<OutputGrid> {
    let full = match model {
        GridModel::EcmwfIfs9km => {
            snapshot.require_product(model.primary_product())?;
            ec9_regular_grid()?
        }
        _ => regular_native_grid(snapshot, model)?,
    };
    let Some(requested) = requested.map(RequestedBounds::validate).transpose()? else {
        return Ok(full);
    };
    crop_output_grid(full, requested)
}

fn crop_output_grid(full: OutputGrid, requested: RequestedBounds) -> Result<OutputGrid> {
    let row_indices = full
        .latitudes
        .iter()
        .enumerate()
        .filter_map(|(index, latitude)| {
            (*latitude >= requested.south - 1e-9 && *latitude <= requested.north + 1e-9)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    let column_indices = full
        .longitudes
        .iter()
        .enumerate()
        .filter_map(|(index, longitude)| {
            (*longitude >= requested.west - 1e-9 && *longitude <= requested.east + 1e-9)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    let row_start = *row_indices
        .first()
        .context("requested bounds contain no grid rows")?;
    let row_end = *row_indices
        .last()
        .context("requested bounds contain no grid rows")?;
    let column_start = *column_indices
        .first()
        .context("requested bounds contain no grid columns")?;
    let column_end = *column_indices
        .last()
        .context("requested bounds contain no grid columns")?;
    Ok(OutputGrid {
        latitudes: full.latitudes[row_start..=row_end].to_vec(),
        longitudes: full.longitudes[column_start..=column_end].to_vec(),
        full_height: full.full_height,
        full_width: full.full_width,
        row_start,
        column_start,
        requested_bounds: Some(requested),
    })
}

fn product_identity(snapshot: &OmDataSnapshot, model: GridModel) -> serde_json::Value {
    let products = model
        .products()
        .iter()
        .filter_map(|name| {
            snapshot.product(name).map(|product| {
                (
                    (*name).to_string(),
                    json!({
                        "coverage_id": product.manifest.coverage_id,
                        "latest_complete_run": product.manifest.latest_complete_run,
                        "public_start_utc": product.manifest.public_start_utc,
                    }),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    json!(products)
}

pub fn source_variable_catalog(
    snapshot: &OmDataSnapshot,
    model: GridModel,
) -> Vec<serde_json::Value> {
    type CatalogEntry = (
        BTreeSet<String>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    );
    let mut variables: BTreeMap<String, CatalogEntry> = BTreeMap::new();
    for product_name in model.products() {
        let Some(product) = snapshot.product(product_name) else {
            continue;
        };
        for key in product.entries.keys() {
            let item = variables.entry(key.variable.clone()).or_default();
            item.0.insert((*product_name).to_string());
            item.1 = Some(
                item.1
                    .map_or(key.valid_time_utc, |value| value.min(key.valid_time_utc)),
            );
            item.2 = Some(
                item.2
                    .map_or(key.valid_time_utc, |value| value.max(key.valid_time_utc)),
            );
        }
    }
    variables
        .into_iter()
        .map(|(name, (products, start, end))| {
            json!({
                "name": name,
                "unit": unit_for_variable(&name),
                "source_products": products,
                "time_start_utc": start,
                "time_end_utc": end,
            })
        })
        .collect()
}

pub fn catalog(
    snapshot: &OmDataSnapshot,
    model: GridModel,
    release: &GridReleaseIdentity,
) -> Result<serde_json::Value> {
    let grid = output_grid(snapshot, model, None)?;
    Ok(json!({
        "schema_version": 1,
        "service": "on-demand-hourly-single-variable-grid",
        "model": model.name(),
        "group": model.group(),
        "model_run": release.model_run,
        "coverage_id": release.coverage_id,
        "products": product_identity(snapshot, model),
        "output_grid": {
            "crs": "EPSG:4326",
            "shape": [grid.height(), grid.width()],
            "row_order": "north_to_south",
            "column_order": "west_to_east",
            "sample_bounds": grid.bounds()?,
            "latitude_count": grid.height(),
            "longitude_count": grid.width(),
            "coordinates_in_each_grid_file": true,
        },
        "available_source_variables": source_variable_catalog(snapshot, model),
        "request": {
            "path": "/v1/internal/grid",
            "required": ["model", "variable", "valid_time"],
            "optional_exact_window": ["west", "east", "south", "north"],
            "expected_run_guard": "expected_run",
        },
        "file_format": {
            "name": "weather-grid-v1",
            "magic_hex": "5747524944310000",
            "prefix": "8-byte magic followed by little-endian u32 JSON-header length",
            "payload_sections": ["latitude float64", "longitude float64", "values float32"],
            "value_index": "row * width + column",
            "missing_value": "IEEE-754 quiet NaN",
            "persistent_server_cache": false,
        },
    }))
}

fn model_has_source_variable(snapshot: &OmDataSnapshot, model: GridModel, variable: &str) -> bool {
    model.products().iter().any(|product_name| {
        snapshot
            .product(product_name)
            .is_some_and(|product| product.entries.keys().any(|key| key.variable == variable))
    })
}

fn validate_request(
    snapshot: &OmDataSnapshot,
    model: GridModel,
    variable: &str,
    valid_time: DateTime<Utc>,
) -> Result<()> {
    if variable.is_empty()
        || variable.len() > 128
        || !variable
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        bail!("invalid grid variable name");
    }
    if valid_time.minute() != 0 || valid_time.second() != 0 || valid_time.nanosecond() != 0 {
        bail!("valid_time must be an exact UTC hour");
    }
    if model == GridModel::Cams
        && !matches!(
            variable,
            "aerosol_optical_depth"
                | "pm2_5"
                | "pm10"
                | "dust"
                | "carbon_monoxide"
                | "nitrogen_dioxide"
                | "ozone"
                | "sulphur_dioxide"
        )
    {
        bail!("CAMS grid export only accepts source concentration and aerosol variables");
    }
    let source_variable = model_has_source_variable(snapshot, model, variable);
    let supported_derived = model != GridModel::Cams
        && is_supported_hourly_grid_variable(model.weather_model(), variable);
    if !source_variable && !supported_derived {
        bail!("variable is not available for {}: {variable}", model.name());
    }
    Ok(())
}

pub fn encode_grid_file(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    model: GridModel,
    release: GridReleaseIdentity,
    variable: &str,
    valid_time: DateTime<Utc>,
    requested_bounds: Option<RequestedBounds>,
) -> Result<EncodedGridFile> {
    validate_request(snapshot, model, variable, valid_time)?;
    let grid = output_grid(snapshot, model, requested_bounds)?;
    let point_count = grid.point_count()?;
    let latitude_block_rows = if model == GridModel::EcmwfIfs9km {
        grid.height().min(64)
    } else {
        grid.height()
    };
    let values = with_weather_model(model.weather_model(), || {
        let mut values = Vec::with_capacity(point_count);
        for latitude_block in grid.latitudes.chunks(latitude_block_rows) {
            let mut block = read_variable_grid_series(
                snapshot,
                decoder,
                variable,
                &[valid_time],
                latitude_block,
                &grid.longitudes,
            )?;
            if block.len() != 1 {
                bail!("grid decoder returned the wrong time count");
            }
            let frame = block.pop().expect("time count checked");
            let expected = latitude_block
                .len()
                .checked_mul(grid.width())
                .context("grid block point count overflow")?;
            if frame.len() != expected {
                bail!("grid decoder returned the wrong spatial point count");
            }
            values.extend(frame);
        }
        Ok(values)
    })?;
    if values.len() != point_count {
        bail!("grid decoder returned an incomplete frame");
    }

    let latitude_bytes = grid
        .height()
        .checked_mul(8)
        .context("latitude byte size overflow")?;
    let longitude_bytes = grid
        .width()
        .checked_mul(8)
        .context("longitude byte size overflow")?;
    let value_bytes = point_count
        .checked_mul(4)
        .context("value byte size overflow")?;
    let payload_len = latitude_bytes
        .checked_add(longitude_bytes)
        .and_then(|value| value.checked_add(value_bytes))
        .context("grid payload size overflow")?;
    let mut payload = Vec::with_capacity(payload_len);
    for value in &grid.latitudes {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    for value in &grid.longitudes {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    for value in values {
        let value = if value.is_finite() {
            round_variable_output_value(variable, value)
        } else {
            f32::NAN
        };
        payload.extend_from_slice(&value.to_le_bytes());
    }
    debug_assert_eq!(payload.len(), payload_len);
    let payload_sha256 = format!("{:x}", Sha256::digest(&payload));
    let actual_bounds = grid.bounds()?;
    let requested_bounds = grid.requested_bounds.map(|bounds| GridBounds {
        west: bounds.west,
        east: bounds.east,
        south: bounds.south,
        north: bounds.north,
    });
    let header = json!({
        "schema_version": 1,
        "format": "weather-grid-v1",
        "model": model.name(),
        "group": model.group(),
        "model_run": release.model_run,
        "coverage_id": release.coverage_id,
        "source_products": product_identity(snapshot, model),
        "variable": variable,
        "unit": unit_for_variable(variable),
        "valid_time_utc": valid_time,
        "crs": "EPSG:4326",
        "shape": [grid.height(), grid.width()],
        "axis_order": ["latitude", "longitude"],
        "row_order": "north_to_south",
        "column_order": "west_to_east",
        "value_index": "row * width + column",
        "full_output_grid_shape": [grid.full_height, grid.full_width],
        "window": {
            "row_start": grid.row_start,
            "column_start": grid.column_start,
            "height": grid.height(),
            "width": grid.width(),
            "requested_bounds": requested_bounds,
            "actual_sample_bounds": actual_bounds,
        },
        "sections": {
            "latitude": {"offset": 0, "bytes": latitude_bytes, "dtype": "<f8", "count": grid.height()},
            "longitude": {"offset": latitude_bytes, "bytes": longitude_bytes, "dtype": "<f8", "count": grid.width()},
            "values": {"offset": latitude_bytes + longitude_bytes, "bytes": value_bytes, "dtype": "<f4", "shape": [grid.height(), grid.width()]},
        },
        "missing_value": "IEEE-754 quiet NaN",
        "coordinate_contract": "Coordinates are explicit payload axes. Bounds are descriptive only and MUST NOT be used as an equality/alignment key.",
        "sampling": if model == GridModel::EcmwfIfs9km { "nearest O1280 cell onto the explicit regular 9 km output axis" } else { "nearest native regional cell onto the explicit output axis" },
        "payload_sha256": payload_sha256,
        "payload_bytes": payload_len,
        "generated_on_demand": true,
        "persisted_by_service": false,
    });
    let header = serde_json::to_vec(&header)?;
    let header_len = u32::try_from(header.len()).context("grid JSON header is too large")?;
    let mut bytes = Vec::with_capacity(GRID_FILE_PREFIX_BYTES + header.len() + payload.len());
    bytes.extend_from_slice(GRID_FILE_MAGIC);
    bytes.extend_from_slice(&header_len.to_le_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(&payload);
    Ok(EncodedGridFile {
        bytes,
        payload_sha256,
        point_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ec9_grid_matches_the_production_regular_axis() {
        let grid = ec9_regular_grid().unwrap();
        assert_eq!((grid.width(), grid.height()), (897, 743));
        assert_eq!(grid.longitudes.first(), Some(&70.0));
        assert_eq!(grid.longitudes.last(), Some(&140.0));
        assert_eq!(grid.latitudes.first(), Some(&57.96875));
        assert_eq!(grid.latitudes.last(), Some(&0.0));
    }

    #[test]
    fn requested_window_uses_explicit_axis_indices() {
        let full = ec9_regular_grid().unwrap();
        let requested = RequestedBounds {
            west: 100.0,
            east: 120.0,
            south: 3.0,
            north: 58.0,
        }
        .validate()
        .unwrap();
        let window = crop_output_grid(full, requested).unwrap();
        assert_eq!(window.row_start, 0);
        assert_eq!(window.column_start, 384);
        assert_eq!(window.latitudes.first(), Some(&57.96875));
        assert_eq!(window.latitudes.last(), Some(&3.046875));
        assert_eq!(window.longitudes.first(), Some(&100.0));
        assert_eq!(window.longitudes.last(), Some(&120.0));
        assert_eq!(window.full_height, 743);
        assert_eq!(window.full_width, 897);
    }

    #[test]
    fn model_aliases_are_explicit() {
        assert_eq!(GridModel::parse("ec9").unwrap(), GridModel::EcmwfIfs9km);
        assert_eq!(GridModel::parse("ec25").unwrap(), GridModel::EcmwfIfs025);
        assert_eq!(GridModel::parse("gfs").unwrap(), GridModel::Gfs);
        assert_eq!(GridModel::parse("cams").unwrap(), GridModel::Cams);
        assert!(GridModel::parse("auto").is_err());
    }
}
