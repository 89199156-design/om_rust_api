//! Production conversion of Shanghai's cropped `.omranges` GFS frames into
//! Open-Meteo native, time-series-optimised `data_run` files.
//!
//! The interpolation and quantisation contract is a direct Rust port of the
//! pinned production fork's `GenericVariableHandle.generateFullRunData` and
//! `InterpolationInplace.swift`. Publication is immutable and marker-last.

use crate::dem::validate_dem_om_file;
use crate::manifest::{
    load_product_snapshot_for_coverage, BundleEntry, NativeGridMetadata, ProductSnapshot,
};
use crate::native::read_native_array_metadata;
use crate::official::{build_v3_array_metadata_blob, BundleRangeReader, OfficialDecoder};
use crate::query::{
    interpolate_solar_backwards_in_place, interpolation_kind_for_variable, round_to_scalefactor,
    InterpolationKind,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDateTime, SecondsFormat, Utc};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::Write;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, FileExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration as StdDuration, SystemTime};

pub const GFS_MATERIALIZATION_REVISION: &str = "official-hourly-quantized-v4";
pub const GFS_PRODUCTS: [&str; 3] = ["gfs013_surface", "gfs025", "gfs_pressure_profile"];
const DATA_TYPE_FLOAT_ARRAY: u8 = 20;
const COMPRESSION_PFOR_DELTA2D_INT16: u8 = 0;
const PROCESS_VALUES: u64 = 2 * 1024 * 1024;
const DEFAULT_MINIMUM_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ESTIMATE_OVERHEAD_BYTES: u64 = 256 * 1024 * 1024;
// Legacy bundles compress each sparse 2-D hour independently. Native files
// compress each location's dense time series, which is materially smaller.
// Three quarters of the cadence-expanded source payload plus fixed overhead
// is still conservative for the pinned GFS layout (and is backed by a hard
// free-space guard before every output block).
const ESTIMATE_NUMERATOR: u64 = 3;
const ESTIMATE_DENOMINATOR: u64 = 4;
const DEFAULT_STALE_STAGING_AGE: StdDuration = StdDuration::from_secs(24 * 60 * 60);
const NATIVE_LIFECYCLE_MANAGER: &str = "om-native-materialize";

#[derive(Debug, Clone)]
pub struct GfsBuildOptions {
    pub data_root: PathBuf,
    pub dem_root: PathBuf,
    pub latest_run: String,
    pub coverage_id: String,
    pub producer_revision: String,
    pub workers: usize,
    pub minimum_free_bytes: u64,
}

impl GfsBuildOptions {
    pub fn production_minimum_free_bytes() -> u64 {
        DEFAULT_MINIMUM_FREE_BYTES
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GfsDiskPreflight {
    pub available_bytes: u64,
    pub estimated_total_bytes: u64,
    pub existing_staging_bytes: u64,
    pub estimated_additional_bytes: u64,
    pub minimum_free_bytes_after_build: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GfsBuildResult {
    pub coverage_id: String,
    pub staging_path: PathBuf,
    pub source_runs: Vec<String>,
    pub files: u64,
    pub bytes: u64,
    pub reused: bool,
    pub disk_preflight: GfsDiskPreflight,
}

#[derive(Debug, Clone, Serialize)]
pub struct GfsValidationResult {
    pub coverage_id: String,
    pub coverage_path: PathBuf,
    pub source_runs: Vec<String>,
    pub om_files: u64,
    pub decoded_probes: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GfsPublishResult {
    pub coverage_id: String,
    pub coverage_path: PathBuf,
    pub marker_path: PathBuf,
    pub current_path: PathBuf,
    pub cleanup: GfsCleanupResult,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GfsCleanupResult {
    pub retained_coverages: Vec<String>,
    pub removed_coverages: Vec<String>,
    pub removed_staging: Vec<String>,
    pub removed_bytes: u64,
}

#[derive(Debug, Clone)]
struct DomainContract {
    name: &'static str,
    grid: NativeGridMetadata,
}

fn gfs013_contract() -> DomainContract {
    DomainContract {
        name: "ncep_gfs013",
        grid: NativeGridMetadata {
            nx: 615,
            ny: 513,
            lon_min: 69.023_437_5,
            lat_min: -0.995_769_475_000_003,
            dx: 0.117_187_5,
            dy: 0.117_149_35,
            dt_seconds: 3600,
            om_file_length: 481,
            full_nx: Some(3072),
            full_ny: Some(1536),
            x0: Some(2125),
            y0: Some(759),
        },
    }
}

fn gfs025_contract() -> DomainContract {
    DomainContract {
        name: "ncep_gfs025",
        grid: NativeGridMetadata {
            nx: 289,
            ny: 241,
            lon_min: 69.0,
            lat_min: -1.0,
            dx: 0.25,
            dy: 0.25,
            dt_seconds: 3600,
            om_file_length: 481,
            full_nx: Some(1440),
            full_ny: Some(721),
            x0: Some(996),
            y0: Some(356),
        },
    }
}

fn grid_marker(contract: &DomainContract) -> Value {
    let grid = &contract.grid;
    json!({
        "grid_type": "regional_regular_lat_lon",
        "full_nx": grid.full_nx,
        "full_ny": grid.full_ny,
        "x0": grid.x0,
        "y0": grid.y0,
        "nx": grid.nx,
        "ny": grid.ny,
        "lon_min": grid.lon_min,
        "lat_min": grid.lat_min,
        "dx": grid.dx,
        "dy": grid.dy,
        "halo_cells": 0,
        "dt_seconds": grid.dt_seconds,
        "om_file_length": grid.om_file_length,
        "requested_bounds": {
            "left_lon": 69.0,
            "right_lon": 141.0,
            "bottom_lat": -1.0,
            "top_lat": 59.0
        }
    })
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyGroupRelease {
    group: String,
    status: String,
    release_id: String,
    latest_complete_run: String,
    product_manifests: HashMap<String, LegacyProductReady>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyProductReady {
    coverage_id: String,
    status: String,
    latest_complete_run: String,
}

#[derive(Debug, Clone)]
struct LegacySourceRun {
    run: String,
    reference_time: DateTime<Utc>,
    horizon: i64,
    products: HashMap<String, Arc<ProductSnapshot>>,
    coverage_ids: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct VariableTask {
    run: String,
    reference_time: DateTime<Utc>,
    horizon: i64,
    domain: DomainContract,
    variable: String,
    source: Arc<ProductSnapshot>,
    destination: PathBuf,
    disk_probe: PathBuf,
    minimum_free_bytes: u64,
}

type RunVariableInventory = BTreeMap<(String, String), Vec<String>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeRunMeta {
    created_at: String,
    crs_wkt: String,
    reference_time: DateTime<Utc>,
    temporal_resolution_seconds: i64,
    valid_times: Vec<String>,
    variables: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GfsCoverageMarker {
    status: String,
    runtime_format: String,
    group: String,
    coverage_id: String,
    release_id: String,
    coverage_path: String,
    latest_complete_run: String,
    source_runs: Vec<String>,
    source_run_max_forecast_hours: Vec<i64>,
    short_run_count: usize,
    full_run_count: usize,
    historical_max_forecast_hour: i64,
    latest_max_forecast_hour: i64,
    public_start_utc: DateTime<Utc>,
    local_day_start_utc: DateTime<Utc>,
    public_end_utc: DateTime<Utc>,
    public_hours: i64,
    producer_revision: String,
    materialization_revision: String,
    files: u64,
    bytes: u64,
    domains: Vec<String>,
    domain_grids: HashMap<String, Value>,
    products: HashMap<String, GfsNativeProduct>,
    static_sources: HashMap<String, GfsNativeStaticSource>,
}

#[derive(Debug, Deserialize)]
struct GfsNativeProduct {
    coverage_id: String,
    runtime_domain: String,
    grid: NativeGridMetadata,
}

#[derive(Debug, Deserialize)]
struct GfsNativeStaticSource {
    source: String,
    runtime_path: String,
    latitude_chunk_min: i32,
    latitude_chunk_max: i32,
    file_count: usize,
}

const GFS013_REQUIRED_VARIABLES: &[&str] = &[
    "temperature_2m",
    "surface_temperature",
    "cloud_cover",
    "cloud_cover_low",
    "cloud_cover_mid",
    "cloud_cover_high",
    "relative_humidity_2m",
    "precipitation",
    "wind_v_component_10m",
    "wind_u_component_10m",
    "snow_depth",
    "showers",
    "snowfall_water_equivalent",
    "uv_index",
    "uv_index_clear_sky",
    "boundary_layer_height",
    "shortwave_radiation",
    "latent_heat_flux",
    "sensible_heat_flux",
    "diffuse_radiation",
    "total_column_integrated_water_vapour",
    "soil_temperature_0_to_10cm",
    "soil_temperature_10_to_40cm",
    "soil_temperature_40_to_100cm",
    "soil_temperature_100_to_200cm",
    "soil_moisture_0_to_10cm",
    "soil_moisture_10_to_40cm",
    "soil_moisture_40_to_100cm",
    "soil_moisture_100_to_200cm",
];

const GFS025_REQUIRED_SURFACE_VARIABLES: &[&str] = &[
    "pressure_msl",
    "categorical_freezing_rain",
    "temperature_80m",
    "temperature_100m",
    "wind_v_component_80m",
    "wind_u_component_80m",
    "wind_v_component_100m",
    "wind_u_component_100m",
    "wind_gusts_10m",
    "freezing_level_height",
    "cape",
    "lifted_index",
    "convective_inhibition",
    "visibility",
];

const PRESSURE_LEVELS: &[u16] = &[
    1000, 975, 950, 925, 900, 850, 800, 750, 700, 650, 600, 550, 500, 450, 400, 350, 300, 250, 200,
    150, 100, 50,
];

const PRESSURE_VARIABLE_FAMILIES: &[&str] = &[
    "temperature",
    "wind_u_component",
    "wind_v_component",
    "geopotential_height",
    "cloud_cover",
    "relative_humidity",
    "vertical_velocity",
];

#[derive(Debug)]
struct LegacyEntryRangeReader {
    bundle_handle: Arc<File>,
    entry: BundleEntry,
}

impl BundleRangeReader for LegacyEntryRangeReader {
    fn read_original_range(&self, start: u64, count: u64) -> Result<Vec<u8>> {
        let end = start
            .checked_add(count)
            .context("OM source range overflow")?;
        let mut remaining_start = start;
        let mut output = Vec::with_capacity(usize::try_from(count)?);
        let mut local_cursor = self.entry.bundle_offset;
        for range in &self.entry.byte_ranges {
            let original_start = range[0];
            let original_end = range[1]
                .checked_add(1)
                .context("OM source inclusive range overflow")?;
            let length = original_end
                .checked_sub(original_start)
                .context("OM source range is reversed")?;
            if remaining_start >= original_end || end <= original_start {
                local_cursor += length;
                continue;
            }
            let part_start = remaining_start.max(original_start);
            let part_end = end.min(original_end);
            if part_start != remaining_start {
                bail!("requested OM source range has a gap in .omranges payload");
            }
            let local_offset = local_cursor + (part_start - original_start);
            let part_length = part_end - part_start;
            let before = output.len();
            output.resize(before + usize::try_from(part_length)?, 0);
            self.bundle_handle
                .read_exact_at(&mut output[before..], local_offset)?;
            remaining_start = part_end;
            if remaining_start == end {
                return Ok(output);
            }
            local_cursor += length;
        }
        bail!("requested OM source range is not present in .omranges payload")
    }
}

#[derive(Debug)]
struct FullFileRangeReader {
    file: Arc<File>,
}

impl BundleRangeReader for FullFileRangeReader {
    fn read_original_range(&self, start: u64, count: u64) -> Result<Vec<u8>> {
        let mut output = vec![0_u8; usize::try_from(count)?];
        self.file.read_exact_at(&mut output, start)?;
        Ok(output)
    }
}

pub fn build_gfs_coverage(
    options: &GfsBuildOptions,
    decoder: &OfficialDecoder,
) -> Result<GfsBuildResult> {
    validate_build_options(options)?;
    let coverage_parent = options.data_root.join("coverages/gfs");
    fs::create_dir_all(&coverage_parent)?;
    require_real_directory(&coverage_parent, "native coverage parent")?;
    let (sources, legacy_marker) = load_legacy_sources(&options.data_root, &options.latest_run)?;
    let identity = build_identity(options, &sources);
    let target = coverage_parent.join(&options.coverage_id);
    if path_is_real_directory(&target, "immutable native coverage")? {
        let validation = validate_gfs_coverage(&target, decoder)?;
        let identity_path = target.join("build_identity.json");
        let existing: Value =
            serde_json::from_slice(&fs::read(&identity_path).with_context(|| {
                format!("read native build identity: {}", identity_path.display())
            })?)?;
        if existing != identity {
            bail!(
                "immutable native coverage identity differs from requested build; choose a new coverage_id: {}",
                target.display()
            );
        }
        let available_bytes = available_space(&coverage_parent)?;
        return Ok(GfsBuildResult {
            coverage_id: options.coverage_id.clone(),
            staging_path: target,
            source_runs: validation.source_runs,
            files: validation.om_files,
            bytes: validation.bytes,
            reused: true,
            disk_preflight: GfsDiskPreflight {
                available_bytes,
                estimated_total_bytes: validation.bytes,
                existing_staging_bytes: validation.bytes,
                estimated_additional_bytes: 0,
                minimum_free_bytes_after_build: options.minimum_free_bytes,
            },
        });
    }
    let source_runs = sources
        .iter()
        .map(|source| source.run.clone())
        .collect::<Vec<_>>();
    let staging = coverage_parent.join(format!(".incoming_{}", options.coverage_id));
    if !path_is_real_directory(&staging, "native staging")? {
        adopt_compatible_staging(&coverage_parent, &staging, &identity)?;
    }
    let identity_path = staging.join("build_identity.json");
    if path_is_real_directory(&staging, "native staging")? {
        if !identity_path.is_file() {
            bail!(
                "existing native staging has no build identity: {}",
                staging.display()
            );
        }
        let existing: Value = serde_json::from_slice(&fs::read(&identity_path)?)?;
        if existing != identity {
            bail!(
                "existing native staging identity does not match requested build: {}",
                identity_path.display()
            );
        }
    }

    let (tasks, run_variables) = build_variable_tasks(options, &sources, &staging)?;
    let disk_preflight = preflight_native_build(options, &tasks, &staging)?;
    fs::create_dir_all(&staging)?;
    if !identity_path.exists() {
        atomic_write_json(&identity_path, &identity)?;
    }
    let completed = AtomicUsize::new(0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.workers)
        .thread_name(|index| format!("om-materialize-{index}"))
        .build()?;
    pool.install(|| {
        tasks.par_iter().try_for_each(|task| {
            materialize_variable(task, decoder)?;
            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(
                completed = done,
                total = tasks.len(),
                run = %task.run,
                domain = task.domain.name,
                variable = %task.variable,
                "materialized native GFS variable"
            );
            Ok::<_, anyhow::Error>(())
        })
    })?;

    write_run_metadata(&sources, &run_variables, &staging)?;
    materialize_static_elevation(
        decoder,
        &options.data_root.join("static/ncep_gfs013/HSURF.om"),
        &staging.join("ncep_gfs013/static/HSURF.om"),
        &gfs013_contract(),
    )?;
    materialize_static_elevation(
        decoder,
        &options.data_root.join("static/ncep_gfs025/HSURF.om"),
        &staging.join("ncep_gfs025/static/HSURF.om"),
        &gfs025_contract(),
    )?;
    link_dem_chunks(&options.dem_root, &staging)?;

    let marker = build_coverage_marker(options, &sources, legacy_marker, &staging)?;
    atomic_write_json(&staging.join("coverage.json"), &marker)?;
    let validation = validate_gfs_coverage(&staging, decoder)?;
    Ok(GfsBuildResult {
        coverage_id: options.coverage_id.clone(),
        staging_path: staging,
        source_runs,
        files: validation.om_files,
        bytes: validation.bytes,
        reused: false,
        disk_preflight,
    })
}

fn validate_build_options(options: &GfsBuildOptions) -> Result<()> {
    validate_component("coverage_id", &options.coverage_id)?;
    parse_run(&options.latest_run)?;
    if options.workers == 0 || options.workers > 16 {
        bail!("native materializer workers must be between 1 and 16");
    }
    if options.minimum_free_bytes < 512 * 1024 * 1024 {
        bail!("native materializer minimum free space must be at least 512 MiB");
    }
    validate_producer_revision(&options.producer_revision)?;
    if !options.data_root.is_dir() {
        bail!(
            "OM data root does not exist: {}",
            options.data_root.display()
        );
    }
    if !options.dem_root.is_dir() {
        bail!("DEM root does not exist: {}", options.dem_root.display());
    }
    Ok(())
}

fn validate_producer_revision(revision: &str) -> Result<()> {
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("producer_revision must be a full lowercase 40-character Git commit SHA");
    }
    Ok(())
}

fn validate_component(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || Path::new(value).is_absolute()
        || value.contains('/')
        || value.contains('\\')
        || value.contains("..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("unsafe {label}: {value}");
    }
    Ok(())
}

fn require_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("read {label} metadata: {}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("{label} must be a real directory: {}", path.display());
    }
    Ok(())
}

fn path_is_real_directory(path: &Path, label: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                bail!("{label} must be a real directory: {}", path.display());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("read {label}: {}", path.display())),
    }
}

fn parse_run(run: &str) -> Result<DateTime<Utc>> {
    if run.len() != 10 || !run.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("run must be exactly 10 ASCII digits in YYYYMMDDHH format");
    }
    let parsed = NaiveDateTime::parse_from_str(&format!("{run}0000"), "%Y%m%d%H%M%S")?;
    let hour = run[8..10].parse::<u8>()?;
    if hour % 6 != 0 {
        bail!("GFS run hour must be one of 00, 06, 12, or 18 UTC");
    }
    Ok(parsed.and_utc())
}

fn run_relative_path(run: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(
        parse_run(run)?.format("%Y/%m/%d/%H00Z").to_string(),
    ))
}

fn expected_source_runs(latest_run: &str) -> Result<Vec<(String, DateTime<Utc>, i64)>> {
    let latest = parse_run(latest_run)?;
    Ok((0..5)
        .map(|index| {
            let time = latest - Duration::hours((4 - index) * 6);
            let horizon = if index < 3 { 5 } else { 384 };
            (time.format("%Y%m%d%H").to_string(), time, horizon)
        })
        .collect())
}

fn shanghai_day_start_utc(time: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let local = time + Duration::hours(8);
    let midnight = local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .context("construct Shanghai local midnight")?
        .and_utc();
    Ok(midnight - Duration::hours(8))
}

fn validate_group_release_id(release_id: &str) -> Result<()> {
    let Some(digest) = release_id.strip_prefix("gfs-") else {
        bail!("GFS source release has a non-canonical release_id: {release_id}");
    };
    if digest.len() != 16
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("GFS source release has a non-canonical release_id: {release_id}");
    }
    Ok(())
}

fn load_group_release_candidates(data_root: &Path) -> Result<Vec<(Value, LegacyGroupRelease)>> {
    let mut candidate_paths = Vec::new();
    let current = data_root.join("groups/gfs/current/ready_for_processing.json");
    if current.is_file() {
        candidate_paths.push(current);
    }
    let releases_root = data_root.join("groups/gfs/releases");
    if releases_root.is_dir() {
        let mut release_paths = fs::read_dir(&releases_root)?
            .map(|entry| entry.map(|value| value.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        release_paths.sort();
        candidate_paths.extend(
            release_paths
                .into_iter()
                .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json")),
        );
    }

    let mut releases = Vec::new();
    for path in candidate_paths {
        let value: Value = serde_json::from_slice(
            &fs::read(&path)
                .with_context(|| format!("read GFS source release: {}", path.display()))?,
        )?;
        let Ok(release) = serde_json::from_value::<LegacyGroupRelease>(value.clone()) else {
            continue;
        };
        if release.group != "gfs" || release.status != "complete" {
            continue;
        }
        parse_run(&release.latest_complete_run)
            .with_context(|| format!("invalid GFS source release run in {}", path.display()))?;
        validate_group_release_id(&release.release_id)?;
        releases.push((value, release));
    }
    Ok(releases)
}

fn release_coverage_ids(release: &LegacyGroupRelease) -> Result<BTreeMap<String, String>> {
    GFS_PRODUCTS
        .iter()
        .map(|product| {
            let ready = release.product_manifests.get(*product).with_context(|| {
                format!(
                    "GFS release {} has no {product} manifest",
                    release.latest_complete_run
                )
            })?;
            if ready.status != "complete"
                || ready.latest_complete_run != release.latest_complete_run
            {
                bail!(
                    "GFS release {}/{product} is not a complete same-run source",
                    release.latest_complete_run
                );
            }
            validate_component("coverage_id", &ready.coverage_id)?;
            Ok(((*product).to_string(), ready.coverage_id.clone()))
        })
        .collect()
}

pub fn latest_available_gfs_run(data_root: &Path) -> Result<String> {
    load_group_release_candidates(data_root)?
        .into_iter()
        .map(|(_, release)| release.latest_complete_run)
        .max()
        .context("no complete retained GFS source releases are available")
}

pub fn default_gfs_coverage_id(latest_run: &str, producer_revision: &str) -> Result<String> {
    parse_run(latest_run)?;
    validate_producer_revision(producer_revision)?;
    Ok(format!(
        "gfs_native_{}_{}_{}",
        latest_run,
        GFS_MATERIALIZATION_REVISION,
        &producer_revision[..12]
    ))
}

fn load_legacy_sources(
    data_root: &Path,
    latest_run: &str,
) -> Result<(Vec<LegacySourceRun>, Value)> {
    let releases = load_group_release_candidates(data_root)?;
    let matching_latest = releases
        .iter()
        .filter(|(_, release)| release.latest_complete_run == latest_run)
        .collect::<Vec<_>>();
    let (legacy_marker, _) = matching_latest
        .first()
        .copied()
        .with_context(|| format!("missing retained GFS source release {latest_run}"))?;
    let latest_identities = matching_latest
        .iter()
        .map(|(_, release)| release_coverage_ids(release))
        .collect::<Result<BTreeSet<_>>>()?;
    if latest_identities.len() != 1 {
        bail!("retained GFS run {latest_run} has ambiguous source coverage identities");
    }

    let mut sources = Vec::new();
    for (run, reference_time, horizon) in expected_source_runs(latest_run)? {
        let candidates = releases
            .iter()
            .filter(|(_, release)| release.latest_complete_run == run)
            .collect::<Vec<_>>();
        let (_, release) = candidates.first().copied().with_context(|| {
                format!(
                    "missing retained GFS source run {run}; frozen native publication requires three short and two full consecutive runs"
                )
            })?;
        let identities = candidates
            .iter()
            .map(|(_, candidate)| release_coverage_ids(candidate))
            .collect::<Result<BTreeSet<_>>>()?;
        if identities.len() != 1 {
            bail!("retained GFS run {run} has ambiguous source coverage identities");
        }
        let mut products = HashMap::new();
        let coverage_ids = release_coverage_ids(release)?;
        for product in GFS_PRODUCTS {
            let ready = release
                .product_manifests
                .get(product)
                .with_context(|| format!("GFS release {run} has no {product} manifest"))?;
            let snapshot = Arc::new(load_product_snapshot_for_coverage(
                data_root,
                product,
                &ready.coverage_id,
            )?);
            if snapshot
                .bundle_file
                .entries
                .iter()
                .any(|entry| entry.source_run != run)
            {
                bail!(
                    "GFS source {run}/{product} contains fallback entries from another run; refusing to label it as frozen same-run native data"
                );
            }
            let max_hour = snapshot
                .bundle_file
                .entries
                .iter()
                .map(|entry| entry.forecast_hour)
                .max()
                .context("legacy GFS source has no frames")?;
            if max_hour != horizon {
                bail!("GFS source {run}/{product} horizon is {max_hour}, expected {horizon}");
            }
            products.insert(product.to_string(), snapshot);
        }
        sources.push(LegacySourceRun {
            run,
            reference_time,
            horizon,
            products,
            coverage_ids,
        });
    }
    Ok((sources, legacy_marker.clone()))
}

fn build_identity(options: &GfsBuildOptions, sources: &[LegacySourceRun]) -> Value {
    let source_coverages = sources
        .iter()
        .map(|source| {
            (
                source.run.clone(),
                serde_json::to_value(&source.coverage_ids).expect("serializable coverage map"),
            )
        })
        .collect::<Map<_, _>>();
    json!({
        "version": 1,
        "lifecycle_manager": NATIVE_LIFECYCLE_MANAGER,
        "algorithm": GFS_MATERIALIZATION_REVISION,
        "coverage_id": options.coverage_id,
        "latest_run": options.latest_run,
        "producer_revision": options.producer_revision,
        "source_coverages": source_coverages,
        "domain_grids": {
            "ncep_gfs013": grid_marker(&gfs013_contract()),
            "ncep_gfs025": grid_marker(&gfs025_contract())
        }
    })
}

fn resume_identity(value: &Value) -> Option<Value> {
    let mut object = value.as_object()?.clone();
    object.remove("coverage_id")?;
    object.remove("producer_revision")?;
    Some(Value::Object(object))
}

fn adopt_compatible_staging(
    coverage_parent: &Path,
    requested_staging: &Path,
    requested_identity: &Value,
) -> Result<bool> {
    if path_is_real_directory(requested_staging, "requested native staging")? {
        return Ok(false);
    }
    let requested_resume = resume_identity(requested_identity)
        .context("requested native staging identity is incomplete")?;
    let mut compatible = Vec::new();
    for entry in fs::read_dir(coverage_parent)? {
        let entry = entry?;
        let path = entry.path();
        if path == requested_staging {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Some(directory_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(coverage_id) = directory_name.strip_prefix(".incoming_") else {
            continue;
        };
        let identity_path = path.join("build_identity.json");
        if !identity_path.is_file() {
            continue;
        }
        let identity: Value = match serde_json::from_slice(&fs::read(&identity_path)?) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if identity.get("lifecycle_manager").and_then(Value::as_str)
            != Some(NATIVE_LIFECYCLE_MANAGER)
            || identity.get("algorithm").and_then(Value::as_str)
                != Some(GFS_MATERIALIZATION_REVISION)
            || identity.get("coverage_id").and_then(Value::as_str) != Some(coverage_id)
        {
            continue;
        }
        let (Some(latest_run), Some(producer_revision)) = (
            identity.get("latest_run").and_then(Value::as_str),
            identity.get("producer_revision").and_then(Value::as_str),
        ) else {
            continue;
        };
        if default_gfs_coverage_id(latest_run, producer_revision)
            .ok()
            .as_deref()
            != Some(coverage_id)
            || resume_identity(&identity).as_ref() != Some(&requested_resume)
        {
            continue;
        }
        // This rejects every symlink anywhere in the managed staging tree
        // before the same-filesystem rename is allowed.
        tree_bytes_and_latest_modified(&path)?;
        compatible.push(path);
    }
    if compatible.len() > 1 {
        bail!(
            "multiple compatible native staging directories require operator review: {}",
            compatible
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let Some(existing) = compatible.pop() else {
        return Ok(false);
    };
    fs::rename(&existing, requested_staging).with_context(|| {
        format!(
            "adopt compatible native staging {} as {}",
            existing.display(),
            requested_staging.display()
        )
    })?;
    atomic_write_json(
        &requested_staging.join("build_identity.json"),
        requested_identity,
    )?;
    sync_directory(coverage_parent)?;
    tracing::info!(
        previous = %existing.display(),
        requested = %requested_staging.display(),
        "adopted compatible native staging after producer revision change"
    );
    Ok(true)
}

fn checked_ceil_ratio(value: u64, numerator: u64, denominator: u64) -> Result<u64> {
    if denominator == 0 {
        bail!("native disk estimate denominator is zero");
    }
    let result = u128::from(value)
        .checked_mul(u128::from(numerator))
        .context("native disk estimate overflow")?
        .checked_add(u128::from(denominator - 1))
        .context("native disk estimate overflow")?
        / u128::from(denominator);
    u64::try_from(result).context("native disk estimate exceeds u64")
}

fn estimate_native_build_bytes(options: &GfsBuildOptions, tasks: &[VariableTask]) -> Result<u64> {
    let mut estimated = 0_u64;
    for task in tasks {
        let entries = source_entries(task)?;
        let first_hour = entries
            .first()
            .map(|entry| entry.forecast_hour)
            .context("native disk estimate found no source frames")?;
        let dense_frames = u64::try_from(task.horizon - first_hour + 1)?;
        let sparse_frames = u64::try_from(entries.len())?;
        let source_bytes = entries.iter().try_fold(0_u64, |total, entry| {
            total
                .checked_add(entry.bundle_bytes)
                .context("native source byte estimate overflow")
        })?;
        let task_estimate = checked_ceil_ratio(source_bytes, dense_frames, sparse_frames)?;
        estimated = estimated
            .checked_add(task_estimate)
            .context("native disk estimate overflow")?;
    }

    for (source, contract) in [
        (
            options.data_root.join("static/ncep_gfs013/HSURF.om"),
            gfs013_contract(),
        ),
        (
            options.data_root.join("static/ncep_gfs025/HSURF.om"),
            gfs025_contract(),
        ),
    ] {
        let source_bytes = source
            .metadata()
            .with_context(|| format!("read static source size: {}", source.display()))?
            .len();
        let regional_cells = contract
            .grid
            .nx
            .checked_mul(contract.grid.ny)
            .context("regional static cell count overflow")?;
        let global_cells = contract
            .grid
            .full_nx
            .context("static grid has no full_nx")?
            .checked_mul(
                contract
                    .grid
                    .full_ny
                    .context("static grid has no full_ny")?,
            )
            .context("global static cell count overflow")?;
        estimated = estimated
            .checked_add(checked_ceil_ratio(
                source_bytes,
                regional_cells,
                global_cells,
            )?)
            .and_then(|value| value.checked_add(1024 * 1024))
            .context("native static byte estimate overflow")?;
    }

    for latitude in 0..=58 {
        let source = options
            .dem_root
            .join("copernicus_dem90/static")
            .join(format!("lat_{latitude}.om"));
        let bytes = source
            .metadata()
            .with_context(|| format!("read DEM source size: {}", source.display()))?
            .len();
        if bytes == 0 {
            bail!(
                "required Copernicus DEM90 chunk is empty: {}",
                source.display()
            );
        }
        estimated = estimated
            .checked_add(bytes)
            .context("native DEM byte estimate overflow")?;
    }

    checked_ceil_ratio(estimated, ESTIMATE_NUMERATOR, ESTIMATE_DENOMINATOR)?
        .checked_add(ESTIMATE_OVERHEAD_BYTES)
        .context("native disk estimate overflow")
}

fn preflight_native_build(
    options: &GfsBuildOptions,
    tasks: &[VariableTask],
    staging: &Path,
) -> Result<GfsDiskPreflight> {
    let estimated_total_bytes = estimate_native_build_bytes(options, tasks)?;
    let existing_staging_bytes = if staging.exists() {
        coverage_stats(staging)?.1
    } else {
        0
    };
    let estimated_additional_bytes = estimated_total_bytes.saturating_sub(existing_staging_bytes);
    let available_bytes = available_space(
        staging
            .parent()
            .context("native staging path has no parent")?,
    )?;
    let required_available = estimated_additional_bytes
        .checked_add(options.minimum_free_bytes)
        .context("native disk preflight byte count overflow")?;
    if available_bytes < required_available {
        bail!(
            "insufficient disk space for native GFS build: available={} estimated_total={} existing_staging={} estimated_additional={} required_post_build_free={}",
            available_bytes,
            estimated_total_bytes,
            existing_staging_bytes,
            estimated_additional_bytes,
            options.minimum_free_bytes
        );
    }
    Ok(GfsDiskPreflight {
        available_bytes,
        estimated_total_bytes,
        existing_staging_bytes,
        estimated_additional_bytes,
        minimum_free_bytes_after_build: options.minimum_free_bytes,
    })
}

fn available_space(path: &Path) -> Result<u64> {
    let path_bytes = path.as_os_str().as_bytes();
    let c_path = CString::new(path_bytes).context("filesystem path contains an interior NUL")?;
    let mut output = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: c_path is NUL-terminated and output points to valid writable memory.
    if unsafe { libc::statvfs(c_path.as_ptr(), output.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("read available disk space for {}", path.display()));
    }
    // SAFETY: statvfs returned success and initialized output.
    let output = unsafe { output.assume_init() };
    output
        .f_bavail
        .checked_mul(output.f_frsize)
        .context("available disk byte count overflow")
}

fn ensure_minimum_free_space(path: &Path, minimum_free_bytes: u64) -> Result<()> {
    let available = available_space(path)?;
    if available < minimum_free_bytes {
        bail!(
            "native GFS materialization stopped before exhausting disk: available={} required_free={}",
            available,
            minimum_free_bytes
        );
    }
    Ok(())
}

fn build_variable_tasks(
    options: &GfsBuildOptions,
    sources: &[LegacySourceRun],
    staging: &Path,
) -> Result<(Vec<VariableTask>, RunVariableInventory)> {
    let mut tasks = Vec::new();
    let mut run_variables = BTreeMap::new();
    for source in sources {
        for (product, domain) in [
            ("gfs013_surface", gfs013_contract()),
            ("gfs025", gfs025_contract()),
            ("gfs_pressure_profile", gfs025_contract()),
        ] {
            let snapshot = source
                .products
                .get(product)
                .with_context(|| format!("source run {} has no {product}", source.run))?
                .clone();
            let variables = snapshot
                .bundle_file
                .entries
                .iter()
                .map(|entry| entry.variable.clone())
                .collect::<BTreeSet<_>>();
            if variables.is_empty() {
                bail!("source run {}/{product} has no variables", source.run);
            }
            let key = (source.run.clone(), domain.name.to_string());
            let domain_variables = run_variables.entry(key).or_insert_with(Vec::new);
            for variable in variables {
                if domain_variables.contains(&variable) {
                    bail!(
                        "duplicate native domain variable {}/{}/{}",
                        source.run,
                        domain.name,
                        variable
                    );
                }
                interpolation_scale(&variable, interpolation_kind_for_variable(&variable))?;
                domain_variables.push(variable.clone());
                let destination = staging
                    .join("data_run")
                    .join(domain.name)
                    .join(run_relative_path(&source.run)?)
                    .join(format!("{variable}.om"));
                tasks.push(VariableTask {
                    run: source.run.clone(),
                    reference_time: source.reference_time,
                    horizon: source.horizon,
                    domain: domain.clone(),
                    variable,
                    source: snapshot.clone(),
                    destination,
                    disk_probe: options.data_root.clone(),
                    minimum_free_bytes: options.minimum_free_bytes,
                });
            }
        }
    }
    for variables in run_variables.values_mut() {
        variables.sort();
    }
    Ok((tasks, run_variables))
}

fn interpolation_scale(variable: &str, kind: InterpolationKind) -> Result<f32> {
    let scale = match kind {
        InterpolationKind::Direct => {
            bail!("unsupported GFS variable interpolation metadata: {variable}")
        }
        InterpolationKind::SolarBackwardsAveraged { scalefactor }
        | InterpolationKind::Linear { scalefactor }
        | InterpolationKind::Backwards { scalefactor }
        | InterpolationKind::BackwardsSum { scalefactor }
        | InterpolationKind::Hermite { scalefactor, .. } => scalefactor,
    };
    if !scale.is_finite() || scale <= 0.0 {
        bail!("invalid GFS variable scale factor for {variable}: {scale}");
    }
    Ok(scale)
}

fn materialize_variable(task: &VariableTask, decoder: &OfficialDecoder) -> Result<()> {
    ensure_minimum_free_space(&task.disk_probe, task.minimum_free_bytes)?;
    let entries = source_entries(task)?;
    let first_hour = entries
        .first()
        .map(|entry| entry.forecast_hour)
        .context("native variable has no source frames")?;
    let expected_first_hour = if variable_omits_hour_zero(task.domain.name, &task.variable) {
        1
    } else {
        0
    };
    if first_hour != expected_first_hour {
        bail!(
            "GFS variable {}/{}/{} starts at f{}, expected f{} from the official hour-zero contract",
            task.run,
            task.domain.name,
            task.variable,
            first_hour,
            expected_first_hour
        );
    }
    if entries.last().map(|entry| entry.forecast_hour) != Some(task.horizon) {
        bail!(
            "GFS variable {}/{}/{} does not reach f{}",
            task.run,
            task.domain.name,
            task.variable,
            task.horizon
        );
    }
    let actual_hours = entries
        .iter()
        .map(|entry| entry.forecast_hour)
        .collect::<Vec<_>>();
    let expected_hours = expected_sparse_forecast_hours(task.horizon, expected_first_hour == 1);
    if actual_hours != expected_hours {
        bail!(
            "GFS variable {}/{}/{} source cadence does not match the official GFS schedule",
            task.run,
            task.domain.name,
            task.variable
        );
    }
    let n_time = u64::try_from(task.horizon - first_hour + 1)?;
    let chunk_x = (1024 / n_time).max(1).min(task.domain.grid.nx);
    let dimensions = vec![task.domain.grid.ny, task.domain.grid.nx, n_time];
    let chunks = vec![1, chunk_x, n_time];
    let kind = interpolation_kind_for_variable(&task.variable);
    let scale_factor = interpolation_scale(&task.variable, kind)?;

    if task.destination.exists() {
        let file = File::open(&task.destination)?;
        let metadata = read_native_array_metadata(&file)?;
        if metadata.dimensions == dimensions
            && metadata.chunks == chunks
            && metadata.data_type == DATA_TYPE_FLOAT_ARRAY
            && metadata.compression == COMPRESSION_PFOR_DELTA2D_INT16
            && metadata.scale_factor == Some(scale_factor)
        {
            return Ok(());
        }
        fs::remove_file(&task.destination).with_context(|| {
            format!(
                "remove invalid resumable OM file {}",
                task.destination.display()
            )
        })?;
    }

    let process_x = ((PROCESS_VALUES / n_time / chunk_x).max(1) * chunk_x)
        .max(1)
        .min(task.domain.grid.nx);
    if process_x != task.domain.grid.nx {
        bail!("native materializer currently requires a complete x block");
    }
    let process_y = (PROCESS_VALUES / n_time / process_x)
        .max(1)
        .min(task.domain.grid.ny);
    let file_name = task
        .destination
        .file_name()
        .and_then(|value| value.to_str())
        .context("native variable destination has no UTF-8 file name")?;
    let temporary = task
        .destination
        .with_file_name(format!(".{file_name}.incoming"));
    if temporary.exists() {
        fs::remove_file(&temporary).with_context(|| {
            format!(
                "remove incomplete native OM file before retry: {}",
                temporary.display()
            )
        })?;
    }
    let mut writer = decoder.create_array_writer(
        &temporary,
        dimensions.clone(),
        chunks,
        scale_factor,
        0.0,
        DATA_TYPE_FLOAT_ARRAY,
        COMPRESSION_PFOR_DELTA2D_INT16,
    )?;
    let sample_points = [
        (0_u64, 0_u64),
        (task.domain.grid.ny / 2, task.domain.grid.nx / 2),
        (task.domain.grid.ny - 1, task.domain.grid.nx - 1),
    ];
    let mut samples = BTreeMap::<(u64, u64), Vec<f32>>::new();
    let requires_interpolation = usize::try_from(n_time)? != entries.len();
    let mut y0 = 0_u64;
    while y0 < task.domain.grid.ny {
        ensure_minimum_free_space(&task.disk_probe, task.minimum_free_bytes)?;
        let height = process_y.min(task.domain.grid.ny - y0);
        let location_count = usize::try_from(height * task.domain.grid.nx)?;
        let n_time_usize = usize::try_from(n_time)?;
        let mut dense = vec![f32::NAN; location_count * n_time_usize];
        for entry in &entries {
            let frame = decode_source_block(task, entry, decoder, y0, height)?;
            if frame.len() != location_count {
                bail!("decoded legacy GFS frame has the wrong regional size");
            }
            let time_index = usize::try_from(entry.forecast_hour - first_hour)?;
            for (location, value) in frame.into_iter().enumerate() {
                dense[location * n_time_usize + time_index] = value;
            }
        }
        if requires_interpolation {
            interpolate_dense_block(task, kind, &mut dense, n_time_usize, y0)?;
            for value in &mut dense {
                if value.is_finite() {
                    *value = round_to_scalefactor(*value, scale_factor);
                }
            }
        }
        if dense.iter().any(|value| value.is_infinite()) {
            bail!(
                "GFS interpolation produced an infinite value for {}/{}/{}",
                task.run,
                task.domain.name,
                task.variable
            );
        }
        for (sample_y, sample_x) in sample_points {
            if sample_y < y0 || sample_y >= y0 + height {
                continue;
            }
            let location = usize::try_from((sample_y - y0) * task.domain.grid.nx + sample_x)?;
            let mut expected =
                dense[location * n_time_usize..(location + 1) * n_time_usize].to_vec();
            // The compressor quantises source frames too, even when no hourly
            // expansion was required.
            for value in &mut expected {
                if value.is_finite() {
                    *value = round_to_scalefactor(*value, scale_factor);
                }
            }
            samples.insert((sample_y, sample_x), expected);
        }
        writer.write_f32_block(&dense, &[height, task.domain.grid.nx, n_time])?;
        y0 += height;
    }
    writer.finish(&task.variable)?;
    validate_written_samples(task, decoder, &samples, &dimensions, &temporary)?;
    fs::rename(&temporary, &task.destination).with_context(|| {
        format!(
            "atomically publish materialized variable {}",
            task.destination.display()
        )
    })?;
    sync_directory(
        task.destination
            .parent()
            .context("native variable destination has no parent")?,
    )?;
    Ok(())
}

fn source_entries(task: &VariableTask) -> Result<Vec<BundleEntry>> {
    let mut entries = task
        .source
        .bundle_file
        .entries
        .iter()
        .filter(|entry| entry.variable == task.variable)
        .cloned()
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.forecast_hour);
    if entries.is_empty() {
        bail!("legacy GFS variable has no entries: {}", task.variable);
    }
    if entries.iter().any(|entry| {
        entry.source_run != task.run
            || entry.forecast_hour < 0
            || entry.forecast_hour > task.horizon
            || entry.valid_time_utc != task.reference_time + Duration::hours(entry.forecast_hour)
    }) {
        bail!(
            "legacy GFS variable provenance/time mismatch: {}/{}/{}",
            task.run,
            task.domain.name,
            task.variable
        );
    }
    Ok(entries)
}

fn decode_source_block(
    task: &VariableTask,
    entry: &BundleEntry,
    decoder: &OfficialDecoder,
    local_y: u64,
    height: u64,
) -> Result<Vec<f32>> {
    if entry.array.dimensions.len() != 2
        || entry.array.chunks.len() != 2
        || entry.selection_ranges.len() != 2
    {
        bail!("legacy GFS entry is not a cropped 2D OM array");
    }
    let global_y = task.domain.grid.y0.context("native grid has no y0")? + local_y;
    let global_x = task.domain.grid.x0.context("native grid has no x0")?;
    let y_end = global_y + height;
    let x_end = global_x + task.domain.grid.nx;
    if global_y < entry.selection_ranges[0][0]
        || y_end > entry.selection_ranges[0][1]
        || global_x < entry.selection_ranges[1][0]
        || x_end > entry.selection_ranges[1][1]
    {
        bail!(
            "legacy GFS entry selection does not cover native regional grid: {}/{}/{}",
            task.run,
            task.domain.name,
            task.variable
        );
    }
    let metadata = build_v3_array_metadata_blob(
        entry.variable_path.as_deref().unwrap_or(&entry.variable),
        entry.array.data_type,
        entry.array.compression,
        &entry.array.dimensions,
        &entry.array.chunks,
        entry
            .array
            .lut_size
            .context("legacy OM entry has no LUT size")?,
        entry
            .array
            .lut_offset
            .context("legacy OM entry has no LUT offset")?,
        entry.array.scale_factor.unwrap_or(1.0),
        entry.array.add_offset.unwrap_or(0.0),
    );
    decoder.decode_grid(
        &metadata,
        &LegacyEntryRangeReader {
            bundle_handle: task.source.bundle_handle.clone(),
            entry: entry.clone(),
        },
        &[global_y, global_x],
        &[height, task.domain.grid.nx],
    )
}

fn interpolate_dense_block(
    task: &VariableTask,
    kind: InterpolationKind,
    data: &mut [f32],
    n_time: usize,
    local_y: u64,
) -> Result<()> {
    match kind {
        InterpolationKind::Direct => {
            bail!("unsupported direct GFS interpolation for {}", task.variable)
        }
        InterpolationKind::Linear { .. } => interpolate_linear(data, n_time),
        InterpolationKind::Backwards { .. } => interpolate_backwards(data, n_time, false),
        InterpolationKind::BackwardsSum { .. } => interpolate_backwards(data, n_time, true),
        InterpolationKind::Hermite { bounds, .. } => interpolate_hermite(data, n_time, bounds),
        InterpolationKind::SolarBackwardsAveraged { .. } => {
            let first_hour = task.horizon + 1 - i64::try_from(n_time)?;
            let start = task.reference_time + Duration::hours(first_hour);
            for (location, values) in data.chunks_exact_mut(n_time).enumerate() {
                let y = local_y + u64::try_from(location)? / task.domain.grid.nx;
                let x = u64::try_from(location)? % task.domain.grid.nx;
                interpolate_solar_backwards_in_place(
                    values,
                    start,
                    grid_latitude(&task.domain.grid, y),
                    grid_longitude(&task.domain.grid, x),
                );
            }
        }
    }
    Ok(())
}

#[allow(clippy::needless_range_loop)] // Preserve the pinned Swift port's index order exactly.
fn interpolate_backwards(data: &mut [f32], n_time: usize, summation: bool) {
    for values in data.chunks_exact_mut(n_time) {
        let mut first_valid = n_time;
        for time in 0..n_time {
            if values[time].is_nan() {
                continue;
            }
            if first_valid == n_time {
                first_valid = time;
                continue;
            }
            first_valid = first_valid.saturating_sub(time - first_valid - 1);
            break;
        }
        let mut previous_index = first_valid as isize - 1;
        for time in first_valid..n_time {
            if !values[time].is_nan() {
                previous_index = time as isize;
                continue;
            }
            for next in time..n_time {
                let value = values[next];
                if value.is_nan() {
                    continue;
                }
                let width = next as isize - previous_index;
                for target in time..=next {
                    values[target] = if summation {
                        value / width as f32
                    } else {
                        value
                    };
                }
                break;
            }
        }
    }
}

#[allow(clippy::needless_range_loop)] // Preserve the pinned Swift port's index order exactly.
fn interpolate_linear(data: &mut [f32], n_time: usize) {
    for values in data.chunks_exact_mut(n_time) {
        let mut previous_value = f32::NAN;
        let mut previous_index = 0_usize;
        for time in 0..n_time {
            if !values[time].is_nan() {
                previous_value = values[time];
                previous_index = time;
                continue;
            }
            for next in time..n_time {
                let value = values[next];
                if value.is_nan() {
                    continue;
                }
                for target in time..next {
                    let fraction =
                        (target - previous_index) as f32 / (next - previous_index) as f32;
                    values[target] = value * fraction + previous_value * (1.0 - fraction);
                }
                break;
            }
        }
    }
}

#[allow(clippy::needless_range_loop)] // Preserve the pinned Swift port's index order exactly.
fn interpolate_hermite(data: &mut [f32], n_time: usize, bounds: Option<(f32, f32)>) {
    for values in data.chunks_exact_mut(n_time) {
        let mut width = 0_usize;
        for time in 0..n_time {
            if !values[time].is_nan() {
                continue;
            }
            let mut c = f32::NAN;
            let mut d = f32::NAN;
            let mut position_c = 0_usize;
            let mut position_d = 0_usize;
            for next in time..n_time {
                let value = values[next];
                if value.is_nan() {
                    continue;
                }
                if c.is_nan() {
                    c = value;
                    position_c = next;
                    continue;
                }
                d = value;
                position_d = next;
                break;
            }
            if c.is_nan() {
                break;
            }
            if d.is_nan() {
                d = c;
                position_d = position_c;
            } else {
                width = position_d - position_c;
            }
            let position_b = position_c.saturating_sub(width);
            let position_a = position_b.checked_sub(width).unwrap_or(position_b);
            let b = values[position_b];
            let a = values[position_a];
            let coefficient_a = -a / 2.0 + (3.0 * b) / 2.0 - (3.0 * c) / 2.0 + d / 2.0;
            let coefficient_b = a - (5.0 * b) / 2.0 + 2.0 * c - d / 2.0;
            let coefficient_c = -a / 2.0 + c / 2.0;
            for target in time..position_c {
                let fraction = (target - position_b) as f32 / (position_c - position_b) as f32;
                let mut value = coefficient_a * fraction * fraction * fraction
                    + coefficient_b * fraction * fraction
                    + coefficient_c * fraction
                    + b;
                if let Some((lower, upper)) = bounds {
                    value = value.clamp(lower, upper);
                }
                values[target] = value;
            }
            let _ = position_d;
        }
    }
}

fn grid_latitude(grid: &NativeGridMetadata, y: u64) -> f64 {
    let global_y = grid.y0.unwrap_or(0) + y;
    let full_ny = grid.full_ny.unwrap_or(grid.ny);
    let dy = grid.dy as f32;
    if full_ny == 1536 {
        (-dy * (full_ny - 1) as f32 / 2.0 + global_y as f32 * dy) as f64
    } else {
        (-90.0_f32 + global_y as f32 * dy) as f64
    }
}

fn grid_longitude(grid: &NativeGridMetadata, x: u64) -> f64 {
    let global_x = grid.x0.unwrap_or(0) + x;
    let full_nx = grid.full_nx.unwrap_or(grid.nx);
    (-180.0_f32 + global_x as f32 * (360.0_f32 / full_nx as f32)) as f64
}

fn validate_written_samples(
    task: &VariableTask,
    decoder: &OfficialDecoder,
    samples: &BTreeMap<(u64, u64), Vec<f32>>,
    dimensions: &[u64],
    path: &Path,
) -> Result<()> {
    let file = Arc::new(File::open(path)?);
    let array = read_native_array_metadata(&file)?;
    let metadata = build_v3_array_metadata_blob(
        &task.variable,
        array.data_type,
        array.compression,
        &array.dimensions,
        &array.chunks,
        array.lut_size.context("written OM file has no LUT size")?,
        array
            .lut_offset
            .context("written OM file has no LUT offset")?,
        array
            .scale_factor
            .context("written OM file has no scale factor")?,
        array.add_offset.unwrap_or(0.0),
    );
    for ((y, x), expected) in samples {
        let decoded = decoder.decode_grid(
            &metadata,
            &FullFileRangeReader { file: file.clone() },
            &[*y, *x, 0],
            &[1, 1, dimensions[2]],
        )?;
        if decoded.len() != expected.len()
            || decoded.iter().zip(expected).any(|(actual, expected)| {
                !(actual.is_nan() && expected.is_nan()) && actual != expected
            })
        {
            bail!(
                "written OM sample differs after decode: {}/{}/{} y={} x={}",
                task.run,
                task.domain.name,
                task.variable,
                y,
                x
            );
        }
    }
    Ok(())
}

fn write_run_metadata(
    sources: &[LegacySourceRun],
    run_variables: &RunVariableInventory,
    staging: &Path,
) -> Result<()> {
    for domain in [gfs013_contract(), gfs025_contract()] {
        let mut latest_meta = None;
        for source in sources {
            let variables = run_variables
                .get(&(source.run.clone(), domain.name.to_string()))
                .with_context(|| {
                    format!(
                        "missing run variable inventory: {}/{}",
                        source.run, domain.name
                    )
                })?
                .clone();
            let valid_times = (0..=source.horizon)
                .map(|hour| {
                    (source.reference_time + Duration::hours(hour))
                        .format("%Y-%m-%dT%H:%MZ")
                        .to_string()
                })
                .collect();
            let meta = NativeRunMeta {
                created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                crs_wkt: crs_wkt(&domain.grid),
                reference_time: source.reference_time,
                temporal_resolution_seconds: 3600,
                valid_times,
                variables,
            };
            let path = staging
                .join("data_run")
                .join(domain.name)
                .join(run_relative_path(&source.run)?)
                .join("meta.json");
            atomic_write_json(&path, &meta)?;
            latest_meta = Some(meta);
        }
        atomic_write_json(
            &staging
                .join("data_run")
                .join(domain.name)
                .join("latest.json"),
            &latest_meta.context("native GFS source list is empty")?,
        )?;
    }
    Ok(())
}

fn crs_wkt(grid: &NativeGridMetadata) -> String {
    let bottom = grid.lat_min;
    let left = grid.lon_min;
    let top = grid.lat_min + grid.dy * (grid.ny - 1) as f64;
    let right = grid.lon_min + grid.dx * (grid.nx - 1) as f64;
    format!(
        "GEOGCRS[\"WGS 84\",\n    DATUM[\"World Geodetic System 1984\",\n        ELLIPSOID[\"WGS 84\",6378137,298.257223563]],\n    CS[ellipsoidal,2],\n        AXIS[\"latitude\",north],\n        AXIS[\"longitude\",east],\n        ANGLEUNIT[\"degree\",0.0174532925199433]\n    USAGE[\n        SCOPE[\"grid\"],\n        BBOX[{bottom},{left},{top},{right}]]]"
    )
}

fn materialize_static_elevation(
    decoder: &OfficialDecoder,
    source: &Path,
    destination: &Path,
    contract: &DomainContract,
) -> Result<()> {
    if destination.exists() {
        let metadata = read_native_array_metadata(&File::open(destination)?)?;
        if metadata.dimensions == [contract.grid.ny, contract.grid.nx]
            && metadata.data_type == DATA_TYPE_FLOAT_ARRAY
        {
            return Ok(());
        }
        fs::remove_file(destination)?;
    }
    let source_file = Arc::new(
        File::open(source)
            .with_context(|| format!("open global static elevation {}", source.display()))?,
    );
    let source_metadata = read_native_array_metadata(&source_file)?;
    if source_metadata.dimensions
        != [
            contract
                .grid
                .full_ny
                .context("static grid has no full_ny")?,
            contract
                .grid
                .full_nx
                .context("static grid has no full_nx")?,
        ]
        || source_metadata.data_type != DATA_TYPE_FLOAT_ARRAY
    {
        bail!(
            "global static elevation dimensions/type do not match {}",
            contract.name
        );
    }
    let metadata = build_v3_array_metadata_blob(
        "HSURF",
        source_metadata.data_type,
        source_metadata.compression,
        &source_metadata.dimensions,
        &source_metadata.chunks,
        source_metadata
            .lut_size
            .context("static elevation has no LUT size")?,
        source_metadata
            .lut_offset
            .context("static elevation has no LUT offset")?,
        source_metadata.scale_factor.unwrap_or(1.0),
        source_metadata.add_offset.unwrap_or(0.0),
    );
    let values = decoder.decode_grid(
        &metadata,
        &FullFileRangeReader { file: source_file },
        &[
            contract.grid.y0.context("static grid has no y0")?,
            contract.grid.x0.context("static grid has no x0")?,
        ],
        &[contract.grid.ny, contract.grid.nx],
    )?;
    let chunks = vec![
        source_metadata.chunks[0].min(contract.grid.ny),
        source_metadata.chunks[1].min(contract.grid.nx),
    ];
    let scale = source_metadata.scale_factor.unwrap_or(1.0);
    let mut writer = decoder.create_array_writer(
        destination,
        vec![contract.grid.ny, contract.grid.nx],
        chunks,
        scale,
        source_metadata.add_offset.unwrap_or(0.0),
        source_metadata.data_type,
        source_metadata.compression,
    )?;
    writer.write_f32_block(&values, &[contract.grid.ny, contract.grid.nx])?;
    writer.finish("HSURF")?;
    Ok(())
}

fn link_dem_chunks(dem_root: &Path, staging: &Path) -> Result<()> {
    let source_root = dem_root.join("copernicus_dem90/static");
    let destination_root = staging.join("copernicus_dem90/static");
    fs::create_dir_all(&destination_root)?;
    for latitude in 0..=58 {
        let name = format!("lat_{latitude}.om");
        let source = source_root.join(&name);
        let destination = destination_root.join(&name);
        if !source.is_file() || source.metadata()?.len() == 0 {
            bail!(
                "required Copernicus DEM90 chunk is missing: {}",
                source.display()
            );
        }
        if destination.exists() {
            if destination.metadata()?.len() == source.metadata()?.len() {
                continue;
            }
            fs::remove_file(&destination)?;
        }
        if let Err(link_error) = fs::hard_link(&source, &destination) {
            fs::copy(&source, &destination).with_context(|| {
                format!(
                    "copy DEM chunk after hard-link failure ({link_error}): {}",
                    source.display()
                )
            })?;
        }
    }
    Ok(())
}

fn build_coverage_marker(
    options: &GfsBuildOptions,
    sources: &[LegacySourceRun],
    legacy_marker: Value,
    staging: &Path,
) -> Result<Value> {
    let mut marker = legacy_marker.as_object().cloned().unwrap_or_default();
    let release_id = marker
        .get("release_id")
        .and_then(Value::as_str)
        .context("latest retained GFS source has no canonical release_id")?
        .to_string();
    validate_group_release_id(&release_id)?;
    let source_runs = sources
        .iter()
        .map(|source| source.run.clone())
        .collect::<Vec<_>>();
    let latest = sources.last().context("native source list is empty")?;
    let oldest = sources.first().context("native source list is empty")?;
    let public_end = latest.reference_time + Duration::hours(latest.horizon);
    let (files, bytes) = coverage_stats(staging)?;
    let grid013 = grid_marker(&gfs013_contract());
    let grid025 = grid_marker(&gfs025_contract());
    let mut set = |name: &str, value: Value| {
        marker.insert(name.to_string(), value);
    };
    set("version", json!(1));
    set("group", json!("gfs"));
    set("status", json!("complete"));
    set("runtime_format", json!("openmeteo-native-v1"));
    set("coverage_id", json!(options.coverage_id));
    set("release_id", json!(release_id));
    set(
        "coverage_path",
        json!(format!("coverages/gfs/{}", options.coverage_id)),
    );
    set("coverage_reused", json!(false));
    set("latest_complete_run", json!(options.latest_run));
    set("source_runs", json!(source_runs));
    set("source_run_max_forecast_hours", json!([5, 5, 5, 384, 384]));
    set("short_run_count", json!(3));
    set("full_run_count", json!(2));
    set("historical_max_forecast_hour", json!(5));
    set("latest_max_forecast_hour", json!(384));
    set(
        "public_start_utc",
        json!(oldest
            .reference_time
            .to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    set(
        "local_day_start_utc",
        json!(shanghai_day_start_utc(latest.reference_time)?
            .to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    set(
        "public_end_utc",
        json!(public_end.to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    set(
        "public_hours",
        json!((public_end - oldest.reference_time).num_hours()),
    );
    set(
        "generated_at",
        json!(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    set("producer_revision", json!(options.producer_revision));
    set(
        "materialization_revision",
        json!(GFS_MATERIALIZATION_REVISION),
    );
    set("files", json!(files));
    set("bytes", json!(bytes));
    set("domains", json!(["ncep_gfs013", "ncep_gfs025"]));
    set(
        "domain_grids",
        json!({"ncep_gfs013": grid013.clone(), "ncep_gfs025": grid025.clone()}),
    );
    set(
        "products",
        json!({
            "gfs013_surface": {
                "coverage_id": options.coverage_id,
                "runtime_domain": "ncep_gfs013",
                "grid": grid013
            },
            "gfs025": {
                "coverage_id": options.coverage_id,
                "runtime_domain": "ncep_gfs025",
                "grid": grid025.clone()
            },
            "gfs_pressure_profile": {
                "coverage_id": options.coverage_id,
                "runtime_domain": "ncep_gfs025",
                "grid": grid025
            }
        }),
    );
    set(
        "static_sources",
        json!({
            "copernicus_dem90": {
                "source": "copernicus_dem90",
                "runtime_path": "copernicus_dem90/static",
                "latitude_chunk_min": 0,
                "latitude_chunk_max": 58,
                "file_count": 59
            }
        }),
    );
    Ok(Value::Object(marker))
}

fn variable_omits_hour_zero(domain: &str, variable: &str) -> bool {
    match domain {
        "ncep_gfs013" => matches!(
            variable,
            "categorical_freezing_rain"
                | "cloud_cover"
                | "cloud_cover_low"
                | "cloud_cover_mid"
                | "cloud_cover_high"
                | "precipitation"
                | "showers"
                | "snowfall_water_equivalent"
                | "sensible_heat_flux"
                | "latent_heat_flux"
                | "shortwave_radiation"
                | "diffuse_radiation"
                | "uv_index"
                | "uv_index_clear_sky"
        ),
        "ncep_gfs025" => matches!(
            variable,
            "categorical_freezing_rain"
                | "precipitation"
                | "showers"
                | "sensible_heat_flux"
                | "latent_heat_flux"
                | "shortwave_radiation"
                | "diffuse_radiation"
                | "uv_index"
                | "uv_index_clear_sky"
        ),
        _ => false,
    }
}

fn expected_sparse_forecast_hours(horizon: i64, omit_hour_zero: bool) -> Vec<i64> {
    let first = i64::from(omit_hour_zero);
    (first..=horizon.min(120))
        .chain((123..=horizon).step_by(3))
        .collect()
}

fn required_variables(domain: &str) -> Result<BTreeSet<String>> {
    match domain {
        "ncep_gfs013" => Ok(GFS013_REQUIRED_VARIABLES
            .iter()
            .map(|value| (*value).to_string())
            .collect()),
        "ncep_gfs025" => {
            let mut required = GFS025_REQUIRED_SURFACE_VARIABLES
                .iter()
                .map(|value| (*value).to_string())
                .collect::<BTreeSet<_>>();
            for family in PRESSURE_VARIABLE_FAMILIES {
                for level in PRESSURE_LEVELS {
                    required.insert(format!("{family}_{level}hPa"));
                }
            }
            Ok(required)
        }
        _ => bail!("unsupported native GFS domain: {domain}"),
    }
}

fn validate_grid_contract(
    actual: &NativeGridMetadata,
    expected: &NativeGridMetadata,
    label: &str,
) -> Result<()> {
    if actual.nx != expected.nx
        || actual.ny != expected.ny
        || actual.lon_min != expected.lon_min
        || actual.lat_min != expected.lat_min
        || actual.dx != expected.dx
        || actual.dy != expected.dy
        || actual.dt_seconds != expected.dt_seconds
        || actual.om_file_length != expected.om_file_length
        || actual.full_nx != expected.full_nx
        || actual.full_ny != expected.full_ny
        || actual.x0 != expected.x0
        || actual.y0 != expected.y0
    {
        bail!("native GFS grid contract mismatch for {label}");
    }
    Ok(())
}

fn parse_valid_time(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%MZ").map(|parsed| parsed.and_utc())
        })
        .with_context(|| format!("invalid native valid time: {value}"))
}

fn validate_native_series(
    decoder: &OfficialDecoder,
    file_path: &Path,
    variable: &str,
    array: &crate::manifest::ArrayMetadata,
) -> Result<u64> {
    let file = Arc::new(File::open(file_path)?);
    let metadata = build_v3_array_metadata_blob(
        variable,
        array.data_type,
        array.compression,
        &array.dimensions,
        &array.chunks,
        array
            .lut_size
            .context("native OM variable has no LUT size")?,
        array
            .lut_offset
            .context("native OM variable has no LUT offset")?,
        array
            .scale_factor
            .context("native OM variable has no scale factor")?,
        array.add_offset.unwrap_or(0.0),
    );
    let ny = array.dimensions[0];
    let nx = array.dimensions[1];
    let n_time = array.dimensions[2];
    let points = [(0, 0), (ny / 2, nx / 2), (ny - 1, nx - 1)];
    let mut decoded_values = 0_u64;
    for (y, x) in points {
        let values = decoder.decode_grid(
            &metadata,
            &FullFileRangeReader { file: file.clone() },
            &[y, x, 0],
            &[1, 1, n_time],
        )?;
        if values.len() != usize::try_from(n_time)?
            || values.iter().any(|value| value.is_infinite())
        {
            bail!(
                "native OM decode probe failed for {} at y={} x={}",
                file_path.display(),
                y,
                x
            );
        }
        decoded_values += n_time;
    }
    Ok(decoded_values)
}

fn validate_static_array(
    decoder: &OfficialDecoder,
    file_path: &Path,
    variable: &str,
    expected_dimensions: Option<&[u64]>,
) -> Result<u64> {
    let file_metadata = fs::symlink_metadata(file_path)
        .with_context(|| format!("read native static file metadata: {}", file_path.display()))?;
    if !file_metadata.is_file()
        || file_metadata.file_type().is_symlink()
        || file_metadata.len() == 0
    {
        bail!(
            "native static file is missing or invalid: {}",
            file_path.display()
        );
    }
    let file = Arc::new(File::open(file_path)?);
    let array = read_native_array_metadata(&file)?;
    if array.dimensions.is_empty()
        || array.dimensions.len() != array.chunks.len()
        || array.dimensions.contains(&0)
        || array
            .chunks
            .iter()
            .zip(&array.dimensions)
            .any(|(chunk, dimension)| *chunk == 0 || chunk > dimension)
        || array.data_type != DATA_TYPE_FLOAT_ARRAY
    {
        bail!(
            "native static OM metadata is invalid: {}",
            file_path.display()
        );
    }
    if expected_dimensions.is_some_and(|expected| array.dimensions != expected) {
        bail!(
            "native static OM dimensions are invalid: {}",
            file_path.display()
        );
    }
    let metadata = build_v3_array_metadata_blob(
        variable,
        array.data_type,
        array.compression,
        &array.dimensions,
        &array.chunks,
        array
            .lut_size
            .context("native static OM file has no LUT size")?,
        array
            .lut_offset
            .context("native static OM file has no LUT offset")?,
        array
            .scale_factor
            .context("native static OM file has no scale factor")?,
        array.add_offset.unwrap_or(0.0),
    );
    let offset = vec![0_u64; array.dimensions.len()];
    let count = vec![1_u64; array.dimensions.len()];
    let decoded = decoder.decode_grid(&metadata, &FullFileRangeReader { file }, &offset, &count)?;
    if decoded.len() != 1 || decoded[0].is_infinite() {
        bail!(
            "native static OM decode probe failed: {}",
            file_path.display()
        );
    }
    Ok(1)
}

fn validate_build_identity(coverage_root: &Path, marker: &GfsCoverageMarker) -> Result<Value> {
    let path = coverage_root.join("build_identity.json");
    let identity: Value = serde_json::from_slice(
        &fs::read(&path)
            .with_context(|| format!("read native build identity: {}", path.display()))?,
    )?;
    let object = identity
        .as_object()
        .context("native build identity is not an object")?;
    if object.get("version").and_then(Value::as_u64) != Some(1)
        || object.get("algorithm").and_then(Value::as_str) != Some(GFS_MATERIALIZATION_REVISION)
        || object.get("coverage_id").and_then(Value::as_str) != Some(marker.coverage_id.as_str())
        || object.get("latest_run").and_then(Value::as_str)
            != Some(marker.latest_complete_run.as_str())
        || object.get("producer_revision").and_then(Value::as_str)
            != Some(marker.producer_revision.as_str())
    {
        bail!("native build identity does not match its coverage marker");
    }
    let source_coverages = object
        .get("source_coverages")
        .and_then(Value::as_object)
        .context("native build identity has no source_coverages object")?;
    if source_coverages.keys().cloned().collect::<BTreeSet<_>>()
        != marker.source_runs.iter().cloned().collect::<BTreeSet<_>>()
    {
        bail!("native build identity source runs differ from its coverage marker");
    }
    for (run, coverages) in source_coverages {
        let coverages = coverages
            .as_object()
            .with_context(|| format!("native source coverage map is invalid for {run}"))?;
        if coverages
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            != GFS_PRODUCTS.into_iter().collect::<BTreeSet<_>>()
        {
            bail!("native source coverage inventory is invalid for {run}");
        }
        for coverage_id in coverages.values() {
            validate_component(
                "source coverage_id",
                coverage_id
                    .as_str()
                    .with_context(|| format!("native source coverage_id is invalid for {run}"))?,
            )?;
        }
    }
    Ok(identity)
}

pub fn validate_gfs_coverage(
    coverage_root: &Path,
    decoder: &OfficialDecoder,
) -> Result<GfsValidationResult> {
    let root_metadata = fs::symlink_metadata(coverage_root)
        .with_context(|| format!("read native coverage metadata: {}", coverage_root.display()))?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        bail!(
            "native coverage root must be a real directory: {}",
            coverage_root.display()
        );
    }
    let marker_path = coverage_root.join("coverage.json");
    let marker_value: Value = serde_json::from_slice(
        &fs::read(&marker_path)
            .with_context(|| format!("read native coverage marker: {}", marker_path.display()))?,
    )?;
    let marker: GfsCoverageMarker = serde_json::from_value(marker_value.clone())?;
    validate_component("coverage_id", &marker.coverage_id)?;
    if marker.status != "complete"
        || marker.runtime_format != "openmeteo-native-v1"
        || marker.group != "gfs"
        || marker.coverage_path != format!("coverages/gfs/{}", marker.coverage_id)
        || marker.materialization_revision != GFS_MATERIALIZATION_REVISION
    {
        bail!("native GFS coverage identity/status contract is invalid");
    }
    validate_group_release_id(&marker.release_id)?;
    validate_producer_revision(&marker.producer_revision)
        .context("native GFS coverage has no valid producer revision")?;
    let expected_runs = expected_source_runs(&marker.latest_complete_run)?;
    let expected_run_names = expected_runs
        .iter()
        .map(|(run, _, _)| run.clone())
        .collect::<Vec<_>>();
    let expected_horizons = expected_runs
        .iter()
        .map(|(_, _, horizon)| *horizon)
        .collect::<Vec<_>>();
    if marker.source_runs != expected_run_names
        || marker.source_run_max_forecast_hours != expected_horizons
        || marker.short_run_count != 3
        || marker.full_run_count != 2
        || marker.historical_max_forecast_hour != 5
        || marker.latest_max_forecast_hour != 384
    {
        bail!("native GFS coverage does not contain three short and two complete runs");
    }
    validate_build_identity(coverage_root, &marker)?;
    let oldest = expected_runs
        .first()
        .context("native GFS run list is empty")?
        .1;
    let latest = expected_runs
        .last()
        .context("native GFS run list is empty")?
        .1;
    let public_end = latest + Duration::hours(384);
    if marker.public_start_utc != oldest
        || marker.local_day_start_utc != shanghai_day_start_utc(latest)?
        || marker.public_end_utc != public_end
        || marker.public_hours != (public_end - oldest).num_hours()
    {
        bail!("native GFS public time-window contract is invalid");
    }

    let expected_domains = BTreeSet::from(["ncep_gfs013".to_string(), "ncep_gfs025".to_string()]);
    if marker.domains.iter().cloned().collect::<BTreeSet<_>>() != expected_domains
        || marker.domains.len() != expected_domains.len()
    {
        bail!("native GFS domain inventory is invalid");
    }
    let expected_products = BTreeMap::from([
        ("gfs013_surface", gfs013_contract()),
        ("gfs025", gfs025_contract()),
        ("gfs_pressure_profile", gfs025_contract()),
    ]);
    if marker
        .products
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != expected_products.keys().copied().collect::<BTreeSet<_>>()
    {
        bail!("native GFS product inventory is invalid");
    }
    for (product_name, contract) in expected_products {
        let product = marker
            .products
            .get(product_name)
            .with_context(|| format!("native GFS marker has no {product_name}"))?;
        if product.coverage_id != marker.coverage_id || product.runtime_domain != contract.name {
            bail!("native GFS product identity is invalid for {product_name}");
        }
        validate_grid_contract(&product.grid, &contract.grid, product_name)?;
    }
    for contract in [gfs013_contract(), gfs025_contract()] {
        if marker.domain_grids.get(contract.name) != Some(&grid_marker(&contract)) {
            bail!(
                "native GFS domain grid marker is invalid for {}",
                contract.name
            );
        }
    }
    let dem = marker
        .static_sources
        .get("copernicus_dem90")
        .context("native GFS marker has no Copernicus DEM90 contract")?;
    if marker.static_sources.len() != 1
        || dem.source != "copernicus_dem90"
        || dem.runtime_path != "copernicus_dem90/static"
        || dem.latitude_chunk_min != 0
        || dem.latitude_chunk_max != 58
        || dem.file_count != 59
    {
        bail!("native GFS Copernicus DEM90 contract is invalid");
    }

    let mut om_files = 0_u64;
    let mut decoded_probes = 0_u64;
    for (run, reference_time, horizon) in &expected_runs {
        for contract in [gfs013_contract(), gfs025_contract()] {
            let run_directory = coverage_root
                .join("data_run")
                .join(contract.name)
                .join(run_relative_path(run)?);
            let meta_path = run_directory.join("meta.json");
            let meta: NativeRunMeta =
                serde_json::from_slice(&fs::read(&meta_path).with_context(|| {
                    format!("read native run metadata: {}", meta_path.display())
                })?)?;
            DateTime::parse_from_rfc3339(&meta.created_at)
                .with_context(|| format!("invalid created_at in {}", meta_path.display()))?;
            if meta.reference_time != *reference_time
                || meta.temporal_resolution_seconds != 3600
                || meta.crs_wkt != crs_wkt(&contract.grid)
                || meta.valid_times.len() != usize::try_from(*horizon + 1)?
                || meta.variables.is_empty()
                || meta.variables.windows(2).any(|pair| pair[0] >= pair[1])
            {
                bail!(
                    "native run metadata contract is invalid: {}",
                    meta_path.display()
                );
            }
            for (index, valid_time) in meta.valid_times.iter().enumerate() {
                if parse_valid_time(valid_time)?
                    != *reference_time + Duration::hours(i64::try_from(index)?)
                {
                    bail!(
                        "native run time axis is not continuous: {}",
                        meta_path.display()
                    );
                }
            }
            let variable_set = meta.variables.iter().cloned().collect::<BTreeSet<_>>();
            let missing = required_variables(contract.name)?
                .difference(&variable_set)
                .take(20)
                .cloned()
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                bail!(
                    "native run {run}/{} is missing required variables: {}",
                    contract.name,
                    missing.join(",")
                );
            }
            let mut disk_variables = BTreeSet::new();
            for entry in fs::read_dir(&run_directory)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("om") {
                    continue;
                }
                let file_metadata = fs::symlink_metadata(&path)?;
                if !file_metadata.is_file()
                    || file_metadata.file_type().is_symlink()
                    || file_metadata.len() == 0
                {
                    bail!("native variable file is invalid: {}", path.display());
                }
                let variable = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .context("native variable filename is not UTF-8")?
                    .to_string();
                disk_variables.insert(variable);
            }
            if disk_variables != variable_set {
                bail!(
                    "native run variable inventory differs from meta.json: {}",
                    run_directory.display()
                );
            }
            for variable in &meta.variables {
                validate_component("variable", variable)?;
                let file_path = run_directory.join(format!("{variable}.om"));
                let array = read_native_array_metadata(&File::open(&file_path)?)?;
                let expected_time_count = meta.valid_times.len()
                    - usize::from(variable_omits_hour_zero(contract.name, variable));
                let expected_time_count_u64 = u64::try_from(expected_time_count)?;
                let expected_chunks = vec![
                    1,
                    (1024 / expected_time_count_u64)
                        .max(1)
                        .min(contract.grid.nx),
                    expected_time_count_u64,
                ];
                let expected_scale =
                    interpolation_scale(variable, interpolation_kind_for_variable(variable))?;
                if array.dimensions != [contract.grid.ny, contract.grid.nx, expected_time_count_u64]
                    || array.chunks != expected_chunks
                    || array.data_type != DATA_TYPE_FLOAT_ARRAY
                    || array.compression != COMPRESSION_PFOR_DELTA2D_INT16
                    || array.scale_factor != Some(expected_scale)
                    || array.add_offset != Some(0.0)
                {
                    bail!(
                        "native OM array contract is invalid: {}",
                        file_path.display()
                    );
                }
                decoded_probes += validate_native_series(decoder, &file_path, variable, &array)?;
                om_files += 1;
            }
        }
    }

    for contract in [gfs013_contract(), gfs025_contract()] {
        let latest_path = coverage_root
            .join("data_run")
            .join(contract.name)
            .join("latest.json");
        let latest_meta: NativeRunMeta =
            serde_json::from_slice(&fs::read(&latest_path).with_context(|| {
                format!("read native latest metadata: {}", latest_path.display())
            })?)?;
        if latest_meta.reference_time != latest
            || latest_meta.valid_times.len() != 385
            || latest_meta.variables.is_empty()
        {
            bail!(
                "native latest metadata is invalid: {}",
                latest_path.display()
            );
        }
        let static_path = coverage_root.join(contract.name).join("static/HSURF.om");
        decoded_probes += validate_static_array(
            decoder,
            &static_path,
            "HSURF",
            Some(&[contract.grid.ny, contract.grid.nx]),
        )?;
        om_files += 1;
    }
    for latitude in 0..=58 {
        let path = coverage_root
            .join("copernicus_dem90/static")
            .join(format!("lat_{latitude}.om"));
        decoded_probes += validate_dem_om_file(decoder, &path)?;
        om_files += 1;
    }

    let (files, bytes) = coverage_stats(coverage_root)?;
    if marker.files != files || marker.bytes != bytes {
        bail!(
            "native GFS coverage stats differ from marker: files={files}/{} bytes={bytes}/{}",
            marker.files,
            marker.bytes
        );
    }
    Ok(GfsValidationResult {
        coverage_id: marker.coverage_id,
        coverage_path: coverage_root.to_path_buf(),
        source_runs: marker.source_runs,
        om_files,
        decoded_probes,
        bytes,
    })
}

pub fn publish_gfs_coverage(
    data_root: &Path,
    coverage_id: &str,
    decoder: &OfficialDecoder,
) -> Result<GfsPublishResult> {
    validate_component("coverage_id", coverage_id)?;
    if !data_root.is_dir() {
        bail!("OM data root does not exist: {}", data_root.display());
    }
    let coverage_parent = data_root.join("coverages/gfs");
    fs::create_dir_all(&coverage_parent)?;
    require_real_directory(&coverage_parent, "native coverage parent")?;
    let staging = coverage_parent.join(format!(".incoming_{coverage_id}"));
    let target = coverage_parent.join(coverage_id);
    let target_exists = path_is_real_directory(&target, "immutable native coverage")?;
    let staging_exists = path_is_real_directory(&staging, "native staging")?;
    let reused = if target_exists {
        if staging_exists {
            bail!(
                "both immutable target and staging coverage exist; refusing to discard either: {} {}",
                target.display(),
                staging.display()
            );
        }
        validate_gfs_coverage(&target, decoder)?;
        true
    } else {
        if !staging_exists {
            bail!(
                "native GFS staging coverage is missing: {}",
                staging.display()
            );
        }
        validate_gfs_coverage(&staging, decoder)?;
        fs::rename(&staging, &target).with_context(|| {
            format!(
                "promote native GFS staging coverage {} to {}",
                staging.display(),
                target.display()
            )
        })?;
        sync_directory(&coverage_parent)?;
        false
    };
    let validation = validate_gfs_coverage(&target, decoder)?;
    if validation.coverage_id != coverage_id {
        bail!("published native GFS coverage identity does not match its directory");
    }
    let marker_path = target.join("coverage.json");
    let mut ready: Value = serde_json::from_slice(&fs::read(&marker_path)?)?;
    ready
        .as_object_mut()
        .context("native GFS coverage marker is not an object")?
        .insert("coverage_reused".to_string(), json!(reused));
    let marker: GfsCoverageMarker = serde_json::from_value(ready.clone())?;
    let expected_target = safe_coverage_path(data_root, &marker.coverage_path)?;
    if expected_target != target {
        bail!("native GFS marker coverage_path does not resolve to its immutable target");
    }

    let source_release_path = data_root
        .join("groups/gfs/releases")
        .join(format!("{}.json", marker.release_id));
    let source_release: LegacyGroupRelease =
        serde_json::from_slice(&fs::read(&source_release_path).with_context(|| {
            format!(
                "retained source release disappeared before native publication: {}",
                source_release_path.display()
            )
        })?)?;
    if source_release.group != "gfs"
        || source_release.status != "complete"
        || source_release.release_id != marker.release_id
        || source_release.latest_complete_run != marker.latest_complete_run
    {
        bail!("retained source release identity changed before native publication");
    }

    let current_marker = data_root.join("groups/gfs/current/ready_for_processing.json");
    let previous_current_id = current_native_coverage_id(&current_marker)?;
    if current_marker.is_file() {
        let current: Value = serde_json::from_slice(&fs::read(&current_marker)?)?;
        if let Some(current_run) = current.get("latest_complete_run").and_then(Value::as_str) {
            if parse_run(current_run)? > parse_run(&marker.latest_complete_run)? {
                bail!(
                    "refusing to roll native GFS current backwards from {current_run} to {}",
                    marker.latest_complete_run
                );
            }
        }
    }
    let current_path = data_root.join("current/gfs");
    atomic_symlink(
        &PathBuf::from("../coverages/gfs").join(coverage_id),
        &current_path,
    )?;
    // Publication is marker-last: API reload cannot observe an unvalidated or
    // partially promoted coverage.
    atomic_write_json(&current_marker, &ready)?;
    let cleanup = prune_completed_gfs_coverages(
        data_root,
        coverage_id,
        previous_current_id.as_deref(),
        decoder,
    )?;
    Ok(GfsPublishResult {
        coverage_id: coverage_id.to_string(),
        coverage_path: target,
        marker_path: current_marker,
        current_path,
        cleanup,
    })
}

#[derive(Debug)]
struct ManagedCoverage {
    coverage_id: String,
    latest_run: DateTime<Utc>,
    modified: SystemTime,
    path: PathBuf,
}

fn current_native_coverage_id(marker_path: &Path) -> Result<Option<String>> {
    if !marker_path.is_file() {
        return Ok(None);
    }
    let marker: Value = serde_json::from_slice(&fs::read(marker_path)?)?;
    if marker.get("runtime_format").and_then(Value::as_str) != Some("openmeteo-native-v1") {
        return Ok(None);
    }
    let Some(coverage_id) = marker.get("coverage_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    validate_component("current native coverage_id", coverage_id)?;
    Ok(Some(coverage_id.to_string()))
}

fn managed_coverage_from_directory(path: &Path) -> Result<Option<ManagedCoverage>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(None);
    }
    let directory_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("native coverage directory name is not UTF-8")?;
    let marker_path = path.join("coverage.json");
    if !marker_path.is_file() {
        return Ok(None);
    }
    let marker: GfsCoverageMarker = match serde_json::from_slice(&fs::read(&marker_path)?) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if marker.group != "gfs"
        || marker.status != "complete"
        || marker.runtime_format != "openmeteo-native-v1"
        || marker.materialization_revision != GFS_MATERIALIZATION_REVISION
        || marker.coverage_id != directory_name
        || marker.coverage_path != format!("coverages/gfs/{directory_name}")
    {
        return Ok(None);
    }
    let expected_id =
        match default_gfs_coverage_id(&marker.latest_complete_run, &marker.producer_revision) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
    if expected_id != directory_name {
        // Custom IDs and manually renamed backups are intentionally outside
        // automatic lifecycle management.
        return Ok(None);
    }
    Ok(Some(ManagedCoverage {
        coverage_id: marker.coverage_id,
        latest_run: parse_run(&marker.latest_complete_run)?,
        modified: marker_path.metadata()?.modified()?,
        path: path.to_path_buf(),
    }))
}

fn collect_managed_coverages(coverage_parent: &Path) -> Result<Vec<ManagedCoverage>> {
    if !coverage_parent.is_dir() {
        return Ok(Vec::new());
    }
    let mut output = Vec::new();
    for entry in fs::read_dir(coverage_parent)? {
        let entry = entry?;
        match managed_coverage_from_directory(&entry.path()) {
            Ok(Some(candidate)) => output.push(candidate),
            Ok(None) => {}
            Err(error) => tracing::warn!(
                path = %entry.path().display(),
                error = %error,
                "skipping unreadable or invalid native lifecycle directory"
            ),
        }
    }
    output.sort_by(|left, right| {
        right
            .latest_run
            .cmp(&left.latest_run)
            .then_with(|| right.modified.cmp(&left.modified))
            .then_with(|| right.coverage_id.cmp(&left.coverage_id))
    });
    Ok(output)
}

fn prune_completed_gfs_coverages(
    data_root: &Path,
    current_coverage_id: &str,
    previous_current_id: Option<&str>,
    decoder: &OfficialDecoder,
) -> Result<GfsCleanupResult> {
    let coverage_parent = data_root.join("coverages/gfs");
    let candidates = collect_managed_coverages(&coverage_parent)?;
    if !candidates
        .iter()
        .any(|candidate| candidate.coverage_id == current_coverage_id)
    {
        // A custom/manual current coverage disables automatic deletion.
        return Ok(GfsCleanupResult::default());
    }

    let keep = select_retained_coverage_ids(&candidates, current_coverage_id, previous_current_id);

    let mut result = GfsCleanupResult {
        retained_coverages: keep.iter().cloned().collect(),
        ..GfsCleanupResult::default()
    };
    for candidate in candidates {
        if keep.contains(&candidate.coverage_id) {
            continue;
        }
        let validation = validate_gfs_coverage(&candidate.path, decoder).with_context(|| {
            format!(
                "refusing to remove unvalidated native coverage {}",
                candidate.path.display()
            )
        })?;
        if validation.coverage_id != candidate.coverage_id {
            bail!("native cleanup candidate identity changed during validation");
        }
        let bytes = tree_regular_file_bytes(&candidate.path)?;
        fs::remove_dir_all(&candidate.path).with_context(|| {
            format!(
                "remove expired managed native coverage {}",
                candidate.path.display()
            )
        })?;
        result.removed_bytes = result
            .removed_bytes
            .checked_add(bytes)
            .context("native cleanup byte count overflow")?;
        result.removed_coverages.push(candidate.coverage_id);
    }
    if !result.removed_coverages.is_empty() {
        sync_directory(&coverage_parent)?;
    }
    result.retained_coverages.sort();
    result.removed_coverages.sort();
    Ok(result)
}

fn select_retained_coverage_ids(
    candidates: &[ManagedCoverage],
    current_coverage_id: &str,
    previous_current_id: Option<&str>,
) -> BTreeSet<String> {
    let mut keep = BTreeSet::from([current_coverage_id.to_string()]);
    if let Some(previous) = previous_current_id.filter(|value| *value != current_coverage_id) {
        if candidates
            .iter()
            .any(|candidate| candidate.coverage_id == previous)
        {
            keep.insert(previous.to_string());
        }
    }
    if keep.len() < 2 {
        if let Some(rollback) = candidates
            .iter()
            .find(|candidate| !keep.contains(&candidate.coverage_id))
        {
            keep.insert(rollback.coverage_id.clone());
        }
    }
    keep
}

pub fn cleanup_stale_gfs_staging(data_root: &Path) -> Result<GfsCleanupResult> {
    cleanup_stale_gfs_staging_older_than(data_root, DEFAULT_STALE_STAGING_AGE)
}

fn cleanup_stale_gfs_staging_older_than(
    data_root: &Path,
    minimum_age: StdDuration,
) -> Result<GfsCleanupResult> {
    let coverage_parent = data_root.join("coverages/gfs");
    if !coverage_parent.is_dir() {
        return Ok(GfsCleanupResult::default());
    }
    let current_id = current_native_coverage_id(
        &data_root.join("groups/gfs/current/ready_for_processing.json"),
    )?;
    let now = SystemTime::now();
    let mut result = GfsCleanupResult::default();
    for entry in fs::read_dir(&coverage_parent)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Some(directory_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(coverage_id) = directory_name.strip_prefix(".incoming_") else {
            continue;
        };
        let identity_path = path.join("build_identity.json");
        if !identity_path.is_file() {
            continue;
        }
        let identity_bytes = match fs::read(&identity_path) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    path = %identity_path.display(),
                    error = %error,
                    "skipping unreadable native staging identity"
                );
                continue;
            }
        };
        let identity: Value = match serde_json::from_slice(&identity_bytes) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let managed = identity.get("lifecycle_manager").and_then(Value::as_str)
            == Some(NATIVE_LIFECYCLE_MANAGER);
        let algorithm =
            identity.get("algorithm").and_then(Value::as_str) == Some(GFS_MATERIALIZATION_REVISION);
        let identity_id = identity.get("coverage_id").and_then(Value::as_str);
        let latest_run = identity.get("latest_run").and_then(Value::as_str);
        let revision = identity.get("producer_revision").and_then(Value::as_str);
        if !managed || !algorithm || identity_id != Some(coverage_id) {
            continue;
        }
        let (Some(latest_run), Some(revision)) = (latest_run, revision) else {
            continue;
        };
        if default_gfs_coverage_id(latest_run, revision)
            .ok()
            .as_deref()
            != Some(coverage_id)
            || current_id.as_deref() == Some(coverage_id)
        {
            continue;
        }
        let (bytes, latest_modified) = match tree_bytes_and_latest_modified(&path) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "skipping unsafe native staging tree"
                );
                continue;
            }
        };
        if now.duration_since(latest_modified).unwrap_or_default() < minimum_age {
            continue;
        }
        fs::remove_dir_all(&path)
            .with_context(|| format!("remove stale managed native staging {}", path.display()))?;
        result.removed_bytes = result
            .removed_bytes
            .checked_add(bytes)
            .context("native staging cleanup byte count overflow")?;
        result.removed_staging.push(directory_name.to_string());
    }
    if !result.removed_staging.is_empty() {
        sync_directory(&coverage_parent)?;
    }
    result.removed_staging.sort();
    Ok(result)
}

fn atomic_symlink(target: &Path, link: &Path) -> Result<()> {
    let parent = link.parent().context("symlink target has no parent")?;
    fs::create_dir_all(parent)?;
    if link.exists() && link.is_dir() && !fs::symlink_metadata(link)?.file_type().is_symlink() {
        bail!(
            "refusing to replace a directory with a symlink: {}",
            link.display()
        );
    }
    let file_name = link
        .file_name()
        .and_then(|value| value.to_str())
        .context("symlink filename is not UTF-8")?;
    let temporary = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));
    if temporary.exists() || fs::symlink_metadata(&temporary).is_ok() {
        let metadata = fs::symlink_metadata(&temporary)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            bail!("refusing to remove a directory at symlink staging path");
        }
        fs::remove_file(&temporary)?;
    }
    symlink(target, &temporary)?;
    fs::rename(&temporary, link)?;
    sync_directory(parent)?;
    Ok(())
}

fn tree_bytes_and_latest_modified(root: &Path) -> Result<(u64, SystemTime)> {
    fn walk(path: &Path, bytes: &mut u64, latest: &mut SystemTime) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "managed native tree must not contain symlinks: {}",
                    entry_path.display()
                );
            }
            if let Ok(modified) = metadata.modified() {
                if modified > *latest {
                    *latest = modified;
                }
            }
            if metadata.is_dir() {
                walk(&entry_path, bytes, latest)?;
            } else if metadata.is_file() {
                *bytes = bytes
                    .checked_add(metadata.len())
                    .context("managed native tree byte count overflow")?;
            }
        }
        Ok(())
    }
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "managed native tree must be a real directory: {}",
            root.display()
        );
    }
    let mut bytes = 0;
    let mut latest = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    walk(root, &mut bytes, &mut latest)?;
    Ok((bytes, latest))
}

fn tree_regular_file_bytes(root: &Path) -> Result<u64> {
    Ok(tree_bytes_and_latest_modified(root)?.0)
}

fn coverage_stats(root: &Path) -> Result<(u64, u64)> {
    fn walk(root: &Path, path: &Path, files: &mut u64, bytes: &mut u64) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let entry_path = entry.path();
            let metadata = fs::symlink_metadata(&entry_path)?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "native coverage must not contain symlinks: {}",
                    entry_path.display()
                );
            }
            if metadata.is_dir() {
                walk(root, &entry_path, files, bytes)?;
            } else if metadata.is_file() {
                if entry_path == root.join("coverage.json") {
                    continue;
                }
                *files += 1;
                *bytes = bytes
                    .checked_add(metadata.len())
                    .context("coverage byte count overflow")?;
            }
        }
        Ok(())
    }
    let mut files = 0;
    let mut bytes = 0;
    walk(root, root, &mut files, &mut bytes)?;
    Ok((files, bytes))
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("JSON target has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .context("JSON target filename is not UTF-8")?,
        std::process::id()
    ));
    let mut file = File::create(&temporary)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    sync_directory(parent)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn safe_coverage_path(data_root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute() {
        bail!("native coverage path must be relative");
    }
    let mut output = data_root.to_path_buf();
    for component in path.components() {
        match component {
            Component::Normal(value) => output.push(value),
            _ => bail!("unsafe native coverage path: {relative}"),
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn frozen_gfs_window_is_three_short_and_two_full_runs() {
        let runs = expected_source_runs("2026072018").unwrap();
        assert_eq!(
            runs.iter()
                .map(|value| value.0.as_str())
                .collect::<Vec<_>>(),
            [
                "2026071918",
                "2026072000",
                "2026072006",
                "2026072012",
                "2026072018"
            ]
        );
        assert_eq!(
            runs.iter().map(|value| value.2).collect::<Vec<_>>(),
            [5, 5, 5, 384, 384]
        );
    }

    #[test]
    fn run_parser_rejects_non_canonical_values() {
        assert!(parse_run("2026072018").is_ok());
        assert!(parse_run("202607201").is_err());
        assert!(parse_run("2026072018Z").is_err());
        assert!(parse_run("2026072024").is_err());
    }

    #[test]
    fn sparse_source_schedule_matches_gfs_after_hour_120() {
        let schedule = expected_sparse_forecast_hours(384, false);
        assert_eq!(schedule.len(), 209);
        assert_eq!(&schedule[..3], &[0, 1, 2]);
        assert_eq!(&schedule[118..124], &[118, 119, 120, 123, 126, 129]);
        assert_eq!(schedule.last(), Some(&384));

        let omitted = expected_sparse_forecast_hours(384, true);
        assert_eq!(omitted.len(), 208);
        assert_eq!(omitted.first(), Some(&1));
        assert_eq!(omitted.last(), Some(&384));
    }

    #[test]
    fn required_inventory_matches_production_contract() {
        assert_eq!(required_variables("ncep_gfs013").unwrap().len(), 29);
        assert_eq!(required_variables("ncep_gfs025").unwrap().len(), 168);
        assert!(required_variables("ncep_gfs025")
            .unwrap()
            .contains("vertical_velocity_50hPa"));
    }

    #[test]
    fn hour_zero_omission_is_domain_specific() {
        assert!(variable_omits_hour_zero(
            "ncep_gfs013",
            "shortwave_radiation"
        ));
        assert!(variable_omits_hour_zero(
            "ncep_gfs025",
            "categorical_freezing_rain"
        ));
        assert!(!variable_omits_hour_zero("ncep_gfs025", "pressure_msl"));
    }

    #[test]
    fn backwards_sum_deaverages_each_sparse_interval() {
        let mut values = vec![f32::NAN, f32::NAN, 6.0, f32::NAN, f32::NAN, 9.0];
        interpolate_backwards(&mut values, 6, true);
        assert_eq!(values, [2.0, 2.0, 2.0, 3.0, 3.0, 3.0]);
    }

    #[test]
    fn linear_and_hermite_fill_only_sparse_slots() {
        let mut linear = vec![0.0, f32::NAN, f32::NAN, 3.0];
        interpolate_linear(&mut linear, 4);
        assert_eq!(linear, [0.0, 1.0, 2.0, 3.0]);

        let mut hermite = vec![0.0, f32::NAN, f32::NAN, 3.0, f32::NAN, f32::NAN, 6.0];
        interpolate_hermite(&mut hermite, 7, Some((0.0, 6.0)));
        assert!(hermite.iter().all(|value| value.is_finite()));
        assert_eq!(hermite[0], 0.0);
        assert_eq!(hermite[3], 3.0);
        assert_eq!(hermite[6], 6.0);
        assert!(hermite.iter().all(|value| (0.0..=6.0).contains(value)));
    }

    #[test]
    fn unsafe_coverage_paths_are_rejected() {
        assert!(safe_coverage_path(Path::new("/data/om_raw"), "../outside").is_err());
        assert!(safe_coverage_path(Path::new("/data/om_raw"), "/absolute").is_err());
        assert_eq!(
            safe_coverage_path(
                Path::new("/data/om_raw"),
                "coverages/gfs/gfs_native_2026072018"
            )
            .unwrap(),
            PathBuf::from("/data/om_raw/coverages/gfs/gfs_native_2026072018")
        );
    }

    #[test]
    fn generated_coverage_identity_is_revision_and_run_specific() {
        let revision = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            default_gfs_coverage_id("2026072018", revision).unwrap(),
            "gfs_native_2026072018_official-hourly-quantized-v4_0123456789ab"
        );
        assert!(default_gfs_coverage_id("2026072021", revision).is_err());
        assert!(default_gfs_coverage_id("2026072018", "not-a-sha").is_err());
    }

    #[test]
    fn completed_retention_keeps_current_and_exact_previous_current() {
        fn candidate(id: &str, run: &str) -> ManagedCoverage {
            ManagedCoverage {
                coverage_id: id.to_string(),
                latest_run: parse_run(run).unwrap(),
                modified: SystemTime::UNIX_EPOCH,
                path: PathBuf::from(id),
            }
        }
        let candidates = vec![
            candidate("new", "2026072018"),
            candidate("same-run-other", "2026072018"),
            candidate("previous", "2026072012"),
            candidate("old", "2026072006"),
        ];
        assert_eq!(
            select_retained_coverage_ids(&candidates, "new", Some("previous")),
            BTreeSet::from(["new".to_string(), "previous".to_string()])
        );
        assert_eq!(
            select_retained_coverage_ids(&candidates, "new", Some("new")),
            BTreeSet::from(["new".to_string(), "same-run-other".to_string()])
        );
    }

    #[test]
    fn stale_staging_cleanup_only_removes_owned_exact_identity() {
        let root = tempdir().unwrap();
        let data_root = root.path();
        let parent = data_root.join("coverages/gfs");
        fs::create_dir_all(&parent).unwrap();
        let revision = "0123456789abcdef0123456789abcdef01234567";
        let coverage_id = default_gfs_coverage_id("2026072018", revision).unwrap();
        let managed = parent.join(format!(".incoming_{coverage_id}"));
        fs::create_dir_all(&managed).unwrap();
        atomic_write_json(
            &managed.join("build_identity.json"),
            &json!({
                "version": 1,
                "lifecycle_manager": NATIVE_LIFECYCLE_MANAGER,
                "algorithm": GFS_MATERIALIZATION_REVISION,
                "coverage_id": coverage_id,
                "latest_run": "2026072018",
                "producer_revision": revision
            }),
        )
        .unwrap();
        fs::write(managed.join("partial.om"), b"owned").unwrap();

        let renamed_backup = parent.join(".incoming_manual_backup");
        fs::create_dir_all(&renamed_backup).unwrap();
        atomic_write_json(
            &renamed_backup.join("build_identity.json"),
            &json!({
                "version": 1,
                "lifecycle_manager": NATIVE_LIFECYCLE_MANAGER,
                "algorithm": GFS_MATERIALIZATION_REVISION,
                "coverage_id": coverage_id,
                "latest_run": "2026072018",
                "producer_revision": revision
            }),
        )
        .unwrap();

        let unknown = parent.join("manual_backups");
        fs::create_dir_all(&unknown).unwrap();
        fs::write(unknown.join("keep"), b"keep").unwrap();

        let result = cleanup_stale_gfs_staging_older_than(data_root, StdDuration::ZERO).unwrap();
        assert_eq!(result.removed_staging, [format!(".incoming_{coverage_id}")]);
        assert!(!managed.exists());
        assert!(renamed_backup.exists());
        assert!(unknown.exists());
    }

    #[test]
    fn real_directory_guard_rejects_staging_symlink() {
        let root = tempdir().unwrap();
        let outside = root.path().join("outside");
        let link = root.path().join("incoming");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, &link).unwrap();
        assert!(path_is_real_directory(&link, "test staging").is_err());
        assert!(outside.is_dir());
    }

    #[test]
    fn compatible_staging_is_adopted_without_copying_payload() {
        let root = tempdir().unwrap();
        let old_revision = "1".repeat(40);
        let new_revision = "2".repeat(40);
        let old_id = default_gfs_coverage_id("2026072018", &old_revision).unwrap();
        let new_id = default_gfs_coverage_id("2026072018", &new_revision).unwrap();
        let old_staging = root.path().join(format!(".incoming_{old_id}"));
        let new_staging = root.path().join(format!(".incoming_{new_id}"));
        fs::create_dir_all(&old_staging).unwrap();
        fs::write(old_staging.join("payload.om"), b"payload").unwrap();
        let old_identity = json!({
            "version": 1,
            "lifecycle_manager": NATIVE_LIFECYCLE_MANAGER,
            "algorithm": GFS_MATERIALIZATION_REVISION,
            "coverage_id": old_id,
            "latest_run": "2026072018",
            "producer_revision": old_revision,
            "source_coverages": {"2026072018": {"gfs013_surface": "source-a"}},
            "domain_grids": {"ncep_gfs013": {"nx": 1}}
        });
        atomic_write_json(&old_staging.join("build_identity.json"), &old_identity).unwrap();
        let mut new_identity = old_identity.clone();
        new_identity["coverage_id"] = json!(new_id);
        new_identity["producer_revision"] = json!(new_revision);

        assert!(adopt_compatible_staging(root.path(), &new_staging, &new_identity).unwrap());

        assert!(!old_staging.exists());
        assert_eq!(
            fs::read(new_staging.join("payload.om")).unwrap(),
            b"payload"
        );
        let adopted: Value =
            serde_json::from_slice(&fs::read(new_staging.join("build_identity.json")).unwrap())
                .unwrap();
        assert_eq!(adopted, new_identity);
    }

    #[test]
    fn available_space_reports_nonzero_filesystem_capacity() {
        let root = tempdir().unwrap();
        assert!(available_space(root.path()).unwrap() > 0);
    }
}
