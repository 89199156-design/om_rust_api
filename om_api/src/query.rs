use crate::manifest::{ArrayMetadata, BundleEntry, EntryKey, ProductSnapshot};
use crate::official::{build_v3_array_metadata_blob, BundleRangeReader, OfficialDecoder};
use crate::snapshot::OmDataSnapshot;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

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

pub fn forecast_for_query(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    query: &PointQuery,
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
    let start = parse_hour(query.start_hour.as_deref())?;
    let end = parse_hour(query.end_hour.as_deref())?;

    let mut responses = Vec::new();
    for (latitude, longitude) in latitudes.into_iter().zip(longitudes.into_iter()) {
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
        if !daily_variables.is_empty() {
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
                match read_variable_value(
                    snapshot, decoder, variable, *time, latitude, longitude,
                ) {
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

    Ok(ForecastResponse {
        latitude,
        longitude,
        generationtime_ms: started.elapsed().as_secs_f64() * 1000.0,
        utc_offset_seconds: 0,
        timezone: "GMT".to_string(),
        timezone_abbreviation: "GMT".to_string(),
        elevation: None,
        hourly_units,
        hourly,
        daily_units: None,
        daily: None,
    })
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
                snapshot,
                decoder,
                variable,
                date,
                latitude,
                longitude,
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
    let public_start = product.manifest.public_start_utc.unwrap_or(first).max(first);
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

fn read_variable_value(
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
            return Ok(surface_pressure(temperature, pressure_msl, 0.0));
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
        &[0.0, 12.0, 35.5, 55.5, 150.5, 250.5, 350.1, 500.5],
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
        daily_mean(snapshot, decoder, "nitrogen_dioxide", day_start, latitude, longitude)?,
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
        daily_mean(snapshot, decoder, "sulphur_dioxide", day_start, latitude, longitude)?,
        &HJ633_SO2_DAILY,
        &HJ633_AQI_BREAKPOINTS,
        500.0,
        0,
    );
    let co = chinese_daily_iaqi(
        daily_mean(snapshot, decoder, "carbon_monoxide", day_start, latitude, longitude)? / 1000.0,
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
        snapshot,
        decoder,
        variable,
        day_start,
        latitude,
        longitude,
        24,
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
    let Some((&last_concentration, &last_aqi)) = concentration_breakpoints
        .last()
        .zip(aqi_breakpoints.last())
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
    for hour in 1..=hours {
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
    let factor = (1.0 - (0.0065 * elevation) / t0).powf(-5.255_781_3);
    pressure_msl / factor
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
    // the three preceding hours into that transition.
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
    Ok(maybe_round_to_scalefactor(high[0], 1.0, round_values))
}

fn product_covers_time(
    product: &ProductSnapshot,
    raw_variable: &str,
    time: DateTime<Utc>,
) -> bool {
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
        InterpolationKind::Direct => {}
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
        "dew_point_2m" | "dewpoint_2m" => "temperature_2m",
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

fn read_entry_value(
    product: &ProductSnapshot,
    entry: &BundleEntry,
    decoder: Option<&OfficialDecoder>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let (y, x) = grid_index_for_lat_lon(&entry.array, latitude, longitude)?;
    ensure_in_selection(entry, y, x)?;
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
    let reader = EntryBundleReader::new(product.bundle_handle.clone(), entry.clone());
    decoder.decode_point(&metadata, &reader, &[y, x])
}

fn grid_index_for_lat_lon(
    array: &ArrayMetadata,
    latitude: f64,
    longitude: f64,
) -> Result<(u64, u64)> {
    if array.dimensions.len() != 2 {
        bail!("only 2D OM entries are supported by the point API");
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

fn grid_latitude_for_index(array: &ArrayMetadata, y: u64) -> Result<f32> {
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

fn model_latitude_for_variable(
    snapshot: &OmDataSnapshot,
    variable: &str,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let (product_name, raw_variable) = product_for_variable(snapshot, variable)?;
    let product = snapshot.require_product(product_name)?;
    let key = EntryKey {
        variable: raw_variable,
        valid_time_utc: time,
    };
    let entry = product
        .entries
        .get(&key)
        .with_context(|| format!("variable/time is not available: {} {}", variable, time))?;
    let (y, _) = grid_index_for_lat_lon(&entry.array, latitude, longitude)?;
    grid_latitude_for_index(&entry.array, y)
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

        let (y, x) = grid_index_for_lat_lon(&array, 4.2, 75.3).unwrap();

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

        let (y, x) = grid_index_for_lat_lon(&array, 11.6, 85.9).unwrap();

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

        let (y, _) = grid_index_for_lat_lon(&array, 22.75, 125.0).unwrap();
        let model_latitude = grid_latitude_for_index(&array, y).unwrap();

        assert!((model_latitude - 22.78555).abs() < 0.00001);
    }

    #[test]
    fn wind_gusts_are_routed_to_gfs025() {
        assert!(is_gfs025_variable("wind_gusts_10m"));
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
    let reader = EntryBundleReader::new(product.bundle_handle.clone(), entry.clone());
    let bytes = reader.read_original_range(original_start, 4)?;
    Ok(f32::from_le_bytes(
        bytes.try_into().expect("length checked"),
    ))
}

#[derive(Debug)]
struct EntryBundleReader {
    bundle_handle: Arc<File>,
    entry: BundleEntry,
}

impl EntryBundleReader {
    fn new(bundle_handle: Arc<File>, entry: BundleEntry) -> Self {
        Self {
            bundle_handle,
            entry,
        }
    }
}

impl BundleRangeReader for EntryBundleReader {
    fn read_original_range(&self, start: u64, count: u64) -> Result<Vec<u8>> {
        let end = start
            .checked_add(count)
            .ok_or_else(|| anyhow!("range overflow"))?;
        let mut remaining_start = start;
        let remaining_end = end;
        let mut out = Vec::with_capacity(count as usize);
        let mut local_cursor = self.entry.bundle_offset;
        let mut file = self.bundle_handle.try_clone()?;

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
            file.seek(SeekFrom::Start(local_offset))?;
            let before = out.len();
            out.resize(before + part_len as usize, 0);
            file.read_exact(&mut out[before..])?;
            remaining_start = part_end;
            if remaining_start == remaining_end {
                return Ok(out);
            }
            local_cursor += len;
        }
        bail!("requested original range is not present in bundle")
    }
}

fn read_weather_code(
    snapshot: &OmDataSnapshot,
    decoder: Option<&OfficialDecoder>,
    time: DateTime<Utc>,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let model_latitude = model_latitude_for_variable(
        snapshot,
        "cloud_cover",
        time,
        latitude,
        longitude,
    )?;
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
        | "european_aqi_sulphur_dioxide" => "European AQI",
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
        | "us_aqi_carbon_monoxide" => "US AQI",
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
            "W/m2"
        }
        "soil_moisture_0_to_10cm"
        | "soil_moisture_10_to_40cm"
        | "soil_moisture_40_to_100cm"
        | "soil_moisture_100_to_200cm" => "m3/m3",
        "total_column_integrated_water_vapour" => "kg/m2",
        _ => "unknown",
    }
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

enum OutputDecimals {
    Integer,
    Fixed(u8),
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
        | "us_aqi_carbon_monoxide"
        | "chinese_aqi"
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
        "snowfall" | "uv_index" | "uv_index_clear_sky" => OutputDecimals::Fixed(2),
        "temperature_2m"
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
        | "snow_depth"
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
