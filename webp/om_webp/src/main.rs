use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use clap::{Parser, ValueEnum};
use fs2::FileExt;
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageEncoder};
use om_api::official::OfficialDecoder;
use om_api::query::{read_variable_grid_series, round_variable_output_value};
use om_api::snapshot::OmDataSnapshot;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const GFS_LAYERS: &[Layer] = &[
    Layer::scalar("cloud_total_1", "cloud_cover", "%", 0.0, 100.0, 0.0, 100.0),
    Layer::scalar(
        "cloud_high_1",
        "cloud_cover_high",
        "%",
        0.0,
        100.0,
        0.0,
        100.0,
    ),
    Layer::scalar(
        "cloud_mid_1",
        "cloud_cover_mid",
        "%",
        0.0,
        100.0,
        0.0,
        100.0,
    ),
    Layer::scalar(
        "cloud_low_1",
        "cloud_cover_low",
        "%",
        0.0,
        100.0,
        0.0,
        100.0,
    ),
    Layer::scalar("t2m", "temperature_2m", "C", -100.0, 100.0, -100.0, 100.0),
    Layer::scalar("d2m", "dew_point_2m", "C", -100.0, 100.0, -100.0, 100.0),
    Layer::scalar("r2", "relative_humidity_2m", "%", 0.0, 100.0, 0.0, 100.0),
    Layer::wind("wind", "wind_u_component_10m", "wind_v_component_10m"),
    Layer::scalar("tp", "precipitation", "mm", 0.0, 600.0, 0.0, 100.0),
    Layer::scaled("snod", "snow_depth", "mm", 0.0, 2000.0, 0.0, 10.0, 1000.0),
    Layer::scalar("gust", "wind_gusts_10m", "m/s", 0.0, 200.0, 0.0, 100.0),
    Layer::scalar("vis", "visibility", "m", 0.0, 100000.0, 0.0, 0.1),
    Layer::derived(
        "precip_phase",
        "weather_code",
        "code",
        0.0,
        4.0,
        Derive::PrecipPhase,
    ),
    Layer::derived(
        "thunderstorm_code",
        "weather_code",
        "wmo code",
        0.0,
        100.0,
        Derive::ThunderstormCode,
    ),
    Layer::scalar("cape", "cape", "J/kg", 0.0, 65535.0, 0.0, 1.0),
    Layer::scaled(
        "prmsl",
        "pressure_msl",
        "Pa",
        50000.0,
        115000.0,
        50000.0,
        1.0,
        100.0,
    ),
    Layer::scaled(
        "sp",
        "surface_pressure",
        "Pa",
        50000.0,
        115000.0,
        50000.0,
        1.0,
        100.0,
    ),
    Layer::scalar("uv_index", "uv_index", "index", 0.0, 100.0, 0.0, 100.0),
];

const CAMS_LAYERS: &[Layer] = &[
    Layer::scalar("pm2_5", "pm2_5", "ug/m3", 0.0, 6000.0, 0.0, 10.0),
    Layer::scalar("pm10", "pm10", "ug/m3", 0.0, 6000.0, 0.0, 10.0),
    Layer::scalar(
        "aerosol_optical_depth",
        "aerosol_optical_depth",
        "1",
        0.0,
        65.0,
        0.0,
        1000.0,
    ),
    Layer::scalar("dust", "dust", "ug/m3", 0.0, 6000.0, 0.0, 10.0),
];

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Scope {
    Gfs,
    Cams,
}

impl Scope {
    fn group(self) -> &'static str {
        match self {
            Self::Gfs => "gfs",
            Self::Cams => "cams",
        }
    }

    fn product_dir(self) -> &'static str {
        match self {
            Self::Gfs => "gfs013_surface",
            Self::Cams => "cams_global",
        }
    }

    fn manifest_name(self) -> &'static str {
        match self {
            Self::Gfs => "gfs013_surface_data.json",
            Self::Cams => "cams_global_data.json",
        }
    }

    fn layers(self) -> &'static [Layer] {
        match self {
            Self::Gfs => GFS_LAYERS,
            Self::Cams => CAMS_LAYERS,
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Render regional lossless WebP layers directly from local OM bundles")]
struct Args {
    #[arg(long, value_enum)]
    scope: Scope,
    #[arg(long, default_value = "/data/om_raw", env = "OM_DATA_ROOT")]
    data_root: PathBuf,
    #[arg(
        long,
        default_value = "/opt/1panel/apps/weather_om_webp/data",
        env = "OM_WEBP_DATA_ROOT"
    )]
    output_root: PathBuf,
    #[arg(long, env = "OM_OMFILE_LIB")]
    decoder_lib: PathBuf,
    #[arg(long, default_value_t = 121)]
    frames: usize,
    #[arg(long, default_value_t = 70.0)]
    left_lon: f64,
    #[arg(long, default_value_t = 140.0)]
    right_lon: f64,
    #[arg(long, default_value_t = 0.0)]
    bottom_lat: f64,
    #[arg(long, default_value_t = 58.0)]
    top_lat: f64,
    #[arg(long, default_value_t = 2, env = "OM_WEBP_WORKERS")]
    workers: usize,
    #[arg(long, default_value_t = 24, env = "OM_WEBP_SERIES_BLOCK_HOURS")]
    series_block_hours: usize,
    #[arg(long)]
    layers: Option<String>,
    #[arg(long)]
    public_root: Option<PathBuf>,
    #[arg(long, default_value_t = 2)]
    keep_releases: usize,
}

#[derive(Debug, Deserialize)]
struct GroupReady {
    status: String,
    latest_complete_run: String,
    release_id: String,
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
struct Layer {
    name: &'static str,
    variable: &'static str,
    variable_v: Option<&'static str>,
    unit: &'static str,
    min: f32,
    max: f32,
    vmin: f32,
    scale: f32,
    multiplier: f32,
    encoding: Encoding,
    derive: Derive,
}

impl Layer {
    const fn scalar(
        name: &'static str,
        variable: &'static str,
        unit: &'static str,
        min: f32,
        max: f32,
        vmin: f32,
        scale: f32,
    ) -> Self {
        Self::scaled(name, variable, unit, min, max, vmin, scale, 1.0)
    }

    const fn scaled(
        name: &'static str,
        variable: &'static str,
        unit: &'static str,
        min: f32,
        max: f32,
        vmin: f32,
        scale: f32,
        multiplier: f32,
    ) -> Self {
        Self {
            name,
            variable,
            variable_v: None,
            unit,
            min,
            max,
            vmin,
            scale,
            multiplier,
            encoding: Encoding::Scalar,
            derive: Derive::None,
        }
    }

    const fn wind(name: &'static str, variable: &'static str, variable_v: &'static str) -> Self {
        Self {
            name,
            variable,
            variable_v: Some(variable_v),
            unit: "m/s",
            min: -100.0,
            max: 100.0,
            vmin: -100.0,
            scale: 10.0,
            multiplier: 1.0,
            encoding: Encoding::Wind,
            derive: Derive::None,
        }
    }

    const fn derived(
        name: &'static str,
        variable: &'static str,
        unit: &'static str,
        min: f32,
        max: f32,
        derive: Derive,
    ) -> Self {
        Self {
            name,
            variable,
            variable_v: None,
            unit,
            min,
            max,
            vmin: 0.0,
            scale: 1.0,
            multiplier: 1.0,
            encoding: Encoding::Scalar,
            derive,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct GridManifest {
    width: usize,
    height: usize,
    row_order: &'static str,
    dx: f64,
    dy: f64,
    sample_bounds: Bounds,
    display_bounds: Bounds,
}

#[derive(Debug, Clone, Serialize)]
struct Bounds {
    lon_min: f64,
    lat_min: f64,
    lon_max: f64,
    lat_max: f64,
}

#[derive(Debug, Clone)]
struct RegionGrid {
    manifest: GridManifest,
    latitudes: Vec<f64>,
    longitudes: Vec<f64>,
}

impl RegionGrid {
    fn len(&self) -> usize {
        self.manifest.width * self.manifest.height
    }
}

#[derive(Debug, Serialize)]
struct LayerManifest {
    subdir: String,
    unit: String,
    encoding: String,
    scale: f32,
    vmin: f32,
    range: [f32; 2],
}

#[derive(Debug, Serialize)]
struct ProductManifest {
    generated_at: i64,
    source: String,
    source_release_id: String,
    source_run: String,
    batch: i64,
    frame_count: usize,
    frame_step_seconds: i64,
    file_pattern: &'static str,
    files: Vec<i64>,
    grid: GridManifest,
    layers: BTreeMap<String, LayerManifest>,
}

#[derive(Debug)]
struct RenderedLayer {
    layer_name: &'static str,
    bytes: Vec<u8>,
    invalid_points: usize,
}

struct StagingGuard {
    path: PathBuf,
    committed: bool,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let workers = if args.workers == 0 {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
    } else {
        args.workers
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()?;
    let lock_root = args.output_root.join("locks");
    fs::create_dir_all(&lock_root)?;
    let lock = File::create(lock_root.join(format!("{}.lock", args.scope.group())))?;
    if lock.try_lock_exclusive().is_err() {
        println!(
            "{{\"status\":\"skipped\",\"reason\":\"renderer already running\",\"scope\":\"{}\"}}",
            args.scope.group()
        );
        return Ok(());
    }

    let ready = load_group_ready(&args.data_root, args.scope)?;
    let current_marker = args
        .output_root
        .join("current")
        .join(format!("{}.json", args.scope.group()));
    if marker_matches(&current_marker, &ready.release_id)? {
        println!("{{\"status\":\"skipped\",\"reason\":\"release already rendered\",\"scope\":\"{}\",\"release_id\":\"{}\"}}", args.scope.group(), ready.release_id);
        return Ok(());
    }

    let selected = select_layers(args.scope.layers(), args.layers.as_deref())?;
    let grid = compute_grid(args.left_lon, args.right_lon, args.bottom_lat, args.top_lat)?;
    let start = parse_run(&ready.latest_complete_run)?;
    let times = render_times(start, args.frames)?;
    let snapshot = Arc::new(OmDataSnapshot::load(&args.data_root)?);
    let decoder = Arc::new(OfficialDecoder::load(&args.decoder_lib)?);
    let release_root = args.output_root.join("releases").join(format!(
        "{}-{}",
        ready.release_id,
        Utc::now().timestamp()
    ));
    let staging = args.output_root.join("staging").join(format!(
        "{}.{}.{}",
        ready.release_id,
        std::process::id(),
        Utc::now().timestamp()
    ));
    let product_staging = staging.join(args.scope.product_dir());
    fs::create_dir_all(&product_staging)?;
    let mut staging_guard = StagingGuard {
        path: staging.clone(),
        committed: false,
    };

    let started = Instant::now();
    let mut last_progress_at = Instant::now();
    let mut last_progress_bytes = 0_u64;
    let mut written_bytes = 0_u64;
    let batch = start.timestamp();
    println!(
        "开始｜阶段：生成 WebP｜类型：{}｜批次：{}｜总帧：{}",
        args.scope.group().to_uppercase(),
        ready.latest_complete_run,
        times.len()
    );
    for layer in &selected {
        fs::create_dir_all(product_staging.join(layer.name))?;
    }
    if args.series_block_hours == 0 {
        bail!("--series-block-hours must be positive");
    }
    let total_invalid = std::sync::atomic::AtomicUsize::new(0);
    for (block_index, block_times) in times.chunks(args.series_block_hours).enumerate() {
        let rendered = pool
            .install(|| render_series_block(&snapshot, &decoder, &grid, &selected, block_times))?;
        for (offset, layers) in rendered.into_iter().enumerate() {
            let frame_index = block_index * args.series_block_hours + offset;
            let time = block_times[offset];
            let stem = format!("{}_{}", time.timestamp(), batch);
            for layer in layers {
                written_bytes += layer.bytes.len() as u64;
                fs::write(
                    product_staging
                        .join(layer.layer_name)
                        .join(format!("{stem}.webp")),
                    layer.bytes,
                )?;
                total_invalid.fetch_add(layer.invalid_points, std::sync::atomic::Ordering::Relaxed);
            }
            let progress_elapsed = last_progress_at.elapsed();
            if progress_elapsed.as_secs() >= 60 {
                let growth_bytes = written_bytes.saturating_sub(last_progress_bytes);
                let speed_mib_s = growth_bytes as f64
                    / progress_elapsed.as_secs_f64().max(0.001)
                    / 1024.0
                    / 1024.0;
                println!(
                    "进度｜阶段：生成 WebP｜类型：{}｜批次：{}｜帧：{}/{}｜近一分钟增长：{:.1} MiB｜速度：{:.2} MiB/s",
                    args.scope.group().to_uppercase(),
                    ready.latest_complete_run,
                    frame_index + 1,
                    times.len(),
                    growth_bytes as f64 / 1024.0 / 1024.0,
                    speed_mib_s
                );
                last_progress_at = Instant::now();
                last_progress_bytes = written_bytes;
            }
        }
    }

    let manifest = build_manifest(args.scope, &ready, &grid, &selected, &times);
    fs::write(
        product_staging.join(args.scope.manifest_name()),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(
        staging.join("complete.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let latest_ready = load_group_ready(&args.data_root, args.scope)?;
    if latest_ready.release_id != ready.release_id {
        bail!(
            "source release changed during rendering: started {}, now {}",
            ready.release_id,
            latest_ready.release_id
        );
    }
    println!(
        "阶段：发布 WebP｜类型：{}｜批次：{}",
        args.scope.group().to_uppercase(),
        ready.latest_complete_run
    );
    fs::create_dir_all(
        release_root
            .parent()
            .context("release root has no parent")?,
    )?;
    fs::rename(&staging, &release_root)?;
    staging_guard.committed = true;
    publish_current(
        &args.output_root,
        args.scope,
        &ready,
        &release_root,
        args.public_root.as_deref(),
    )?;
    prune_releases(&args.output_root, args.scope, args.keep_releases.max(1))?;

    println!("{{\"status\":\"success\",\"scope\":\"{}\",\"release_id\":\"{}\",\"run\":\"{}\",\"layers\":{},\"frames\":{},\"grid\":\"{}x{}\",\"invalid_samples\":{},\"elapsed_seconds\":{:.3}}}",
        args.scope.group(), ready.release_id, ready.latest_complete_run, selected.len(), times.len(), grid.manifest.width, grid.manifest.height, total_invalid.load(std::sync::atomic::Ordering::Relaxed), started.elapsed().as_secs_f64());
    Ok(())
}

fn load_group_ready(data_root: &Path, scope: Scope) -> Result<GroupReady> {
    let path = data_root
        .join("groups")
        .join(scope.group())
        .join("current/ready_for_processing.json");
    let ready: GroupReady = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )?;
    if ready.status != "complete"
        || ready.release_id.is_empty()
        || ready.latest_complete_run.is_empty()
    {
        bail!("group {} is not ready", scope.group());
    }
    Ok(ready)
}

fn parse_run(run: &str) -> Result<DateTime<Utc>> {
    let parsed = NaiveDateTime::parse_from_str(&format!("{run}00"), "%Y%m%d%H%M")?;
    Ok(Utc.from_utc_datetime(&parsed))
}

fn render_times(start: DateTime<Utc>, frames: usize) -> Result<Vec<DateTime<Utc>>> {
    if frames == 0 {
        bail!("--frames must be positive");
    }
    Ok((0..frames)
        .map(|offset| start + Duration::hours(offset as i64))
        .collect())
}

fn compute_grid(left: f64, right: f64, bottom: f64, top: f64) -> Result<RegionGrid> {
    let full_nx = 3072usize;
    let full_ny = 1536usize;
    let dx = 360.0 / full_nx as f64;
    let dy = 0.11714935f64;
    let lon_origin = -180.0;
    let lat_origin = -dy * (full_ny as f64 - 1.0) / 2.0;
    let x0 = (((left - lon_origin) / dx) - 1e-9).ceil().max(0.0) as usize;
    let x1 = (((right - lon_origin) / dx) + 1e-9)
        .floor()
        .min((full_nx - 1) as f64) as usize;
    let y0 = (((bottom - lat_origin) / dy) - 1e-9).ceil().max(0.0) as usize;
    let y1 = (((top - lat_origin) / dy) + 1e-9)
        .floor()
        .min((full_ny - 1) as f64) as usize;
    if x0 > x1 || y0 > y1 {
        bail!("region does not overlap GFS013 grid");
    }
    let width = x1 - x0 + 1;
    let height = y1 - y0 + 1;
    let longitudes = (x0..=x1)
        .map(|x| round6(lon_origin + x as f64 * dx))
        .collect::<Vec<_>>();
    let latitudes = (y0..=y1)
        .rev()
        .map(|y| round6(lat_origin + y as f64 * dy))
        .collect::<Vec<_>>();
    let sample_bounds = Bounds {
        lon_min: *longitudes.first().unwrap(),
        lat_min: *latitudes.last().unwrap(),
        lon_max: *longitudes.last().unwrap(),
        lat_max: *latitudes.first().unwrap(),
    };
    let display_bounds = Bounds {
        lon_min: round6(sample_bounds.lon_min - dx / 2.0),
        lat_min: round6(sample_bounds.lat_min - dy / 2.0),
        lon_max: round6(sample_bounds.lon_max + dx / 2.0),
        lat_max: round6(sample_bounds.lat_max + dy / 2.0),
    };
    Ok(RegionGrid {
        manifest: GridManifest {
            width,
            height,
            row_order: "north_to_south",
            dx: round6(dx),
            dy: round6(dy),
            sample_bounds,
            display_bounds,
        },
        latitudes,
        longitudes,
    })
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn select_layers(all: &'static [Layer], names: Option<&str>) -> Result<Vec<Layer>> {
    let Some(names) = names else {
        return Ok(all.to_vec());
    };
    let requested = names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    let mut selected = Vec::new();
    for name in requested {
        let layer = all
            .iter()
            .find(|layer| layer.name == name)
            .with_context(|| format!("unknown layer {name}"))?;
        selected.push(*layer);
    }
    if selected.is_empty() {
        bail!("no layers selected");
    }
    Ok(selected)
}

fn encode_layer_values(
    grid: &RegionGrid,
    layer: &Layer,
    values: &[f32],
    values_v: Option<&[f32]>,
) -> Result<RenderedLayer> {
    let mut rgba = vec![0u8; grid.len() * 4];
    let invalid = std::sync::atomic::AtomicUsize::new(0);
    rgba.par_chunks_mut(4)
        .enumerate()
        .for_each(|(index, pixel)| match layer.encoding {
            Encoding::Scalar => {
                let mut value = values[index];
                value = derive_value(value, layer.derive) * layer.multiplier;
                encode_scalar(pixel, value, layer.vmin, layer.scale, &invalid);
            }
            Encoding::Wind => {
                let u = values[index];
                let v = values_v.expect("wind v")[index];
                encode_wind(pixel, u, v, &invalid);
            }
        });
    let mut bytes = Vec::new();
    WebPEncoder::new_lossless(&mut bytes).write_image(
        &rgba,
        grid.manifest.width as u32,
        grid.manifest.height as u32,
        ExtendedColorType::Rgba8,
    )?;
    Ok(RenderedLayer {
        layer_name: layer.name,
        bytes,
        invalid_points: invalid.load(std::sync::atomic::Ordering::Relaxed),
    })
}

fn render_series_block(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    grid: &RegionGrid,
    layers: &[Layer],
    times: &[DateTime<Utc>],
) -> Result<Vec<Vec<RenderedLayer>>> {
    let (weather_layers, regular_layers): (Vec<&Layer>, Vec<&Layer>) = layers
        .iter()
        .partition(|layer| !matches!(layer.derive, Derive::None));
    let mut rendered = (0..times.len()).map(|_| Vec::new()).collect::<Vec<_>>();
    for layer in regular_layers {
        let values = read_layer_grid_series(snapshot, decoder, layer.variable, times, grid)?;
        let values_v = match layer.variable_v {
            Some(variable) => Some(read_layer_grid_series(
                snapshot, decoder, variable, times, grid,
            )?),
            None => None,
        };
        let encoded = values
            .par_iter()
            .enumerate()
            .map(|(index, values)| {
                encode_layer_values(
                    grid,
                    layer,
                    values,
                    values_v.as_ref().map(|series| series[index].as_slice()),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        for (frame, layer) in rendered.iter_mut().zip(encoded) {
            frame.push(layer);
        }
    }
    if !weather_layers.is_empty() {
        let weather_codes =
            read_layer_grid_series(snapshot, decoder, weather_layers[0].variable, times, grid)?;
        for layer in weather_layers {
            let encoded = weather_codes
                .par_iter()
                .map(|values| encode_cached_scalar_layer(grid, layer, values))
                .collect::<Result<Vec<_>>>()?;
            for (frame, layer) in rendered.iter_mut().zip(encoded) {
                frame.push(layer);
            }
        }
    }
    Ok(rendered)
}

fn encode_cached_scalar_layer(
    grid: &RegionGrid,
    layer: &Layer,
    values: &[f32],
) -> Result<RenderedLayer> {
    let mut rgba = vec![0u8; grid.len() * 4];
    let invalid = std::sync::atomic::AtomicUsize::new(0);
    rgba.par_chunks_mut(4)
        .zip(values.par_iter())
        .for_each(|(pixel, value)| {
            encode_scalar(
                pixel,
                derive_value(*value, layer.derive) * layer.multiplier,
                layer.vmin,
                layer.scale,
                &invalid,
            );
        });
    let mut bytes = Vec::new();
    WebPEncoder::new_lossless(&mut bytes).write_image(
        &rgba,
        grid.manifest.width as u32,
        grid.manifest.height as u32,
        ExtendedColorType::Rgba8,
    )?;
    Ok(RenderedLayer {
        layer_name: layer.name,
        bytes,
        invalid_points: invalid.load(std::sync::atomic::Ordering::Relaxed),
    })
}

fn read_layer_grid_series(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    grid: &RegionGrid,
) -> Result<Vec<Vec<f32>>> {
    match read_variable_grid_series(
        snapshot,
        decoder,
        variable,
        times,
        &grid.latitudes,
        &grid.longitudes,
    ) {
        Ok(mut series) => {
            for values in &mut series {
                values
                    .iter_mut()
                    .for_each(|value| *value = round_variable_output_value(variable, *value));
            }
            Ok(series)
        }
        Err(error) if error.to_string().contains("variable/time is not available") => {
            Ok(vec![vec![f32::NAN; grid.len()]; times.len()])
        }
        Err(error) => Err(error),
    }
}

fn derive_value(value: f32, derive: Derive) -> f32 {
    if !value.is_finite() {
        return value;
    }
    let code = value.round() as i32;
    match derive {
        Derive::None => value,
        Derive::PrecipPhase => match code {
            51 | 53 | 55 | 61 | 63 | 65 | 80 | 81 | 82 => 1.0,
            71 | 73 | 75 | 77 | 85 | 86 => 2.0,
            56 | 57 | 66 | 67 => 4.0,
            _ => 0.0,
        },
        Derive::ThunderstormCode => {
            if matches!(code, 95 | 96 | 99) {
                code as f32
            } else {
                0.0
            }
        }
    }
}

fn encode_scalar(
    pixel: &mut [u8],
    value: f32,
    vmin: f32,
    scale: f32,
    invalid: &std::sync::atomic::AtomicUsize,
) {
    if !value.is_finite() {
        pixel.copy_from_slice(&[0, 0, 0, 0]);
        invalid.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    let encoded = ((value - vmin) * scale).round().clamp(0.0, 65535.0) as u16;
    pixel.copy_from_slice(&[(encoded >> 8) as u8, encoded as u8, 0, 255]);
}

fn encode_wind(pixel: &mut [u8], u: f32, v: f32, invalid: &std::sync::atomic::AtomicUsize) {
    let speed = (u * u + v * v).sqrt();
    if !u.is_finite()
        || !v.is_finite()
        || speed > 150.0
        || !(-100.0..=100.0).contains(&u)
        || !(-100.0..=100.0).contains(&v)
    {
        pixel.copy_from_slice(&[0, 0, 0, 0]);
        invalid.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    let eu = (u / 0.1).round().clamp(-1000.0, 3095.0) as i32 + 1000;
    let ev = (v / 0.1).round().clamp(-1000.0, 3095.0) as i32 + 1000;
    let u12 = eu as u16;
    let v12 = ev as u16;
    pixel.copy_from_slice(&[
        (u12 >> 4) as u8,
        (((u12 & 0x0f) << 4) | (v12 >> 8)) as u8,
        v12 as u8,
        255,
    ]);
}

fn build_manifest(
    scope: Scope,
    ready: &GroupReady,
    grid: &RegionGrid,
    layers: &[Layer],
    times: &[DateTime<Utc>],
) -> ProductManifest {
    let layer_map = layers
        .iter()
        .map(|layer| {
            (
                layer.name.to_string(),
                LayerManifest {
                    subdir: layer.name.to_string(),
                    unit: layer.unit.to_string(),
                    encoding: match layer.encoding {
                        Encoding::Scalar => {
                            if matches!(layer.derive, Derive::None) {
                                "scalar"
                            } else {
                                "categorical"
                            }
                        }
                        Encoding::Wind => "uv",
                    }
                    .to_string(),
                    scale: layer.scale,
                    vmin: layer.vmin,
                    range: [layer.min, layer.max],
                },
            )
        })
        .collect();
    ProductManifest {
        generated_at: Utc::now().timestamp(),
        source: scope.group().to_string(),
        source_release_id: ready.release_id.clone(),
        source_run: ready.latest_complete_run.clone(),
        batch: times[0].timestamp(),
        frame_count: times.len(),
        frame_step_seconds: 3600,
        file_pattern: "{timestamp}_{batch}.webp",
        files: times.iter().map(DateTime::timestamp).collect(),
        grid: grid.manifest.clone(),
        layers: layer_map,
    }
}

fn publish_current(
    output_root: &Path,
    scope: Scope,
    ready: &GroupReady,
    release_root: &Path,
    public_root: Option<&Path>,
) -> Result<()> {
    let current_root = output_root.join("current");
    fs::create_dir_all(&current_root)?;
    let marker = current_root.join(format!("{}.json", scope.group()));
    let marker_tmp = current_root.join(format!(".{}.{}.tmp", scope.group(), std::process::id()));
    if let Some(public_root) = public_root {
        fs::create_dir_all(public_root)?;
        let catalog_path = public_root.join("weather_layer_catalog.json");
        let catalog_tmp =
            public_root.join(format!(".weather_layer_catalog.{}.tmp", std::process::id()));
        fs::write(&catalog_tmp, serde_json::to_vec_pretty(&catalog_payload())?)?;
        fs::rename(catalog_tmp, catalog_path)?;
        let link = public_root.join(scope.product_dir());
        if link.exists() && !link.is_symlink() {
            bail!("refusing to replace non-symlink {}", link.display());
        }
        let tmp = public_root.join(format!(
            ".{}.{}.tmp",
            scope.product_dir(),
            std::process::id()
        ));
        if tmp.exists() {
            fs::remove_file(&tmp)?;
        }
        symlink(release_root.join(scope.product_dir()), &tmp)?;
        fs::rename(tmp, link)?;
    }
    fs::write(
        &marker_tmp,
        serde_json::to_vec_pretty(
            &serde_json::json!({"status":"complete","scope":scope.group(),"release_id":ready.release_id,"run":ready.latest_complete_run,"path":release_root}),
        )?,
    )?;
    fs::rename(marker_tmp, marker)?;
    Ok(())
}

fn catalog_payload() -> serde_json::Value {
    fn layers(scope: Scope) -> serde_json::Value {
        serde_json::Value::Object(
            scope
                .layers()
                .iter()
                .map(|layer| {
                    (
                        layer.name.to_string(),
                        serde_json::json!({
                            "subdir": layer.name,
                            "unit": layer.unit,
                            "encoding": match layer.encoding {
                                Encoding::Wind => "uv",
                                Encoding::Scalar if !matches!(layer.derive, Derive::None) => "categorical",
                                Encoding::Scalar => "scalar",
                            },
                            "scale": layer.scale,
                            "vmin": layer.vmin,
                            "range": [layer.min, layer.max],
                            "source_resolution": source_resolution(scope, layer.name),
                        }),
                    )
                })
                .collect(),
        )
    }
    serde_json::json!({
        "version": 1,
        "products": {
            "gfs": {
                "source": "gfs",
                "manifest": Scope::Gfs.manifest_name(),
                "file_pattern": "{timestamp}_{batch}.webp",
                "layers": layers(Scope::Gfs),
            },
            "cams": {
                "source": "cams",
                "manifest": Scope::Cams.manifest_name(),
                "file_pattern": "{timestamp}_{batch}.webp",
                "layers": layers(Scope::Cams),
            }
        }
    })
}

fn source_resolution(scope: Scope, name: &str) -> &'static str {
    match scope {
        Scope::Cams => "44km",
        Scope::Gfs => match name {
            "gust" | "vis" | "cape" | "prmsl" => "28km",
            "precip_phase" | "thunderstorm_code" | "sp" => "28km(13+28)",
            _ => "13km",
        },
    }
}

fn marker_matches(path: &Path, release_id: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(value.get("release_id").and_then(|value| value.as_str()) == Some(release_id))
}

fn prune_releases(output_root: &Path, scope: Scope, keep: usize) -> Result<()> {
    let releases = output_root.join("releases");
    if !releases.exists() {
        return Ok(());
    }
    let mut candidates = fs::read_dir(&releases)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(scope.product_dir()).exists())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|entry| {
        std::cmp::Reverse(entry.metadata().and_then(|meta| meta.modified()).ok())
    });
    for entry in candidates.into_iter().skip(keep) {
        fs::remove_dir_all(entry.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singapore_grid_matches_production_manifest() {
        let grid = compute_grid(70.0, 140.0, 0.0, 58.0).unwrap();
        assert_eq!((grid.manifest.width, grid.manifest.height), (597, 495));
        assert_eq!(grid.manifest.sample_bounds.lon_min, 70.078125);
        assert_eq!(grid.manifest.sample_bounds.lat_max, 57.930354);
    }

    #[test]
    fn layer_inventory_matches_singapore() {
        assert_eq!(GFS_LAYERS.len(), 18);
        assert_eq!(CAMS_LAYERS.len(), 4);
        let surface_pressure = GFS_LAYERS.iter().find(|layer| layer.name == "sp").unwrap();
        assert_eq!(
            (surface_pressure.vmin, surface_pressure.scale),
            (50000.0, 1.0)
        );
        let dust = CAMS_LAYERS
            .iter()
            .find(|layer| layer.name == "dust")
            .unwrap();
        assert_eq!((dust.max, dust.scale), (6000.0, 10.0));
    }

    #[test]
    fn categorical_transforms_match_contract() {
        assert_eq!(derive_value(95.0, Derive::ThunderstormCode), 95.0);
        assert_eq!(derive_value(80.0, Derive::PrecipPhase), 1.0);
        assert_eq!(derive_value(71.0, Derive::PrecipPhase), 2.0);
        assert_eq!(derive_value(66.0, Derive::PrecipPhase), 4.0);
    }

    #[test]
    fn gfs_and_cams_each_render_121_hourly_webp_frames() {
        let start = parse_run("2026071306").unwrap();
        let gfs = render_times(start, 121).unwrap();
        let cams = render_times(start, 121).unwrap();

        assert_eq!(gfs.len(), 121);
        assert_eq!(cams.len(), 121);
        assert_eq!(gfs, cams);
        assert_eq!(gfs[0], start);
        assert_eq!(*gfs.last().unwrap(), start + Duration::hours(120));
    }

    #[test]
    fn client_node_defaults_webp_to_two_workers() {
        let args = Args::try_parse_from([
            "om-webp",
            "--scope",
            "gfs",
            "--decoder-lib",
            "/tmp/libomfileformat.so",
        ])
        .unwrap();

        assert_eq!(args.workers, 2);
    }
}
