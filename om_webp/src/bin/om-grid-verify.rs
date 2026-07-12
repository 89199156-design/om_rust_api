use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, ValueEnum};
use om_api::official::OfficialDecoder;
use om_api::query::{read_variable_grid, read_variable_value, round_variable_output_value};
use om_api::snapshot::OmDataSnapshot;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Scope {
    Gfs,
    Cams,
}

#[derive(Debug, Parser)]
#[command(about = "Verify regional OM grid reads against the point-query path")]
struct Args {
    #[arg(long, value_enum)]
    scope: Scope,
    #[arg(long)]
    data_root: PathBuf,
    #[arg(long)]
    decoder_lib: PathBuf,
    #[arg(long)]
    time: DateTime<Utc>,
    #[arg(long, default_value_t = 32)]
    samples: usize,
}

#[derive(Debug, Serialize)]
struct VariableResult {
    variable: &'static str,
    compared: usize,
    max_abs_difference: f32,
}

const GFS_VARIABLES: &[&str] = &[
    "cloud_cover",
    "cloud_cover_high",
    "cloud_cover_mid",
    "cloud_cover_low",
    "temperature_2m",
    "dew_point_2m",
    "relative_humidity_2m",
    "wind_u_component_10m",
    "wind_v_component_10m",
    "precipitation",
    "snow_depth",
    "wind_gusts_10m",
    "visibility",
    "weather_code",
    "cape",
    "pressure_msl",
    "surface_pressure",
    "uv_index",
];

const CAMS_VARIABLES: &[&str] = &["pm2_5", "pm10", "aerosol_optical_depth", "dust"];

fn main() -> Result<()> {
    let args = Args::parse();
    let snapshot = OmDataSnapshot::load(&args.data_root)?;
    let decoder = OfficialDecoder::load(&args.decoder_lib)?;
    let (latitudes, longitudes) = region_grid();
    let variables = match args.scope {
        Scope::Gfs => GFS_VARIABLES,
        Scope::Cams => CAMS_VARIABLES,
    };
    let indices = sample_indices(latitudes.len() * longitudes.len(), args.samples.max(3));
    let mut results = Vec::new();
    for variable in variables {
        let grid = match read_variable_grid(
            &snapshot,
            &decoder,
            variable,
            args.time,
            &latitudes,
            &longitudes,
        ) {
            Ok(values) => values,
            Err(error) if error.to_string().contains("variable/time is not available") => {
                println!("[om-grid-verify] variable={variable} unavailable");
                continue;
            }
            Err(error) => return Err(error).with_context(|| format!("grid read {variable}")),
        };
        let mut compared = 0;
        let mut max_abs_difference = 0.0_f32;
        for index in &indices {
            let y = index / longitudes.len();
            let x = index % longitudes.len();
            let point = read_variable_value(
                &snapshot,
                Some(&decoder),
                variable,
                args.time,
                latitudes[y],
                longitudes[x],
            )?;
            let grid_value = round_variable_output_value(variable, grid[*index]);
            let point_value = round_variable_output_value(variable, point);
            if grid_value.is_nan() && point_value.is_nan() {
                continue;
            }
            let difference = (grid_value - point_value).abs();
            if !difference.is_finite() || difference > 0.000_1 {
                bail!(
                    "mismatch variable={} index={} lat={} lon={} grid={} point={}",
                    variable,
                    index,
                    latitudes[y],
                    longitudes[x],
                    grid_value,
                    point_value
                );
            }
            max_abs_difference = max_abs_difference.max(difference);
            compared += 1;
        }
        results.push(VariableResult {
            variable,
            compared,
            max_abs_difference,
        });
    }
    println!(
        "{}",
        serde_json::json!({
            "status": "success",
            "scope": match args.scope { Scope::Gfs => "gfs", Scope::Cams => "cams" },
            "time": args.time,
            "grid": format!("{}x{}", longitudes.len(), latitudes.len()),
            "samples_per_variable": indices.len(),
            "variables": results,
        })
    );
    Ok(())
}

fn region_grid() -> (Vec<f64>, Vec<f64>) {
    let dx = 360.0 / 3072.0;
    let dy = 0.11714935;
    let lat_origin = -dy * (1536.0 - 1.0) / 2.0;
    let x0 = (((70.0_f64 + 180.0) / dx) - 1e-9).ceil() as usize;
    let x1 = (((140.0_f64 + 180.0) / dx) + 1e-9).floor() as usize;
    let y0 = (((0.0_f64 - lat_origin) / dy) - 1e-9).ceil() as usize;
    let y1 = (((58.0_f64 - lat_origin) / dy) + 1e-9).floor() as usize;
    let longitudes = (x0..=x1).map(|x| round6(-180.0 + x as f64 * dx)).collect();
    let latitudes = (y0..=y1)
        .rev()
        .map(|y| round6(lat_origin + y as f64 * dy))
        .collect();
    (latitudes, longitudes)
}

fn sample_indices(length: usize, count: usize) -> Vec<usize> {
    let mut indices = vec![0, length / 2, length - 1];
    let mut state = 0x9e37_79b9_u64;
    while indices.len() < count {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let index = (state % length as u64) as usize;
        if !indices.contains(&index) {
            indices.push(index);
        }
    }
    indices
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}
