use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use clap::{Parser, ValueEnum};
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageEncoder};
use om_api::official::OfficialDecoder;
use om_api::query::{
    read_variable_grid_series, round_variable_output_value, with_ecmwf_request_cache,
    with_weather_model, WeatherModel,
};
use om_api::snapshot::OmDataSnapshot;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt};
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

// ECMWF IFS 0.25 degree uses the same client-side encoding contract as GFS.
// The free deterministic feed publishes gust but not visibility or UV index,
// so the two unavailable GFS-only layers are absent rather than synthesized.
const ECMWF_IFS025_LAYERS: &[Layer] = &[
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
];

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Scope {
    Gfs,
    Cams,
    #[value(name = "ecmwf_ifs025", alias = "ecmwf", alias = "ec")]
    EcmwfIfs025,
}

impl Scope {
    fn group(self) -> &'static str {
        match self {
            Self::Gfs => "gfs",
            Self::Cams => "cams",
            Self::EcmwfIfs025 => "ecmwf",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Gfs => "gfs",
            Self::Cams => "cams",
            Self::EcmwfIfs025 => "ecmwf_ifs025",
        }
    }

    fn product_dir(self) -> &'static str {
        match self {
            Self::Gfs => "gfs013_surface",
            Self::Cams => "cams_global",
            Self::EcmwfIfs025 => "ecmwf_ifs025",
        }
    }

    fn manifest_name(self) -> &'static str {
        match self {
            Self::Gfs => "gfs013_surface_data.json",
            Self::Cams => "cams_global_data.json",
            Self::EcmwfIfs025 => "ecmwf_ifs025_data.json",
        }
    }

    fn layers(self) -> &'static [Layer] {
        match self {
            Self::Gfs => GFS_LAYERS,
            Self::Cams => CAMS_LAYERS,
            Self::EcmwfIfs025 => ECMWF_IFS025_LAYERS,
        }
    }

    fn weather_model(self) -> WeatherModel {
        match self {
            Self::Gfs | Self::Cams => WeatherModel::Gfs,
            Self::EcmwfIfs025 => WeatherModel::EcmwfIfs025,
        }
    }

    fn tolerate_unavailable_layers(self) -> bool {
        !matches!(self, Self::EcmwfIfs025)
    }

    fn data_attribution(self) -> Option<DataAttribution> {
        match self {
            Self::EcmwfIfs025 => Some(DataAttribution {
                attribution: "Weather data by Open-Meteo.com. This service is based on data and products of the European Centre for Medium-Range Weather Forecasts (ECMWF).",
                provider: "European Centre for Medium-Range Weather Forecasts (ECMWF)",
                provider_url: "https://www.ecmwf.int/",
                distributor: "Open-Meteo",
                distributor_url: "https://open-meteo.com/",
                license: "CC-BY-4.0",
                license_url: "https://creativecommons.org/licenses/by/4.0/",
                terms_url: "https://apps.ecmwf.int/datasets/licences/general/",
                modified: true,
                transformations: &[
                    "spatial subsetting",
                    "range extraction",
                    "temporal and spatial interpolation",
                    "unit conversion and derived-variable calculation",
                    "lossless WebP encoding",
                ],
            }),
            Self::Gfs | Self::Cams => None,
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
    #[arg(long, default_value = "/data/om_webp", env = "OM_WEBP_DATA_ROOT")]
    output_root: PathBuf,
    #[arg(long, default_value = "/data", env = "OM_STRICT_DATA_ROOT")]
    strict_data_root: PathBuf,
    #[arg(
        long,
        default_value_t = 10_737_418_240_u64,
        env = "OM_DATA_MIN_FREE_BYTES"
    )]
    minimum_free_bytes: u64,
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

    #[allow(clippy::too_many_arguments)]
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

#[derive(Debug, Clone, Copy, Serialize)]
struct DataAttribution {
    attribution: &'static str,
    provider: &'static str,
    provider_url: &'static str,
    distributor: &'static str,
    distributor_url: &'static str,
    license: &'static str,
    license_url: &'static str,
    terms_url: &'static str,
    modified: bool,
    transformations: &'static [&'static str],
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
    #[serde(skip_serializing_if = "Option::is_none")]
    data_attribution: Option<DataAttribution>,
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

fn existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut candidate = path.to_path_buf();
    loop {
        if candidate.exists() {
            return candidate
                .canonicalize()
                .with_context(|| format!("resolve storage path {}", candidate.display()));
        }
        if !candidate.pop() {
            bail!("storage path has no existing ancestor: {}", path.display());
        }
    }
}

fn validate_strict_data_layout(strict_root: &Path, paths: &[&Path]) -> Result<()> {
    if !strict_root.is_absolute() || !strict_root.is_dir() {
        bail!(
            "strict data root must be an absolute mounted directory: {}",
            strict_root.display()
        );
    }
    let resolved_root = strict_root
        .canonicalize()
        .with_context(|| format!("resolve strict data root {}", strict_root.display()))?;
    let root_device = fs::metadata(&resolved_root)?.dev();
    if fs::metadata("/")?.dev() == root_device {
        bail!(
            "strict data root shares the system filesystem device: {}",
            resolved_root.display()
        );
    }
    for path in paths {
        if !path.is_absolute() {
            bail!("strict data path must be absolute: {}", path.display());
        }
        let resolved = if path.exists() {
            path.canonicalize()?
        } else {
            let ancestor = existing_ancestor(path)?;
            let suffix = path.strip_prefix(&ancestor).unwrap_or(Path::new(""));
            ancestor.join(suffix)
        };
        if !resolved.starts_with(&resolved_root) {
            bail!(
                "strict data path escapes {}: {}",
                resolved_root.display(),
                resolved.display()
            );
        }
        let ancestor = existing_ancestor(path)?;
        if fs::metadata(&ancestor)?.dev() != root_device {
            bail!("strict data path is on another device: {}", path.display());
        }
    }
    Ok(())
}

fn available_space(path: &Path) -> Result<u64> {
    let ancestor = existing_ancestor(path)?;
    let c_path = CString::new(ancestor.as_os_str().as_bytes())
        .context("storage path contains an interior NUL")?;
    let mut output = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: statvfs writes the output structure on success.
    if unsafe { libc::statvfs(c_path.as_ptr(), output.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("read free space for {}", ancestor.display()));
    }
    // SAFETY: the successful statvfs call initialized output.
    let output = unsafe { output.assume_init() };
    Ok(output.f_bavail.saturating_mul(output.f_frsize))
}

fn ensure_free_space(path: &Path, reserve_bytes: u64, additional_bytes: u64) -> Result<()> {
    if reserve_bytes < 512 * 1024 * 1024 {
        bail!("WebP minimum free-space reserve must be at least 512 MiB");
    }
    let required = reserve_bytes
        .checked_add(additional_bytes)
        .context("WebP free-space requirement overflow")?;
    let available = available_space(path)?;
    if available < required {
        bail!(
            "insufficient data-disk space for WebP staging: available={} additional={} reserve={}",
            available,
            additional_bytes,
            reserve_bytes
        );
    }
    Ok(())
}

fn estimate_staging_bytes(grid_points: usize, layers: usize, frames: usize) -> Result<u64> {
    let rgba_bytes = u64::try_from(grid_points)?
        .checked_mul(4)
        .and_then(|value| value.checked_mul(u64::try_from(layers).ok()?))
        .and_then(|value| value.checked_mul(u64::try_from(frames).ok()?))
        .context("WebP staging estimate overflow")?;
    rgba_bytes
        .checked_add(16 * 1024 * 1024)
        .context("WebP staging estimate overflow")
}

fn main() -> Result<()> {
    let args = Args::parse();
    validate_strict_data_layout(
        &args.strict_data_root,
        &[&args.data_root, &args.output_root],
    )?;
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
    let estimated_staging_bytes = estimate_staging_bytes(grid.len(), selected.len(), times.len())?;
    ensure_free_space(
        &args.output_root,
        args.minimum_free_bytes,
        estimated_staging_bytes,
    )?;
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
        let rendered = pool.install(|| {
            with_weather_model(args.scope.weather_model(), || {
                render_series_block(
                    &snapshot,
                    &decoder,
                    &grid,
                    &selected,
                    block_times,
                    args.scope.tolerate_unavailable_layers(),
                )
            })
        })?;
        for (offset, layers) in rendered.into_iter().enumerate() {
            let frame_index = block_index * args.series_block_hours + offset;
            let time = block_times[offset];
            let stem = format!("{}_{}", time.timestamp(), batch);
            for layer in layers {
                ensure_free_space(
                    &args.output_root,
                    args.minimum_free_bytes,
                    layer.bytes.len() as u64,
                )?;
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
        args.scope.name(), ready.release_id, ready.latest_complete_run, selected.len(), times.len(), grid.manifest.width, grid.manifest.height, total_invalid.load(std::sync::atomic::Ordering::Relaxed), started.elapsed().as_secs_f64());
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
    tolerate_unavailable: bool,
) -> Result<Vec<Vec<RenderedLayer>>> {
    let (weather_layers, regular_layers): (Vec<&Layer>, Vec<&Layer>) = layers
        .iter()
        .partition(|layer| !matches!(layer.derive, Derive::None));
    let mut rendered = (0..times.len()).map(|_| Vec::new()).collect::<Vec<_>>();
    for layer in regular_layers {
        let values = read_layer_grid_series(
            snapshot,
            decoder,
            layer.variable,
            times,
            grid,
            tolerate_unavailable,
        )?;
        let values_v = match layer.variable_v {
            Some(variable) => Some(read_layer_grid_series(
                snapshot,
                decoder,
                variable,
                times,
                grid,
                tolerate_unavailable,
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
        let weather_codes = read_layer_grid_series(
            snapshot,
            decoder,
            weather_layers[0].variable,
            times,
            grid,
            tolerate_unavailable,
        )?;
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
    tolerate_unavailable: bool,
) -> Result<Vec<Vec<f32>>> {
    // The ECMWF regular-series cache can retain roughly one full regional
    // forecast per raw dependency. Keep it scoped to one output layer so a
    // 16-layer WebP block does not pin every source variable at once.
    let values = with_ecmwf_request_cache(|| {
        read_variable_grid_series(
            snapshot,
            decoder,
            variable,
            times,
            &grid.latitudes,
            &grid.longitudes,
        )
    });
    match values {
        Ok(mut series) => {
            for values in &mut series {
                values
                    .iter_mut()
                    .for_each(|value| *value = round_variable_output_value(variable, *value));
            }
            Ok(series)
        }
        Err(error)
            if tolerate_unavailable
                && error.to_string().contains("variable/time is not available") =>
        {
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
        source: scope.name().to_string(),
        source_release_id: ready.release_id.clone(),
        source_run: ready.latest_complete_run.clone(),
        batch: times[0].timestamp(),
        frame_count: times.len(),
        frame_step_seconds: 3600,
        file_pattern: "{timestamp}_{batch}.webp",
        files: times.iter().map(DateTime::timestamp).collect(),
        grid: grid.manifest.clone(),
        layers: layer_map,
        data_attribution: scope.data_attribution(),
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
    let marker = current_root.join(format!("{}.json", scope.name()));
    let marker_tmp = current_root.join(format!(".{}.{}.tmp", scope.name(), std::process::id()));
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
            &serde_json::json!({"status":"complete","scope":scope.name(),"release_id":ready.release_id,"run":ready.latest_complete_run,"path":release_root}),
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
            },
            "ecmwf_ifs025": {
                "source": "ecmwf_ifs025",
                "manifest": Scope::EcmwfIfs025.manifest_name(),
                "file_pattern": "{timestamp}_{batch}.webp",
                "layers": layers(Scope::EcmwfIfs025),
                "data_attribution": Scope::EcmwfIfs025.data_attribution(),
                "unavailable_layers": {
                    "vis": "not_published_by_free_ecmwf_ifs025",
                    "uv_index": "not_published_by_free_ecmwf_ifs025",
                    "showers": "not_published_by_free_ecmwf_ifs025",
                },
            }
        }
    })
}

fn source_resolution(scope: Scope, name: &str) -> &'static str {
    match scope {
        Scope::Cams => "44km",
        Scope::EcmwfIfs025 => "25km",
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
    fn layer_inventory_matches_client_contract() {
        assert_eq!(GFS_LAYERS.len(), 18);
        assert_eq!(CAMS_LAYERS.len(), 4);
        assert_eq!(ECMWF_IFS025_LAYERS.len(), 16);
        for unavailable in ["vis", "uv_index"] {
            assert!(
                ECMWF_IFS025_LAYERS
                    .iter()
                    .all(|layer| layer.name != unavailable),
                "free ECMWF IFS025 must not publish {unavailable}"
            );
        }
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
    fn every_scope_renders_121_hourly_webp_frames() {
        let start = parse_run("2026071306").unwrap();
        let gfs = render_times(start, 121).unwrap();
        let cams = render_times(start, 121).unwrap();
        let ecmwf = render_times(start, 121).unwrap();

        assert_eq!(gfs.len(), 121);
        assert_eq!(cams.len(), 121);
        assert_eq!(ecmwf.len(), 121);
        assert_eq!(gfs, cams);
        assert_eq!(gfs, ecmwf);
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
        assert_eq!(args.output_root, PathBuf::from("/data/om_webp"));
        assert_eq!(args.strict_data_root, PathBuf::from("/data"));
        assert_eq!(args.minimum_free_bytes, 10_737_418_240);
    }

    #[test]
    fn staging_preflight_uses_uncompressed_rgba_upper_bound() {
        assert_eq!(
            estimate_staging_bytes(597 * 495, 16, 121).unwrap(),
            (597_u64 * 495 * 4 * 16 * 121) + 16 * 1024 * 1024
        );
    }

    #[test]
    fn ecmwf_scope_uses_model_product_and_raw_group_contracts() {
        let args = Args::try_parse_from([
            "om-webp",
            "--scope",
            "ecmwf_ifs025",
            "--decoder-lib",
            "/tmp/libomfileformat.so",
        ])
        .unwrap();

        assert_eq!(args.scope.name(), "ecmwf_ifs025");
        assert_eq!(args.scope.group(), "ecmwf");
        assert_eq!(args.scope.product_dir(), "ecmwf_ifs025");
        assert_eq!(args.scope.manifest_name(), "ecmwf_ifs025_data.json");
        assert_eq!(args.scope.weather_model(), WeatherModel::EcmwfIfs025);
    }

    #[test]
    fn catalog_publishes_ecmwf_under_the_model_key() {
        let catalog = catalog_payload();
        let product = &catalog["products"]["ecmwf_ifs025"];
        assert_eq!(product["source"], "ecmwf_ifs025");
        assert_eq!(product["manifest"], "ecmwf_ifs025_data.json");
        assert_eq!(
            product["data_attribution"]["provider"],
            "European Centre for Medium-Range Weather Forecasts (ECMWF)"
        );
        assert_eq!(product["data_attribution"]["license"], "CC-BY-4.0");
        assert_eq!(product["data_attribution"]["modified"], true);
        assert!(product["data_attribution"]["transformations"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        assert!(product["layers"].get("gust").is_some());
        assert!(product["layers"].get("vis").is_none());
        assert!(product["layers"].get("uv_index").is_none());
        assert!(product["layers"].get("showers").is_none());
        assert!(product["layers"].get("tp").is_some());
        assert!(product["unavailable_layers"].get("gust").is_none());
    }
}
