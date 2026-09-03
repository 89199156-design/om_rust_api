//! Materialize cropped EC9 RegionPack releases into immutable Open-Meteo OM.
//!
//! RegionPack is a transport/archive format owned by the private downloader.
//! The public API and WebP renderer must not keep that archive alive.  This
//! module decodes the retained five-run stack onto a compact copy of the
//! cropped native O1280 topology, writes time-series OM arrays, validates
//! them, and atomically publishes the native marker. Only after downstream
//! validation may the downloader acknowledge and remove the source batch.

use crate::manifest::NativeGridMetadata;
use crate::native::{load_native_group_products, read_native_array_metadata};
use crate::official::{build_v3_array_metadata_blob, BundleRangeReader, OfficialDecoder};
use crate::query::ecmwf_ifs9km_storage_scale_factor;
use crate::regionpack::{
    O1280RegionTopology, RegionPackRun, RegionPackSamplingPlan, RegionPackSnapshot,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use rayon::prelude::*;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const GROUP: &str = "ecmwf_ifs9km";
const PRODUCT: &str = "ecmwf_ifs9km";
const RUNTIME_FORMAT: &str = "openmeteo-native-v1";
const MATERIALIZATION_REVISION: &str = "regionpack-to-native-v2-ecpds-semantics";
const DATA_TYPE_FLOAT_ARRAY: u8 = 20;
const COMPRESSION_PFOR_DELTA2D_INT16: u8 = 0;

#[derive(Debug, Clone)]
pub struct Ec9BuildOptions {
    pub source_root: PathBuf,
    pub data_root: PathBuf,
    pub producer_revision: String,
    pub workers: usize,
    pub minimum_free_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ec9BuildResult {
    pub coverage_id: String,
    pub coverage_path: PathBuf,
    pub latest_complete_run: String,
    pub source_runs: Vec<String>,
    pub om_files: u64,
    pub bytes: u64,
    pub decoded_probes: u64,
    pub reused: bool,
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

fn grid(topology: &O1280RegionTopology) -> NativeGridMetadata {
    NativeGridMetadata {
        grid_type: Some("o1280_reduced_gaussian_crop".to_string()),
        nx: topology.location_count(),
        ny: 1,
        lon_min: 70.0,
        lat_min: 0.0,
        // Rendering remains on the established nominal 9 km regular grid;
        // point storage itself is the compact native O1280 topology below.
        dx: 0.078125,
        dy: 0.078125,
        dt_seconds: 3600,
        om_file_length: 361,
        o1280_row_start: Some(topology.row_start),
        o1280_row_offsets: Some(topology.row_offsets.clone()),
        o1280_first_columns: Some(topology.first_columns.clone()),
        o1280_column_counts: Some(topology.column_counts.clone()),
        ..NativeGridMetadata::default()
    }
}

fn grid_marker(grid: &NativeGridMetadata) -> Value {
    serde_json::to_value(grid).expect("EC9 native grid metadata is serializable")
}

fn validate_revision(value: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("EC9 producer revision must be a full lowercase Git commit SHA");
    }
    Ok(())
}

fn validate_component(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid EC9 native coverage component");
    }
    Ok(())
}

fn run_path(run: &RegionPackRun) -> PathBuf {
    PathBuf::from(format!(
        "{:04}/{:02}/{:02}/{:02}00Z",
        run.reference_time.year(),
        run.reference_time.month(),
        run.reference_time.day(),
        run.reference_time.hour()
    ))
}

fn selected_frames(
    run: &RegionPackRun,
) -> Vec<(DateTime<Utc>, &Arc<crate::regionpack::RegionPackFile>)> {
    run.frames
        .iter()
        .filter(|(valid_time, _)| {
            let forecast_hours = (**valid_time - run.reference_time).num_hours();
            // Preserve the source schedule exactly.  Current IFS operational
            // runs are hourly through f090, three-hourly through f144 and
            // (for 00Z/12Z) six-hourly afterwards.  Discarding the native
            // hourly frames and reconstructing them from a three-hour lattice
            // changes even direct fields such as temperature_2m.
            forecast_hours >= 0
        })
        .map(|(valid_time, file)| (*valid_time, file))
        .collect()
}

fn ensure_free_space(path: &Path, minimum: u64) -> Result<()> {
    if minimum == 0 {
        bail!("EC9 native minimum free-space reserve must be positive");
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::mem::MaybeUninit;
        use std::os::unix::ffi::OsStrExt;
        let mut ancestor = path;
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .context("EC9 data root has no existing ancestor")?;
        }
        let encoded = CString::new(ancestor.as_os_str().as_bytes())?;
        let mut output = MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: encoded is NUL terminated and output is initialized on success.
        if unsafe { libc::statvfs(encoded.as_ptr(), output.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("read EC9 free space for {}", ancestor.display()));
        }
        // SAFETY: the successful statvfs call initialized output.
        let output = unsafe { output.assume_init() };
        let available = output.f_bavail.saturating_mul(output.f_frsize);
        if available < minimum {
            bail!(
                "insufficient data-disk space for EC9 native output: available={} reserve={}",
                available,
                minimum
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_variable(
    run: &RegionPackRun,
    frames: &[(DateTime<Utc>, &Arc<crate::regionpack::RegionPackFile>)],
    snapshot: &RegionPackSnapshot,
    plan: &RegionPackSamplingPlan,
    grid: &NativeGridMetadata,
    variable: &str,
    destination: &Path,
    data_root: &Path,
    minimum_free_bytes: u64,
    decoder: &OfficialDecoder,
) -> Result<bool> {
    ensure_free_space(data_root, minimum_free_bytes)?;
    let n_time = u64::try_from(frames.len())?;
    let location_count = usize::try_from(grid.nx * grid.ny)?;
    let n_time_usize = usize::try_from(n_time)?;
    let mut dense = vec![f32::NAN; location_count * n_time_usize];
    let mut found = false;
    let expected_scale_factor = ecmwf_ifs9km_storage_scale_factor(variable);
    let mut source_scale_factor = None;
    for (time_index, (_, file)) in frames.iter().enumerate() {
        if let Some(scale_factor) = file.variable_scale_factor(snapshot.bounds(), variable)? {
            if scale_factor != expected_scale_factor {
                bail!(
                    "EC9 source scale factor for {variable} changed: source={scale_factor} expected={expected_scale_factor}"
                );
            }
            if source_scale_factor.is_some_and(|previous| previous != scale_factor) {
                bail!("EC9 source scale factor for {variable} changes within one run");
            }
            source_scale_factor = Some(scale_factor);
        }
        let Some(values) = file.decode(decoder, snapshot.bounds(), variable, plan)? else {
            continue;
        };
        if values.len() != location_count {
            bail!("decoded EC9 frame has the wrong compact-grid size");
        }
        found = true;
        for (location, value) in values.into_iter().enumerate() {
            dense[location * n_time_usize + time_index] = value;
        }
    }
    if !found {
        return Ok(false);
    }
    if dense.iter().any(|value| value.is_infinite()) {
        bail!(
            "EC9 source produced an infinite value: {}/{}",
            run.source_run,
            variable
        );
    }
    // A compact location axis preserves every cropped reduced-Gaussian point
    // without padding each row to the equatorial width.
    let chunks = vec![1, 64_u64.min(grid.nx), n_time];
    let dimensions = vec![grid.ny, grid.nx, n_time];
    let source_scale_factor = source_scale_factor
        .with_context(|| format!("EC9 source has no scale factor for {variable}"))?;
    let mut writer = decoder.create_array_writer(
        destination,
        dimensions,
        chunks,
        source_scale_factor,
        0.0,
        DATA_TYPE_FLOAT_ARRAY,
        COMPRESSION_PFOR_DELTA2D_INT16,
    )?;
    writer.write_f32_block(&dense, &[grid.ny, grid.nx, n_time])?;
    writer.finish(variable)?;
    Ok(true)
}

fn validate_file(
    path: &Path,
    variable: &str,
    expected_time: u64,
    grid: &NativeGridMetadata,
    decoder: &OfficialDecoder,
) -> Result<u64> {
    let file = Arc::new(File::open(path)?);
    let array = read_native_array_metadata(&file)?;
    if array.dimensions != [grid.ny, grid.nx, expected_time]
        || array.chunks.len() != 3
        || array.data_type != DATA_TYPE_FLOAT_ARRAY
        || array.compression != COMPRESSION_PFOR_DELTA2D_INT16
    {
        bail!("invalid EC9 native OM array: {}", path.display());
    }
    let metadata = build_v3_array_metadata_blob(
        variable,
        array.data_type,
        array.compression,
        &array.dimensions,
        &array.chunks,
        array.lut_size.context("EC9 OM array has no LUT size")?,
        array.lut_offset.context("EC9 OM array has no LUT offset")?,
        array
            .scale_factor
            .context("EC9 OM array has no scale factor")?,
        array.add_offset.unwrap_or(0.0),
    );
    for (y, x) in [(0, 0), (0, grid.nx / 2), (grid.ny - 1, grid.nx - 1)] {
        let values = decoder.decode_grid(
            &metadata,
            &FullFileRangeReader { file: file.clone() },
            &[y, x, 0],
            &[1, 1, expected_time],
        )?;
        if values.len() != usize::try_from(expected_time)?
            || values.iter().any(|value| value.is_infinite())
        {
            bail!("EC9 native OM decode probe failed: {}", path.display());
        }
    }
    Ok(3 * expected_time)
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_file_name(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .context("JSON path is not UTF-8")?,
        std::process::id()
    ));
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644))?;
    fs::rename(&temporary, path)?;
    File::open(path.parent().context("JSON path has no parent")?)?.sync_all()?;
    Ok(())
}

fn tree_stats(root: &Path) -> Result<(u64, u64)> {
    fn walk(path: &Path, files: &mut u64, bytes: &mut u64) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                walk(&entry.path(), files, bytes)?;
            } else if metadata.is_file() {
                *files += 1;
                *bytes = bytes.saturating_add(metadata.len());
            }
        }
        Ok(())
    }
    let (mut files, mut bytes) = (0, 0);
    walk(root, &mut files, &mut bytes)?;
    Ok((files, bytes))
}

fn cleanup_old_coverages(data_root: &Path, current: &str) -> Result<()> {
    let root = data_root.join("coverages").join(GROUP);
    let mut managed = fs::read_dir(&root)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| (name, entry.path()))
        })
        .filter(|(name, _)| name.starts_with("ec9-native-") && name != current)
        .collect::<Vec<_>>();
    managed.sort_by(|left, right| right.0.cmp(&left.0));
    // Keep one prior immutable release for restart-safe rollback.  Older
    // producer-owned releases are no longer referenced by any current marker.
    for (_, path) in managed.into_iter().skip(1) {
        fs::remove_dir_all(&path)
            .with_context(|| format!("remove obsolete EC9 native coverage {}", path.display()))?;
    }
    Ok(())
}

pub fn build_and_publish_ec9_coverage(
    options: &Ec9BuildOptions,
    decoder: &OfficialDecoder,
) -> Result<Ec9BuildResult> {
    validate_revision(&options.producer_revision)?;
    if options.workers == 0 {
        bail!("EC9 native worker count must be positive");
    }
    let snapshot = Arc::new(
        RegionPackSnapshot::load(&options.source_root)?
            .context("source root has no complete EC9 RegionPack release")?,
    );
    let suffix = &options.producer_revision[..12];
    let coverage_id = format!(
        "ec9-native-{}-{}-{}",
        snapshot.latest_complete_run(),
        snapshot.public_start_utc().format("%Y%m%d"),
        suffix
    );
    validate_component(&coverage_id)?;
    let immutable = options
        .data_root
        .join("coverages")
        .join(GROUP)
        .join(&coverage_id);
    let marker_path = options
        .data_root
        .join("groups")
        .join(GROUP)
        .join("current/ready_for_processing.json");
    if immutable.is_dir() {
        let marker: Value = serde_json::from_slice(&fs::read(immutable.join("coverage.json"))?)?;
        if marker.get("coverage_id").and_then(Value::as_str) != Some(coverage_id.as_str()) {
            bail!("existing EC9 immutable coverage has the wrong identity");
        }
        write_json(&marker_path, &marker)?;
        let (files, bytes) = tree_stats(&immutable)?;
        return Ok(Ec9BuildResult {
            coverage_id,
            coverage_path: immutable,
            latest_complete_run: snapshot.latest_complete_run().to_string(),
            source_runs: snapshot
                .runs_oldest_to_newest()
                .iter()
                .map(|run| run.source_run.clone())
                .collect(),
            om_files: files.saturating_sub(1),
            bytes,
            decoded_probes: 0,
            reused: true,
        });
    }

    ensure_free_space(&options.data_root, options.minimum_free_bytes)?;
    let staging_parent = options.data_root.join("staging");
    fs::create_dir_all(&staging_parent)?;
    let staging = staging_parent.join(format!("{}.incoming.{}", coverage_id, std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    let (plan, topology) = snapshot.native_region_plan()?;
    let plan = Arc::new(plan);
    let grid = grid(&topology);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.workers)
        .thread_name(|index| format!("ec9-native-{index}"))
        .build()?;
    let mut run_variables = BTreeMap::<String, Vec<String>>::new();
    let mut run_valid_times = BTreeMap::<String, Vec<DateTime<Utc>>>::new();
    let mut horizons = Vec::new();
    for run in snapshot.runs_oldest_to_newest() {
        let frames = selected_frames(run);
        let last = frames
            .last()
            .context("EC9 retained run has no selected frames")?
            .0;
        let horizon = (last - run.reference_time).num_hours();
        horizons.push(horizon);
        let destination = staging.join("data_run").join(PRODUCT).join(run_path(run));
        fs::create_dir_all(&destination)?;
        let variables = snapshot.available_variables().to_vec();
        let written = pool.install(|| {
            variables
                .par_iter()
                .map(|variable| {
                    let path = destination.join(format!("{variable}.om"));
                    write_variable(
                        run,
                        &frames,
                        snapshot.as_ref(),
                        plan.as_ref(),
                        &grid,
                        variable,
                        &path,
                        &options.data_root,
                        options.minimum_free_bytes,
                        decoder,
                    )
                    .map(|present| present.then(|| variable.clone()))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut written = written.into_iter().flatten().collect::<Vec<_>>();
        written.sort();
        if written.is_empty() {
            bail!("EC9 native run {} produced no variables", run.source_run);
        }
        let valid_times = frames.iter().map(|(time, _)| *time).collect::<Vec<_>>();
        let meta = json!({
            "created_at": Utc::now().to_rfc3339(),
            "crs_wkt": "GEOGCRS[\"WGS 84\"]",
            "reference_time": run.reference_time,
            "temporal_resolution_seconds": 3600,
            "valid_times": valid_times,
            "variables": written,
        });
        write_json(&destination.join("meta.json"), &meta)?;
        run_variables.insert(run.source_run.clone(), written);
        run_valid_times.insert(run.source_run.clone(), valid_times);
    }
    let source_runs = snapshot
        .runs_oldest_to_newest()
        .iter()
        .map(|run| run.source_run.clone())
        .collect::<Vec<_>>();
    let grid_marker = grid_marker(&grid);
    let marker = json!({
        "schema_version": 1,
        "native_producer_contract": 1,
        "status": "complete",
        "runtime_format": RUNTIME_FORMAT,
        "group": GROUP,
        "coverage_id": coverage_id,
        "release_id": coverage_id,
        "batch_id": snapshot.batch_id(),
        "latest_complete_run": snapshot.latest_complete_run(),
        "source_runs": source_runs,
        "source_run_max_forecast_hours": horizons,
        "source_run_roles": ["oldest-history", "history", "older-support", "immediate-fallback", "target"],
        "nan_fallback_depth": 4,
        "public_start_utc": snapshot.public_start_utc(),
        "local_utc_offset_hours": 8,
        "coverage_path": format!("coverages/{GROUP}/{coverage_id}"),
        "producer_revision": options.producer_revision,
        "materialization_revision": MATERIALIZATION_REVISION,
        "coverage_policy": {
            "source_order": "newest_to_oldest",
            "structural_fallback": "first_covering_run",
            "nan_fallback": "continue_to_all_older_retained_runs",
            "source_cadence": "hourly_to_f090_then_3h_to_f144_then_6h",
            "public_cadence": "hourly_with_sparse_tail_interpolated"
        },
        "products": {
            PRODUCT: {
                "coverage_id": coverage_id,
                "runtime_domain": PRODUCT,
                "grid": grid_marker,
                "source_runs": source_runs,
                "source_run_max_forecast_hours": horizons,
            }
        },
        "data_license": "CC-BY-4.0",
        "modified": true,
    });
    write_json(&staging.join("coverage.json"), &marker)?;

    let mut decoded_probes = 0_u64;
    for run in snapshot.runs_oldest_to_newest() {
        let variables = run_variables
            .get(&run.source_run)
            .context("missing EC9 run inventory")?;
        let expected_time = u64::try_from(
            run_valid_times
                .get(&run.source_run)
                .context("missing EC9 run time axis")?
                .len(),
        )?;
        let root = staging.join("data_run").join(PRODUCT).join(run_path(run));
        for variable in variables {
            decoded_probes += validate_file(
                &root.join(format!("{variable}.om")),
                variable,
                expected_time,
                &grid,
                decoder,
            )?;
        }
    }
    fs::create_dir_all(
        immutable
            .parent()
            .context("EC9 immutable path has no parent")?,
    )?;
    fs::rename(&staging, &immutable)?;
    File::open(immutable.parent().unwrap())?.sync_all()?;

    let previous_marker = fs::read(&marker_path).ok();
    write_json(&marker_path, &marker)?;
    let mut products = HashMap::new();
    let mut history = HashMap::new();
    let validation = load_native_group_products(
        &options.data_root,
        GROUP,
        &[PRODUCT],
        &mut products,
        &mut history,
    );
    if let Err(error) = validation {
        if let Some(previous) = previous_marker {
            let previous: Value = serde_json::from_slice(&previous)?;
            write_json(&marker_path, &previous)?;
        } else if marker_path.is_file() {
            fs::remove_file(&marker_path)?;
        }
        return Err(error).context("validate atomically published EC9 native coverage");
    }
    if !products.contains_key(PRODUCT) {
        bail!("published EC9 native marker did not load its product");
    }
    cleanup_old_coverages(&options.data_root, &coverage_id)?;
    let (files, bytes) = tree_stats(&immutable)?;
    Ok(Ec9BuildResult {
        coverage_id,
        coverage_path: immutable,
        latest_complete_run: snapshot.latest_complete_run().to_string(),
        source_runs,
        om_files: files.saturating_sub(6),
        bytes,
        decoded_probes,
        reused: false,
    })
}

#[cfg(test)]
mod tests {
    use super::{grid, grid_marker};
    use crate::regionpack::O1280RegionTopology;

    #[test]
    fn published_grid_marker_matches_native_webp_contract() {
        let topology = O1280RegionTopology {
            row_start: 995,
            row_offsets: vec![0, 3, 5],
            first_columns: vec![1488, 1490],
            column_counts: vec![3, 2],
        };
        let marker = grid_marker(&grid(&topology));
        assert_eq!(
            marker.get("grid_type").and_then(|value| value.as_str()),
            Some("o1280_reduced_gaussian_crop")
        );
        assert_eq!(marker.get("nx").and_then(|value| value.as_u64()), Some(5));
        assert_eq!(marker.get("ny").and_then(|value| value.as_u64()), Some(1));
        assert_eq!(
            marker
                .get("o1280_row_start")
                .and_then(|value| value.as_u64()),
            Some(995)
        );
    }
}
