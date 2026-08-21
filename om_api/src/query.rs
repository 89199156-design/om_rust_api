use crate::dem::read_dem90;
use crate::manifest::{ArrayMetadata, BundleEntry, EntryKey, ProductSnapshot};
use crate::official::{build_v3_array_metadata_blob, BundleRangeReader, OfficialDecoder};
use crate::snapshot::OmDataSnapshot;
use crate::solar::{
    backwards_diffuse_radiation, backwards_direct_normal_irradiance,
    backwards_global_tilted_irradiance, backwards_sunshine_duration, backwards_to_instant_factor,
    clear_sky_radiation_factor_backwards, daylight_duration, extra_terrestrial_radiation_backwards,
    extra_terrestrial_radiation_factor_backwards, is_day, sun_transit, SunTransit,
    PI as SWIFT_FLOAT_PI, SOLAR_CONSTANT,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use rayon::prelude::*;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const GFS013_STATIC_ELEVATION_PATH: &str = "static/ncep_gfs013/HSURF.om";
const GFS013_STATIC_DIMENSIONS: &[u64] = &[1536, 3072];
const GFS013_STATIC_CHUNKS: &[u64] = &[20, 20];
const GFS013_STATIC_LUT_OFFSET: u64 = 1_439_999;
const GFS013_STATIC_LUT_SIZE: u64 = 15_438;
const GFS013_STATIC_FILE_SIZE: u64 = 1_455_544;
const GFS025_STATIC_ELEVATION_PATH: &str = "static/ncep_gfs025/HSURF.om";
const GFS025_STATIC_DIMENSIONS: &[u64] = &[721, 1440];
const GFS025_STATIC_CHUNKS: &[u64] = &[20, 20];
const GFS025_STATIC_LUT_OFFSET: u64 = 404_885;
const GFS025_STATIC_LUT_SIZE: u64 = 3_444;
const GFS025_STATIC_FILE_SIZE: u64 = 408_440;
const GEFS025_STATIC_ELEVATION_PATH: &str = "static/ncep_gefs025/HSURF.om";
const GEFS025_STATIC_DIMENSIONS: &[u64] = &[721, 1440];
const GEFS025_STATIC_CHUNKS: &[u64] = &[20, 20];
const GEFS025_STATIC_LUT_OFFSET: u64 = 404_885;
const GEFS025_STATIC_LUT_SIZE: u64 = 3_444;
const GEFS025_STATIC_FILE_SIZE: u64 = 408_440;
const GEFS05_STATIC_ELEVATION_PATH: &str = "static/ncep_gefs05/HSURF.om";
const GEFS05_STATIC_DIMENSIONS: &[u64] = &[361, 720];
const GEFS05_STATIC_CHUNKS: &[u64] = &[20, 20];
const GEFS05_STATIC_LUT_OFFSET: u64 = 117_112;
const GEFS05_STATIC_LUT_SIZE: u64 = 869;
const GEFS05_STATIC_FILE_SIZE: u64 = 118_088;
const ECMWF025_STATIC_ELEVATION_PATH: &str = "static/ecmwf_ifs025/HSURF.om";
const ECMWF025_STATIC_DIMENSIONS: &[u64] = &[721, 1440];
const ECMWF025_STATIC_CHUNKS: &[u64] = &[20, 20];
const ECMWF025_STATIC_LUT_OFFSET: u64 = 430_052;
const ECMWF025_STATIC_LUT_SIZE: u64 = 3_486;
const ECMWF025_STATIC_FILE_SIZE: u64 = 433_648;
const ECMWF025_ENSEMBLE_STATIC_ELEVATION_PATH: &str = "static/ecmwf_ifs025_ensemble/HSURF.om";
const ECMWF025_ENSEMBLE_STATIC_DIMENSIONS: &[u64] = &[721, 1440];
const ECMWF025_ENSEMBLE_STATIC_CHUNKS: &[u64] = &[20, 20];
const ECMWF025_ENSEMBLE_STATIC_LUT_OFFSET: u64 = 430_151;
const ECMWF025_ENSEMBLE_STATIC_LUT_SIZE: u64 = 3_486;
const ECMWF025_ENSEMBLE_STATIC_FILE_SIZE: u64 = 433_744;
type ElevationCache = HashMap<(PathBuf, u64, u64), f32>;
static ELEVATION_CACHE: OnceLock<Mutex<ElevationCache>> = OnceLock::new();

pub const ECMWF_UPSTREAM_BASELINE: &str = "fc670930b55c963b10e9578c8628a824da43a3ab";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WeatherModel {
    Gfs,
    EcmwfIfs025,
}

impl WeatherModel {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self> {
        let value = value.unwrap_or("gfs").trim().to_ascii_lowercase();
        match value.as_str() {
            "" | "gfs" | "ncep_gfs" | "ncep_gfs013" => Ok(Self::Gfs),
            "ec" | "ecmwf" | "ecmwf_ifs025" => Ok(Self::EcmwfIfs025),
            _ => bail!("unsupported weather model: {value}"),
        }
    }

    fn primary_product(self) -> &'static str {
        match self {
            Self::Gfs => "gfs013_surface",
            Self::EcmwfIfs025 => "ecmwf_ifs025",
        }
    }

    fn static_elevation_spec(self) -> StaticElevationSpec {
        match self {
            Self::Gfs => GFS013_STATIC_SPEC,
            Self::EcmwfIfs025 => ECMWF025_STATIC_SPEC,
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Gfs => "GFS013",
            Self::EcmwfIfs025 => "ECMWF IFS025",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DirectGridSeriesCacheKey {
    model: WeatherModel,
    variable: String,
    start_unix: i64,
    end_unix: i64,
    count: usize,
    latitude_bits: u64,
    longitude_bits: u64,
    sampling: Option<(u64, u64, u32, u32)>,
    round_values: bool,
}

type DirectGridSeriesValues = Vec<Vec<f32>>;
type DirectGridSeriesCacheSlot = Arc<Mutex<Option<Arc<DirectGridSeriesValues>>>>;
type DirectGridSeriesRequestCache =
    Arc<Mutex<HashMap<DirectGridSeriesCacheKey, DirectGridSeriesCacheSlot>>>;

thread_local! {
    static REQUEST_WEATHER_MODEL: RefCell<WeatherModel> = const { RefCell::new(WeatherModel::Gfs) };
    static ECMWF_REQUEST_REGULAR_CACHE: RefCell<Option<HashMap<EcmwfRegularCacheKey, Arc<EcmwfRegularSeries>>>> = const { RefCell::new(None) };
    static GFS_REQUEST_PROBABILITY_HISTORY_CACHE: RefCell<Option<HashMap<GfsProbabilityHistoryCacheKey, Arc<GfsProbabilityPointHistory>>>> = const { RefCell::new(None) };
    static GFS_REQUEST_PROBABILITY_SUPPORT_CACHE: RefCell<Option<HashMap<GfsProbabilitySupportCacheKey, Option<f32>>>> = const { RefCell::new(None) };
    static DAILY_REQUEST_SERIES_CACHE: RefCell<Option<HashMap<DailySeriesCacheKey, Arc<Vec<f32>>>>> = const { RefCell::new(None) };
    static DIRECT_GRID_SERIES_REQUEST_CACHE: RefCell<Option<DirectGridSeriesRequestCache>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GfsProbabilityHistoryCacheKey {
    product_coverages: Vec<(String, String)>,
    raw_variable: String,
    latitude_bits: u64,
    longitude_bits: u64,
}

#[derive(Debug)]
struct GfsProbabilityPointHistory {
    regular_seconds: i64,
    regular_times: Vec<DateTime<Utc>>,
    values: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GfsProbabilitySupportCacheKey {
    product_coverages: Vec<(String, String)>,
    raw_variable: String,
    unix_time: i64,
    latitude_bits: u64,
    longitude_bits: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DailySeriesCacheKey {
    model: WeatherModel,
    variable: String,
    start_unix: i64,
    end_unix: i64,
    count: usize,
    latitude_bits: u64,
    longitude_bits: u64,
}

pub fn with_weather_model<T>(
    model: WeatherModel,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let previous = REQUEST_WEATHER_MODEL.with(|current| current.replace(model));
    let result = operation();
    REQUEST_WEATHER_MODEL.with(|current| current.replace(previous));
    result
}

/// Reuse the quantized regular three-hour ECMWF intermediate within one API
/// request (or one WebP render block). Request decoding stays on one blocking
/// worker, so a thread-local cache has no cross-request synchronization cost.
pub fn with_ecmwf_request_cache<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let owns_cache = ECMWF_REQUEST_REGULAR_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.is_some() {
            false
        } else {
            *cache = Some(HashMap::new());
            true
        }
    });
    let result = operation();
    if owns_cache {
        ECMWF_REQUEST_REGULAR_CACHE.with(|cache| {
            cache.borrow_mut().take();
        });
    }
    result
}

fn with_gfs_request_cache<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let owns_cache = GFS_REQUEST_PROBABILITY_SUPPORT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.is_some() {
            false
        } else {
            *cache = Some(HashMap::new());
            true
        }
    });
    if owns_cache {
        GFS_REQUEST_PROBABILITY_HISTORY_CACHE.with(|cache| {
            *cache.borrow_mut() = Some(HashMap::new());
        });
    }
    let result = operation();
    if owns_cache {
        GFS_REQUEST_PROBABILITY_HISTORY_CACHE.with(|cache| {
            cache.borrow_mut().take();
        });
        GFS_REQUEST_PROBABILITY_SUPPORT_CACHE.with(|cache| {
            cache.borrow_mut().take();
        });
    }
    result
}

fn with_daily_request_cache<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let owns_cache = DAILY_REQUEST_SERIES_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.is_some() {
            false
        } else {
            *cache = Some(HashMap::new());
            true
        }
    });
    let result = operation();
    if owns_cache {
        DAILY_REQUEST_SERIES_CACHE.with(|cache| {
            cache.borrow_mut().take();
        });
    }
    result
}

/// Share decoded point time slabs across all fields in one API request. Derived
/// fields such as wind speed and wind direction read the same U/V slabs, and a
/// complete forecast asks for many such combinations concurrently on Rayon.
/// Per-key slots prevent two workers from inflating the same OM chunk at once.
fn with_direct_grid_series_request_cache<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let owns_cache = DIRECT_GRID_SERIES_REQUEST_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.is_some() {
            false
        } else {
            *cache = Some(Arc::new(Mutex::new(HashMap::new())));
            true
        }
    });
    let result = operation();
    if owns_cache {
        DIRECT_GRID_SERIES_REQUEST_CACHE.with(|cache| {
            cache.borrow_mut().take();
        });
    }
    result
}

fn current_direct_grid_series_request_cache() -> Option<DirectGridSeriesRequestCache> {
    DIRECT_GRID_SERIES_REQUEST_CACHE.with(|cache| cache.borrow().clone())
}

fn with_direct_grid_series_request_cache_handle<T>(
    cache: Option<DirectGridSeriesRequestCache>,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let Some(cache) = cache else {
        return operation();
    };
    let previous = DIRECT_GRID_SERIES_REQUEST_CACHE.with(|current| current.replace(Some(cache)));
    let result = operation();
    DIRECT_GRID_SERIES_REQUEST_CACHE.with(|current| current.replace(previous));
    result
}

fn current_weather_model() -> WeatherModel {
    REQUEST_WEATHER_MODEL.with(|current| *current.borrow())
}

fn derives_diffuse_radiation(model: WeatherModel) -> bool {
    model == WeatherModel::EcmwfIfs025
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridSelectionMode {
    Land,
    Sea,
    Nearest,
}

impl GridSelectionMode {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("land").trim().to_ascii_lowercase().as_str() {
            "land" | "" => Ok(Self::Land),
            "sea" => Ok(Self::Sea),
            "nearest" => Ok(Self::Nearest),
            value => bail!("unsupported cell_selection: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ModelSampling {
    latitude: f64,
    longitude: f64,
    model_elevation: f32,
    target_elevation: f32,
}

#[derive(Debug, Clone, Copy)]
struct RequestSampling {
    gfs013: Option<ModelSampling>,
    gfs025: Option<ModelSampling>,
    gefs025: Option<ModelSampling>,
    gefs05: Option<ModelSampling>,
    ecmwf025: Option<ModelSampling>,
    ecmwf025_ensemble: Option<ModelSampling>,
    response_elevation: f32,
}

thread_local! {
    static REQUEST_SAMPLING: RefCell<Option<RequestSampling>> = const { RefCell::new(None) };
}

fn with_request_sampling<T>(
    sampling: RequestSampling,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let previous = REQUEST_SAMPLING.with(|current| current.replace(Some(sampling)));
    let result = operation();
    REQUEST_SAMPLING.with(|current| current.replace(previous));
    result
}

fn current_product_sampling(product: &str) -> Option<ModelSampling> {
    REQUEST_SAMPLING.with(|current| {
        current
            .borrow()
            .as_ref()
            .and_then(|sampling| match product {
                "gfs013_surface" => sampling.gfs013,
                "gfs025" | "gfs_pressure_profile" => sampling.gfs025,
                "ncep_gefs025" => sampling.gefs025,
                "ncep_gefs05" => sampling.gefs05,
                "ecmwf_ifs025" => sampling.ecmwf025,
                "ecmwf_ifs025_ensemble" => sampling.ecmwf025_ensemble,
                _ => None,
            })
    })
}

fn current_request_sampling() -> Option<RequestSampling> {
    REQUEST_SAMPLING.with(|current| *current.borrow())
}

pub const OPENMETEO_UPSTREAM_BASELINE: &str = "4efb9c49fb4a3718ed385fb22580d2e0fc56bdb2";
pub const OPENMETEO_IMAGE_BASELINE: &str = "weather-forecast-openmeteo:9849315";

#[derive(Debug, Clone, Deserialize)]
pub struct PointQuery {
    #[serde(deserialize_with = "deserialize_csv_field")]
    pub latitude: String,
    #[serde(deserialize_with = "deserialize_csv_field")]
    pub longitude: String,
    #[serde(default, deserialize_with = "deserialize_optional_csv_field")]
    pub hourly: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_csv_field")]
    pub daily: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_csv_field")]
    pub start_hour: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_csv_field")]
    pub end_hour: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_csv_field")]
    pub start_date: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_csv_field")]
    pub end_date: Option<String>,
    #[serde(default)]
    pub forecast_hours: Option<usize>,
    #[serde(default)]
    pub past_days: Option<usize>,
    #[serde(default)]
    pub forecast_days: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_optional_csv_field")]
    pub timezone: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_csv_field")]
    pub cell_selection: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_csv_field")]
    pub elevation: Option<String>,
    #[serde(
        default,
        alias = "model",
        deserialize_with = "deserialize_optional_csv_field"
    )]
    pub models: Option<String>,
}

fn csv_scalar(value: serde_json::Value) -> std::result::Result<String, String> {
    match value {
        serde_json::Value::String(value) => Ok(value),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(csv_scalar)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(|values| values.join(",")),
        serde_json::Value::Null => Err("CSV field must not be null".to_string()),
        serde_json::Value::Object(_) => {
            Err("CSV field must be a scalar or an array of scalars".to_string())
        }
    }
}

fn deserialize_csv_field<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    csv_scalar(serde_json::Value::deserialize(deserializer)?).map_err(D::Error::custom)
}

fn deserialize_optional_csv_field<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    value.map(csv_scalar).transpose().map_err(D::Error::custom)
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteQuery {
    pub points: Vec<RoutePoint>,
    pub hourly: Vec<String>,
    #[serde(default, alias = "model")]
    pub models: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutePoint {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub time: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ForecastResponse {
    pub latitude: f64,
    pub longitude: f64,
    pub generationtime_ms: f64,
    pub utc_offset_seconds: i32,
    pub timezone: String,
    pub timezone_abbreviation: String,
    pub elevation: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<usize>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hourly_units: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hourly: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_units: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Serialize)]
pub struct RouteResponse {
    pub generationtime_ms: f64,
    pub points: Vec<RoutePointResponse>,
}

#[derive(Debug, Serialize)]
pub struct RoutePointResponse {
    pub latitude: f64,
    pub longitude: f64,
    pub time: Option<DateTime<Utc>>,
    pub hourly_units: BTreeMap<String, String>,
    pub hourly: BTreeMap<String, serde_json::Value>,
}

pub fn parse_csv_f64(value: &str, name: &str) -> Result<Vec<f64>> {
    value
        .split(',')
        .filter(|item| !item.trim().is_empty())
        .map(|item| {
            item.trim()
                .parse::<f64>()
                .with_context(|| format!("invalid {} value: {}", name, item))
        })
        .collect()
}

pub fn parse_variables(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn requested_variables(query: &PointQuery) -> (Vec<String>, Vec<String>) {
    (
        parse_variables(query.hourly.as_deref()),
        parse_variables(query.daily.as_deref()),
    )
}

pub fn validate_explicit_variables(query: &PointQuery) -> Result<()> {
    let (hourly, daily) = requested_variables(query);
    if hourly.is_empty() && daily.is_empty() {
        bail!("at least one hourly or daily variable must be specified");
    }
    Ok(())
}

pub fn validate_gfs_query(query: &PointQuery) -> Result<()> {
    validate_explicit_variables(query)?;
    let (hourly, daily) = requested_variables(query);
    if hourly
        .iter()
        .any(|variable| is_air_quality_variable(variable))
        || daily
            .iter()
            .any(|variable| is_chinese_air_quality_daily_variable(variable))
    {
        bail!("CAMS variables must be requested from /v1/cams");
    }
    Ok(())
}

pub fn validate_cams_query(query: &PointQuery) -> Result<()> {
    validate_explicit_variables(query)?;
    let (hourly, daily) = requested_variables(query);
    if hourly
        .iter()
        .any(|variable| !is_air_quality_variable(variable))
        || daily
            .iter()
            .any(|variable| !is_chinese_air_quality_daily_variable(variable))
    {
        bail!("only CAMS air-quality and aerosol variables are available from /v1/cams");
    }
    Ok(())
}

const PUBLIC_PRESSURE_LEVELS: &[u16] = &[
    10, 50, 100, 150, 200, 250, 300, 350, 400, 450, 500, 550, 600, 650, 700, 750, 800, 850, 900,
    925, 950, 975, 1000,
];
pub const ECMWF_PUBLIC_PRESSURE_LEVELS: &[u16] = &[
    1000, 925, 850, 700, 600, 500, 400, 300, 250, 200, 150, 100, 50, 10,
];
pub const ECMWF_PUBLIC_SURFACE_VARIABLES: &[&str] = &[
    "temperature_2m",
    "temperature_2m_min",
    "temperature_2m_max",
    "relative_humidity_2m",
    "dew_point_2m",
    "apparent_temperature",
    "wet_bulb_temperature_2m",
    "pressure_msl",
    "surface_pressure",
    "cloud_cover",
    "cloud_cover_low",
    "cloud_cover_mid",
    "cloud_cover_high",
    "precipitation",
    "precipitation_probability",
    "rain",
    "snowfall",
    "snowfall_water_equivalent",
    "precipitation_type",
    "runoff",
    "snow_depth",
    "snow_depth_water_equivalent",
    "cape",
    "shortwave_radiation",
    "shortwave_radiation_instant",
    "direct_radiation",
    "direct_radiation_instant",
    "diffuse_radiation",
    "diffuse_radiation_instant",
    "direct_normal_irradiance",
    "direct_normal_irradiance_instant",
    "global_tilted_irradiance",
    "global_tilted_irradiance_instant",
    "sunshine_duration",
    "et0_fao_evapotranspiration",
    "vapor_pressure_deficit",
    "wind_speed_10m",
    "wind_direction_10m",
    "wind_gusts_10m",
    "wind_speed_100m",
    "wind_direction_100m",
    "surface_temperature",
    "skin_temperature",
    "soil_temperature_0cm",
    "soil_temperature_0_to_7cm",
    "soil_temperature_7_to_28cm",
    "soil_temperature_28_to_100cm",
    "soil_temperature_100_to_255cm",
    "soil_temperature_0_to_100cm",
    "soil_moisture_0_to_7cm",
    "soil_moisture_7_to_28cm",
    "soil_moisture_28_to_100cm",
    "soil_moisture_100_to_255cm",
    "soil_moisture_0_to_100cm",
    "total_column_integrated_water_vapour",
    "growing_degree_days_base_0_limit_50",
    "weather_code",
    "is_day",
];
pub const ECMWF_PUBLIC_PRESSURE_VARIABLE_TYPES: &[&str] = &[
    "temperature",
    "relative_humidity",
    "geopotential_height",
    "wind_u_component",
    "wind_v_component",
    "wind_speed",
    "wind_direction",
    "vertical_velocity",
    "dew_point",
    "cloud_cover",
];
pub const ECMWF_PUBLIC_DAILY_VARIABLES: &[&str] = &[
    "apparent_temperature_max",
    "apparent_temperature_mean",
    "apparent_temperature_min",
    "cape_max",
    "cape_mean",
    "cape_min",
    "cloud_cover_max",
    "cloud_cover_mean",
    "cloud_cover_min",
    "dew_point_2m_max",
    "dew_point_2m_mean",
    "dew_point_2m_min",
    "et0_fao_evapotranspiration_sum",
    "growing_degree_days_base_0_limit_50",
    "precipitation_hours",
    "precipitation_probability_max",
    "precipitation_probability_mean",
    "precipitation_probability_min",
    "precipitation_sum",
    "pressure_msl_max",
    "pressure_msl_mean",
    "pressure_msl_min",
    "rain_sum",
    "relative_humidity_2m_max",
    "relative_humidity_2m_mean",
    "relative_humidity_2m_min",
    "shortwave_radiation_sum",
    "snowfall_sum",
    "snowfall_water_equivalent_sum",
    "snow_depth_max",
    "snow_depth_mean",
    "snow_depth_min",
    "soil_moisture_0_to_7cm_mean",
    "soil_moisture_7_to_28cm_mean",
    "soil_moisture_28_to_100cm_mean",
    "soil_moisture_100_to_255cm_mean",
    "soil_moisture_0_to_100cm_mean",
    "soil_temperature_0_to_7cm_mean",
    "soil_temperature_7_to_28cm_mean",
    "soil_temperature_28_to_100cm_mean",
    "soil_temperature_100_to_255cm_mean",
    "soil_temperature_0_to_100cm_mean",
    "sunrise",
    "sunset",
    "daylight_duration",
    "sunshine_duration",
    "surface_pressure_max",
    "surface_pressure_mean",
    "surface_pressure_min",
    "temperature_2m_max",
    "temperature_2m_mean",
    "temperature_2m_min",
    "vapor_pressure_deficit_max",
    "weather_code",
    "wind_direction_10m_dominant",
    "wind_gusts_10m_max",
    "wind_gusts_10m_mean",
    "wind_gusts_10m_min",
    "wind_speed_10m_max",
    "wind_speed_10m_mean",
    "wind_speed_10m_min",
    "wet_bulb_temperature_2m_max",
    "wet_bulb_temperature_2m_mean",
    "wet_bulb_temperature_2m_min",
    "wind_direction_100m_dominant",
    "wind_speed_100m_max",
    "wind_speed_100m_mean",
    "wind_speed_100m_min",
];

pub fn ecmwf_public_hourly_variables() -> Vec<String> {
    let mut variables = ECMWF_PUBLIC_SURFACE_VARIABLES
        .iter()
        .map(|variable| (*variable).to_string())
        .collect::<Vec<_>>();
    for level in ECMWF_PUBLIC_PRESSURE_LEVELS {
        for variable_type in ECMWF_PUBLIC_PRESSURE_VARIABLE_TYPES {
            variables.push(format!("{variable_type}_{level}hPa"));
        }
    }
    variables
}

fn is_public_pressure_variable(variable: &str) -> bool {
    let Some((name, level_text)) = variable.rsplit_once('_') else {
        return false;
    };
    let Some(level) = level_text
        .strip_suffix("hPa")
        .and_then(|value| value.parse::<u16>().ok())
    else {
        return false;
    };
    PUBLIC_PRESSURE_LEVELS.contains(&level)
        && matches!(
            name,
            "temperature"
                | "relative_humidity"
                | "relativehumidity"
                | "dew_point"
                | "dewpoint"
                | "cloud_cover"
                | "cloudcover"
                | "wind_speed"
                | "windspeed"
                | "wind_direction"
                | "winddirection"
                | "geopotential_height"
                | "vertical_velocity"
        )
}

fn is_public_hourly_variable(variable: &str) -> bool {
    matches!(
        variable,
        "temperature_2m"
            | "apparent_temperature"
            | "relative_humidity_2m"
            | "relativehumidity_2m"
            | "dew_point_2m"
            | "dewpoint_2m"
            | "wet_bulb_temperature_2m"
            | "surface_temperature"
            | "soil_temperature_0_to_10cm"
            | "soil_temperature_0_to_7cm"
            | "soil_temperature_7_to_28cm"
            | "soil_temperature_28_to_100cm"
            | "soil_temperature_100_to_255cm"
            | "soil_temperature_10_to_40cm"
            | "soil_temperature_40_to_100cm"
            | "soil_temperature_100_to_200cm"
            | "soil_moisture_0_to_7cm"
            | "soil_moisture_7_to_28cm"
            | "soil_moisture_28_to_100cm"
            | "soil_moisture_100_to_255cm"
            | "soil_moisture_0_to_10cm"
            | "soil_moisture_10_to_40cm"
            | "soil_moisture_40_to_100cm"
            | "soil_moisture_100_to_200cm"
            | "pressure_msl"
            | "surface_pressure"
            | "visibility"
            | "weather_code"
            | "weathercode"
            | "is_day"
            | "precipitation"
            | "precipitation_probability"
            | "rain"
            | "showers"
            | "snowfall"
            | "snowfall_water_equivalent"
            | "snow_depth"
            | "cloud_cover"
            | "cloudcover"
            | "cloud_cover_low"
            | "cloudcover_low"
            | "cloud_cover_mid"
            | "cloudcover_mid"
            | "cloud_cover_high"
            | "cloudcover_high"
            | "freezing_level_height"
            | "temperature_80m"
            | "temperature_100m"
            | "temperature_120m"
            | "wind_speed_10m"
            | "windspeed_10m"
            | "wind_direction_10m"
            | "winddirection_10m"
            | "wind_gusts_10m"
            | "wind_speed_80m"
            | "windspeed_80m"
            | "wind_direction_80m"
            | "winddirection_80m"
            | "wind_speed_100m"
            | "windspeed_100m"
            | "wind_direction_100m"
            | "winddirection_100m"
            | "wind_speed_120m"
            | "windspeed_120m"
            | "wind_direction_120m"
            | "winddirection_120m"
            | "cape"
            | "uv_index"
            | "uv_index_clear_sky"
            | "shortwave_radiation"
            | "shortwave_radiation_instant"
            | "sunshine_duration"
            | "et0_fao_evapotranspiration"
            | "vapour_pressure_deficit"
            | "vapor_pressure_deficit"
            | "aerosol_optical_depth"
            | "pm2_5"
            | "pm10"
            | "dust"
            | "carbon_monoxide"
            | "nitrogen_dioxide"
            | "ozone"
            | "sulphur_dioxide"
            | "chinese_aqi"
            | "chinese_aqi_pm2_5"
            | "chinese_aqi_pm10"
            | "chinese_aqi_no2"
            | "chinese_aqi_nitrogen_dioxide"
            | "chinese_aqi_o3"
            | "chinese_aqi_ozone"
            | "chinese_aqi_so2"
            | "chinese_aqi_sulphur_dioxide"
            | "chinese_aqi_co"
            | "chinese_aqi_carbon_monoxide"
    ) || is_public_pressure_variable(variable)
}

fn is_ecmwf_public_pressure_variable(variable: &str) -> bool {
    let Some((name, level_text)) = variable.rsplit_once('_') else {
        return false;
    };
    let Some(level) = level_text
        .strip_suffix("hPa")
        .and_then(|value| value.parse::<u16>().ok())
    else {
        return false;
    };
    ECMWF_PUBLIC_PRESSURE_LEVELS.contains(&level)
        && ECMWF_PUBLIC_PRESSURE_VARIABLE_TYPES.contains(&name)
}

fn is_ecmwf_public_hourly_variable(variable: &str) -> bool {
    ECMWF_PUBLIC_SURFACE_VARIABLES.contains(&variable)
        || is_ecmwf_public_pressure_variable(variable)
}

fn is_gfs_public_daily_variable(variable: &str) -> bool {
    matches!(
        variable,
        "temperature_2m_max"
            | "temperature_2m_min"
            | "temperature_2m_mean"
            | "apparent_temperature_max"
            | "apparent_temperature_min"
            | "apparent_temperature_mean"
            | "precipitation_sum"
            | "precipitation_probability_max"
            | "precipitation_probability_min"
            | "precipitation_probability_mean"
            | "rain_sum"
            | "showers_sum"
            | "snowfall_sum"
            | "snowfall_water_equivalent_sum"
            | "weather_code"
            | "weathercode"
            | "wind_speed_10m_max"
            | "windspeed_10m_max"
            | "wind_speed_10m_min"
            | "windspeed_10m_min"
            | "wind_speed_10m_mean"
            | "windspeed_10m_mean"
            | "wind_gusts_10m_max"
            | "windgusts_10m_max"
            | "wind_gusts_10m_min"
            | "windgusts_10m_min"
            | "wind_gusts_10m_mean"
            | "windgusts_10m_mean"
            | "wind_direction_10m_dominant"
            | "winddirection_10m_dominant"
            | "precipitation_hours"
            | "visibility_max"
            | "visibility_min"
            | "visibility_mean"
            | "pressure_msl_max"
            | "pressure_msl_min"
            | "pressure_msl_mean"
            | "surface_pressure_max"
            | "surface_pressure_min"
            | "surface_pressure_mean"
            | "cloud_cover_max"
            | "cloudcover_max"
            | "cloud_cover_min"
            | "cloudcover_min"
            | "cloud_cover_mean"
            | "cloudcover_mean"
            | "dew_point_2m_max"
            | "dewpoint_2m_max"
            | "dew_point_2m_min"
            | "dewpoint_2m_min"
            | "dew_point_2m_mean"
            | "dewpoint_2m_mean"
            | "relative_humidity_2m_max"
            | "relative_humidity_2m_min"
            | "relative_humidity_2m_mean"
            | "snow_depth_max"
            | "snow_depth_min"
            | "snow_depth_mean"
            | "uv_index_max"
            | "uv_index_clear_sky_max"
            | "shortwave_radiation_sum"
            | "sunshine_duration"
    )
}

fn validate_public_hourly_variables(variables: &[String]) -> Result<()> {
    for variable in variables {
        let supported = match current_weather_model() {
            WeatherModel::Gfs => is_public_hourly_variable(variable),
            WeatherModel::EcmwfIfs025 => is_ecmwf_public_hourly_variable(variable),
        };
        if !supported {
            bail!("unsupported public hourly variable: {variable}");
        }
    }
    Ok(())
}

fn validate_public_daily_variables(variables: &[String]) -> Result<()> {
    for variable in variables {
        if current_weather_model() == WeatherModel::EcmwfIfs025 {
            if !ECMWF_PUBLIC_DAILY_VARIABLES.contains(&variable.as_str()) {
                bail!("unsupported ECMWF daily variable: {variable}");
            }
        } else {
            if !is_gfs_public_daily_variable(variable) {
                bail!(
                    "unsupported daily weather variable: {variable}; this server only exposes official aggregations backed by locally downloaded fields"
                );
            }
            daily_weather_aggregation(variable)?;
        }
    }
    Ok(())
}

pub fn parse_hour(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    value
        .map(|text| {
            DateTime::parse_from_rfc3339(
                if text.ends_with('Z') {
                    text.to_string()
                } else {
                    format!("{text}:00Z")
                }
                .as_str(),
            )
            .or_else(|_| DateTime::parse_from_rfc3339(text))
            .map(|dt| dt.with_timezone(&Utc))
            .with_context(|| format!("invalid hour: {text}"))
        })
        .transpose()
}

#[derive(Debug, Clone)]
struct QueryTimezone {
    offset: FixedOffset,
    identifier: String,
    abbreviation: String,
}

fn parse_query_timezones(
    value: Option<&str>,
    coordinate_count: usize,
) -> Result<Vec<QueryTimezone>> {
    let requested = value
        .unwrap_or("GMT")
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(parse_query_timezone)
        .collect::<Result<Vec<_>>>()?;
    if requested.len() == 1 {
        return Ok(vec![requested[0].clone(); coordinate_count]);
    }
    if requested.len() != coordinate_count {
        bail!("timezone and coordinates must have the same number of elements");
    }
    Ok(requested)
}

fn parse_query_timezone(value: &str) -> Result<QueryTimezone> {
    if value.eq_ignore_ascii_case("auto") {
        bail!("timezone=auto requires the official coordinate timezone database and is not enabled; provide an explicit IANA timezone");
    }
    if value.eq_ignore_ascii_case("GMT") || value.eq_ignore_ascii_case("UTC") {
        return Ok(QueryTimezone {
            offset: FixedOffset::east_opt(0).expect("valid GMT offset"),
            identifier: "GMT".to_string(),
            abbreviation: "GMT".to_string(),
        });
    }
    let timezone = value
        .parse::<Tz>()
        .with_context(|| format!("invalid timezone: {value}"))?;
    let local_now = Utc::now().with_timezone(&timezone);
    Ok(QueryTimezone {
        offset: local_now.offset().fix(),
        identifier: timezone.name().to_string(),
        abbreviation: local_now.format("%Z").to_string(),
    })
}

fn parse_hour_with_timezone(
    value: Option<&str>,
    timezone: &QueryTimezone,
) -> Result<Option<DateTime<Utc>>> {
    value
        .map(|text| {
            if text.ends_with('Z') || DateTime::parse_from_rfc3339(text).is_ok() {
                return parse_hour(Some(text)).map(|value| value.expect("value is present"));
            }
            let local = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M")
                .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
                .with_context(|| format!("invalid hour: {text}"))?;
            timezone
                .offset
                .from_local_datetime(&local)
                .single()
                .map(|value| value.with_timezone(&Utc))
                .with_context(|| format!("invalid local hour: {text}"))
        })
        .transpose()
}

fn apply_response_timezone(
    response: &mut ForecastResponse,
    timezone: &QueryTimezone,
) -> Result<()> {
    response.utc_offset_seconds = timezone.offset.local_minus_utc();
    response.timezone = timezone.identifier.clone();
    response.timezone_abbreviation = timezone.abbreviation.clone();
    let Some(serde_json::Value::Array(times)) = response.hourly.get_mut("time") else {
        return Ok(());
    };
    for value in times {
        let text = value
            .as_str()
            .context("hourly time must be an ISO-8601 string")?;
        let utc = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M")
            .with_context(|| format!("invalid hourly response time: {text}"))?
            .and_utc();
        *value = serde_json::Value::String(
            utc.with_timezone(&timezone.offset)
                .format("%Y-%m-%dT%H:%M")
                .to_string(),
        );
    }
    Ok(())
}

pub fn forecast_for_query(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    query: &PointQuery,
) -> Result<serde_json::Value> {
    let model = WeatherModel::parse(query.models.as_deref())?;
    with_direct_grid_series_request_cache(|| {
        with_weather_model(model, || {
            with_daily_request_cache(|| match model {
                WeatherModel::EcmwfIfs025 => with_ecmwf_request_cache(|| {
                    forecast_for_query_for_model(snapshot, decoder, query)
                }),
                WeatherModel::Gfs => with_gfs_request_cache(|| {
                    forecast_for_query_for_model(snapshot, decoder, query)
                }),
            })
        })
    })
}

fn forecast_for_query_for_model(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    query: &PointQuery,
) -> Result<serde_json::Value> {
    let latitudes = parse_csv_f64(&query.latitude, "latitude")?;
    let longitudes = parse_csv_f64(&query.longitude, "longitude")?;
    if latitudes.len() != longitudes.len() {
        bail!("latitude and longitude count must match");
    }
    let elevations = parse_query_elevations(query.elevation.as_deref(), latitudes.len())?;
    let cell_selection = GridSelectionMode::parse(query.cell_selection.as_deref())?;
    let daily_variables = query
        .daily
        .as_deref()
        .map(|value| parse_variables(Some(value)))
        .unwrap_or_default();
    let variables = if query.hourly.is_none() && !daily_variables.is_empty() {
        Vec::new()
    } else {
        parse_variables(query.hourly.as_deref())
    };
    if variables.is_empty() && daily_variables.is_empty() {
        bail!("at least one hourly or daily variable must be specified");
    }
    validate_public_hourly_variables(&variables)?;
    if !daily_variables
        .iter()
        .any(|variable| is_chinese_air_quality_daily_variable(variable))
    {
        validate_public_daily_variables(&daily_variables)?;
    }
    let timezones = parse_query_timezones(query.timezone.as_deref(), latitudes.len())?;
    let daily_has_air_quality = daily_variables
        .iter()
        .any(|variable| is_chinese_air_quality_daily_variable(variable));
    let daily_is_air_quality = !daily_variables.is_empty()
        && daily_variables
            .iter()
            .all(|variable| is_chinese_air_quality_daily_variable(variable));
    let has_weather = variables
        .iter()
        .any(|variable| !is_air_quality_variable(variable))
        || (!daily_variables.is_empty() && !daily_is_air_quality);
    let has_air_quality = variables
        .iter()
        .any(|variable| is_air_quality_variable(variable))
        || daily_is_air_quality;

    if daily_has_air_quality && !daily_is_air_quality {
        bail!("daily weather and Chinese air-quality variables cannot be mixed in one request");
    }

    let mut responses = Vec::new();
    for index in 0..latitudes.len() {
        let latitude = latitudes[index];
        let longitude = longitudes[index];
        let timezone = &timezones[index];
        let start = parse_hour_with_timezone(query.start_hour.as_deref(), timezone)?;
        let end = parse_hour_with_timezone(query.end_hour.as_deref(), timezone)?;
        let sampling = match decoder {
            Some(_) => Some(resolve_request_sampling(
                snapshot,
                decoder,
                latitude,
                longitude,
                elevations[index],
                cell_selection,
            )?),
            None => {
                let explicit_selection = query
                    .cell_selection
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default();
                if !explicit_selection.is_empty()
                    && !explicit_selection.eq_ignore_ascii_case("nearest")
                {
                    bail!("cell_selection={explicit_selection} requires DEM/static grid selection data");
                }
                None
            }
        };
        let build_response = || {
            let mut response = point_forecast(
                snapshot,
                decoder,
                latitude,
                longitude,
                &variables,
                start,
                end,
                query.forecast_hours,
            )?;
            apply_response_timezone(&mut response, timezone)?;
            if daily_is_air_quality {
                attach_daily_chinese_air_quality(
                    &mut response,
                    snapshot,
                    decoder,
                    latitude,
                    longitude,
                    &daily_variables,
                    query.start_date.as_deref(),
                    query.end_date.as_deref(),
                )?;
            } else if !daily_variables.is_empty() {
                attach_daily_weather(
                    &mut response,
                    snapshot,
                    decoder,
                    latitude,
                    longitude,
                    &daily_variables,
                    query.start_date.as_deref(),
                    query.end_date.as_deref(),
                    query.past_days,
                    query.forecast_days,
                    timezone,
                )?;
            }
            if has_weather {
                if let Some(sampling) = sampling {
                    let model = match current_weather_model() {
                        WeatherModel::Gfs => sampling.gfs013,
                        WeatherModel::EcmwfIfs025 => sampling.ecmwf025,
                    };
                    if let Some(model) = model {
                        response.latitude = model.latitude;
                        response.longitude = model.longitude;
                        response.elevation = Some(sampling.response_elevation as f64);
                    }
                }
            } else if has_air_quality {
                if let Some(sampling) = sampling {
                    if let Some((model_latitude, model_longitude)) =
                        air_quality_model_location(snapshot, latitude, longitude)?
                    {
                        response.latitude = model_latitude;
                        response.longitude = model_longitude;
                        response.elevation = Some(sampling.response_elevation as f64);
                    }
                }
            }
            Ok(response)
        };
        let mut response = match sampling {
            Some(sampling) => with_request_sampling(sampling, build_response)?,
            None => build_response()?,
        };
        if index != 0 {
            response.location_id = Some(index);
        }
        responses.push(response);
    }

    if responses.len() == 1 {
        Ok(serde_json::to_value(responses.remove(0))?)
    } else {
        Ok(serde_json::to_value(responses)?)
    }
}

fn parse_query_elevations(
    value: Option<&str>,
    coordinate_count: usize,
) -> Result<Vec<Option<f32>>> {
    let Some(value) = value else {
        return Ok(vec![None; coordinate_count]);
    };
    let values = value
        .split(',')
        .filter(|item| !item.trim().is_empty())
        .map(|item| {
            item.trim()
                .parse::<f32>()
                .with_context(|| format!("invalid elevation value: {item}"))
        })
        .collect::<Result<Vec<_>>>()?;
    if values.len() != coordinate_count {
        bail!("elevation and coordinates must have the same number of elements");
    }
    Ok(values.into_iter().map(Some).collect())
}

pub fn route_forecast(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    query: &RouteQuery,
) -> Result<RouteResponse> {
    let model = WeatherModel::parse(query.models.as_deref())?;
    with_weather_model(model, || match model {
        WeatherModel::EcmwfIfs025 => {
            with_ecmwf_request_cache(|| route_forecast_for_model(snapshot, decoder, query))
        }
        WeatherModel::Gfs => {
            with_gfs_request_cache(|| route_forecast_for_model(snapshot, decoder, query))
        }
    })
}

fn route_forecast_for_model(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    query: &RouteQuery,
) -> Result<RouteResponse> {
    if query.points.is_empty() {
        bail!("points must not be empty");
    }
    if query.hourly.is_empty() {
        bail!("hourly must not be empty");
    }
    validate_public_hourly_variables(&query.hourly)?;
    let started = std::time::Instant::now();
    let mut points = Vec::with_capacity(query.points.len());
    for point in &query.points {
        let start = point.time;
        let build_response = || {
            point_forecast(
                snapshot,
                decoder,
                point.latitude,
                point.longitude,
                &query.hourly,
                start,
                start,
                Some(1),
            )
        };
        let response = match decoder {
            Some(_) => {
                let sampling = resolve_request_sampling(
                    snapshot,
                    decoder,
                    point.latitude,
                    point.longitude,
                    None,
                    GridSelectionMode::Land,
                )?;
                with_request_sampling(sampling, build_response)?
            }
            None => build_response()?,
        };
        points.push(RoutePointResponse {
            latitude: point.latitude,
            longitude: point.longitude,
            time: point.time,
            hourly_units: response.hourly_units,
            hourly: response.hourly,
        });
    }
    Ok(RouteResponse {
        generationtime_ms: started.elapsed().as_secs_f64() * 1000.0,
        points,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn point_forecast(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
    variables: &[String],
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    limit: Option<usize>,
) -> Result<ForecastResponse> {
    with_direct_grid_series_request_cache(|| {
        point_forecast_with_request_cache(
            snapshot, decoder, latitude, longitude, variables, start, end, limit,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn point_forecast_with_request_cache(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
    variables: &[String],
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    limit: Option<usize>,
) -> Result<ForecastResponse> {
    validate_coordinate(latitude, longitude)?;
    let started = std::time::Instant::now();
    let mut hourly_units = BTreeMap::new();
    let mut hourly = BTreeMap::new();
    if !variables.is_empty() {
        let times = select_times(snapshot, variables, start, end, limit)?;
        hourly_units.insert("time".to_string(), "iso8601".to_string());
        hourly.insert(
            "time".to_string(),
            serde_json::to_value(
                times
                    .iter()
                    .map(|time| time.format("%Y-%m-%dT%H:%M").to_string())
                    .collect::<Vec<String>>(),
            )?,
        );

        let model = current_weather_model();
        let sampling = current_request_sampling();
        let direct_grid_series_cache = current_direct_grid_series_request_cache();
        let read_variable = |variable: &String| {
            let read = || {
                read_hourly_variable_values(
                    snapshot, decoder, variable, &times, latitude, longitude,
                )
            };
            with_direct_grid_series_request_cache_handle(direct_grid_series_cache.clone(), || {
                with_weather_model(model, || {
                    let read_with_cache = || match model {
                        WeatherModel::EcmwfIfs025 => with_ecmwf_request_cache(read),
                        WeatherModel::Gfs => with_gfs_request_cache(read),
                    };
                    match sampling {
                        Some(sampling) => with_request_sampling(sampling, read_with_cache),
                        None => read_with_cache(),
                    }
                })
            })
            .map(|values| (variable.clone(), values))
        };
        // A complete point response contains dozens of independent fields.
        // Decode those fields concurrently on Rayon's bounded global pool;
        // each worker restores the request's model, sampling and cache context
        // above, so nearest-land selection and model-specific derivations stay
        // identical to the serial path. Small route and probe requests avoid
        // the scheduling overhead.
        let decoded = if decoder.is_some() && variables.len() >= 4 {
            variables
                .par_iter()
                .map(read_variable)
                .collect::<Vec<Result<(String, Vec<f32>)>>>()
        } else {
            variables
                .iter()
                .map(read_variable)
                .collect::<Vec<Result<(String, Vec<f32>)>>>()
        };
        for result in decoded {
            let (variable, values) = result?;
            hourly_units.insert(variable.clone(), unit_for_variable(&variable).to_string());
            hourly.insert(variable.clone(), json_array_for_variable(&variable, values));
        }
    }

    Ok(ForecastResponse {
        latitude,
        longitude,
        generationtime_ms: started.elapsed().as_secs_f64() * 1000.0,
        utc_offset_seconds: 0,
        timezone: "GMT".to_string(),
        timezone_abbreviation: "GMT".to_string(),
        elevation: None,
        location_id: None,
        hourly_units,
        hourly,
        daily_units: None,
        daily: None,
    })
}

fn read_hourly_variable_values(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    times: &[DateTime<Utc>],
    latitude: f64,
    longitude: f64,
) -> Result<Vec<f32>> {
    if current_weather_model() == WeatherModel::Gfs && is_gfs_unavailable_ecmwf_soil_layer(variable)
    {
        return Ok(vec![f32::NAN; times.len()]);
    }
    let fast_values = if variable == "precipitation_probability"
        && current_weather_model() == WeatherModel::Gfs
    {
        Some(read_gfs_precipitation_probability_point_series(
            snapshot, decoder, times, latitude, longitude,
        )?)
    } else if let Some(decoder) = decoder {
        match read_variable_point_series(snapshot, decoder, variable, times, latitude, longitude) {
            Ok(values) => values,
            Err(error) if error.to_string().contains("variable/time is not available") => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    if let Some(values) = fast_values {
        return Ok(values);
    }
    let mut values = Vec::with_capacity(times.len());
    for time in times {
        match read_variable_value(snapshot, decoder, variable, *time, latitude, longitude) {
            Ok(value) => values.push(value),
            Err(error) if error.to_string().contains("variable/time is not available") => {
                values.push(f32::NAN);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(values)
}

/// The official GFS API accepts ECMWF layer-depth aliases in a mixed request,
/// but GFS has different native soil layers and returns null for these fields.
/// Avoid attempting hundreds of unavailable OM lookups before producing the
/// same all-null response. Native GFS 0-10/10-40/40-100/100-200 cm fields are
/// separate public variables and continue through normal decoding.
fn is_gfs_unavailable_ecmwf_soil_layer(variable: &str) -> bool {
    matches!(
        variable,
        "soil_temperature_0_to_7cm"
            | "soil_temperature_7_to_28cm"
            | "soil_temperature_28_to_100cm"
            | "soil_temperature_100_to_255cm"
            | "soil_moisture_0_to_7cm"
            | "soil_moisture_7_to_28cm"
            | "soil_moisture_28_to_100cm"
            | "soil_moisture_100_to_255cm"
    )
}

#[derive(Debug, Clone, Copy)]
enum DailyWeatherAggregation {
    Max(&'static str),
    Min(&'static str),
    Mean(&'static str),
    Sum(&'static str),
    RadiationSum(&'static str),
    PrecipitationHours(&'static str),
    DominantWindDirection {
        speed: &'static str,
        direction: &'static str,
        output: &'static str,
    },
    Sunrise,
    Sunset,
    DaylightDuration,
}

impl DailyWeatherAggregation {
    fn seed_variable(self) -> &'static str {
        match self {
            Self::Max(variable)
            | Self::Min(variable)
            | Self::Mean(variable)
            | Self::Sum(variable)
            | Self::RadiationSum(variable)
            | Self::PrecipitationHours(variable) => variable,
            Self::DominantWindDirection { speed, .. } => speed,
            Self::Sunrise | Self::Sunset | Self::DaylightDuration => "temperature_2m",
        }
    }

    fn output_variable(self) -> &'static str {
        match self {
            Self::Max(variable)
            | Self::Min(variable)
            | Self::Mean(variable)
            | Self::Sum(variable)
            | Self::RadiationSum(variable) => variable,
            Self::PrecipitationHours(_) => "precipitation",
            Self::DominantWindDirection { output, .. } => output,
            Self::Sunrise => "sunrise",
            Self::Sunset => "sunset",
            Self::DaylightDuration => "daylight_duration",
        }
    }
}

fn daily_weather_aggregation(variable: &str) -> Result<DailyWeatherAggregation> {
    let aggregation = match variable {
        "temperature_2m_max" => DailyWeatherAggregation::Max("temperature_2m"),
        "temperature_2m_min" => DailyWeatherAggregation::Min("temperature_2m"),
        "temperature_2m_mean" => DailyWeatherAggregation::Mean("temperature_2m"),
        "apparent_temperature_max" => DailyWeatherAggregation::Max("apparent_temperature"),
        "apparent_temperature_min" => DailyWeatherAggregation::Min("apparent_temperature"),
        "apparent_temperature_mean" => DailyWeatherAggregation::Mean("apparent_temperature"),
        "precipitation_sum" => DailyWeatherAggregation::Sum("precipitation"),
        "precipitation_probability_max" => {
            DailyWeatherAggregation::Max("precipitation_probability")
        }
        "precipitation_probability_min" => {
            DailyWeatherAggregation::Min("precipitation_probability")
        }
        "precipitation_probability_mean" => {
            DailyWeatherAggregation::Mean("precipitation_probability")
        }
        "rain_sum" => DailyWeatherAggregation::Sum("rain"),
        "showers_sum" => DailyWeatherAggregation::Sum("showers"),
        "snowfall_sum" => DailyWeatherAggregation::Sum("snowfall"),
        "snowfall_water_equivalent_sum" => DailyWeatherAggregation::Sum("snowfall_water_equivalent"),
        "shortwave_radiation_sum" => DailyWeatherAggregation::RadiationSum("shortwave_radiation"),
        "et0_fao_evapotranspiration" | "et0_fao_evapotranspiration_sum" => {
            DailyWeatherAggregation::Sum("et0_fao_evapotranspiration")
        }
        "sunshine_duration" => DailyWeatherAggregation::Sum("sunshine_duration"),
        "growing_degree_days_base_0_limit_50" => {
            DailyWeatherAggregation::Sum("growing_degree_days_base_0_limit_50")
        }
        "sunrise" => DailyWeatherAggregation::Sunrise,
        "sunset" => DailyWeatherAggregation::Sunset,
        "daylight_duration" => DailyWeatherAggregation::DaylightDuration,
        "weather_code" | "weathercode" => DailyWeatherAggregation::Max("weather_code"),
        "wind_speed_10m_max" | "windspeed_10m_max" => DailyWeatherAggregation::Max("wind_speed_10m"),
        "wind_speed_10m_min" | "windspeed_10m_min" => DailyWeatherAggregation::Min("wind_speed_10m"),
        "wind_speed_10m_mean" | "windspeed_10m_mean" => DailyWeatherAggregation::Mean("wind_speed_10m"),
        "wind_gusts_10m_max" | "windgusts_10m_max" => DailyWeatherAggregation::Max("wind_gusts_10m"),
        "wind_gusts_10m_min" | "windgusts_10m_min" => DailyWeatherAggregation::Min("wind_gusts_10m"),
        "wind_gusts_10m_mean" | "windgusts_10m_mean" => DailyWeatherAggregation::Mean("wind_gusts_10m"),
        "wind_speed_100m_max" => DailyWeatherAggregation::Max("wind_speed_100m"),
        "wind_speed_100m_min" => DailyWeatherAggregation::Min("wind_speed_100m"),
        "wind_speed_100m_mean" => DailyWeatherAggregation::Mean("wind_speed_100m"),
        "wind_direction_10m_dominant" | "winddirection_10m_dominant" => {
            DailyWeatherAggregation::DominantWindDirection {
                speed: "wind_speed_10m",
                direction: "wind_direction_10m",
                output: "wind_direction_10m",
            }
        }
        "wind_direction_100m_dominant" => DailyWeatherAggregation::DominantWindDirection {
            speed: "wind_speed_100m",
            direction: "wind_direction_100m",
            output: "wind_direction_100m",
        },
        "precipitation_hours" => DailyWeatherAggregation::PrecipitationHours("precipitation"),
        "visibility_max" => DailyWeatherAggregation::Max("visibility"),
        "visibility_min" => DailyWeatherAggregation::Min("visibility"),
        "visibility_mean" => DailyWeatherAggregation::Mean("visibility"),
        "pressure_msl_max" => DailyWeatherAggregation::Max("pressure_msl"),
        "pressure_msl_min" => DailyWeatherAggregation::Min("pressure_msl"),
        "pressure_msl_mean" => DailyWeatherAggregation::Mean("pressure_msl"),
        "surface_pressure_max" => DailyWeatherAggregation::Max("surface_pressure"),
        "surface_pressure_min" => DailyWeatherAggregation::Min("surface_pressure"),
        "surface_pressure_mean" => DailyWeatherAggregation::Mean("surface_pressure"),
        "cape_max" => DailyWeatherAggregation::Max("cape"),
        "cape_min" => DailyWeatherAggregation::Min("cape"),
        "cape_mean" => DailyWeatherAggregation::Mean("cape"),
        "cloud_cover_max" | "cloudcover_max" => DailyWeatherAggregation::Max("cloud_cover"),
        "cloud_cover_min" | "cloudcover_min" => DailyWeatherAggregation::Min("cloud_cover"),
        "cloud_cover_mean" | "cloudcover_mean" => DailyWeatherAggregation::Mean("cloud_cover"),
        "dew_point_2m_max" | "dewpoint_2m_max" => DailyWeatherAggregation::Max("dew_point_2m"),
        "dew_point_2m_min" | "dewpoint_2m_min" => DailyWeatherAggregation::Min("dew_point_2m"),
        "dew_point_2m_mean" | "dewpoint_2m_mean" => DailyWeatherAggregation::Mean("dew_point_2m"),
        "relative_humidity_2m_max" => DailyWeatherAggregation::Max("relative_humidity_2m"),
        "relative_humidity_2m_min" => DailyWeatherAggregation::Min("relative_humidity_2m"),
        "relative_humidity_2m_mean" => DailyWeatherAggregation::Mean("relative_humidity_2m"),
        "wet_bulb_temperature_2m_max" => {
            DailyWeatherAggregation::Max("wet_bulb_temperature_2m")
        }
        "wet_bulb_temperature_2m_min" => {
            DailyWeatherAggregation::Min("wet_bulb_temperature_2m")
        }
        "wet_bulb_temperature_2m_mean" => {
            DailyWeatherAggregation::Mean("wet_bulb_temperature_2m")
        }
        "vapour_pressure_deficit_max" | "vapor_pressure_deficit_max" => {
            DailyWeatherAggregation::Max("vapour_pressure_deficit")
        }
        "soil_moisture_0_to_7cm_mean" => {
            DailyWeatherAggregation::Mean("soil_moisture_0_to_7cm")
        }
        "soil_moisture_7_to_28cm_mean" => {
            DailyWeatherAggregation::Mean("soil_moisture_7_to_28cm")
        }
        "soil_moisture_28_to_100cm_mean" => {
            DailyWeatherAggregation::Mean("soil_moisture_28_to_100cm")
        }
        "soil_moisture_100_to_255cm_mean" => {
            DailyWeatherAggregation::Mean("soil_moisture_100_to_255cm")
        }
        "soil_moisture_0_to_100cm_mean" => {
            DailyWeatherAggregation::Mean("soil_moisture_0_to_100cm")
        }
        "soil_temperature_0_to_7cm_mean" => {
            DailyWeatherAggregation::Mean("soil_temperature_0_to_7cm")
        }
        "soil_temperature_7_to_28cm_mean" => {
            DailyWeatherAggregation::Mean("soil_temperature_7_to_28cm")
        }
        "soil_temperature_28_to_100cm_mean" => {
            DailyWeatherAggregation::Mean("soil_temperature_28_to_100cm")
        }
        "soil_temperature_100_to_255cm_mean" => {
            DailyWeatherAggregation::Mean("soil_temperature_100_to_255cm")
        }
        "soil_temperature_0_to_100cm_mean" => {
            DailyWeatherAggregation::Mean("soil_temperature_0_to_100cm")
        }
        "snow_depth_max" => DailyWeatherAggregation::Max("snow_depth"),
        "snow_depth_min" => DailyWeatherAggregation::Min("snow_depth"),
        "snow_depth_mean" => DailyWeatherAggregation::Mean("snow_depth"),
        "uv_index_max" => DailyWeatherAggregation::Max("uv_index"),
        "uv_index_clear_sky_max" => DailyWeatherAggregation::Max("uv_index_clear_sky"),
        _ => bail!(
            "unsupported daily weather variable: {variable}; this server only exposes official aggregations backed by locally downloaded fields"
        ),
    };
    Ok(aggregation)
}

#[allow(clippy::too_many_arguments)]
fn attach_daily_weather(
    response: &mut ForecastResponse,
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
    variables: &[String],
    start_date: Option<&str>,
    end_date: Option<&str>,
    past_days: Option<usize>,
    forecast_days: Option<usize>,
    timezone: &QueryTimezone,
) -> Result<()> {
    let aggregations = variables
        .iter()
        .map(|variable| daily_weather_aggregation(variable))
        .collect::<Result<Vec<_>>>()?;
    let seed = aggregations
        .first()
        .context("daily weather variables must not be empty")?
        .seed_variable();
    let dates = select_weather_dates(
        snapshot,
        seed,
        start_date,
        end_date,
        past_days,
        forecast_days,
        timezone,
    )?;

    let mut daily_units = BTreeMap::new();
    daily_units.insert("time".to_string(), "iso8601".to_string());
    let mut daily = BTreeMap::new();
    daily.insert(
        "time".to_string(),
        serde_json::to_value(
            dates
                .iter()
                .map(|date| date.format("%Y-%m-%d").to_string())
                .collect::<Vec<_>>(),
        )?,
    );

    for (variable, aggregation) in variables.iter().zip(aggregations) {
        if matches!(
            aggregation,
            DailyWeatherAggregation::Sunrise | DailyWeatherAggregation::Sunset
        ) {
            let use_rise = matches!(aggregation, DailyWeatherAggregation::Sunrise);
            let values = dates
                .iter()
                .map(|date| {
                    daily_sun_event(*date, timezone, latitude, longitude, use_rise).map(|time| {
                        time.with_timezone(&timezone.offset)
                            .format("%Y-%m-%dT%H:%M")
                            .to_string()
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            daily_units.insert(variable.clone(), "iso8601".to_string());
            daily.insert(variable.clone(), serde_json::to_value(values)?);
            continue;
        }
        let values = dates
            .iter()
            .map(|date| {
                daily_weather_value(
                    snapshot,
                    decoder,
                    aggregation,
                    *date,
                    timezone,
                    latitude,
                    longitude,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        daily_units.insert(
            variable.clone(),
            daily_weather_unit(variable, aggregation).to_string(),
        );
        daily.insert(
            variable.clone(),
            json_array_for_daily_variable(variable, aggregation, values),
        );
    }
    response.daily_units = Some(daily_units);
    response.daily = Some(daily);
    Ok(())
}

fn daily_model_coordinates(latitude: f64, longitude: f64) -> (f32, f32) {
    current_product_sampling(current_weather_model().primary_product())
        .map(|sampling| (sampling.latitude as f32, sampling.longitude as f32))
        .unwrap_or((latitude as f32, longitude as f32))
}

fn daily_sun_event(
    date: NaiveDate,
    timezone: &QueryTimezone,
    latitude: f64,
    longitude: f64,
    rise: bool,
) -> Result<DateTime<Utc>> {
    let local_midnight = date
        .and_hms_opt(0, 0, 0)
        .context("invalid daily local midnight")?;
    let real_midnight = timezone
        .offset
        .from_local_datetime(&local_midnight)
        .single()
        .context("invalid daily local midnight")?
        .with_timezone(&Utc);
    let offset = Duration::seconds(timezone.offset.local_minus_utc() as i64);
    let display_midnight = real_midnight + offset;
    let (latitude, longitude) = daily_model_coordinates(latitude, longitude);
    let shifted_event = match sun_transit(display_midnight, latitude, longitude) {
        SunTransit::PolarNight => display_midnight,
        SunTransit::PolarDay => {
            if rise {
                display_midnight
            } else {
                display_midnight + Duration::days(1)
            }
        }
        SunTransit::Transit {
            rise_seconds,
            set_seconds,
        } => display_midnight + Duration::seconds(if rise { rise_seconds } else { set_seconds }),
    };
    Ok(shifted_event - offset)
}

fn select_weather_dates(
    snapshot: &OmDataSnapshot,
    seed_variable: &str,
    start_date: Option<&str>,
    end_date: Option<&str>,
    past_days: Option<usize>,
    forecast_days: Option<usize>,
    timezone: &QueryTimezone,
) -> Result<Vec<NaiveDate>> {
    if start_date.is_some() != end_date.is_some() {
        bail!("both start_date and end_date must be set");
    }
    if start_date.is_some() && (past_days.unwrap_or(0) != 0 || forecast_days.unwrap_or(0) != 0) {
        bail!("past_days and forecast_days cannot be combined with start_date and end_date");
    }

    let raw_seed = if current_weather_model() == WeatherModel::EcmwfIfs025
        || seed_variable == "precipitation_probability"
    {
        // The free IFS025 product always requires temperature_2m. Use that
        // stable axis so an unavailable first daily field cannot change the
        // response window or make variable order affect request success.
        "temperature_2m".to_string()
    } else {
        seed_variable_for_times(seed_variable)
    };
    let (product_name, raw_variable) = product_for_variable(snapshot, &raw_seed)?;
    let mut times = snapshot
        .product_snapshots(product_name)
        .into_iter()
        .flat_map(|product| {
            product
                .entries
                .keys()
                .filter(|key| key.variable == raw_variable)
                .map(|key| key.valid_time_utc)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    times.sort();
    times.dedup();
    let first = *times
        .first()
        .context("no data available for requested daily weather variable")?;
    let last = *times
        .last()
        .context("no data available for requested daily weather variable")?;
    let first_date = first.with_timezone(&timezone.offset).date_naive();
    let last_date = last.with_timezone(&timezone.offset).date_naive();

    let (requested_start, requested_end) = match (start_date, end_date) {
        (Some(start), Some(end)) => (
            NaiveDate::parse_from_str(start, "%Y-%m-%d")
                .with_context(|| format!("invalid date: {start}"))?,
            NaiveDate::parse_from_str(end, "%Y-%m-%d")
                .with_context(|| format!("invalid date: {end}"))?,
        ),
        (None, None) => {
            let past_days = past_days.unwrap_or(0);
            let forecast_days = forecast_days.unwrap_or(7);
            if forecast_days == 0 || forecast_days > 16 {
                bail!("forecast_days must be between 1 and 16");
            }
            let today = Utc::now().with_timezone(&timezone.offset).date_naive();
            (
                today
                    .checked_sub_signed(Duration::days(past_days as i64))
                    .context("daily start date overflow")?,
                today
                    .checked_add_signed(Duration::days(forecast_days as i64 - 1))
                    .context("daily end date overflow")?,
            )
        }
        _ => unreachable!("validated matching start/end date options"),
    };
    if requested_start > requested_end {
        bail!("start_date must not be after end_date");
    }
    if requested_start < first_date || requested_end > last_date {
        bail!(
            "daily date range is outside available data: {} to {}",
            first_date,
            last_date
        );
    }

    let mut dates = Vec::new();
    let mut date = requested_start;
    while date <= requested_end {
        dates.push(date);
        date = date.succ_opt().context("daily date range overflow")?;
    }
    Ok(dates)
}

fn daily_weather_value(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    aggregation: DailyWeatherAggregation,
    date: NaiveDate,
    timezone: &QueryTimezone,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let local_start = date
        .and_hms_opt(0, 0, 0)
        .context("invalid daily local start")?;
    let local_end = date
        .succ_opt()
        .context("daily date overflow")?
        .and_hms_opt(0, 0, 0)
        .context("invalid daily local end")?;
    let start = timezone
        .offset
        .from_local_datetime(&local_start)
        .single()
        .context("invalid daily local start")?
        .with_timezone(&Utc);
    let end = timezone
        .offset
        .from_local_datetime(&local_end)
        .single()
        .context("invalid daily local end")?
        .with_timezone(&Utc);

    if matches!(aggregation, DailyWeatherAggregation::DaylightDuration) {
        let display_midnight = start + Duration::seconds(timezone.offset.local_minus_utc() as i64);
        let (latitude, _) = daily_model_coordinates(latitude, longitude);
        return Ok(daylight_duration(display_midnight, latitude));
    }

    if let DailyWeatherAggregation::DominantWindDirection {
        speed, direction, ..
    } = aggregation
    {
        let step_hours = if current_weather_model() == WeatherModel::EcmwfIfs025 {
            3
        } else {
            1
        };
        let times = daily_sample_times(start, end, step_hours);
        let speed_values =
            read_daily_series(snapshot, decoder, speed, &times, latitude, longitude)?;
        let direction_values =
            read_daily_series(snapshot, decoder, direction, &times, latitude, longitude)?;
        if speed_values
            .iter()
            .chain(&direction_values)
            .any(|value| !value.is_finite())
        {
            return Ok(f32::NAN);
        }
        return Ok(dominant_wind_direction(&speed_values, &direction_values));
    }

    let source = aggregation.seed_variable();
    let step_hours = if current_weather_model() == WeatherModel::EcmwfIfs025
        && matches!(
            aggregation,
            DailyWeatherAggregation::Mean(_)
                | DailyWeatherAggregation::Sum(_)
                | DailyWeatherAggregation::RadiationSum(_)
                | DailyWeatherAggregation::PrecipitationHours(_)
        ) {
        3
    } else {
        1
    };
    let times = daily_sample_times(start, end, step_hours);
    let values = read_daily_series(snapshot, decoder, source, &times, latitude, longitude)?;

    Ok(aggregate_daily_weather_values(
        aggregation,
        &values,
        step_hours,
    ))
}

fn aggregate_daily_weather_values(
    aggregation: DailyWeatherAggregation,
    values: &[f32],
    step_hours: i64,
) -> f32 {
    let finite_extreme = |take_max: bool| {
        values
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .reduce(|left, right| {
                if take_max {
                    left.max(right)
                } else {
                    left.min(right)
                }
            })
            .unwrap_or(f32::NAN)
    };
    let complete = values.iter().all(|value| value.is_finite());
    let value = match aggregation {
        // Open-Meteo does not publish regular daily aggregates for a partial
        // local day. `precipitation_hours` is the sole exception: it counts
        // the available hourly frames so callers can see partial coverage.
        DailyWeatherAggregation::Max(_) if complete => finite_extreme(true),
        DailyWeatherAggregation::Min(_) if complete => finite_extreme(false),
        DailyWeatherAggregation::Mean(_) if complete => {
            values.iter().sum::<f32>() / values.len() as f32
        }
        DailyWeatherAggregation::Sum(_) if complete => values.iter().sum(),
        DailyWeatherAggregation::RadiationSum(_) if complete => {
            values.iter().sum::<f32>() * (step_hours * 3600) as f32 / 1_000_000.0
        }
        DailyWeatherAggregation::PrecipitationHours(_) => values
            .iter()
            .filter(|value| value.is_finite())
            .map(|value| {
                if *value > 0.001 {
                    step_hours as f32
                } else {
                    0.0
                }
            })
            .sum(),
        DailyWeatherAggregation::Max(_)
        | DailyWeatherAggregation::Min(_)
        | DailyWeatherAggregation::Mean(_)
        | DailyWeatherAggregation::Sum(_)
        | DailyWeatherAggregation::RadiationSum(_) => f32::NAN,
        DailyWeatherAggregation::DominantWindDirection { .. }
        | DailyWeatherAggregation::DaylightDuration
        | DailyWeatherAggregation::Sunrise
        | DailyWeatherAggregation::Sunset => unreachable!("handled above"),
    };
    value
}

fn daily_sample_times(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step_hours: i64,
) -> Vec<DateTime<Utc>> {
    let mut times = Vec::new();
    let mut time = start;
    while time < end {
        times.push(time);
        time += Duration::hours(step_hours);
    }
    times
}

fn read_daily_series(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    times: &[DateTime<Utc>],
    latitude: f64,
    longitude: f64,
) -> Result<Vec<f32>> {
    let Some((first, last)) = times.first().zip(times.last()) else {
        return Ok(Vec::new());
    };
    let key = DailySeriesCacheKey {
        model: current_weather_model(),
        variable: variable.to_string(),
        start_unix: first.timestamp(),
        end_unix: last.timestamp(),
        count: times.len(),
        latitude_bits: latitude.to_bits(),
        longitude_bits: longitude.to_bits(),
    };
    let cached = DAILY_REQUEST_SERIES_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .and_then(|cache| cache.get(&key).cloned())
    });
    if let Some(values) = cached {
        return Ok((*values).clone());
    }
    let values =
        read_daily_series_uncached(snapshot, decoder, variable, times, latitude, longitude)?;
    DAILY_REQUEST_SERIES_CACHE.with(|cache| {
        if let Some(cache) = cache.borrow_mut().as_mut() {
            cache.insert(key, Arc::new(values.clone()));
        }
    });
    Ok(values)
}

fn read_daily_series_uncached(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    times: &[DateTime<Utc>],
    latitude: f64,
    longitude: f64,
) -> Result<Vec<f32>> {
    if current_weather_model() == WeatherModel::Gfs && is_gfs_unavailable_ecmwf_soil_layer(variable)
    {
        return Ok(vec![f32::NAN; times.len()]);
    }
    if variable == "precipitation_probability" && current_weather_model() == WeatherModel::Gfs {
        return read_gfs_precipitation_probability_point_series(
            snapshot, decoder, times, latitude, longitude,
        );
    }
    if let Some(decoder) = decoder {
        // Use the same retained-run selection as the hourly point endpoint.
        // In particular, GFS hours before the latest complete run must prefer
        // the newest covering short run; decoding the whole grid series here
        // would incorrectly force those hours onto an older complete run.
        match read_variable_point_series(snapshot, decoder, variable, times, latitude, longitude) {
            Ok(Some(values)) => return Ok(values),
            Ok(None) => {}
            Err(error) if error.to_string().contains("variable/time is not available") => {}
            Err(error) => return Err(error),
        }
    }
    times
        .iter()
        .map(|time| read_daily_hour(snapshot, decoder, variable, *time, latitude, longitude))
        .collect()
}

fn read_daily_hour(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    match read_variable_value(snapshot, decoder, variable, time, latitude, longitude) {
        Ok(value) => Ok(value),
        Err(error) if error.to_string().contains("variable/time is not available") => Ok(f32::NAN),
        Err(error) => Err(error),
    }
}

fn daily_weather_unit(variable: &str, aggregation: DailyWeatherAggregation) -> &'static str {
    match variable {
        "precipitation_hours" => "h",
        "shortwave_radiation_sum" => "MJ/m\u{00B2}",
        "sunrise" | "sunset" => "iso8601",
        "daylight_duration" | "sunshine_duration" => "s",
        _ => unit_for_variable(aggregation.output_variable()),
    }
}

#[allow(clippy::too_many_arguments)]
fn attach_daily_chinese_air_quality(
    response: &mut ForecastResponse,
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
    variables: &[String],
    start_date: Option<&str>,
    end_date: Option<&str>,
) -> Result<()> {
    if variables
        .iter()
        .any(|variable| !is_chinese_air_quality_daily_variable(variable))
    {
        bail!("unsupported daily Chinese air-quality variable")
    }

    let dates = select_chinese_aqi_dates(snapshot, start_date, end_date)?;
    let mut daily_units = BTreeMap::new();
    daily_units.insert("time".to_string(), "iso8601".to_string());
    let mut daily = BTreeMap::new();
    let mut selected_dates = Vec::new();
    let mut values_by_variable = variables
        .iter()
        .map(|variable| (variable.clone(), Vec::new()))
        .collect::<BTreeMap<_, _>>();

    for date in dates {
        let stats = daily_chinese_air_quality_stats(snapshot, decoder, date, latitude, longitude)?;
        let mut values = Vec::with_capacity(variables.len());
        for variable in variables {
            values.push(daily_chinese_air_quality_value(&stats, variable)?);
        }
        selected_dates.push(date.format("%Y-%m-%d").to_string());
        for (variable, value) in variables.iter().zip(values) {
            values_by_variable
                .get_mut(variable)
                .expect("created from requested variables")
                .push(value);
        }
    }

    daily.insert("time".to_string(), serde_json::to_value(selected_dates)?);
    for (variable, values) in values_by_variable {
        daily_units.insert(variable.clone(), unit_for_variable(&variable).to_string());
        daily.insert(variable.clone(), json_array_for_variable(&variable, values));
    }
    response.daily_units = Some(daily_units);
    response.daily = Some(daily);
    Ok(())
}

fn select_chinese_aqi_dates(
    snapshot: &OmDataSnapshot,
    start_date: Option<&str>,
    end_date: Option<&str>,
) -> Result<Vec<NaiveDate>> {
    let product = snapshot.require_product("cams_global")?;
    let mut times: Vec<DateTime<Utc>> = snapshot
        .product_snapshots("cams_global")
        .iter()
        .flat_map(|candidate| candidate.entries.keys())
        .filter(|key| key.variable == "pm2_5")
        .map(|key| key.valid_time_utc)
        .collect();
    times.sort();
    times.dedup();
    let first = *times
        .first()
        .context("no CAMS PM2.5 data is available for daily Chinese AQI")?;
    let last = *times
        .last()
        .context("no CAMS PM2.5 data is available for daily Chinese AQI")?;
    let china_offset = FixedOffset::east_opt(8 * 3600).expect("valid China UTC offset");
    let first_date = first.with_timezone(&china_offset).date_naive();
    let last_date = last.with_timezone(&china_offset).date_naive();
    let requested_start = parse_date(start_date)?.unwrap_or(first_date);
    let requested_end = parse_date(end_date)?.unwrap_or(last_date);
    if requested_start > requested_end {
        bail!("start_date must not be after end_date")
    }
    let _ = product;
    let mut dates = Vec::new();
    let mut date = requested_start.max(first_date);
    let end = requested_end.min(last_date);
    while date <= end {
        dates.push(date);
        date = date.succ_opt().context("daily date range overflow")?;
    }
    Ok(dates)
}

fn parse_date(value: Option<&str>) -> Result<Option<NaiveDate>> {
    value
        .map(|text| {
            NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .with_context(|| format!("invalid date: {text}"))
        })
        .transpose()
}

fn validate_coordinate(latitude: f64, longitude: f64) -> Result<()> {
    if !(-90.0..=90.0).contains(&latitude) {
        bail!("latitude must be between -90 and 90");
    }
    if !(-180.0..=180.0).contains(&longitude) {
        bail!("longitude must be between -180 and 180");
    }
    Ok(())
}

fn select_times(
    snapshot: &OmDataSnapshot,
    variables: &[String],
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    limit: Option<usize>,
) -> Result<Vec<DateTime<Utc>>> {
    let contains_weather = variables.is_empty()
        || variables
            .iter()
            .any(|variable| !is_air_quality_variable(variable));
    let requested_seed = variables
        .first()
        .map(|value| seed_variable_for_times(value))
        .unwrap_or_else(|| "temperature_2m".to_string());
    let seed_var = if contains_weather
        && (current_weather_model() == WeatherModel::EcmwfIfs025
            || requested_seed == "precipitation_probability")
    {
        "temperature_2m".to_string()
    } else {
        requested_seed
    };
    let (product_name, raw_var) = product_for_variable(snapshot, &seed_var)?;
    let product = snapshot.require_product(product_name)?;
    let mut times: Vec<DateTime<Utc>> = snapshot
        .product_snapshots(product_name)
        .into_iter()
        .flat_map(|candidate| {
            candidate
                .entries
                .keys()
                .filter(|key| key.variable == raw_var)
                .map(|key| key.valid_time_utc)
                .collect::<Vec<_>>()
        })
        .collect();
    times.sort();
    times.dedup();
    let first = *times
        .first()
        .context("no data available for requested variable")?;
    let last = *times
        .last()
        .context("no data available for requested variable")?;
    let public_start = product
        .manifest
        .public_start_utc
        .unwrap_or(first)
        .max(first);
    let selected_start = match start {
        // ECMWF retains complete source cycles specifically so an explicit
        // request can read the hours before the newest cycle and apply the
        // same one-generation fallback as the upstream API. Keep the normal
        // default window at public_start, but do not hide retained history
        // from an explicitly bounded request.
        Some(requested) if current_weather_model() == WeatherModel::EcmwfIfs025 => {
            requested.max(first)
        }
        Some(requested) => requested.max(public_start),
        None => public_start,
    };
    let selected_end = end.unwrap_or(last).min(last);
    times.clear();
    let mut time = selected_start;
    while time <= selected_end {
        times.push(time);
        time += Duration::hours(1);
    }
    if let Some(limit) = limit {
        times.truncate(limit);
    }
    if times.is_empty() {
        bail!("no data available for requested time range");
    }
    Ok(times)
}

pub fn read_variable_value(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    if let Some(value) =
        read_derived_air_quality(snapshot, decoder, variable, time, latitude, longitude)?
    {
        return Ok(value);
    }
    if let Some(value) =
        read_derived_pressure(snapshot, decoder, variable, time, latitude, longitude)?
    {
        return Ok(value);
    }
    match variable {
        "weather_code" | "weathercode" => {
            return read_weather_code(snapshot, decoder, time, latitude, longitude);
        }
        "apparent_temperature" => {
            let temperature = read_direct(
                snapshot,
                decoder,
                "temperature_2m",
                time,
                latitude,
                longitude,
            )?;
            let relative_humidity = read_direct(
                snapshot,
                decoder,
                "relative_humidity_2m",
                time,
                latitude,
                longitude,
            )?;
            let wind_speed = read_variable_value(
                snapshot,
                decoder,
                "wind_speed_10m",
                time,
                latitude,
                longitude,
            )?;
            let shortwave_radiation = read_direct(
                snapshot,
                decoder,
                "shortwave_radiation",
                time,
                latitude,
                longitude,
            )?;
            return Ok(apparent_temperature(
                temperature,
                relative_humidity,
                wind_speed,
                Some(shortwave_radiation),
            ));
        }
        "surface_pressure" => {
            let temperature = read_direct(
                snapshot,
                decoder,
                "temperature_2m",
                time,
                latitude,
                longitude,
            )?;
            let pressure_msl =
                read_direct(snapshot, decoder, "pressure_msl", time, latitude, longitude)?;
            let product = current_weather_model().primary_product();
            let elevation = current_product_sampling(product)
                .map(surface_pressure_elevation)
                .or_else(|| {
                    weather_model_location(snapshot, decoder, latitude, longitude)
                        .ok()
                        .flatten()
                        .map(|(_, _, elevation)| elevation)
                })
                .unwrap_or(0.0);
            return Ok(surface_pressure(temperature, pressure_msl, elevation));
        }
        "dew_point_2m" | "dewpoint_2m" => {
            let temperature = read_direct(
                snapshot,
                decoder,
                "temperature_2m",
                time,
                latitude,
                longitude,
            )?;
            let relative_humidity = read_direct(
                snapshot,
                decoder,
                "relative_humidity_2m",
                time,
                latitude,
                longitude,
            )?;
            return Ok(dew_point(temperature, relative_humidity));
        }
        "wet_bulb_temperature_2m" => {
            let temperature = read_direct(
                snapshot,
                decoder,
                "temperature_2m",
                time,
                latitude,
                longitude,
            )?;
            let relative_humidity = read_direct(
                snapshot,
                decoder,
                "relative_humidity_2m",
                time,
                latitude,
                longitude,
            )?;
            return Ok(wet_bulb_temperature(temperature, relative_humidity));
        }
        "evapotranspiration" => {
            let latent_heat_flux = read_direct(
                snapshot,
                decoder,
                "latent_heat_flux",
                time,
                latitude,
                longitude,
            )?;
            return Ok(evapotranspiration(latent_heat_flux));
        }
        "vapour_pressure_deficit" | "vapor_pressure_deficit" => {
            let temperature = read_direct(
                snapshot,
                decoder,
                "temperature_2m",
                time,
                latitude,
                longitude,
            )?;
            let relative_humidity = read_direct(
                snapshot,
                decoder,
                "relative_humidity_2m",
                time,
                latitude,
                longitude,
            )?;
            let dewpoint = dew_point(temperature, relative_humidity);
            return Ok(vapor_pressure_deficit(temperature, dewpoint));
        }
        "et0_fao_evapotranspiration" => {
            let temperature = read_direct(
                snapshot,
                decoder,
                "temperature_2m",
                time,
                latitude,
                longitude,
            )?;
            let relative_humidity = read_direct(
                snapshot,
                decoder,
                "relative_humidity_2m",
                time,
                latitude,
                longitude,
            )?;
            let shortwave_radiation = read_direct(
                snapshot,
                decoder,
                "shortwave_radiation",
                time,
                latitude,
                longitude,
            )?;
            let wind_speed = read_variable_value(
                snapshot,
                decoder,
                "wind_speed_10m",
                time,
                latitude,
                longitude,
            )?;
            let sampling = weather_model_sampling(snapshot, decoder, latitude, longitude)?;
            let extraterrestrial = extra_terrestrial_radiation_backwards(
                time,
                3600,
                sampling.latitude as f32,
                sampling.longitude as f32,
            );
            return Ok(et0_evapotranspiration(
                temperature,
                wind_speed,
                dew_point(temperature, relative_humidity),
                shortwave_radiation,
                sampling.target_elevation,
                extraterrestrial,
                3600,
            ));
        }
        "snowfall" => {
            return Ok(read_direct(
                snapshot,
                decoder,
                "snowfall_water_equivalent",
                time,
                latitude,
                longitude,
            )? * 0.7);
        }
        "showers" if current_weather_model() == WeatherModel::EcmwfIfs025 => {
            // Open-Meteo's EcmwfReader exposes zero convective precipitation
            // for deterministic IFS. The raw showers field is AIFS-only.
            return Ok(0.0);
        }
        "rain" => {
            let swe = read_direct(
                snapshot,
                decoder,
                "snowfall_water_equivalent",
                time,
                latitude,
                longitude,
            )?;
            let precipitation = read_direct(
                snapshot,
                decoder,
                "precipitation",
                time,
                latitude,
                longitude,
            )?;
            let showers = if current_weather_model() == WeatherModel::EcmwfIfs025 {
                0.0
            } else {
                read_direct(snapshot, decoder, "showers", time, latitude, longitude)?
            };
            if !precipitation.is_finite() || !swe.is_finite() || !showers.is_finite() {
                return Ok(f32::NAN);
            }
            return Ok((precipitation - swe - showers).max(0.0));
        }
        "wind_speed_10m" | "windspeed_10m" => {
            let u = read_direct(
                snapshot,
                decoder,
                "wind_u_component_10m",
                time,
                latitude,
                longitude,
            )?;
            let v = read_direct(
                snapshot,
                decoder,
                "wind_v_component_10m",
                time,
                latitude,
                longitude,
            )?;
            return Ok((u * u + v * v).sqrt());
        }
        "wind_direction_10m" | "winddirection_10m" => {
            let u = read_direct(
                snapshot,
                decoder,
                "wind_u_component_10m",
                time,
                latitude,
                longitude,
            )?;
            let v = read_direct(
                snapshot,
                decoder,
                "wind_v_component_10m",
                time,
                latitude,
                longitude,
            )?;
            return Ok(wind_direction(u, v));
        }
        "wind_speed_80m" | "windspeed_80m" => {
            let u = read_direct(
                snapshot,
                decoder,
                "wind_u_component_80m",
                time,
                latitude,
                longitude,
            )?;
            let v = read_direct(
                snapshot,
                decoder,
                "wind_v_component_80m",
                time,
                latitude,
                longitude,
            )?;
            return Ok((u * u + v * v).sqrt());
        }
        "wind_direction_80m" | "winddirection_80m" => {
            let u = read_direct(
                snapshot,
                decoder,
                "wind_u_component_80m",
                time,
                latitude,
                longitude,
            )?;
            let v = read_direct(
                snapshot,
                decoder,
                "wind_v_component_80m",
                time,
                latitude,
                longitude,
            )?;
            return Ok(wind_direction(u, v));
        }
        "wind_speed_100m" | "windspeed_100m" => {
            let u = read_direct(
                snapshot,
                decoder,
                "wind_u_component_100m",
                time,
                latitude,
                longitude,
            )?;
            let v = read_direct(
                snapshot,
                decoder,
                "wind_v_component_100m",
                time,
                latitude,
                longitude,
            )?;
            return Ok((u * u + v * v).sqrt());
        }
        "wind_direction_100m"
        | "winddirection_100m"
        | "wind_direction_120m"
        | "winddirection_120m" => {
            if current_weather_model() == WeatherModel::EcmwfIfs025 && variable.contains("120m") {
                bail!("variable/time is not available for ECMWF IFS025: {variable} {time}");
            }
            let u = read_direct(
                snapshot,
                decoder,
                "wind_u_component_100m",
                time,
                latitude,
                longitude,
            )?;
            let v = read_direct(
                snapshot,
                decoder,
                "wind_v_component_100m",
                time,
                latitude,
                longitude,
            )?;
            return Ok(wind_direction(u, v));
        }
        "wind_speed_120m" | "windspeed_120m" => {
            if current_weather_model() == WeatherModel::EcmwfIfs025 {
                bail!("variable/time is not available for ECMWF IFS025: {variable} {time}");
            }
            let u = read_direct(
                snapshot,
                decoder,
                "wind_u_component_100m",
                time,
                latitude,
                longitude,
            )?;
            let v = read_direct(
                snapshot,
                decoder,
                "wind_v_component_100m",
                time,
                latitude,
                longitude,
            )?;
            return Ok((u * u + v * v).sqrt() * wind_scale_factor(100.0, 120.0));
        }
        "temperature_120m" => {
            if current_weather_model() == WeatherModel::EcmwfIfs025 {
                bail!("variable/time is not available for ECMWF IFS025: {variable} {time}");
            }
            return read_direct(
                snapshot,
                decoder,
                "temperature_100m",
                time,
                latitude,
                longitude,
            );
        }
        "diffuse_radiation" if derives_diffuse_radiation(current_weather_model()) => {
            let shortwave = read_direct(
                snapshot,
                decoder,
                "shortwave_radiation",
                time,
                latitude,
                longitude,
            )?;
            let sampling = weather_model_sampling(snapshot, decoder, latitude, longitude)?;
            return Ok(backwards_diffuse_radiation(
                shortwave,
                time,
                3600,
                sampling.latitude as f32,
                sampling.longitude as f32,
            ));
        }
        "direct_radiation" => {
            let shortwave = read_direct(
                snapshot,
                decoder,
                "shortwave_radiation",
                time,
                latitude,
                longitude,
            )?;
            let diffuse = if derives_diffuse_radiation(current_weather_model()) {
                let sampling = weather_model_sampling(snapshot, decoder, latitude, longitude)?;
                backwards_diffuse_radiation(
                    shortwave,
                    time,
                    3600,
                    sampling.latitude as f32,
                    sampling.longitude as f32,
                )
            } else {
                read_direct(
                    snapshot,
                    decoder,
                    "diffuse_radiation",
                    time,
                    latitude,
                    longitude,
                )?
            };
            return Ok((shortwave - diffuse).max(0.0));
        }
        "shortwave_radiation_instant"
        | "diffuse_radiation_instant"
        | "direct_radiation_instant" => {
            let raw = match variable {
                "shortwave_radiation_instant" => read_direct(
                    snapshot,
                    decoder,
                    "shortwave_radiation",
                    time,
                    latitude,
                    longitude,
                )?,
                "diffuse_radiation_instant" => read_variable_value(
                    snapshot,
                    decoder,
                    "diffuse_radiation",
                    time,
                    latitude,
                    longitude,
                )?,
                _ => read_variable_value(
                    snapshot,
                    decoder,
                    "direct_radiation",
                    time,
                    latitude,
                    longitude,
                )?,
            };
            let sampling = weather_model_sampling(snapshot, decoder, latitude, longitude)?;
            return Ok(raw
                * backwards_to_instant_factor(
                    time,
                    3600,
                    sampling.latitude as f32,
                    sampling.longitude as f32,
                ));
        }
        "direct_normal_irradiance" | "direct_normal_irradiance_instant" => {
            let direct = read_variable_value(
                snapshot,
                decoder,
                "direct_radiation",
                time,
                latitude,
                longitude,
            )?;
            let sampling = weather_model_sampling(snapshot, decoder, latitude, longitude)?;
            return Ok(backwards_direct_normal_irradiance(
                direct,
                time,
                3600,
                sampling.latitude as f32,
                sampling.longitude as f32,
                variable.ends_with("_instant"),
            ));
        }
        "global_tilted_irradiance" | "global_tilted_irradiance_instant" => {
            let direct = read_variable_value(
                snapshot,
                decoder,
                "direct_radiation",
                time,
                latitude,
                longitude,
            )?;
            let diffuse = read_variable_value(
                snapshot,
                decoder,
                "diffuse_radiation",
                time,
                latitude,
                longitude,
            )?;
            let sampling = weather_model_sampling(snapshot, decoder, latitude, longitude)?;
            return Ok(backwards_global_tilted_irradiance(
                direct,
                diffuse,
                time,
                3600,
                sampling.latitude as f32,
                sampling.longitude as f32,
                0.0,
                0.0,
                variable.ends_with("_instant"),
            ));
        }
        "sunshine_duration" => {
            let direct = read_variable_value(
                snapshot,
                decoder,
                "direct_radiation",
                time,
                latitude,
                longitude,
            )?;
            let sampling = weather_model_sampling(snapshot, decoder, latitude, longitude)?;
            return Ok(backwards_sunshine_duration(
                direct,
                time,
                3600,
                sampling.latitude as f32,
                sampling.longitude as f32,
            ));
        }
        "is_day" => {
            let sampling = weather_model_sampling(snapshot, decoder, latitude, longitude)?;
            return Ok(is_day(
                time,
                sampling.latitude as f32,
                sampling.longitude as f32,
            ));
        }
        "cloudcover" => {
            return read_direct(snapshot, decoder, "cloud_cover", time, latitude, longitude)
        }
        "cloudcover_low" => {
            return read_direct(
                snapshot,
                decoder,
                "cloud_cover_low",
                time,
                latitude,
                longitude,
            )
        }
        "cloudcover_mid" => {
            return read_direct(
                snapshot,
                decoder,
                "cloud_cover_mid",
                time,
                latitude,
                longitude,
            )
        }
        "cloudcover_high" => {
            return read_direct(
                snapshot,
                decoder,
                "cloud_cover_high",
                time,
                latitude,
                longitude,
            )
        }
        "relativehumidity_2m" => {
            return read_direct(
                snapshot,
                decoder,
                "relative_humidity_2m",
                time,
                latitude,
                longitude,
            )
        }
        "precip_phase" => {
            let code = read_weather_code(snapshot, decoder, time, latitude, longitude)?;
            return Ok(precip_phase(code));
        }
        "thunderstorm_code" => {
            let code = read_weather_code(snapshot, decoder, time, latitude, longitude)?;
            return Ok(if [95.0, 96.0, 99.0].contains(&code) {
                code
            } else {
                0.0
            });
        }
        _ => {}
    }
    read_direct(snapshot, decoder, variable, time, latitude, longitude)
}

fn read_derived_pressure(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<f32>> {
    let Some((name, level_text)) = variable.rsplit_once('_') else {
        return Ok(None);
    };
    let Some(level) = level_text
        .strip_suffix("hPa")
        .and_then(|value| value.parse::<u16>().ok())
    else {
        return Ok(None);
    };
    let raw = |prefix: &str| format!("{prefix}_{level_text}");
    let value = match name {
        "wind_speed" | "windspeed" => {
            let u = read_direct(
                snapshot,
                decoder,
                &raw("wind_u_component"),
                time,
                latitude,
                longitude,
            )?;
            let v = read_direct(
                snapshot,
                decoder,
                &raw("wind_v_component"),
                time,
                latitude,
                longitude,
            )?;
            Some((u * u + v * v).sqrt())
        }
        "wind_direction" | "winddirection" => {
            let u = read_direct(
                snapshot,
                decoder,
                &raw("wind_u_component"),
                time,
                latitude,
                longitude,
            )?;
            let v = read_direct(
                snapshot,
                decoder,
                &raw("wind_v_component"),
                time,
                latitude,
                longitude,
            )?;
            Some(wind_direction(u, v))
        }
        "dew_point" | "dewpoint" => {
            let temperature = read_direct(
                snapshot,
                decoder,
                &raw("temperature"),
                time,
                latitude,
                longitude,
            )?;
            let relative_humidity = read_direct(
                snapshot,
                decoder,
                &raw("relative_humidity"),
                time,
                latitude,
                longitude,
            )?;
            Some(dew_point(temperature, relative_humidity))
        }
        "cloudcover" | "cloud_cover" if current_weather_model() == WeatherModel::EcmwfIfs025 => {
            let relative_humidity = read_direct(
                snapshot,
                decoder,
                &raw("relative_humidity"),
                time,
                latitude,
                longitude,
            )?;
            Some(relative_humidity_to_cloud_cover(
                relative_humidity,
                level as f32,
            ))
        }
        "cloudcover" => Some(read_direct(
            snapshot,
            decoder,
            &raw("cloud_cover"),
            time,
            latitude,
            longitude,
        )?),
        "relativehumidity" => Some(read_direct(
            snapshot,
            decoder,
            &raw("relative_humidity"),
            time,
            latitude,
            longitude,
        )?),
        _ => None,
    };
    Ok(value)
}

/// Read a regional grid directly from the local OM bundles.
///
/// The returned values are row-major in the supplied latitude/longitude order.
/// Each underlying variable is decoded as one bounding rectangle and then
/// sampled with the same nearest-grid-cell rule as the point API.
pub fn read_variable_grid(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<f32>> {
    if latitudes.is_empty() || longitudes.is_empty() {
        bail!("regional grid dimensions must not be empty");
    }
    if current_weather_model() == WeatherModel::EcmwfIfs025 {
        let mut frames =
            read_variable_grid_series(snapshot, decoder, variable, &[time], latitudes, longitudes)?;
        return Ok(frames.remove(0));
    }
    let combine2 = |left: Vec<f32>, right: Vec<f32>, op: fn(f32, f32) -> f32| {
        left.into_iter().zip(right).map(|(a, b)| op(a, b)).collect()
    };
    match variable {
        "dew_point_2m" | "dewpoint_2m" => {
            let temperature = read_direct_grid(
                snapshot,
                decoder,
                "temperature_2m",
                time,
                latitudes,
                longitudes,
                true,
            )?;
            let humidity = read_direct_grid(
                snapshot,
                decoder,
                "relative_humidity_2m",
                time,
                latitudes,
                longitudes,
                true,
            )?;
            Ok(combine2(temperature, humidity, dew_point))
        }
        "surface_pressure" => {
            let temperature = read_direct_grid(
                snapshot,
                decoder,
                "temperature_2m",
                time,
                latitudes,
                longitudes,
                true,
            )?;
            let pressure = read_direct_grid(
                snapshot,
                decoder,
                "pressure_msl",
                time,
                latitudes,
                longitudes,
                true,
            )?;
            let elevation =
                read_weather_surface_elevation_grid(snapshot, decoder, latitudes, longitudes)?;
            Ok(temperature
                .into_iter()
                .zip(pressure)
                .zip(elevation)
                .map(|((temperature, pressure), elevation)| {
                    surface_pressure(temperature, pressure, elevation)
                })
                .collect())
        }
        "weather_code" | "weathercode" | "precip_phase" | "thunderstorm_code" => {
            read_weather_code_grid(snapshot, decoder, time, latitudes, longitudes)
        }
        "snowfall" => Ok(read_direct_grid(
            snapshot,
            decoder,
            "snowfall_water_equivalent",
            time,
            latitudes,
            longitudes,
            true,
        )?
        .into_iter()
        .map(|value| value * 0.7)
        .collect()),
        "showers" if current_weather_model() == WeatherModel::EcmwfIfs025 => {
            Ok(vec![0.0; latitudes.len() * longitudes.len()])
        }
        "rain" => {
            let precipitation = read_direct_grid(
                snapshot,
                decoder,
                "precipitation",
                time,
                latitudes,
                longitudes,
                true,
            )?;
            let snowfall = read_direct_grid(
                snapshot,
                decoder,
                "snowfall_water_equivalent",
                time,
                latitudes,
                longitudes,
                true,
            )?;
            let showers = if current_weather_model() == WeatherModel::EcmwfIfs025 {
                vec![0.0; precipitation.len()]
            } else {
                read_direct_grid(
                    snapshot, decoder, "showers", time, latitudes, longitudes, true,
                )?
            };
            Ok(precipitation
                .into_iter()
                .zip(snowfall)
                .zip(showers)
                .map(|((precipitation, snowfall), showers)| {
                    if precipitation.is_finite() && snowfall.is_finite() && showers.is_finite() {
                        (precipitation - snowfall - showers).max(0.0)
                    } else {
                        f32::NAN
                    }
                })
                .collect())
        }
        "cloudcover" => read_direct_grid(
            snapshot,
            decoder,
            "cloud_cover",
            time,
            latitudes,
            longitudes,
            true,
        ),
        "cloudcover_low" => read_direct_grid(
            snapshot,
            decoder,
            "cloud_cover_low",
            time,
            latitudes,
            longitudes,
            true,
        ),
        "cloudcover_mid" => read_direct_grid(
            snapshot,
            decoder,
            "cloud_cover_mid",
            time,
            latitudes,
            longitudes,
            true,
        ),
        "cloudcover_high" => read_direct_grid(
            snapshot,
            decoder,
            "cloud_cover_high",
            time,
            latitudes,
            longitudes,
            true,
        ),
        "relativehumidity_2m" => read_direct_grid(
            snapshot,
            decoder,
            "relative_humidity_2m",
            time,
            latitudes,
            longitudes,
            true,
        ),
        _ => read_direct_grid(
            snapshot, decoder, variable, time, latitudes, longitudes, true,
        ),
    }
}

fn series_dt_seconds(times: &[DateTime<Utc>]) -> i64 {
    times
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).num_seconds())
        .filter(|seconds| *seconds > 0)
        .min()
        .unwrap_or(3600)
}

/// Read several regional output hours while allowing native 3D OM files to be
/// decoded as one time slab. Open-Meteo stores the complete run time axis in a
/// single chunk, so decoding one hour at a time repeatedly inflates the same
/// chunk and is prohibitively slow for WebP production.
pub fn read_variable_grid_series(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    requested_latitudes: &[f64],
    requested_longitudes: &[f64],
) -> Result<Vec<Vec<f32>>> {
    if times.is_empty() || requested_latitudes.is_empty() || requested_longitudes.is_empty() {
        bail!("regional grid series dimensions must not be empty");
    }
    let selected_sampling = if requested_latitudes.len() == 1 && requested_longitudes.len() == 1 {
        current_product_sampling(current_weather_model().primary_product())
    } else {
        None
    };
    let selected_latitudes = selected_sampling.map(|sampling| [sampling.latitude]);
    let selected_longitudes = selected_sampling.map(|sampling| [sampling.longitude]);
    let latitudes = selected_latitudes
        .as_ref()
        .map_or(requested_latitudes, |values| values.as_slice());
    let longitudes = selected_longitudes
        .as_ref()
        .map_or(requested_longitudes, |values| values.as_slice());
    if variable == "precipitation_probability" && current_weather_model() == WeatherModel::Gfs {
        return read_gfs_precipitation_probability_grid_series(
            snapshot, decoder, times, latitudes, longitudes,
        );
    }
    let sampled_latitudes;
    let sampled_longitudes;
    let (latitudes, longitudes) = if current_weather_model() == WeatherModel::EcmwfIfs025
        && latitudes.len() == 1
        && longitudes.len() == 1
    {
        let sampling_product = product_for_variable(snapshot, &seed_variable_for_times(variable))
            .ok()
            .map(|(product, _)| product);
        if let Some(sampling) = sampling_product.and_then(current_product_sampling) {
            sampled_latitudes = [sampling.latitude];
            sampled_longitudes = [sampling.longitude];
            (&sampled_latitudes[..], &sampled_longitudes[..])
        } else {
            (latitudes, longitudes)
        }
    } else {
        (latitudes, longitudes)
    };
    let combine2 = |left: Vec<Vec<f32>>, right: Vec<Vec<f32>>, op: fn(f32, f32) -> f32| {
        left.into_iter()
            .zip(right)
            .map(|(left, right)| left.into_iter().zip(right).map(|(a, b)| op(a, b)).collect())
            .collect()
    };
    let dt_seconds = series_dt_seconds(times);
    if let Some((name, level_text)) = variable.rsplit_once('_') {
        if level_text
            .strip_suffix("hPa")
            .and_then(|value| value.parse::<u16>().ok())
            .is_some()
        {
            let raw = |prefix: &str| format!("{prefix}_{level_text}");
            match name {
                "wind_speed" | "windspeed" => {
                    return Ok(combine2(
                        read_direct_grid_series(
                            snapshot,
                            decoder,
                            &raw("wind_u_component"),
                            times,
                            latitudes,
                            longitudes,
                            true,
                        )?,
                        read_direct_grid_series(
                            snapshot,
                            decoder,
                            &raw("wind_v_component"),
                            times,
                            latitudes,
                            longitudes,
                            true,
                        )?,
                        |u, v| (u * u + v * v).sqrt(),
                    ));
                }
                "wind_direction" | "winddirection" => {
                    return Ok(wind_direction_grid_series(
                        &read_direct_grid_series(
                            snapshot,
                            decoder,
                            &raw("wind_u_component"),
                            times,
                            latitudes,
                            longitudes,
                            true,
                        )?,
                        &read_direct_grid_series(
                            snapshot,
                            decoder,
                            &raw("wind_v_component"),
                            times,
                            latitudes,
                            longitudes,
                            true,
                        )?,
                    ));
                }
                "dew_point" | "dewpoint" => {
                    return Ok(combine2(
                        read_direct_grid_series(
                            snapshot,
                            decoder,
                            &raw("temperature"),
                            times,
                            latitudes,
                            longitudes,
                            true,
                        )?,
                        read_direct_grid_series(
                            snapshot,
                            decoder,
                            &raw("relative_humidity"),
                            times,
                            latitudes,
                            longitudes,
                            true,
                        )?,
                        dew_point,
                    ));
                }
                "cloudcover" | "cloud_cover"
                    if current_weather_model() == WeatherModel::EcmwfIfs025 =>
                {
                    let level = level_text
                        .strip_suffix("hPa")
                        .and_then(|value| value.parse::<f32>().ok())
                        .context("invalid ECMWF pressure level")?;
                    return Ok(read_direct_grid_series(
                        snapshot,
                        decoder,
                        &raw("relative_humidity"),
                        times,
                        latitudes,
                        longitudes,
                        true,
                    )?
                    .into_iter()
                    .map(|values| {
                        values
                            .into_iter()
                            .map(|rh| relative_humidity_to_cloud_cover(rh, level))
                            .collect()
                    })
                    .collect());
                }
                "cloudcover" => {
                    return read_direct_grid_series(
                        snapshot,
                        decoder,
                        &raw("cloud_cover"),
                        times,
                        latitudes,
                        longitudes,
                        true,
                    );
                }
                "relativehumidity" => {
                    return read_direct_grid_series(
                        snapshot,
                        decoder,
                        &raw("relative_humidity"),
                        times,
                        latitudes,
                        longitudes,
                        true,
                    );
                }
                _ => {}
            }
        }
    }
    match variable {
        "dew_point_2m" | "dewpoint_2m" => Ok(combine2(
            read_direct_grid_series(
                snapshot,
                decoder,
                "temperature_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            read_direct_grid_series(
                snapshot,
                decoder,
                "relative_humidity_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            dew_point,
        )),
        "wet_bulb_temperature_2m" => Ok(combine2(
            read_direct_grid_series(
                snapshot,
                decoder,
                "temperature_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            read_direct_grid_series(
                snapshot,
                decoder,
                "relative_humidity_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            wet_bulb_temperature,
        )),
        "vapour_pressure_deficit" | "vapor_pressure_deficit" => Ok(combine2(
            read_direct_grid_series(
                snapshot,
                decoder,
                "temperature_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            read_direct_grid_series(
                snapshot,
                decoder,
                "relative_humidity_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            |temperature, humidity| {
                vapor_pressure_deficit(temperature, dew_point(temperature, humidity))
            },
        )),
        "skin_temperature" | "soil_temperature_0cm" => read_direct_grid_series(
            snapshot,
            decoder,
            "surface_temperature",
            times,
            latitudes,
            longitudes,
            true,
        ),
        "soil_temperature_0_to_100cm" | "soil_moisture_0_to_100cm" => {
            let prefix = if variable.starts_with("soil_temperature") {
                "soil_temperature"
            } else {
                "soil_moisture"
            };
            let shallow = read_direct_grid_series(
                snapshot,
                decoder,
                &format!("{prefix}_0_to_7cm"),
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let middle = read_direct_grid_series(
                snapshot,
                decoder,
                &format!("{prefix}_7_to_28cm"),
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let deep = read_direct_grid_series(
                snapshot,
                decoder,
                &format!("{prefix}_28_to_100cm"),
                times,
                latitudes,
                longitudes,
                true,
            )?;
            Ok(shallow
                .into_iter()
                .zip(middle)
                .zip(deep)
                .map(|((shallow, middle), deep)| {
                    shallow
                        .into_iter()
                        .zip(middle)
                        .zip(deep)
                        .map(|((shallow, middle), deep)| {
                            weighted_soil_layer_0_to_100cm(shallow, middle, deep)
                        })
                        .collect()
                })
                .collect())
        }
        "growing_degree_days_base_0_limit_50" => Ok(read_direct_grid_series(
            snapshot,
            decoder,
            "temperature_2m",
            times,
            latitudes,
            longitudes,
            true,
        )?
        .into_iter()
        .map(|frame| {
            frame
                .into_iter()
                .map(|temperature| temperature.clamp(0.0, 50.0) / 24.0)
                .collect()
        })
        .collect()),
        "surface_pressure" => {
            let temperature = read_direct_grid_series(
                snapshot,
                decoder,
                "temperature_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let pressure = read_direct_grid_series(
                snapshot,
                decoder,
                "pressure_msl",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let primary_product = current_weather_model().primary_product();
            let elevation = if let Some(sampling) = current_product_sampling(primary_product) {
                vec![surface_pressure_elevation(sampling); latitudes.len() * longitudes.len()]
            } else {
                read_weather_surface_elevation_grid(snapshot, decoder, latitudes, longitudes)?
            };
            Ok(temperature
                .into_iter()
                .zip(pressure)
                .map(|(temperature, pressure)| {
                    temperature
                        .into_iter()
                        .zip(pressure)
                        .zip(elevation.iter().copied())
                        .map(|((temperature, pressure), elevation)| {
                            surface_pressure(temperature, pressure, elevation)
                        })
                        .collect()
                })
                .collect())
        }
        "weather_code" | "weathercode" | "precip_phase" | "thunderstorm_code" => {
            read_weather_code_grid_series(snapshot, decoder, times, latitudes, longitudes)
        }
        "apparent_temperature" => {
            let temperature = read_direct_grid_series(
                snapshot,
                decoder,
                "temperature_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let humidity = read_direct_grid_series(
                snapshot,
                decoder,
                "relative_humidity_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let u = read_direct_grid_series(
                snapshot,
                decoder,
                "wind_u_component_10m",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let v = read_direct_grid_series(
                snapshot,
                decoder,
                "wind_v_component_10m",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let radiation = read_direct_grid_series(
                snapshot,
                decoder,
                "shortwave_radiation",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            Ok(temperature
                .into_iter()
                .zip(humidity)
                .zip(u)
                .zip(v)
                .zip(radiation)
                .map(|((((temperature, humidity), u), v), radiation)| {
                    temperature
                        .into_iter()
                        .zip(humidity)
                        .zip(u)
                        .zip(v)
                        .zip(radiation)
                        .map(|((((temperature, humidity), u), v), radiation)| {
                            apparent_temperature(
                                temperature,
                                humidity,
                                (u * u + v * v).sqrt(),
                                Some(radiation),
                            )
                        })
                        .collect()
                })
                .collect())
        }
        "diffuse_radiation" if derives_diffuse_radiation(current_weather_model()) => {
            let shortwave = read_direct_grid_series(
                snapshot,
                decoder,
                "shortwave_radiation",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let width = longitudes.len();
            Ok(shortwave
                .into_iter()
                .zip(times)
                .map(|(frame, time)| {
                    frame
                        .into_iter()
                        .enumerate()
                        .map(|(index, value)| {
                            backwards_diffuse_radiation(
                                value,
                                *time,
                                dt_seconds,
                                latitudes[index / width] as f32,
                                longitudes[index % width] as f32,
                            )
                        })
                        .collect()
                })
                .collect())
        }
        "diffuse_radiation" => read_direct_grid_series(
            snapshot,
            decoder,
            "diffuse_radiation",
            times,
            latitudes,
            longitudes,
            true,
        ),
        "direct_radiation" => Ok(combine2(
            read_direct_grid_series(
                snapshot,
                decoder,
                "shortwave_radiation",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            read_variable_grid_series(
                snapshot,
                decoder,
                "diffuse_radiation",
                times,
                latitudes,
                longitudes,
            )?,
            |shortwave, diffuse| (shortwave - diffuse).max(0.0),
        )),
        "shortwave_radiation_instant"
        | "direct_radiation_instant"
        | "diffuse_radiation_instant" => {
            let source = match variable {
                "direct_radiation_instant" => "direct_radiation",
                "diffuse_radiation_instant" => "diffuse_radiation",
                _ => "shortwave_radiation",
            };
            let values =
                read_variable_grid_series(snapshot, decoder, source, times, latitudes, longitudes)?;
            let width = longitudes.len();
            Ok(values
                .into_iter()
                .zip(times)
                .map(|(frame, time)| {
                    frame
                        .into_iter()
                        .enumerate()
                        .map(|(index, value)| {
                            value
                                * backwards_to_instant_factor(
                                    *time,
                                    dt_seconds,
                                    latitudes[index / width] as f32,
                                    longitudes[index % width] as f32,
                                )
                        })
                        .collect()
                })
                .collect())
        }
        "direct_normal_irradiance" | "direct_normal_irradiance_instant" => {
            let direct = read_variable_grid_series(
                snapshot,
                decoder,
                "direct_radiation",
                times,
                latitudes,
                longitudes,
            )?;
            let instant = variable.ends_with("_instant");
            let width = longitudes.len();
            Ok(direct
                .into_iter()
                .zip(times)
                .map(|(frame, time)| {
                    frame
                        .into_iter()
                        .enumerate()
                        .map(|(index, value)| {
                            backwards_direct_normal_irradiance(
                                value,
                                *time,
                                dt_seconds,
                                latitudes[index / width] as f32,
                                longitudes[index % width] as f32,
                                instant,
                            )
                        })
                        .collect()
                })
                .collect())
        }
        "global_tilted_irradiance" | "global_tilted_irradiance_instant" => {
            let direct = read_variable_grid_series(
                snapshot,
                decoder,
                "direct_radiation",
                times,
                latitudes,
                longitudes,
            )?;
            let diffuse = read_variable_grid_series(
                snapshot,
                decoder,
                "diffuse_radiation",
                times,
                latitudes,
                longitudes,
            )?;
            let instant = variable.ends_with("_instant");
            let width = longitudes.len();
            Ok(direct
                .into_iter()
                .zip(diffuse)
                .zip(times)
                .map(|((direct, diffuse), time)| {
                    direct
                        .into_iter()
                        .zip(diffuse)
                        .enumerate()
                        .map(|(index, (direct, diffuse))| {
                            backwards_global_tilted_irradiance(
                                direct,
                                diffuse,
                                *time,
                                dt_seconds,
                                latitudes[index / width] as f32,
                                longitudes[index % width] as f32,
                                0.0,
                                0.0,
                                instant,
                            )
                        })
                        .collect()
                })
                .collect())
        }
        "sunshine_duration" => {
            let direct = read_variable_grid_series(
                snapshot,
                decoder,
                "direct_radiation",
                times,
                latitudes,
                longitudes,
            )?;
            let width = longitudes.len();
            Ok(direct
                .into_iter()
                .zip(times)
                .map(|(frame, time)| {
                    frame
                        .into_iter()
                        .enumerate()
                        .map(|(index, value)| {
                            backwards_sunshine_duration(
                                value,
                                *time,
                                dt_seconds,
                                latitudes[index / width] as f32,
                                longitudes[index % width] as f32,
                            )
                        })
                        .collect()
                })
                .collect())
        }
        "is_day" => {
            let width = longitudes.len();
            Ok(times
                .iter()
                .map(|time| {
                    (0..latitudes.len() * longitudes.len())
                        .map(|index| {
                            is_day(
                                *time,
                                latitudes[index / width] as f32,
                                longitudes[index % width] as f32,
                            )
                        })
                        .collect()
                })
                .collect())
        }
        "et0_fao_evapotranspiration" => {
            let temperature = read_direct_grid_series(
                snapshot,
                decoder,
                "temperature_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let humidity = read_direct_grid_series(
                snapshot,
                decoder,
                "relative_humidity_2m",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let shortwave = read_direct_grid_series(
                snapshot,
                decoder,
                "shortwave_radiation",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let wind = read_variable_grid_series(
                snapshot,
                decoder,
                "wind_speed_10m",
                times,
                latitudes,
                longitudes,
            )?;
            let elevation = if let Some(sampling) =
                current_product_sampling(current_weather_model().primary_product())
            {
                vec![sampling.target_elevation; latitudes.len() * longitudes.len()]
            } else {
                read_weather_surface_elevation_grid(snapshot, decoder, latitudes, longitudes)?
            };
            let width = longitudes.len();
            Ok(temperature
                .into_iter()
                .zip(humidity)
                .zip(shortwave)
                .zip(wind)
                .zip(times)
                .map(|((((temperature, humidity), shortwave), wind), time)| {
                    temperature
                        .into_iter()
                        .zip(humidity)
                        .zip(shortwave)
                        .zip(wind)
                        .zip(elevation.iter().copied())
                        .enumerate()
                        .map(
                            |(index, ((((temperature, humidity), shortwave), wind), elevation))| {
                                et0_evapotranspiration(
                                    temperature,
                                    wind,
                                    dew_point(temperature, humidity),
                                    shortwave,
                                    elevation,
                                    extra_terrestrial_radiation_backwards(
                                        *time,
                                        dt_seconds,
                                        latitudes[index / width] as f32,
                                        longitudes[index % width] as f32,
                                    ),
                                    dt_seconds,
                                )
                            },
                        )
                        .collect()
                })
                .collect())
        }
        "rain" => {
            let precipitation = read_direct_grid_series(
                snapshot,
                decoder,
                "precipitation",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let snowfall = read_direct_grid_series(
                snapshot,
                decoder,
                "snowfall_water_equivalent",
                times,
                latitudes,
                longitudes,
                true,
            )?;
            let showers = if current_weather_model() == WeatherModel::EcmwfIfs025 {
                vec![vec![0.0; latitudes.len() * longitudes.len()]; times.len()]
            } else {
                read_direct_grid_series(
                    snapshot, decoder, "showers", times, latitudes, longitudes, true,
                )?
            };
            Ok(precipitation
                .into_iter()
                .zip(snowfall)
                .zip(showers)
                .map(|((precipitation, snowfall), showers)| {
                    precipitation
                        .into_iter()
                        .zip(snowfall)
                        .zip(showers)
                        .map(|((precipitation, snowfall), showers)| {
                            if precipitation.is_finite()
                                && snowfall.is_finite()
                                && showers.is_finite()
                            {
                                (precipitation - snowfall - showers).max(0.0)
                            } else {
                                f32::NAN
                            }
                        })
                        .collect()
                })
                .collect())
        }
        "showers" if current_weather_model() == WeatherModel::EcmwfIfs025 => Ok(vec![
            vec![0.0; latitudes.len() * longitudes.len()];
            times.len()
        ]),
        "wind_speed_10m" | "windspeed_10m" => Ok(combine2(
            read_direct_grid_series(
                snapshot,
                decoder,
                "wind_u_component_10m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            read_direct_grid_series(
                snapshot,
                decoder,
                "wind_v_component_10m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            |u, v| (u * u + v * v).sqrt(),
        )),
        "wind_direction_10m" | "winddirection_10m" => Ok(wind_direction_grid_series(
            &read_direct_grid_series(
                snapshot,
                decoder,
                "wind_u_component_10m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            &read_direct_grid_series(
                snapshot,
                decoder,
                "wind_v_component_10m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
        )),
        "wind_speed_100m" | "windspeed_100m" => Ok(combine2(
            read_direct_grid_series(
                snapshot,
                decoder,
                "wind_u_component_100m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            read_direct_grid_series(
                snapshot,
                decoder,
                "wind_v_component_100m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            |u, v| (u * u + v * v).sqrt(),
        )),
        "wind_direction_100m" | "winddirection_100m" => Ok(wind_direction_grid_series(
            &read_direct_grid_series(
                snapshot,
                decoder,
                "wind_u_component_100m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
            &read_direct_grid_series(
                snapshot,
                decoder,
                "wind_v_component_100m",
                times,
                latitudes,
                longitudes,
                true,
            )?,
        )),
        "snowfall" => Ok(read_direct_grid_series(
            snapshot,
            decoder,
            "snowfall_water_equivalent",
            times,
            latitudes,
            longitudes,
            true,
        )?
        .into_iter()
        .map(|values| values.into_iter().map(|value| value * 0.7).collect())
        .collect()),
        "cloudcover" => read_direct_grid_series(
            snapshot,
            decoder,
            "cloud_cover",
            times,
            latitudes,
            longitudes,
            true,
        ),
        "cloudcover_low" => read_direct_grid_series(
            snapshot,
            decoder,
            "cloud_cover_low",
            times,
            latitudes,
            longitudes,
            true,
        ),
        "cloudcover_mid" => read_direct_grid_series(
            snapshot,
            decoder,
            "cloud_cover_mid",
            times,
            latitudes,
            longitudes,
            true,
        ),
        "cloudcover_high" => read_direct_grid_series(
            snapshot,
            decoder,
            "cloud_cover_high",
            times,
            latitudes,
            longitudes,
            true,
        ),
        "relativehumidity_2m" => read_direct_grid_series(
            snapshot,
            decoder,
            "relative_humidity_2m",
            times,
            latitudes,
            longitudes,
            true,
        ),
        _ => read_direct_grid_series(
            snapshot, decoder, variable, times, latitudes, longitudes, true,
        ),
    }
}

fn read_gfs_precipitation_probability_grid_series(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<Vec<f32>>> {
    let grid_len = latitudes.len() * longitudes.len();
    let read_product = |product_name: &str| -> Result<Option<Vec<Vec<f32>>>> {
        if snapshot.product(product_name).is_none() {
            return Ok(None);
        }
        times
            .iter()
            .map(|time| {
                read_gfs_product_history_grid_with_rounding(
                    snapshot,
                    decoder,
                    product_name,
                    "precipitation_probability",
                    "precipitation_probability",
                    *time,
                    latitudes,
                    longitudes,
                    true,
                )
                .map(|values| values.unwrap_or_else(|| vec![f32::NAN; grid_len]))
            })
            .collect::<Result<Vec<_>>>()
            .map(Some)
    };

    let high_resolution = read_product("ncep_gefs025")?;
    let low_resolution = read_product("ncep_gefs05")?;
    let mut output = match high_resolution {
        Some(values) => values,
        None => vec![vec![f32::NAN; grid_len]; times.len()],
    };
    let Some(low_resolution) = low_resolution else {
        if output
            .iter()
            .all(|frame| frame.iter().all(|value| value.is_nan()))
            && snapshot.product("ncep_gefs025").is_none()
        {
            bail!("GFS precipitation probability products are not available");
        }
        return Ok(output);
    };

    integrate_probability_if_nan_smooth(&mut output, &low_resolution, grid_len);
    Ok(output)
}

fn read_gfs_precipitation_probability_point_series(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    times: &[DateTime<Utc>],
    latitude: f64,
    longitude: f64,
) -> Result<Vec<f32>> {
    let read_product = |product_name: &str| -> Result<Option<Vec<Vec<f32>>>> {
        if snapshot.product(product_name).is_none() {
            return Ok(None);
        }
        let products = snapshot.product_snapshots(product_name);
        read_gfs_probability_history_point_series_with_rounding(
            &products,
            decoder,
            "precipitation_probability",
            times,
            latitude,
            longitude,
            true,
        )
        .map(|values| values.into_iter().map(|value| vec![value]).collect())
        .map(Some)
    };
    let high_resolution = read_product("ncep_gefs025")?;
    let low_resolution = read_product("ncep_gefs05")?;
    let mut output = high_resolution.unwrap_or_else(|| vec![vec![f32::NAN]; times.len()]);
    if let Some(low_resolution) = low_resolution {
        integrate_probability_if_nan_smooth(&mut output, &low_resolution, 1);
    } else if snapshot.product("ncep_gefs025").is_none() {
        bail!("GFS precipitation probability products are not available");
    }
    output
        .into_iter()
        .map(|mut frame| {
            frame
                .pop()
                .context("GFS precipitation probability point frame is empty")
        })
        .collect()
}

fn integrate_probability_if_nan_smooth(
    high_resolution: &mut [Vec<f32>],
    low_resolution: &[Vec<f32>],
    grid_len: usize,
) {
    debug_assert_eq!(high_resolution.len(), low_resolution.len());
    // Open-Meteo's GenericReaderMixerSameDomain reads GEFS025 first, fills
    // gaps from GEFS05, and blends the three output steps immediately before
    // a transition. Preserve that contract so the f240 resolution boundary
    // does not create an artificial jump.
    for point in 0..grid_len {
        let mut steps_since_gap = 3_i32;
        for index in (0..high_resolution.len()).rev() {
            steps_since_gap += 1;
            let low = low_resolution[index][point];
            if low.is_nan() {
                continue;
            }
            let high = &mut high_resolution[index][point];
            if high.is_nan() {
                steps_since_gap = 0;
                *high = low;
                continue;
            }
            if steps_since_gap <= 3 {
                *high = (low * (4 - steps_since_gap) as f32 + *high * steps_since_gap as f32) / 4.0;
            }
        }
    }
}

fn read_variable_point_series(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    latitude: f64,
    longitude: f64,
) -> Result<Option<Vec<f32>>> {
    let seed = seed_variable_for_times(variable);
    let (product, _) = product_for_variable(snapshot, &seed)?;
    if is_ecmwf_product(product) {
        let values = read_variable_grid_series(
            snapshot,
            decoder,
            variable,
            times,
            &[latitude],
            &[longitude],
        )?
        .into_iter()
        .map(|mut values| {
            if values.len() != 1 {
                bail!(
                    "point time-slab decode returned {} grid values",
                    values.len()
                );
            }
            Ok(values.remove(0))
        })
        .collect::<Result<Vec<_>>>()?;
        return Ok(Some(values));
    }
    if !is_gfs_product(product) {
        return Ok(None);
    }
    let latest_full_start = snapshot
        .product_snapshots(product)
        .into_iter()
        .find(|candidate| gfs_snapshot_is_full(candidate))
        .and_then(|candidate| {
            candidate
                .entries
                .values()
                .next()
                .map(|entry| entry.valid_time_utc - Duration::hours(entry.forecast_hour))
        })
        .ok_or_else(|| anyhow!("GFS complete run is not available for {product}"))?;
    let split = times.partition_point(|time| *time < latest_full_start);
    let mut values = Vec::with_capacity(times.len());
    // Hours before the latest complete run may be supplied by any of the
    // retained short snapshots. Keep the established newest-covering-run
    // selection for that small prefix instead of forcing an older full run.
    for time in &times[..split] {
        match read_variable_value(
            snapshot,
            Some(decoder),
            variable,
            *time,
            latitude,
            longitude,
        ) {
            Ok(value) => values.push(value),
            Err(error) if error.to_string().contains("variable/time is not available") => {
                values.push(f32::NAN)
            }
            Err(error) => return Err(error),
        }
    }
    if split < times.len() {
        values.extend(
            read_variable_grid_series(
                snapshot,
                decoder,
                variable,
                &times[split..],
                &[latitude],
                &[longitude],
            )?
            .into_iter()
            .map(|mut values| {
                if values.len() != 1 {
                    bail!(
                        "point time-slab decode returned {} grid values",
                        values.len()
                    );
                }
                Ok(values.remove(0))
            })
            .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok(Some(values))
}

fn read_derived_air_quality(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<f32>> {
    let value = match variable {
        "european_aqi" => finite_max(&[
            european_aqi_pm2_5(snapshot, decoder, time, latitude, longitude)?,
            european_aqi_pm10(snapshot, decoder, time, latitude, longitude)?,
            european_aqi_no2(snapshot, decoder, time, latitude, longitude)?,
            european_aqi_o3(snapshot, decoder, time, latitude, longitude)?,
            european_aqi_so2(snapshot, decoder, time, latitude, longitude)?,
        ]),
        "european_aqi_pm2_5" => european_aqi_pm2_5(snapshot, decoder, time, latitude, longitude)?,
        "european_aqi_pm10" => european_aqi_pm10(snapshot, decoder, time, latitude, longitude)?,
        "european_aqi_no2" | "european_aqi_nitrogen_dioxide" => {
            european_aqi_no2(snapshot, decoder, time, latitude, longitude)?
        }
        "european_aqi_o3" | "european_aqi_ozone" => {
            european_aqi_o3(snapshot, decoder, time, latitude, longitude)?
        }
        "european_aqi_so2" | "european_aqi_sulphur_dioxide" => {
            european_aqi_so2(snapshot, decoder, time, latitude, longitude)?
        }
        "us_aqi" => finite_max(&[
            us_aqi_pm2_5(snapshot, decoder, time, latitude, longitude)?,
            us_aqi_pm10(snapshot, decoder, time, latitude, longitude)?,
            us_aqi_no2(snapshot, decoder, time, latitude, longitude)?,
            us_aqi_o3(snapshot, decoder, time, latitude, longitude)?,
            us_aqi_so2(snapshot, decoder, time, latitude, longitude)?,
            us_aqi_co(snapshot, decoder, time, latitude, longitude)?,
        ]),
        "us_aqi_pm2_5" => us_aqi_pm2_5(snapshot, decoder, time, latitude, longitude)?,
        "us_aqi_pm10" => us_aqi_pm10(snapshot, decoder, time, latitude, longitude)?,
        "us_aqi_no2" | "us_aqi_nitrogen_dioxide" => {
            us_aqi_no2(snapshot, decoder, time, latitude, longitude)?
        }
        "us_aqi_o3" | "us_aqi_ozone" => us_aqi_o3(snapshot, decoder, time, latitude, longitude)?,
        "us_aqi_so2" | "us_aqi_sulphur_dioxide" => {
            us_aqi_so2(snapshot, decoder, time, latitude, longitude)?
        }
        "us_aqi_co" | "us_aqi_carbon_monoxide" => {
            us_aqi_co(snapshot, decoder, time, latitude, longitude)?
        }
        "chinese_aqi" => finite_max(&[
            chinese_aqi_pm2_5(snapshot, decoder, time, latitude, longitude)?,
            chinese_aqi_pm10(snapshot, decoder, time, latitude, longitude)?,
            chinese_aqi_no2(snapshot, decoder, time, latitude, longitude)?,
            chinese_aqi_o3(snapshot, decoder, time, latitude, longitude)?,
            chinese_aqi_so2(snapshot, decoder, time, latitude, longitude)?,
            chinese_aqi_co(snapshot, decoder, time, latitude, longitude)?,
        ]),
        "chinese_aqi_pm2_5" => chinese_aqi_pm2_5(snapshot, decoder, time, latitude, longitude)?,
        "chinese_aqi_pm10" => chinese_aqi_pm10(snapshot, decoder, time, latitude, longitude)?,
        "chinese_aqi_no2" | "chinese_aqi_nitrogen_dioxide" => {
            chinese_aqi_no2(snapshot, decoder, time, latitude, longitude)?
        }
        "chinese_aqi_o3" | "chinese_aqi_ozone" => {
            chinese_aqi_o3(snapshot, decoder, time, latitude, longitude)?
        }
        "chinese_aqi_so2" | "chinese_aqi_sulphur_dioxide" => {
            chinese_aqi_so2(snapshot, decoder, time, latitude, longitude)?
        }
        "chinese_aqi_co" | "chinese_aqi_carbon_monoxide" => {
            chinese_aqi_co(snapshot, decoder, time, latitude, longitude)?
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn european_aqi_pm2_5(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let mean = rolling_mean_before(snapshot, decoder, "pm2_5", time, latitude, longitude, 24)?;
    Ok(position_extrapolated(&[0.0, 10.0, 20.0, 25.0, 50.0, 75.0], mean) * 20.0)
}

fn european_aqi_pm10(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let mean = rolling_mean_before(snapshot, decoder, "pm10", time, latitude, longitude, 24)?;
    Ok(position_extrapolated(&[0.0, 20.0, 40.0, 50.0, 100.0, 150.0], mean) * 20.0)
}

fn european_aqi_no2(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let no2 = read_direct(
        snapshot,
        decoder,
        "nitrogen_dioxide",
        time,
        latitude,
        longitude,
    )?;
    Ok(position_extrapolated(&[0.0, 40.0, 90.0, 120.0, 230.0, 340.0], no2) * 20.0)
}

fn european_aqi_o3(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let o3 = read_direct(snapshot, decoder, "ozone", time, latitude, longitude)?;
    Ok(position_extrapolated(&[0.0, 50.0, 100.0, 130.0, 240.0, 380.0], o3) * 20.0)
}

fn european_aqi_so2(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let so2 = read_direct(
        snapshot,
        decoder,
        "sulphur_dioxide",
        time,
        latitude,
        longitude,
    )?;
    Ok(position_extrapolated(&[0.0, 100.0, 200.0, 350.0, 500.0, 750.0], so2) * 20.0)
}

fn us_aqi_pm2_5(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let mean = rolling_mean_before(snapshot, decoder, "pm2_5", time, latitude, longitude, 24)?;
    Ok(us_aqi_scale(position_extrapolated(
        &[0.0, 9.0, 35.5, 55.5, 125.5, 225.5, 325.5, 500.5],
        mean,
    )))
}

fn us_aqi_pm10(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let mean = rolling_mean_before(snapshot, decoder, "pm10", time, latitude, longitude, 24)?;
    Ok(us_aqi_scale(position_extrapolated(
        &[0.0, 55.0, 155.0, 255.0, 355.0, 425.0, 505.0, 605.0],
        mean,
    )))
}

fn us_aqi_no2(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let no2 = read_direct(
        snapshot,
        decoder,
        "nitrogen_dioxide",
        time,
        latitude,
        longitude,
    )?;
    Ok(us_aqi_scale(position_extrapolated(
        &[0.0, 54.0, 100.0, 360.0, 650.0, 1250.0, 1650.0, 2050.0],
        no2 / 1.88,
    )))
}

fn us_aqi_o3(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let o3 = read_direct(snapshot, decoder, "ozone", time, latitude, longitude)? / 1.96;
    let o3_8h =
        rolling_mean_before(snapshot, decoder, "ozone", time, latitude, longitude, 8)? / 1.96;
    let hourly = position_extrapolated(
        &[f32::NAN, f32::NAN, 125.0, 165.0, 205.0, 405.0, 505.0, 605.0],
        o3,
    );
    let averaged = position_extrapolated(
        &[0.0, 55.0, 70.0, 85.0, 105.0, 200.0, f32::NAN, f32::NAN],
        o3_8h,
    );
    if hourly.is_nan() {
        return Ok(us_aqi_scale(averaged));
    }
    if averaged.is_nan() {
        return Ok(us_aqi_scale(hourly));
    }
    Ok(us_aqi_scale(hourly.max(averaged)))
}

fn us_aqi_so2(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let so2 = read_direct(
        snapshot,
        decoder,
        "sulphur_dioxide",
        time,
        latitude,
        longitude,
    )? / 2.62;
    let so2_24h = rolling_mean_before(
        snapshot,
        decoder,
        "sulphur_dioxide",
        time,
        latitude,
        longitude,
        24,
    )? / 2.62;
    let hourly = position_extrapolated(
        &[0.0, 35.0, 75.0, 185.0, 305.0, f32::NAN, f32::NAN, f32::NAN],
        so2,
    );
    let averaged = position_extrapolated(
        &[
            f32::NAN,
            f32::NAN,
            f32::NAN,
            f32::NAN,
            305.0,
            605.0,
            805.0,
            1005.0,
        ],
        so2_24h,
    );
    Ok(if hourly.is_nan() {
        us_aqi_scale(averaged)
    } else {
        us_aqi_scale(hourly)
    })
}

fn us_aqi_co(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let mean = rolling_mean_before(
        snapshot,
        decoder,
        "carbon_monoxide",
        time,
        latitude,
        longitude,
        8,
    )?;
    Ok(us_aqi_scale(position_extrapolated(
        &[0.0, 4.5, 9.5, 12.5, 15.5, 30.5, 40.5, 50.5],
        mean / 1.15 / 1000.0,
    )))
}

fn chinese_aqi_pm2_5(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    chinese_hourly_iaqi(
        read_direct_unrounded(snapshot, decoder, "pm2_5", time, latitude, longitude)?,
        &HJ633_PM2_5_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    )
}

fn chinese_aqi_pm10(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    chinese_hourly_iaqi(
        read_direct_unrounded(snapshot, decoder, "pm10", time, latitude, longitude)?,
        &HJ633_PM10_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    )
}

fn chinese_aqi_no2(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    chinese_hourly_iaqi(
        read_direct_unrounded(
            snapshot,
            decoder,
            "nitrogen_dioxide",
            time,
            latitude,
            longitude,
        )?,
        &HJ633_NO2_HOURLY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    )
}

fn chinese_aqi_o3(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    chinese_hourly_iaqi(
        read_direct_unrounded(snapshot, decoder, "ozone", time, latitude, longitude)?,
        &HJ633_O3_HOURLY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    )
}

fn chinese_aqi_so2(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    chinese_hourly_iaqi(
        read_direct_unrounded(
            snapshot,
            decoder,
            "sulphur_dioxide",
            time,
            latitude,
            longitude,
        )?,
        &HJ633_SO2_HOURLY,
        &HJ633_AQI_BREAKPOINTS[..5],
        200.0,
        0,
    )
}

fn chinese_aqi_co(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    chinese_hourly_iaqi(
        read_direct_unrounded(
            snapshot,
            decoder,
            "carbon_monoxide",
            time,
            latitude,
            longitude,
        )? / 1000.0,
        &HJ633_CO_HOURLY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        1,
    )
}

const HJ633_AQI_BREAKPOINTS: [f32; 8] = [0.0, 50.0, 100.0, 150.0, 200.0, 300.0, 400.0, 500.0];
const HJ633_SO2_DAILY: [f32; 8] = [0.0, 50.0, 150.0, 475.0, 800.0, 1600.0, 2100.0, 2620.0];
const HJ633_SO2_HOURLY: [f32; 5] = [0.0, 150.0, 500.0, 650.0, 800.0];
const HJ633_NO2_DAILY: [f32; 8] = [0.0, 40.0, 80.0, 180.0, 280.0, 565.0, 750.0, 940.0];
const HJ633_NO2_HOURLY: [f32; 8] = [0.0, 100.0, 200.0, 700.0, 1200.0, 2340.0, 3090.0, 3840.0];
const HJ633_CO_DAILY: [f32; 8] = [0.0, 2.0, 4.0, 14.0, 24.0, 36.0, 48.0, 60.0];
const HJ633_CO_HOURLY: [f32; 8] = [0.0, 5.0, 10.0, 35.0, 60.0, 90.0, 120.0, 150.0];
const HJ633_O3_8H: [f32; 6] = [0.0, 100.0, 160.0, 215.0, 265.0, 800.0];
const HJ633_O3_HOURLY: [f32; 8] = [0.0, 160.0, 200.0, 300.0, 400.0, 800.0, 1000.0, 1200.0];
const HJ633_PM10_DAILY: [f32; 8] = [0.0, 50.0, 120.0, 250.0, 350.0, 420.0, 500.0, 600.0];
const HJ633_PM2_5_DAILY: [f32; 8] = [0.0, 30.0, 60.0, 115.0, 150.0, 250.0, 350.0, 500.0];

#[derive(Debug, Clone, Copy)]
struct ChineseDailyAirQualityStats {
    pm2_5_mean: f32,
    pm10_mean: f32,
    nitrogen_dioxide_mean: f32,
    ozone_maximum_8h_mean: f32,
    sulphur_dioxide_mean: f32,
    carbon_monoxide_mean: f32,
}

fn daily_chinese_air_quality_stats(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    date: NaiveDate,
    latitude: f64,
    longitude: f64,
) -> Result<ChineseDailyAirQualityStats> {
    let day_start = china_day_start_utc(date)?;
    Ok(ChineseDailyAirQualityStats {
        pm2_5_mean: daily_mean(snapshot, decoder, "pm2_5", day_start, latitude, longitude)?,
        pm10_mean: daily_mean(snapshot, decoder, "pm10", day_start, latitude, longitude)?,
        nitrogen_dioxide_mean: daily_mean(
            snapshot,
            decoder,
            "nitrogen_dioxide",
            day_start,
            latitude,
            longitude,
        )?,
        ozone_maximum_8h_mean: daily_maximum_8h_mean(
            snapshot, decoder, day_start, latitude, longitude,
        )?,
        sulphur_dioxide_mean: daily_mean(
            snapshot,
            decoder,
            "sulphur_dioxide",
            day_start,
            latitude,
            longitude,
        )?,
        carbon_monoxide_mean: daily_mean(
            snapshot,
            decoder,
            "carbon_monoxide",
            day_start,
            latitude,
            longitude,
        )?,
    })
}

fn daily_chinese_air_quality_value(
    stats: &ChineseDailyAirQualityStats,
    variable: &str,
) -> Result<f32> {
    let pm2_5 = chinese_daily_iaqi(
        stats.pm2_5_mean,
        &HJ633_PM2_5_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    );
    let pm10 = chinese_daily_iaqi(
        stats.pm10_mean,
        &HJ633_PM10_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    );
    let no2 = chinese_daily_iaqi(
        stats.nitrogen_dioxide_mean,
        &HJ633_NO2_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    );
    let o3 = chinese_daily_iaqi(
        stats.ozone_maximum_8h_mean,
        &HJ633_O3_8H,
        &HJ633_AQI_BREAKPOINTS[..6],
        300.0,
        0,
    );
    let so2 = chinese_daily_iaqi(
        stats.sulphur_dioxide_mean,
        &HJ633_SO2_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    );
    let co = chinese_daily_iaqi(
        stats.carbon_monoxide_mean / 1000.0,
        &HJ633_CO_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        1,
    );
    let all = [pm2_5, pm10, no2, o3, so2, co];
    let value = match variable {
        "chinese_aqi" => all_finite_max(&all),
        "chinese_aqi_pm2_5" => pm2_5,
        "chinese_aqi_pm10" => pm10,
        "chinese_aqi_no2" | "chinese_aqi_nitrogen_dioxide" => no2,
        "chinese_aqi_o3" | "chinese_aqi_ozone" => o3,
        "chinese_aqi_so2" | "chinese_aqi_sulphur_dioxide" => so2,
        "chinese_aqi_co" | "chinese_aqi_carbon_monoxide" => co,
        "pm2_5_mean" => round_ties_to_even(stats.pm2_5_mean, 0),
        "pm10_mean" => round_ties_to_even(stats.pm10_mean, 0),
        "nitrogen_dioxide_mean" => round_ties_to_even(stats.nitrogen_dioxide_mean, 0),
        "ozone_maximum_8h_mean" => round_ties_to_even(stats.ozone_maximum_8h_mean, 0),
        "sulphur_dioxide_mean" => round_ties_to_even(stats.sulphur_dioxide_mean, 0),
        "carbon_monoxide_mean" => round_ties_to_even(stats.carbon_monoxide_mean / 1000.0, 1),
        _ => bail!("unsupported daily Chinese AQI variable: {variable}"),
    };
    Ok(value)
}

fn china_day_start_utc(date: NaiveDate) -> Result<DateTime<Utc>> {
    let local_midnight = date
        .and_hms_opt(0, 0, 0)
        .context("invalid Chinese AQI day")?;
    Ok(DateTime::from_naive_utc_and_offset(
        local_midnight - Duration::hours(8),
        Utc,
    ))
}

fn daily_mean(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    day_start: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    mean_including(
        snapshot, decoder, variable, day_start, latitude, longitude, 24,
    )
}

fn daily_maximum_8h_mean(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    day_start: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let mut maximum = f32::NEG_INFINITY;
    for hour in 8..=24 {
        let value = trailing_mean_including(
            snapshot,
            decoder,
            "ozone",
            day_start + Duration::hours(hour),
            latitude,
            longitude,
            8,
        )?;
        if !value.is_finite() {
            return Ok(f32::NAN);
        }
        maximum = maximum.max(value);
    }
    Ok(maximum)
}

fn mean_including(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    start: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    hours: i64,
) -> Result<f32> {
    let mut sum = 0.0;
    for hour in 0..hours {
        let value = read_direct_unrounded(
            snapshot,
            decoder,
            variable,
            start + Duration::hours(hour),
            latitude,
            longitude,
        )?;
        if !value.is_finite() {
            return Ok(f32::NAN);
        }
        sum += value;
    }
    Ok(sum / hours as f32)
}

fn trailing_mean_including(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    end: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    hours: i64,
) -> Result<f32> {
    mean_including(
        snapshot,
        decoder,
        variable,
        end - Duration::hours(hours - 1),
        latitude,
        longitude,
        hours,
    )
}

fn chinese_hourly_iaqi(
    concentration: f32,
    concentration_breakpoints: &[f32],
    aqi_breakpoints: &[f32],
    upper_limit: f32,
    decimals: u32,
) -> Result<f32> {
    Ok(hj633_2026_iaqi(
        concentration,
        concentration_breakpoints,
        aqi_breakpoints,
        upper_limit,
        decimals,
    ))
}

fn chinese_daily_iaqi(
    concentration: f32,
    concentration_breakpoints: &[f32],
    aqi_breakpoints: &[f32],
    upper_limit: f32,
    decimals: u32,
) -> f32 {
    hj633_2026_iaqi(
        concentration,
        concentration_breakpoints,
        aqi_breakpoints,
        upper_limit,
        decimals,
    )
}

fn hj633_2026_iaqi(
    concentration: f32,
    concentration_breakpoints: &[f32],
    aqi_breakpoints: &[f32],
    upper_limit: f32,
    decimals: u32,
) -> f32 {
    if !concentration.is_finite() {
        return f32::NAN;
    }
    let scale = 10_i64.pow(decimals);
    let concentration = round_ties_to_even(concentration.max(0.0), decimals);
    let concentration_scaled = ((concentration as f64) * scale as f64).round() as i64;
    let Some((&last_concentration, &last_aqi)) =
        concentration_breakpoints.last().zip(aqi_breakpoints.last())
    else {
        return f32::NAN;
    };
    let last_concentration_scaled = ((last_concentration as f64) * scale as f64).round() as i64;
    if concentration_scaled > last_concentration_scaled {
        return upper_limit.min(last_aqi);
    }
    for index in 1..concentration_breakpoints.len() {
        let low_scaled =
            ((concentration_breakpoints[index - 1] as f64) * scale as f64).round() as i64;
        let high_scaled = ((concentration_breakpoints[index] as f64) * scale as f64).round() as i64;
        if concentration_scaled <= high_scaled {
            let aqi_low = aqi_breakpoints[index - 1] as f64;
            let aqi_high = aqi_breakpoints[index] as f64;
            let iaqi = aqi_low
                + (aqi_high - aqi_low) * (concentration_scaled - low_scaled) as f64
                    / (high_scaled - low_scaled) as f64;
            return (iaqi.ceil() as f32).min(upper_limit);
        }
    }
    upper_limit.min(last_aqi)
}

fn round_ties_to_even(value: f32, decimals: u32) -> f32 {
    let factor = 10_f64.powi(decimals as i32);
    ((value as f64 * factor).round_ties_even() / factor) as f32
}

fn all_finite_max(values: &[f32]) -> f32 {
    if values.iter().any(|value| !value.is_finite()) {
        f32::NAN
    } else {
        values.iter().copied().reduce(f32::max).unwrap_or(f32::NAN)
    }
}

fn is_chinese_aqi_variable(variable: &str) -> bool {
    matches!(
        variable,
        "chinese_aqi"
            | "chinese_aqi_pm2_5"
            | "chinese_aqi_pm10"
            | "chinese_aqi_no2"
            | "chinese_aqi_nitrogen_dioxide"
            | "chinese_aqi_o3"
            | "chinese_aqi_ozone"
            | "chinese_aqi_so2"
            | "chinese_aqi_sulphur_dioxide"
            | "chinese_aqi_co"
            | "chinese_aqi_carbon_monoxide"
    )
}

fn is_chinese_air_quality_daily_variable(variable: &str) -> bool {
    is_chinese_aqi_variable(variable)
        || matches!(
            variable,
            "pm2_5_mean"
                | "pm10_mean"
                | "nitrogen_dioxide_mean"
                | "ozone_maximum_8h_mean"
                | "sulphur_dioxide_mean"
                | "carbon_monoxide_mean"
        )
}

fn rolling_mean_before(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    hours: i64,
) -> Result<f32> {
    let mut sum = 0.0;
    // Swift's slidingAverageDroppingFirstDt reduces the window from the
    // oldest sample to the newest; preserve that order for identical f32 ties.
    for hour in (1..=hours).rev() {
        let sample_time = time - Duration::hours(hour);
        let Some(value) = read_optional_direct(
            snapshot,
            decoder,
            variable,
            sample_time,
            latitude,
            longitude,
        )?
        else {
            return Ok(f32::NAN);
        };
        sum += value;
    }
    Ok(sum / hours as f32)
}

fn position_extrapolated(thresholds: &[f32], search: f32) -> f32 {
    let mut previous = f32::NAN;
    let mut slope = f32::NAN;
    for (index, value) in thresholds.iter().enumerate() {
        slope = *value - previous;
        if search < *value {
            return index as f32 - 1.0 + (search - previous) / slope;
        }
        previous = *value;
    }
    thresholds.len() as f32 - 1.0 + (search - previous) / slope
}

fn us_aqi_scale(value: f32) -> f32 {
    if value <= 4.0 {
        value * 50.0
    } else {
        value * 100.0 - 200.0
    }
}

fn finite_max(values: &[f32]) -> f32 {
    values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .reduce(f32::max)
        .unwrap_or(f32::NAN)
}

fn surface_pressure(temperature: f32, pressure_msl: f32, elevation: f32) -> f32 {
    let elevation = if elevation.is_nan() { 0.0 } else { elevation };
    let t0 = temperature + 273.15 + 0.0065 * elevation;
    let factor = (1.0 - (0.0065 * elevation) / t0).powf(-5.255_781);
    pressure_msl / factor
}

fn surface_pressure_elevation(sampling: ModelSampling) -> f32 {
    if sampling.target_elevation.is_finite() {
        sampling.target_elevation
    } else {
        sampling.model_elevation
    }
}

fn read_weather_surface_elevation_grid(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<f32>> {
    let model = current_weather_model();
    let product = snapshot.require_product(model.primary_product())?;
    let mut values = if let Some(entry) = product.static_entries.get("surface_elevation") {
        read_entry_grid(&product, entry, decoder, latitudes, longitudes)?
    } else {
        read_static_elevation_grid_for_coordinates(
            snapshot,
            decoder,
            model.static_elevation_spec(),
            latitudes,
            longitudes,
        )?
    };
    values.iter_mut().for_each(|value| {
        if !value.is_finite() || *value <= -900.0 {
            *value = 0.0;
        }
    });
    Ok(values)
}

fn read_direct(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    read_direct_with_rounding(snapshot, decoder, variable, time, latitude, longitude, true)
}

fn read_direct_unrounded(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    read_direct_with_rounding(
        snapshot, decoder, variable, time, latitude, longitude, false,
    )
}

fn read_direct_with_rounding(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    round_values: bool,
) -> Result<f32> {
    if variable == "carbon_monoxide"
        && snapshot.product("cams_global_greenhouse_gases").is_some()
        && snapshot.product("cams_global").is_some()
    {
        return read_cams_mixed_carbon_monoxide(
            snapshot,
            decoder,
            time,
            latitude,
            longitude,
            round_values,
        );
    }
    if variable == "precipitation_probability" && current_weather_model() == WeatherModel::Gfs {
        return read_gfs_precipitation_probability_with_rounding(
            snapshot,
            decoder,
            time,
            latitude,
            longitude,
            round_values,
        );
    }
    let (product_name, raw_variable) = product_for_variable(snapshot, variable)?;
    read_product_history_value_with_rounding(
        snapshot,
        decoder,
        product_name,
        variable,
        &raw_variable,
        time,
        latitude,
        longitude,
        round_values,
    )
}

fn read_gfs_precipitation_probability_with_rounding(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    round_values: bool,
) -> Result<f32> {
    let mut found_product = false;
    let mut fallback = f32::NAN;
    for product_name in ["ncep_gefs025", "ncep_gefs05"] {
        if snapshot.product(product_name).is_none() {
            continue;
        }
        found_product = true;
        let value = read_product_history_value_with_rounding(
            snapshot,
            decoder,
            product_name,
            "precipitation_probability",
            "precipitation_probability",
            time,
            latitude,
            longitude,
            round_values,
        )?;
        if value.is_finite() {
            return Ok(value);
        }
        fallback = value;
    }
    if found_product {
        Ok(fallback)
    } else {
        bail!("GFS precipitation probability products are not available")
    }
}

fn read_direct_grid(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Vec<f32>> {
    if variable == "carbon_monoxide" {
        bail!("regional carbon_monoxide mixing is not implemented");
    }
    if current_weather_model() == WeatherModel::EcmwfIfs025 {
        let mut frames = read_direct_grid_series(
            snapshot,
            decoder,
            variable,
            &[time],
            latitudes,
            longitudes,
            round_values,
        )?;
        return Ok(frames.remove(0));
    }
    if variable == "precipitation_probability" {
        let mut found_product = false;
        let mut output = vec![f32::NAN; latitudes.len() * longitudes.len()];
        for product_name in ["ncep_gefs025", "ncep_gefs05"] {
            let Some(values) = read_gfs_product_history_grid_with_rounding(
                snapshot,
                decoder,
                product_name,
                variable,
                variable,
                time,
                latitudes,
                longitudes,
                round_values,
            )?
            else {
                continue;
            };
            found_product = true;
            fill_nan_from_fallback(&mut output, &values);
            if output.iter().all(|value| value.is_finite()) {
                break;
            }
        }
        if found_product {
            return Ok(output);
        }
        bail!("GFS precipitation probability products are not available");
    }
    let (product_name, raw_variable) = product_for_variable(snapshot, variable)?;
    if is_gfs_product(product_name) {
        if let Some(values) = read_gfs_product_history_grid_with_rounding(
            snapshot,
            decoder,
            product_name,
            variable,
            &raw_variable,
            time,
            latitudes,
            longitudes,
            round_values,
        )? {
            return Ok(values);
        }
    }
    let products = snapshot.product_snapshots(product_name);
    if is_cams_product(product_name) {
        let covering_products = newest_and_previous_products(&products)
            // CAMS fallback is deliberately limited to the newest and the
            // immediately previous retained run. Filter for time coverage
            // only after applying that limit, otherwise a historical query
            // could silently promote the third retained run to a fallback.
            .filter(|product| product_covers_time(product, &raw_variable, time))
            .collect::<Vec<_>>();
        if let Some(first) = covering_products.first() {
            let mut values = read_product_grid_with_rounding(
                first,
                decoder,
                variable,
                &raw_variable,
                time,
                latitudes,
                longitudes,
                round_values,
            )?;
            for product in covering_products.iter().skip(1) {
                if !values.iter().any(|value| value.is_nan()) {
                    break;
                }
                let fallback = read_product_grid_with_rounding(
                    product,
                    decoder,
                    variable,
                    &raw_variable,
                    time,
                    latitudes,
                    longitudes,
                    round_values,
                )?;
                for (value, fallback_value) in values.iter_mut().zip(fallback) {
                    if value.is_nan() && !fallback_value.is_nan() {
                        *value = fallback_value;
                    }
                }
            }
            return Ok(values);
        }
    }
    for product in &products {
        if !product_covers_time(product, &raw_variable, time) {
            continue;
        }
        return read_product_grid_with_rounding(
            product,
            decoder,
            variable,
            &raw_variable,
            time,
            latitudes,
            longitudes,
            round_values,
        );
    }
    if products.iter().any(|product| {
        product
            .entries
            .keys()
            .any(|entry_key| entry_key.variable == raw_variable)
    }) {
        return Ok(vec![f32::NAN; latitudes.len() * longitudes.len()]);
    }
    bail!("variable/time is not available: {} {}", raw_variable, time)
}

#[allow(clippy::too_many_arguments)]
fn read_gfs_product_history_grid_with_rounding(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    product_name: &str,
    variable: &str,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Option<Vec<f32>>> {
    let products = snapshot.product_snapshots(product_name);
    if products.is_empty() {
        return Ok(None);
    }
    let sampled_latitudes;
    let sampled_longitudes;
    let (latitudes, longitudes) = if latitudes.len() == 1 && longitudes.len() == 1 {
        if let Some(sampling) = current_product_sampling(product_name) {
            sampled_latitudes = [sampling.latitude];
            sampled_longitudes = [sampling.longitude];
            (&sampled_latitudes[..], &sampled_longitudes[..])
        } else {
            (latitudes, longitudes)
        }
    } else {
        (latitudes, longitudes)
    };
    if is_gfs_probability_product(product_name) && raw_variable == "precipitation_probability" {
        return read_gfs_probability_history_grid_with_rounding(
            &products,
            decoder,
            raw_variable,
            time,
            latitudes,
            longitudes,
            round_values,
        )
        .map(Some);
    }
    if let Some(primary) = products
        .iter()
        .find(|product| product_covers_time(product, raw_variable, time))
    {
        let mut values = read_product_grid_with_rounding(
            primary,
            decoder,
            variable,
            raw_variable,
            time,
            latitudes,
            longitudes,
            round_values,
        )?;
        if values.iter().any(|value| value.is_nan()) {
            let fallback = products.iter().find(|product| {
                !Arc::ptr_eq(primary, product) && product_covers_time(product, raw_variable, time)
            });
            if let Some(product) = fallback {
                let fallback = read_product_grid_with_rounding(
                    product,
                    decoder,
                    variable,
                    raw_variable,
                    time,
                    latitudes,
                    longitudes,
                    round_values,
                )?;
                fill_nan_from_fallback(&mut values, &fallback);
            }
        }
        return Ok(Some(values));
    }
    if products.iter().any(|product| {
        product
            .entries
            .keys()
            .any(|entry_key| entry_key.variable == raw_variable)
    }) {
        return Ok(Some(vec![f32::NAN; latitudes.len() * longitudes.len()]));
    }
    Ok(None)
}

fn read_direct_grid_series(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Vec<Vec<f32>>> {
    if times.is_empty() || latitudes.len() != 1 || longitudes.len() != 1 {
        return read_direct_grid_series_uncached(
            snapshot,
            decoder,
            variable,
            times,
            latitudes,
            longitudes,
            round_values,
        );
    }
    let Some(cache) = current_direct_grid_series_request_cache() else {
        return read_direct_grid_series_uncached(
            snapshot,
            decoder,
            variable,
            times,
            latitudes,
            longitudes,
            round_values,
        );
    };
    let (product_name, _) = product_for_variable(snapshot, variable)?;
    let sampling = current_product_sampling(product_name).map(|sampling| {
        (
            sampling.latitude.to_bits(),
            sampling.longitude.to_bits(),
            sampling.model_elevation.to_bits(),
            sampling.target_elevation.to_bits(),
        )
    });
    let key = DirectGridSeriesCacheKey {
        model: current_weather_model(),
        variable: variable.to_string(),
        start_unix: times[0].timestamp(),
        end_unix: times[times.len() - 1].timestamp(),
        count: times.len(),
        latitude_bits: latitudes[0].to_bits(),
        longitude_bits: longitudes[0].to_bits(),
        sampling,
        round_values,
    };
    let slot = {
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow!("direct grid-series request cache lock is poisoned"))?;
        cache
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    };
    let mut cached = slot
        .lock()
        .map_err(|_| anyhow!("direct grid-series request cache slot lock is poisoned"))?;
    if let Some(values) = cached.as_ref() {
        return Ok((**values).clone());
    }
    let values = read_direct_grid_series_uncached(
        snapshot,
        decoder,
        variable,
        times,
        latitudes,
        longitudes,
        round_values,
    )?;
    *cached = Some(Arc::new(values.clone()));
    Ok(values)
}

fn read_direct_grid_series_uncached(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Vec<Vec<f32>>> {
    let (product_name, raw_variable) = product_for_variable(snapshot, variable)?;
    // Point requests resolve a potentially different nearest land/model cell
    // for every product (for example GFS 0.13° surface versus 0.25° wind).
    // The time-slab path must use that product-specific cell just like the
    // single-value path in read_entry_value(). Regional/WebP requests have no
    // thread-local sampling and continue to use their supplied output grid.
    let sampled_latitudes;
    let sampled_longitudes;
    let (latitudes, longitudes) = if latitudes.len() == 1 && longitudes.len() == 1 {
        if let Some(sampling) = current_product_sampling(product_name) {
            sampled_latitudes = [sampling.latitude];
            sampled_longitudes = [sampling.longitude];
            (&sampled_latitudes[..], &sampled_longitudes[..])
        } else {
            (latitudes, longitudes)
        }
    } else {
        (latitudes, longitudes)
    };
    let products = snapshot.product_snapshots(product_name);
    if is_ecmwf_product(product_name) {
        let mut values = read_ecmwf_window_grid_series(
            &products,
            decoder,
            variable,
            &raw_variable,
            times,
            latitudes,
            longitudes,
        )?;
        for frame in &mut values {
            for value in frame {
                *value = apply_elevation_correction(product_name, variable, *value);
            }
        }
        return Ok(values);
    }
    if is_gfs_product(product_name) {
        let full_products = products
            .iter()
            .filter(|product| {
                gfs_snapshot_is_full(product)
                    && times
                        .iter()
                        .any(|time| product_covers_time(product, &raw_variable, *time))
            })
            .take(2)
            .collect::<Vec<_>>();
        if let Some(first) = full_products.first() {
            if let Some(mut values) = read_gfs_product_grid_series(
                first,
                decoder,
                variable,
                &raw_variable,
                times,
                latitudes,
                longitudes,
                round_values,
            )? {
                if values
                    .iter()
                    .any(|frame| frame.iter().any(|value| value.is_nan()))
                {
                    for fallback_product in products.iter().filter(|product| {
                        !Arc::ptr_eq(first, product)
                            && times
                                .iter()
                                .any(|time| product_covers_time(product, &raw_variable, *time))
                    }) {
                        if let Some(fallback_frames) = read_gfs_product_grid_series(
                            fallback_product,
                            decoder,
                            variable,
                            &raw_variable,
                            times,
                            latitudes,
                            longitudes,
                            round_values,
                        )? {
                            for (frame, fallback_frame) in values.iter_mut().zip(fallback_frames) {
                                for (value, fallback_value) in frame.iter_mut().zip(fallback_frame)
                                {
                                    if value.is_nan() && !fallback_value.is_nan() {
                                        *value = fallback_value;
                                    }
                                }
                            }
                        } else {
                            // A fallback run can cross an official output-step
                            // boundary at a different index, so it may not be
                            // decodable as one contiguous slab. Only anomaly
                            // frames take this slower path; healthy WebP runs
                            // remain one-slab decodes.
                            for (index, frame) in values.iter_mut().enumerate() {
                                if !frame.iter().any(|value| value.is_nan())
                                    || !product_covers_time(
                                        fallback_product,
                                        &raw_variable,
                                        times[index],
                                    )
                                {
                                    continue;
                                }
                                let fallback_frame = read_product_grid_with_rounding(
                                    fallback_product,
                                    decoder,
                                    variable,
                                    &raw_variable,
                                    times[index],
                                    latitudes,
                                    longitudes,
                                    round_values,
                                )?;
                                for (value, fallback_value) in frame.iter_mut().zip(fallback_frame)
                                {
                                    if value.is_nan() && !fallback_value.is_nan() {
                                        *value = fallback_value;
                                    }
                                }
                            }
                        }
                        if !values
                            .iter()
                            .any(|frame| frame.iter().any(|value| value.is_nan()))
                        {
                            break;
                        }
                    }
                }
                // Native time-slab decoding bypasses read_entry_value(), so
                // apply the same point-request elevation correction here.
                // Regional/WebP calls have no request sampling and therefore
                // remain byte-for-byte unchanged.
                for frame in &mut values {
                    for value in frame {
                        *value = apply_elevation_correction(product_name, variable, *value);
                    }
                }
                return Ok(values);
            }
        }
    }
    times
        .iter()
        .map(|time| {
            read_direct_grid(
                snapshot,
                decoder,
                variable,
                *time,
                latitudes,
                longitudes,
                round_values,
            )
        })
        .collect()
}

fn is_gfs_product(product_name: &str) -> bool {
    matches!(
        product_name,
        "gfs013_surface" | "gfs025" | "gfs_pressure_profile" | "ncep_gefs025" | "ncep_gefs05"
    )
}

fn is_gfs_probability_product(product_name: &str) -> bool {
    matches!(product_name, "ncep_gefs025" | "ncep_gefs05")
}

fn is_ecmwf_product(product_name: &str) -> bool {
    matches!(product_name, "ecmwf_ifs025" | "ecmwf_ifs025_ensemble")
}

fn is_cams_product(product_name: &str) -> bool {
    matches!(product_name, "cams_global" | "cams_global_greenhouse_gases")
}

fn newest_and_previous_products(
    products: &[Arc<ProductSnapshot>],
) -> impl Iterator<Item = &Arc<ProductSnapshot>> {
    products.iter().take(2)
}

fn gfs_snapshot_is_full(product: &ProductSnapshot) -> bool {
    product
        .entries
        .values()
        .map(|entry| entry.forecast_hour)
        .max()
        .is_some_and(|forecast_hour| forecast_hour > 6)
}

#[allow(clippy::too_many_arguments)]
fn read_product_grid_with_rounding(
    product: &ProductSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Vec<f32>> {
    let native_times = native_times_for_variable(product, raw_variable);
    match interpolation_kind_for_variable(variable) {
        InterpolationKind::Direct => {
            read_native_grid(product, decoder, raw_variable, time, latitudes, longitudes)
        }
        InterpolationKind::SolarBackwardsAveraged { scalefactor } => read_solar_backwards_grid(
            product,
            decoder,
            raw_variable,
            time,
            latitudes,
            longitudes,
            scalefactor,
            round_values,
        ),
        InterpolationKind::BackwardsSum { scalefactor }
        | InterpolationKind::Backwards { scalefactor } => {
            if native_times.is_empty()
                || time < native_times[0]
                || time > *native_times.last().expect("checked not empty")
            {
                return Ok(vec![f32::NAN; latitudes.len() * longitudes.len()]);
            }
            let Some(index) = native_times
                .iter()
                .position(|native_time| *native_time >= time)
            else {
                return Ok(vec![f32::NAN; latitudes.len() * longitudes.len()]);
            };
            let mut values = read_native_grid(
                product,
                decoder,
                raw_variable,
                native_times[index],
                latitudes,
                longitudes,
            )?;
            if matches!(
                interpolation_kind_for_variable(variable),
                InterpolationKind::BackwardsSum { .. }
            ) {
                let native_dt_seconds = native_dt_seconds_at(&native_times, index);
                if native_dt_seconds > 0 {
                    let factor = 3600.0 / native_dt_seconds as f32;
                    values.iter_mut().for_each(|value| *value *= factor);
                }
            }
            if round_values {
                values
                    .iter_mut()
                    .for_each(|value| *value = round_to_scalefactor(*value, scalefactor));
            }
            Ok(values)
        }
        InterpolationKind::Linear { scalefactor } => {
            let Some((index, fraction)) = interpolation_index(&native_times, time) else {
                return Ok(vec![f32::NAN; latitudes.len() * longitudes.len()]);
            };
            let a = read_native_grid(
                product,
                decoder,
                raw_variable,
                native_times[index],
                latitudes,
                longitudes,
            )?;
            let b = if index + 1 < native_times.len() {
                read_native_grid(
                    product,
                    decoder,
                    raw_variable,
                    native_times[index + 1],
                    latitudes,
                    longitudes,
                )?
            } else {
                a.clone()
            };
            Ok(a.into_iter()
                .zip(b)
                .map(|(a, b)| {
                    maybe_round_to_scalefactor(
                        a * (1.0 - fraction) + b * fraction,
                        scalefactor,
                        round_values,
                    )
                })
                .collect())
        }
        InterpolationKind::Hermite {
            scalefactor,
            bounds,
        } => read_hermite_grid(
            product,
            decoder,
            raw_variable,
            time,
            latitudes,
            longitudes,
            scalefactor,
            bounds,
            round_values,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn read_hermite_grid(
    product: &ProductSnapshot,
    decoder: &OfficialDecoder,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
    scalefactor: f32,
    bounds: Option<(f32, f32)>,
    round_values: bool,
) -> Result<Vec<f32>> {
    let native_times = native_times_for_variable(product, raw_variable);
    let Some((index, fraction)) = interpolation_index(&native_times, time) else {
        return Ok(vec![f32::NAN; latitudes.len() * longitudes.len()]);
    };
    let b = read_native_grid(
        product,
        decoder,
        raw_variable,
        native_times[index],
        latitudes,
        longitudes,
    )?;
    if index + 1 >= native_times.len() {
        return Ok(b
            .into_iter()
            .map(|value| maybe_round_to_scalefactor(value, scalefactor, round_values))
            .collect());
    }
    let stride_seconds = interpolation_stride_seconds(&native_times, index);
    let a_time = native_times[index] - Duration::seconds(stride_seconds);
    let a = if native_times.binary_search(&a_time).is_ok() {
        read_native_grid(
            product,
            decoder,
            raw_variable,
            a_time,
            latitudes,
            longitudes,
        )?
    } else {
        b.clone()
    };
    let c = read_native_grid(
        product,
        decoder,
        raw_variable,
        native_times[index + 1],
        latitudes,
        longitudes,
    )?;
    let d_time = native_times[index + 1] + Duration::seconds(stride_seconds);
    let d = if native_times.binary_search(&d_time).is_ok() {
        read_native_grid(
            product,
            decoder,
            raw_variable,
            d_time,
            latitudes,
            longitudes,
        )?
    } else {
        c.clone()
    };
    Ok(a.into_iter()
        .zip(b)
        .zip(c)
        .zip(d)
        .map(|(((a, b), c), d)| {
            let a = if a.is_nan() { b } else { a };
            let c = if c.is_nan() { b } else { c };
            let d = if d.is_nan() {
                missing_second_lookahead_value(product, b, c)
            } else {
                d
            };
            let coeff_a = -a / 2.0 + (3.0 * b) / 2.0 - (3.0 * c) / 2.0 + d / 2.0;
            let coeff_b = a - (5.0 * b) / 2.0 + 2.0 * c - d / 2.0;
            let coeff_c = -a / 2.0 + c / 2.0;
            let h = coeff_a * fraction * fraction * fraction
                + coeff_b * fraction * fraction
                + coeff_c * fraction
                + b;
            let mut value = maybe_round_to_scalefactor(h, scalefactor, round_values);
            if let Some((lower, upper)) = bounds {
                value = value.clamp(lower, upper);
            }
            value
        })
        .collect())
}

fn read_native_grid(
    product: &ProductSnapshot,
    decoder: &OfficialDecoder,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<f32>> {
    let key = EntryKey {
        variable: raw_variable.to_string(),
        valid_time_utc: time,
    };
    let entry = product
        .entries
        .get(&key)
        .with_context(|| format!("variable/time is not available: {} {}", raw_variable, time))?;
    read_entry_grid(product, entry, decoder, latitudes, longitudes)
}

#[allow(clippy::too_many_arguments)]
fn read_exact_native_grid_series(
    product: &ProductSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    raw_variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Option<Vec<Vec<f32>>>> {
    // GFS is hourly through forecast hour 120, exactly the WebP window. CAMS
    // requires interpolation and intentionally keeps the per-hour path.
    if !product.product.starts_with("gfs") {
        return Ok(None);
    }
    let entries = times
        .iter()
        .map(|time| {
            product.entries.get(&EntryKey {
                variable: raw_variable.to_string(),
                valid_time_utc: *time,
            })
        })
        .collect::<Vec<_>>();
    let available = entries.iter().filter(|entry| entry.is_some()).count();
    if available == 0
        || entries
            .iter()
            .enumerate()
            .any(|(index, entry)| entry.is_none() && index != 0)
        || entries.iter().flatten().any(|entry| {
            entry.native_file_path.is_none()
                || entry.native_time_index.is_none()
                || entry.array.dimensions.len() != 3
        })
    {
        return Ok(None);
    }

    let grid_len = latitudes.len() * longitudes.len();
    let mut output = vec![vec![f32::NAN; grid_len]; times.len()];
    let mut output_index = 0;
    while output_index < entries.len() {
        let Some(first) = entries[output_index] else {
            output_index += 1;
            continue;
        };
        let first_native_index = first.native_time_index.expect("checked native entry");
        let first_path = first
            .native_file_path
            .as_deref()
            .expect("checked native entry");
        let mut end = output_index + 1;
        while end < entries.len() {
            let Some(next) = entries[end] else {
                break;
            };
            if next.native_file_path.as_deref() != Some(first_path)
                || next.native_time_index != Some(first_native_index + (end - output_index) as u64)
            {
                break;
            }
            end += 1;
        }
        let decoded = read_native_entry_grid_time_range(
            product,
            first,
            decoder,
            latitudes,
            longitudes,
            first_native_index,
            end - output_index,
        )?;
        for (offset, values) in decoded.into_iter().enumerate() {
            output[output_index + offset] = values;
        }
        output_index = end;
    }

    let native_times = native_times_for_variable(product, raw_variable);
    let interpolation = interpolation_kind_for_variable(variable);
    for (time, values) in times.iter().zip(output.iter_mut()) {
        if values.iter().all(|value| value.is_nan()) {
            continue;
        }
        match interpolation {
            InterpolationKind::Direct => {}
            InterpolationKind::SolarBackwardsAveraged { scalefactor } => {
                if round_values {
                    values
                        .iter_mut()
                        .for_each(|value| *value = round_to_scalefactor(*value, scalefactor));
                }
            }
            InterpolationKind::Linear { scalefactor }
            | InterpolationKind::Backwards { scalefactor } => {
                if round_values {
                    values
                        .iter_mut()
                        .for_each(|value| *value = round_to_scalefactor(*value, scalefactor));
                }
            }
            InterpolationKind::Hermite {
                scalefactor,
                bounds,
            } => {
                values.iter_mut().for_each(|value| {
                    *value = maybe_round_to_scalefactor(*value, scalefactor, round_values);
                    if let Some((lower, upper)) = bounds {
                        *value = value.clamp(lower, upper);
                    }
                });
            }
            InterpolationKind::BackwardsSum { scalefactor } => {
                let index = native_times
                    .binary_search(time)
                    .map_err(|_| anyhow!("exact native GFS time disappeared from the index"))?;
                let native_dt_seconds = native_dt_seconds_at(&native_times, index);
                let factor = if native_dt_seconds > 0 {
                    3600.0 / native_dt_seconds as f32
                } else {
                    1.0
                };
                values.iter_mut().for_each(|value| {
                    *value *= factor;
                    if round_values {
                        *value = round_to_scalefactor(*value, scalefactor);
                    }
                });
            }
        }
    }
    for (index, entry) in entries.iter().enumerate() {
        if entry.is_some() {
            continue;
        }
        match read_product_grid_with_rounding(
            product,
            decoder,
            variable,
            raw_variable,
            times[index],
            latitudes,
            longitudes,
            round_values,
        ) {
            Ok(values) => output[index] = values,
            Err(error) if error.to_string().contains("variable/time is not available") => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Some(output))
}

#[derive(Debug)]
struct EcmwfRegularRun {
    source_run: String,
    start: DateTime<Utc>,
    frames: Vec<Vec<f32>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EcmwfRegularCacheKey {
    coverage_id: String,
    raw_variable: String,
    latitudes: Vec<u64>,
    longitudes: Vec<u64>,
}

#[derive(Debug)]
struct EcmwfRegularSeries {
    runs: Vec<EcmwfRegularRun>,
    public_start_utc: Option<DateTime<Utc>>,
    latest_source_run: Option<String>,
}

#[cfg(test)]
static ECMWF_REGULAR_SERIES_BUILD_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn interpolate_ecmwf_backwards_in_place(values: &mut [f32], is_summation: bool) {
    let mut first_valid = values.len();
    for (index, value) in values.iter().enumerate() {
        if value.is_nan() {
            continue;
        }
        if first_valid == values.len() {
            first_valid = index;
            continue;
        }
        first_valid = first_valid.saturating_sub(index - first_valid - 1);
        break;
    }
    if first_valid >= values.len() {
        return;
    }

    let mut previous_index = first_valid as isize - 1;
    for index in first_valid..values.len() {
        if !values[index].is_nan() {
            previous_index = index as isize;
            continue;
        }
        let Some(next) = (index..values.len()).find(|next| !values[*next].is_nan()) else {
            break;
        };
        let value = values[next];
        let divisor = (next as isize - previous_index) as f32;
        for target in values.iter_mut().take(next + 1).skip(index) {
            *target = if is_summation { value / divisor } else { value };
        }
    }
}

fn interpolate_ecmwf_linear_in_place(values: &mut [f32]) {
    let mut previous_value = f32::NAN;
    let mut previous_index = 0usize;
    for index in 0..values.len() {
        if !values[index].is_nan() {
            previous_value = values[index];
            previous_index = index;
            continue;
        }
        let Some(next) = (index..values.len()).find(|next| !values[*next].is_nan()) else {
            break;
        };
        let next_value = values[next];
        for (offset, target_value) in values[index..next].iter_mut().enumerate() {
            let target = index + offset;
            let fraction = (target - previous_index) as f32 / (next - previous_index) as f32;
            *target_value = next_value * fraction + previous_value * (1.0 - fraction);
        }
    }
}

fn interpolate_ecmwf_hermite_in_place(
    values: &mut [f32],
    bounds: Option<(f32, f32)>,
    require_full_support: bool,
    allow_missing_terminal_d: bool,
) {
    let mut width = 0usize;
    for index in 0..values.len() {
        if !values[index].is_nan() {
            continue;
        }
        let mut c = f32::NAN;
        let mut d = f32::NAN;
        let mut pos_c = 0usize;
        let mut pos_d = 0usize;
        for (next, &value) in values.iter().enumerate().skip(index) {
            if value.is_nan() {
                continue;
            }
            if c.is_nan() {
                c = value;
                pos_c = next;
            } else {
                d = value;
                pos_d = next;
                break;
            }
        }
        if c.is_nan() {
            break;
        }
        let has_distinct_d = !d.is_nan();
        if d.is_nan() {
            d = c;
        } else {
            width = pos_d - pos_c;
        }
        let pos_b = pos_c.saturating_sub(width);
        let pos_a = pos_b.checked_sub(width).unwrap_or(pos_b);
        if require_full_support
            && (pos_a == pos_b
                || index <= pos_b
                || !values[pos_a].is_finite()
                || !values[pos_b].is_finite()
                || (!has_distinct_d && !allow_missing_terminal_d))
        {
            continue;
        }
        let b = values[pos_b];
        let a = values[pos_a];
        let coefficient_a = -a / 2.0 + (3.0 * b) / 2.0 - (3.0 * c) / 2.0 + d / 2.0;
        let coefficient_b = a - (5.0 * b) / 2.0 + 2.0 * c - d / 2.0;
        let coefficient_c = -a / 2.0 + c / 2.0;
        for (offset, target_value) in values[index..pos_c].iter_mut().enumerate() {
            if pos_c == pos_b {
                break;
            }
            let target = index + offset;
            let fraction = (target - pos_b) as f32 / (pos_c - pos_b) as f32;
            let mut interpolated = coefficient_a * fraction * fraction * fraction
                + coefficient_b * fraction * fraction
                + coefficient_c * fraction
                + b;
            if let Some((lower, upper)) = bounds {
                interpolated = interpolated.clamp(lower, upper);
            }
            *target_value = interpolated;
        }
    }
}

fn ecmwf_first_stage_point(
    values: &mut [f32],
    kind: InterpolationKind,
    start: DateTime<Utc>,
    latitude: f32,
    longitude: f32,
    require_full_hermite_support: bool,
    allow_missing_terminal_hermite_d: bool,
) {
    match kind {
        InterpolationKind::Direct => {}
        InterpolationKind::BackwardsSum { .. } => {
            interpolate_ecmwf_backwards_in_place(values, true)
        }
        InterpolationKind::Backwards { .. } => interpolate_ecmwf_backwards_in_place(values, false),
        InterpolationKind::Linear { .. } => interpolate_ecmwf_linear_in_place(values),
        InterpolationKind::Hermite { bounds, .. } => interpolate_ecmwf_hermite_in_place(
            values,
            bounds,
            require_full_hermite_support,
            allow_missing_terminal_hermite_d,
        ),
        InterpolationKind::SolarBackwardsAveraged { .. } => {
            interpolate_solar_backwards_with_dt_in_place(
                values,
                start,
                3 * 3600,
                latitude as f64,
                longitude as f64,
            )
        }
    }
    let scalefactor = match kind {
        InterpolationKind::Direct => return,
        InterpolationKind::BackwardsSum { scalefactor }
        | InterpolationKind::Backwards { scalefactor }
        | InterpolationKind::Linear { scalefactor }
        | InterpolationKind::Hermite { scalefactor, .. }
        | InterpolationKind::SolarBackwardsAveraged { scalefactor } => scalefactor,
    };
    for value in values.iter_mut().filter(|value| value.is_finite()) {
        *value = round_to_scalefactor(*value, scalefactor);
    }
}

#[allow(clippy::too_many_arguments)]
fn ecmwf_regularize_source_run(
    product: &ProductSnapshot,
    decoder: &OfficialDecoder,
    source_run: &str,
    raw_variable: &str,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Option<EcmwfRegularRun>> {
    let mut entries = product
        .entries_by_source_run
        .get(source_run)
        .into_iter()
        .flatten()
        .filter(|entry| entry.variable == raw_variable)
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Ok(None);
    }
    entries.sort_by_key(|entry| entry.forecast_hour);
    let logical_hour = |entry: &BundleEntry| entry.forecast_hour;
    let min_hour = entries
        .iter()
        .map(|entry| logical_hour(entry))
        .min()
        .unwrap();
    let max_hour = entries
        .iter()
        .map(|entry| logical_hour(entry))
        .max()
        .unwrap();
    if min_hour < 0 || min_hour % 3 != 0 || max_hour % 3 != 0 {
        bail!("ECMWF native forecast hours are not aligned to the 3-hour conversion axis");
    }
    let base = entries[0].valid_time_utc - Duration::hours(logical_hour(entries[0]));
    if entries
        .iter()
        .any(|entry| entry.valid_time_utc - Duration::hours(logical_hour(entry)) != base)
    {
        bail!("ECMWF logical source run has inconsistent reference times: {source_run}");
    }
    let start = base + Duration::hours(min_hour);
    let count = usize::try_from((max_hour - min_hour) / 3 + 1)?;
    let grid_len = latitudes.len() * longitudes.len();
    let mut frames = vec![vec![f32::NAN; grid_len]; count];
    let mut entry_index = 0;
    while entry_index < entries.len() {
        let first = entries[entry_index];
        // Native ECMWF variables keep their time axis in one 3D OM file.
        // Decode each contiguous span in one codec call so a cold point request
        // does not repeat the LUT and file-range work for every forecast frame.
        let contiguous_end = match (first.native_file_path.as_deref(), first.native_time_index) {
            (Some(first_path), Some(first_native_index)) if first.array.dimensions.len() == 3 => {
                let mut end = entry_index + 1;
                while end < entries.len() {
                    let next = entries[end];
                    let offset = u64::try_from(end - entry_index)?;
                    if next.native_file_path.as_deref() != Some(first_path)
                        || next.native_time_index != Some(first_native_index + offset)
                        || next.array.dimensions.len() != 3
                    {
                        break;
                    }
                    end += 1;
                }
                end
            }
            _ => entry_index + 1,
        };

        if contiguous_end - entry_index > 1 {
            let decoded = read_native_entry_grid_time_range(
                product,
                first,
                decoder,
                latitudes,
                longitudes,
                first.native_time_index.expect("checked native time index"),
                contiguous_end - entry_index,
            )?;
            for (entry, values) in entries[entry_index..contiguous_end].iter().zip(decoded) {
                let index = usize::try_from((logical_hour(entry) - min_hour) / 3)?;
                frames[index] = values;
            }
        } else {
            let index = usize::try_from((logical_hour(first) - min_hour) / 3)?;
            frames[index] = read_entry_grid(product, first, decoder, latitudes, longitudes)?;
        }
        entry_index = contiguous_end;
    }

    let kind = ecmwf_interpolation_kind(raw_variable);
    let is_gust = raw_variable == "wind_gusts_10m";
    let allow_missing_terminal_hermite_d = is_gust
        && product
            .manifest
            .coverage_plan
            .iter()
            .filter(|entry| entry.source_run == source_run)
            .map(|entry| entry.forecast_hour)
            .max()
            == Some(max_hour);
    let width = longitudes.len();
    for point in 0..grid_len {
        let mut values = frames.iter().map(|frame| frame[point]).collect::<Vec<_>>();
        ecmwf_first_stage_point(
            &mut values,
            kind,
            start,
            latitudes[point / width] as f32,
            longitudes[point % width] as f32,
            is_gust,
            allow_missing_terminal_hermite_d,
        );
        for (frame, value) in frames.iter_mut().zip(values) {
            frame[point] = value;
        }
    }
    Ok(Some(EcmwfRegularRun {
        source_run: source_run.to_string(),
        start,
        frames,
    }))
}

#[cfg(test)]
fn ecmwf_second_stage_value(
    values: &[f32],
    regular_times: &[DateTime<Utc>],
    kind: InterpolationKind,
    time: DateTime<Utc>,
    latitude: f32,
    longitude: f32,
    output_dt_seconds: i64,
) -> f32 {
    ecmwf_second_stage_value_with_terminal_extension(
        values,
        regular_times,
        kind,
        time,
        latitude,
        longitude,
        output_dt_seconds,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn ecmwf_second_stage_value_with_terminal_extension(
    values: &[f32],
    regular_times: &[DateTime<Utc>],
    kind: InterpolationKind,
    time: DateTime<Utc>,
    latitude: f32,
    longitude: f32,
    output_dt_seconds: i64,
    extend_terminal_interval: bool,
) -> f32 {
    if values.is_empty() || regular_times.is_empty() || time < regular_times[0] {
        return f32::NAN;
    }
    let output_dt_seconds = output_dt_seconds.max(1);
    let regular_dt_seconds = regular_times
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).num_seconds())
        .find(|seconds| *seconds > 0)
        .unwrap_or(3 * 3600);
    let last_time = regular_times[regular_times.len() - 1];
    if time > last_time {
        if !extend_terminal_interval || time >= last_time + Duration::seconds(regular_dt_seconds) {
            return f32::NAN;
        }
        // Open-Meteo still evaluates the final Hermite interval with the
        // missing C/D control points replaced by B. A flat or gently changing
        // tail can look like a constant hold, but a steep final slope may
        // legitimately round to a different published integer.
        if matches!(kind, InterpolationKind::Hermite { .. }) {
            // Continue into the normal Hermite branch below.
        } else {
            let value = values.last().copied().unwrap_or(f32::NAN);
            if !value.is_finite() {
                return f32::NAN;
            }
            let scalefactor = match kind {
                InterpolationKind::Direct => 1.0,
                InterpolationKind::BackwardsSum { scalefactor }
                | InterpolationKind::Backwards { scalefactor }
                | InterpolationKind::Linear { scalefactor }
                | InterpolationKind::Hermite { scalefactor, .. }
                | InterpolationKind::SolarBackwardsAveraged { scalefactor } => scalefactor,
            };
            return round_to_scalefactor(value, scalefactor);
        }
    }
    let elapsed = (time - regular_times[0]).num_seconds();
    // A request on the native regular axis is a database read, not another
    // interpolation pass. Re-running solar interpolation at a low-angle
    // native frame can replace a valid 1 W/m² value with the neighbouring
    // clearness index and materially change nonlinear derivatives such as
    // daily ET0.
    if output_dt_seconds == regular_dt_seconds && elapsed % regular_dt_seconds == 0 {
        return usize::try_from(elapsed / regular_dt_seconds)
            .ok()
            .and_then(|index| values.get(index))
            .copied()
            .unwrap_or(f32::NAN);
    }
    let scalefactor = match kind {
        InterpolationKind::Direct => 1.0,
        InterpolationKind::BackwardsSum { scalefactor }
        | InterpolationKind::Backwards { scalefactor }
        | InterpolationKind::Linear { scalefactor }
        | InterpolationKind::Hermite { scalefactor, .. }
        | InterpolationKind::SolarBackwardsAveraged { scalefactor } => scalefactor,
    };
    match kind {
        InterpolationKind::Direct => {
            if elapsed % regular_dt_seconds == 0 {
                usize::try_from(elapsed / regular_dt_seconds)
                    .ok()
                    .and_then(|index| values.get(index))
                    .copied()
                    .unwrap_or(f32::NAN)
            } else {
                f32::NAN
            }
        }
        InterpolationKind::BackwardsSum { .. } | InterpolationKind::Backwards { .. } => {
            let index = (elapsed - output_dt_seconds).div_euclid(regular_dt_seconds) + 1;
            let index = index.clamp(0, values.len() as i64 - 1) as usize;
            let multiply = if matches!(kind, InterpolationKind::BackwardsSum { .. }) {
                output_dt_seconds as f32 / regular_dt_seconds as f32
            } else {
                1.0
            };
            round_to_scalefactor(values[index] * multiply, scalefactor)
        }
        InterpolationKind::Linear { .. } => {
            let index =
                usize::try_from(elapsed.div_euclid(regular_dt_seconds)).unwrap_or(usize::MAX);
            if index >= values.len() {
                return f32::NAN;
            }
            let fraction =
                elapsed.rem_euclid(regular_dt_seconds) as f32 / regular_dt_seconds as f32;
            let b = values.get(index + 1).copied().unwrap_or(values[index]);
            round_to_scalefactor(values[index] * (1.0 - fraction) + b * fraction, scalefactor)
        }
        InterpolationKind::Hermite { bounds, .. } => {
            let index =
                usize::try_from(elapsed.div_euclid(regular_dt_seconds)).unwrap_or(usize::MAX);
            if index >= values.len() || values[index].is_nan() {
                return f32::NAN;
            }
            let fraction =
                elapsed.rem_euclid(regular_dt_seconds) as f32 / regular_dt_seconds as f32;
            let b = values[index];
            let a = if index == 0 || values[index - 1].is_nan() {
                b
            } else {
                values[index - 1]
            };
            let c = values
                .get(index + 1)
                .copied()
                .filter(|value| !value.is_nan())
                .unwrap_or(b);
            let d = values
                .get(index + 2)
                .copied()
                .filter(|value| !value.is_nan())
                .unwrap_or(b);
            let coefficient_a = -a / 2.0 + (3.0 * b) / 2.0 - (3.0 * c) / 2.0 + d / 2.0;
            let coefficient_b = a - (5.0 * b) / 2.0 + 2.0 * c - d / 2.0;
            let coefficient_c = -a / 2.0 + c / 2.0;
            let value = coefficient_a * fraction * fraction * fraction
                + coefficient_b * fraction * fraction
                + coefficient_c * fraction
                + b;
            let value = round_to_scalefactor(value, scalefactor);
            bounds.map_or(value, |(lower, upper)| value.clamp(lower, upper))
        }
        InterpolationKind::SolarBackwardsAveraged { .. } => {
            let Some(position) =
                solar_interpolation_position(regular_times, time, output_dt_seconds)
            else {
                return f32::NAN;
            };
            interpolate_solar_backwards_value(
                [
                    values[position.a],
                    values[position.b],
                    values[position.c],
                    values[position.d],
                ],
                regular_times,
                position,
                time,
                latitude,
                longitude,
                scalefactor,
                true,
                output_dt_seconds,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_ecmwf_regular_series(
    product: &ProductSnapshot,
    decoder: &OfficialDecoder,
    raw_variable: &str,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Option<EcmwfRegularSeries>> {
    if !is_ecmwf_product(&product.product) {
        return Ok(None);
    }
    #[cfg(test)]
    ECMWF_REGULAR_SERIES_BUILD_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut source_runs = product
        .entries_by_source_run
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    source_runs.sort();
    let mut runs = Vec::new();
    for source_run in source_runs {
        if let Some(run) = ecmwf_regularize_source_run(
            product,
            decoder,
            &source_run,
            raw_variable,
            latitudes,
            longitudes,
        )? {
            runs.push(run);
        }
    }
    if runs.is_empty() {
        return Ok(None);
    }
    runs.sort_by(|left, right| left.source_run.cmp(&right.source_run));
    Ok(Some(EcmwfRegularSeries {
        runs,
        public_start_utc: product.manifest.public_start_utc,
        latest_source_run: product.manifest.latest_complete_run.clone(),
    }))
}

fn ecmwf_latest_support_start(
    run_start: DateTime<Utc>,
    public_start_utc: DateTime<Utc>,
) -> DateTime<Utc> {
    let regular_step_seconds = 3 * 3600;
    let elapsed_seconds = (public_start_utc - run_start).num_seconds().max(0);
    let containing_frame_seconds =
        elapsed_seconds.div_euclid(regular_step_seconds) * regular_step_seconds;
    run_start + Duration::seconds((containing_frame_seconds - regular_step_seconds).max(0))
}

fn ecmwf_run_is_eligible(
    source_run: &str,
    run_start: DateTime<Utc>,
    latest_source_run: Option<&str>,
    public_start_utc: Option<DateTime<Utc>>,
    time: DateTime<Utc>,
) -> bool {
    if latest_source_run != Some(source_run) {
        return true;
    }
    public_start_utc
        .map(|boundary| time >= ecmwf_latest_support_start(run_start, boundary))
        .unwrap_or(true)
}

fn ecmwf_cached_regular_series(
    key: EcmwfRegularCacheKey,
    build: impl FnOnce() -> Result<Option<EcmwfRegularSeries>>,
) -> Result<Option<Arc<EcmwfRegularSeries>>> {
    let cached = ECMWF_REQUEST_REGULAR_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .and_then(|cache| cache.get(&key).cloned())
    });
    if cached.is_some() {
        return Ok(cached);
    }
    let Some(built) = build()? else {
        return Ok(None);
    };
    let built = Arc::new(built);
    ECMWF_REQUEST_REGULAR_CACHE.with(|cache| {
        if let Some(cache) = cache.borrow_mut().as_mut() {
            cache.insert(key, built.clone());
        }
    });
    Ok(Some(built))
}

fn ecmwf_structural_value(run: &EcmwfRegularRun, time: DateTime<Utc>, point: usize) -> Option<f32> {
    let elapsed = (time - run.start).num_seconds();
    if elapsed < 0 || elapsed % (3 * 3600) != 0 {
        return None;
    }
    let index = usize::try_from(elapsed / (3 * 3600)).ok()?;
    run.frames
        .get(index)
        .and_then(|frame| frame.get(point))
        .copied()
}

#[cfg(test)]
fn ecmwf_one_level_native_value(
    runs: &[&EcmwfRegularRun],
    latest_source_run: Option<&str>,
    public_start_utc: Option<DateTime<Utc>>,
    time: DateTime<Utc>,
    point: usize,
) -> f32 {
    let mut primary_value = None;
    let mut index = 0;
    while index < runs.len() {
        let source_run = runs[index].source_run.as_str();
        let mut end = index + 1;
        while end < runs.len() && runs[end].source_run == source_run {
            end += 1;
        }

        // One source generation can occur in several immutable coverages: the
        // newest coverage carries a short interpolation-support prefix, while
        // its retained release carries the complete run and long tail. Treat
        // those records as one generation. The newest coverage remains
        // authoritative where it structurally covers the time; the complete
        // retained record only extends missing structure.
        let value = runs[index..end].iter().find_map(|run| {
            if !ecmwf_run_is_eligible(
                &run.source_run,
                run.start,
                latest_source_run,
                public_start_utc,
                time,
            ) {
                return None;
            }
            ecmwf_structural_value(run, time, point)
        });
        if let Some(value) = value {
            if primary_value.is_none() {
                if value.is_finite() {
                    return value;
                }
                primary_value = Some(value);
            } else {
                // Match the update semantics with an explicit one-generation
                // bound: only the immediately older structurally covering run
                // may replace a NaN. A second NaN cannot promote a third run.
                return if value.is_finite() { value } else { f32::NAN };
            }
        }
        index = end;
    }
    f32::NAN
}

fn ecmwf_coverage_native_value(
    series: &EcmwfRegularSeries,
    latest_source_run: Option<&str>,
    public_start_utc: Option<DateTime<Utc>>,
    time: DateTime<Utc>,
    point: usize,
) -> Option<f32> {
    let mut structural_value = None;
    for run in series.runs.iter().rev() {
        if !ecmwf_run_is_eligible(
            &run.source_run,
            run.start,
            latest_source_run,
            public_start_utc,
            time,
        ) {
            continue;
        }
        let Some(value) = ecmwf_structural_value(run, time, point) else {
            continue;
        };
        if value.is_finite() {
            return Some(value);
        }
        structural_value = Some(value);
    }
    structural_value
}

fn ecmwf_one_level_generation_value(
    regular_series: &[Arc<EcmwfRegularSeries>],
    latest_source_run: Option<&str>,
    public_start_utc: Option<DateTime<Utc>>,
    time: DateTime<Utc>,
    point: usize,
) -> f32 {
    let mut series = regular_series.iter().collect::<Vec<_>>();
    series.sort_by(|left, right| right.latest_source_run.cmp(&left.latest_source_run));

    let mut primary_value = None;
    let mut index = 0;
    while index < series.len() {
        let generation = series[index].latest_source_run.as_deref();
        let mut end = index + 1;
        while end < series.len() && series[end].latest_source_run.as_deref() == generation {
            end += 1;
        }

        // A published coverage may contain older source frames solely to
        // bridge a variable's official sparse schedule. Resolve those frames
        // inside the same generation before applying the one-generation NaN
        // fallback to the preceding retained coverage.
        let mut generation_value = None;
        for coverage in &series[index..end] {
            let Some(value) = ecmwf_coverage_native_value(
                coverage,
                latest_source_run,
                public_start_utc,
                time,
                point,
            ) else {
                continue;
            };
            if value.is_finite() {
                generation_value = Some(value);
                break;
            }
            generation_value = Some(value);
        }

        if let Some(value) = generation_value {
            if primary_value.is_none() {
                if value.is_finite() {
                    return value;
                }
                primary_value = Some(value);
            } else {
                return if value.is_finite() { value } else { f32::NAN };
            }
        }
        index = end;
    }
    f32::NAN
}

#[allow(clippy::too_many_arguments)]
fn read_ecmwf_regular_series_grid(
    regular_series: &[Arc<EcmwfRegularSeries>],
    product_name: &str,
    raw_variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<Vec<f32>>> {
    let mut runs = regular_series
        .iter()
        .flat_map(|regular| regular.runs.iter())
        .collect::<Vec<_>>();
    runs.sort_by(|left, right| right.source_run.cmp(&left.source_run));
    if runs.is_empty() {
        return Ok(vec![
            vec![f32::NAN; latitudes.len() * longitudes.len()];
            times.len()
        ]);
    }
    let latest_source_run = regular_series
        .iter()
        .filter_map(|regular| regular.latest_source_run.as_deref())
        .max();
    let public_start_utc = latest_source_run
        .and_then(|latest| {
            regular_series
                .iter()
                .find(|regular| regular.latest_source_run.as_deref() == Some(latest))
        })
        .and_then(|regular| regular.public_start_utc);
    let regular_start = runs.iter().map(|run| run.start).min().unwrap();
    let regular_end = runs
        .iter()
        .filter_map(|run| {
            run.frames
                .len()
                .checked_sub(1)
                .map(|last| run.start + Duration::hours(last as i64 * 3))
        })
        .max()
        .unwrap();
    let regular_times = (0..=((regular_end - regular_start).num_hours() / 3))
        .map(|index| regular_start + Duration::hours(index * 3))
        .collect::<Vec<_>>();
    let kind = ecmwf_interpolation_kind(raw_variable);
    let output_dt_seconds = series_dt_seconds(times);
    let grid_len = latitudes.len() * longitudes.len();
    let width = longitudes.len();
    let mut output = vec![vec![f32::NAN; grid_len]; times.len()];
    for point in 0..grid_len {
        let merged_values = regular_times
            .iter()
            .map(|time| {
                ecmwf_one_level_generation_value(
                    regular_series,
                    latest_source_run,
                    public_start_utc,
                    *time,
                    point,
                )
            })
            .collect::<Vec<_>>();
        for (output_frame, time) in output.iter_mut().zip(times) {
            output_frame[point] = ecmwf_second_stage_value_with_terminal_extension(
                &merged_values,
                &regular_times,
                kind,
                *time,
                latitudes[point / width] as f32,
                longitudes[point % width] as f32,
                output_dt_seconds,
                product_name == "ecmwf_ifs025_ensemble"
                    && raw_variable == "precipitation_probability",
            );
        }
    }
    Ok(output)
}

fn fill_nan_from_fallback(values: &mut [f32], fallback: &[f32]) {
    for (value, fallback_value) in values.iter_mut().zip(fallback) {
        if value.is_nan() && fallback_value.is_finite() {
            *value = *fallback_value;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn read_ecmwf_window_grid_series(
    products: &[Arc<ProductSnapshot>],
    decoder: &OfficialDecoder,
    variable: &str,
    raw_variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<Vec<f32>>> {
    if products.is_empty() {
        bail!("ECMWF IFS025 product snapshot is not available");
    }
    let mut regular_series = Vec::new();
    for product in products {
        let key = EcmwfRegularCacheKey {
            coverage_id: product.manifest.coverage_id.clone(),
            raw_variable: raw_variable.to_string(),
            latitudes: latitudes.iter().map(|value| value.to_bits()).collect(),
            longitudes: longitudes.iter().map(|value| value.to_bits()).collect(),
        };
        if let Some(regular) = ecmwf_cached_regular_series(key, || {
            build_ecmwf_regular_series(product, decoder, raw_variable, latitudes, longitudes)
        })? {
            regular_series.push(regular);
        }
    }
    if regular_series.is_empty() {
        bail!("variable/time is not available: {variable}");
    }
    read_ecmwf_regular_series_grid(
        &regular_series,
        &products[0].product,
        raw_variable,
        times,
        latitudes,
        longitudes,
    )
}

#[allow(clippy::too_many_arguments)]
fn read_gfs_product_grid_series(
    product: &ProductSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    raw_variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Option<Vec<Vec<f32>>>> {
    if let InterpolationKind::SolarBackwardsAveraged { scalefactor } =
        interpolation_kind_for_variable(variable)
    {
        if let Some(values) = read_sparse_solar_grid_series(
            product,
            decoder,
            variable,
            raw_variable,
            times,
            latitudes,
            longitudes,
            scalefactor,
            round_values,
        )? {
            return Ok(Some(values));
        }
    }
    read_exact_native_grid_series(
        product,
        decoder,
        variable,
        raw_variable,
        times,
        latitudes,
        longitudes,
        round_values,
    )
}

fn read_native_entry_grid_time_range(
    product: &ProductSnapshot,
    entry: &BundleEntry,
    decoder: &OfficialDecoder,
    latitudes: &[f64],
    longitudes: &[f64],
    time_index: u64,
    time_count: usize,
) -> Result<Vec<Vec<f32>>> {
    let y_indices = latitudes
        .iter()
        .map(|latitude| {
            grid_index_for_lat_lon(
                &entry.array,
                entry.native_grid.as_ref(),
                *latitude,
                longitudes[0],
            )
            .map(|value| value.0)
        })
        .collect::<Result<Vec<_>>>()?;
    let x_indices = longitudes
        .iter()
        .map(|longitude| {
            grid_index_for_lat_lon(
                &entry.array,
                entry.native_grid.as_ref(),
                latitudes[0],
                *longitude,
            )
            .map(|value| value.1)
        })
        .collect::<Result<Vec<_>>>()?;
    let y0 = *y_indices
        .iter()
        .min()
        .context("regional grid has no rows")?;
    let y1 = *y_indices
        .iter()
        .max()
        .context("regional grid has no rows")?;
    let x0 = *x_indices
        .iter()
        .min()
        .context("regional grid has no columns")?;
    let x1 = *x_indices
        .iter()
        .max()
        .context("regional grid has no columns")?;
    ensure_in_selection(entry, y0, x0)?;
    ensure_in_selection(entry, y1, x1)?;
    let time_count_u64 = u64::try_from(time_count)?;
    if entry.array.chunks.len() != 3
        || entry.selection_ranges.len() != 2
        || time_index + time_count_u64 > entry.array.dimensions[2]
    {
        bail!("native OM time-slab decoding dimensions do not match entry type");
    }
    let height = y1 - y0 + 1;
    let width = x1 - x0 + 1;
    let metadata = build_v3_array_metadata_blob(
        entry.variable_path.as_deref().unwrap_or(&entry.variable),
        entry.array.data_type,
        entry.array.compression,
        &entry.array.dimensions,
        &entry.array.chunks,
        entry
            .array
            .lut_size
            .context("array metadata missing lut_size")?,
        entry
            .array
            .lut_offset
            .context("array metadata missing lut_offset")?,
        entry.array.scale_factor.unwrap_or(1.0),
        entry.array.add_offset.unwrap_or(0.0),
    );
    let reader = entry_range_reader(product, entry)?;
    let rectangle = decoder.decode_grid(
        &metadata,
        &reader,
        &[y0, x0, time_index],
        &[height, width, time_count_u64],
    )?;
    let expected = usize::try_from(height * width * time_count_u64)?;
    if rectangle.len() != expected {
        bail!("decoded native OM time slab has the wrong element count");
    }
    let mut output = vec![Vec::with_capacity(latitudes.len() * longitudes.len()); time_count];
    for y in y_indices {
        for x in &x_indices {
            let point_start = usize::try_from(((y - y0) * width + (*x - x0)) * time_count_u64)?;
            for time_offset in 0..time_count {
                output[time_offset].push(rectangle[point_start + time_offset]);
            }
        }
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn read_product_history_value_with_rounding(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    product_name: &str,
    variable: &str,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    round_values: bool,
) -> Result<f32> {
    let products = snapshot.product_snapshots(product_name);
    if is_gfs_product(product_name) {
        if is_gfs_probability_product(product_name) && raw_variable == "precipitation_probability" {
            return read_gfs_probability_history_value_with_rounding(
                &products,
                decoder,
                raw_variable,
                time,
                latitude,
                longitude,
                round_values,
            );
        }
        if let Some(primary) = products
            .iter()
            .find(|product| product_covers_time(product, raw_variable, time))
        {
            let mut value = read_product_value_with_rounding(
                primary,
                decoder,
                variable,
                raw_variable,
                time,
                latitude,
                longitude,
                round_values,
            )?;
            value = apply_elevation_correction(&primary.product, variable, value);
            if value.is_nan() {
                if let Some(fallback) = products.iter().find(|product| {
                    !Arc::ptr_eq(primary, product)
                        && product_covers_time(product, raw_variable, time)
                }) {
                    value = read_product_value_with_rounding(
                        fallback,
                        decoder,
                        variable,
                        raw_variable,
                        time,
                        latitude,
                        longitude,
                        round_values,
                    )?;
                    value = apply_elevation_correction(&fallback.product, variable, value);
                }
            }
            return Ok(value);
        }
    }
    if is_cams_product(product_name) {
        let mut fallback = f32::NAN;
        let mut found_coverage = false;
        for product in newest_and_previous_products(&products) {
            if !product_covers_time(product, raw_variable, time) {
                continue;
            }
            found_coverage = true;
            fallback = read_product_value_with_rounding(
                product,
                decoder,
                variable,
                raw_variable,
                time,
                latitude,
                longitude,
                round_values,
            )?;
            fallback = apply_elevation_correction(&product.product, variable, fallback);
            if !fallback.is_nan() {
                return Ok(fallback);
            }
        }
        if found_coverage {
            return Ok(fallback);
        }
    }
    for product in &products {
        if !product_covers_time(product, raw_variable, time) {
            continue;
        }
        let value = read_product_value_with_rounding(
            product,
            decoder,
            variable,
            raw_variable,
            time,
            latitude,
            longitude,
            round_values,
        );
        return value.map(|value| apply_elevation_correction(&product.product, variable, value));
    }
    if products.iter().any(|product| {
        product
            .entries
            .keys()
            .any(|entry_key| entry_key.variable == raw_variable)
    }) {
        return Ok(f32::NAN);
    }
    bail!("variable/time is not available: {} {}", raw_variable, time)
}

fn gfs_probability_interpolation_position(
    native_times: &[DateTime<Utc>],
    time: DateTime<Utc>,
) -> Option<(usize, f32, i64)> {
    if native_times.is_empty() || time < native_times[0] {
        return None;
    }
    match native_times.binary_search(&time) {
        Ok(index) => Some((
            index,
            0.0,
            interpolation_stride_seconds(native_times, index),
        )),
        Err(next_index) if next_index > 0 && next_index < native_times.len() => {
            let index = next_index - 1;
            let stride_seconds = (native_times[next_index] - native_times[index]).num_seconds();
            if stride_seconds <= 0 {
                return None;
            }
            Some((
                index,
                (time - native_times[index]).num_seconds() as f32 / stride_seconds as f32,
                stride_seconds,
            ))
        }
        Err(next_index) if next_index == native_times.len() && native_times.len() > 1 => {
            let index = native_times.len() - 1;
            let stride_seconds = (native_times[index] - native_times[index - 1]).num_seconds();
            let elapsed = (time - native_times[index]).num_seconds();
            if stride_seconds <= 0 || elapsed >= stride_seconds {
                return None;
            }
            Some((
                index,
                elapsed as f32 / stride_seconds as f32,
                stride_seconds,
            ))
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn gfs_probability_regularize_source_run(
    product: &ProductSnapshot,
    decoder: Option<&OfficialDecoder>,
    source_run: &str,
    raw_variable: &str,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Option<EcmwfRegularRun>> {
    let mut entries = product
        .entries_by_source_run
        .get(source_run)
        .into_iter()
        .flatten()
        .filter(|entry| entry.variable == raw_variable)
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return Ok(None);
    }
    entries.sort_by_key(|entry| entry.forecast_hour);
    let min_hour = entries
        .iter()
        .map(|entry| entry.forecast_hour)
        .min()
        .unwrap();
    let max_hour = entries
        .iter()
        .map(|entry| entry.forecast_hour)
        .max()
        .unwrap();
    if min_hour % 3 != 0 || max_hour % 3 != 0 {
        bail!("GFS probability source run is not aligned to its 3-hour database axis");
    }
    let base = entries[0].valid_time_utc - Duration::hours(entries[0].forecast_hour);
    if entries
        .iter()
        .any(|entry| entry.valid_time_utc - Duration::hours(entry.forecast_hour) != base)
    {
        bail!("GFS probability source run has inconsistent reference times: {source_run}");
    }
    let start = base + Duration::hours(min_hour);
    let count = usize::try_from((max_hour - min_hour) / 3 + 1)?;
    let grid_len = latitudes.len() * longitudes.len();
    let mut frames = vec![vec![f32::NAN; grid_len]; count];
    for entry in entries {
        let index = usize::try_from((entry.forecast_hour - min_hour) / 3)?;
        frames[index] = if let Some(decoder) = decoder {
            read_entry_grid(product, entry, decoder, latitudes, longitudes)?
        } else if grid_len == 1 {
            vec![read_entry_value(
                product,
                entry,
                None,
                latitudes[0],
                longitudes[0],
            )?]
        } else {
            bail!("official OM decoder is required for a GFS probability grid");
        };
    }
    for point in 0..grid_len {
        let mut values = frames.iter().map(|frame| frame[point]).collect::<Vec<_>>();
        interpolate_ecmwf_hermite_in_place(&mut values, Some((0.0, 100.0)), false, false);
        for (frame, value) in frames.iter_mut().zip(values) {
            frame[point] = if value.is_finite() {
                round_to_scalefactor(value, 1.0)
            } else {
                value
            };
        }
    }
    Ok(Some(EcmwfRegularRun {
        source_run: source_run.to_string(),
        start,
        frames,
    }))
}

fn gfs_history_native_times(
    products: &[Arc<ProductSnapshot>],
    raw_variable: &str,
) -> Vec<DateTime<Utc>> {
    products
        .iter()
        .flat_map(|product| native_times_for_variable(product, raw_variable))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn newest_gfs_history_product_for_time<'a>(
    products: &'a [Arc<ProductSnapshot>],
    raw_variable: &str,
    time: DateTime<Utc>,
) -> Option<&'a ProductSnapshot> {
    let key = EntryKey {
        variable: raw_variable.to_string(),
        valid_time_utc: time,
    };
    products
        .iter()
        .find(|product| product.entries.contains_key(&key))
        .map(AsRef::as_ref)
}

#[allow(clippy::too_many_arguments)]
fn read_gfs_probability_support_value(
    products: &[Arc<ProductSnapshot>],
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<f32>> {
    let key = GfsProbabilitySupportCacheKey {
        product_coverages: products
            .iter()
            .map(|product| {
                (
                    product.product.clone(),
                    product.manifest.coverage_id.clone(),
                )
            })
            .collect(),
        raw_variable: raw_variable.to_string(),
        unix_time: time.timestamp(),
        latitude_bits: latitude.to_bits(),
        longitude_bits: longitude.to_bits(),
    };
    let cached = GFS_REQUEST_PROBABILITY_SUPPORT_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .and_then(|cache| cache.get(&key).copied())
    });
    if let Some(value) = cached {
        return Ok(value);
    }
    let mut runs = Vec::new();
    for product in products {
        let mut source_runs = product
            .entries_by_source_run
            .iter()
            .filter(|(_, entries)| {
                entries
                    .iter()
                    .any(|entry| entry.variable == raw_variable && entry.interpolation_support)
            })
            .map(|(source_run, _)| source_run.clone())
            .collect::<Vec<_>>();
        source_runs.sort();
        for source_run in source_runs {
            if let Some(run) = gfs_probability_regularize_source_run(
                product,
                decoder,
                &source_run,
                raw_variable,
                &[latitude],
                &[longitude],
            )? {
                runs.push(run);
            }
        }
    }
    runs.sort_by(|left, right| left.source_run.cmp(&right.source_run));
    let mut value = None;
    for run in runs {
        let elapsed = (time - run.start).num_seconds();
        if elapsed < 0 || elapsed % (3 * 3600) != 0 {
            continue;
        }
        let index = usize::try_from(elapsed / (3 * 3600))?;
        let Some(candidate) = run.frames.get(index).and_then(|frame| frame.first()) else {
            continue;
        };
        if candidate.is_finite() {
            value = Some(*candidate);
        }
    }
    GFS_REQUEST_PROBABILITY_SUPPORT_CACHE.with(|cache| {
        if let Some(cache) = cache.borrow_mut().as_mut() {
            cache.insert(key, value);
        }
    });
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
fn read_gfs_probability_support_grid(
    products: &[Arc<ProductSnapshot>],
    decoder: &OfficialDecoder,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Option<Vec<f32>>> {
    let grid_len = latitudes.len() * longitudes.len();
    let mut runs = Vec::new();
    for product in products {
        let mut source_runs = product
            .entries_by_source_run
            .iter()
            .filter(|(_, entries)| {
                entries
                    .iter()
                    .any(|entry| entry.variable == raw_variable && entry.interpolation_support)
            })
            .map(|(source_run, _)| source_run.clone())
            .collect::<Vec<_>>();
        source_runs.sort();
        for source_run in source_runs {
            if let Some(run) = gfs_probability_regularize_source_run(
                product,
                Some(decoder),
                &source_run,
                raw_variable,
                latitudes,
                longitudes,
            )? {
                runs.push(run);
            }
        }
    }
    runs.sort_by(|left, right| left.source_run.cmp(&right.source_run));
    let mut values = vec![f32::NAN; grid_len];
    let mut found = false;
    for run in runs {
        let elapsed = (time - run.start).num_seconds();
        if elapsed < 0 || elapsed % (3 * 3600) != 0 {
            continue;
        }
        let index = usize::try_from(elapsed / (3 * 3600))?;
        let Some(frame) = run.frames.get(index) else {
            continue;
        };
        for (target, candidate) in values.iter_mut().zip(frame) {
            if candidate.is_finite() {
                *target = *candidate;
                found = true;
            }
        }
    }
    Ok(found.then_some(values))
}

#[allow(clippy::too_many_arguments)]
fn read_gfs_probability_history_value_with_rounding(
    products: &[Arc<ProductSnapshot>],
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    round_values: bool,
) -> Result<f32> {
    read_gfs_probability_history_point_series_with_rounding(
        products,
        decoder,
        raw_variable,
        &[time],
        latitude,
        longitude,
        round_values,
    )?
    .pop()
    .context("GFS probability point series is empty")
}

fn build_gfs_probability_point_history(
    products: &[Arc<ProductSnapshot>],
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    latitude: f64,
    longitude: f64,
) -> Result<GfsProbabilityPointHistory> {
    let regular_seconds = products
        .iter()
        .flat_map(|product| native_times_for_variable(product, raw_variable).into_iter())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).num_seconds())
        .filter(|seconds| *seconds > 0)
        .min()
        .unwrap_or(3 * 3600);
    let mut stitched = BTreeMap::<DateTime<Utc>, f32>::new();
    for product in products.iter().rev() {
        let native_times = native_times_for_variable(product, raw_variable);
        let (Some(first), Some(last)) =
            (native_times.first().copied(), native_times.last().copied())
        else {
            continue;
        };
        let span = (last - first).num_seconds();
        if span < 0 || span % regular_seconds != 0 {
            bail!("GFS probability run is not aligned to its 3-hour database axis");
        }
        let mut values = vec![f32::NAN; usize::try_from(span / regular_seconds + 1)?];
        for native_time in native_times {
            let elapsed = (native_time - first).num_seconds();
            if elapsed < 0 || elapsed % regular_seconds != 0 {
                bail!("GFS probability frame is not aligned to its 3-hour database axis");
            }
            values[usize::try_from(elapsed / regular_seconds)?] = read_native_value(
                product,
                decoder,
                raw_variable,
                native_time,
                latitude,
                longitude,
            )?;
        }
        interpolate_ecmwf_hermite_in_place(&mut values, Some((0.0, 100.0)), false, false);
        for (index, value) in values.into_iter().enumerate() {
            if value.is_finite() {
                stitched.insert(
                    first + Duration::seconds(regular_seconds * index as i64),
                    round_to_scalefactor(value, 1.0),
                );
            }
        }
    }
    let (Some(first), Some(last)) = (
        stitched.first_key_value().map(|(time, _)| *time),
        stitched.last_key_value().map(|(time, _)| *time),
    ) else {
        return Ok(GfsProbabilityPointHistory {
            regular_seconds,
            regular_times: Vec::new(),
            values: Vec::new(),
        });
    };
    let span = (last - first).num_seconds();
    let regular_times = (0..=span / regular_seconds)
        .map(|index| first + Duration::seconds(regular_seconds * index))
        .collect::<Vec<_>>();
    let values = regular_times
        .iter()
        .map(|time| stitched.get(time).copied().unwrap_or(f32::NAN))
        .collect::<Vec<_>>();
    Ok(GfsProbabilityPointHistory {
        regular_seconds,
        regular_times,
        values,
    })
}

fn gfs_cached_probability_point_history(
    key: GfsProbabilityHistoryCacheKey,
    build: impl FnOnce() -> Result<GfsProbabilityPointHistory>,
) -> Result<Arc<GfsProbabilityPointHistory>> {
    let cached = GFS_REQUEST_PROBABILITY_HISTORY_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .and_then(|cache| cache.get(&key).cloned())
    });
    if let Some(cached) = cached {
        return Ok(cached);
    }
    let built = Arc::new(build()?);
    GFS_REQUEST_PROBABILITY_HISTORY_CACHE.with(|cache| {
        if let Some(cache) = cache.borrow_mut().as_mut() {
            cache.insert(key, Arc::clone(&built));
        }
    });
    Ok(built)
}

#[allow(clippy::too_many_arguments)]
fn read_gfs_probability_history_point_series_with_rounding(
    products: &[Arc<ProductSnapshot>],
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    times: &[DateTime<Utc>],
    latitude: f64,
    longitude: f64,
    round_values: bool,
) -> Result<Vec<f32>> {
    let key = GfsProbabilityHistoryCacheKey {
        product_coverages: products
            .iter()
            .map(|product| {
                (
                    product.product.clone(),
                    product.manifest.coverage_id.clone(),
                )
            })
            .collect(),
        raw_variable: raw_variable.to_string(),
        latitude_bits: latitude.to_bits(),
        longitude_bits: longitude.to_bits(),
    };
    let history = gfs_cached_probability_point_history(key, || {
        build_gfs_probability_point_history(products, decoder, raw_variable, latitude, longitude)
    })?;
    if history.regular_times.is_empty() {
        return Ok(vec![f32::NAN; times.len()]);
    }
    let regular_seconds = history.regular_seconds;
    let regular_times = &history.regular_times;
    let values = &history.values;
    times
        .iter()
        .map(|time| {
            let Some((index, fraction, _)) =
                gfs_probability_interpolation_position(regular_times, *time)
            else {
                return Ok(f32::NAN);
            };
            let b = values[index];
            if !b.is_finite() || (fraction == 0.0 && index + 1 >= regular_times.len()) {
                return Ok(maybe_round_to_scalefactor(b, 1.0, round_values));
            }
            let a = index
                .checked_sub(1)
                .and_then(|previous| values.get(previous))
                .copied()
                .filter(|value| value.is_finite())
                .unwrap_or(b);
            let c = values
                .get(index + 1)
                .copied()
                .filter(|value| value.is_finite())
                .unwrap_or(b);
            let d_time = regular_times[index] + Duration::seconds(regular_seconds * 2);
            let direct_d = values
                .get(index + 2)
                .copied()
                .filter(|value| value.is_finite());
            let support_d = if direct_d.is_none() {
                read_gfs_probability_support_value(
                    products,
                    decoder,
                    raw_variable,
                    d_time,
                    latitude,
                    longitude,
                )?
            } else {
                None
            };
            let d = direct_d.or(support_d).unwrap_or(b);
            let coeff_a = -a / 2.0 + (3.0 * b) / 2.0 - (3.0 * c) / 2.0 + d / 2.0;
            let coeff_b = a - (5.0 * b) / 2.0 + 2.0 * c - d / 2.0;
            let coeff_c = -a / 2.0 + c / 2.0;
            let value = coeff_a * fraction * fraction * fraction
                + coeff_b * fraction * fraction
                + coeff_c * fraction
                + b;
            Ok(maybe_round_to_scalefactor(value, 1.0, round_values).clamp(0.0, 100.0))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn read_gfs_probability_history_grid_with_rounding(
    products: &[Arc<ProductSnapshot>],
    decoder: &OfficialDecoder,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Vec<f32>> {
    let grid_len = latitudes.len() * longitudes.len();
    let native_times = gfs_history_native_times(products, raw_variable);
    let Some((index, fraction, stride_seconds)) =
        gfs_probability_interpolation_position(&native_times, time)
    else {
        return Ok(vec![f32::NAN; grid_len]);
    };
    let read = |native_time| -> Result<Option<Vec<f32>>> {
        newest_gfs_history_product_for_time(products, raw_variable, native_time)
            .map(|product| {
                read_native_grid(
                    product,
                    decoder,
                    raw_variable,
                    native_time,
                    latitudes,
                    longitudes,
                )
            })
            .transpose()
    };
    let b = read(native_times[index])?.unwrap_or_else(|| vec![f32::NAN; grid_len]);
    if fraction == 0.0 && index + 1 >= native_times.len() {
        return Ok(b
            .into_iter()
            .map(|value| maybe_round_to_scalefactor(value, 1.0, round_values))
            .collect());
    }
    let a =
        read(native_times[index] - Duration::seconds(stride_seconds))?.unwrap_or_else(|| b.clone());
    let c_time = native_times[index] + Duration::seconds(stride_seconds);
    let c = read(c_time)?.unwrap_or_else(|| b.clone());
    let d_time = c_time + Duration::seconds(stride_seconds);
    let d = match read(d_time)? {
        Some(values) => Some(values),
        None => read_gfs_probability_support_grid(
            products,
            decoder,
            raw_variable,
            d_time,
            latitudes,
            longitudes,
        )?,
    };
    Ok((0..grid_len)
        .map(|point| {
            let b = b[point];
            if !b.is_finite() {
                return b;
            }
            let a = a[point];
            let a = if a.is_finite() { a } else { b };
            let c = c[point];
            let c = if c.is_finite() { c } else { b };
            let d = d.as_ref().map(|values| values[point]).map_or(b, |value| {
                if value.is_finite() {
                    value
                } else {
                    b
                }
            });
            let coeff_a = -a / 2.0 + (3.0 * b) / 2.0 - (3.0 * c) / 2.0 + d / 2.0;
            let coeff_b = a - (5.0 * b) / 2.0 + 2.0 * c - d / 2.0;
            let coeff_c = -a / 2.0 + c / 2.0;
            let value = coeff_a * fraction * fraction * fraction
                + coeff_b * fraction * fraction
                + coeff_c * fraction
                + b;
            maybe_round_to_scalefactor(value, 1.0, round_values).clamp(0.0, 100.0)
        })
        .collect())
}

fn read_cams_mixed_carbon_monoxide(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    round_values: bool,
) -> Result<f32> {
    // This is the CamsMixer's integrateIfNaNSmooth(width: 3): use the
    // greenhouse-gas CO where present, fill a gap from CAMS Global, then blend
    // the three preceding hours into that transition. Each source reader first
    // quantizes its hourly Hermite result to the variable scale factor (1 for
    // CO); only the cross-source blend remains fractional.
    let mut high = Vec::with_capacity(4);
    let mut low = Vec::with_capacity(4);
    for offset in 0..=3 {
        let sample_time = time + Duration::hours(offset);
        high.push(read_cams_greenhouse_carbon_monoxide_for_mixer(
            snapshot,
            decoder,
            sample_time,
            latitude,
            longitude,
        )?);
        low.push(read_product_history_value_with_rounding(
            snapshot,
            decoder,
            "cams_global",
            "carbon_monoxide",
            "carbon_monoxide",
            sample_time,
            latitude,
            longitude,
            true,
        )?);
    }

    let mut steps_since_nan = 3_i32;
    for index in (0..high.len()).rev() {
        steps_since_nan += 1;
        if low[index].is_nan() {
            continue;
        }
        if high[index].is_nan() {
            steps_since_nan = 0;
            high[index] = low[index];
            continue;
        }
        if steps_since_nan > 3 {
            continue;
        }
        high[index] = (low[index] * (4 - steps_since_nan) as f32
            + high[index] * steps_since_nan as f32)
            / 4.0;
    }
    // Carbon monoxide is serialized with one decimal. Do not quantize the
    // blended transition before JSON serialization (e.g. 102.75 -> 102.8).
    let _ = round_values;
    Ok(high[0])
}

fn read_cams_greenhouse_carbon_monoxide_for_mixer(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let products = snapshot.product_snapshots("cams_global_greenhouse_gases");
    let product = products
        .first()
        .cloned()
        .context("CAMS greenhouse product is unavailable")?;
    if let Some(covering) = newest_and_previous_products(&products)
        .find(|candidate| product_covers_time(candidate, "carbon_monoxide", time))
    {
        return read_cams_greenhouse_carbon_monoxide_with_history(
            snapshot, covering, decoder, time, latitude, longitude,
        );
    }
    let native_times = native_times_for_variable(&product, "carbon_monoxide");
    let Some(first) = native_times.first().copied() else {
        return Ok(f32::NAN);
    };
    let Some(last) = native_times.last().copied() else {
        return Ok(f32::NAN);
    };
    if time < first {
        return Ok(f32::NAN);
    }
    let cadence = native_times
        .windows(2)
        .next()
        .map(|pair| pair[1] - pair[0])
        .unwrap_or_else(|| Duration::hours(3));
    if time > last {
        if time < last + cadence {
            // GenericReader expands the source range by Hermite's padding.
            // Immediately after the final stored greenhouse frame, the
            // unavailable C and D samples are therefore present as NaN and
            // Interpolation.interpolateHermite replaces both with B.  Do not
            // clamp these two hourly samples to B: the preceding A sample
            // still changes the Hermite curve before it settles at B.
            let b = read_native_value(
                &product,
                decoder,
                "carbon_monoxide",
                last,
                latitude,
                longitude,
            )?;
            if !b.is_finite() {
                return Ok(f32::NAN);
            }
            let a_time = last - cadence;
            let a = match read_native_value_if_present(
                &product,
                decoder,
                "carbon_monoxide",
                &native_times,
                a_time,
                latitude,
                longitude,
            )? {
                Some(value) if value.is_finite() => value,
                _ => b,
            };
            let fraction = (time - last).num_seconds() as f32 / cadence.num_seconds() as f32;
            let c = b;
            let d = b;
            let coeff_a = -a / 2.0 + (3.0 * b) / 2.0 - (3.0 * c) / 2.0 + d / 2.0;
            let coeff_b = a - (5.0 * b) / 2.0 + 2.0 * c - d / 2.0;
            let coeff_c = -a / 2.0 + c / 2.0;
            let h = coeff_a * fraction * fraction * fraction
                + coeff_b * fraction * fraction
                + coeff_c * fraction
                + b;
            return Ok(round_to_scalefactor(h, 1.0).clamp(0.0, f32::INFINITY));
        }
        return Ok(f32::NAN);
    }
    Ok(f32::NAN)
}

fn read_cams_greenhouse_carbon_monoxide_with_history(
    snapshot: &OmDataSnapshot,
    product: &ProductSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let native_times = native_times_for_variable(product, "carbon_monoxide");
    let Some((index, fraction)) = interpolation_index(&native_times, time) else {
        return Ok(f32::NAN);
    };
    let b = read_native_value(
        product,
        decoder,
        "carbon_monoxide",
        native_times[index],
        latitude,
        longitude,
    )?;
    if index + 1 >= native_times.len() {
        return Ok(round_to_scalefactor(b, 1.0).clamp(0.0, f32::INFINITY));
    }
    let stride_seconds = interpolation_stride_seconds(&native_times, index);
    let a_time = native_times[index] - Duration::seconds(stride_seconds);
    let a = match read_native_value_if_present(
        product,
        decoder,
        "carbon_monoxide",
        &native_times,
        a_time,
        latitude,
        longitude,
    )? {
        Some(value) if value.is_finite() => value,
        Some(_) => b,
        None => read_cams_greenhouse_retained_native_value(
            snapshot, decoder, a_time, latitude, longitude,
        )?
        .filter(|value| value.is_finite())
        .unwrap_or(b),
    };
    let c = read_native_value(
        product,
        decoder,
        "carbon_monoxide",
        native_times[index + 1],
        latitude,
        longitude,
    )?;
    let c = if c.is_finite() { c } else { b };
    let d_time = native_times[index + 1] + Duration::seconds(stride_seconds);
    let d = match read_native_value_if_present(
        product,
        decoder,
        "carbon_monoxide",
        &native_times,
        d_time,
        latitude,
        longitude,
    )? {
        Some(value) if value.is_finite() => value,
        Some(_) => b,
        None => b,
    };
    let coeff_a = -a / 2.0 + (3.0 * b) / 2.0 - (3.0 * c) / 2.0 + d / 2.0;
    let coeff_b = a - (5.0 * b) / 2.0 + 2.0 * c - d / 2.0;
    let coeff_c = -a / 2.0 + c / 2.0;
    let h = coeff_a * fraction * fraction * fraction
        + coeff_b * fraction * fraction
        + coeff_c * fraction
        + b;
    Ok(round_to_scalefactor(h, 1.0).clamp(0.0, f32::INFINITY))
}

fn read_cams_greenhouse_retained_native_value(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<f32>> {
    for product in snapshot.product_snapshots("cams_global_greenhouse_gases") {
        let native_times = native_times_for_variable(&product, "carbon_monoxide");
        if native_times.binary_search(&time).is_err() {
            continue;
        }
        return read_native_value(
            &product,
            decoder,
            "carbon_monoxide",
            time,
            latitude,
            longitude,
        )
        .map(Some);
    }
    Ok(None)
}

fn product_covers_time(product: &ProductSnapshot, raw_variable: &str, time: DateTime<Utc>) -> bool {
    let native_times = native_times_for_variable(product, raw_variable);
    match (native_times.first(), native_times.last()) {
        (Some(first), Some(last)) => time >= *first && time <= *last,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn read_product_value_with_rounding(
    product: &ProductSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    round_values: bool,
) -> Result<f32> {
    match interpolation_kind_for_variable(variable) {
        InterpolationKind::Direct => {}
        InterpolationKind::SolarBackwardsAveraged { scalefactor } => {
            return read_solar_backwards_value(
                product,
                decoder,
                raw_variable,
                time,
                latitude,
                longitude,
                scalefactor,
                round_values,
            );
        }
        InterpolationKind::BackwardsSum { scalefactor } => {
            return read_backwards_value(
                product,
                decoder,
                raw_variable,
                time,
                latitude,
                longitude,
                true,
                scalefactor,
                round_values,
            );
        }
        InterpolationKind::Backwards { scalefactor } => {
            return read_backwards_value(
                product,
                decoder,
                raw_variable,
                time,
                latitude,
                longitude,
                false,
                scalefactor,
                round_values,
            );
        }
        InterpolationKind::Linear { scalefactor } => {
            return read_linear_value(
                product,
                decoder,
                raw_variable,
                time,
                latitude,
                longitude,
                scalefactor,
                round_values,
            );
        }
        InterpolationKind::Hermite {
            scalefactor,
            bounds,
        } => {
            return read_hermite_value(
                product,
                decoder,
                raw_variable,
                time,
                latitude,
                longitude,
                scalefactor,
                bounds,
                round_values,
            );
        }
    }
    let key = EntryKey {
        variable: raw_variable.to_string(),
        valid_time_utc: time,
    };
    let entry = product
        .entries
        .get(&key)
        .with_context(|| format!("variable/time is not available: {} {}", raw_variable, time))?;
    read_entry_value(product, entry, decoder, latitude, longitude)
}

#[derive(Debug, Clone, Copy)]
struct SolarInterpolationPosition {
    a: usize,
    b: usize,
    c: usize,
    d: usize,
    fraction: f32,
}

/// Match Open-Meteo's `interpolateSolarBackwards`: native radiation values
/// are backward averages centred half a native interval before their valid
/// time, while the requested value is centred half its output timestep before
/// its timestamp.
fn solar_interpolation_position(
    native_times: &[DateTime<Utc>],
    time: DateTime<Utc>,
    output_dt_seconds: i64,
) -> Option<SolarInterpolationPosition> {
    if native_times.is_empty() || time > *native_times.last()? {
        return None;
    }
    let centers = native_times
        .iter()
        .enumerate()
        .map(|(index, native_time)| {
            *native_time - Duration::seconds(native_dt_seconds_at(native_times, index) / 2)
        })
        .collect::<Vec<_>>();
    let target_center = time - Duration::seconds(output_dt_seconds.max(1) / 2);
    if target_center < centers[0] {
        return None;
    }
    let next = centers.partition_point(|center| *center <= target_center);
    let (b, c, fraction) = if next < centers.len() {
        let b = next.saturating_sub(1);
        let c = next;
        let span = (centers[c] - centers[b]).num_seconds();
        let fraction = if span <= 0 {
            0.0
        } else {
            (target_center - centers[b]).num_seconds() as f32 / span as f32
        };
        (b, c, fraction)
    } else {
        let b = centers.len() - 1;
        let span = native_dt_seconds_at(native_times, b).max(1);
        let fraction = (target_center - centers[b]).num_seconds() as f32 / span as f32;
        (b, b, fraction)
    };
    Some(SolarInterpolationPosition {
        a: b.saturating_sub(1),
        b,
        c,
        d: (c + 1).min(native_times.len() - 1),
        fraction,
    })
}

#[allow(clippy::too_many_arguments)]
fn interpolate_solar_backwards_value(
    values: [f32; 4],
    native_times: &[DateTime<Utc>],
    position: SolarInterpolationPosition,
    time: DateTime<Utc>,
    latitude: f32,
    longitude: f32,
    scalefactor: f32,
    round_values: bool,
    output_dt_seconds: i64,
) -> f32 {
    const SOLAR_CONSTANT: f32 = 1367.7;
    let [a_value, b_value, c_value, d_value] = values;
    if b_value.is_nan() {
        return f32::NAN;
    }
    let output_solar = extra_terrestrial_radiation_factor_backwards(
        time,
        output_dt_seconds.max(1),
        latitude,
        longitude,
    );
    if output_solar == 0.0 {
        return 0.0;
    }
    let sample_solar = |index: usize| {
        extra_terrestrial_radiation_factor_backwards(
            native_times[index],
            native_dt_seconds_at(native_times, index),
            latitude,
            longitude,
        )
    };
    let minimum_solar = 5.0 / SOLAR_CONSTANT;
    let radiation_limit = SOLAR_CONSTANT * 0.95;
    let clearness = |value: f32, solar: f32| {
        if solar <= minimum_solar {
            f32::NAN
        } else {
            (value / solar).min(radiation_limit)
        }
    };
    let mut kt_a = clearness(a_value, sample_solar(position.a));
    let mut kt_b = clearness(b_value, sample_solar(position.b));
    let mut kt_c = clearness(c_value, sample_solar(position.c));
    let mut kt_d = clearness(d_value, sample_solar(position.d));
    if kt_a.is_nan() {
        kt_a = if !kt_b.is_nan() {
            kt_b
        } else if !kt_c.is_nan() {
            kt_c
        } else {
            kt_d
        };
    }
    if kt_b.is_nan() {
        kt_b = if !kt_a.is_nan() {
            kt_a
        } else if !kt_c.is_nan() {
            kt_c
        } else {
            kt_d
        };
    }
    if kt_c.is_nan() {
        kt_c = if !kt_b.is_nan() {
            kt_b
        } else if !kt_d.is_nan() {
            kt_d
        } else {
            kt_a
        };
    }
    if kt_d.is_nan() {
        kt_d = if !kt_c.is_nan() {
            kt_c
        } else if !kt_b.is_nan() {
            kt_b
        } else {
            kt_a
        };
    }
    let fraction = position.fraction;
    let coefficient_a = -kt_a / 2.0 + (3.0 * kt_b) / 2.0 - (3.0 * kt_c) / 2.0 + kt_d / 2.0;
    let coefficient_b = kt_a - (5.0 * kt_b) / 2.0 + 2.0 * kt_c - kt_d / 2.0;
    let coefficient_c = -kt_a / 2.0 + kt_c / 2.0;
    let mut value = (coefficient_a * fraction * fraction * fraction
        + coefficient_b * fraction * fraction
        + coefficient_c * fraction
        + kt_b)
        * output_solar;
    if value < 0.0 && b_value >= 0.0 && c_value >= 0.0 {
        value = (kt_b * (1.0 - fraction) + kt_c * fraction) * output_solar;
    }
    maybe_round_to_scalefactor(value, scalefactor, round_values)
}

#[allow(clippy::too_many_arguments)]
fn read_backwards_value(
    product: &ProductSnapshot,
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    preserve_sum: bool,
    scalefactor: f32,
    round_values: bool,
) -> Result<f32> {
    let native_times = native_times_for_variable(product, raw_variable);
    if native_times.is_empty() {
        bail!("variable/time is not available: {} {}", raw_variable, time);
    }
    if time < native_times[0] || time > *native_times.last().expect("checked not empty") {
        return Ok(f32::NAN);
    }
    let Some(native_index) = native_times
        .iter()
        .position(|native_time| *native_time >= time)
    else {
        return Ok(f32::NAN);
    };
    let key = EntryKey {
        variable: raw_variable.to_string(),
        valid_time_utc: native_times[native_index],
    };
    let entry = product
        .entries
        .get(&key)
        .with_context(|| format!("variable/time is not available: {} {}", raw_variable, time))?;
    let value = read_entry_value(product, entry, decoder, latitude, longitude)?;
    let native_dt_seconds = native_dt_seconds_at(&native_times, native_index);
    let scaled = if preserve_sum && native_dt_seconds > 0 {
        value * (3600.0 / native_dt_seconds as f32)
    } else {
        value
    };
    Ok(maybe_round_to_scalefactor(
        scaled,
        scalefactor,
        round_values,
    ))
}

/// Port of Open-Meteo's current `interpolateInplaceSolarBackwards`. The source
/// algorithm is deliberately sequential: after a sparse interval is filled,
/// its C value is deaveraged in place and becomes the already-processed B input
/// of later intervals. Centered clearness-index splines preserve each source
/// interval mean across mixed three- and six-hour cadences.
pub(crate) fn interpolate_solar_backwards_in_place(
    values: &mut [f32],
    start: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) {
    interpolate_solar_backwards_with_dt_in_place(values, start, 3600, latitude, longitude);
}

fn interpolate_solar_backwards_with_dt_in_place(
    values: &mut [f32],
    start: DateTime<Utc>,
    dt_seconds: i64,
    latitude: f64,
    longitude: f64,
) {
    if values.is_empty() || !values.iter().any(|value| value.is_nan()) {
        return;
    }

    let mut first_valid = values.len();
    for (time_index, value) in values.iter().enumerate() {
        if value.is_nan() {
            continue;
        }
        if first_valid == values.len() {
            first_valid = time_index;
            continue;
        }
        first_valid = first_valid.saturating_sub(time_index - first_valid - 1);
        break;
    }
    if first_valid >= values.len() {
        return;
    }

    let solar = (0..values.len())
        .map(|index| {
            clear_sky_radiation_factor_backwards(
                start + Duration::seconds(index as i64 * dt_seconds),
                dt_seconds,
                latitude as f32,
                longitude as f32,
            )
        })
        .collect::<Vec<_>>();
    let radiation_limit = SOLAR_CONSTANT * 0.95;
    let radiation_minimum = 5.0 / SOLAR_CONSTANT;

    for missing in first_valid..values.len() {
        if !values[missing].is_nan() {
            continue;
        }
        // Official input ranges contain a valid padding value before the first
        // interpolated hour. Avoid an invalid B index for malformed standalone
        // data while preserving the production path.
        if missing == 0 {
            break;
        }
        let pos_b = missing - 1;
        let maximum_missing_steps = 12 * 3600 / dt_seconds;
        let search_end =
            (missing + 2 + usize::try_from(maximum_missing_steps).unwrap_or(0)).min(values.len());
        let Some(pos_c) = ((missing + 1)..search_end).find(|index| !values[*index].is_nan()) else {
            break;
        };
        let value_c = values[pos_c];
        let width = pos_c - pos_b;
        let pos_a = pos_b.checked_sub(width);
        let pos_d = pos_c
            .checked_add(width)
            .filter(|index| *index < values.len());
        let pos_b_valid = pos_a.is_some_and(|index| !values[index].is_nan());
        let pos_d_valid = pos_d.is_some_and(|index| !values[index].is_nan());

        let solar_average_c = solar[(pos_b + 1)..=pos_c].iter().sum::<f32>() / width as f32;
        let kt_c = if solar_average_c <= radiation_minimum || value_c <= 0.0 {
            f32::NAN
        } else {
            (value_c / solar_average_c).min(radiation_limit)
        };

        let kt_b = if pos_b_valid {
            let pos_a = pos_a.expect("checked valid A position");
            let value_b = values[(pos_a + 1)..=pos_b].iter().sum::<f32>() / width as f32;
            let solar_average_b = solar[(pos_a + 1)..=pos_b].iter().sum::<f32>() / width as f32;
            if solar_average_b <= radiation_minimum {
                kt_c
            } else {
                (value_b / solar_average_b).min(radiation_limit)
            }
        } else {
            kt_c
        };

        let kt_d = if pos_d_valid {
            let pos_d = pos_d.expect("checked valid D position");
            let solar_average_d = solar[(pos_c + 1)..=pos_d].iter().sum::<f32>() / width as f32;
            if solar_average_d <= radiation_minimum {
                kt_c
            } else {
                (values[pos_d] / solar_average_d).min(radiation_limit)
            }
        } else {
            kt_c
        };

        if kt_b.is_nan() && kt_c.is_nan() && kt_d.is_nan() {
            values[missing..pos_c].fill(0.0);
            continue;
        }

        let recovered = || {
            let kt_c = if kt_c.is_finite() {
                kt_c
            } else if kt_b.is_finite() {
                kt_b
            } else {
                kt_d
            };
            let kt_b = if kt_b.is_finite() { kt_b } else { kt_c };
            let kt_d = if kt_d.is_finite() { kt_d } else { kt_c };
            (kt_b, kt_c, kt_d)
        };
        let spline = |fraction: f32| {
            let (kt_b, kt_c, kt_d) = recovered();
            let fraction = fraction.clamp(0.0, 1.0);
            let inverse = 1.0 - fraction;
            let start_point = (kt_b + kt_c) * 0.5;
            let end_point = (kt_c + kt_d) * 0.5;
            inverse * inverse * start_point
                + 2.0 * inverse * fraction * kt_c
                + fraction * fraction * end_point
        };

        let mean_preservation_factor = if value_c > 5.0 {
            let count = pos_c - missing + 1;
            let mean = (missing..=pos_c)
                .map(|target| {
                    let fraction = (target - pos_b) as f32 / width as f32;
                    spline(fraction).max(0.0) * solar[target]
                })
                .sum::<f32>();
            if mean <= 5.0 {
                0.0
            } else {
                value_c / mean * count as f32
            }
        } else {
            1.0
        };

        for target in missing..=pos_c {
            let fraction = (target - pos_b) as f32 / width as f32;
            values[target] = spline(fraction).max(0.0) * solar[target] * mean_preservation_factor;
        }
    }
}

fn sparse_solar_first_missing_hour(native_times: &[DateTime<Utc>]) -> Option<DateTime<Utc>> {
    native_times.windows(2).find_map(|pair| {
        (pair[1] - pair[0] > Duration::hours(1)).then_some(pair[0] + Duration::hours(1))
    })
}

#[allow(clippy::too_many_arguments)]
fn read_sparse_solar_grid_series(
    product: &ProductSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    raw_variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
    scalefactor: f32,
    round_values: bool,
) -> Result<Option<Vec<Vec<f32>>>> {
    let native_times = native_times_for_variable(product, raw_variable);
    let Some(first_missing) = sparse_solar_first_missing_hour(&native_times) else {
        return Ok(None);
    };
    if !times.iter().any(|time| *time >= first_missing) {
        return Ok(None);
    }
    let Some(first_native) = native_times.first().copied() else {
        return Ok(None);
    };
    let last_native = *native_times.last().expect("checked non-empty native times");
    let raw_frames = if let Some(frames) = read_exact_native_grid_series(
        product,
        decoder,
        variable,
        raw_variable,
        &native_times,
        latitudes,
        longitudes,
        false,
    )? {
        frames
    } else {
        native_times
            .iter()
            .map(|time| {
                read_native_grid(product, decoder, raw_variable, *time, latitudes, longitudes)
            })
            .collect::<Result<Vec<_>>>()?
    };
    let grid_len = latitudes.len() * longitudes.len();
    let expanded_len = usize::try_from((last_native - first_native).num_hours() + 1)?;
    let mut expanded = vec![vec![f32::NAN; grid_len]; expanded_len];
    for (native_time, frame) in native_times.iter().zip(raw_frames) {
        let elapsed = (*native_time - first_native).num_seconds();
        if elapsed < 0 || elapsed % 3600 != 0 || frame.len() != grid_len {
            bail!("sparse solar native series is not aligned to the hourly grid");
        }
        expanded[usize::try_from(elapsed / 3600)?] = frame;
    }

    let width = longitudes.len();
    for point in 0..grid_len {
        let mut point_values = expanded
            .iter()
            .map(|frame| frame[point])
            .collect::<Vec<_>>();
        interpolate_solar_backwards_in_place(
            &mut point_values,
            first_native,
            latitudes[point / width],
            longitudes[point % width],
        );
        for (frame, value) in expanded.iter_mut().zip(point_values) {
            frame[point] = value;
        }
    }

    Ok(Some(
        times
            .iter()
            .map(|time| {
                let elapsed = (*time - first_native).num_seconds();
                if elapsed < 0 || elapsed % 3600 != 0 || *time > last_native {
                    return vec![f32::NAN; grid_len];
                }
                expanded[usize::try_from(elapsed / 3600).expect("checked non-negative hour")]
                    .iter()
                    .map(|value| maybe_round_to_scalefactor(*value, scalefactor, round_values))
                    .collect()
            })
            .collect(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn read_solar_backwards_value(
    product: &ProductSnapshot,
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    scalefactor: f32,
    round_values: bool,
) -> Result<f32> {
    let native_times = native_times_for_variable(product, raw_variable);
    let (latitude, longitude) = current_product_sampling(&product.product)
        .map(|sampling| (sampling.latitude, sampling.longitude))
        .unwrap_or((latitude, longitude));
    if sparse_solar_first_missing_hour(&native_times).is_some_and(|first| time >= first) {
        let first_native = *native_times
            .first()
            .context("sparse solar series is empty")?;
        let last_native = *native_times
            .last()
            .context("sparse solar series is empty")?;
        if time < first_native
            || time > last_native
            || (time - first_native).num_seconds() % 3600 != 0
        {
            return Ok(f32::NAN);
        }
        let mut expanded =
            vec![f32::NAN; usize::try_from((last_native - first_native).num_hours() + 1)?];
        for native_time in &native_times {
            let index = usize::try_from((*native_time - first_native).num_hours())?;
            expanded[index] = read_native_value(
                product,
                decoder,
                raw_variable,
                *native_time,
                latitude,
                longitude,
            )?;
        }
        interpolate_solar_backwards_in_place(&mut expanded, first_native, latitude, longitude);
        return Ok(maybe_round_to_scalefactor(
            expanded[usize::try_from((time - first_native).num_hours())?],
            scalefactor,
            round_values,
        ));
    }
    let Ok(index) = native_times.binary_search(&time) else {
        return Ok(f32::NAN);
    };
    let value = read_native_value(
        product,
        decoder,
        raw_variable,
        native_times[index],
        latitude,
        longitude,
    )?;
    Ok(maybe_round_to_scalefactor(value, scalefactor, round_values))
}

#[allow(clippy::too_many_arguments)]
fn read_solar_backwards_grid(
    product: &ProductSnapshot,
    decoder: &OfficialDecoder,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
    scalefactor: f32,
    round_values: bool,
) -> Result<Vec<f32>> {
    let native_times = native_times_for_variable(product, raw_variable);
    if let Some(mut frames) = read_sparse_solar_grid_series(
        product,
        decoder,
        raw_variable,
        raw_variable,
        &[time],
        latitudes,
        longitudes,
        scalefactor,
        round_values,
    )? {
        return Ok(frames.remove(0));
    }
    let Ok(index) = native_times.binary_search(&time) else {
        return Ok(vec![f32::NAN; latitudes.len() * longitudes.len()]);
    };
    let values = read_native_grid(
        product,
        decoder,
        raw_variable,
        native_times[index],
        latitudes,
        longitudes,
    )?;
    Ok(values
        .into_iter()
        .map(|value| maybe_round_to_scalefactor(value, scalefactor, round_values))
        .collect())
}

#[allow(clippy::too_many_arguments)]
fn read_linear_value(
    product: &ProductSnapshot,
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    scalefactor: f32,
    round_values: bool,
) -> Result<f32> {
    let native_times = native_times_for_variable(product, raw_variable);
    let Some((index, fraction)) = interpolation_index(&native_times, time) else {
        return Ok(f32::NAN);
    };
    let a = read_native_value(
        product,
        decoder,
        raw_variable,
        native_times[index],
        latitude,
        longitude,
    )?;
    let b = if index + 1 >= native_times.len() {
        a
    } else {
        read_native_value(
            product,
            decoder,
            raw_variable,
            native_times[index + 1],
            latitude,
            longitude,
        )?
    };
    Ok(maybe_round_to_scalefactor(
        a * (1.0 - fraction) + b * fraction,
        scalefactor,
        round_values,
    ))
}

#[allow(clippy::too_many_arguments)]
fn read_hermite_value(
    product: &ProductSnapshot,
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    scalefactor: f32,
    bounds: Option<(f32, f32)>,
    round_values: bool,
) -> Result<f32> {
    // Singapore's Swift CAMS producer materializes the official three-hour
    // Hermite interpolation onto a quantized hourly OM axis. Public raw
    // concentrations should use those materialized values, but derived air
    // quality indices require the unquantized reader value used by the
    // official bucket API. Reconstruct that value from the unchanged
    // three-hour source anchors already present in the same native file.
    let native_times = if !round_values && product.product == "cams_global" {
        cams_native_source_anchor_times(product, raw_variable)
            .unwrap_or_else(|| native_times_for_variable(product, raw_variable))
    } else {
        native_times_for_variable(product, raw_variable)
    };
    let Some((index, fraction)) = interpolation_index(&native_times, time) else {
        return Ok(f32::NAN);
    };
    let b = read_native_value(
        product,
        decoder,
        raw_variable,
        native_times[index],
        latitude,
        longitude,
    )?;
    if index + 1 >= native_times.len() {
        return Ok(maybe_round_to_scalefactor(b, scalefactor, round_values));
    }
    let stride_seconds = interpolation_stride_seconds(&native_times, index);
    let a_time = native_times[index] - Duration::seconds(stride_seconds);
    let a = match read_native_value_if_present(
        product,
        decoder,
        raw_variable,
        &native_times,
        a_time,
        latitude,
        longitude,
    )? {
        Some(value) if !value.is_nan() => value,
        _ => b,
    };
    let c = read_native_value(
        product,
        decoder,
        raw_variable,
        native_times[index + 1],
        latitude,
        longitude,
    )?;
    let c = if c.is_nan() { b } else { c };
    let d_time = native_times[index + 1] + Duration::seconds(stride_seconds);
    let d = match read_native_value_if_present(
        product,
        decoder,
        raw_variable,
        &native_times,
        d_time,
        latitude,
        longitude,
    )? {
        Some(value) if !value.is_nan() => value,
        Some(_) => b,
        None => missing_second_lookahead_value(product, b, c),
    };
    let coeff_a = -a / 2.0 + (3.0 * b) / 2.0 - (3.0 * c) / 2.0 + d / 2.0;
    let coeff_b = a - (5.0 * b) / 2.0 + 2.0 * c - d / 2.0;
    let coeff_c = -a / 2.0 + c / 2.0;
    let h = coeff_a * fraction * fraction * fraction
        + coeff_b * fraction * fraction
        + coeff_c * fraction
        + b;
    let mut scaled = maybe_round_to_scalefactor(h, scalefactor, round_values);
    if let Some((lower, upper)) = bounds {
        scaled = scaled.clamp(lower, upper);
    }
    Ok(scaled)
}

fn cams_native_source_anchor_times(
    product: &ProductSnapshot,
    raw_variable: &str,
) -> Option<Vec<DateTime<Utc>>> {
    // The official CAMS bucket keeps gases and dust on their source 3-hour
    // axis, while PM2.5, PM10 and aerosol optical depth are already published
    // hourly. Only reconstruct variables that are sparse in that reference
    // product; otherwise an authoritative hourly PM value could be replaced
    // by a different interpolation through its surrounding anchors.
    if !matches!(
        raw_variable,
        "carbon_monoxide" | "dust" | "nitrogen_dioxide" | "ozone" | "sulphur_dioxide"
    ) {
        return None;
    }
    let entries = product
        .entries
        .values()
        .filter(|entry| entry.variable == raw_variable)
        .collect::<Vec<_>>();
    if !entries.iter().any(|entry| {
        entry
            .native_grid
            .as_ref()
            .is_some_and(|grid| grid.dt_seconds == 3600)
    }) {
        return None;
    }
    let mut times = entries
        .into_iter()
        .filter(|entry| entry.forecast_hour.rem_euclid(3) == 0)
        .map(|entry| entry.valid_time_utc)
        .collect::<Vec<_>>();
    times.sort_unstable();
    times.dedup();
    (times.len() >= 2).then_some(times)
}

fn missing_second_lookahead_value(product: &ProductSnapshot, b: f32, c: f32) -> f32 {
    // The official CAMS greenhouse reader retains the unavailable next 3-hour slot as NaN,
    // so Hermite falls back to B. The global reader's native tail ends at C.
    if product.product == "cams_global_greenhouse_gases" {
        b
    } else {
        c
    }
}

fn interpolation_index(times: &[DateTime<Utc>], time: DateTime<Utc>) -> Option<(usize, f32)> {
    if times.is_empty() || time < times[0] || time > *times.last().expect("checked not empty") {
        return None;
    }
    match times.binary_search(&time) {
        Ok(index) => Some((index, 0.0)),
        Err(next_index) if next_index > 0 && next_index < times.len() => {
            let index = next_index - 1;
            let dt = (times[index + 1] - times[index]).num_seconds();
            if dt <= 0 {
                return None;
            }
            let offset = (time - times[index]).num_seconds();
            Some((index, offset as f32 / dt as f32))
        }
        _ => None,
    }
}

fn interpolation_stride_seconds(times: &[DateTime<Utc>], index: usize) -> i64 {
    if index + 1 < times.len() {
        return (times[index + 1] - times[index]).num_seconds();
    }
    if index > 0 {
        return (times[index] - times[index - 1]).num_seconds();
    }
    3600
}

#[allow(clippy::too_many_arguments)]
fn read_native_value_if_present(
    product: &ProductSnapshot,
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    native_times: &[DateTime<Utc>],
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<f32>> {
    if native_times.binary_search(&time).is_err() {
        return Ok(None);
    }
    read_native_value(product, decoder, raw_variable, time, latitude, longitude).map(Some)
}

fn read_native_value(
    product: &ProductSnapshot,
    decoder: Option<&OfficialDecoder>,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let key = EntryKey {
        variable: raw_variable.to_string(),
        valid_time_utc: time,
    };
    let entry = product
        .entries
        .get(&key)
        .with_context(|| format!("variable/time is not available: {} {}", raw_variable, time))?;
    read_entry_value(product, entry, decoder, latitude, longitude)
}

fn native_times_for_variable(product: &ProductSnapshot, raw_variable: &str) -> Vec<DateTime<Utc>> {
    let mut times: Vec<DateTime<Utc>> = product
        .entries
        .keys()
        .filter(|key| key.variable == raw_variable)
        .map(|key| key.valid_time_utc)
        .collect();
    times.sort();
    times.dedup();
    times
}

fn native_dt_seconds_at(times: &[DateTime<Utc>], index: usize) -> i64 {
    if times.len() < 2 {
        return 3600;
    }
    if index > 0 {
        return (times[index] - times[index - 1]).num_seconds();
    }
    (times[index + 1] - times[index]).num_seconds()
}

pub(crate) fn round_to_scalefactor(value: f32, scalefactor: f32) -> f32 {
    (value * scalefactor).round() / scalefactor
}

fn maybe_round_to_scalefactor(value: f32, scalefactor: f32, round_values: bool) -> f32 {
    if round_values {
        round_to_scalefactor(value, scalefactor)
    } else {
        value
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum InterpolationKind {
    Direct,
    SolarBackwardsAveraged {
        scalefactor: f32,
    },
    Linear {
        scalefactor: f32,
    },
    Hermite {
        scalefactor: f32,
        bounds: Option<(f32, f32)>,
    },
    Backwards {
        scalefactor: f32,
    },
    BackwardsSum {
        scalefactor: f32,
    },
}

pub(crate) fn interpolation_kind_for_variable(variable: &str) -> InterpolationKind {
    if is_cams_variable(variable) {
        return cams_interpolation_kind(variable);
    }
    if current_weather_model() == WeatherModel::EcmwfIfs025 {
        return ecmwf_interpolation_kind(variable);
    }
    if variable.ends_with("hPa") {
        return pressure_interpolation_kind(variable);
    }
    match variable {
        "precipitation" | "showers" | "snowfall_water_equivalent" => {
            InterpolationKind::BackwardsSum { scalefactor: 10.0 }
        }
        "shortwave_radiation" | "diffuse_radiation" => {
            InterpolationKind::SolarBackwardsAveraged { scalefactor: 1.0 }
        }
        "uv_index" | "uv_index_clear_sky" => {
            InterpolationKind::SolarBackwardsAveraged { scalefactor: 20.0 }
        }
        "categorical_freezing_rain" | "frozen_precipitation_percent" => {
            InterpolationKind::Backwards { scalefactor: 1.0 }
        }
        "visibility" => InterpolationKind::Linear { scalefactor: 0.05 },
        "freezing_level_height" => InterpolationKind::Linear { scalefactor: 0.1 },
        "snow_depth" => InterpolationKind::Linear { scalefactor: 100.0 },
        "temperature_2m"
        | "temperature_80m"
        | "temperature_100m"
        | "surface_temperature"
        | "soil_temperature_0_to_10cm"
        | "soil_temperature_10_to_40cm"
        | "soil_temperature_40_to_100cm"
        | "soil_temperature_100_to_200cm" => InterpolationKind::Hermite {
            scalefactor: 20.0,
            bounds: None,
        },
        "cloud_cover" | "cloud_cover_low" | "cloud_cover_mid" | "cloud_cover_high" => {
            InterpolationKind::Hermite {
                scalefactor: 1.0,
                bounds: Some((0.0, 100.0)),
            }
        }
        "precipitation_probability" => InterpolationKind::Hermite {
            scalefactor: 1.0,
            bounds: Some((0.0, 100.0)),
        },
        "relative_humidity_2m" => InterpolationKind::Hermite {
            scalefactor: 1.0,
            bounds: Some((0.0, 100.0)),
        },
        "pressure_msl" => InterpolationKind::Hermite {
            scalefactor: 10.0,
            bounds: None,
        },
        "wind_u_component_10m"
        | "wind_v_component_10m"
        | "wind_u_component_80m"
        | "wind_v_component_80m"
        | "wind_u_component_100m"
        | "wind_v_component_100m" => InterpolationKind::Hermite {
            scalefactor: 10.0,
            bounds: None,
        },
        "wind_gusts_10m" => InterpolationKind::Hermite {
            scalefactor: 10.0,
            bounds: Some((0.0, 10e9)),
        },
        "cape" => InterpolationKind::Hermite {
            scalefactor: 0.1,
            bounds: Some((0.0, 10e9)),
        },
        "lifted_index" => InterpolationKind::Hermite {
            scalefactor: 10.0,
            bounds: None,
        },
        "convective_inhibition" => InterpolationKind::Hermite {
            scalefactor: 1.0,
            bounds: Some((0.0, 10e9)),
        },
        "boundary_layer_height" => InterpolationKind::Hermite {
            scalefactor: 0.2,
            bounds: Some((0.0, 10e9)),
        },
        "sensible_heat_flux" | "latent_heat_flux" => InterpolationKind::Hermite {
            scalefactor: 0.144,
            bounds: None,
        },
        "soil_moisture_0_to_10cm"
        | "soil_moisture_10_to_40cm"
        | "soil_moisture_40_to_100cm"
        | "soil_moisture_100_to_200cm" => InterpolationKind::Hermite {
            scalefactor: 1000.0,
            bounds: None,
        },
        "total_column_integrated_water_vapour" => InterpolationKind::Hermite {
            scalefactor: 10.0,
            bounds: None,
        },
        "mass_density_8m" => InterpolationKind::Linear { scalefactor: 0.1 },
        _ => InterpolationKind::Direct,
    }
}

/// Port of `EcmwfVariable.interpolation` and `scalefactor` from the pinned
/// Open-Meteo ECMWF source baseline. Unlike GFS, every non-accumulated ECMWF
/// field defaults to Hermite interpolation.
fn ecmwf_interpolation_kind(variable: &str) -> InterpolationKind {
    let scalefactor = if let Some((name, level)) = pressure_variable_name_and_level(variable) {
        match name {
            "temperature" => interpolate_range(2.0, 10.0, fraction_in_range(300.0, 1000.0, level)),
            "wind_u_component" | "wind_v_component" => {
                interpolate_range(3.0, 10.0, fraction_in_range(500.0, 1000.0, level))
            }
            "geopotential_height" => {
                interpolate_range(0.05, 1.0, fraction_in_range(0.0, 500.0, level))
            }
            "relative_humidity" => {
                interpolate_range(0.2, 1.0, fraction_in_range(0.0, 800.0, level))
            }
            "vertical_velocity" => {
                interpolate_range(20.0, 100.0, fraction_in_range(0.0, 500.0, level))
            }
            _ => 1.0,
        }
    } else {
        match variable {
            "precipitation_type" => 1.0,
            "precipitation" | "showers" | "snowfall_water_equivalent" | "runoff" => 10.0,
            "snow_depth" => 100.0,
            "snow_depth_water_equivalent" => 1.0,
            "temperature_2m"
            | "temperature_2m_min"
            | "temperature_2m_max"
            | "surface_temperature"
            | "soil_temperature_0_to_10cm"
            | "soil_temperature_0_to_7cm"
            | "soil_temperature_7_to_28cm"
            | "soil_temperature_28_to_100cm"
            | "soil_temperature_100_to_255cm" => 20.0,
            "pressure_msl"
            | "total_column_integrated_water_vapour"
            | "wind_u_component_10m"
            | "wind_v_component_10m"
            | "wind_u_component_100m"
            | "wind_v_component_100m"
            | "wind_gusts_10m" => 10.0,
            "cloud_cover"
            | "cloud_cover_low"
            | "cloud_cover_mid"
            | "cloud_cover_high"
            | "relative_humidity_2m"
            | "shortwave_radiation" => 1.0,
            "soil_moisture_0_to_7cm"
            | "soil_moisture_7_to_28cm"
            | "soil_moisture_28_to_100cm"
            | "soil_moisture_100_to_255cm" => 1000.0,
            "cape" => 0.1,
            _ => 1.0,
        }
    };

    match variable {
        "precipitation" | "showers" | "snowfall_water_equivalent" | "runoff" => {
            InterpolationKind::BackwardsSum { scalefactor }
        }
        "shortwave_radiation" => InterpolationKind::SolarBackwardsAveraged { scalefactor },
        "temperature_2m_min" | "temperature_2m_max" => InterpolationKind::Backwards { scalefactor },
        "cloud_cover"
        | "cloud_cover_low"
        | "cloud_cover_mid"
        | "cloud_cover_high"
        | "relative_humidity_2m"
        | "precipitation_probability" => InterpolationKind::Hermite {
            scalefactor,
            bounds: Some((0.0, 100.0)),
        },
        "wind_gusts_10m" => InterpolationKind::Hermite {
            scalefactor,
            bounds: Some((0.0, 10e9)),
        },
        _ if variable.starts_with("relative_humidity_") && variable.ends_with("hPa") => {
            InterpolationKind::Hermite {
                scalefactor,
                bounds: Some((0.0, 100.0)),
            }
        }
        _ => InterpolationKind::Hermite {
            scalefactor,
            bounds: None,
        },
    }
}

fn cams_interpolation_kind(variable: &str) -> InterpolationKind {
    let scalefactor = match variable {
        "pm10" | "pm2_5" | "nitrogen_dioxide" | "sulphur_dioxide" => 10.0,
        "aerosol_optical_depth" => 100.0,
        "dust" | "carbon_monoxide" | "ozone" => 1.0,
        _ => 1.0,
    };
    InterpolationKind::Hermite {
        scalefactor,
        bounds: Some((0.0, f32::INFINITY)),
    }
}

fn pressure_interpolation_kind(variable: &str) -> InterpolationKind {
    let Some((name, level)) = pressure_variable_name_and_level(variable) else {
        return InterpolationKind::Direct;
    };
    match name {
        "temperature" => InterpolationKind::Hermite {
            scalefactor: interpolate_range(2.0, 10.0, fraction_in_range(300.0, 1000.0, level)),
            bounds: None,
        },
        "wind_u_component" | "wind_v_component" => InterpolationKind::Hermite {
            scalefactor: interpolate_range(3.0, 10.0, fraction_in_range(500.0, 1000.0, level)),
            bounds: None,
        },
        "geopotential_height" => InterpolationKind::Linear {
            scalefactor: interpolate_range(0.05, 1.0, fraction_in_range(0.0, 500.0, level)),
        },
        "cloud_cover" => InterpolationKind::Linear {
            scalefactor: interpolate_range(0.2, 1.0, fraction_in_range(0.0, 800.0, level)),
        },
        "relative_humidity" => InterpolationKind::Hermite {
            scalefactor: interpolate_range(0.2, 1.0, fraction_in_range(0.0, 800.0, level)),
            bounds: Some((0.0, 100.0)),
        },
        "vertical_velocity" => InterpolationKind::Hermite {
            scalefactor: interpolate_range(20.0, 100.0, fraction_in_range(0.0, 500.0, level)),
            bounds: None,
        },
        _ => InterpolationKind::Direct,
    }
}

fn pressure_variable_name_and_level(variable: &str) -> Option<(&str, f32)> {
    let (name, level_text) = variable.rsplit_once('_')?;
    let level = level_text.strip_suffix("hPa")?.parse::<f32>().ok()?;
    Some((name, level))
}

fn fraction_in_range(lower: f32, upper: f32, value: f32) -> f32 {
    ((value.clamp(lower, upper) - lower) / (upper - lower)).clamp(0.0, 1.0)
}

fn interpolate_range(lower: f32, upper: f32, fraction: f32) -> f32 {
    (lower + (upper - lower) * fraction).clamp(lower, upper)
}

fn read_optional_direct(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<f32>> {
    match read_direct(snapshot, decoder, variable, time, latitude, longitude) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Ok(None),
    }
}

fn product_for_variable(
    snapshot: &OmDataSnapshot,
    variable: &str,
) -> Result<(&'static str, String)> {
    let candidates: &[&str] = if variable == "carbon_monoxide" {
        &["cams_global_greenhouse_gases", "cams_global"]
    } else if is_cams_variable(variable) {
        &["cams_global"]
    } else if variable == "precipitation_probability" {
        match current_weather_model() {
            WeatherModel::Gfs => &["ncep_gefs025", "ncep_gefs05"],
            WeatherModel::EcmwfIfs025 => &["ecmwf_ifs025_ensemble"],
        }
    } else if current_weather_model() == WeatherModel::EcmwfIfs025 {
        &["ecmwf_ifs025"]
    } else if is_gfs025_variable(variable) {
        &["gfs025"]
    } else if is_pressure_variable(variable) {
        &["gfs_pressure_profile"]
    } else {
        &["gfs013_surface"]
    };
    let raw_variable = if current_weather_model() == WeatherModel::EcmwfIfs025 {
        match variable {
            // These compatibility aliases are the same aliases exposed by
            // Open-Meteo's official EcmwfReader.
            "soil_temperature_0_to_10cm" => "soil_temperature_0_to_7cm",
            "soil_moisture_0_to_10cm" => "soil_moisture_0_to_7cm",
            "skin_temperature" | "soil_temperature_0cm" => "surface_temperature",
            _ => variable,
        }
    } else {
        variable
    };
    for product in candidates {
        if snapshot.product(product).is_some() {
            return Ok((product, raw_variable.to_string()));
        }
    }
    bail!(
        "product is not available for variable {}: {}",
        variable,
        candidates.join(", ")
    );
}

fn seed_variable_for_times(variable: &str) -> String {
    if let Some((name, level_text)) = variable.rsplit_once('_') {
        if level_text
            .strip_suffix("hPa")
            .and_then(|value| value.parse::<u16>().ok())
            .is_some()
        {
            let prefix = match name {
                "wind_speed" | "windspeed" | "wind_direction" | "winddirection" => {
                    Some("wind_u_component")
                }
                "dew_point" | "dewpoint" => Some("temperature"),
                "cloudcover" | "cloud_cover" => {
                    Some(if current_weather_model() == WeatherModel::EcmwfIfs025 {
                        "relative_humidity"
                    } else {
                        "cloud_cover"
                    })
                }
                "relativehumidity" => Some("relative_humidity"),
                _ => None,
            };
            if let Some(prefix) = prefix {
                return format!("{prefix}_{level_text}");
            }
        }
    }
    let seed = match variable {
        "dew_point_2m"
        | "dewpoint_2m"
        | "wet_bulb_temperature_2m"
        | "apparent_temperature"
        | "vapour_pressure_deficit"
        | "vapor_pressure_deficit"
        | "et0_fao_evapotranspiration"
        | "growing_degree_days_base_0_limit_50"
        | "is_day" => "temperature_2m",
        "skin_temperature" | "soil_temperature_0cm" => "surface_temperature",
        "soil_temperature_0_to_100cm" => "soil_temperature_0_to_7cm",
        "soil_moisture_0_to_100cm" => "soil_moisture_0_to_7cm",
        "evapotranspiration" => "latent_heat_flux",
        "direct_radiation"
        | "shortwave_radiation_instant"
        | "direct_radiation_instant"
        | "direct_normal_irradiance"
        | "direct_normal_irradiance_instant"
        | "global_tilted_irradiance"
        | "global_tilted_irradiance_instant"
        | "sunshine_duration" => "shortwave_radiation",
        "diffuse_radiation_instant" => "diffuse_radiation",
        "surface_pressure" => "temperature_2m",
        "weather_code" | "weathercode" => "cloud_cover",
        "rain" => "precipitation",
        "snowfall" => "snowfall_water_equivalent",
        "wind_speed_10m" | "windspeed_10m" | "wind_direction_10m" | "winddirection_10m" => {
            "wind_u_component_10m"
        }
        "wind_speed_80m" | "windspeed_80m" | "wind_direction_80m" | "winddirection_80m" => {
            "wind_u_component_80m"
        }
        "wind_speed_100m"
        | "windspeed_100m"
        | "wind_direction_100m"
        | "winddirection_100m"
        | "wind_speed_120m"
        | "windspeed_120m"
        | "wind_direction_120m"
        | "winddirection_120m" => "wind_u_component_100m",
        "temperature_120m" => "temperature_100m",
        "precip_phase" | "thunderstorm_code" => "cloud_cover",
        "european_aqi" | "european_aqi_pm2_5" | "european_aqi_pm10" | "us_aqi" | "us_aqi_pm2_5"
        | "us_aqi_pm10" | "chinese_aqi" | "chinese_aqi_pm2_5" | "chinese_aqi_pm10" => "pm2_5",
        "european_aqi_no2"
        | "european_aqi_nitrogen_dioxide"
        | "us_aqi_no2"
        | "us_aqi_nitrogen_dioxide"
        | "chinese_aqi_no2"
        | "chinese_aqi_nitrogen_dioxide" => "nitrogen_dioxide",
        "european_aqi_o3" | "european_aqi_ozone" | "us_aqi_o3" | "us_aqi_ozone"
        | "chinese_aqi_o3" | "chinese_aqi_ozone" => "ozone",
        "european_aqi_so2"
        | "european_aqi_sulphur_dioxide"
        | "us_aqi_so2"
        | "us_aqi_sulphur_dioxide"
        | "chinese_aqi_so2"
        | "chinese_aqi_sulphur_dioxide" => "sulphur_dioxide",
        "us_aqi_co"
        | "us_aqi_carbon_monoxide"
        | "chinese_aqi_co"
        | "chinese_aqi_carbon_monoxide" => "carbon_monoxide",
        _ => variable,
    };
    seed.to_string()
}

fn is_cams_variable(variable: &str) -> bool {
    matches!(
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
}

fn is_air_quality_variable(variable: &str) -> bool {
    is_cams_variable(&seed_variable_for_times(variable))
        || variable.starts_with("european_aqi")
        || variable.starts_with("us_aqi")
        || variable.starts_with("chinese_aqi")
}

fn is_gfs025_variable(variable: &str) -> bool {
    matches!(
        variable,
        "pressure_msl"
            | "visibility"
            | "wind_gusts_10m"
            | "cape"
            | "lifted_index"
            | "categorical_freezing_rain"
            | "freezing_level_height"
            | "convective_inhibition"
            | "temperature_80m"
            | "temperature_100m"
            | "wind_u_component_80m"
            | "wind_v_component_80m"
            | "wind_u_component_100m"
            | "wind_v_component_100m"
    )
}

fn is_pressure_variable(variable: &str) -> bool {
    variable.ends_with("hPa")
}

fn is_elevation_correctable(variable: &str) -> bool {
    match current_weather_model() {
        WeatherModel::EcmwfIfs025 => matches!(
            variable,
            "temperature_2m"
                | "temperature_2m_min"
                | "temperature_2m_max"
                | "soil_temperature_0_to_10cm"
                | "soil_temperature_0_to_7cm"
                | "soil_temperature_7_to_28cm"
                | "soil_temperature_28_to_100cm"
                | "soil_temperature_100_to_255cm"
        ),
        WeatherModel::Gfs => matches!(
            variable,
            "temperature_2m"
                | "temperature_80m"
                | "temperature_100m"
                | "surface_temperature"
                | "soil_temperature_0_to_10cm"
                | "soil_temperature_10_to_40cm"
                | "soil_temperature_40_to_100cm"
                | "soil_temperature_100_to_200cm"
        ),
    }
}

fn apply_elevation_correction(product: &str, variable: &str, value: f32) -> f32 {
    if !is_elevation_correctable(variable) || !value.is_finite() {
        return value;
    }
    let Some(sampling) = current_product_sampling(product) else {
        return value;
    };
    if !sampling.model_elevation.is_finite() || !sampling.target_elevation.is_finite() {
        return value;
    }
    value + (sampling.model_elevation - sampling.target_elevation) * 0.0065
}

fn read_entry_grid(
    product: &ProductSnapshot,
    entry: &BundleEntry,
    decoder: &OfficialDecoder,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<f32>> {
    let y_indices = latitudes
        .iter()
        .map(|latitude| {
            grid_index_for_lat_lon(
                &entry.array,
                entry.native_grid.as_ref(),
                *latitude,
                longitudes[0],
            )
            .map(|v| v.0)
        })
        .collect::<Result<Vec<_>>>()?;
    let x_indices = longitudes
        .iter()
        .map(|longitude| {
            grid_index_for_lat_lon(
                &entry.array,
                entry.native_grid.as_ref(),
                latitudes[0],
                *longitude,
            )
            .map(|v| v.1)
        })
        .collect::<Result<Vec<_>>>()?;
    let y0 = *y_indices
        .iter()
        .min()
        .context("regional grid has no rows")?;
    let y1 = *y_indices
        .iter()
        .max()
        .context("regional grid has no rows")?;
    let x0 = *x_indices
        .iter()
        .min()
        .context("regional grid has no columns")?;
    let x1 = *x_indices
        .iter()
        .max()
        .context("regional grid has no columns")?;
    ensure_in_selection(entry, y0, x0)?;
    ensure_in_selection(entry, y1, x1)?;
    if entry.native_time_index.is_none() && entry.array.compression == 4 {
        let mut values = Vec::with_capacity(latitudes.len() * longitudes.len());
        for y in y_indices {
            for x in &x_indices {
                values.push(read_uncompressed_point(product, entry, y, *x)?);
            }
        }
        return Ok(values);
    }
    let lut_size = entry
        .array
        .lut_size
        .context("array metadata missing lut_size")?;
    let lut_offset = entry
        .array
        .lut_offset
        .context("array metadata missing lut_offset")?;
    let is_native = entry.native_time_index.is_some();
    if (!is_native && entry.array.chunks.len() != 2)
        || (is_native && entry.array.chunks.len() != 3)
        || entry.selection_ranges.len() != 2
    {
        bail!("OM regional decoding dimensions do not match entry type");
    }
    let height = y1 - y0 + 1;
    let width = x1 - x0 + 1;
    let metadata = build_v3_array_metadata_blob(
        entry.variable_path.as_deref().unwrap_or(&entry.variable),
        entry.array.data_type,
        entry.array.compression,
        &entry.array.dimensions,
        &entry.array.chunks,
        lut_size,
        lut_offset,
        entry.array.scale_factor.unwrap_or(1.0),
        entry.array.add_offset.unwrap_or(0.0),
    );
    let reader = entry_range_reader(product, entry)?;
    let (offset, count) = if let Some(time_index) = entry.native_time_index {
        (vec![y0, x0, time_index], vec![height, width, 1])
    } else {
        (vec![y0, x0], vec![height, width])
    };
    let rectangle = decoder.decode_grid(&metadata, &reader, &offset, &count)?;
    let mut values = Vec::with_capacity(latitudes.len() * longitudes.len());
    for y in y_indices {
        for x in &x_indices {
            let index = ((y - y0) * width + (*x - x0)) as usize;
            values.push(
                rectangle
                    .get(index)
                    .copied()
                    .context("decoded OM rectangle does not contain requested grid point")?,
            );
        }
    }
    Ok(values)
}

fn read_entry_value(
    product: &ProductSnapshot,
    entry: &BundleEntry,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let (latitude, longitude) = current_product_sampling(&product.product)
        .map(|sampling| (sampling.latitude, sampling.longitude))
        .unwrap_or((latitude, longitude));
    let (y, x) = grid_index_for_lat_lon(
        &entry.array,
        entry.native_grid.as_ref(),
        latitude,
        longitude,
    )?;
    ensure_in_selection(entry, y, x)?;
    if let Some(time_index) = entry.native_time_index {
        let decoder =
            decoder.context("official OM decoder library is required for native runtime files")?;
        let metadata = build_v3_array_metadata_blob(
            entry.variable_path.as_deref().unwrap_or(&entry.variable),
            entry.array.data_type,
            entry.array.compression,
            &entry.array.dimensions,
            &entry.array.chunks,
            entry
                .array
                .lut_size
                .context("array metadata missing lut_size")?,
            entry
                .array
                .lut_offset
                .context("array metadata missing lut_offset")?,
            entry.array.scale_factor.unwrap_or(1.0),
            entry.array.add_offset.unwrap_or(0.0),
        );
        let reader = entry_range_reader(product, entry)?;
        return decoder.decode_point(&metadata, &reader, &[y, x, time_index]);
    }
    if entry.array.compression == 4 {
        return read_uncompressed_point(product, entry, y, x);
    }
    let decoder = decoder.ok_or_else(|| {
        anyhow!(
            "official OM decoder library is required for compression {}; set OM_OMFILE_LIB",
            entry.array.compression
        )
    })?;
    let lut_size = entry
        .array
        .lut_size
        .context("array metadata missing lut_size")?;
    let lut_offset = entry
        .array
        .lut_offset
        .context("array metadata missing lut_offset")?;
    if entry.array.chunks.len() != 2 || entry.selection_ranges.len() != 2 {
        bail!("only 2D OM chunk caching is supported");
    }
    let chunk_y = entry.array.chunks[0];
    let chunk_x = entry.array.chunks[1];
    let tile_y = chunk_y.saturating_mul(4);
    let tile_x = chunk_x.saturating_mul(4);
    let y_range = entry.selection_ranges[0];
    let x_range = entry.selection_ranges[1];
    let y0 = (y / tile_y * tile_y).max(y_range[0]);
    let x0 = (x / tile_x * tile_x).max(x_range[0]);
    let y1 = ((y / tile_y + 1) * tile_y).min(y_range[1]);
    let x1 = ((x / tile_x + 1) * tile_x).min(x_range[1]);
    let height = y1 - y0;
    let width = x1 - x0;
    let cache_handle = entry_file_handle(product, entry)?;
    let key = TileCacheKey {
        bundle: Arc::as_ptr(&cache_handle) as usize,
        entry_offset: entry.bundle_offset,
        y0,
        x0,
        height,
        width,
    };
    let cached = DECODED_TILE_CACHE.with(|cache| cache.borrow().get(&key).cloned());
    let tile = if let Some(cached) = cached {
        cached
    } else {
        let metadata = build_v3_array_metadata_blob(
            entry.variable_path.as_deref().unwrap_or(&entry.variable),
            entry.array.data_type,
            entry.array.compression,
            &entry.array.dimensions,
            &entry.array.chunks,
            lut_size,
            lut_offset,
            entry.array.scale_factor.unwrap_or(1.0),
            entry.array.add_offset.unwrap_or(0.0),
        );
        let reader = entry_range_reader(product, entry)?;
        let decoded =
            Arc::new(decoder.decode_grid(&metadata, &reader, &[y0, x0], &[height, width])?);
        DECODED_TILE_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() >= 128 {
                cache.clear();
            }
            cache.insert(key, decoded.clone());
        });
        decoded
    };
    let index = ((y - y0) * width + (x - x0)) as usize;
    tile.get(index)
        .copied()
        .context("decoded OM tile does not contain requested point")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TileCacheKey {
    bundle: usize,
    entry_offset: u64,
    y0: u64,
    x0: u64,
    height: u64,
    width: u64,
}

thread_local! {
    static DECODED_TILE_CACHE: RefCell<HashMap<TileCacheKey, Arc<Vec<f32>>>> =
        RefCell::new(HashMap::new());
}

fn grid_index_for_lat_lon(
    array: &ArrayMetadata,
    native_grid: Option<&crate::manifest::NativeGridMetadata>,
    latitude: f64,
    longitude: f64,
) -> Result<(u64, u64)> {
    if let Some(grid) = native_grid {
        if !matches!(array.dimensions.len(), 2 | 3)
            || array.dimensions[0] != grid.ny
            || array.dimensions[1] != grid.nx
        {
            bail!("native OM array dimensions do not match grid contract");
        }
        // A regional OM bundle keeps only a rectangular subset, but its cell
        // selection must remain bit-for-bit identical to the original global
        // Open-Meteo grid.  Calculating from `lon_min` is not equivalent at a
        // half-cell boundary: for example `(75.0 - 69.2) / 0.4` is represented
        // just below 14.5, whereas the full CAMS grid selects global cell 638
        // from `(75.0 + 180.0) / 0.4`.  Preserve the full-grid f32 arithmetic
        // whenever the producer recorded its origin indices.
        if let (Some(full_nx), Some(full_ny), Some(x0), Some(y0)) =
            (grid.full_nx, grid.full_ny, grid.x0, grid.y0)
        {
            let dx = 360.0_f32 / full_nx as f32;
            let mut lon = longitude as f32;
            while lon < -180.0 {
                lon += 360.0;
            }
            while lon >= 180.0 {
                lon -= 360.0;
            }
            let global_x = ((lon + 180.0_f32) / dx).round() as i64;
            let (global_lat_min, dy) = if full_ny == 1536 {
                let dy = 0.11714935_f32;
                (-dy * (full_ny as f32 - 1.0) / 2.0, dy)
            } else {
                (-90.0_f32, 180.0_f32 / (full_ny as f32 - 1.0))
            };
            let global_y = (((latitude as f32) - global_lat_min) / dy).round() as i64;
            let x = global_x - x0 as i64;
            let y = global_y - y0 as i64;
            if y < 0 || y >= grid.ny as i64 || x < 0 || x >= grid.nx as i64 {
                bail!("point is outside native regional grid");
            }
            return Ok((y as u64, x as u64));
        }
        let x = ((longitude - grid.lon_min) / grid.dx).round() as i64;
        let y = ((latitude - grid.lat_min) / grid.dy).round() as i64;
        if y < 0 || y >= grid.ny as i64 || x < 0 || x >= grid.nx as i64 {
            bail!("point is outside native regional grid");
        }
        return Ok((y as u64, x as u64));
    }
    if !matches!(array.dimensions.len(), 2 | 3) {
        bail!("only 2D or 3D OM entries are supported by the point API");
    }
    let ny = array.dimensions[0] as f32;
    let nx = array.dimensions[1] as f32;
    let dx = 360.0_f32 / nx;
    let (lat_min, dy) = if array.dimensions[0] == 1536 {
        let dy = 0.11714935_f32;
        (-dy * (ny - 1.0) / 2.0, dy)
    } else {
        (-90.0_f32, 180.0_f32 / (ny - 1.0))
    };
    let mut lon = longitude as f32;
    while lon < -180.0 {
        lon += 360.0;
    }
    while lon >= 180.0 {
        lon -= 360.0;
    }
    let x = ((lon + 180.0_f32) / dx).round() as i64;
    let y = (((latitude as f32) - lat_min) / dy).round() as i64;
    if y < 0 || y >= array.dimensions[0] as i64 || x < 0 || x >= array.dimensions[1] as i64 {
        bail!("point is outside grid");
    }
    Ok((y as u64, x as u64))
}

fn grid_latitude_for_index(
    array: &ArrayMetadata,
    native_grid: Option<&crate::manifest::NativeGridMetadata>,
    y: u64,
) -> Result<f32> {
    if let Some(grid) = native_grid {
        if y >= grid.ny {
            bail!("invalid native latitude grid index");
        }
        if grid.full_ny == Some(1536) {
            let dy = grid.dy as f32;
            let global_y = grid.y0.unwrap_or(0) + y;
            let global_lat_min = -dy * (1536.0_f32 - 1.0) / 2.0;
            return Ok(global_lat_min + global_y as f32 * dy);
        }
        if let (Some(_full_ny), Some(y0)) = (grid.full_ny, grid.y0) {
            // Preserve Open-Meteo's global-grid f32 arithmetic after a native
            // regional crop. Computing from the regional f64 origin changes
            // the shortest JSON representation (for example 16.800003 becomes
            // 16.8) even though it selects the same cell.
            let dy = grid.dy as f32;
            let global_lat_min = (grid.lat_min - y0 as f64 * grid.dy) as f32;
            return Ok(global_lat_min + (y0 + y) as f32 * dy);
        }
        return Ok((grid.lat_min + y as f64 * grid.dy) as f32);
    }
    if !matches!(array.dimensions.len(), 2 | 3) || y >= array.dimensions[0] {
        bail!("invalid latitude grid index");
    }
    let ny = array.dimensions[0] as f32;
    let (lat_min, dy) = if array.dimensions[0] == 1536 {
        let dy = 0.11714935_f32;
        (-dy * (ny - 1.0) / 2.0, dy)
    } else {
        (-90.0_f32, 180.0_f32 / (ny - 1.0))
    };
    Ok(lat_min + y as f32 * dy)
}

fn grid_longitude_for_index(
    array: &ArrayMetadata,
    native_grid: Option<&crate::manifest::NativeGridMetadata>,
    x: u64,
) -> Result<f32> {
    if let Some(grid) = native_grid {
        if x >= grid.nx {
            bail!("invalid native longitude grid index");
        }
        if let (Some(full_nx), Some(x0)) = (grid.full_nx, grid.x0) {
            let dx = 360.0_f32 / full_nx as f32;
            return Ok(-180.0_f32 + (x0 + x) as f32 * dx);
        }
        return Ok((grid.lon_min + x as f64 * grid.dx) as f32);
    }
    if !matches!(array.dimensions.len(), 2 | 3) || x >= array.dimensions[1] {
        bail!("invalid longitude grid index");
    }
    Ok(-180.0_f32 + x as f32 * (360.0_f32 / array.dimensions[1] as f32))
}

#[derive(Debug, Clone, Copy)]
struct StaticElevationSpec {
    relative_path: &'static str,
    dimensions: &'static [u64],
    chunks: &'static [u64],
    lut_offset: u64,
    lut_size: u64,
    file_size: u64,
}

const GFS013_STATIC_SPEC: StaticElevationSpec = StaticElevationSpec {
    relative_path: GFS013_STATIC_ELEVATION_PATH,
    dimensions: GFS013_STATIC_DIMENSIONS,
    chunks: GFS013_STATIC_CHUNKS,
    lut_offset: GFS013_STATIC_LUT_OFFSET,
    lut_size: GFS013_STATIC_LUT_SIZE,
    file_size: GFS013_STATIC_FILE_SIZE,
};

const GFS025_STATIC_SPEC: StaticElevationSpec = StaticElevationSpec {
    relative_path: GFS025_STATIC_ELEVATION_PATH,
    dimensions: GFS025_STATIC_DIMENSIONS,
    chunks: GFS025_STATIC_CHUNKS,
    lut_offset: GFS025_STATIC_LUT_OFFSET,
    lut_size: GFS025_STATIC_LUT_SIZE,
    file_size: GFS025_STATIC_FILE_SIZE,
};

const GEFS025_STATIC_SPEC: StaticElevationSpec = StaticElevationSpec {
    relative_path: GEFS025_STATIC_ELEVATION_PATH,
    dimensions: GEFS025_STATIC_DIMENSIONS,
    chunks: GEFS025_STATIC_CHUNKS,
    lut_offset: GEFS025_STATIC_LUT_OFFSET,
    lut_size: GEFS025_STATIC_LUT_SIZE,
    file_size: GEFS025_STATIC_FILE_SIZE,
};

const GEFS05_STATIC_SPEC: StaticElevationSpec = StaticElevationSpec {
    relative_path: GEFS05_STATIC_ELEVATION_PATH,
    dimensions: GEFS05_STATIC_DIMENSIONS,
    chunks: GEFS05_STATIC_CHUNKS,
    lut_offset: GEFS05_STATIC_LUT_OFFSET,
    lut_size: GEFS05_STATIC_LUT_SIZE,
    file_size: GEFS05_STATIC_FILE_SIZE,
};

const ECMWF025_STATIC_SPEC: StaticElevationSpec = StaticElevationSpec {
    relative_path: ECMWF025_STATIC_ELEVATION_PATH,
    dimensions: ECMWF025_STATIC_DIMENSIONS,
    chunks: ECMWF025_STATIC_CHUNKS,
    lut_offset: ECMWF025_STATIC_LUT_OFFSET,
    lut_size: ECMWF025_STATIC_LUT_SIZE,
    file_size: ECMWF025_STATIC_FILE_SIZE,
};

const ECMWF025_ENSEMBLE_STATIC_SPEC: StaticElevationSpec = StaticElevationSpec {
    relative_path: ECMWF025_ENSEMBLE_STATIC_ELEVATION_PATH,
    dimensions: ECMWF025_ENSEMBLE_STATIC_DIMENSIONS,
    chunks: ECMWF025_ENSEMBLE_STATIC_CHUNKS,
    lut_offset: ECMWF025_ENSEMBLE_STATIC_LUT_OFFSET,
    lut_size: ECMWF025_ENSEMBLE_STATIC_LUT_SIZE,
    file_size: ECMWF025_ENSEMBLE_STATIC_FILE_SIZE,
};

fn resolve_request_sampling(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
    requested_elevation: Option<f32>,
    mode: GridSelectionMode,
) -> Result<RequestSampling> {
    let decoder =
        decoder.context("official OM decoder library is required for DEM and grid selection")?;
    // Copernicus DEM is a shared asset, not part of the selected weather-model
    // release. `read_dem90` gives OM_DEM_ROOT precedence; these legacy GFS
    // product roots are only fallbacks for older layouts that colocated DEM.
    let dem_root = snapshot
        .product("gfs013_surface")
        .or_else(|| snapshot.product("gfs025"))
        .or_else(|| snapshot.product("gfs_pressure_profile"))
        .map(|product| product.product_root.clone())
        .unwrap_or_else(|| snapshot.data_root.clone());
    let target = match requested_elevation {
        Some(value) => value,
        None => read_dem90(decoder, &dem_root, latitude, longitude)?,
    };
    let gfs013 = if current_weather_model() == WeatherModel::Gfs
        && snapshot.product("gfs013_surface").is_some()
    {
        Some(resolve_model_sampling(
            snapshot,
            decoder,
            "gfs013_surface",
            GFS013_STATIC_SPEC,
            latitude,
            longitude,
            target,
            mode,
        )?)
    } else {
        None
    };
    let gfs025 = if current_weather_model() == WeatherModel::Gfs
        && (snapshot.product("gfs025").is_some()
            || snapshot.product("gfs_pressure_profile").is_some())
    {
        Some(resolve_model_sampling(
            snapshot,
            decoder,
            if snapshot.product("gfs025").is_some() {
                "gfs025"
            } else {
                "gfs_pressure_profile"
            },
            GFS025_STATIC_SPEC,
            latitude,
            longitude,
            target,
            mode,
        )?)
    } else {
        None
    };
    let gefs025 = if current_weather_model() == WeatherModel::Gfs
        && snapshot.product("ncep_gefs025").is_some()
    {
        Some(resolve_model_sampling(
            snapshot,
            decoder,
            "ncep_gefs025",
            GEFS025_STATIC_SPEC,
            latitude,
            longitude,
            target,
            mode,
        )?)
    } else {
        None
    };
    let gefs05 = if current_weather_model() == WeatherModel::Gfs
        && snapshot.product("ncep_gefs05").is_some()
    {
        Some(resolve_model_sampling(
            snapshot,
            decoder,
            "ncep_gefs05",
            GEFS05_STATIC_SPEC,
            latitude,
            longitude,
            target,
            mode,
        )?)
    } else {
        None
    };
    let ecmwf025 = if current_weather_model() == WeatherModel::EcmwfIfs025
        && snapshot.product("ecmwf_ifs025").is_some()
    {
        Some(resolve_model_sampling(
            snapshot,
            decoder,
            "ecmwf_ifs025",
            ECMWF025_STATIC_SPEC,
            latitude,
            longitude,
            target,
            mode,
        )?)
    } else {
        None
    };
    let ecmwf025_ensemble = if current_weather_model() == WeatherModel::EcmwfIfs025
        && snapshot.product("ecmwf_ifs025_ensemble").is_some()
    {
        Some(resolve_model_sampling(
            snapshot,
            decoder,
            "ecmwf_ifs025_ensemble",
            ECMWF025_ENSEMBLE_STATIC_SPEC,
            latitude,
            longitude,
            target,
            mode,
        )?)
    } else {
        None
    };
    let response_elevation = if target.is_nan() {
        gfs013
            .or(gfs025)
            .or(gefs025)
            .or(gefs05)
            .or(ecmwf025)
            .or(ecmwf025_ensemble)
            .map(|sampling| sampling.model_elevation)
            .unwrap_or(f32::NAN)
    } else {
        target
    };
    Ok(RequestSampling {
        gfs013,
        gfs025,
        gefs025,
        gefs05,
        ecmwf025,
        ecmwf025_ensemble,
        response_elevation,
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_model_sampling(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    product_name: &str,
    spec: StaticElevationSpec,
    latitude: f64,
    longitude: f64,
    target_elevation: f32,
    mode: GridSelectionMode,
) -> Result<ModelSampling> {
    let product = snapshot
        .product(product_name)
        .with_context(|| format!("missing product {product_name}"))?;
    let entry = product
        .entries
        .values()
        .find(|entry| entry.variable == "temperature_2m")
        .or_else(|| product.entries.values().next())
        .with_context(|| format!("{product_name} has no grid entries"))?;
    if entry.native_grid.is_none()
        && !static_elevation_spatial_dimensions_match(&entry.array.dimensions, spec.dimensions)
    {
        bail!("static elevation grid does not match {product_name} dimensions");
    }
    let (center_y, center_x) = grid_index_for_lat_lon(
        &entry.array,
        entry.native_grid.as_ref(),
        latitude,
        longitude,
    )?;
    let ny = entry.array.dimensions[0];
    let nx = entry.array.dimensions[1];
    let y0 = center_y.saturating_sub(1);
    let x0 = center_x.saturating_sub(1);
    let y1 = (center_y + 1).min(ny - 1);
    let x1 = (center_x + 1).min(nx - 1);
    let width = x1 - x0 + 1;
    let latitudes = (y0..=y1)
        .map(|y| {
            grid_latitude_for_index(&entry.array, entry.native_grid.as_ref(), y).map(f64::from)
        })
        .collect::<Result<Vec<_>>>()?;
    let longitudes = (x0..=x1)
        .map(|x| {
            grid_longitude_for_index(&entry.array, entry.native_grid.as_ref(), x).map(f64::from)
        })
        .collect::<Result<Vec<_>>>()?;
    let elevations = if let Some(static_entry) = product.static_entries.get("surface_elevation") {
        read_entry_grid(&product, static_entry, decoder, &latitudes, &longitudes)?
    } else {
        // Native cycle files are regional crops, while the fixed HSURF file is
        // the official global grid. Regional y/x indices therefore cannot be
        // used directly against the global file; remap the regional cell
        // centres to the corresponding global indices first.
        read_static_elevation_grid_for_coordinates(
            snapshot,
            decoder,
            spec,
            &latitudes,
            &longitudes,
        )?
    };
    let center_index = (elevations.len() / 2).min(elevations.len().saturating_sub(1));
    let center_elevation = elevations[center_index];
    let center_position = center_y * nx + center_x;
    let mut selected_position = center_position;
    let mut selected_elevation = center_elevation;

    match mode {
        GridSelectionMode::Nearest => {}
        GridSelectionMode::Land if target_elevation.is_nan() => {}
        GridSelectionMode::Land => {
            let delta_center = (center_elevation - target_elevation).abs();
            if delta_center > 100.0 {
                let mut min_delta = delta_center;
                let mut min_elevation = f32::NAN;
                for (index, elevation) in elevations.iter().copied().enumerate() {
                    if elevation.is_nan() || elevation <= -999.0 {
                        continue;
                    }
                    let x = x0 + index as u64 % width;
                    let y = y0 + index as u64 / width;
                    let grid_latitude =
                        grid_latitude_for_index(&entry.array, entry.native_grid.as_ref(), y)?;
                    let grid_longitude =
                        grid_longitude_for_index(&entry.array, entry.native_grid.as_ref(), x)?;
                    let distance_squared = (grid_latitude - latitude as f32).powi(2)
                        + (grid_longitude - longitude as f32).powi(2);
                    let distance_km = distance_squared.sqrt() * 111.0;
                    let distance_penalty = distance_km * 30.0;
                    let delta = if elevation >= 9999.0 {
                        0.0
                    } else {
                        (elevation - target_elevation).abs()
                    } + distance_penalty;
                    if delta < min_delta && distance_km < 50.0 {
                        min_delta = delta;
                        selected_position = y * nx + x;
                        min_elevation = elevation;
                    }
                }
                if min_elevation.is_nan() || min_delta > 1500.0 {
                    selected_position = center_position;
                    selected_elevation = center_elevation;
                } else {
                    selected_elevation = min_elevation;
                }
            }
        }
        GridSelectionMode::Sea => {
            if center_elevation > -999.0 {
                let mut min_distance = f32::INFINITY;
                let mut found = false;
                for (index, elevation) in elevations.iter().copied().enumerate() {
                    if elevation.is_nan() || elevation > -999.0 {
                        continue;
                    }
                    let x = x0 + index as u64 % width;
                    let y = y0 + index as u64 / width;
                    let grid_latitude =
                        grid_latitude_for_index(&entry.array, entry.native_grid.as_ref(), y)?;
                    let grid_longitude =
                        grid_longitude_for_index(&entry.array, entry.native_grid.as_ref(), x)?;
                    let distance = (grid_latitude - latitude as f32).powi(2)
                        + (grid_longitude - longitude as f32).powi(2);
                    if distance < min_distance {
                        min_distance = distance;
                        selected_position = y * nx + x;
                        selected_elevation = elevation;
                        found = true;
                    }
                }
                if !found {
                    selected_position = center_position;
                    selected_elevation = center_elevation;
                }
            }
        }
    }

    let y = selected_position / nx;
    let x = selected_position % nx;
    let model_elevation = elevation_numeric(selected_elevation);
    let target_elevation = if target_elevation.is_nan() {
        model_elevation
    } else {
        target_elevation
    };
    Ok(ModelSampling {
        latitude: official_f32_json_number(grid_latitude_for_index(
            &entry.array,
            entry.native_grid.as_ref(),
            y,
        )?)?,
        longitude: official_f32_json_number(grid_longitude_for_index(
            &entry.array,
            entry.native_grid.as_ref(),
            x,
        )?)?,
        model_elevation,
        target_elevation,
    })
}

fn static_elevation_spatial_dimensions_match(
    forecast_dimensions: &[u64],
    static_dimensions: &[u64],
) -> bool {
    matches!(forecast_dimensions.len(), 2 | 3)
        && forecast_dimensions.get(..2) == Some(static_dimensions)
}

fn read_static_elevation_grid(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    spec: StaticElevationSpec,
    y0: u64,
    x0: u64,
    height: u64,
    width: u64,
) -> Result<Vec<f32>> {
    let static_root = model_static_root(snapshot)?;
    let path = static_root.join(spec.relative_path);
    let file =
        Arc::new(File::open(&path).with_context(|| format!("failed to open {}", path.display()))?);
    if file.metadata()?.len() != spec.file_size {
        bail!(
            "official static elevation file size is invalid: {}",
            path.display()
        );
    }
    let metadata = build_v3_array_metadata_blob(
        "",
        20,
        0,
        spec.dimensions,
        spec.chunks,
        spec.lut_size,
        spec.lut_offset,
        1.0,
        0.0,
    );
    decoder.decode_grid(
        &metadata,
        &FullFileRangeReader { file },
        &[y0, x0],
        &[height, width],
    )
}

fn resolve_model_static_root(
    data_root: &Path,
    configured_root: Option<PathBuf>,
) -> Result<PathBuf> {
    let static_root = configured_root.unwrap_or_else(|| data_root.to_path_buf());
    if !static_root.is_absolute() {
        bail!("OM_MODEL_STATIC_ROOT must be absolute");
    }
    Ok(static_root)
}

fn model_static_root(snapshot: &OmDataSnapshot) -> Result<PathBuf> {
    resolve_model_static_root(
        &snapshot.data_root,
        std::env::var_os("OM_MODEL_STATIC_ROOT").map(PathBuf::from),
    )
}

fn read_static_elevation_grid_for_coordinates(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    spec: StaticElevationSpec,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<f32>> {
    if latitudes.is_empty() || longitudes.is_empty() {
        bail!("static elevation grid coordinates must not be empty");
    }
    let array = ArrayMetadata {
        data_type: 20,
        compression: 0,
        dimensions: spec.dimensions.to_vec(),
        chunks: spec.chunks.to_vec(),
        lut_offset: Some(spec.lut_offset),
        lut_size: Some(spec.lut_size),
        scale_factor: Some(1.0),
        add_offset: Some(0.0),
    };
    let y_indices = latitudes
        .iter()
        .map(|latitude| {
            grid_index_for_lat_lon(&array, None, *latitude, longitudes[0]).map(|index| index.0)
        })
        .collect::<Result<Vec<_>>>()?;
    let x_indices = longitudes
        .iter()
        .map(|longitude| {
            grid_index_for_lat_lon(&array, None, latitudes[0], *longitude).map(|index| index.1)
        })
        .collect::<Result<Vec<_>>>()?;
    let y0 = *y_indices
        .iter()
        .min()
        .context("static elevation grid has no rows")?;
    let y1 = *y_indices
        .iter()
        .max()
        .context("static elevation grid has no rows")?;
    let x0 = *x_indices
        .iter()
        .min()
        .context("static elevation grid has no columns")?;
    let x1 = *x_indices
        .iter()
        .max()
        .context("static elevation grid has no columns")?;
    let width = x1 - x0 + 1;
    let decoded = read_static_elevation_grid(snapshot, decoder, spec, y0, x0, y1 - y0 + 1, width)?;
    let mut values = Vec::with_capacity(latitudes.len() * longitudes.len());
    for y in y_indices {
        for x in &x_indices {
            values.push(decoded[((y - y0) * width + (*x - x0)) as usize]);
        }
    }
    Ok(values)
}

fn elevation_numeric(value: f32) -> f32 {
    if value <= -999.0 {
        0.0
    } else if value >= 9999.0 {
        f32::NAN
    } else {
        value
    }
}

fn weather_model_location(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<(f64, f64, f32)>> {
    let model = current_weather_model();
    let product_name = model.primary_product();
    let spec = model.static_elevation_spec();
    if let Some(sampling) = current_product_sampling(product_name) {
        return Ok(Some((
            sampling.latitude,
            sampling.longitude,
            sampling.model_elevation,
        )));
    }
    let Some(product) = snapshot.product(product_name) else {
        return Ok(None);
    };
    let entry = product
        .entries
        .values()
        .find(|entry| entry.variable == "temperature_2m")
        .or_else(|| product.entries.values().next())
        .with_context(|| format!("{product_name} has no grid entries"))?;
    let (y, x) = grid_index_for_lat_lon(
        &entry.array,
        entry.native_grid.as_ref(),
        latitude,
        longitude,
    )?;
    let model_latitude = official_f32_json_number(grid_latitude_for_index(
        &entry.array,
        entry.native_grid.as_ref(),
        y,
    )?)?;
    let model_longitude = official_f32_json_number(grid_longitude_for_index(
        &entry.array,
        entry.native_grid.as_ref(),
        x,
    )?)?;

    let Some(decoder) = decoder else {
        return Ok(None);
    };
    let static_path = model_static_root(snapshot)?.join(spec.relative_path);
    if !static_path.exists() {
        bail!(
            "required official {} static elevation file is missing: {}",
            model.display_name(),
            static_path.display()
        );
    }
    let cache_key = (static_path.clone(), y, x);
    let cache = ELEVATION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(elevation) = cache
        .lock()
        .map_err(|_| anyhow!("weather-model elevation cache poisoned"))?
        .get(&cache_key)
        .copied()
    {
        return Ok(Some((model_latitude, model_longitude, elevation)));
    }
    let file = Arc::new(
        File::open(&static_path)
            .with_context(|| format!("failed to open {}", static_path.display()))?,
    );
    if file.metadata()?.len() != spec.file_size {
        bail!(
            "official {} static elevation file size is invalid",
            model.display_name()
        );
    }
    let metadata = build_v3_array_metadata_blob(
        "",
        20,
        0,
        spec.dimensions,
        spec.chunks,
        spec.lut_size,
        spec.lut_offset,
        1.0,
        0.0,
    );
    let reader = FullFileRangeReader { file };
    let elevation = match decoder.decode_point(&metadata, &reader, &[y, x])? {
        value if value <= -900.0 => 0.0,
        value => value,
    };
    cache
        .lock()
        .map_err(|_| anyhow!("weather-model elevation cache poisoned"))?
        .insert(cache_key, elevation);
    Ok(Some((model_latitude, model_longitude, elevation)))
}

fn air_quality_model_location(
    snapshot: &OmDataSnapshot,
    latitude: f64,
    longitude: f64,
) -> Result<Option<(f64, f64)>> {
    let Some(product) = snapshot
        .product("cams_global_greenhouse_gases")
        .or_else(|| snapshot.product("cams_global"))
    else {
        return Ok(None);
    };
    let entry = product
        .entries
        .values()
        .find(|entry| entry.variable == "carbon_monoxide")
        .or_else(|| product.entries.values().next())
        .context("air-quality model has no grid entries")?;
    let (y, x) = grid_index_for_lat_lon(
        &entry.array,
        entry.native_grid.as_ref(),
        latitude,
        longitude,
    )?;
    Ok(Some((
        official_f32_json_number(grid_latitude_for_index(
            &entry.array,
            entry.native_grid.as_ref(),
            y,
        )?)?,
        official_f32_json_number(grid_longitude_for_index(
            &entry.array,
            entry.native_grid.as_ref(),
            x,
        )?)?,
    )))
}

fn official_f32_json_number(value: f32) -> Result<f64> {
    let mut buffer = ryu::Buffer::new();
    buffer
        .format_finite(value)
        .parse::<f64>()
        .context("failed to format model coordinate")
}

#[derive(Debug)]
struct FullFileRangeReader {
    file: Arc<File>,
}

impl BundleRangeReader for FullFileRangeReader {
    fn read_original_range(&self, start: u64, count: u64) -> Result<Vec<u8>> {
        let mut out = vec![0_u8; count as usize];
        self.file.read_exact_at(&mut out, start)?;
        Ok(out)
    }
}

fn model_latitude_for_variable(
    snapshot: &OmDataSnapshot,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let (product_name, raw_variable) = product_for_variable(snapshot, variable)?;
    if let Some(sampling) = current_product_sampling(product_name) {
        return Ok(sampling.latitude as f32);
    }
    let product = snapshot.require_product(product_name)?;
    let key = EntryKey {
        variable: raw_variable,
        valid_time_utc: time,
    };
    let entry = product
        .entries
        .get(&key)
        .with_context(|| format!("variable/time is not available: {} {}", variable, time))?;
    let (y, _) = grid_index_for_lat_lon(
        &entry.array,
        entry.native_grid.as_ref(),
        latitude,
        longitude,
    )?;
    grid_latitude_for_index(&entry.array, entry.native_grid.as_ref(), y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gfs_only_short_circuits_ecmwf_soil_layer_depths() {
        for variable in [
            "soil_temperature_0_to_7cm",
            "soil_temperature_7_to_28cm",
            "soil_temperature_28_to_100cm",
            "soil_temperature_100_to_255cm",
            "soil_moisture_0_to_7cm",
            "soil_moisture_7_to_28cm",
            "soil_moisture_28_to_100cm",
            "soil_moisture_100_to_255cm",
        ] {
            assert!(is_gfs_unavailable_ecmwf_soil_layer(variable));
        }
        for variable in [
            "soil_temperature_0_to_10cm",
            "soil_temperature_10_to_40cm",
            "soil_temperature_40_to_100cm",
            "soil_temperature_100_to_200cm",
            "soil_moisture_0_to_10cm",
            "soil_moisture_10_to_40cm",
            "soil_moisture_40_to_100cm",
            "soil_moisture_100_to_200cm",
        ] {
            assert!(!is_gfs_unavailable_ecmwf_soil_layer(variable));
        }
    }

    #[test]
    fn static_elevation_accepts_spatial_and_spatial_time_archives() {
        assert!(static_elevation_spatial_dimensions_match(
            &[361, 720],
            GEFS05_STATIC_DIMENSIONS,
        ));
        assert!(static_elevation_spatial_dimensions_match(
            &[361, 720, 313],
            GEFS05_STATIC_DIMENSIONS,
        ));
        assert!(!static_elevation_spatial_dimensions_match(
            &[720, 361, 313],
            GEFS05_STATIC_DIMENSIONS,
        ));
        assert!(!static_elevation_spatial_dimensions_match(
            &[361, 720, 313, 2],
            GEFS05_STATIC_DIMENSIONS,
        ));
    }

    #[test]
    fn gfs_probability_mixer_matches_official_three_step_transition() {
        let mut high = vec![
            vec![0.0],
            vec![27.0],
            vec![31.0],
            vec![35.0],
            vec![39.0],
            vec![43.0],
            vec![f32::NAN],
            vec![f32::NAN],
        ];
        let low = vec![
            vec![91.0],
            vec![83.0],
            vec![75.0],
            vec![67.0],
            vec![59.0],
            vec![51.0],
            vec![64.0],
            vec![f32::NAN],
        ];

        integrate_probability_if_nan_smooth(&mut high, &low, 1);

        assert_eq!(
            &high[..7],
            &[
                vec![0.0],
                vec![27.0],
                vec![31.0],
                vec![43.0],
                vec![49.0],
                vec![49.0],
                vec![64.0],
            ]
        );
        assert!(high[7][0].is_nan());
    }

    #[test]
    fn gfs_probability_terminal_first_stage_repeats_c_like_open_meteo() {
        let mut values = vec![2.0, f32::NAN, 8.0, f32::NAN, 10.0];

        interpolate_ecmwf_hermite_in_place(&mut values, Some((0.0, 100.0)), false, false);

        assert!((values[3] - 9.375).abs() < 1e-6);
    }

    #[test]
    fn dew_point_preserves_open_meteo_float_order_at_vpd_rounding_boundary() {
        // Frozen ECMWF p0146 at 2026-07-28T11:00Z. The model value is
        // elevation-corrected from 336 m to the requested DEM elevation of
        // 387 m before the derived VPD is calculated.
        let temperature = 21.9_f32 + (336.0 - 387.0) * 0.0065;
        let deficit = vapor_pressure_deficit(temperature, dew_point(temperature, 80.0));

        assert_eq!(
            json_value_for_variable("vapor_pressure_deficit", deficit),
            serde_json::json!(0.51)
        );
    }

    #[test]
    fn combined_soil_layer_preserves_official_float_weight_order() {
        let value = weighted_soil_layer_0_to_100cm(0.354, 0.356, 0.343);
        assert_eq!(
            json_value_for_variable("soil_moisture_0_to_100cm", value),
            serde_json::json!(0.347)
        );
    }

    #[test]
    fn et0_propagates_a_missing_shortwave_tail_frame() {
        assert!(et0_evapotranspiration(-0.4, 0.71, -0.8, f32::NAN, 4_884.0, 0.0, 3600,).is_nan());
    }

    #[test]
    fn wind_direction_matches_pinned_official_fast_approximation() {
        let inputs = [
            (-1.0, -3.0),
            (-0.0, -2.0),
            (0.0, -1.0),
            (1.0, -0.0),
            (2.0, 0.0),
            (3.0, 1.0),
            (4.0, 2.0),
            (5.0, 3.0),
            (6.0, 4.0),
        ];
        // Bit patterns captured from the pinned official C source compiled with
        // the same Linux/x86_64 native production flags.
        let expected = [
            f32::from_bits(0x41937afd),
            f32::from_bits(0x43b40000),
            f32::from_bits(0x43b40000),
            f32::from_bits(0x43870000),
            f32::from_bits(0x43870000),
            f32::from_bits(0x437b90a1),
            f32::from_bits(0x43736f5d),
            f32::from_bits(0x436f094b),
            f32::from_bits(0x436c4f56),
        ];

        let u = inputs.map(|(u, _)| u);
        let v = inputs.map(|(_, v)| v);
        assert_eq!(wind_directions(&u, &v), expected);
        assert!(wind_direction(f32::NAN, 1.0).is_nan());
    }

    #[test]
    fn ecmwf_native_wind_batch_matches_official_north_boundary_bits() {
        let u: [f32; 8] = [-1.2, 0.5, 0.2, 1.8, 2.1, 5.3, -3.8, -4.9];
        let v: [f32; 8] = [2.6, 1.1, -0.7, 2.2, -8.6, -9.6, -4.5, 0.6];
        let speed: [f32; 8] =
            std::array::from_fn(|index| (u[index] * u[index] + v[index] * v[index]).sqrt());
        let direction = wind_directions(&u, &v);
        let expected_direction_bits = [
            0x431b3992, 0x434c71a6, 0x43ac06fb, 0x435b4a15, 0x43ad238b, 0x43a58c83, 0x4220b77a,
            0x42c1f641,
        ];
        assert_eq!(
            direction
                .iter()
                .map(|value| value.to_bits())
                .collect::<Vec<_>>(),
            expected_direction_bits
        );

        let dominant = dominant_wind_direction(&speed, &direction);
        assert_eq!(dominant.to_bits(), 0x38070908);
        assert_eq!(
            json_value_for_variable("wind_direction_100m", dominant),
            serde_json::json!(0)
        );
    }

    #[test]
    fn dominant_wind_direction_preserves_north_as_360() {
        // Internal single-precision model frames from the frozen p0014
        // boundary case. Rounding these inputs before vector reconstruction
        // changes the strict result from the official 360 to 0.
        let speed = [
            0.98994946, 0.2, 1.0630146, 1.1, 1.3453624, 1.5, 1.9235384, 1.8439089,
        ];
        let direction = [
            44.999893, 180.0, 228.81418, 270.0, 318.01288, 360.0, 27.897186, 40.60121,
        ];
        assert_eq!(
            json_value_for_variable(
                "wind_direction_100m",
                dominant_wind_direction(&speed, &direction),
            ),
            serde_json::json!(360)
        );
    }

    #[test]
    fn dominant_wind_direction_uses_swift_float_pi() {
        // Frozen p0052 internal frames land exactly on an integer-output
        // boundary. Rust's one-ULP-higher PI returns 185; Swift Float.pi,
        // used by Open-Meteo's uWind/vWind helpers, returns the official 184.
        let speed = [
            4.909175, 5.5443664, 5.323533, 4.785394, 4.8270073, 5.830952, 6.207254, 6.1,
        ];
        let direction = [
            183.50346, 187.2531, 174.61078, 190.84026, 193.17256, 185.90605, 182.77016, 180.0,
        ];
        assert_eq!(
            json_value_for_variable(
                "wind_direction_10m",
                dominant_wind_direction(&speed, &direction),
            ),
            serde_json::json!(184)
        );
    }

    #[test]
    fn dominant_wind_direction_preserves_sub_ulp_north_as_360() {
        // Frozen ECMWF p0487 spatial-archive frames for 2026-07-30. Their
        // reconstructed u sum is less than one ULP of the northward v sum;
        // the official source preserves the north boundary as 360.
        let speed = [
            f32::from_bits(0x3f3a5eed),
            f32::from_bits(0x3f9bb7fe),
            f32::from_bits(0x3f9bb7fe),
            f32::from_bits(0x3eb89aaf),
            f32::from_bits(0x3fe4f92f),
            f32::from_bits(0x3fdc3833),
            f32::from_bits(0x3fd9f9e5),
            f32::from_bits(0x3f80a36f),
        ];
        let direction = [
            f32::from_bits(0x42d3e414),
            f32::from_bits(0x42c6ecac),
            f32::from_bits(0x42c6ecac),
            f32::from_bits(0x4397d855),
            f32::from_bits(0x43944852),
            f32::from_bits(0x43a23b2a),
            f32::from_bits(0x43b2511a),
            f32::from_bits(0x43b1250e),
        ];
        assert_eq!(
            json_value_for_variable(
                "wind_direction_100m",
                dominant_wind_direction(&speed, &direction),
            ),
            serde_json::json!(360)
        );
    }

    #[test]
    fn hj633_co_interpolation_does_not_ceil_exact_integer_due_to_f32_noise() {
        assert_eq!(
            hj633_2026_iaqi(0.6, &HJ633_CO_DAILY, &HJ633_AQI_BREAKPOINTS, 500.0, 1,),
            15.0
        );
        assert_eq!(
            hj633_2026_iaqi(0.7, &HJ633_CO_DAILY, &HJ633_AQI_BREAKPOINTS, 500.0, 1,),
            18.0
        );
    }

    #[test]
    fn gfs_flux_variables_follow_official_interpolation_metadata() {
        assert!(matches!(
            interpolation_kind_for_variable("uv_index_clear_sky"),
            InterpolationKind::SolarBackwardsAveraged { scalefactor: 20.0 }
        ));
        assert!(matches!(
            interpolation_kind_for_variable("shortwave_radiation"),
            InterpolationKind::SolarBackwardsAveraged { scalefactor: 1.0 }
        ));
        assert!(matches!(
            interpolation_kind_for_variable("latent_heat_flux"),
            InterpolationKind::Hermite {
                scalefactor: 0.144,
                bounds: None
            }
        ));
    }

    #[test]
    fn grid_index_matches_official_float_rounding_on_half_cell() {
        let array = ArrayMetadata {
            data_type: 20,
            compression: 4,
            dimensions: vec![451, 900],
            chunks: vec![32, 32],
            lut_offset: None,
            lut_size: None,
            scale_factor: None,
            add_offset: None,
        };

        let (y, x) = grid_index_for_lat_lon(&array, None, 4.2, 75.3).unwrap();

        assert_eq!((y, x), (235, 638));
    }

    #[test]
    fn global_spatial_time_archive_uses_its_leading_grid_dimensions() {
        let array = ArrayMetadata {
            data_type: 20,
            compression: 0,
            dimensions: vec![361, 720, 313],
            chunks: vec![1, 9, 313],
            lut_offset: None,
            lut_size: None,
            scale_factor: None,
            add_offset: None,
        };

        let (y, x) = grid_index_for_lat_lon(&array, None, 30.6722, 104.0597).unwrap();

        assert_eq!((y, x), (241, 568));
        assert_eq!(grid_latitude_for_index(&array, None, y).unwrap(), 30.5);
        assert_eq!(grid_longitude_for_index(&array, None, x).unwrap(), 104.0);
    }

    #[test]
    fn greenhouse_grid_coordinates_match_air_quality_response_metadata() {
        let array = ArrayMetadata {
            data_type: 20,
            compression: 4,
            dimensions: vec![1801, 3600],
            chunks: vec![32, 32],
            lut_offset: None,
            lut_size: None,
            scale_factor: None,
            add_offset: None,
        };

        let (y, x) = grid_index_for_lat_lon(&array, None, 29.5638, 106.5505).unwrap();
        assert_eq!(grid_latitude_for_index(&array, None, y).unwrap(), 29.599998);
        assert_eq!(
            grid_longitude_for_index(&array, None, x).unwrap(),
            106.600006
        );
    }

    #[test]
    fn native_cams_crop_preserves_official_global_float_coordinates() {
        let array = ArrayMetadata {
            data_type: 20,
            compression: 4,
            dimensions: vec![150, 180, 121],
            chunks: vec![32, 32, 121],
            lut_offset: None,
            lut_size: None,
            scale_factor: None,
            add_offset: None,
        };
        let grid = crate::manifest::NativeGridMetadata {
            nx: 180,
            ny: 150,
            lon_min: 69.20000000000002,
            lat_min: -0.7999999999999972,
            dx: 0.4,
            dy: 0.4,
            dt_seconds: 3600,
            om_file_length: 217,
            full_nx: Some(900),
            full_ny: Some(451),
            x0: Some(623),
            y0: Some(223),
        };

        assert_eq!(
            grid_latitude_for_index(&array, Some(&grid), 44).unwrap(),
            16.800003
        );
        // At 75°E the regional origin calculation lies infinitesimally below
        // a half-cell.  It must nevertheless select the same global CAMS cell
        // as Shanghai's uncropped 900-column source grid.
        assert_eq!(
            grid_index_for_lat_lon(&array, Some(&grid), 16.8, 75.0).unwrap(),
            (44, 15)
        );
    }

    #[test]
    fn grid_index_uses_official_gfs013_latitude_spacing() {
        let array = ArrayMetadata {
            data_type: 20,
            compression: 4,
            dimensions: vec![1536, 3072],
            chunks: vec![32, 32],
            lut_offset: None,
            lut_size: None,
            scale_factor: None,
            add_offset: None,
        };

        let (y, x) = grid_index_for_lat_lon(&array, None, 11.6, 85.9).unwrap();

        assert_eq!((y, x), (867, 2269));
    }

    #[test]
    fn grid_latitude_uses_selected_gfs013_cell_center() {
        let array = ArrayMetadata {
            data_type: 20,
            compression: 4,
            dimensions: vec![1536, 3072],
            chunks: vec![32, 32],
            lut_offset: None,
            lut_size: None,
            scale_factor: None,
            add_offset: None,
        };

        let (y, _) = grid_index_for_lat_lon(&array, None, 22.75, 125.0).unwrap();
        let model_latitude = grid_latitude_for_index(&array, None, y).unwrap();

        assert!((model_latitude - 22.78555).abs() < 0.00001);
    }

    #[test]
    fn regional_gfs_cell_centres_map_to_global_static_indices() {
        let regional_array = ArrayMetadata {
            data_type: 20,
            compression: 0,
            dimensions: vec![513, 615, 481],
            chunks: vec![20, 20, 481],
            lut_offset: None,
            lut_size: None,
            scale_factor: Some(1.0),
            add_offset: Some(0.0),
        };
        let regional_grid = crate::manifest::NativeGridMetadata {
            nx: 615,
            ny: 513,
            lon_min: 69.0234375,
            lat_min: -0.995769475000003,
            dx: 0.1171875,
            dy: 0.11714935,
            dt_seconds: 3600,
            om_file_length: 481,
            full_nx: Some(3072),
            full_ny: Some(1536),
            x0: Some(2125),
            y0: Some(759),
        };
        let global_array = ArrayMetadata {
            data_type: 20,
            compression: 0,
            dimensions: GFS013_STATIC_SPEC.dimensions.to_vec(),
            chunks: GFS013_STATIC_SPEC.chunks.to_vec(),
            lut_offset: Some(GFS013_STATIC_SPEC.lut_offset),
            lut_size: Some(GFS013_STATIC_SPEC.lut_size),
            scale_factor: Some(1.0),
            add_offset: Some(0.0),
        };

        for (regional_y, regional_x) in [(0, 0), (275, 448), (512, 614)] {
            let latitude = f64::from(
                grid_latitude_for_index(&regional_array, Some(&regional_grid), regional_y).unwrap(),
            );
            let longitude = f64::from(
                grid_longitude_for_index(&regional_array, Some(&regional_grid), regional_x)
                    .unwrap(),
            );
            assert_eq!(
                grid_index_for_lat_lon(&global_array, None, latitude, longitude).unwrap(),
                (regional_y + 759, regional_x + 2125)
            );
        }
    }

    #[test]
    fn model_coordinates_use_official_shortest_float_representation() {
        assert_eq!(official_f32_json_number(131.953_13).unwrap(), 131.95312);
        assert_eq!(official_f32_json_number(75.585_94).unwrap(), 75.58594);
        assert_eq!(official_f32_json_number(82.734375).unwrap(), 82.734375);
        assert_eq!(official_f32_json_number(39.069_32).unwrap(), 39.06932);
    }

    #[test]
    fn wind_gusts_are_routed_to_gfs025() {
        assert!(is_gfs025_variable("wind_gusts_10m"));
    }

    #[test]
    fn point_weather_code_uses_land_selected_surface_model_latitude() {
        let sampling = RequestSampling {
            gfs013: Some(ModelSampling {
                latitude: 13.647_903,
                longitude: 123.164_06,
                model_elevation: 0.0,
                target_elevation: 0.0,
            }),
            gfs025: Some(ModelSampling {
                latitude: 13.75,
                longitude: 123.25,
                model_elevation: 0.0,
                target_elevation: 0.0,
            }),
            gefs025: None,
            gefs05: None,
            ecmwf025: None,
            ecmwf025_ensemble: None,
            response_elevation: 0.0,
        };
        let selected_latitude = with_request_sampling(sampling, || {
            Ok(
                point_weather_code_model_latitudes(&[13.765_053], &[123.164_06])
                    .expect("point sampling")
                    .remove(0),
            )
        })
        .unwrap();

        let calculate = |latitude| {
            weather_code(
                100.0,
                4.1,
                Some(4.1),
                0.0,
                Some(2.0),
                Some(930.0),
                Some(-2.6),
                Some(23.0),
                Some(15.0),
                Some(3720.0),
                Some(0.0),
                3600,
                latitude,
            )
        };
        assert_eq!(calculate(selected_latitude), Some(81.0));
        assert_eq!(calculate(13.765_053), Some(95.0));
    }

    #[test]
    fn sparse_solar_preserves_official_half_step_quantization() {
        let start = Utc.with_ymd_and_hms(2026, 7, 20, 18, 0, 0).unwrap();
        let mut values = vec![f32::NAN; 385];
        for (index, bits) in [
            (168, 0x0000_0000),
            (171, 0x0000_0000),
            (174, 0x3e4c_cccd),
            (177, 0x4088_0000),
            (180, 0x4108_cccd),
            (183, 0x40d1_999a),
            (186, 0x3f99_999a),
            (189, 0x0000_0000),
            (192, 0x0000_0000),
            (195, 0x0000_0000),
        ] {
            values[index] = f32::from_bits(bits);
        }

        interpolate_solar_backwards_in_place(
            &mut values,
            start,
            f32::from_bits(0x4163_bd08) as f64,
            f32::from_bits(0x42d2_3c00) as f64,
        );

        assert_eq!(values[181].to_bits(), 0x4102_f407);
        assert_eq!(
            maybe_round_to_scalefactor(values[181], 20.0, true).to_bits(),
            0x4103_3333
        );
    }

    #[test]
    fn ecmwf_solar_transition_matches_frozen_2026080400_official_point() {
        let start = Utc.with_ymd_and_hms(2026, 8, 4, 3, 0, 0).unwrap();
        let mut values = vec![f32::NAN; 61];
        for (hour, value) in [
            (129, 13.0),
            (132, 5.0),
            (135, 0.0),
            (138, 0.0),
            (141, 0.0),
            (144, 151.0),
            (150, 555.0),
            (156, 142.0),
            (162, 0.0),
            (168, 110.0),
            (174, 395.0),
            (180, 85.0),
        ] {
            values[(hour - 3) / 3] = value;
        }

        interpolate_solar_backwards_with_dt_in_place(&mut values, start, 3 * 3600, 20.0, 134.0);
        for value in values.iter_mut().filter(|value| value.is_finite()) {
            *value = round_to_scalefactor(*value, 1.0);
        }
        let times = (0..values.len())
            .map(|index| start + Duration::hours(index as i64 * 3))
            .collect::<Vec<_>>();
        let hourly = (141..=156)
            .map(|hour| {
                ecmwf_second_stage_value(
                    &values,
                    &times,
                    InterpolationKind::SolarBackwardsAveraged { scalefactor: 1.0 },
                    Utc.with_ymd_and_hms(2026, 8, 4, 0, 0, 0).unwrap() + Duration::hours(hour),
                    20.0,
                    134.0,
                    3600,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            hourly,
            vec![
                2.0, 63.0, 154.0, 271.0, 414.0, 540.0, 609.0, 624.0, 593.0, 528.0, 425.0, 285.0,
                130.0, 15.0, 0.0, 0.0,
            ]
        );
    }

    #[test]
    fn ecmwf_retained_gust_frame_supplies_official_second_stage_lookahead() {
        let start = Utc.with_ymd_and_hms(2026, 7, 26, 12, 0, 0).unwrap();
        let kind = InterpolationKind::Hermite {
            scalefactor: 10.0,
            bounds: Some((0.0, 10e9)),
        };
        let values = vec![3.6, 6.3, 7.1, 6.8];
        let times = (0..values.len())
            .map(|index| start + Duration::hours(index as i64 * 3))
            .collect::<Vec<_>>();
        let hour_88 = ecmwf_second_stage_value(
            &values,
            &times,
            kind,
            start + Duration::hours(4),
            0.0,
            70.0,
            3600,
        );

        assert_eq!(hour_88, 6.7);
    }

    #[test]
    fn ecmwf_probability_extends_the_final_hermite_interval() {
        let start = Utc.with_ymd_and_hms(2026, 8, 12, 9, 0, 0).unwrap();
        let kind = InterpolationKind::Hermite {
            scalefactor: 1.0,
            bounds: Some((0.0, 100.0)),
        };
        let values = vec![60.0, 57.0];
        let times = vec![start, start + Duration::hours(3)];

        let value_at = |hours, extend_terminal_interval| {
            ecmwf_second_stage_value_with_terminal_extension(
                &values,
                &times,
                kind,
                start + Duration::hours(hours),
                20.0,
                134.0,
                3600,
                extend_terminal_interval,
            )
        };

        assert_eq!(value_at(3, true), 57.0);
        assert_eq!(value_at(4, true), 57.0);
        assert_eq!(value_at(5, true), 57.0);
        assert!(value_at(6, true).is_nan());
        assert!(value_at(4, false).is_nan());

        let rising = vec![26.0, 45.0];
        assert_eq!(
            ecmwf_second_stage_value_with_terminal_extension(
                &rising,
                &times,
                kind,
                start + Duration::hours(4),
                34.0,
                76.0,
                3600,
                true,
            ),
            46.0
        );
        assert_eq!(
            ecmwf_second_stage_value_with_terminal_extension(
                &rising,
                &times,
                kind,
                start + Duration::hours(5),
                34.0,
                76.0,
                3600,
                true,
            ),
            46.0
        );
    }

    #[test]
    fn ecmwf_gust_first_stage_leaves_boundary_gaps_for_older_complete_run() {
        let mut values = vec![6.3, f32::NAN, 6.6, f32::NAN, 6.9, f32::NAN, 9.6];
        interpolate_ecmwf_hermite_in_place(&mut values, Some((0.0, 10e9)), true, false);

        assert!(values[1].is_nan());
        assert!((values[3] - 6.6).abs() < 1e-6);
        assert!(values[5].is_nan());
    }

    #[test]
    fn ecmwf_gust_first_stage_does_not_bridge_an_unsupported_internal_window() {
        let mut values = vec![6.3, f32::NAN, f32::NAN, f32::NAN, 7.1, f32::NAN, 6.8];
        interpolate_ecmwf_hermite_in_place(&mut values, Some((0.0, 10e9)), true, false);

        assert!(values[1..4].iter().all(|value| value.is_nan()));
        assert!(values[5].is_nan());
    }

    #[test]
    fn ecmwf_gust_terminal_tail_uses_c_as_missing_d_only_at_true_horizon() {
        let start = Utc.with_ymd_and_hms(2026, 8, 6, 12, 0, 0).unwrap();
        let kind = InterpolationKind::Hermite {
            scalefactor: 10.0,
            bounds: Some((0.0, 10e9)),
        };
        let mut values = vec![3.1, f32::NAN, 3.5, f32::NAN, 2.1];
        ecmwf_first_stage_point(&mut values, kind, start, 0.0, 70.0, true, true);

        assert!(values[1].is_nan());
        assert_eq!(values[3], 2.8);

        values[1] = 3.4;
        let times = (0..values.len())
            .map(|index| start + Duration::hours(index as i64 * 3))
            .collect::<Vec<_>>();
        let hourly = (0..=12)
            .map(|hour| {
                ecmwf_second_stage_value(
                    &values,
                    &times,
                    kind,
                    start + Duration::hours(hour),
                    0.0,
                    70.0,
                    3600,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            hourly,
            vec![3.1, 3.2, 3.3, 3.4, 3.5, 3.5, 3.5, 3.3, 3.1, 2.8, 2.5, 2.2, 2.1]
        );
    }

    #[test]
    fn ecmwf_rollover_respects_the_public_start_support_boundary() {
        let start = Utc.with_ymd_and_hms(2026, 7, 31, 0, 0, 0).unwrap();
        let public_start = start + Duration::hours(16);
        assert_eq!(
            ecmwf_latest_support_start(start, public_start),
            start + Duration::hours(12)
        );
        assert!(!ecmwf_run_is_eligible(
            "2026073100",
            start,
            Some("2026073100"),
            Some(public_start),
            start + Duration::hours(11)
        ));
        assert!(ecmwf_run_is_eligible(
            "2026073100",
            start,
            Some("2026073100"),
            Some(public_start),
            start + Duration::hours(12)
        ));

        let probability_times = [9, 12, 15, 18]
            .into_iter()
            .map(|hour| start + Duration::hours(hour))
            .collect::<Vec<_>>();
        let probability = ecmwf_second_stage_value(
            &[55.0, 90.0, 94.0, 78.0],
            &probability_times,
            InterpolationKind::Hermite {
                scalefactor: 1.0,
                bounds: Some((0.0, 100.0)),
            },
            start + Duration::hours(14),
            14.0,
            140.0,
            3600,
        );
        assert_eq!(probability, 95.0);
    }

    #[test]
    fn ecmwf_native_fallback_uses_only_the_immediately_older_covering_run() {
        let start = Utc.with_ymd_and_hms(2026, 8, 3, 12, 0, 0).unwrap();
        let runs = [
            EcmwfRegularRun {
                source_run: "2026080312".to_string(),
                start,
                frames: vec![vec![f32::NAN]],
            },
            EcmwfRegularRun {
                source_run: "2026080306".to_string(),
                start,
                frames: vec![vec![f32::NAN]],
            },
            EcmwfRegularRun {
                source_run: "2026080300".to_string(),
                start,
                frames: vec![vec![18.4]],
            },
        ];
        let newest_first = runs.iter().collect::<Vec<_>>();

        assert!(
            ecmwf_one_level_native_value(&newest_first, Some("2026080312"), None, start, 0,)
                .is_nan()
        );

        let runs = [
            EcmwfRegularRun {
                source_run: "2026080312".to_string(),
                start,
                frames: vec![vec![f32::NAN]],
            },
            EcmwfRegularRun {
                source_run: "2026080306".to_string(),
                start,
                frames: vec![vec![19.0]],
            },
        ];
        let newest_first = runs.iter().collect::<Vec<_>>();
        assert_eq!(
            ecmwf_one_level_native_value(&newest_first, Some("2026080312"), None, start, 0,),
            19.0
        );
    }

    #[test]
    fn ecmwf_tail_skips_non_covering_short_run_without_spending_nan_fallback() {
        let start = Utc.with_ymd_and_hms(2026, 8, 3, 0, 0, 0).unwrap();
        let current = EcmwfRegularRun {
            source_run: "2026080312".to_string(),
            start,
            frames: vec![vec![f32::NAN]],
        };
        let short = EcmwfRegularRun {
            source_run: "2026080306".to_string(),
            start,
            frames: vec![vec![f32::NAN]],
        };
        let older_long = EcmwfRegularRun {
            source_run: "2026080300".to_string(),
            start,
            frames: vec![vec![12.0], vec![13.0]],
        };
        let runs = vec![&current, &short, &older_long];

        assert_eq!(
            ecmwf_one_level_native_value(
                &runs,
                Some("2026080312"),
                None,
                start + Duration::hours(3),
                0,
            ),
            13.0
        );
    }

    #[test]
    fn ecmwf_duplicate_support_run_extends_from_its_complete_retained_record() {
        let start = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap();
        let support = EcmwfRegularRun {
            source_run: "2026080200".to_string(),
            start,
            frames: vec![vec![10.0], vec![11.0]],
        };
        let complete = EcmwfRegularRun {
            source_run: "2026080200".to_string(),
            start,
            frames: vec![vec![20.0], vec![21.0], vec![22.0]],
        };
        let runs = vec![&support, &complete];

        assert_eq!(
            ecmwf_one_level_native_value(&runs, Some("2026080206"), None, start, 0),
            10.0
        );
        assert_eq!(
            ecmwf_one_level_native_value(
                &runs,
                Some("2026080206"),
                None,
                start + Duration::hours(6),
                0,
            ),
            22.0
        );
    }

    #[test]
    fn ecmwf_generation_resolves_packaged_support_before_nan_fallback() {
        let start = Utc.with_ymd_and_hms(2026, 8, 6, 3, 0, 0).unwrap();
        let current = Arc::new(EcmwfRegularSeries {
            runs: vec![
                EcmwfRegularRun {
                    source_run: "2026073000".to_string(),
                    start,
                    frames: vec![vec![12.0]],
                },
                EcmwfRegularRun {
                    source_run: "2026080206".to_string(),
                    start,
                    frames: vec![vec![f32::NAN]],
                },
            ],
            public_start_utc: None,
            latest_source_run: Some("2026080206".to_string()),
        });
        let previous = Arc::new(EcmwfRegularSeries {
            runs: vec![EcmwfRegularRun {
                source_run: "2026080200".to_string(),
                start,
                frames: vec![vec![20.0]],
            }],
            public_start_utc: None,
            latest_source_run: Some("2026080200".to_string()),
        });

        assert_eq!(
            ecmwf_one_level_generation_value(
                &[current, previous],
                Some("2026080206"),
                None,
                start,
                0,
            ),
            12.0
        );
    }

    #[test]
    fn ecmwf_generation_nan_fallback_stops_after_previous_coverage() {
        let start = Utc.with_ymd_and_hms(2026, 8, 6, 3, 0, 0).unwrap();
        let series = [
            ("2026080206", f32::NAN),
            ("2026080200", f32::NAN),
            ("2026080118", 18.0),
        ]
        .into_iter()
        .map(|(source_run, value)| {
            Arc::new(EcmwfRegularSeries {
                runs: vec![EcmwfRegularRun {
                    source_run: source_run.to_string(),
                    start,
                    frames: vec![vec![value]],
                }],
                public_start_utc: None,
                latest_source_run: Some(source_run.to_string()),
            })
        })
        .collect::<Vec<_>>();

        assert!(
            ecmwf_one_level_generation_value(&series, Some("2026080206"), None, start, 0,).is_nan()
        );
    }

    #[test]
    fn sparse_solar_full_series_preserves_current_clear_sky_sunset_tail() {
        let start = Utc.with_ymd_and_hms(2026, 8, 4, 9, 0, 0).unwrap();
        let mut values = vec![f32::NAN; 10];
        values[0] = 100.0;
        values[3] = 100.0;
        values[6] = 0.0;
        values[9] = 0.0;

        interpolate_solar_backwards_in_place(&mut values, start, 50.315_66, 107.929_69);

        assert!((values[4] - 0.829_054_7).abs() < 1e-6);
        assert!(values.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn sparse_solar_full_series_keeps_current_nighttime_interval_zero() {
        let start = Utc.with_ymd_and_hms(2026, 7, 26, 6, 0, 0).unwrap();
        let mut values = vec![f32::NAN; 10];
        values[0] = 100.0;
        values[3] = 100.0;
        values[6] = 100.0;
        values[9] = 0.0;

        interpolate_solar_backwards_in_place(&mut values, start, 6.853_241, 132.656_25);

        assert_eq!(values[4], 0.0);
        assert!(values.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn sparse_solar_full_series_reuses_deaveraged_values_sequentially() {
        let start = Utc.with_ymd_and_hms(2026, 7, 30, 18, 0, 0).unwrap();
        let mut values = vec![f32::NAN; 13];
        for index in [0, 3, 6, 9, 12] {
            values[index] = 100.0;
        }
        let raw_b = values[6];

        interpolate_solar_backwards_in_place(&mut values, start, 9.899_124, 137.812_5);

        // 00 UTC is C of the preceding interval and B of the interval that
        // contains 01 UTC. Official processing deaverages it before the later
        // interval is evaluated.
        assert_ne!(values[6], raw_b);
        assert!(values[7].is_finite());
        assert!(values[7] > 0.0);
        assert!(values.iter().all(|value| value.is_finite()));
    }

    #[test]
    fn surface_pressure_matches_openmeteo_formula() {
        assert_eq!(surface_pressure(20.0, 1013.25, f32::NAN), 1013.25);
        assert!((surface_pressure(20.0, 1013.25, 1000.0) - 902.9).abs() < 0.2);
        assert_eq!(
            seed_variable_for_times("surface_pressure"),
            "temperature_2m"
        );
    }

    #[test]
    fn surface_pressure_uses_model_elevation_when_target_is_nan() {
        let sampling = ModelSampling {
            latitude: 30.0,
            longitude: 120.0,
            model_elevation: 864.0,
            target_elevation: f32::NAN,
        };
        assert_eq!(surface_pressure_elevation(sampling), 864.0);
    }

    #[test]
    fn fixed_model_static_root_overrides_cycle_data_root() {
        let data_root = Path::new("/data/om_raw");
        let fixed_root = PathBuf::from("/opt/1panel/apps/weather_om_api");

        assert_eq!(
            resolve_model_static_root(data_root, Some(fixed_root.clone())).unwrap(),
            fixed_root
        );
        assert_eq!(
            resolve_model_static_root(data_root, None).unwrap(),
            data_root
        );
        assert!(resolve_model_static_root(data_root, Some(PathBuf::from("relative"))).is_err());
    }

    #[test]
    fn webp_output_rounding_matches_json_precision() {
        assert_eq!(round_variable_output_value("temperature_2m", 24.85), 24.9);
        assert_eq!(round_variable_output_value("cloud_cover", 49.6), 50.0);
        assert_eq!(
            round_variable_output_value("aerosol_optical_depth", 0.126),
            0.13
        );
    }

    #[test]
    fn weather_model_aliases_and_context_are_isolated() {
        assert_eq!(WeatherModel::parse(None).unwrap(), WeatherModel::Gfs);
        assert_eq!(
            WeatherModel::parse(Some("ecmwf_ifs025")).unwrap(),
            WeatherModel::EcmwfIfs025
        );
        with_weather_model(WeatherModel::EcmwfIfs025, || {
            assert_eq!(current_weather_model(), WeatherModel::EcmwfIfs025);
            assert_eq!(
                seed_variable_for_times("cloud_cover_850hPa"),
                "relative_humidity_850hPa"
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(current_weather_model(), WeatherModel::Gfs);
        assert!(WeatherModel::parse(Some("unknown")).is_err());
    }

    #[test]
    fn ecmwf_interpolation_contract_matches_pinned_official_source() {
        with_weather_model(WeatherModel::EcmwfIfs025, || {
            assert!(matches!(
                interpolation_kind_for_variable("precipitation"),
                InterpolationKind::BackwardsSum { scalefactor: 10.0 }
            ));
            assert!(matches!(
                interpolation_kind_for_variable("shortwave_radiation"),
                InterpolationKind::SolarBackwardsAveraged { scalefactor: 1.0 }
            ));
            assert!(matches!(
                interpolation_kind_for_variable("snow_depth"),
                InterpolationKind::Hermite {
                    scalefactor: 100.0,
                    bounds: None
                }
            ));
            assert!(matches!(
                interpolation_kind_for_variable("relative_humidity_850hPa"),
                InterpolationKind::Hermite {
                    bounds: Some((0.0, 100.0)),
                    ..
                }
            ));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn ecmwf_solar_interpolation_uses_backwards_average_centres() {
        let start = Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap();
        let native = (0..4)
            .map(|index| start + Duration::hours(index * 3))
            .collect::<Vec<_>>();
        let at_native_time = solar_interpolation_position(&native, start, 3600).unwrap();
        assert_eq!((at_native_time.b, at_native_time.c), (0, 1));
        assert!((at_native_time.fraction - 1.0 / 3.0).abs() < 1e-6);
        let two_hours_later =
            solar_interpolation_position(&native, start + Duration::hours(2), 3600).unwrap();
        assert_eq!((two_hours_later.b, two_hours_later.c), (1, 2));
        assert_eq!(two_hours_later.fraction, 0.0);
    }

    #[test]
    fn solar_interpolation_preserves_normalized_factor_at_integer_boundary() {
        // Frozen p0076 inputs at 2026-08-02T11:00Z. Multiplying the normalized
        // solar factor by SOLAR_CONSTANT and dividing it back loses one ULP and
        // incorrectly rounds the public value to 367 instead of the official 366.
        let native_times = [
            Utc.with_ymd_and_hms(2026, 8, 2, 9, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 2, 12, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 2, 15, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 2, 18, 0, 0).unwrap(),
        ];
        let value = interpolate_solar_backwards_value(
            [772.0, 362.0, 19.0, 0.0],
            &native_times,
            SolarInterpolationPosition {
                a: 0,
                b: 1,
                c: 2,
                d: 3,
                fraction: 0.0,
            },
            Utc.with_ymd_and_hms(2026, 8, 2, 11, 0, 0).unwrap(),
            44.0,
            93.75,
            1.0,
            true,
            3600,
        );
        assert_eq!(value, 366.0);
    }

    #[test]
    fn ecmwf_pressure_cloud_cover_matches_sundqvist_reference() {
        let cloud_cover = relative_humidity_to_cloud_cover(80.0, 500.0);
        assert!((cloud_cover - 18.35034).abs() < 0.0001);
        assert_eq!(relative_humidity_to_cloud_cover(80.0, 1000.0), 0.0);
    }
}

fn ensure_in_selection(entry: &BundleEntry, y: u64, x: u64) -> Result<()> {
    if entry.selection_ranges.len() != 2 {
        bail!("only 2D selection ranges are supported");
    }
    let y_range = entry.selection_ranges[0];
    let x_range = entry.selection_ranges[1];
    if y < y_range[0] || y >= y_range[1] || x < x_range[0] || x >= x_range[1] {
        bail!(
            "point is outside downloaded product coverage for variable {}",
            entry.variable
        );
    }
    Ok(())
}

fn read_uncompressed_point(
    product: &ProductSnapshot,
    entry: &BundleEntry,
    y: u64,
    x: u64,
) -> Result<f32> {
    if entry.array.data_type != 20 {
        bail!("uncompressed fallback only supports float arrays");
    }
    if entry.array.chunks != entry.array.dimensions {
        bail!("uncompressed fallback requires one full-array chunk");
    }
    let data_range = entry
        .data_byte_ranges
        .first()
        .context("entry has no data_byte_ranges")?;
    let nx = entry.array.dimensions[1];
    let offset = (y * nx + x) * 4;
    let original_start = data_range[0] + offset;
    let reader = entry_range_reader(product, entry)?;
    let bytes = reader.read_original_range(original_start, 4)?;
    Ok(f32::from_le_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

#[derive(Debug)]
struct EntryBundleReader {
    bundle_handle: Arc<File>,
    entry: BundleEntry,
    direct_file: bool,
}

impl EntryBundleReader {
    fn new(bundle_handle: Arc<File>, entry: BundleEntry) -> Self {
        Self {
            bundle_handle,
            entry,
            direct_file: false,
        }
    }

    fn direct(bundle_handle: Arc<File>, entry: BundleEntry) -> Self {
        Self {
            bundle_handle,
            entry,
            direct_file: true,
        }
    }
}

impl BundleRangeReader for EntryBundleReader {
    fn read_original_range(&self, start: u64, count: u64) -> Result<Vec<u8>> {
        if self.direct_file {
            let mut output = vec![0_u8; count as usize];
            self.bundle_handle.read_exact_at(&mut output, start)?;
            return Ok(output);
        }
        let end = start
            .checked_add(count)
            .ok_or_else(|| anyhow!("range overflow"))?;
        let mut remaining_start = start;
        let remaining_end = end;
        let mut out = Vec::with_capacity(count as usize);
        let mut local_cursor = self.entry.bundle_offset;
        for range in &self.entry.byte_ranges {
            let original_start = range[0];
            let original_end = range[1] + 1;
            let len = original_end - original_start;
            if remaining_start >= original_end || remaining_end <= original_start {
                local_cursor += len;
                continue;
            }
            let part_start = remaining_start.max(original_start);
            let part_end = remaining_end.min(original_end);
            if part_start > remaining_start {
                bail!("requested original range has a gap not present in bundle");
            }
            let local_offset = local_cursor + (part_start - original_start);
            let part_len = part_end - part_start;
            let before = out.len();
            out.resize(before + part_len as usize, 0);
            self.bundle_handle
                .read_exact_at(&mut out[before..], local_offset)?;
            remaining_start = part_end;
            if remaining_start == remaining_end {
                return Ok(out);
            }
            local_cursor += len;
        }
        bail!("requested original range is not present in bundle")
    }
}

fn entry_file_handle(product: &ProductSnapshot, entry: &BundleEntry) -> Result<Arc<File>> {
    if let Some(path) = &entry.native_file_path {
        return product
            .native_handles
            .get(path)
            .cloned()
            .with_context(|| format!("native OM handle is missing: {path}"));
    }
    Ok(product.bundle_handle.clone())
}

fn entry_range_reader(product: &ProductSnapshot, entry: &BundleEntry) -> Result<EntryBundleReader> {
    let handle = entry_file_handle(product, entry)?;
    Ok(if entry.native_file_path.is_some() {
        EntryBundleReader::direct(handle, entry.clone())
    } else {
        EntryBundleReader::new(handle, entry.clone())
    })
}

fn read_optional_direct_grid_series_rounded(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Option<Vec<Vec<f32>>>> {
    match read_direct_grid_series(
        snapshot, decoder, variable, times, latitudes, longitudes, true,
    ) {
        Ok(values) => Ok(Some(values)),
        Err(_) => Ok(None),
    }
}

fn point_weather_code_model_latitudes(latitudes: &[f64], longitudes: &[f64]) -> Option<Vec<f32>> {
    if latitudes.len() != 1 || longitudes.len() != 1 {
        return None;
    }
    current_product_sampling(current_weather_model().primary_product())
        .map(|sampling| vec![sampling.latitude as f32])
}

fn read_weather_code_grid_series(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<Vec<f32>>> {
    let cloudcover = read_direct_grid_series(
        snapshot,
        decoder,
        "cloud_cover",
        times,
        latitudes,
        longitudes,
        true,
    )?;
    let precipitation = read_direct_grid_series(
        snapshot,
        decoder,
        "precipitation",
        times,
        latitudes,
        longitudes,
        true,
    )?;
    let snowfall = read_direct_grid_series(
        snapshot,
        decoder,
        "snowfall_water_equivalent",
        times,
        latitudes,
        longitudes,
        true,
    )?;
    let showers = if current_weather_model() == WeatherModel::EcmwfIfs025 {
        None
    } else {
        Some(read_direct_grid_series(
            snapshot, decoder, "showers", times, latitudes, longitudes, true,
        )?)
    };
    let cape = read_optional_direct_grid_series_rounded(
        snapshot, decoder, "cape", times, latitudes, longitudes,
    )?;
    let ecmwf = current_weather_model() == WeatherModel::EcmwfIfs025;
    let gusts = if ecmwf {
        None
    } else {
        read_optional_direct_grid_series_rounded(
            snapshot,
            decoder,
            "wind_gusts_10m",
            times,
            latitudes,
            longitudes,
        )?
    };
    let visibility = if ecmwf {
        None
    } else {
        read_optional_direct_grid_series_rounded(
            snapshot,
            decoder,
            "visibility",
            times,
            latitudes,
            longitudes,
        )?
    };
    let freezing_rain = if ecmwf {
        None
    } else {
        read_optional_direct_grid_series_rounded(
            snapshot,
            decoder,
            "categorical_freezing_rain",
            times,
            latitudes,
            longitudes,
        )?
    };
    let lifted_index = if ecmwf {
        None
    } else {
        read_optional_direct_grid_series_rounded(
            snapshot,
            decoder,
            "lifted_index",
            times,
            latitudes,
            longitudes,
        )?
    };
    let cin = if ecmwf {
        None
    } else {
        read_optional_direct_grid_series_rounded(
            snapshot,
            decoder,
            "convective_inhibition",
            times,
            latitudes,
            longitudes,
        )?
    };
    let pbl = if ecmwf {
        None
    } else {
        read_optional_direct_grid_series_rounded(
            snapshot,
            decoder,
            "boundary_layer_height",
            times,
            latitudes,
            longitudes,
        )?
    };

    let (product_name, raw_variable) = product_for_variable(snapshot, "cloud_cover")?;
    let products = snapshot.product_snapshots(product_name);
    // The GFS data cadence becomes three-hourly after f120. The values above
    // are still interpolated to hourly frames, so derive the invariant grid
    // coordinates from any cloud-cover entry rather than requiring a raw
    // frame at a requested interpolated hour.
    let entry = products
        .iter()
        .find_map(|product| {
            product
                .entries
                .iter()
                .find(|(key, _)| key.variable == raw_variable)
                .map(|(_, entry)| entry)
        })
        .context("weather-code series has no cloud-cover grid entry")?;
    // The point time-slab path replaces each product's coordinates with the
    // cell selected by cell_selection. WeatherCode must receive that same
    // GFS013 model latitude; using the original request latitude can cross the
    // strict thunderstorm-probability threshold even though every input value
    // came from the selected model cell.
    let model_latitudes = match point_weather_code_model_latitudes(latitudes, longitudes) {
        Some(values) => values,
        None => latitudes
            .iter()
            .map(|latitude| {
                let (y, _) = grid_index_for_lat_lon(
                    &entry.array,
                    entry.native_grid.as_ref(),
                    *latitude,
                    longitudes[0],
                )?;
                grid_latitude_for_index(&entry.array, entry.native_grid.as_ref(), y)
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let width = longitudes.len();
    let mut output = Vec::with_capacity(times.len());
    for time_index in 0..times.len() {
        let mut values = Vec::with_capacity(cloudcover[time_index].len());
        for index in 0..cloudcover[time_index].len() {
            let optional = |series: &Option<Vec<Vec<f32>>>| {
                series.as_ref().map(|series| series[time_index][index])
            };
            values.push(
                weather_code(
                    cloudcover[time_index][index],
                    precipitation[time_index][index],
                    showers.as_ref().map(|showers| showers[time_index][index]),
                    snowfall[time_index][index] * 0.7,
                    optional(&gusts),
                    optional(&cape),
                    optional(&lifted_index),
                    optional(&cin),
                    optional(&pbl),
                    optional(&visibility),
                    optional(&freezing_rain),
                    3600,
                    model_latitudes[index / width],
                )
                .unwrap_or(f32::NAN),
            );
        }
        output.push(values);
    }
    Ok(output)
}

fn read_optional_direct_grid_rounded(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Option<Vec<f32>>> {
    match read_direct_grid(
        snapshot, decoder, variable, time, latitudes, longitudes, true,
    ) {
        Ok(values) => Ok(Some(values)),
        Err(_) => Ok(None),
    }
}

fn read_weather_code_grid(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<f32>> {
    let cloudcover = read_direct_grid(
        snapshot,
        decoder,
        "cloud_cover",
        time,
        latitudes,
        longitudes,
        true,
    )?;
    let precipitation = read_direct_grid(
        snapshot,
        decoder,
        "precipitation",
        time,
        latitudes,
        longitudes,
        true,
    )?;
    let snowfall = read_direct_grid(
        snapshot,
        decoder,
        "snowfall_water_equivalent",
        time,
        latitudes,
        longitudes,
        true,
    )?;
    let showers = if current_weather_model() == WeatherModel::EcmwfIfs025 {
        None
    } else {
        Some(read_direct_grid(
            snapshot, decoder, "showers", time, latitudes, longitudes, true,
        )?)
    };
    let cape =
        read_optional_direct_grid_rounded(snapshot, decoder, "cape", time, latitudes, longitudes)?;
    let ecmwf = current_weather_model() == WeatherModel::EcmwfIfs025;
    let gusts = if ecmwf {
        None
    } else {
        read_optional_direct_grid_rounded(
            snapshot,
            decoder,
            "wind_gusts_10m",
            time,
            latitudes,
            longitudes,
        )?
    };
    let visibility = if ecmwf {
        None
    } else {
        read_optional_direct_grid_rounded(
            snapshot,
            decoder,
            "visibility",
            time,
            latitudes,
            longitudes,
        )?
    };
    let freezing_rain = if ecmwf {
        None
    } else {
        read_optional_direct_grid_rounded(
            snapshot,
            decoder,
            "categorical_freezing_rain",
            time,
            latitudes,
            longitudes,
        )?
    };
    let lifted_index = if ecmwf {
        None
    } else {
        read_optional_direct_grid_rounded(
            snapshot,
            decoder,
            "lifted_index",
            time,
            latitudes,
            longitudes,
        )?
    };
    let cin = if ecmwf {
        None
    } else {
        read_optional_direct_grid_rounded(
            snapshot,
            decoder,
            "convective_inhibition",
            time,
            latitudes,
            longitudes,
        )?
    };
    let pbl = if ecmwf {
        None
    } else {
        read_optional_direct_grid_rounded(
            snapshot,
            decoder,
            "boundary_layer_height",
            time,
            latitudes,
            longitudes,
        )?
    };
    let (product_name, raw_variable) = product_for_variable(snapshot, "cloud_cover")?;
    let products = snapshot.product_snapshots(product_name);
    let entry = products
        .iter()
        .find_map(|product| {
            product.entries.get(&EntryKey {
                variable: raw_variable.clone(),
                valid_time_utc: time,
            })
        })
        .with_context(|| format!("variable/time is not available: cloud_cover {}", time))?;
    let model_latitudes = match point_weather_code_model_latitudes(latitudes, longitudes) {
        Some(values) => values,
        None => latitudes
            .iter()
            .map(|latitude| {
                let (y, _) = grid_index_for_lat_lon(
                    &entry.array,
                    entry.native_grid.as_ref(),
                    *latitude,
                    longitudes[0],
                )?;
                grid_latitude_for_index(&entry.array, entry.native_grid.as_ref(), y)
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let width = longitudes.len();
    let mut values = Vec::with_capacity(cloudcover.len());
    for index in 0..cloudcover.len() {
        let optional = |values: &Option<Vec<f32>>| values.as_ref().map(|values| values[index]);
        values.push(
            weather_code(
                cloudcover[index],
                precipitation[index],
                showers.as_ref().map(|showers| showers[index]),
                snowfall[index] * 0.7,
                optional(&gusts),
                optional(&cape),
                optional(&lifted_index),
                optional(&cin),
                optional(&pbl),
                optional(&visibility),
                optional(&freezing_rain),
                3600,
                model_latitudes[index / width],
            )
            .unwrap_or(f32::NAN),
        );
    }
    Ok(values)
}

fn read_weather_code(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let model_latitude =
        model_latitude_for_variable(snapshot, "cloud_cover", time, latitude, longitude)?;
    let cloudcover = read_direct(snapshot, decoder, "cloud_cover", time, latitude, longitude)?;
    let precipitation = read_direct(
        snapshot,
        decoder,
        "precipitation",
        time,
        latitude,
        longitude,
    )?;
    let snowfall = read_direct(
        snapshot,
        decoder,
        "snowfall_water_equivalent",
        time,
        latitude,
        longitude,
    )? * 0.7;
    let showers = if current_weather_model() == WeatherModel::EcmwfIfs025 {
        None
    } else {
        Some(read_direct(
            snapshot, decoder, "showers", time, latitude, longitude,
        )?)
    };
    let cape = read_optional_direct(snapshot, decoder, "cape", time, latitude, longitude)?;
    let ecmwf = current_weather_model() == WeatherModel::EcmwfIfs025;
    let gusts = if ecmwf {
        None
    } else {
        read_optional_direct(
            snapshot,
            decoder,
            "wind_gusts_10m",
            time,
            latitude,
            longitude,
        )?
    };
    let visibility = if ecmwf {
        None
    } else {
        read_optional_direct(snapshot, decoder, "visibility", time, latitude, longitude)?
    };
    let freezing_rain = if ecmwf {
        None
    } else {
        read_optional_direct(
            snapshot,
            decoder,
            "categorical_freezing_rain",
            time,
            latitude,
            longitude,
        )?
    };
    let lifted_index = if ecmwf {
        None
    } else {
        read_optional_direct(snapshot, decoder, "lifted_index", time, latitude, longitude)?
    };
    let cin = if ecmwf {
        None
    } else {
        read_optional_direct(
            snapshot,
            decoder,
            "convective_inhibition",
            time,
            latitude,
            longitude,
        )?
    };
    let pbl = if ecmwf {
        None
    } else {
        read_optional_direct(
            snapshot,
            decoder,
            "boundary_layer_height",
            time,
            latitude,
            longitude,
        )?
    };
    Ok(weather_code(
        cloudcover,
        precipitation,
        showers,
        snowfall,
        gusts,
        cape,
        lifted_index,
        cin,
        pbl,
        visibility,
        freezing_rain,
        3600,
        model_latitude,
    )
    .unwrap_or(f32::NAN))
}

#[allow(clippy::too_many_arguments)]
pub fn weather_code(
    cloudcover: f32,
    precipitation: f32,
    convective_precipitation: Option<f32>,
    snowfall_centimeters: f32,
    gusts: Option<f32>,
    cape: Option<f32>,
    lifted_index: Option<f32>,
    convective_inhibition: Option<f32>,
    pbl_height: Option<f32>,
    visibility_meters: Option<f32>,
    categorical_freezing_rain: Option<f32>,
    model_dt_seconds: i32,
    latitude: f32,
) -> Option<f32> {
    if !cloudcover.is_finite() || !precipitation.is_finite() || !snowfall_centimeters.is_finite() {
        return None;
    }
    let model_dt_hours = model_dt_seconds as f32 / 3600.0;
    if let Some(cape_value) = cape {
        // Exact port of Open-Meteo WeatherCode.swift at 3a64572c7797738300a5d1d87081a7cbd8f35b3c.
        let thunderstorms = thunderstorm_probability(
            convective_precipitation,
            precipitation,
            cloudcover,
            gusts,
            cape_value,
            lifted_index,
            convective_inhibition,
            pbl_height,
            model_dt_seconds,
            latitude,
        );
        if thunderstorms > 85.0 {
            return Some(96.0);
        }
        if thunderstorms > 60.0 {
            return Some(95.0);
        }
    }
    if categorical_freezing_rain.unwrap_or(0.0) >= 1.0 {
        match precipitation / model_dt_hours {
            x if (0.01..0.5).contains(&x) => return Some(56.0),
            x if (0.5..1.3).contains(&x) => return Some(57.0),
            x if (1.3..2.5).contains(&x) => return Some(66.0),
            x if x >= 2.5 => return Some(67.0),
            _ => {}
        }
    }
    if convective_precipitation.unwrap_or(0.0) > 0.0 || cape.unwrap_or(0.0) >= 800.0 {
        match snowfall_centimeters / model_dt_hours {
            x if (0.01..0.8).contains(&x) => return Some(85.0),
            x if x >= 0.8 => return Some(86.0),
            _ => {}
        }
        match precipitation / model_dt_hours {
            x if (1.3..2.5).contains(&x) => return Some(80.0),
            x if (2.5..7.6).contains(&x) => return Some(81.0),
            x if x >= 7.6 => return Some(82.0),
            _ => {}
        }
    }
    match snowfall_centimeters / model_dt_hours {
        x if (0.01..0.2).contains(&x) => return Some(71.0),
        x if (0.2..0.8).contains(&x) => return Some(73.0),
        x if x >= 0.8 => return Some(75.0),
        _ => {}
    }
    match precipitation / model_dt_hours {
        x if (0.01..0.5).contains(&x) => return Some(51.0),
        x if (0.5..1.0).contains(&x) => return Some(53.0),
        x if (1.0..1.3).contains(&x) => return Some(55.0),
        x if (1.3..2.5).contains(&x) => return Some(61.0),
        x if (2.5..7.6).contains(&x) => return Some(63.0),
        x if x >= 7.6 => return Some(65.0),
        _ => {}
    }
    if visibility_meters.is_some_and(|value| value <= 1000.0) {
        return Some(45.0);
    }
    match cloudcover {
        x if (0.0..20.0).contains(&x) => Some(0.0),
        x if (20.0..50.0).contains(&x) => Some(1.0),
        x if (50.0..80.0).contains(&x) => Some(2.0),
        x if x >= 80.0 => Some(3.0),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn thunderstorm_probability(
    convective_precipitation: Option<f32>,
    precipitation: f32,
    cloudcover: f32,
    gusts: Option<f32>,
    cape: f32,
    lifted_index: Option<f32>,
    convective_inhibition: Option<f32>,
    pbl_height: Option<f32>,
    model_dt_seconds: i32,
    latitude: f32,
) -> f32 {
    if cape <= 10.0 {
        return 0.0;
    }
    if cloudcover < 30.0 {
        return 0.0;
    }
    if convective_inhibition.is_some_and(|value| value > 250.0) {
        return 0.0;
    }
    if lifted_index.is_some_and(|value| value > 2.0) {
        return 0.0;
    }
    let abs_lat = latitude.abs();
    let latitude_factor = if abs_lat >= 30.0 {
        1.0
    } else {
        0.8 + (0.2 * (abs_lat / 30.0))
    };
    let mut accumulated_score = 0.0;
    let mut total_weight = 0.0;

    let cape_weight = 0.25;
    let max_cape_threshold = 2500.0 + (1500.0 * (1.0 - (abs_lat.min(30.0) / 30.0)));
    let cape_score = ((cape - 300.0) / (max_cape_threshold - 300.0)).clamp(0.0, 1.0);
    accumulated_score += cape_score * cape_weight;
    total_weight += cape_weight;

    if let Some(cin) = convective_inhibition {
        let cin_weight = 0.15;
        let cin_score = if cin <= 15.0 {
            1.0
        } else {
            (1.0 - ((cin - 15.0) / 135.0)).clamp(0.0, 1.0)
        };
        accumulated_score += cin_score * cin_weight;
        total_weight += cin_weight;
    }
    if let Some(li) = lifted_index {
        let li_weight = 0.15;
        let li_score = ((0.0 - li) / 8.0).clamp(0.0, 1.0);
        accumulated_score += li_score * li_weight;
        total_weight += li_weight;
    }
    let dt_hours = model_dt_seconds as f32 / 3600.0;
    let reference_precip_per_hour = 2.0 + (3.0 * (1.0 - (abs_lat.min(30.0) / 30.0)));
    let reference_precip = reference_precip_per_hour * dt_hours;
    let precip_weight = 0.25;
    if let Some(showers) = convective_precipitation.filter(|value| *value > 0.0) {
        let precip_score = (showers / reference_precip).clamp(0.0, 1.0);
        accumulated_score += precip_score * precip_weight;
        total_weight += precip_weight;
    } else {
        let fallback_reference_precip = reference_precip * 1.6;
        let fallback_precip_score = (precipitation / fallback_reference_precip).clamp(0.0, 1.0);
        accumulated_score += fallback_precip_score * precip_weight * 0.6;
        total_weight += precip_weight * 0.6;
    }
    if let Some(pbl) = pbl_height {
        let pbl_weight = 0.075;
        let pbl_score = ((pbl - 300.0) / 1200.0).clamp(0.0, 1.0);
        accumulated_score += pbl_score * pbl_weight;
        total_weight += pbl_weight;
    }
    if let Some(gust) = gusts {
        let gust_weight = 0.075;
        let gust_score = ((gust - 5.0) / 13.0).clamp(0.0, 1.0);
        accumulated_score += gust_score * gust_weight;
        total_weight += gust_weight;
    }
    let mut base_probability = (accumulated_score / total_weight) * 100.0;
    if let (Some(precip), Some(cin)) = (convective_precipitation, convective_inhibition) {
        let trigger_rain_threshold = 0.1 * dt_hours;
        if precip > trigger_rain_threshold && cape > 300.0 && cin < 50.0 {
            base_probability = (base_probability * 1.3).min(100.0);
        }
    }
    if convective_precipitation.unwrap_or(precipitation) <= 0.0 {
        base_probability *= 0.7;
    }
    if convective_inhibition.is_some_and(|cin| cin > 100.0) {
        base_probability *= 0.3;
    }
    let cloud_cover_factor = if cloudcover >= 60.0 {
        1.0
    } else {
        0.6 + (0.4 * ((cloudcover - 30.0) / 30.0))
    };
    (base_probability * cloud_cover_factor * latitude_factor).clamp(0.0, 100.0)
}

unsafe extern "C" {
    fn om_windirection_fast(num_points: usize, ys: *const f32, xs: *const f32, out: *mut f32);
}

/// Call Open-Meteo's bulk C implementation without splitting the input into
/// scalar operations. The official helper is intentionally array-oriented.
fn wind_directions(u: &[f32], v: &[f32]) -> Vec<f32> {
    assert_eq!(u.len(), v.len(), "wind component lengths must match");
    let mut out = vec![f32::NAN; u.len()];
    if !u.is_empty() {
        // SAFETY: all pointers remain valid for exactly `u.len()` elements and
        // the output buffer is uniquely owned for the duration of the call.
        unsafe {
            om_windirection_fast(u.len(), u.as_ptr(), v.as_ptr(), out.as_mut_ptr());
        }
    }
    out
}

fn wind_direction(u: f32, v: f32) -> f32 {
    wind_directions(&[u], &[v])[0]
}

/// `read_variable_grid_series` stores values as `[time][point]`, while the
/// official point reader evaluates the complete time vector for each point.
fn wind_direction_grid_series(u: &[Vec<f32>], v: &[Vec<f32>]) -> Vec<Vec<f32>> {
    assert_eq!(u.len(), v.len(), "wind time dimensions must match");
    let Some(first) = u.first() else {
        return Vec::new();
    };
    let width = first.len();
    assert!(
        u.iter().chain(v).all(|frame| frame.len() == width),
        "wind grid dimensions must match"
    );

    let mut out = vec![vec![f32::NAN; width]; u.len()];
    let mut point_u = Vec::with_capacity(u.len());
    let mut point_v = Vec::with_capacity(v.len());
    for point in 0..width {
        point_u.clear();
        point_v.clear();
        point_u.extend(u.iter().map(|frame| frame[point]));
        point_v.extend(v.iter().map(|frame| frame[point]));
        for (time, direction) in wind_directions(&point_u, &point_v).into_iter().enumerate() {
            out[time][point] = direction;
        }
    }
    out
}

/// Reproduce `GenericDailyCalculator.dominantDirection` from the pinned
/// Open-Meteo source. The official path derives speed and direction for every
/// model frame, reconstructs the two components in single precision, sums
/// those reconstructed components, and only then evaluates the fast angle.
fn dominant_wind_direction(speed: &[f32], direction: &[f32]) -> f32 {
    debug_assert_eq!(speed.len(), direction.len());
    let u = speed
        .iter()
        .zip(direction)
        .map(|(&speed, &direction)| -speed * (direction * SWIFT_FLOAT_PI / 180.0).sin())
        .sum::<f32>();
    let v = speed
        .iter()
        .zip(direction)
        .map(|(&speed, &direction)| -speed * (direction * SWIFT_FLOAT_PI / 180.0).cos())
        .sum::<f32>();
    wind_direction(u, v)
}

fn wind_scale_factor(from: f32, to: f32) -> f32 {
    let factor_from = 4.87 / (67.8 * from - 5.42).ln();
    let factor_to = 4.87 / (67.8 * to - 5.42).ln();
    factor_from / factor_to
}

fn weather_model_sampling(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
) -> Result<ModelSampling> {
    let model = current_weather_model();
    if let Some(sampling) = current_product_sampling(model.primary_product()) {
        return Ok(sampling);
    }
    let (latitude, longitude, elevation) =
        weather_model_location(snapshot, decoder, latitude, longitude)?
            .with_context(|| format!("{} sampling is not available", model.display_name()))?;
    Ok(ModelSampling {
        latitude,
        longitude,
        model_elevation: elevation,
        target_elevation: elevation,
    })
}

fn precip_phase(code: f32) -> f32 {
    match code as i32 {
        51 | 53 | 55 | 61 | 63 | 65 | 80 | 81 | 82 => 1.0,
        71 | 73 | 75 | 77 | 85 | 86 => 2.0,
        56 | 57 | 66 | 67 => 4.0,
        _ => 0.0,
    }
}

/// Open-Meteo/ECMWF Sundqvist cloud-cover parameterisation used when IFS
/// exposes relative humidity, but no cloud-cover field, on pressure levels.
fn relative_humidity_to_cloud_cover(relative_humidity: f32, pressure_hpa: f32) -> f32 {
    if !relative_humidity.is_finite() || !pressure_hpa.is_finite() || pressure_hpa <= 0.0 {
        return f32::NAN;
    }
    let rh_critical = 0.7 + (0.9 - 0.7) * (1.0 - (1013.25_f32 / pressure_hpa).powf(4.0)).exp();
    (1.0 - ((1.0 - relative_humidity / 100.0).max(0.0) / (1.0 - rh_critical)).sqrt()).max(0.0)
        * 100.0
}

fn dew_point(temperature: f32, relative_humidity: f32) -> f32 {
    let beta = 17.625_f32;
    let lambda = 243.04_f32;
    let log_relative_humidity = (relative_humidity / 100.0).ln();
    let temperature_term = (beta * temperature) / (lambda + temperature);
    // Keep the same single-precision operation order as
    // Meteorology.dewpoint in the pinned Open-Meteo Swift source. Combining
    // the two denominator terms first is mathematically equivalent, but can
    // cross a public two-decimal rounding boundary in downstream VPD.
    lambda * (log_relative_humidity + temperature_term)
        / (beta - log_relative_humidity - temperature_term)
}

fn wet_bulb_temperature(temperature: f32, relative_humidity: f32) -> f32 {
    let wet = temperature * (0.151977 * (relative_humidity + 8.313659).sqrt()).atan()
        + (temperature + relative_humidity).atan()
        - (relative_humidity - 1.676331).atan()
        + 0.00391838 * relative_humidity.powf(1.5) * (0.023101 * relative_humidity).atan()
        - 4.686035;
    wet.min(temperature)
}

fn evapotranspiration(latent_heat_flux: f32) -> f32 {
    (latent_heat_flux * -3600.0 / 2.5e6).max(0.0)
}

fn vapor_pressure_deficit(temperature: f32, dewpoint: f32) -> f32 {
    let saturated = 0.6108 * ((17.27 * temperature) / (temperature + 237.3)).exp();
    let actual = 0.6108 * ((17.27 * dewpoint) / (dewpoint + 237.3)).exp();
    (saturated - actual).max(0.0)
}

fn weighted_soil_layer_0_to_100cm(shallow: f32, middle: f32, deep: f32) -> f32 {
    shallow * 0.07 + middle * (0.28 - 0.07) + deep * (1.0 - 0.28)
}

#[allow(clippy::too_many_arguments)]
fn et0_evapotranspiration(
    temperature: f32,
    wind_speed_10m: f32,
    dewpoint: f32,
    shortwave_radiation: f32,
    elevation: f32,
    extraterrestrial_radiation: f32,
    dt_seconds: i64,
) -> f32 {
    if !temperature.is_finite()
        || !wind_speed_10m.is_finite()
        || !dewpoint.is_finite()
        || !shortwave_radiation.is_finite()
        || !elevation.is_finite()
        || !extraterrestrial_radiation.is_finite()
    {
        return f32::NAN;
    }
    let wind_speed_2m = wind_scale_factor(10.0, 2.0) * wind_speed_10m;
    let beta = 17.27_f32;
    let lambda = 237.3_f32;
    let slope = 4098.0 * (0.6108 * (beta * temperature / (temperature + lambda)).exp())
        / (temperature + lambda).powi(2);
    let atmospheric_pressure = 101.3 * ((293.0 - 0.0065 * elevation) / 293.0).powf(5.26);
    let psychrometric = 0.000665 * atmospheric_pressure;
    let saturated = 0.6108 * ((beta * temperature) / (temperature + lambda)).exp();
    let actual = 0.6108 * ((beta * dewpoint) / (dewpoint + lambda)).exp();
    let deficit = saturated - actual;
    let net_shortwave = shortwave_radiation * (1.0 - 0.23) * 0.0864 / 24.0;
    let clear_sky = (0.75 + 0.00002 * elevation) * extraterrestrial_radiation;
    let relative_humidity = (100.0 * ((17.625 * dewpoint) / (243.04 + dewpoint)).exp()
        / ((17.625 * temperature) / (243.04 + temperature)).exp())
    .clamp(0.0, 100.0);
    let relative_approximation = 0.4 + relative_humidity / 100.0 * 0.4;
    let relative_radiation = if extraterrestrial_radiation <= 0.0 {
        relative_approximation
    } else {
        (shortwave_radiation / clear_sky).min(1.0)
    };
    let net_longwave = 0.20429166e-9
        * (temperature + 273.16).powi(4)
        * (0.34 - 0.14 * actual.sqrt())
        * (1.35 * relative_radiation - 0.35);
    let net_radiation = net_shortwave - net_longwave;
    let soil_heat_flux = if shortwave_radiation <= 0.0 {
        0.5 * net_radiation
    } else {
        0.1 * net_radiation
    };
    let et0 = (0.408 * slope * (net_radiation - soil_heat_flux)
        + psychrometric * (37.0 / (temperature + 273.0)) * wind_speed_2m * deficit)
        / (slope + psychrometric * (1.0 + 0.34 * wind_speed_2m));
    (et0 * (dt_seconds / 3600) as f32).max(0.0)
}

fn apparent_temperature(
    temperature_2m: f32,
    relative_humidity_2m: f32,
    wind_speed_10m: f32,
    shortwave_radiation: Option<f32>,
) -> f32 {
    let wind_speed_2m = wind_speed_10m * 0.75;
    let vapor_pressure = relative_humidity_2m / 100.0
        * 6.105
        * (17.27 * temperature_2m / (237.7 + temperature_2m)).exp();
    let radiation = (0.1 * (shortwave_radiation.unwrap_or(550.0) - 550.0)).max(0.0);
    temperature_2m + 0.348 * vapor_pressure - 0.70 * wind_speed_2m
        + 0.70 * (radiation / (wind_speed_2m + 10.0))
        - 4.25
}

fn unit_for_variable(variable: &str) -> &'static str {
    if variable.ends_with("hPa") {
        if variable.starts_with("temperature_") {
            return "°C";
        }
        if variable.starts_with("dew_point_") || variable.starts_with("dewpoint_") {
            return "°C";
        }
        if variable.starts_with("wind_speed_") || variable.starts_with("windspeed_") {
            return "m/s";
        }
        if variable.starts_with("wind_direction_") || variable.starts_with("winddirection_") {
            return "°";
        }
        if variable.starts_with("relative_humidity_") || variable.starts_with("cloud_cover_") {
            return "%";
        }
        if variable.starts_with("wind_u_component_")
            || variable.starts_with("wind_v_component_")
            || variable.starts_with("vertical_velocity_")
        {
            return "m/s";
        }
        if variable.starts_with("geopotential_height_") {
            return "m";
        }
    }
    match variable {
        "temperature_2m"
        | "temperature_2m_min"
        | "temperature_2m_max"
        | "apparent_temperature"
        | "temperature_80m"
        | "temperature_100m"
        | "temperature_120m"
        | "dew_point_2m"
        | "dewpoint_2m"
        | "wet_bulb_temperature_2m"
        | "surface_temperature"
        | "skin_temperature"
        | "soil_temperature_0cm"
        | "soil_temperature_0_to_10cm"
        | "soil_temperature_0_to_7cm"
        | "soil_temperature_7_to_28cm"
        | "soil_temperature_28_to_100cm"
        | "soil_temperature_100_to_255cm"
        | "soil_temperature_0_to_100cm"
        | "soil_temperature_10_to_40cm"
        | "soil_temperature_40_to_100cm"
        | "soil_temperature_100_to_200cm" => "°C",
        "relative_humidity_2m"
        | "relativehumidity_2m"
        | "cloud_cover"
        | "cloudcover"
        | "cloud_cover_low"
        | "cloudcover_low"
        | "cloud_cover_mid"
        | "cloudcover_mid"
        | "cloud_cover_high"
        | "cloudcover_high"
        | "precipitation_probability"
        | "precipitation_probability_max"
        | "precipitation_probability_mean"
        | "precipitation_probability_min" => "%",
        "precipitation"
        | "showers"
        | "rain"
        | "snowfall_water_equivalent"
        | "snow_depth_water_equivalent"
        | "runoff" => "mm",
        "snowfall" => "cm",
        "wind_u_component_10m"
        | "wind_v_component_10m"
        | "wind_u_component_80m"
        | "wind_v_component_80m"
        | "wind_u_component_100m"
        | "wind_v_component_100m"
        | "wind_speed_10m"
        | "windspeed_10m"
        | "wind_speed_80m"
        | "windspeed_80m"
        | "wind_speed_100m"
        | "windspeed_100m"
        | "wind_speed_120m"
        | "windspeed_120m"
        | "wind_gusts_10m" => "m/s",
        "wind_direction_10m"
        | "winddirection_10m"
        | "wind_direction_80m"
        | "winddirection_80m"
        | "wind_direction_100m"
        | "winddirection_100m"
        | "wind_direction_120m"
        | "winddirection_120m" => "°",
        "pressure_msl" | "surface_pressure" => "hPa",
        "visibility" => "m",
        "freezing_level_height" | "boundary_layer_height" | "snow_depth" => "m",
        "weather_code" | "weathercode" => "wmo code",
        "precipitation_type" => "",
        "precip_phase" | "thunderstorm_code" => "",
        "pm2_5"
        | "pm10"
        | "dust"
        | "carbon_monoxide"
        | "nitrogen_dioxide"
        | "ozone"
        | "sulphur_dioxide"
        | "pm2_5_mean"
        | "pm10_mean"
        | "nitrogen_dioxide_mean"
        | "ozone_maximum_8h_mean"
        | "sulphur_dioxide_mean" => "μg/m³",
        "carbon_monoxide_mean" => "mg/m³",
        "aerosol_optical_depth" => "",
        "european_aqi"
        | "european_aqi_pm2_5"
        | "european_aqi_pm10"
        | "european_aqi_no2"
        | "european_aqi_o3"
        | "european_aqi_so2"
        | "european_aqi_nitrogen_dioxide"
        | "european_aqi_ozone"
        | "european_aqi_sulphur_dioxide" => "EAQI",
        "us_aqi"
        | "us_aqi_pm2_5"
        | "us_aqi_pm10"
        | "us_aqi_no2"
        | "us_aqi_o3"
        | "us_aqi_so2"
        | "us_aqi_co"
        | "us_aqi_nitrogen_dioxide"
        | "us_aqi_ozone"
        | "us_aqi_sulphur_dioxide"
        | "us_aqi_carbon_monoxide" => "USAQI",
        "chinese_aqi"
        | "chinese_aqi_pm2_5"
        | "chinese_aqi_pm10"
        | "chinese_aqi_no2"
        | "chinese_aqi_o3"
        | "chinese_aqi_so2"
        | "chinese_aqi_co"
        | "chinese_aqi_nitrogen_dioxide"
        | "chinese_aqi_ozone"
        | "chinese_aqi_sulphur_dioxide"
        | "chinese_aqi_carbon_monoxide" => "Chinese AQI",
        "uv_index"
        | "uv_index_clear_sky"
        | "lifted_index"
        | "categorical_freezing_rain"
        | "is_day" => "",
        "cape" | "convective_inhibition" => "J/kg",
        "shortwave_radiation"
        | "diffuse_radiation"
        | "direct_radiation"
        | "shortwave_radiation_instant"
        | "diffuse_radiation_instant"
        | "direct_radiation_instant"
        | "direct_normal_irradiance"
        | "direct_normal_irradiance_instant"
        | "global_tilted_irradiance"
        | "global_tilted_irradiance_instant"
        | "latent_heat_flux"
        | "sensible_heat_flux" => "W/m\u{00B2}",
        "sunshine_duration" => "s",
        "evapotranspiration" | "et0_fao_evapotranspiration" => "mm",
        "vapour_pressure_deficit" | "vapor_pressure_deficit" => "kPa",
        "soil_moisture_0_to_10cm"
        | "soil_moisture_0_to_7cm"
        | "soil_moisture_7_to_28cm"
        | "soil_moisture_28_to_100cm"
        | "soil_moisture_100_to_255cm"
        | "soil_moisture_0_to_100cm"
        | "soil_moisture_10_to_40cm"
        | "soil_moisture_40_to_100cm"
        | "soil_moisture_100_to_200cm" => "m\u{00B3}/m\u{00B3}",
        "total_column_integrated_water_vapour" => "kg/m\u{00B2}",
        "growing_degree_days_base_0_limit_50" => "GGDc",
        _ => "unknown",
    }
}

fn json_array_for_daily_variable(
    variable: &str,
    aggregation: DailyWeatherAggregation,
    values: Vec<f32>,
) -> serde_json::Value {
    if matches!(aggregation, DailyWeatherAggregation::RadiationSum(_)) {
        return serde_json::Value::Array(
            values
                .into_iter()
                .map(|value| json_value_with_decimals(value, 2))
                .collect(),
        );
    }
    let decimals = match variable {
        "wind_gusts_10m_mean" | "windgusts_10m_mean" | "visibility_mean" => Some(2),
        _ => None,
    };
    match decimals {
        Some(decimals) => serde_json::Value::Array(
            values
                .into_iter()
                .map(|value| json_value_with_decimals(value, decimals))
                .collect(),
        ),
        None => json_array_for_variable(aggregation.output_variable(), values),
    }
}

fn json_value_with_decimals(value: f32, decimals: u8) -> serde_json::Value {
    if !value.is_finite() {
        return serde_json::Value::Null;
    }
    let factor = 10_f32.powi(decimals as i32);
    serde_json::json!(((value * factor).round() as i64) as f64 / factor as f64)
}

fn json_array_for_variable(variable: &str, values: Vec<f32>) -> serde_json::Value {
    serde_json::Value::Array(
        values
            .into_iter()
            .map(|value| json_value_for_variable(variable, value))
            .collect(),
    )
}

fn json_value_for_variable(variable: &str, value: f32) -> serde_json::Value {
    if !value.is_finite() {
        return serde_json::Value::Null;
    }
    match output_decimals_for_variable(variable) {
        OutputDecimals::Integer => serde_json::json!(value.round() as i64),
        OutputDecimals::Fixed(decimals) => {
            let factor = 10_f32.powi(decimals as i32);
            let abs_value = if value < 0.0 { -value } else { value };
            let scaled = (abs_value * factor).round() as i64;
            let rounded = scaled as f64 / factor as f64;
            let rounded = if value < 0.0 { -rounded } else { rounded };
            serde_json::json!(rounded)
        }
    }
}

pub fn round_variable_output_value(variable: &str, value: f32) -> f32 {
    if !value.is_finite() {
        return value;
    }
    match output_decimals_for_variable(variable) {
        OutputDecimals::Integer => value.round(),
        OutputDecimals::Fixed(decimals) => {
            let factor = 10_f32.powi(decimals as i32);
            let abs_value = if value < 0.0 { -value } else { value };
            let rounded = (abs_value * factor).round() / factor;
            if value < 0.0 {
                -rounded
            } else {
                rounded
            }
        }
    }
}

enum OutputDecimals {
    Integer,
    Fixed(u8),
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn growing_degree_days_uses_official_api_unit_token() {
        assert_eq!(
            unit_for_variable("growing_degree_days_base_0_limit_50"),
            "GGDc"
        );
    }

    #[test]
    fn growing_degree_days_use_official_two_decimal_precision() {
        assert_eq!(
            json_array_for_variable("growing_degree_days_base_0_limit_50", vec![1.1375],),
            serde_json::json!([1.14]),
        );
        assert_eq!(
            json_array_for_daily_variable(
                "growing_degree_days_base_0_limit_50",
                DailyWeatherAggregation::Sum("growing_degree_days_base_0_limit_50",),
                vec![17.006],
            ),
            serde_json::json!([17.01]),
        );
    }

    #[test]
    fn daylight_duration_uses_official_two_decimal_precision() {
        assert_eq!(
            json_array_for_daily_variable(
                "daylight_duration",
                DailyWeatherAggregation::DaylightDuration,
                vec![43_625.707],
            ),
            serde_json::json!([43625.71]),
        );
    }

    #[test]
    fn precipitation_type_uses_official_dimensionless_json_precision() {
        assert_eq!(
            json_array_for_variable("precipitation_type", vec![1.0, 0.0]).to_string(),
            "[1.0,0.0]"
        );
    }

    #[test]
    fn public_hourly_scope_keeps_core_outputs_and_hides_internal_inputs() {
        for variable in [
            "temperature_2m",
            "apparent_temperature",
            "shortwave_radiation",
            "shortwave_radiation_instant",
            "sunshine_duration",
            "et0_fao_evapotranspiration",
            "vapour_pressure_deficit",
            "vapor_pressure_deficit",
            "uv_index",
            "is_day",
            "wind_speed_850hPa",
            "vertical_velocity_500hPa",
            "chinese_aqi_pm2_5",
            "soil_moisture_0_to_10cm",
            "soil_temperature_0_to_10cm",
        ] {
            assert!(is_public_hourly_variable(variable), "{variable}");
        }
        for variable in [
            "direct_normal_irradiance",
            "wind_u_component_10m",
            "wind_u_component_850hPa",
            "temperature_875hPa",
            "european_aqi",
        ] {
            assert!(!is_public_hourly_variable(variable), "{variable}");
        }
    }

    #[test]
    fn official_daily_radiation_and_cape_aggregations_are_supported() {
        assert!(daily_weather_aggregation("shortwave_radiation_sum").is_ok());
        assert!(is_gfs_public_daily_variable("shortwave_radiation_sum"));
        assert!(is_gfs_public_daily_variable("sunshine_duration"));
        assert!(daily_weather_aggregation("cape_max").is_ok());
        assert!(daily_weather_aggregation("uv_index_max").is_ok());
    }

    #[test]
    fn daily_radiation_uses_megajoules_and_two_decimal_output() {
        let aggregation = DailyWeatherAggregation::RadiationSum("shortwave_radiation");
        assert_eq!(
            daily_weather_unit("shortwave_radiation_sum", aggregation),
            "MJ/m²"
        );
        assert_eq!(
            json_array_for_daily_variable("shortwave_radiation_sum", aggregation, vec![12.345]),
            serde_json::json!([12.35])
        );
    }

    #[test]
    fn daily_mean_precision_matches_official_output() {
        let value = json_array_for_daily_variable(
            "wind_gusts_10m_mean",
            DailyWeatherAggregation::Mean("wind_gusts_10m"),
            vec![5.158],
        );
        assert_eq!(value, serde_json::json!([5.16]));
    }

    #[test]
    fn snow_depth_uses_official_two_decimal_precision() {
        assert_eq!(
            json_array_for_variable("snow_depth", vec![0.006]),
            serde_json::json!([0.01])
        );
    }

    #[test]
    fn ecmwf_native_soil_moisture_keeps_three_decimal_precision() {
        for variable in [
            "soil_moisture_0_to_7cm",
            "soil_moisture_7_to_28cm",
            "soil_moisture_28_to_100cm",
            "soil_moisture_100_to_255cm",
        ] {
            assert_eq!(
                json_array_for_variable(variable, vec![0.1434]),
                serde_json::json!([0.143]),
                "{variable}"
            );
        }
    }

    #[test]
    fn ecmwf_public_pressure_scope_includes_available_ten_hectopascal_level() {
        assert!(is_public_hourly_variable("temperature_10hPa"));
        assert!(is_public_hourly_variable("wind_speed_10hPa"));
        assert!(is_public_hourly_variable("cloud_cover_10hPa"));
    }

    #[test]
    fn ecmwf_two_stage_interpolation_quantizes_the_regular_three_hour_axis() {
        let start = Utc.with_ymd_and_hms(2026, 7, 23, 0, 0, 0).unwrap();
        let kind = InterpolationKind::Hermite {
            scalefactor: 20.0,
            bounds: None,
        };
        let raw = vec![0.93, 0.22, 2.17, f32::NAN, -1.64, f32::NAN, -1.01];
        let mut staged = raw.clone();
        ecmwf_first_stage_point(&mut staged, kind, start, 31.25, 121.5, false, false);
        assert_eq!(staged, vec![0.95, 0.2, 2.15, 0.3, -1.65, -1.55, -1.0]);

        let regular_times = (0..staged.len())
            .map(|index| start + Duration::hours(index as i64 * 3))
            .collect::<Vec<_>>();
        let staged_hour = ecmwf_second_stage_value(
            &staged,
            &regular_times,
            kind,
            start + Duration::hours(7),
            31.25,
            121.5,
            3600,
        );
        let mut unquantized = raw;
        interpolate_ecmwf_hermite_in_place(&mut unquantized, None, false, false);
        let direct_hour = ecmwf_second_stage_value(
            &unquantized,
            &regular_times,
            kind,
            start + Duration::hours(7),
            31.25,
            121.5,
            3600,
        );
        assert_eq!(staged_hour, 1.8);
        assert_eq!(direct_hour, 1.85);
    }

    #[test]
    fn ecmwf_daily_accumulations_keep_regular_three_hour_buckets() {
        let start = Utc.with_ymd_and_hms(2026, 7, 23, 0, 0, 0).unwrap();
        let regular_times = (0..8)
            .map(|index| start + Duration::hours(index * 3))
            .collect::<Vec<_>>();
        let regular_buckets = vec![0.0, 0.3, 0.0, 0.6, 0.9, 0.0, 1.2, 1.5];
        let kind = InterpolationKind::BackwardsSum { scalefactor: 10.0 };
        let daily_buckets = regular_times
            .iter()
            .map(|time| {
                ecmwf_second_stage_value(
                    &regular_buckets,
                    &regular_times,
                    kind,
                    *time,
                    31.25,
                    121.5,
                    10_800,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(daily_buckets, regular_buckets);
        assert_eq!(
            aggregate_daily_weather_values(
                DailyWeatherAggregation::Sum("precipitation"),
                &daily_buckets,
                3,
            ),
            4.5
        );
        assert_eq!(
            aggregate_daily_weather_values(
                DailyWeatherAggregation::PrecipitationHours("precipitation"),
                &daily_buckets,
                3,
            ),
            15.0
        );

        let hourly_at_three = ecmwf_second_stage_value(
            &regular_buckets,
            &regular_times,
            kind,
            start + Duration::hours(3),
            31.25,
            121.5,
            3600,
        );
        assert_eq!(hourly_at_three, 0.1);
    }

    #[test]
    fn ecmwf_native_three_hour_solar_frame_is_not_reinterpolated() {
        let start = Utc.with_ymd_and_hms(2026, 7, 23, 18, 0, 0).unwrap();
        let regular_times = (0..4)
            .map(|index| start + Duration::hours(index * 3))
            .collect::<Vec<_>>();
        let values = vec![0.0, 1.0, 272.0, 764.0];
        let native = ecmwf_second_stage_value(
            &values,
            &regular_times,
            InterpolationKind::SolarBackwardsAveraged { scalefactor: 1.0 },
            start + Duration::hours(3),
            0.0,
            140.0,
            10_800,
        );

        assert_eq!(native, 1.0);
    }

    #[test]
    fn ecmwf_daily_derived_series_uses_native_three_hour_dt() {
        let start = Utc.with_ymd_and_hms(2026, 7, 23, 0, 0, 0).unwrap();
        let daily = (0..8)
            .map(|index| start + Duration::hours(index * 3))
            .collect::<Vec<_>>();
        let hourly = (0..24)
            .map(|index| start + Duration::hours(index))
            .collect::<Vec<_>>();
        assert_eq!(series_dt_seconds(&daily), 10_800);
        assert_eq!(series_dt_seconds(&hourly), 3_600);
        let daily_sunshine =
            backwards_sunshine_duration(200.0, daily[4], series_dt_seconds(&daily), 31.25, 121.5);
        let wrongly_hourly = backwards_sunshine_duration(200.0, daily[4], 3_600, 31.25, 121.5);
        assert_ne!(daily_sunshine.to_bits(), wrongly_hourly.to_bits());
        assert!(daily_sunshine <= 10_800.0);
    }

    #[test]
    fn only_ecmwf_derives_diffuse_radiation_from_shortwave() {
        assert!(!derives_diffuse_radiation(WeatherModel::Gfs));
        assert!(derives_diffuse_radiation(WeatherModel::EcmwfIfs025));
    }

    #[test]
    fn daily_probability_mean_aggregates_the_unrounded_source_series() {
        let values = [39.49, 39.49];
        assert_eq!(
            aggregate_daily_weather_values(
                DailyWeatherAggregation::Mean("precipitation_probability"),
                &values,
                1,
            ),
            39.49
        );
        assert_eq!(
            json_array_for_daily_variable(
                "precipitation_probability_mean",
                DailyWeatherAggregation::Mean("precipitation_probability"),
                vec![39.49],
            ),
            serde_json::json!([39])
        );
    }

    #[test]
    fn ecmwf_interpolates_after_merging_adjacent_run_control_points() {
        let start = Utc.with_ymd_and_hms(2026, 8, 3, 0, 0, 0).unwrap();
        let regular = Arc::new(EcmwfRegularSeries {
            runs: vec![
                EcmwfRegularRun {
                    source_run: "2026080300".to_string(),
                    start,
                    frames: vec![vec![10.0], vec![20.0], vec![60.0]],
                },
                EcmwfRegularRun {
                    source_run: "2026080306".to_string(),
                    start: start + Duration::hours(3),
                    frames: vec![vec![30.0]],
                },
            ],
            public_start_utc: None,
            latest_source_run: Some("2026080306".to_string()),
        });
        let values = read_ecmwf_regular_series_grid(
            &[regular],
            "ecmwf_ifs025",
            "temperature_2m",
            &[start + Duration::hours(4)],
            &[31.25],
            &[121.5],
        )
        .unwrap();

        let regular_times = [0, 3, 6]
            .into_iter()
            .map(|hour| start + Duration::hours(hour))
            .collect::<Vec<_>>();
        let expected = ecmwf_second_stage_value(
            &[10.0, 30.0, 60.0],
            &regular_times,
            ecmwf_interpolation_kind("temperature_2m"),
            start + Duration::hours(4),
            31.25,
            121.5,
            3600,
        );
        let older_run_only = ecmwf_second_stage_value(
            &[10.0, 20.0, 60.0],
            &regular_times,
            ecmwf_interpolation_kind("temperature_2m"),
            start + Duration::hours(4),
            31.25,
            121.5,
            3600,
        );
        assert_eq!(values, vec![vec![expected]]);
        assert_ne!(expected, older_run_only);
    }

    #[test]
    fn ecmwf_nan_fallback_is_one_way_and_keeps_finite_primary_values() {
        let mut primary = vec![25.0, f32::NAN, f32::NAN];
        let previous_complete = vec![30.0, 26.0, f32::NAN];

        fill_nan_from_fallback(&mut primary, &previous_complete);

        assert_eq!(primary[0], 25.0);
        assert_eq!(primary[1], 26.0);
        assert!(primary[2].is_nan());
    }

    #[test]
    fn ecmwf_daily_sum_requires_front_hours_to_be_filled_before_aggregation() {
        let mut day = vec![f32::NAN, f32::NAN, 0.3, 0.6, 0.0, 0.9, 0.0, 1.2];
        assert!(aggregate_daily_weather_values(
            DailyWeatherAggregation::Sum("precipitation"),
            &day,
            3,
        )
        .is_nan());

        fill_nan_from_fallback(&mut day[..2], &[0.0, 0.3]);

        let total =
            aggregate_daily_weather_values(DailyWeatherAggregation::Sum("precipitation"), &day, 3);
        assert!((total - 3.3).abs() < 1e-6);
    }

    #[test]
    fn ecmwf_regular_cache_builds_each_raw_stream_once_per_request() {
        use std::cell::Cell;

        let builds = Cell::new(0usize);
        let key = EcmwfRegularCacheKey {
            coverage_id: "ecmwf_ifs025_2026072300".to_string(),
            raw_variable: "temperature_2m".to_string(),
            latitudes: vec![31.25_f64.to_bits()],
            longitudes: vec![121.5_f64.to_bits()],
        };
        with_ecmwf_request_cache(|| {
            let first = ecmwf_cached_regular_series(key.clone(), || {
                builds.set(builds.get() + 1);
                Ok(Some(EcmwfRegularSeries {
                    runs: vec![EcmwfRegularRun {
                        source_run: "2026072300".to_string(),
                        start: Utc.with_ymd_and_hms(2026, 7, 23, 0, 0, 0).unwrap(),
                        frames: vec![vec![20.0]],
                    }],
                    public_start_utc: None,
                    latest_source_run: Some("2026072300".to_string()),
                }))
            })?
            .unwrap();
            let second = ecmwf_cached_regular_series(key, || {
                builds.set(builds.get() + 1);
                Ok(None)
            })?
            .unwrap();
            assert!(Arc::ptr_eq(&first, &second));
            Ok(())
        })
        .unwrap();
        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn gfs_probability_cache_builds_each_point_history_once_per_request() {
        use std::cell::Cell;

        let builds = Cell::new(0usize);
        let key = GfsProbabilityHistoryCacheKey {
            product_coverages: vec![(
                "ncep_gefs025".to_string(),
                "gfs_native_2026073006@2026073006".to_string(),
            )],
            raw_variable: "precipitation_probability".to_string(),
            latitude_bits: 31.25_f64.to_bits(),
            longitude_bits: 121.5_f64.to_bits(),
        };
        with_gfs_request_cache(|| {
            let first = gfs_cached_probability_point_history(key.clone(), || {
                builds.set(builds.get() + 1);
                Ok(GfsProbabilityPointHistory {
                    regular_seconds: 10_800,
                    regular_times: vec![Utc.with_ymd_and_hms(2026, 7, 30, 6, 0, 0).unwrap()],
                    values: vec![42.0],
                })
            })?;
            let second = gfs_cached_probability_point_history(key, || {
                builds.set(builds.get() + 1);
                Ok(GfsProbabilityPointHistory {
                    regular_seconds: 0,
                    regular_times: Vec::new(),
                    values: Vec::new(),
                })
            })?;
            assert!(Arc::ptr_eq(&first, &second));
            Ok(())
        })
        .unwrap();
        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn gfs_probability_cache_separates_domains_in_one_native_coverage() {
        use std::cell::Cell;

        let builds = Cell::new(0usize);
        let make_key = |product: &str| GfsProbabilityHistoryCacheKey {
            product_coverages: vec![(
                product.to_string(),
                "gfs_native_2026080112@2026080112".to_string(),
            )],
            raw_variable: "precipitation_probability".to_string(),
            latitude_bits: 31.25_f64.to_bits(),
            longitude_bits: 121.5_f64.to_bits(),
        };
        with_gfs_request_cache(|| {
            let high = gfs_cached_probability_point_history(make_key("ncep_gefs025"), || {
                builds.set(builds.get() + 1);
                Ok(GfsProbabilityPointHistory {
                    regular_seconds: 10_800,
                    regular_times: vec![Utc.with_ymd_and_hms(2026, 8, 1, 15, 0, 0).unwrap()],
                    values: vec![25.0],
                })
            })?;
            let low = gfs_cached_probability_point_history(make_key("ncep_gefs05"), || {
                builds.set(builds.get() + 1);
                Ok(GfsProbabilityPointHistory {
                    regular_seconds: 10_800,
                    regular_times: vec![Utc.with_ymd_and_hms(2026, 8, 1, 15, 0, 0).unwrap()],
                    values: vec![50.0],
                })
            })?;
            assert!(!Arc::ptr_eq(&high, &low));
            assert_eq!(high.values, vec![25.0]);
            assert_eq!(low.values, vec![50.0]);
            Ok(())
        })
        .unwrap();
        assert_eq!(builds.get(), 2);
    }
}

fn output_decimals_for_variable(variable: &str) -> OutputDecimals {
    if variable.ends_with("hPa") {
        if variable.starts_with("geopotential_height_") {
            return OutputDecimals::Fixed(2);
        }
        if variable.starts_with("temperature_") {
            return OutputDecimals::Fixed(1);
        }
        if variable.starts_with("dew_point_") || variable.starts_with("dewpoint_") {
            return OutputDecimals::Fixed(1);
        }
        if variable.starts_with("wind_speed_") || variable.starts_with("windspeed_") {
            return OutputDecimals::Fixed(2);
        }
        if variable.starts_with("wind_direction_") || variable.starts_with("winddirection_") {
            return OutputDecimals::Integer;
        }
        if variable.starts_with("relativehumidity_") || variable.starts_with("cloudcover_") {
            return OutputDecimals::Integer;
        }
        if variable.starts_with("relative_humidity_") || variable.starts_with("cloud_cover_") {
            return OutputDecimals::Integer;
        }
        if variable.starts_with("wind_u_component_")
            || variable.starts_with("wind_v_component_")
            || variable.starts_with("vertical_velocity_")
        {
            return OutputDecimals::Fixed(2);
        }
    }
    match variable {
        "european_aqi"
        | "european_aqi_pm2_5"
        | "european_aqi_pm10"
        | "european_aqi_no2"
        | "european_aqi_o3"
        | "european_aqi_so2"
        | "european_aqi_nitrogen_dioxide"
        | "european_aqi_ozone"
        | "european_aqi_sulphur_dioxide"
        | "us_aqi"
        | "us_aqi_pm2_5"
        | "us_aqi_pm10"
        | "us_aqi_no2"
        | "us_aqi_o3"
        | "us_aqi_so2"
        | "us_aqi_co"
        | "us_aqi_nitrogen_dioxide"
        | "us_aqi_ozone"
        | "us_aqi_sulphur_dioxide"
        | "us_aqi_carbon_monoxide" => OutputDecimals::Integer,
        "chinese_aqi"
        | "chinese_aqi_pm2_5"
        | "chinese_aqi_pm10"
        | "chinese_aqi_no2"
        | "chinese_aqi_o3"
        | "chinese_aqi_so2"
        | "chinese_aqi_co"
        | "chinese_aqi_nitrogen_dioxide"
        | "chinese_aqi_ozone"
        | "chinese_aqi_sulphur_dioxide"
        | "chinese_aqi_carbon_monoxide" => OutputDecimals::Integer,
        "weather_code"
        | "weathercode"
        | "relative_humidity_2m"
        | "relativehumidity_2m"
        | "cloud_cover"
        | "cloudcover"
        | "cloud_cover_low"
        | "cloudcover_low"
        | "cloud_cover_mid"
        | "cloudcover_mid"
        | "cloud_cover_high"
        | "cloudcover_high"
        | "precipitation_probability"
        | "precipitation_probability_max"
        | "precipitation_probability_mean"
        | "precipitation_probability_min"
        | "wind_direction_10m"
        | "winddirection_10m"
        | "wind_direction_80m"
        | "winddirection_80m"
        | "wind_direction_100m"
        | "winddirection_100m"
        | "wind_direction_120m"
        | "winddirection_120m"
        | "categorical_freezing_rain"
        | "is_day" => OutputDecimals::Integer,
        "precipitation_type" => OutputDecimals::Fixed(1),
        "wind_speed_10m"
        | "windspeed_10m"
        | "wind_speed_80m"
        | "windspeed_80m"
        | "wind_speed_100m"
        | "windspeed_100m"
        | "wind_speed_120m"
        | "windspeed_120m"
        | "wind_u_component_10m"
        | "wind_v_component_10m"
        | "wind_u_component_80m"
        | "wind_v_component_80m"
        | "wind_u_component_100m"
        | "wind_v_component_100m"
        | "vertical_velocity"
        | "aerosol_optical_depth" => OutputDecimals::Fixed(2),
        "soil_moisture_0_to_10cm"
        | "soil_moisture_0_to_7cm"
        | "soil_moisture_7_to_28cm"
        | "soil_moisture_28_to_100cm"
        | "soil_moisture_100_to_255cm"
        | "soil_moisture_0_to_100cm"
        | "soil_moisture_10_to_40cm"
        | "soil_moisture_40_to_100cm"
        | "soil_moisture_100_to_200cm" => OutputDecimals::Fixed(3),
        "snowfall"
        | "snow_depth"
        | "uv_index"
        | "uv_index_clear_sky"
        | "sunshine_duration"
        | "daylight_duration"
        | "evapotranspiration"
        | "et0_fao_evapotranspiration"
        | "vapour_pressure_deficit"
        | "vapor_pressure_deficit"
        | "growing_degree_days_base_0_limit_50" => OutputDecimals::Fixed(2),
        "direct_radiation"
        | "shortwave_radiation_instant"
        | "diffuse_radiation_instant"
        | "direct_radiation_instant"
        | "direct_normal_irradiance"
        | "direct_normal_irradiance_instant"
        | "global_tilted_irradiance"
        | "global_tilted_irradiance_instant" => OutputDecimals::Fixed(1),
        "temperature_2m"
        | "temperature_2m_min"
        | "temperature_2m_max"
        | "apparent_temperature"
        | "temperature_80m"
        | "temperature_100m"
        | "temperature_120m"
        | "dew_point_2m"
        | "dewpoint_2m"
        | "wet_bulb_temperature_2m"
        | "surface_temperature"
        | "skin_temperature"
        | "soil_temperature_0cm"
        | "soil_temperature_0_to_10cm"
        | "soil_temperature_0_to_7cm"
        | "soil_temperature_7_to_28cm"
        | "soil_temperature_28_to_100cm"
        | "soil_temperature_100_to_255cm"
        | "soil_temperature_0_to_100cm"
        | "soil_temperature_10_to_40cm"
        | "soil_temperature_40_to_100cm"
        | "soil_temperature_100_to_200cm"
        | "precipitation"
        | "showers"
        | "rain"
        | "snowfall_water_equivalent"
        | "wind_gusts_10m"
        | "pressure_msl"
        | "visibility"
        | "freezing_level_height"
        | "boundary_layer_height"
        | "cape"
        | "convective_inhibition"
        | "lifted_index"
        | "pm2_5"
        | "pm10"
        | "dust"
        | "carbon_monoxide"
        | "nitrogen_dioxide"
        | "ozone"
        | "sulphur_dioxide" => OutputDecimals::Fixed(1),
        "pm2_5_mean"
        | "pm10_mean"
        | "nitrogen_dioxide_mean"
        | "ozone_maximum_8h_mean"
        | "sulphur_dioxide_mean" => OutputDecimals::Integer,
        "carbon_monoxide_mean" => OutputDecimals::Fixed(1),
        _ => OutputDecimals::Fixed(1),
    }
}
