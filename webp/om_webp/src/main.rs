use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDateTime, TimeZone, Utc};
use clap::{Parser, ValueEnum};
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageEncoder};
use om_api::official::OfficialDecoder;
use om_api::query::{
    read_variable_grid_series, round_variable_output_value, with_weather_model, WeatherModel,
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

const WEBP_CONTRACT_VERSION: u32 = 2;
const RENDERER_REVISION: &str = match option_env!("OM_BUILD_REVISION") {
    Some(revision) => revision,
    None => "unversioned",
};
const GFS_100_TO_120_WIND_SCALE: f32 = 1.020_684_4;

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
    Layer::scalar(
        "surface_temperature",
        "surface_temperature",
        "C",
        -100.0,
        100.0,
        -100.0,
        100.0,
    ),
    Layer::scalar("t80m", "temperature_80m", "C", -100.0, 100.0, -100.0, 100.0),
    Layer::scalar(
        "t100m",
        "temperature_100m",
        "C",
        -100.0,
        100.0,
        -100.0,
        100.0,
    ),
    // The public API defines 120 m temperature as the GFS 100 m value.
    Layer::scalar(
        "t120m",
        "temperature_100m",
        "C",
        -100.0,
        100.0,
        -100.0,
        100.0,
    ),
    Layer::scalar("d2m", "dew_point_2m", "C", -100.0, 100.0, -100.0, 100.0),
    Layer::scalar("r2", "relative_humidity_2m", "%", 0.0, 100.0, 0.0, 100.0),
    Layer::wind("wind", "wind_u_component_10m", "wind_v_component_10m"),
    Layer::wind("wind_80m", "wind_u_component_80m", "wind_v_component_80m"),
    Layer::wind(
        "wind_100m",
        "wind_u_component_100m",
        "wind_v_component_100m",
    ),
    Layer::scaled_wind(
        "wind_120m",
        "wind_u_component_100m",
        "wind_v_component_100m",
        GFS_100_TO_120_WIND_SCALE,
    ),
    Layer::scalar(
        "freezing_level_height",
        "freezing_level_height",
        "m",
        0.0,
        20000.0,
        0.0,
        1.0,
    ),
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
    Layer::scalar(
        "surface_temperature",
        "surface_temperature",
        "C",
        -100.0,
        100.0,
        -100.0,
        100.0,
    ),
    Layer::scalar("d2m", "dew_point_2m", "C", -100.0, 100.0, -100.0, 100.0),
    Layer::scalar("r2", "relative_humidity_2m", "%", 0.0, 100.0, 0.0, 100.0),
    Layer::wind("wind", "wind_u_component_10m", "wind_v_component_10m"),
    Layer::wind(
        "wind_100m",
        "wind_u_component_100m",
        "wind_v_component_100m",
    ),
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

// IFS 9 km is decoded from the immutable native OM coverage materialized from
// the private downloader's cropped RegionPack transport batch.
// Keep the established client encoding contract and publish only fields that
// the EC9 source actually carries; visibility, showers, and 200 m wind are
// available here even though they are absent from the free IFS 0.25 feed.
const ECMWF_IFS9KM_LAYERS: &[Layer] = &[
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
    Layer::scalar(
        "surface_temperature",
        "surface_temperature",
        "C",
        -100.0,
        100.0,
        -100.0,
        100.0,
    ),
    Layer::scalar("d2m", "dew_point_2m", "C", -100.0, 100.0, -100.0, 100.0),
    Layer::scalar("r2", "relative_humidity_2m", "%", 0.0, 100.0, 0.0, 100.0),
    Layer::wind("wind", "wind_u_component_10m", "wind_v_component_10m"),
    Layer::wind(
        "wind_100m",
        "wind_u_component_100m",
        "wind_v_component_100m",
    ),
    Layer::wind(
        "wind_200m",
        "wind_u_component_200m",
        "wind_v_component_200m",
    ),
    Layer::scalar("tp", "precipitation", "mm", 0.0, 600.0, 0.0, 100.0),
    Layer::scalar("showers", "showers", "mm", 0.0, 600.0, 0.0, 100.0),
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
];

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Scope {
    Gfs,
    Cams,
    #[value(name = "ecmwf_ifs025", alias = "ecmwf", alias = "ec")]
    EcmwfIfs025,
    #[value(name = "ecmwf_ifs9km", alias = "ec9", alias = "ecmwf9km")]
    EcmwfIfs9km,
}

impl Scope {
    fn group(self) -> &'static str {
        match self {
            Self::Gfs => "gfs",
            Self::Cams => "cams",
            Self::EcmwfIfs025 => "ecmwf",
            Self::EcmwfIfs9km => "ecmwf_ifs9km",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Gfs => "gfs",
            Self::Cams => "cams",
            Self::EcmwfIfs025 => "ecmwf_ifs025",
            Self::EcmwfIfs9km => "ecmwf_ifs9km",
        }
    }

    fn product_dir(self) -> &'static str {
        match self {
            Self::Gfs => "gfs013_surface",
            Self::Cams => "cams_global",
            Self::EcmwfIfs025 => "ecmwf_ifs025",
            Self::EcmwfIfs9km => "ecmwf_ifs9km",
        }
    }

    fn manifest_name(self) -> &'static str {
        match self {
            Self::Gfs => "gfs013_surface_data.json",
            Self::Cams => "cams_global_data.json",
            Self::EcmwfIfs025 => "ecmwf_ifs025_data.json",
            Self::EcmwfIfs9km => "ecmwf_ifs9km_data.json",
        }
    }

    fn layers(self) -> &'static [Layer] {
        match self {
            Self::Gfs => GFS_LAYERS,
            Self::Cams => CAMS_LAYERS,
            Self::EcmwfIfs025 => ECMWF_IFS025_LAYERS,
            Self::EcmwfIfs9km => ECMWF_IFS9KM_LAYERS,
        }
    }

    fn weather_model(self) -> WeatherModel {
        match self {
            Self::Gfs | Self::Cams => WeatherModel::Gfs,
            Self::EcmwfIfs025 => WeatherModel::EcmwfIfs025,
            Self::EcmwfIfs9km => WeatherModel::EcmwfIfs9km,
        }
    }

    fn tolerate_unavailable_layers(self) -> bool {
        !matches!(self, Self::EcmwfIfs025 | Self::EcmwfIfs9km)
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
                    "native-grid range extraction",
                    "temporal interpolation",
                    "unit conversion and derived-variable calculation",
                    "lossless WebP encoding",
                ],
            }),
            Self::EcmwfIfs9km => Some(DataAttribution {
                attribution: "This service is based on data and products of the European Centre for Medium-Range Weather Forecasts (ECMWF). Contains modified ECMWF data.",
                provider: "European Centre for Medium-Range Weather Forecasts (ECMWF)",
                provider_url: "https://www.ecmwf.int/",
                distributor: "ECMWF Open Data",
                distributor_url: "https://www.ecmwf.int/en/forecasts/datasets/open-data",
                license: "CC-BY-4.0",
                license_url: "https://creativecommons.org/licenses/by/4.0/",
                terms_url: "https://apps.ecmwf.int/datasets/licences/general/",
                modified: true,
                transformations: &[
                    "HTTP range extraction and spatial subsetting",
                    "materialization into immutable Open-Meteo OM arrays",
                    "nearest-grid sampling to the published regular grid",
                    "temporal interpolation",
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
    /// Dedicated filesystem that contains every large writable WebP artifact.
    ///
    /// The raw OM source may be on a different, read-only filesystem. This is
    /// intentional on small two-disk nodes: rendering must never write through
    /// `data_root`, while staging, releases, and current markers must remain on
    /// this dedicated output filesystem.
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
    #[arg(long, default_value_t = 6, env = "OM_WEBP_SERIES_BLOCK_HOURS")]
    series_block_hours: usize,
    #[arg(long)]
    layers: Option<String>,
    #[arg(long)]
    public_root: Option<PathBuf>,
    #[arg(long, default_value_t = 1)]
    keep_releases: usize,
}

#[derive(Debug, Deserialize)]
struct GroupReady {
    status: String,
    latest_complete_run: String,
    release_id: String,
    #[serde(default)]
    public_start_utc: Option<DateTime<Utc>>,
    #[serde(default)]
    products: BTreeMap<String, ReadyProduct>,
}

#[derive(Debug, Deserialize)]
struct ReadyProduct {
    grid: ReadyGrid,
}

#[derive(Debug, Deserialize)]
struct ReadyGrid {
    grid_type: String,
    nx: usize,
    ny: usize,
    dx: f64,
    dy: f64,
    lon_min: f64,
    lat_min: f64,
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
        Self::scaled_wind(name, variable, variable_v, 1.0)
    }

    const fn scaled_wind(
        name: &'static str,
        variable: &'static str,
        variable_v: &'static str,
        multiplier: f32,
    ) -> Self {
        Self {
            name,
            variable,
            variable_v: Some(variable_v),
            unit: "m/s",
            min: -100.0,
            max: 100.0,
            vmin: -100.0,
            scale: 10.0,
            multiplier,
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
    renderer_revision: String,
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
        let ancestor = existing_ancestor(path)?;
        if fs::metadata(&ancestor)?.dev() != root_device {
            bail!(
                "strict data path is not on the dedicated output filesystem {}: {}",
                resolved_root.display(),
                resolved.display()
            );
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

#[cfg(target_os = "linux")]
fn release_source_group_memory() {
    // A source series contains every output frame so the native OM decoder
    // inflates each time slab only once.  glibc may otherwise retain those
    // large, short-lived allocations in its arenas after the Vecs are
    // dropped, which makes RSS grow once per source group on small production
    // hosts.  Trimming here preserves the fast full-series read while keeping
    // the process peak close to one source group.
    // SAFETY: malloc_trim only asks the process allocator to return unused
    // heap pages; no live allocation is invalidated.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(target_os = "linux"))]
fn release_source_group_memory() {}

fn source_read_block_frames(
    scope: Scope,
    variable: &str,
    configured_block_frames: usize,
    total_frames: usize,
) -> usize {
    // Direct native GFS/CAMS slabs and ordinary ECMWF variables are fastest
    // and no larger when decoded once for the complete output axis. Repeating
    // an ECMWF read for every six output frames rebuilds all retained source
    // runs and their regular three-hour interpolation grids, which measured
    // almost thirteen times slower without lowering that decoder peak.
    //
    // Weather code is different: it holds cloud cover, precipitation, snow,
    // and instability dependencies at the same time. Keep that derived group
    // bounded on the relatively dense GFS render grid. ECMWF and CAMS render
    // on their much smaller native OM regional grids, so a complete source
    // series remains within the production memory guard and avoids rebuilding
    // ECMWF run stitching for every six output hours.
    // IFS 9 km weather code expands several dependency grids together.  One
    // native three-hour interval keeps that derived group below the memory
    // ceiling of the 4 GiB production host without slowing every EC9 layer.
    if variable == "weather_code" && matches!(scope, Scope::EcmwfIfs9km) {
        configured_block_frames.min(3).min(total_frames)
    } else if matches!(scope, Scope::EcmwfIfs9km)
        || (variable == "weather_code" && matches!(scope, Scope::Gfs))
    {
        configured_block_frames.min(total_frames)
    } else {
        total_frames
    }
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
    validate_strict_data_layout(&args.strict_data_root, &[&args.output_root])?;
    prune_stale_staging(&args.output_root, args.scope)?;
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
    let selected = select_layers(args.scope.layers(), args.layers.as_deref())?;
    let current_marker = args
        .output_root
        .join("current")
        .join(format!("{}.json", args.scope.name()));
    if marker_matches(
        &current_marker,
        &ready.release_id,
        &ready.latest_complete_run,
        &selected,
        RENDERER_REVISION,
    )? {
        prune_releases(&args.output_root, args.scope, args.keep_releases.max(1))?;
        println!("{{\"status\":\"skipped\",\"reason\":\"release already rendered\",\"scope\":\"{}\",\"release_id\":\"{}\"}}", args.scope.group(), ready.release_id);
        return Ok(());
    }

    let grid = compute_scope_grid(
        args.scope,
        &ready,
        args.left_lon,
        args.right_lon,
        args.bottom_lat,
        args.top_lat,
    )?;
    let start = match args.scope {
        Scope::EcmwfIfs9km => ready
            .public_start_utc
            .context("ECMWF IFS 9 km ready marker has no public_start_utc")?,
        Scope::Gfs | Scope::Cams | Scope::EcmwfIfs025 => parse_run(&ready.latest_complete_run)?,
    };
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
    let mut completed_images = 0_usize;
    let layer_groups = group_layers_by_source(&selected);
    pool.install(|| {
        with_weather_model(args.scope.weather_model(), || {
            for (group_index, layers) in layer_groups.iter().enumerate() {
                let source_layer = layers[0];
                let group_started = Instant::now();
                println!(
                    "进度｜阶段：读取 OM｜类型：{}｜批次：{}｜源组：{}/{}｜变量：{}",
                    args.scope.group().to_uppercase(),
                    ready.latest_complete_run,
                    group_index + 1,
                    layer_groups.len(),
                    source_layer.variable,
                );
                let read_block_frames = source_read_block_frames(
                    args.scope,
                    source_layer.variable,
                    args.series_block_hours,
                    times.len(),
                );
                for source_times in times.chunks(read_block_frames) {
                    let values = read_layer_grid_series(
                        &snapshot,
                        &decoder,
                        source_layer.variable,
                        source_times,
                        &grid,
                        args.scope,
                        args.scope.tolerate_unavailable_layers(),
                    )?;
                    let values_v = match source_layer.variable_v {
                        Some(variable) => Some(read_layer_grid_series(
                            &snapshot,
                            &decoder,
                            variable,
                            source_times,
                            &grid,
                            args.scope,
                            args.scope.tolerate_unavailable_layers(),
                        )?),
                        None => None,
                    };
                    for layer in layers {
                        write_layer_series(
                            &grid,
                            layer,
                            source_times,
                            &values,
                            values_v.as_deref(),
                            args.series_block_hours,
                            &args.output_root,
                            &product_staging,
                            args.minimum_free_bytes,
                            batch,
                            selected.len(),
                            times.len(),
                            &mut completed_images,
                            &mut written_bytes,
                            &total_invalid,
                            &mut last_progress_at,
                            &mut last_progress_bytes,
                            args.scope,
                            &ready.latest_complete_run,
                        )?;
                    }
                    drop(values_v);
                    drop(values);
                    release_source_group_memory();
                }
                println!(
                    "进度｜阶段：完成源组｜类型：{}｜批次：{}｜源组：{}/{}｜变量：{}｜耗时：{:.3}s",
                    args.scope.group().to_uppercase(),
                    ready.latest_complete_run,
                    group_index + 1,
                    layer_groups.len(),
                    source_layer.variable,
                    group_started.elapsed().as_secs_f64(),
                );
            }
            Ok(())
        })
    })?;

    if completed_images != selected.len() * times.len() {
        bail!(
            "WebP image count mismatch: completed={} expected={}",
            completed_images,
            selected.len() * times.len()
        );
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
        &selected,
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

fn compute_ecmwf_ifs9km_grid(left: f64, right: f64, bottom: f64, top: f64) -> Result<RegionGrid> {
    const STEP: f64 = 360.0 / 4_608.0;
    let x0 = (((left + 180.0) / STEP) - 1e-9).ceil().max(0.0) as usize;
    let x1 = (((right + 180.0) / STEP) + 1e-9).floor().min(4_607.0) as usize;
    let y0 = (((bottom + 90.0) / STEP) - 1e-9).ceil().max(0.0) as usize;
    let y1 = (((top + 90.0) / STEP) + 1e-9).floor().min(2_304.0) as usize;
    if x0 > x1 || y0 > y1 {
        bail!("region does not overlap the ECMWF IFS 9 km render grid");
    }
    let longitudes = (x0..=x1)
        .map(|x| round6(-180.0 + x as f64 * STEP))
        .collect::<Vec<_>>();
    let latitudes = (y0..=y1)
        .rev()
        .map(|y| round6(-90.0 + y as f64 * STEP))
        .collect::<Vec<_>>();
    let sample_bounds = Bounds {
        lon_min: *longitudes
            .first()
            .context("EC9 render grid has no longitude")?,
        lat_min: *latitudes
            .last()
            .context("EC9 render grid has no latitude")?,
        lon_max: *longitudes
            .last()
            .context("EC9 render grid has no longitude")?,
        lat_max: *latitudes
            .first()
            .context("EC9 render grid has no latitude")?,
    };
    let display_bounds = Bounds {
        lon_min: round6(sample_bounds.lon_min - STEP / 2.0),
        lat_min: round6(sample_bounds.lat_min - STEP / 2.0),
        lon_max: round6(sample_bounds.lon_max + STEP / 2.0),
        lat_max: round6(sample_bounds.lat_max + STEP / 2.0),
    };
    Ok(RegionGrid {
        manifest: GridManifest {
            width: longitudes.len(),
            height: latitudes.len(),
            row_order: "north_to_south",
            dx: round6(STEP),
            dy: round6(STEP),
            sample_bounds,
            display_bounds,
        },
        latitudes,
        longitudes,
    })
}

fn compute_scope_grid(
    scope: Scope,
    ready: &GroupReady,
    left: f64,
    right: f64,
    bottom: f64,
    top: f64,
) -> Result<RegionGrid> {
    match scope {
        // Preserve the established public GFS render extent. EC and CAMS use
        // every cell from their immutable, already regionalized OM products.
        Scope::Gfs => compute_grid(left, right, bottom, top),
        Scope::EcmwfIfs025 | Scope::Cams => compute_native_product_grid(scope, ready),
        // EC9 is stored losslessly as a compact reduced-Gaussian topology.
        // WebP remains a regular render product, so its output grid is not the
        // storage array's one-dimensional packed axis.
        Scope::EcmwfIfs9km => compute_ecmwf_ifs9km_grid(left, right, bottom, top),
    }
}

fn compute_native_product_grid(scope: Scope, ready: &GroupReady) -> Result<RegionGrid> {
    let product = ready
        .products
        .get(scope.product_dir())
        .with_context(|| format!("ready manifest has no {} product", scope.product_dir()))?;
    let source = &product.grid;
    if source.grid_type != "regional_regular_lat_lon" {
        bail!(
            "unsupported {} source grid type: {}",
            scope.name(),
            source.grid_type
        );
    }
    if source.nx == 0
        || source.ny == 0
        || !source.dx.is_finite()
        || !source.dy.is_finite()
        || !source.lon_min.is_finite()
        || !source.lat_min.is_finite()
        || source.dx <= 0.0
        || source.dy <= 0.0
    {
        bail!("invalid {} native source grid", scope.name());
    }

    let longitudes = (0..source.nx)
        .map(|x| round6(source.lon_min + x as f64 * source.dx))
        .collect::<Vec<_>>();
    let latitudes = (0..source.ny)
        .rev()
        .map(|y| round6(source.lat_min + y as f64 * source.dy))
        .collect::<Vec<_>>();
    let sample_bounds = Bounds {
        lon_min: *longitudes.first().context("native grid has no longitude")?,
        lat_min: *latitudes.last().context("native grid has no latitude")?,
        lon_max: *longitudes.last().context("native grid has no longitude")?,
        lat_max: *latitudes.first().context("native grid has no latitude")?,
    };
    let display_bounds = Bounds {
        lon_min: round6(sample_bounds.lon_min - source.dx / 2.0),
        lat_min: round6(sample_bounds.lat_min - source.dy / 2.0),
        lon_max: round6(sample_bounds.lon_max + source.dx / 2.0),
        lat_max: round6(sample_bounds.lat_max + source.dy / 2.0),
    };
    Ok(RegionGrid {
        manifest: GridManifest {
            width: source.nx,
            height: source.ny,
            row_order: "north_to_south",
            dx: round6(source.dx),
            dy: round6(source.dy),
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
                let u = values[index] * layer.multiplier;
                let v = values_v.expect("wind v")[index] * layer.multiplier;
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

fn group_layers_by_source(layers: &[Layer]) -> Vec<Vec<&Layer>> {
    let mut groups: Vec<Vec<&Layer>> = Vec::new();
    for layer in layers {
        if let Some(group) = groups.iter_mut().find(|group| {
            group[0].variable == layer.variable && group[0].variable_v == layer.variable_v
        }) {
            group.push(layer);
        } else {
            groups.push(vec![layer]);
        }
    }
    groups
}

#[allow(clippy::too_many_arguments)]
fn write_layer_series(
    grid: &RegionGrid,
    layer: &Layer,
    times: &[DateTime<Utc>],
    values: &[Vec<f32>],
    values_v: Option<&[Vec<f32>]>,
    encode_block_frames: usize,
    output_root: &Path,
    product_staging: &Path,
    minimum_free_bytes: u64,
    batch: i64,
    total_layers: usize,
    total_frames: usize,
    completed_images: &mut usize,
    written_bytes: &mut u64,
    total_invalid: &std::sync::atomic::AtomicUsize,
    last_progress_at: &mut Instant,
    last_progress_bytes: &mut u64,
    scope: Scope,
    run: &str,
) -> Result<()> {
    if values.len() != times.len() {
        bail!(
            "source series length mismatch for {}: values={} times={}",
            layer.name,
            values.len(),
            times.len()
        );
    }
    if let Some(values_v) = values_v {
        if values_v.len() != times.len() {
            bail!(
                "vector source series length mismatch for {}: values_v={} times={}",
                layer.name,
                values_v.len(),
                times.len()
            );
        }
    }
    for block_start in (0..times.len()).step_by(encode_block_frames) {
        let block_end = (block_start + encode_block_frames).min(times.len());
        let encoded = (block_start..block_end)
            .into_par_iter()
            .enumerate()
            .map(|(_, index)| {
                encode_layer_values(
                    grid,
                    layer,
                    &values[index],
                    values_v.as_ref().map(|series| series[index].as_slice()),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        for (offset, rendered) in encoded.into_iter().enumerate() {
            let frame_index = block_start + offset;
            let stem = format!("{}_{}", times[frame_index].timestamp(), batch);
            ensure_free_space(output_root, minimum_free_bytes, rendered.bytes.len() as u64)?;
            *written_bytes += rendered.bytes.len() as u64;
            fs::write(
                product_staging
                    .join(rendered.layer_name)
                    .join(format!("{stem}.webp")),
                rendered.bytes,
            )?;
            total_invalid.fetch_add(
                rendered.invalid_points,
                std::sync::atomic::Ordering::Relaxed,
            );
            *completed_images += 1;
            let progress_elapsed = last_progress_at.elapsed();
            if progress_elapsed.as_secs() >= 60 {
                let growth_bytes = written_bytes.saturating_sub(*last_progress_bytes);
                let speed_mib_s = growth_bytes as f64
                    / progress_elapsed.as_secs_f64().max(0.001)
                    / 1024.0
                    / 1024.0;
                let equivalent_frames = completed_images.div_ceil(total_layers).min(total_frames);
                println!(
                    "进度｜阶段：生成 WebP｜类型：{}｜批次：{}｜帧：{}/{}｜图层：{}｜近一分钟增长：{:.1} MiB｜速度：{:.2} MiB/s",
                    scope.group().to_uppercase(),
                    run,
                    equivalent_frames,
                    total_frames,
                    layer.name,
                    growth_bytes as f64 / 1024.0 / 1024.0,
                    speed_mib_s
                );
                *last_progress_at = Instant::now();
                *last_progress_bytes = *written_bytes;
            }
        }
    }
    Ok(())
}

fn read_layer_grid_series(
    snapshot: &OmDataSnapshot,
    decoder: &OfficialDecoder,
    variable: &str,
    times: &[DateTime<Utc>],
    grid: &RegionGrid,
    scope: Scope,
    tolerate_unavailable: bool,
) -> Result<Vec<Vec<f32>>> {
    // Read every requested output frame once for this source dependency. The
    // caller groups layers that share a source and encodes the returned frames
    // in small blocks, avoiding both repeated native ECMWF regularization and
    // the unbounded multi-dependency request cache used by the point API.
    // EC9 stitches five retained runs on an hourly intermediate. Building that
    // intermediate for all 743 rows at once can exceed a 4 GiB production
    // host even with one renderer worker. Process latitude strips instead;
    // each returned frame remains in the same row-major order, while the peak
    // retained-run working set is bounded independently of the full region.
    let latitude_block_rows = source_spatial_block_rows(scope, grid.latitudes.len());
    let values = if latitude_block_rows < grid.latitudes.len() {
        (|| -> Result<Vec<Vec<f32>>> {
            let mut series = (0..times.len())
                .map(|_| Vec::with_capacity(grid.len()))
                .collect::<Vec<_>>();
            for latitude_block in grid.latitudes.chunks(latitude_block_rows) {
                let block = read_variable_grid_series(
                    snapshot,
                    decoder,
                    variable,
                    times,
                    latitude_block,
                    &grid.longitudes,
                )?;
                if block.len() != times.len() {
                    bail!("regional EC9 source block returned the wrong time count");
                }
                let expected_points = latitude_block.len() * grid.longitudes.len();
                for (frame, block_frame) in series.iter_mut().zip(block) {
                    if block_frame.len() != expected_points {
                        bail!("regional EC9 source block returned the wrong grid size");
                    }
                    frame.extend(block_frame);
                }
            }
            Ok(series)
        })()
    } else {
        read_variable_grid_series(
            snapshot,
            decoder,
            variable,
            times,
            &grid.latitudes,
            &grid.longitudes,
        )
    };
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

fn source_spatial_block_rows(scope: Scope, total_rows: usize) -> usize {
    if matches!(scope, Scope::EcmwfIfs9km) {
        total_rows.min(64)
    } else {
        total_rows
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
        renderer_revision: RENDERER_REVISION.to_string(),
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
    layers: &[Layer],
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
        serde_json::to_vec_pretty(&serde_json::json!({
            "status":"complete",
            "scope":scope.name(),
            "release_id":ready.release_id,
            "run":ready.latest_complete_run,
            "path":release_root,
            "contract_version":WEBP_CONTRACT_VERSION,
            "renderer_revision":RENDERER_REVISION,
            "layers":layers.iter().map(|layer| layer.name).collect::<Vec<_>>(),
        }))?,
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
        "version": WEBP_CONTRACT_VERSION,
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
                    "t80m": "not_published_by_free_ecmwf_ifs025",
                    "t100m": "not_published_by_free_ecmwf_ifs025",
                    "t120m": "not_published_by_free_ecmwf_ifs025",
                    "wind_80m": "not_published_by_free_ecmwf_ifs025",
                    "wind_120m": "not_published_by_free_ecmwf_ifs025",
                    "freezing_level_height": "not_published_by_free_ecmwf_ifs025",
                },
            },
            "ecmwf_ifs9km": {
                "source": "ecmwf_ifs9km",
                "manifest": Scope::EcmwfIfs9km.manifest_name(),
                "file_pattern": "{timestamp}_{batch}.webp",
                "layers": layers(Scope::EcmwfIfs9km),
                "data_attribution": Scope::EcmwfIfs9km.data_attribution(),
                "unavailable_layers": {
                    "uv_index": "not_present_in_ecmwf_ifs9km_regionpack",
                    "t80m": "not_present_in_ecmwf_ifs9km_regionpack",
                    "t100m": "not_present_in_ecmwf_ifs9km_regionpack",
                    "t120m": "not_present_in_ecmwf_ifs9km_regionpack",
                    "wind_80m": "not_present_in_ecmwf_ifs9km_regionpack",
                    "wind_120m": "not_present_in_ecmwf_ifs9km_regionpack",
                    "freezing_level_height": "not_present_in_ecmwf_ifs9km_regionpack",
                },
            }
        }
    })
}

fn source_resolution(scope: Scope, name: &str) -> &'static str {
    match scope {
        Scope::Cams => "44km",
        Scope::EcmwfIfs025 => "25km",
        Scope::EcmwfIfs9km => "9km",
        Scope::Gfs => match name {
            "gust"
            | "vis"
            | "cape"
            | "prmsl"
            | "t80m"
            | "t100m"
            | "t120m"
            | "wind_80m"
            | "wind_100m"
            | "wind_120m"
            | "freezing_level_height" => "28km",
            "precip_phase" | "thunderstorm_code" | "sp" => "28km(13+28)",
            _ => "13km",
        },
    }
}

fn marker_matches(
    path: &Path,
    release_id: &str,
    run: &str,
    layers: &[Layer],
    renderer_revision: &str,
) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    let expected_layers = layers
        .iter()
        .map(|layer| layer.name.to_string())
        .collect::<Vec<_>>();
    Ok(
        value.get("release_id").and_then(|value| value.as_str()) == Some(release_id)
            && value.get("run").and_then(|value| value.as_str()) == Some(run)
            && value
                .get("contract_version")
                .and_then(|value| value.as_u64())
                == Some(u64::from(WEBP_CONTRACT_VERSION))
            && value
                .get("renderer_revision")
                .and_then(|value| value.as_str())
                == Some(renderer_revision)
            && value
                .get("layers")
                .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok())
                .as_deref()
                == Some(expected_layers.as_slice()),
    )
}

fn prune_releases(output_root: &Path, scope: Scope, keep: usize) -> Result<()> {
    let releases = output_root.join("releases");
    if !releases.exists() {
        return Ok(());
    }
    let current_release = output_root
        .join("current")
        .join(format!("{}.json", scope.name()));
    let current_release = fs::read(&current_release)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("path")
                .and_then(|path| path.as_str())
                .map(PathBuf::from)
        });
    let mut candidates = fs::read_dir(&releases)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(scope.product_dir()).exists())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|entry| {
        (
            current_release.as_ref() != Some(&entry.path()),
            std::cmp::Reverse(entry.metadata().and_then(|meta| meta.modified()).ok()),
        )
    });
    for entry in candidates.into_iter().skip(keep) {
        fs::remove_dir_all(entry.path())?;
    }
    Ok(())
}

fn prune_stale_staging(output_root: &Path, scope: Scope) -> Result<()> {
    let staging = output_root.join("staging");
    if !staging.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(staging)?.filter_map(Result::ok) {
        let path = entry.path();
        if path.join(scope.product_dir()).exists() {
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_defaults_bound_memory_and_retain_only_current_release() {
        let args = Args::try_parse_from([
            "om-webp",
            "--scope",
            "gfs",
            "--decoder-lib",
            "libomfileformat.so",
        ])
        .unwrap();
        assert_eq!(args.series_block_hours, 6);
        assert_eq!(args.keep_releases, 1);
    }

    #[test]
    fn layer_source_groups_reuse_shared_dependencies() {
        let groups = group_layers_by_source(GFS_LAYERS);
        let weather = groups
            .iter()
            .find(|group| group[0].variable == "weather_code")
            .unwrap();
        assert_eq!(
            weather.iter().map(|layer| layer.name).collect::<Vec<_>>(),
            vec!["precip_phase", "thunderstorm_code"]
        );
        assert!(groups.len() < GFS_LAYERS.len());
    }

    #[test]
    fn dense_gfs_weather_code_and_every_ec9_layer_use_bounded_source_reads() {
        assert_eq!(
            source_read_block_frames(Scope::Gfs, "weather_code", 6, 121),
            6
        );
        assert_eq!(
            source_read_block_frames(Scope::Gfs, "temperature_2m", 6, 121),
            121
        );
        assert_eq!(
            source_read_block_frames(Scope::EcmwfIfs025, "weather_code", 6, 121),
            121
        );
        assert_eq!(
            source_read_block_frames(Scope::EcmwfIfs025, "temperature_2m", 6, 121),
            121
        );
        assert_eq!(source_read_block_frames(Scope::Cams, "pm2_5", 6, 121), 121);
        assert_eq!(
            source_read_block_frames(Scope::EcmwfIfs9km, "temperature_2m", 6, 121),
            6
        );
        assert_eq!(
            source_read_block_frames(Scope::EcmwfIfs9km, "weather_code", 6, 121),
            3
        );
        assert_eq!(
            source_read_block_frames(Scope::EcmwfIfs9km, "weather_code", 2, 121),
            2
        );
        assert_eq!(source_spatial_block_rows(Scope::EcmwfIfs9km, 743), 64);
        assert_eq!(source_spatial_block_rows(Scope::EcmwfIfs9km, 32), 32);
        assert_eq!(source_spatial_block_rows(Scope::Gfs, 743), 743);
    }

    #[test]
    fn current_marker_is_bound_to_the_renderer_revision() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("gfs.json");
        let layers = &GFS_LAYERS[..2];
        fs::write(
            &marker,
            serde_json::to_vec(&serde_json::json!({
                "release_id": "release-1",
                "run": "2026080500",
                "contract_version": WEBP_CONTRACT_VERSION,
                "renderer_revision": "revision-1",
                "layers": layers.iter().map(|layer| layer.name).collect::<Vec<_>>(),
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(marker_matches(&marker, "release-1", "2026080500", layers, "revision-1").unwrap());
        assert!(!marker_matches(&marker, "release-1", "2026080500", layers, "revision-2").unwrap());
    }

    #[test]
    fn release_pruning_protects_the_published_scope_and_ignores_other_scopes() {
        let root = tempfile::tempdir().unwrap();
        let releases = root.path().join("releases");
        let published = releases.join("published-gfs");
        let obsolete = releases.join("newer-but-obsolete-gfs");
        let cams = releases.join("cams-release");
        fs::create_dir_all(published.join(Scope::Gfs.product_dir())).unwrap();
        fs::create_dir_all(obsolete.join(Scope::Gfs.product_dir())).unwrap();
        fs::create_dir_all(cams.join(Scope::Cams.product_dir())).unwrap();
        fs::create_dir_all(root.path().join("current")).unwrap();
        fs::write(
            root.path().join("current/gfs.json"),
            serde_json::to_vec(&serde_json::json!({"path": published})).unwrap(),
        )
        .unwrap();

        prune_releases(root.path(), Scope::Gfs, 1).unwrap();

        assert!(published.exists());
        assert!(!obsolete.exists());
        assert!(cams.exists());
    }

    #[test]
    fn startup_prunes_only_interrupted_staging_for_the_requested_scope() {
        let root = tempfile::tempdir().unwrap();
        let gfs = root
            .path()
            .join("staging/interrupted-gfs")
            .join(Scope::Gfs.product_dir());
        let cams = root
            .path()
            .join("staging/interrupted-cams")
            .join(Scope::Cams.product_dir());
        fs::create_dir_all(&gfs).unwrap();
        fs::create_dir_all(&cams).unwrap();

        prune_stale_staging(root.path(), Scope::Gfs).unwrap();

        assert!(!gfs.parent().unwrap().exists());
        assert!(cams.exists());
    }

    #[test]
    fn singapore_grid_matches_production_manifest() {
        let grid = compute_grid(70.0, 140.0, 0.0, 58.0).unwrap();
        assert_eq!((grid.manifest.width, grid.manifest.height), (597, 495));
        assert_eq!(grid.manifest.sample_bounds.lon_min, 70.078125);
        assert_eq!(grid.manifest.sample_bounds.lat_max, 57.930354);
    }

    #[test]
    fn ec9_regular_render_grid_is_nominally_nine_kilometres() {
        let grid = compute_ecmwf_ifs9km_grid(70.0, 140.0, 0.0, 58.0).unwrap();
        assert_eq!((grid.manifest.width, grid.manifest.height), (897, 743));
        assert_eq!(grid.manifest.dx, 0.078125);
        assert_eq!(grid.manifest.dy, 0.078125);
        assert_eq!(grid.manifest.sample_bounds.lon_min, 70.0);
        assert_eq!(grid.manifest.sample_bounds.lon_max, 140.0);
        assert_eq!(grid.manifest.sample_bounds.lat_min, 0.0);
        assert_eq!(grid.manifest.sample_bounds.lat_max, 57.96875);
    }

    #[test]
    fn layer_inventory_matches_client_contract() {
        assert_eq!(GFS_LAYERS.len(), 26);
        assert_eq!(CAMS_LAYERS.len(), 4);
        assert_eq!(ECMWF_IFS025_LAYERS.len(), 18);
        assert_eq!(ECMWF_IFS9KM_LAYERS.len(), 21);
        for unavailable in [
            "vis",
            "uv_index",
            "t80m",
            "t100m",
            "t120m",
            "wind_80m",
            "wind_120m",
            "freezing_level_height",
        ] {
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
        let wind_120m = GFS_LAYERS
            .iter()
            .find(|layer| layer.name == "wind_120m")
            .unwrap();
        assert!((wind_120m.multiplier - GFS_100_TO_120_WIND_SCALE).abs() < f32::EPSILON);
        assert!(ECMWF_IFS025_LAYERS
            .iter()
            .any(|layer| layer.name == "wind_100m"));
        for layers in [GFS_LAYERS, ECMWF_IFS025_LAYERS, ECMWF_IFS9KM_LAYERS] {
            let surface_temperature = layers
                .iter()
                .find(|layer| layer.name == "surface_temperature")
                .unwrap();
            assert_eq!(
                (
                    surface_temperature.vmin,
                    surface_temperature.scale,
                    surface_temperature.min,
                    surface_temperature.max,
                ),
                (-100.0, 100.0, -100.0, 100.0)
            );
        }
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
    fn ec9_scope_uses_regionpack_group_and_model_contracts() {
        let args = Args::try_parse_from([
            "om-webp",
            "--scope",
            "ec9",
            "--decoder-lib",
            "/tmp/libomfileformat.so",
        ])
        .unwrap();

        assert_eq!(args.scope.name(), "ecmwf_ifs9km");
        assert_eq!(args.scope.group(), "ecmwf_ifs9km");
        assert_eq!(args.scope.product_dir(), "ecmwf_ifs9km");
        assert_eq!(args.scope.manifest_name(), "ecmwf_ifs9km_data.json");
        assert_eq!(args.scope.weather_model(), WeatherModel::EcmwfIfs9km);
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

        let ec9 = &catalog["products"]["ecmwf_ifs9km"];
        assert_eq!(ec9["source"], "ecmwf_ifs9km");
        assert_eq!(ec9["manifest"], "ecmwf_ifs9km_data.json");
        for available in ["gust", "vis", "showers", "wind_100m", "wind_200m"] {
            assert!(
                ec9["layers"].get(available).is_some(),
                "missing {available}"
            );
            assert!(ec9["unavailable_layers"].get(available).is_none());
        }
        assert_eq!(ec9["data_attribution"]["license"], "CC-BY-4.0");
    }
}
