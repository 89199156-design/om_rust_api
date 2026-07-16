use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use image::RgbaImage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const WIDTH: usize = 597;
const HEIGHT: usize = 495;

#[derive(Debug, Parser)]
#[command(about = "Strictly compare production WebP pixels with the HTTP point API")]
struct Args {
    #[arg(long, default_value = "/opt/1panel/apps/weather/data")]
    public_root: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:8088")]
    api_base: String,
    #[arg(long, default_value = "/data/om_raw")]
    raw_root: PathBuf,
    #[arg(long, default_value_t = 5000)]
    points: usize,
    #[arg(
        long,
        default_value = "/opt/1panel/apps/weather_om_webp/reports/webp_api_5000.json"
    )]
    report: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    source_release_id: String,
    source_run: String,
    batch: i64,
    files: Vec<i64>,
    grid: GridManifest,
    layers: BTreeMap<String, LayerManifest>,
}

#[derive(Debug, Deserialize)]
struct Ready {
    status: String,
    release_id: String,
}

#[derive(Debug, Deserialize)]
struct GridManifest {
    width: usize,
    height: usize,
    sample_bounds: Bounds,
    dx: f64,
    dy: f64,
}

#[derive(Debug, Deserialize)]
struct Bounds {
    lon_min: f64,
    lat_max: f64,
}

#[derive(Debug, Deserialize)]
struct LayerManifest {
    vmin: f32,
    scale: f32,
}

#[derive(Debug, Clone, Copy)]
enum Encoding {
    Scalar,
    Wind,
}

#[derive(Debug, Clone, Copy)]
enum Derive {
    None,
    PrecipPhase,
    ThunderstormCode,
}

#[derive(Debug, Clone, Copy)]
struct LayerSpec {
    name: &'static str,
    variable: &'static str,
    variable_v: Option<&'static str>,
    multiplier: f32,
    encoding: Encoding,
    derive: Derive,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    flat_index: usize,
    x: usize,
    y: usize,
    latitude: f64,
    longitude: f64,
}

#[derive(Debug, Serialize)]
struct ScopeReport {
    release_id: String,
    run: String,
    points: usize,
    layers: usize,
    comparisons: usize,
    api_requests: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    status: &'static str,
    generated_at: DateTime<Utc>,
    requested_unique_grid_points: usize,
    total_pixel_comparisons: usize,
    elapsed_seconds: f64,
    scopes: BTreeMap<String, ScopeReport>,
}

const GFS_LAYERS: &[LayerSpec] = &[
    scalar("cloud_total_1", "cloud_cover"),
    scalar("cloud_high_1", "cloud_cover_high"),
    scalar("cloud_mid_1", "cloud_cover_mid"),
    scalar("cloud_low_1", "cloud_cover_low"),
    scalar("t2m", "temperature_2m"),
    scalar("d2m", "dew_point_2m"),
    scalar("r2", "relative_humidity_2m"),
    LayerSpec {
        name: "wind",
        variable: "wind_u_component_10m",
        variable_v: Some("wind_v_component_10m"),
        multiplier: 1.0,
        encoding: Encoding::Wind,
        derive: Derive::None,
    },
    scalar("tp", "precipitation"),
    scaled("snod", "snow_depth", 1000.0),
    scalar("gust", "wind_gusts_10m"),
    scalar("vis", "visibility"),
    derived("precip_phase", Derive::PrecipPhase),
    derived("thunderstorm_code", Derive::ThunderstormCode),
    scalar("cape", "cape"),
    scaled("prmsl", "pressure_msl", 100.0),
    scaled("sp", "surface_pressure", 100.0),
    scalar("uv_index", "uv_index"),
];

const CAMS_LAYERS: &[LayerSpec] = &[
    scalar("pm2_5", "pm2_5"),
    scalar("pm10", "pm10"),
    scalar("aerosol_optical_depth", "aerosol_optical_depth"),
    scalar("dust", "dust"),
];

const fn scalar(name: &'static str, variable: &'static str) -> LayerSpec {
    scaled(name, variable, 1.0)
}

const fn scaled(name: &'static str, variable: &'static str, multiplier: f32) -> LayerSpec {
    LayerSpec {
        name,
        variable,
        variable_v: None,
        multiplier,
        encoding: Encoding::Scalar,
        derive: Derive::None,
    }
}

const fn derived(name: &'static str, derive: Derive) -> LayerSpec {
    LayerSpec {
        name,
        variable: "weather_code",
        variable_v: None,
        multiplier: 1.0,
        encoding: Encoding::Scalar,
        derive,
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.points == 0 || args.points > WIDTH * HEIGHT {
        bail!("points must be between 1 and {}", WIDTH * HEIGHT);
    }
    let started = Instant::now();
    let gfs_link = args.public_root.join("gfs013_surface");
    let gfs_root = gfs_link.canonicalize()?;
    let gfs_manifest = load_manifest(&gfs_root.join("gfs013_surface_data.json"))?;
    let gfs_release_id = gfs_manifest.source_release_id.clone();
    ensure_current_release(&args.raw_root, "gfs", &gfs_release_id)?;
    let samples = sample_points(&gfs_manifest.grid, args.points)?;
    let mut scopes = BTreeMap::new();
    let gfs = verify_scope(
        "gfs",
        "v1/forecast",
        &gfs_root,
        gfs_manifest,
        GFS_LAYERS,
        &samples,
        &args.api_base,
    )?;
    ensure_current_release(&args.raw_root, "gfs", &gfs_release_id)?;
    if gfs_link.canonicalize()? != gfs_root {
        bail!("GFS public release changed during verification");
    }
    scopes.insert("gfs".to_string(), gfs);
    let cams_link = args.public_root.join("cams_global");
    let cams_root = cams_link.canonicalize()?;
    let cams_manifest = load_manifest(&cams_root.join("cams_global_data.json"))?;
    let cams_release_id = cams_manifest.source_release_id.clone();
    ensure_current_release(&args.raw_root, "cams", &cams_release_id)?;
    ensure_same_grid(&samples, &cams_manifest.grid)?;
    let cams = verify_scope(
        "cams",
        "v1/air-quality",
        &cams_root,
        cams_manifest,
        CAMS_LAYERS,
        &samples,
        &args.api_base,
    )?;
    ensure_current_release(&args.raw_root, "cams", &cams_release_id)?;
    if cams_link.canonicalize()? != cams_root {
        bail!("CAMS public release changed during verification");
    }
    scopes.insert("cams".to_string(), cams);
    let total_pixel_comparisons = scopes.values().map(|scope| scope.comparisons).sum();
    let report = Report {
        status: "success",
        generated_at: Utc::now(),
        requested_unique_grid_points: samples.len(),
        total_pixel_comparisons,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        scopes,
    };
    if let Some(parent) = args.report.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_vec_pretty(&report)?;
    let temporary = args
        .report
        .with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&temporary, &payload)?;
    fs::rename(temporary, &args.report)?;
    println!("{}", String::from_utf8(payload)?);
    Ok(())
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

fn ensure_current_release(raw_root: &Path, group: &str, release_id: &str) -> Result<()> {
    let path = raw_root
        .join("groups")
        .join(group)
        .join("current/ready_for_processing.json");
    let ready: Ready = serde_json::from_slice(&fs::read(&path)?)?;
    if ready.status != "complete" || ready.release_id != release_id {
        bail!(
            "{} WebP release {} does not match current API release {}",
            group,
            release_id,
            ready.release_id
        );
    }
    Ok(())
}

fn sample_points(grid: &GridManifest, count: usize) -> Result<Vec<Sample>> {
    if (grid.width, grid.height) != (WIDTH, HEIGHT) {
        bail!("unexpected production grid {}x{}", grid.width, grid.height);
    }
    let dx = 360.0 / 3072.0;
    let dy = 0.11714935;
    let lat_origin = -dy * (1536.0 - 1.0) / 2.0;
    let x0 = (((70.0_f64 + 180.0) / dx) - 1e-9).ceil() as usize;
    let y1 = (((58.0_f64 - lat_origin) / dy) + 1e-9).floor() as usize;
    if grid.dx != round6(dx)
        || grid.dy != round6(dy)
        || grid.sample_bounds.lon_min != round6(-180.0 + x0 as f64 * dx)
        || grid.sample_bounds.lat_max != round6(lat_origin + y1 as f64 * dy)
    {
        bail!("production manifest grid does not match the GFS013 grid contract");
    }
    let mut indices = Vec::with_capacity(count);
    let mut state = 0xd1b5_4a32_d192_ed03_u64;
    while indices.len() < count {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let index = (state % (WIDTH * HEIGHT) as u64) as usize;
        if !indices.contains(&index) {
            indices.push(index);
        }
    }
    Ok(indices
        .into_iter()
        .map(|flat_index| {
            let x = flat_index % WIDTH;
            let y = flat_index / WIDTH;
            Sample {
                flat_index,
                x,
                y,
                latitude: round6(lat_origin + (y1 - y) as f64 * dy),
                longitude: round6(-180.0 + (x0 + x) as f64 * dx),
            }
        })
        .collect())
}

fn ensure_same_grid(_samples: &[Sample], grid: &GridManifest) -> Result<()> {
    if (grid.width, grid.height) != (WIDTH, HEIGHT) {
        bail!("CAMS grid dimensions differ from GFS");
    }
    if grid.sample_bounds.lon_min != 70.078125
        || grid.sample_bounds.lat_max != 57.930354
        || grid.dx != 0.117188
        || grid.dy != 0.117149
    {
        bail!("CAMS and GFS manifest grids differ");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn verify_scope(
    scope: &str,
    endpoint: &str,
    product_root: &Path,
    manifest: Manifest,
    layers: &[LayerSpec],
    samples: &[Sample],
    api_base: &str,
) -> Result<ScopeReport> {
    if manifest.files.len() != 121 {
        bail!(
            "{scope} manifest has {} frames, expected 121",
            manifest.files.len()
        );
    }
    let mut comparisons = 0;
    let mut api_requests = 0;
    for (frame_index, timestamp) in manifest.files.iter().enumerate() {
        let frame_samples = samples
            .iter()
            .filter(|sample| sample.flat_index % manifest.files.len() == frame_index)
            .copied()
            .collect::<Vec<_>>();
        if frame_samples.is_empty() {
            continue;
        }
        let responses = fetch_api(api_base, endpoint, layers, *timestamp, &frame_samples)?;
        api_requests += 1;
        if responses.len() != frame_samples.len() {
            bail!(
                "{scope} response count {} != {}",
                responses.len(),
                frame_samples.len()
            );
        }
        let stem = format!("{}_{}", timestamp, manifest.batch);
        for layer in layers {
            let layer_manifest = manifest
                .layers
                .get(layer.name)
                .with_context(|| format!("manifest missing layer {}", layer.name))?;
            let image_path = product_root.join(layer.name).join(format!("{stem}.webp"));
            let image = image::open(&image_path)
                .with_context(|| format!("decode {}", image_path.display()))?
                .to_rgba8();
            if (image.width(), image.height()) != (WIDTH as u32, HEIGHT as u32) {
                bail!("invalid image dimensions for {}", image_path.display());
            }
            compare_layer(
                scope,
                *timestamp,
                layer,
                layer_manifest,
                &image,
                &frame_samples,
                &responses,
            )?;
            comparisons += frame_samples.len();
        }
    }
    Ok(ScopeReport {
        release_id: manifest.source_release_id,
        run: manifest.source_run,
        points: samples.len(),
        layers: layers.len(),
        comparisons,
        api_requests,
    })
}

fn fetch_api(
    api_base: &str,
    endpoint: &str,
    layers: &[LayerSpec],
    timestamp: i64,
    samples: &[Sample],
) -> Result<Vec<Value>> {
    let mut variables = Vec::new();
    for layer in layers {
        if !variables.contains(&layer.variable) {
            variables.push(layer.variable);
        }
        if let Some(variable) = layer.variable_v {
            if !variables.contains(&variable) {
                variables.push(variable);
            }
        }
    }
    let latitude = samples
        .iter()
        .map(|sample| format!("{:.6}", sample.latitude))
        .collect::<Vec<_>>()
        .join(",");
    let longitude = samples
        .iter()
        .map(|sample| format!("{:.6}", sample.longitude))
        .collect::<Vec<_>>()
        .join(",");
    let hour = DateTime::from_timestamp(timestamp, 0)
        .context("manifest timestamp is out of range")?
        .format("%Y-%m-%dT%H:00")
        .to_string();
    let output = Command::new("/usr/bin/curl")
        .args([
            "-sS",
            "--fail-with-body",
            "--get",
            &format!("{}/{}", api_base.trim_end_matches('/'), endpoint),
        ])
        .args(["--data", &format!("latitude={latitude}")])
        .args(["--data", &format!("longitude={longitude}")])
        .args(["--data", &format!("hourly={}", variables.join(","))])
        .args(["--data", &format!("start_hour={hour}")])
        .args(["--data", &format!("end_hour={hour}")])
        .output()?;
    if !output.status.success() {
        bail!(
            "API request failed at {}: stderr={} body={}",
            hour,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout)?;
    match value {
        Value::Array(values) => Ok(values),
        Value::Object(_) if samples.len() == 1 => Ok(vec![value]),
        _ => bail!("unexpected API response shape"),
    }
}

#[allow(clippy::too_many_arguments)]
fn compare_layer(
    scope: &str,
    timestamp: i64,
    layer: &LayerSpec,
    manifest: &LayerManifest,
    image: &RgbaImage,
    samples: &[Sample],
    responses: &[Value],
) -> Result<()> {
    for (sample, response) in samples.iter().zip(responses) {
        let actual = image.get_pixel(sample.x as u32, sample.y as u32).0;
        let first = api_value(response, layer.variable)?;
        let expected = match layer.encoding {
            Encoding::Scalar => encode_scalar(
                first.map(|value| derive_value(value, layer.derive) * layer.multiplier),
                manifest.vmin,
                manifest.scale,
            ),
            Encoding::Wind => encode_wind(
                first,
                api_value(response, layer.variable_v.expect("wind v"))?,
            ),
        };
        if actual != expected {
            bail!(
                "pixel mismatch scope={} layer={} timestamp={} flat_index={} x={} y={} lat={} lon={} api_value={:?} actual={:?} expected={:?}",
                scope,
                layer.name,
                timestamp,
                sample.flat_index,
                sample.x,
                sample.y,
                sample.latitude,
                sample.longitude,
                first,
                actual,
                expected
            );
        }
    }
    Ok(())
}

fn api_value(response: &Value, variable: &str) -> Result<Option<f32>> {
    let value = response
        .get("hourly")
        .and_then(|hourly| hourly.get(variable))
        .and_then(Value::as_array)
        .and_then(|values| values.first())
        .with_context(|| format!("API response missing hourly.{variable}[0]"))?;
    if value.is_null() {
        Ok(None)
    } else {
        Ok(Some(
            value.as_f64().context("API value is not numeric")? as f32
        ))
    }
}

fn encode_scalar(value: Option<f32>, vmin: f32, scale: f32) -> [u8; 4] {
    let Some(value) = value.filter(|value| value.is_finite()) else {
        return [0, 0, 0, 0];
    };
    let encoded = ((value - vmin) * scale).round().clamp(0.0, 65535.0) as u16;
    [(encoded >> 8) as u8, encoded as u8, 0, 255]
}

fn encode_wind(u: Option<f32>, v: Option<f32>) -> [u8; 4] {
    let (Some(u), Some(v)) = (u, v) else {
        return [0, 0, 0, 0];
    };
    let speed = (u * u + v * v).sqrt();
    if !u.is_finite()
        || !v.is_finite()
        || speed > 150.0
        || !(-100.0..=100.0).contains(&u)
        || !(-100.0..=100.0).contains(&v)
    {
        return [0, 0, 0, 0];
    }
    let eu = (u / 0.1).round().clamp(-1000.0, 3095.0) as i32 + 1000;
    let ev = (v / 0.1).round().clamp(-1000.0, 3095.0) as i32 + 1000;
    let u12 = eu as u16;
    let v12 = ev as u16;
    [
        (u12 >> 4) as u8,
        (((u12 & 0x0f) << 4) | (v12 >> 8)) as u8,
        v12 as u8,
        255,
    ]
}

fn derive_value(value: f32, derive: Derive) -> f32 {
    let code = value.round() as i32;
    match derive {
        Derive::None => value,
        Derive::PrecipPhase => match code {
            51 | 53 | 55 | 61 | 63 | 65 | 80 | 81 | 82 => 1.0,
            71 | 73 | 75 | 77 | 85 | 86 => 2.0,
            56 | 57 | 66 | 67 => 4.0,
            _ => 0.0,
        },
        Derive::ThunderstormCode if matches!(code, 95 | 96 | 99) => code as f32,
        Derive::ThunderstormCode => 0.0,
    }
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}
