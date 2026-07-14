use crate::manifest::{ArrayMetadata, BundleEntry, EntryKey, ProductSnapshot};
use crate::native::read_native_array_metadata;
use crate::official::{build_v3_array_metadata_blob, BundleRangeReader, OfficialDecoder};
use crate::snapshot::OmDataSnapshot;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

const GFS013_STATIC_ELEVATION_PATH: &str = "static/ncep_gfs013/HSURF.om";
const GFS013_STATIC_DIMENSIONS: &[u64] = &[1536, 3072];
const GFS013_STATIC_CHUNKS: &[u64] = &[20, 20];
const GFS013_STATIC_LUT_OFFSET: u64 = 1_439_999;
const GFS013_STATIC_LUT_SIZE: u64 = 15_438;
const GFS013_STATIC_FILE_SIZE: u64 = 1_455_544;
type GfsElevationCache = HashMap<(PathBuf, u64, u64), f32>;
static GFS_ELEVATION_CACHE: OnceLock<Mutex<GfsElevationCache>> = OnceLock::new();
type Dem90FileCache = HashMap<PathBuf, Arc<Dem90File>>;
static DEM90_FILE_CACHE: OnceLock<Mutex<Dem90FileCache>> = OnceLock::new();

#[derive(Debug)]
struct Dem90File {
    file: Arc<File>,
    metadata: Vec<u64>,
}

pub const OPENMETEO_UPSTREAM_BASELINE: &str = "4efb9c49fb4a3718ed385fb22580d2e0fc56bdb2";
pub const OPENMETEO_IMAGE_BASELINE: &str = "weather-forecast-openmeteo:9849315";

#[derive(Debug, Clone, Deserialize)]
pub struct PointQuery {
    pub latitude: String,
    pub longitude: String,
    #[serde(default)]
    pub hourly: Option<String>,
    #[serde(default)]
    pub daily: Option<String>,
    #[serde(default)]
    pub start_hour: Option<String>,
    #[serde(default)]
    pub end_hour: Option<String>,
    #[serde(default)]
    pub start_date: Option<String>,
    #[serde(default)]
    pub end_date: Option<String>,
    #[serde(default)]
    pub forecast_hours: Option<usize>,
    #[serde(default)]
    pub past_days: Option<usize>,
    #[serde(default)]
    pub forecast_days: Option<usize>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub elevation: Option<String>,
    #[serde(default)]
    pub cell_selection: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RouteQuery {
    pub points: Vec<RoutePoint>,
    pub hourly: Vec<String>,
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
    pub hourly_units: BTreeMap<String, String>,
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
        .unwrap_or("temperature_2m")
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
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
    include_gfs_elevation: bool,
) -> Result<serde_json::Value> {
    if let Some(cell_selection) = &query.cell_selection {
        let normalized = cell_selection.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "" | "nearest" => {}
            "land" => bail!(
                "cell_selection=land requires DEM/static grid selection data from the Singapore baseline and is not enabled yet"
            ),
            _ => bail!("unsupported cell_selection: {}", cell_selection),
        }
    }
    let latitudes = parse_csv_f64(&query.latitude, "latitude")?;
    let longitudes = parse_csv_f64(&query.longitude, "longitude")?;
    if latitudes.len() != longitudes.len() {
        bail!("latitude and longitude count must match");
    }
    let explicit_elevations = if let Some(value) = query.elevation.as_deref() {
        let values = parse_csv_f64(value, "elevation")?;
        if values.len() != latitudes.len() {
            bail!("elevation count must match latitude and longitude count");
        }
        values.into_iter().map(Some).collect::<Vec<_>>()
    } else {
        vec![None; latitudes.len()]
    };
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
    let timezones = parse_query_timezones(query.timezone.as_deref(), latitudes.len())?;
    let daily_has_aqi = daily_variables
        .iter()
        .any(|variable| is_chinese_aqi_variable(variable));
    let daily_is_aqi = !daily_variables.is_empty()
        && daily_variables
            .iter()
            .all(|variable| is_chinese_aqi_variable(variable));
    if daily_has_aqi && !daily_is_aqi {
        bail!("daily weather and Chinese AQI variables cannot be mixed in one request");
    }

    let mut responses = Vec::new();
    for (((latitude, longitude), timezone), explicit_elevation) in latitudes
        .into_iter()
        .zip(longitudes.into_iter())
        .zip(timezones.into_iter())
        .zip(explicit_elevations.into_iter())
    {
        let target_elevation = if include_gfs_elevation {
            match explicit_elevation {
                Some(value) => Some(value as f32),
                None => read_dem90_elevation(snapshot, decoder, latitude, longitude)?,
            }
        } else {
            None
        };
        let start = parse_hour_with_timezone(query.start_hour.as_deref(), &timezone)?;
        let end = parse_hour_with_timezone(query.end_hour.as_deref(), &timezone)?;
        let mut response = point_forecast(
            snapshot,
            decoder,
            latitude,
            longitude,
            &variables,
            start,
            end,
            query.forecast_hours,
            include_gfs_elevation,
            target_elevation,
        )?;
        apply_response_timezone(&mut response, &timezone)?;
        if daily_is_aqi {
            attach_daily_chinese_aqi(
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
                &timezone,
            )?;
        }
        responses.push(response);
    }

    if responses.len() == 1 {
        Ok(serde_json::to_value(responses.remove(0))?)
    } else {
        Ok(serde_json::to_value(responses)?)
    }
}

pub fn route_forecast(
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
    let started = std::time::Instant::now();
    let mut points = Vec::with_capacity(query.points.len());
    for point in &query.points {
        let start = point.time;
        let response = point_forecast(
            snapshot,
            decoder,
            point.latitude,
            point.longitude,
            &query.hourly,
            start,
            start,
            Some(1),
            true,
            None,
        )?;
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

pub fn point_forecast(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
    variables: &[String],
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    limit: Option<usize>,
    include_gfs_elevation: bool,
    target_elevation: Option<f32>,
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

        for variable in variables {
            let mut values = Vec::with_capacity(times.len());
            for time in &times {
                let result = if matches!(variable.as_str(), "surface_pressure" | "surfacepressure")
                {
                    read_surface_pressure_value(
                        snapshot,
                        decoder,
                        *time,
                        latitude,
                        longitude,
                        target_elevation,
                    )
                } else {
                    read_variable_value(snapshot, decoder, variable, *time, latitude, longitude)
                };
                match result {
                    Ok(value) => values.push(value),
                    Err(error) if error.to_string().contains("variable/time is not available") => {
                        values.push(f32::NAN)
                    }
                    Err(error) => return Err(error),
                }
            }
            hourly_units.insert(variable.clone(), unit_for_variable(variable).to_string());
            hourly.insert(variable.clone(), json_array_for_variable(variable, values));
        }
    }

    let point_metadata = if include_gfs_elevation {
        read_gfs_point_metadata(snapshot, decoder, latitude, longitude)?
    } else {
        None
    };
    let (response_latitude, response_longitude, model_elevation) =
        point_metadata.unwrap_or((latitude, longitude, f32::NAN));
    let elevation = target_elevation.unwrap_or(model_elevation);
    Ok(ForecastResponse {
        latitude: response_latitude,
        longitude: response_longitude,
        generationtime_ms: started.elapsed().as_secs_f64() * 1000.0,
        utc_offset_seconds: 0,
        timezone: "GMT".to_string(),
        timezone_abbreviation: "GMT".to_string(),
        elevation: elevation.is_finite().then_some(elevation as f64),
        hourly_units,
        hourly,
        daily_units: None,
        daily: None,
    })
}

#[derive(Debug, Clone, Copy)]
enum DailyWeatherAggregation {
    Max(&'static str),
    Min(&'static str),
    Mean(&'static str),
    Sum(&'static str),
    RadiationSum(&'static str),
    PrecipitationHours(&'static str),
    DominantWindDirection,
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
            Self::DominantWindDirection => "wind_u_component_10m",
        }
    }

    fn output_variable(self) -> &'static str {
        match self {
            Self::Max(variable)
            | Self::Min(variable)
            | Self::Mean(variable)
            | Self::Sum(variable) => variable,
            Self::RadiationSum(_) => "uv_index",
            Self::PrecipitationHours(_) => "precipitation",
            Self::DominantWindDirection => "wind_direction_10m",
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
        "rain_sum" => DailyWeatherAggregation::Sum("rain"),
        "showers_sum" => DailyWeatherAggregation::Sum("showers"),
        "snowfall_sum" => DailyWeatherAggregation::Sum("snowfall"),
        "snowfall_water_equivalent_sum" => DailyWeatherAggregation::Sum("snowfall_water_equivalent"),
        "weather_code" | "weathercode" => DailyWeatherAggregation::Max("weather_code"),
        "shortwave_radiation_sum" => DailyWeatherAggregation::RadiationSum("shortwave_radiation"),
        "wind_speed_10m_max" | "windspeed_10m_max" => DailyWeatherAggregation::Max("wind_speed_10m"),
        "wind_speed_10m_min" | "windspeed_10m_min" => DailyWeatherAggregation::Min("wind_speed_10m"),
        "wind_speed_10m_mean" | "windspeed_10m_mean" => DailyWeatherAggregation::Mean("wind_speed_10m"),
        "wind_gusts_10m_max" | "windgusts_10m_max" => DailyWeatherAggregation::Max("wind_gusts_10m"),
        "wind_gusts_10m_min" | "windgusts_10m_min" => DailyWeatherAggregation::Min("wind_gusts_10m"),
        "wind_gusts_10m_mean" | "windgusts_10m_mean" => DailyWeatherAggregation::Mean("wind_gusts_10m"),
        "wind_direction_10m_dominant" | "winddirection_10m_dominant" => DailyWeatherAggregation::DominantWindDirection,
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

    let raw_seed = seed_variable_for_times(seed_variable);
    let (product_name, raw_variable) = product_for_variable(snapshot, raw_seed)?;
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

    if matches!(aggregation, DailyWeatherAggregation::DominantWindDirection) {
        let mut u_sum = 0.0_f32;
        let mut v_sum = 0.0_f32;
        let mut time = start;
        while time < end {
            let u = read_daily_hour(
                snapshot,
                decoder,
                "wind_u_component_10m",
                time,
                latitude,
                longitude,
            )?;
            let v = read_daily_hour(
                snapshot,
                decoder,
                "wind_v_component_10m",
                time,
                latitude,
                longitude,
            )?;
            if !u.is_finite() || !v.is_finite() {
                return Ok(f32::NAN);
            }
            u_sum += u;
            v_sum += v;
            time += Duration::hours(1);
        }
        return Ok(wind_direction(u_sum, v_sum));
    }

    let source = aggregation.seed_variable();
    let mut values = Vec::new();
    let mut time = start;
    while time < end {
        values.push(read_daily_hour(
            snapshot, decoder, source, time, latitude, longitude,
        )?);
        time += Duration::hours(1);
    }

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
        DailyWeatherAggregation::Max(_) => finite_extreme(true),
        DailyWeatherAggregation::Min(_) => finite_extreme(false),
        DailyWeatherAggregation::Mean(_) if complete => {
            values.iter().sum::<f32>() / values.len() as f32
        }
        DailyWeatherAggregation::Sum(_) if complete => values.iter().sum(),
        DailyWeatherAggregation::RadiationSum(_) if complete => {
            (values.iter().map(|value| value * 0.0036).sum::<f32>() * 100.0).round() / 100.0
        }
        DailyWeatherAggregation::PrecipitationHours(_) if complete => values
            .iter()
            .map(|value| if *value > 0.001 { 1.0 } else { 0.0 })
            .sum(),
        DailyWeatherAggregation::Mean(_)
        | DailyWeatherAggregation::Sum(_)
        | DailyWeatherAggregation::RadiationSum(_)
        | DailyWeatherAggregation::PrecipitationHours(_) => f32::NAN,
        DailyWeatherAggregation::DominantWindDirection => unreachable!("handled above"),
    };
    Ok(value)
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
        "shortwave_radiation_sum" => "MJ/m\u{00B2}",
        "precipitation_hours" => "h",
        _ => unit_for_variable(aggregation.output_variable()),
    }
}

fn attach_daily_chinese_aqi(
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
        .any(|variable| !is_chinese_aqi_variable(variable))
    {
        bail!("daily currently supports Chinese AQI variables only")
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
        let mut values = Vec::with_capacity(variables.len());
        for variable in variables {
            values.push(daily_chinese_aqi_value(
                snapshot, decoder, variable, date, latitude, longitude,
            )?);
        }
        if values.iter().any(|value| !value.is_finite()) {
            continue;
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
    let seed_var = variables
        .first()
        .map(|value| seed_variable_for_times(value))
        .unwrap_or("temperature_2m");
    let (product_name, raw_var) = product_for_variable(snapshot, seed_var)?;
    let product = snapshot.require_product(product_name)?;
    let mut times: Vec<DateTime<Utc>> = product
        .entries
        .keys()
        .filter(|key| key.variable == raw_var)
        .map(|key| key.valid_time_utc)
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
    let selected_start = start.unwrap_or(public_start).max(public_start);
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
            return read_surface_pressure_value(snapshot, decoder, time, latitude, longitude, None);
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
            let showers = read_direct(snapshot, decoder, "showers", time, latitude, longitude)?;
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
                read_gfs_surface_elevation_grid(snapshot, decoder, latitudes, longitudes)?;
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

/// Read several regional output hours while allowing native 3D OM files to be
/// decoded as one time slab. Open-Meteo stores the complete run time axis in a
/// single chunk, so decoding one hour at a time repeatedly inflates the same
/// chunk and is prohibitively slow for WebP production.
pub fn read_variable_grid_series(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<Vec<f32>>> {
    if times.is_empty() || latitudes.is_empty() || longitudes.is_empty() {
        bail!("regional grid series dimensions must not be empty");
    }
    let combine2 = |left: Vec<Vec<f32>>, right: Vec<Vec<f32>>, op: fn(f32, f32) -> f32| {
        left.into_iter()
            .zip(right)
            .map(|(left, right)| left.into_iter().zip(right).map(|(a, b)| op(a, b)).collect())
            .collect()
    };
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
            let elevation =
                read_gfs_surface_elevation_grid(snapshot, decoder, latitudes, longitudes)?;
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

fn daily_chinese_aqi_value(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    date: NaiveDate,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let day_start = china_day_start_utc(date)?;
    let pm2_5 = chinese_daily_iaqi(
        daily_mean(snapshot, decoder, "pm2_5", day_start, latitude, longitude)?,
        &HJ633_PM2_5_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    );
    let pm10 = chinese_daily_iaqi(
        daily_mean(snapshot, decoder, "pm10", day_start, latitude, longitude)?,
        &HJ633_PM10_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    );
    let no2 = chinese_daily_iaqi(
        daily_mean(
            snapshot,
            decoder,
            "nitrogen_dioxide",
            day_start,
            latitude,
            longitude,
        )?,
        &HJ633_NO2_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    );
    let o3 = chinese_daily_iaqi(
        daily_maximum_8h_mean(snapshot, decoder, day_start, latitude, longitude)?,
        &HJ633_O3_8H,
        &HJ633_AQI_BREAKPOINTS[..6],
        300.0,
        0,
    );
    let so2 = chinese_daily_iaqi(
        daily_mean(
            snapshot,
            decoder,
            "sulphur_dioxide",
            day_start,
            latitude,
            longitude,
        )?,
        &HJ633_SO2_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    );
    let co = chinese_daily_iaqi(
        daily_mean(
            snapshot,
            decoder,
            "carbon_monoxide",
            day_start,
            latitude,
            longitude,
        )? / 1000.0,
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
    for hour in 0..24 {
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
    let concentration = round_ties_to_even(concentration, decimals);
    let Some((&last_concentration, &last_aqi)) =
        concentration_breakpoints.last().zip(aqi_breakpoints.last())
    else {
        return f32::NAN;
    };
    if concentration > last_concentration {
        return upper_limit.min(last_aqi);
    }
    for index in 1..concentration_breakpoints.len() {
        let low = concentration_breakpoints[index - 1];
        let high = concentration_breakpoints[index];
        if concentration <= high {
            let aqi_low = aqi_breakpoints[index - 1];
            let aqi_high = aqi_breakpoints[index];
            let iaqi = (aqi_high - aqi_low) / (high - low) * (concentration - low) + aqi_low;
            return iaqi.ceil().min(upper_limit);
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

fn read_surface_pressure_value(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    target_elevation: Option<f32>,
) -> Result<f32> {
    let temperature = read_direct(
        snapshot,
        decoder,
        "temperature_2m",
        time,
        latitude,
        longitude,
    )?;
    let pressure_msl = read_direct(snapshot, decoder, "pressure_msl", time, latitude, longitude)?;
    let elevation = match target_elevation {
        Some(value) => value,
        None => gfs013_model_location(snapshot, decoder, latitude, longitude)?
            .map(|(_, _, value)| value)
            .unwrap_or(0.0),
    };
    Ok(surface_pressure(temperature, pressure_msl, elevation))
}

fn dem90_pixel(latitude: i32) -> u64 {
    match latitude {
        value if value < -85 => 120,
        value if value < -80 => 240,
        value if value < -70 => 400,
        value if value < -60 => 600,
        value if value < -50 => 800,
        value if value < 50 => 1200,
        value if value < 60 => 800,
        value if value < 70 => 600,
        value if value < 80 => 400,
        value if value < 85 => 240,
        _ => 120,
    }
}

fn dem90_latitude_chunk(latitude: f64) -> i32 {
    if latitude < 0.0 {
        latitude as i32 - 1
    } else {
        latitude as i32
    }
}

fn read_dem90_elevation(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<f32>> {
    if !(-90.0..90.0).contains(&latitude) || !(-180.0..180.0).contains(&longitude) {
        return Ok(None);
    }
    let Some(product) = snapshot.product("gfs013_surface") else {
        return Ok(None);
    };
    let static_root = product.product_root.join("copernicus_dem90/static");
    if !static_root.exists() {
        return Ok(None);
    }
    let latitude_chunk = dem90_latitude_chunk(latitude);
    let path = static_root.join(format!("lat_{latitude_chunk}.om"));
    if !path.is_file() {
        bail!(
            "required Copernicus DEM90 latitude chunk is missing: {}",
            path.display()
        );
    }
    let pixels = dem90_pixel(latitude_chunk);
    let cached = {
        let cache = DEM90_FILE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow!("Copernicus DEM90 file cache poisoned"))?;
        if let Some(value) = cache.get(&path) {
            value.clone()
        } else {
            let file =
                Arc::new(File::open(&path).with_context(|| format!("open {}", path.display()))?);
            let array = read_native_array_metadata(&file)
                .with_context(|| format!("parse Copernicus DEM90 file {}", path.display()))?;
            if array.dimensions != [1200, pixels * 360] || array.chunks.len() != 2 {
                bail!(
                    "Copernicus DEM90 dimensions do not match latitude chunk: {}",
                    path.display()
                );
            }
            let metadata = build_v3_array_metadata_blob(
                "",
                array.data_type,
                array.compression,
                &array.dimensions,
                &array.chunks,
                array
                    .lut_size
                    .context("Copernicus DEM90 metadata missing lut_size")?,
                array
                    .lut_offset
                    .context("Copernicus DEM90 metadata missing lut_offset")?,
                array.scale_factor.unwrap_or(1.0),
                array.add_offset.unwrap_or(0.0),
            );
            let value = Arc::new(Dem90File { file, metadata });
            cache.insert(path.clone(), value.clone());
            value
        }
    };
    let latitude_row = ((latitude * 1200.0 + 90.0 * 1200.0) as u64) % 1200;
    let longitude_row = ((longitude + 180.0) * pixels as f64) as u64;
    let decoder =
        decoder.context("official OM decoder library is required for Copernicus DEM90")?;
    let reader = FullFileRangeReader {
        file: cached.file.clone(),
    };
    Ok(Some(decoder.decode_point(
        &cached.metadata,
        &reader,
        &[latitude_row, longitude_row],
    )?))
}

fn surface_pressure(temperature: f32, pressure_msl: f32, elevation: f32) -> f32 {
    let elevation = if elevation.is_nan() { 0.0 } else { elevation };
    let t0 = temperature + 273.15 + 0.0065 * elevation;
    let factor = (1.0 - (0.0065 * elevation) / t0).powf(-5.255_781_3);
    pressure_msl / factor
}

fn gfs_surface_elevation_entry(
    snapshot: &OmDataSnapshot,
) -> Option<(Arc<ProductSnapshot>, BundleEntry)> {
    let product = snapshot.product("gfs013_surface")?;
    let entry = product.static_entries.get("surface_elevation")?.clone();
    Some((product, entry))
}

fn read_gfs_surface_elevation_grid(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Vec<f32>> {
    let Some((product, entry)) = gfs_surface_elevation_entry(snapshot) else {
        return Ok(vec![0.0; latitudes.len() * longitudes.len()]);
    };
    let mut values = read_entry_grid(&product, &entry, decoder, latitudes, longitudes)?;
    values
        .iter_mut()
        .for_each(|value| *value = normalize_surface_elevation(*value));
    Ok(values)
}

fn read_gfs_point_metadata(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<(f64, f64, f32)>> {
    let Some((product, entry)) = gfs_surface_elevation_entry(snapshot) else {
        return Ok(None);
    };
    let (y, x) = grid_index_for_lat_lon(
        &entry.array,
        entry.native_grid.as_ref(),
        latitude,
        longitude,
    )?;
    let model_latitude = grid_latitude_for_index(&entry.array, entry.native_grid.as_ref(), y)?;
    let model_longitude = grid_longitude_for_index(&entry.array, entry.native_grid.as_ref(), x)?;
    let elevation = normalize_surface_elevation(read_entry_value(
        &product, &entry, decoder, latitude, longitude,
    )?);
    Ok(Some((
        json_f32_as_f64(model_latitude),
        json_f32_as_f64(model_longitude),
        elevation,
    )))
}

fn normalize_surface_elevation(value: f32) -> f32 {
    if value.is_finite() && value > -900.0 {
        value
    } else {
        0.0
    }
}

fn json_f32_as_f64(value: f32) -> f64 {
    value.to_string().parse::<f64>().unwrap_or(value as f64)
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

fn read_direct_grid(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Vec<f32>> {
    if variable == "carbon_monoxide"
        && snapshot.product("cams_global_greenhouse_gases").is_some()
        && snapshot.product("cams_global").is_some()
    {
        return read_cams_mixed_carbon_monoxide_grid(
            snapshot,
            decoder,
            time,
            latitudes,
            longitudes,
            round_values,
        );
    }
    let (product_name, raw_variable) = product_for_variable(snapshot, variable)?;
    let products = snapshot.product_snapshots(product_name);
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

fn read_direct_grid_series(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Vec<Vec<f32>>> {
    let (product_name, raw_variable) = product_for_variable(snapshot, variable)?;
    let products = snapshot.product_snapshots(product_name);
    for product in &products {
        if let Some(values) = read_exact_native_grid_series(
            product,
            decoder,
            variable,
            &raw_variable,
            times,
            latitudes,
            longitudes,
            round_values,
        )? {
            return Ok(values);
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
            if product.entries.contains_key(&EntryKey {
                variable: raw_variable.to_string(),
                valid_time_utc: time,
            }) {
                read_native_grid(product, decoder, raw_variable, time, latitudes, longitudes)
            } else {
                Ok(vec![f32::NAN; latitudes.len() * longitudes.len()])
            }
        }
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
        InterpolationKind::SolarBackwards { scalefactor } => read_solar_backwards_grid(
            product,
            decoder,
            raw_variable,
            time,
            latitudes,
            longitudes,
            scalefactor,
            round_values,
        ),
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
    if rejects_cross_run_hermite_core(product, raw_variable, &native_times, index, time) {
        return Ok(vec![f32::NAN; latitudes.len() * longitudes.len()]);
    }
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
    let b_time = native_times[index];
    let a_time = b_time - Duration::seconds(stride_seconds);
    let a = if native_times.binary_search(&a_time).is_ok()
        && entries_share_source_run(product, raw_variable, a_time, b_time)
    {
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
    let c_time = native_times[index + 1];
    let d_time = c_time + Duration::seconds(stride_seconds);
    let d = if native_times.binary_search(&d_time).is_ok()
        && entries_share_source_run(product, raw_variable, c_time, d_time)
    {
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
    // CAMS is stored every three hours and requires interpolation. GFS is
    // hourly through forecast hour 120, exactly the WebP window.
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
            InterpolationKind::Linear { scalefactor }
            | InterpolationKind::SolarBackwards { scalefactor }
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
    // A variable may omit latest-run f000 while the retained history supplies
    // f-001 and the new run supplies f001. Resolve that gap through the same
    // grid interpolation policy as the point API; a cross-run Hermite core is
    // returned as NaN rather than synthesized across model initializations.
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
    for product in &products {
        if !product_covers_time(product, raw_variable, time) {
            continue;
        }
        return read_product_value_with_rounding(
            product,
            decoder,
            variable,
            raw_variable,
            time,
            latitude,
            longitude,
            round_values,
        );
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

#[allow(clippy::too_many_arguments)]
fn read_product_history_grid_with_rounding(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    product_name: &str,
    variable: &str,
    raw_variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Vec<f32>> {
    let products = snapshot.product_snapshots(product_name);
    for product in &products {
        if !product_covers_time(product, raw_variable, time) {
            continue;
        }
        return read_product_grid_with_rounding(
            product,
            decoder,
            variable,
            raw_variable,
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

fn read_cams_mixed_carbon_monoxide(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
    round_values: bool,
) -> Result<f32> {
    // Matches Open-Meteo CamsMixer.integrateIfNaNSmooth(width: 3): prefer
    // greenhouse CO, fill its gaps from cams_global, and smooth the preceding
    // three hourly values into a source transition.
    let mut high = Vec::with_capacity(4);
    let mut low = Vec::with_capacity(4);
    for offset in 0..=3 {
        let sample_time = time + Duration::hours(offset);
        high.push(read_product_history_value_with_rounding(
            snapshot,
            decoder,
            "cams_global_greenhouse_gases",
            "carbon_monoxide",
            "carbon_monoxide",
            sample_time,
            latitude,
            longitude,
            false,
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
            false,
        )?);
    }
    smooth_cams_co_values(&mut high, &low);
    Ok(maybe_round_to_scalefactor(high[0], 1.0, round_values))
}

#[allow(clippy::too_many_arguments)]
fn read_cams_mixed_carbon_monoxide_grid(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
    round_values: bool,
) -> Result<Vec<f32>> {
    let point_count = latitudes.len() * longitudes.len();
    let mut high = Vec::with_capacity(4);
    let mut low = Vec::with_capacity(4);
    for offset in 0..=3 {
        let sample_time = time + Duration::hours(offset);
        high.push(read_product_history_grid_with_rounding(
            snapshot,
            decoder,
            "cams_global_greenhouse_gases",
            "carbon_monoxide",
            "carbon_monoxide",
            sample_time,
            latitudes,
            longitudes,
            false,
        )?);
        low.push(read_product_history_grid_with_rounding(
            snapshot,
            decoder,
            "cams_global",
            "carbon_monoxide",
            "carbon_monoxide",
            sample_time,
            latitudes,
            longitudes,
            false,
        )?);
    }
    let mut output = vec![f32::NAN; point_count];
    for point in 0..point_count {
        let mut high_values = [
            high[0][point],
            high[1][point],
            high[2][point],
            high[3][point],
        ];
        let low_values = [low[0][point], low[1][point], low[2][point], low[3][point]];
        smooth_cams_co_values(&mut high_values, &low_values);
        output[point] = maybe_round_to_scalefactor(high_values[0], 1.0, round_values);
    }
    Ok(output)
}

fn smooth_cams_co_values(high: &mut [f32], low: &[f32]) {
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
}

fn product_covers_time(product: &ProductSnapshot, raw_variable: &str, time: DateTime<Utc>) -> bool {
    let native_times = native_times_for_variable(product, raw_variable);
    match (native_times.first(), native_times.last()) {
        (Some(first), Some(last)) => time >= *first && time <= *last,
        _ => false,
    }
}

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
        InterpolationKind::Direct => {
            if !product.entries.contains_key(&EntryKey {
                variable: raw_variable.to_string(),
                valid_time_utc: time,
            }) {
                return Ok(f32::NAN);
            }
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
        InterpolationKind::SolarBackwards { scalefactor } => {
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

#[derive(Debug, Clone, Copy)]
struct SolarBlock {
    b: usize,
    c: usize,
    a: Option<usize>,
    d: Option<usize>,
    fraction: f32,
    direct: bool,
}

fn solar_block(
    product: &ProductSnapshot,
    raw_variable: &str,
    native_times: &[DateTime<Utc>],
    time: DateTime<Utc>,
) -> Option<SolarBlock> {
    if native_times.is_empty() || time < native_times[0] || time > *native_times.last()? {
        return None;
    }
    let (b, c, fraction, direct) = match native_times.binary_search(&time) {
        Ok(index) => {
            if index == 0 {
                return Some(SolarBlock {
                    b: index,
                    c: index,
                    a: None,
                    d: None,
                    fraction: 0.0,
                    direct: true,
                });
            }
            let seconds = (native_times[index] - native_times[index - 1]).num_seconds();
            if seconds <= 3600
                || !entries_share_source_run(
                    product,
                    raw_variable,
                    native_times[index - 1],
                    native_times[index],
                )
            {
                return Some(SolarBlock {
                    b: index,
                    c: index,
                    a: None,
                    d: None,
                    fraction: 0.0,
                    direct: true,
                });
            }
            (index - 1, index, 1.0, false)
        }
        Err(next) if next > 0 && next < native_times.len() => {
            let previous = next - 1;
            let seconds = (native_times[next] - native_times[previous]).num_seconds();
            if seconds <= 0
                || seconds > 12 * 3600
                || !entries_share_source_run(
                    product,
                    raw_variable,
                    native_times[previous],
                    native_times[next],
                )
            {
                return None;
            }
            (
                previous,
                next,
                (time - native_times[previous]).num_seconds() as f32 / seconds as f32,
                false,
            )
        }
        _ => return None,
    };
    let width = native_times[c] - native_times[b];
    let a_time = native_times[b] - width;
    let d_time = native_times[c] + width;
    let a = native_times.binary_search(&a_time).ok().filter(|index| {
        entries_share_source_run(product, raw_variable, native_times[*index], native_times[b])
    });
    let d = native_times.binary_search(&d_time).ok().filter(|index| {
        entries_share_source_run(product, raw_variable, native_times[c], native_times[*index])
    });
    Some(SolarBlock {
        b,
        c,
        a,
        d,
        fraction,
        direct,
    })
}

fn mean_solar_factor(
    start_exclusive: DateTime<Utc>,
    end_inclusive: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> f32 {
    let mut cursor = start_exclusive + Duration::hours(1);
    let mut total = 0.0_f32;
    let mut count = 0_u32;
    while cursor <= end_inclusive {
        total += crate::solar::backwards_averaged_factor(cursor, latitude, longitude);
        count += 1;
        cursor += Duration::hours(1);
    }
    if count == 0 {
        crate::solar::backwards_averaged_factor(end_inclusive, latitude, longitude)
    } else {
        total / count as f32
    }
}

fn source_clearness_index(
    product: &ProductSnapshot,
    raw_variable: &str,
    native_times: &[DateTime<Utc>],
    index: usize,
    raw_value: f32,
    latitude: f64,
    longitude: f64,
) -> f32 {
    const RADIATION_MINIMUM: f32 = 5.0 / 1367.7;
    const RADIATION_LIMIT: f32 = 1367.7 * 0.95;
    if raw_value.is_nan() {
        return f32::NAN;
    }
    let factor = if index > 0
        && entries_share_source_run(
            product,
            raw_variable,
            native_times[index - 1],
            native_times[index],
        )
        && (native_times[index] - native_times[index - 1]).num_seconds() <= 12 * 3600
    {
        mean_solar_factor(
            native_times[index - 1],
            native_times[index],
            latitude,
            longitude,
        )
    } else {
        crate::solar::backwards_averaged_factor(native_times[index], latitude, longitude)
    };
    if factor <= RADIATION_MINIMUM {
        f32::NAN
    } else {
        (raw_value / factor).min(RADIATION_LIMIT)
    }
}

#[allow(clippy::too_many_arguments)]
fn interpolate_solar_block_value(
    product: &ProductSnapshot,
    raw_variable: &str,
    native_times: &[DateTime<Utc>],
    block: SolarBlock,
    time: DateTime<Utc>,
    raw_a: Option<f32>,
    raw_b: f32,
    raw_c: f32,
    raw_d: Option<f32>,
    latitude: f64,
    longitude: f64,
    scalefactor: f32,
    round_values: bool,
) -> f32 {
    let solar = crate::solar::backwards_averaged_factor(time, latitude, longitude);
    if solar == 0.0 {
        return 0.0;
    }
    let mut kt_b = source_clearness_index(
        product,
        raw_variable,
        native_times,
        block.b,
        raw_b,
        latitude,
        longitude,
    );
    let mut kt_c = source_clearness_index(
        product,
        raw_variable,
        native_times,
        block.c,
        raw_c,
        latitude,
        longitude,
    );
    let mut kt_a = block
        .a
        .zip(raw_a)
        .map(|(index, value)| {
            source_clearness_index(
                product,
                raw_variable,
                native_times,
                index,
                value,
                latitude,
                longitude,
            )
        })
        .unwrap_or(kt_b);
    let mut kt_d = block
        .d
        .zip(raw_d)
        .map(|(index, value)| {
            source_clearness_index(
                product,
                raw_variable,
                native_times,
                index,
                value,
                latitude,
                longitude,
            )
        })
        .unwrap_or(kt_c);

    if kt_c.is_nan() && kt_b > 0.0 {
        kt_c = kt_b;
    }
    if kt_c.is_nan() && kt_a > 0.0 {
        kt_b = kt_a;
        kt_c = kt_a;
    }
    if kt_c.is_nan() && kt_d > 0.0 {
        kt_a = kt_d;
        kt_b = kt_d;
        kt_c = kt_d;
    }
    if kt_b.is_nan() {
        kt_b = kt_c;
    }
    if kt_a.is_nan() {
        kt_a = kt_b;
    }
    if kt_d.is_nan() {
        kt_d = kt_c;
    }
    if !kt_b.is_finite() || !kt_c.is_finite() {
        return f32::NAN;
    }

    let fraction = block.fraction;
    let coefficient_a = -kt_a / 2.0 + (3.0 * kt_b) / 2.0 - (3.0 * kt_c) / 2.0 + kt_d / 2.0;
    let coefficient_b = kt_a - (5.0 * kt_b) / 2.0 + 2.0 * kt_c - kt_d / 2.0;
    let coefficient_c = -kt_a / 2.0 + kt_c / 2.0;
    let kt = coefficient_a * fraction.powi(3)
        + coefficient_b * fraction.powi(2)
        + coefficient_c * fraction
        + kt_b;
    let value = if kt < 0.0 && raw_b >= 0.0 && raw_c >= 0.0 {
        (kt_b * (1.0 - fraction) + kt_c * fraction) * solar
    } else {
        kt.max(0.0) * solar
    };
    maybe_round_to_scalefactor(value, scalefactor, round_values)
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
    let Some(block) = solar_block(product, raw_variable, &native_times, time) else {
        return Ok(f32::NAN);
    };
    if block.direct {
        let value = read_native_value(
            product,
            decoder,
            raw_variable,
            native_times[block.b],
            latitude,
            longitude,
        )?;
        return Ok(maybe_round_to_scalefactor(value, scalefactor, round_values));
    }
    let read = |index| {
        read_native_value(
            product,
            decoder,
            raw_variable,
            native_times[index],
            latitude,
            longitude,
        )
    };
    Ok(interpolate_solar_block_value(
        product,
        raw_variable,
        &native_times,
        block,
        time,
        block.a.map(&read).transpose()?,
        read(block.b)?,
        read(block.c)?,
        block.d.map(&read).transpose()?,
        latitude,
        longitude,
        scalefactor,
        round_values,
    ))
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
    let Some(block) = solar_block(product, raw_variable, &native_times, time) else {
        return Ok(vec![f32::NAN; latitudes.len() * longitudes.len()]);
    };
    if block.direct {
        return read_native_grid(
            product,
            decoder,
            raw_variable,
            native_times[block.b],
            latitudes,
            longitudes,
        )
        .map(|mut values| {
            if round_values {
                values
                    .iter_mut()
                    .for_each(|value| *value = round_to_scalefactor(*value, scalefactor));
            }
            values
        });
    }
    let read = |index| {
        read_native_grid(
            product,
            decoder,
            raw_variable,
            native_times[index],
            latitudes,
            longitudes,
        )
    };
    let raw_a = block.a.map(&read).transpose()?;
    let raw_b = read(block.b)?;
    let raw_c = read(block.c)?;
    let raw_d = block.d.map(&read).transpose()?;
    let width = longitudes.len();
    Ok(raw_b
        .iter()
        .zip(&raw_c)
        .enumerate()
        .map(|(index, (b, c))| {
            let latitude = latitudes[index / width];
            let longitude = longitudes[index % width];
            interpolate_solar_block_value(
                product,
                raw_variable,
                &native_times,
                block,
                time,
                raw_a.as_ref().map(|values| values[index]),
                *b,
                *c,
                raw_d.as_ref().map(|values| values[index]),
                latitude,
                longitude,
                scalefactor,
                round_values,
            )
        })
        .collect())
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
    let native_times = native_times_for_variable(product, raw_variable);
    let Some((index, fraction)) = interpolation_index(&native_times, time) else {
        return Ok(f32::NAN);
    };
    if rejects_cross_run_hermite_core(product, raw_variable, &native_times, index, time) {
        return Ok(f32::NAN);
    }
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
    let b_time = native_times[index];
    let a_time = b_time - Duration::seconds(stride_seconds);
    let a = if entries_share_source_run(product, raw_variable, a_time, b_time) {
        match read_native_value_if_present(
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
        }
    } else {
        b
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
    let c_time = native_times[index + 1];
    let d_time = c_time + Duration::seconds(stride_seconds);
    let d = if entries_share_source_run(product, raw_variable, c_time, d_time) {
        match read_native_value_if_present(
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
        }
    } else {
        missing_second_lookahead_value(product, b, c)
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

fn missing_second_lookahead_value(product: &ProductSnapshot, b: f32, c: f32) -> f32 {
    if product.product == "cams_global_greenhouse_gases" {
        b
    } else {
        c
    }
}

fn entries_share_source_run(
    product: &ProductSnapshot,
    raw_variable: &str,
    left: DateTime<Utc>,
    right: DateTime<Utc>,
) -> bool {
    let source_run = |time| {
        product
            .entries
            .get(&EntryKey {
                variable: raw_variable.to_string(),
                valid_time_utc: time,
            })
            .map(|entry| entry.source_run.as_str())
    };
    matches!((source_run(left), source_run(right)), (Some(left), Some(right)) if left == right)
}

fn rejects_cross_run_hermite_core(
    product: &ProductSnapshot,
    raw_variable: &str,
    native_times: &[DateTime<Utc>],
    index: usize,
    time: DateTime<Utc>,
) -> bool {
    index + 1 < native_times.len()
        && native_times[index] != time
        && !entries_share_source_run(
            product,
            raw_variable,
            native_times[index],
            native_times[index + 1],
        )
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

fn round_to_scalefactor(value: f32, scalefactor: f32) -> f32 {
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
enum InterpolationKind {
    Direct,
    Linear {
        scalefactor: f32,
    },
    SolarBackwards {
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

fn interpolation_kind_for_variable(variable: &str) -> InterpolationKind {
    if is_cams_variable(variable) {
        return cams_interpolation_kind(variable);
    }
    if variable.ends_with("hPa") {
        return pressure_interpolation_kind(variable);
    }
    match variable {
        "precipitation" | "showers" | "snowfall_water_equivalent" => {
            InterpolationKind::BackwardsSum { scalefactor: 10.0 }
        }
        "categorical_freezing_rain" | "frozen_precipitation_percent" => {
            InterpolationKind::Backwards { scalefactor: 1.0 }
        }
        "visibility" => InterpolationKind::Linear { scalefactor: 0.05 },
        "freezing_level_height" => InterpolationKind::Linear { scalefactor: 0.1 },
        "snow_depth" => InterpolationKind::Linear { scalefactor: 100.0 },
        "shortwave_radiation" => InterpolationKind::SolarBackwards { scalefactor: 1.0 },
        "uv_index" | "uv_index_clear_sky" => {
            InterpolationKind::SolarBackwards { scalefactor: 20.0 }
        }
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

fn cams_interpolation_kind(variable: &str) -> InterpolationKind {
    let _ = variable;
    // Singapore intentionally serves the original CAMS forecast steps. It
    // does not synthesize hourly values between three-hour source frames.
    InterpolationKind::Direct
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

fn read_optional_direct_unrounded(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<f32>> {
    match read_direct_unrounded(snapshot, decoder, variable, time, latitude, longitude) {
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
    } else if is_gfs025_variable(variable) {
        &["gfs025"]
    } else if is_pressure_variable(variable) {
        &["gfs_pressure_profile"]
    } else {
        &["gfs013_surface"]
    };
    for product in candidates {
        if snapshot.product(product).is_some() {
            return Ok((product, variable.to_string()));
        }
    }
    bail!(
        "product is not available for variable {}: {}",
        variable,
        candidates.join(", ")
    );
}

fn seed_variable_for_times(variable: &str) -> &str {
    match variable {
        "dew_point_2m" | "dewpoint_2m" | "apparent_temperature" => "temperature_2m",
        "surface_pressure" => "temperature_2m",
        "weather_code" | "weathercode" => "cloud_cover",
        "rain" => "precipitation",
        "snowfall" => "snowfall_water_equivalent",
        "wind_speed_10m" | "windspeed_10m" | "wind_direction_10m" | "winddirection_10m" => {
            "wind_u_component_10m"
        }
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
    }
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
    let (read_offset, read_count) = if let Some(time_index) = entry.native_time_index {
        (vec![y0, x0, time_index], vec![height, width, 1])
    } else {
        (vec![y0, x0], vec![height, width])
    };
    let rectangle = decoder.decode_grid(&metadata, &reader, &read_offset, &read_count)?;
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
        let lut_size = entry
            .array
            .lut_size
            .context("array metadata missing lut_size")?;
        let lut_offset = entry
            .array
            .lut_offset
            .context("array metadata missing lut_offset")?;
        if entry.array.chunks.len() != 3 {
            bail!("native runtime entry must be a 3D array");
        }
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
        let x = ((longitude - grid.lon_min) / grid.dx).round() as i64;
        let y = ((latitude - grid.lat_min) / grid.dy).round() as i64;
        if y < 0 || y >= grid.ny as i64 || x < 0 || x >= grid.nx as i64 {
            bail!("point is outside native regional grid");
        }
        return Ok((y as u64, x as u64));
    }
    if array.dimensions.len() != 2 {
        bail!("only 2D bundle entries are supported by the point API");
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
    let x = ((lon + 180.0) / dx).round() as i64;
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
        return Ok((grid.lat_min + y as f64 * grid.dy) as f32);
    }
    if array.dimensions.len() != 2 || y >= array.dimensions[0] {
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

fn gfs013_model_location(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<(f64, f64, f32)>> {
    if let Some(location) = read_gfs_point_metadata(snapshot, decoder, latitude, longitude)? {
        return Ok(Some(location));
    }
    legacy_gfs013_model_location(snapshot, decoder, latitude, longitude)
}

fn legacy_gfs013_model_location(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
) -> Result<Option<(f64, f64, f32)>> {
    let Some(product) = snapshot.product("gfs013_surface") else {
        return Ok(None);
    };
    let entry = product
        .entries
        .values()
        .find(|entry| entry.variable == "temperature_2m")
        .or_else(|| product.entries.values().next())
        .context("gfs013_surface has no grid entries")?;
    let (y, x) = grid_index_for_lat_lon(&entry.array, None, latitude, longitude)?;
    let model_latitude = official_f32_json_number(grid_latitude_for_index(&entry.array, None, y)?)?;
    let model_longitude =
        official_f32_json_number(grid_longitude_for_index(&entry.array, None, x)?)?;

    let Some(decoder) = decoder else {
        return Ok(None);
    };
    let static_path = snapshot.data_root.join(GFS013_STATIC_ELEVATION_PATH);
    if !static_path.exists() {
        bail!(
            "required official GFS013 static elevation file is missing: {}",
            static_path.display()
        );
    }
    let cache_key = (static_path.clone(), y, x);
    let cache = GFS_ELEVATION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(elevation) = cache
        .lock()
        .map_err(|_| anyhow!("GFS elevation cache poisoned"))?
        .get(&cache_key)
        .copied()
    {
        return Ok(Some((model_latitude, model_longitude, elevation)));
    }
    let file = Arc::new(
        File::open(&static_path)
            .with_context(|| format!("failed to open {}", static_path.display()))?,
    );
    if file.metadata()?.len() != GFS013_STATIC_FILE_SIZE {
        bail!("official GFS013 static elevation file size is invalid");
    }
    let metadata = build_v3_array_metadata_blob(
        "",
        20,
        0,
        GFS013_STATIC_DIMENSIONS,
        GFS013_STATIC_CHUNKS,
        GFS013_STATIC_LUT_SIZE,
        GFS013_STATIC_LUT_OFFSET,
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
        .map_err(|_| anyhow!("GFS elevation cache poisoned"))?
        .insert(cache_key, elevation);
    Ok(Some((model_latitude, model_longitude, elevation)))
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
    if array.dimensions.len() != 2 || x >= array.dimensions[1] {
        bail!("invalid longitude grid index");
    }
    let dx = 360.0_f32 / array.dimensions[1] as f32;
    Ok(-180.0_f32 + x as f32 * dx)
}

fn model_latitude_for_variable(
    snapshot: &OmDataSnapshot,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let (product_name, raw_variable) = product_for_variable(snapshot, variable)?;
    for product in snapshot.product_snapshots(product_name) {
        if !product_covers_time(&product, &raw_variable, time) {
            continue;
        }
        // Grid geometry is invariant within a product coverage. Derived
        // hourly values may sit between sparse native frames, so requiring an
        // entry at the exact requested time incorrectly creates holes.
        let entry = product
            .entries
            .get(&EntryKey {
                variable: raw_variable.clone(),
                valid_time_utc: time,
            })
            .or_else(|| {
                product
                    .entries
                    .iter()
                    .find(|(key, _)| key.variable == raw_variable)
                    .map(|(_, entry)| entry)
            });
        let Some(entry) = entry else {
            continue;
        };
        let (y, _) = grid_index_for_lat_lon(
            &entry.array,
            entry.native_grid.as_ref(),
            latitude,
            longitude,
        )?;
        return grid_latitude_for_index(&entry.array, entry.native_grid.as_ref(), y);
    }
    bail!("variable/time is not available: {} {}", variable, time)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn model_coordinates_use_official_shortest_float_representation() {
        assert_eq!(official_f32_json_number(131.953125).unwrap(), 131.95312);
        assert_eq!(official_f32_json_number(75.5859375).unwrap(), 75.58594);
        assert_eq!(official_f32_json_number(82.734375).unwrap(), 82.734375);
        assert_eq!(
            official_f32_json_number(39.06932067871094).unwrap(),
            39.06932
        );
    }

    #[test]
    fn wind_gusts_are_routed_to_gfs025() {
        assert!(is_gfs025_variable("wind_gusts_10m"));
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
    fn dem90_resolution_matches_official_latitude_bands() {
        assert_eq!(dem90_pixel(0), 1200);
        assert_eq!(dem90_pixel(49), 1200);
        assert_eq!(dem90_pixel(50), 800);
        assert_eq!(dem90_pixel(59), 800);
        assert_eq!(dem90_pixel(60), 600);
        assert_eq!(dem90_pixel(80), 240);
        assert_eq!(dem90_pixel(85), 120);
    }

    #[test]
    fn dem90_latitude_chunk_matches_official_swift_truncation() {
        assert_eq!(dem90_latitude_chunk(0.0), 0);
        assert_eq!(dem90_latitude_chunk(58.999), 58);
        assert_eq!(dem90_latitude_chunk(-0.001), -1);
        assert_eq!(dem90_latitude_chunk(-1.25), -2);
    }

    #[test]
    fn apparent_temperature_propagates_explicit_missing_shortwave_only() {
        let fallback = apparent_temperature(20.0, 50.0, 4.0, None);

        assert!(fallback.is_finite());
        assert_eq!(fallback, apparent_temperature(20.0, 50.0, 4.0, Some(550.0)));
        assert!(apparent_temperature(20.0, 50.0, 4.0, Some(f32::NAN)).is_nan());
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
            let mut out = vec![0_u8; count as usize];
            self.bundle_handle.read_exact_at(&mut out, start)?;
            return Ok(out);
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
            .with_context(|| format!("native OM handle is missing: {}", path));
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

fn read_optional_direct_grid_unrounded(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    time: DateTime<Utc>,
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Option<Vec<f32>>> {
    match read_direct_grid(
        snapshot, decoder, variable, time, latitudes, longitudes, false,
    ) {
        Ok(values) => Ok(Some(values)),
        Err(_) => Ok(None),
    }
}

fn read_optional_direct_grid_series_unrounded(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    latitudes: &[f64],
    longitudes: &[f64],
) -> Result<Option<Vec<Vec<f32>>>> {
    match read_direct_grid_series(
        snapshot, decoder, variable, times, latitudes, longitudes, false,
    ) {
        Ok(values) => Ok(Some(values)),
        Err(_) => Ok(None),
    }
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
    let showers = read_direct_grid_series(
        snapshot, decoder, "showers", times, latitudes, longitudes, false,
    )?;
    let cape = read_optional_direct_grid_series_unrounded(
        snapshot, decoder, "cape", times, latitudes, longitudes,
    )?;
    let gusts = read_optional_direct_grid_series_unrounded(
        snapshot,
        decoder,
        "wind_gusts_10m",
        times,
        latitudes,
        longitudes,
    )?;
    let visibility = read_optional_direct_grid_series_unrounded(
        snapshot,
        decoder,
        "visibility",
        times,
        latitudes,
        longitudes,
    )?;
    let freezing_rain = read_optional_direct_grid_series_unrounded(
        snapshot,
        decoder,
        "categorical_freezing_rain",
        times,
        latitudes,
        longitudes,
    )?;
    let lifted_index = read_optional_direct_grid_series_unrounded(
        snapshot,
        decoder,
        "lifted_index",
        times,
        latitudes,
        longitudes,
    )?;
    let cin = read_optional_direct_grid_series_unrounded(
        snapshot,
        decoder,
        "convective_inhibition",
        times,
        latitudes,
        longitudes,
    )?;
    let pbl = read_optional_direct_grid_series_unrounded(
        snapshot,
        decoder,
        "boundary_layer_height",
        times,
        latitudes,
        longitudes,
    )?;

    let (product_name, raw_variable) = product_for_variable(snapshot, "cloud_cover")?;
    let product = snapshot.require_product(product_name)?;
    let exact_cloud_times = times
        .iter()
        .map(|time| {
            product.entries.contains_key(&EntryKey {
                variable: raw_variable.clone(),
                valid_time_utc: *time,
            })
        })
        .collect::<Vec<_>>();
    let entry = times
        .iter()
        .find_map(|time| {
            product.entries.get(&EntryKey {
                variable: raw_variable.clone(),
                valid_time_utc: *time,
            })
        })
        .context("weather-code series has no cloud-cover grid entry")?;
    let model_latitudes = latitudes
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
        .collect::<Result<Vec<_>>>()?;
    let width = longitudes.len();
    let mut output = Vec::with_capacity(times.len());
    for time_index in 0..times.len() {
        if !exact_cloud_times[time_index] {
            output.push(vec![f32::NAN; cloudcover[time_index].len()]);
            continue;
        }
        let mut values = Vec::with_capacity(cloudcover[time_index].len());
        for index in 0..cloudcover[time_index].len() {
            let optional = |series: &Option<Vec<Vec<f32>>>| {
                series.as_ref().map(|series| series[time_index][index])
            };
            values.push(
                weather_code(
                    cloudcover[time_index][index],
                    precipitation[time_index][index],
                    Some(showers[time_index][index]),
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
    let showers = read_direct_grid(
        snapshot, decoder, "showers", time, latitudes, longitudes, false,
    )?;
    let cape = read_optional_direct_grid_unrounded(
        snapshot, decoder, "cape", time, latitudes, longitudes,
    )?;
    let gusts = read_optional_direct_grid_unrounded(
        snapshot,
        decoder,
        "wind_gusts_10m",
        time,
        latitudes,
        longitudes,
    )?;
    let visibility = read_optional_direct_grid_unrounded(
        snapshot,
        decoder,
        "visibility",
        time,
        latitudes,
        longitudes,
    )?;
    let freezing_rain = read_optional_direct_grid_unrounded(
        snapshot,
        decoder,
        "categorical_freezing_rain",
        time,
        latitudes,
        longitudes,
    )?;
    let lifted_index = read_optional_direct_grid_unrounded(
        snapshot,
        decoder,
        "lifted_index",
        time,
        latitudes,
        longitudes,
    )?;
    let cin = read_optional_direct_grid_unrounded(
        snapshot,
        decoder,
        "convective_inhibition",
        time,
        latitudes,
        longitudes,
    )?;
    let pbl = read_optional_direct_grid_unrounded(
        snapshot,
        decoder,
        "boundary_layer_height",
        time,
        latitudes,
        longitudes,
    )?;
    let (product_name, raw_variable) = product_for_variable(snapshot, "cloud_cover")?;
    let product = snapshot.require_product(product_name)?;
    let entry = product
        .entries
        .get(&EntryKey {
            variable: raw_variable,
            valid_time_utc: time,
        })
        .with_context(|| format!("variable/time is not available: cloud_cover {}", time))?;
    let model_latitudes = latitudes
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
        .collect::<Result<Vec<_>>>()?;
    let width = longitudes.len();
    let mut values = Vec::with_capacity(cloudcover.len());
    for index in 0..cloudcover.len() {
        let optional = |values: &Option<Vec<f32>>| values.as_ref().map(|values| values[index]);
        values.push(
            weather_code(
                cloudcover[index],
                precipitation[index],
                Some(showers[index]),
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
    let showers = read_direct_unrounded(snapshot, decoder, "showers", time, latitude, longitude)?;
    let cape =
        read_optional_direct_unrounded(snapshot, decoder, "cape", time, latitude, longitude)?;
    let gusts = read_optional_direct_unrounded(
        snapshot,
        decoder,
        "wind_gusts_10m",
        time,
        latitude,
        longitude,
    )?;
    let visibility =
        read_optional_direct_unrounded(snapshot, decoder, "visibility", time, latitude, longitude)?;
    let freezing_rain = read_optional_direct_unrounded(
        snapshot,
        decoder,
        "categorical_freezing_rain",
        time,
        latitude,
        longitude,
    )?;
    let lifted_index = read_optional_direct_unrounded(
        snapshot,
        decoder,
        "lifted_index",
        time,
        latitude,
        longitude,
    )?;
    let cin = read_optional_direct_unrounded(
        snapshot,
        decoder,
        "convective_inhibition",
        time,
        latitude,
        longitude,
    )?;
    let pbl = read_optional_direct_unrounded(
        snapshot,
        decoder,
        "boundary_layer_height",
        time,
        latitude,
        longitude,
    )?;
    Ok(weather_code(
        cloudcover,
        precipitation,
        Some(showers),
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

fn wind_direction(u: f32, v: f32) -> f32 {
    if v == 0.0 {
        return if u < 0.0 { 90.0 } else { 270.0 };
    }
    if u == 0.0 {
        return if v < 0.0 { 360.0 } else { 180.0 };
    }
    180.0 + u.atan2(v).to_degrees()
}

fn precip_phase(code: f32) -> f32 {
    match code as i32 {
        51 | 53 | 55 | 61 | 63 | 65 | 80 | 81 | 82 => 1.0,
        71 | 73 | 75 | 77 | 85 | 86 => 2.0,
        56 | 57 | 66 | 67 => 4.0,
        _ => 0.0,
    }
}

fn dew_point(temperature: f32, relative_humidity: f32) -> f32 {
    let beta = 17.625_f32;
    let lambda = 243.04_f32;
    let x = (relative_humidity / 100.0).ln() + ((beta * temperature) / (lambda + temperature));
    lambda * x / (beta - x)
}

fn apparent_temperature(
    temperature_2m: f32,
    relative_humidity_2m: f32,
    wind_speed_10m: f32,
    shortwave_radiation: Option<f32>,
) -> f32 {
    let shortwave_radiation = match shortwave_radiation {
        Some(value) if !value.is_finite() => return f32::NAN,
        Some(value) => value,
        None => 550.0,
    };
    let wind_speed_2m = wind_speed_10m * 0.75;
    let vapor_pressure = relative_humidity_2m / 100.0
        * 6.105
        * (17.27 * temperature_2m / (237.7 + temperature_2m)).exp();
    let radiation = (0.1 * (shortwave_radiation - 550.0)).max(0.0);
    temperature_2m + 0.348 * vapor_pressure - 0.70 * wind_speed_2m
        + 0.70 * (radiation / (wind_speed_2m + 10.0))
        - 4.25
}

fn unit_for_variable(variable: &str) -> &'static str {
    if variable.ends_with("hPa") {
        if variable.starts_with("temperature_") {
            return "°C";
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
        | "apparent_temperature"
        | "temperature_80m"
        | "temperature_100m"
        | "dew_point_2m"
        | "dewpoint_2m"
        | "surface_temperature"
        | "soil_temperature_0_to_10cm"
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
        | "cloudcover_high" => "%",
        "precipitation" | "showers" | "rain" | "snowfall_water_equivalent" => "mm",
        "snowfall" => "cm",
        "wind_u_component_10m"
        | "wind_v_component_10m"
        | "wind_u_component_80m"
        | "wind_v_component_80m"
        | "wind_u_component_100m"
        | "wind_v_component_100m"
        | "wind_speed_10m"
        | "windspeed_10m"
        | "wind_gusts_10m" => "m/s",
        "wind_direction_10m" | "winddirection_10m" => "°",
        "pressure_msl" | "surface_pressure" => "hPa",
        "visibility" => "m",
        "freezing_level_height" | "boundary_layer_height" | "snow_depth" => "m",
        "weather_code" | "weathercode" => "wmo code",
        "precip_phase" | "thunderstorm_code" => "",
        "pm2_5" | "pm10" | "dust" | "carbon_monoxide" | "nitrogen_dioxide" | "ozone"
        | "sulphur_dioxide" => "μg/m³",
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
        "uv_index" | "uv_index_clear_sky" | "lifted_index" | "categorical_freezing_rain" => "",
        "cape" | "convective_inhibition" => "J/kg",
        "shortwave_radiation" | "diffuse_radiation" | "latent_heat_flux" | "sensible_heat_flux" => {
            "W/m\u{00B2}"
        }
        "soil_moisture_0_to_10cm"
        | "soil_moisture_10_to_40cm"
        | "soil_moisture_40_to_100cm"
        | "soil_moisture_100_to_200cm" => "m\u{00B3}/m\u{00B3}",
        "total_column_integrated_water_vapour" => "kg/m\u{00B2}",
        _ => "unknown",
    }
}

fn json_array_for_daily_variable(
    variable: &str,
    aggregation: DailyWeatherAggregation,
    values: Vec<f32>,
) -> serde_json::Value {
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
}

fn output_decimals_for_variable(variable: &str) -> OutputDecimals {
    if variable.ends_with("hPa") {
        if variable.starts_with("geopotential_height_") {
            return OutputDecimals::Fixed(2);
        }
        if variable.starts_with("temperature_") {
            return OutputDecimals::Fixed(1);
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
        | "chinese_aqi_carbon_monoxide" => OutputDecimals::Fixed(4),
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
        | "wind_direction_10m"
        | "winddirection_10m"
        | "categorical_freezing_rain" => OutputDecimals::Integer,
        "wind_speed_10m"
        | "windspeed_10m"
        | "wind_u_component_10m"
        | "wind_v_component_10m"
        | "wind_u_component_80m"
        | "wind_v_component_80m"
        | "wind_u_component_100m"
        | "wind_v_component_100m"
        | "vertical_velocity"
        | "aerosol_optical_depth" => OutputDecimals::Fixed(2),
        "soil_moisture_0_to_10cm"
        | "soil_moisture_10_to_40cm"
        | "soil_moisture_40_to_100cm"
        | "soil_moisture_100_to_200cm" => OutputDecimals::Fixed(3),
        "snowfall" | "snow_depth" | "uv_index" | "uv_index_clear_sky" => OutputDecimals::Fixed(2),
        "temperature_2m"
        | "apparent_temperature"
        | "temperature_80m"
        | "temperature_100m"
        | "dew_point_2m"
        | "dewpoint_2m"
        | "surface_temperature"
        | "soil_temperature_0_to_10cm"
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
        _ => OutputDecimals::Fixed(1),
    }
}
