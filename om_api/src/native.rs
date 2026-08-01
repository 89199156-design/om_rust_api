use crate::manifest::{
    ArrayMetadata, BundleEntry, CoveragePlanEntry, EntryKey, ManifestFile, NativeGridMetadata,
    ProductManifest, ProductSnapshot,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDateTime, Timelike, Utc};
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const OM_TRAILER_SIZE: u64 = 24;
const GFS013_HSURF_RELATIVE_PATH: &str = "static/ncep_gfs013/HSURF.om";
const GFS013_HSURF_BYTES: u64 = 1_455_544;
const GFS013_HSURF_SHA256: &str =
    "203745df4dfa10069e1a39206350e006818a0eea644bb19c1668c0f32f7475e0";
const GFS025_HSURF_RELATIVE_PATH: &str = "static/ncep_gfs025/HSURF.om";
const GFS025_HSURF_BYTES: u64 = 408_440;
const GFS025_HSURF_SHA256: &str =
    "fdd9587e606e64d6d85474c703b9898669d230aac1574fc460cc3087227e868d";

#[derive(Debug, Deserialize)]
struct NativeReady {
    status: String,
    runtime_format: String,
    group: String,
    coverage_id: String,
    latest_complete_run: String,
    source_runs: Vec<String>,
    #[serde(default)]
    greenhouse_source_runs: Vec<String>,
    public_start_utc: DateTime<Utc>,
    #[serde(default)]
    generated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    local_utc_offset_hours: Option<i64>,
    coverage_path: String,
    products: HashMap<String, NativeProductReady>,
    #[serde(default)]
    static_sources: HashMap<String, NativeStaticSourceReady>,
    #[serde(default)]
    short_run_count: Option<usize>,
    #[serde(default)]
    full_run_count: Option<usize>,
    #[serde(default)]
    source_run_max_forecast_hours: Vec<i64>,
    #[serde(default)]
    source_run_roles: Vec<String>,
    #[serde(default)]
    gust_support_run_count: Option<usize>,
    #[serde(default)]
    gust_support_max_forecast_hour: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct NativeProductReady {
    runtime_domain: String,
    grid: NativeGridMetadata,
    #[serde(default)]
    source_runs: Vec<String>,
    #[serde(default)]
    source_run_max_forecast_hours: Vec<i64>,
}

#[derive(Debug, Deserialize)]
struct NativeStaticSourceReady {
    source: String,
    runtime_path: String,
    #[serde(default)]
    storage: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    latitude_chunk_min: Option<i32>,
    #[serde(default)]
    latitude_chunk_max: Option<i32>,
    #[serde(default)]
    file_count: Option<usize>,
    #[serde(default)]
    bytes: Option<u64>,
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NativeRunMeta {
    reference_time: DateTime<Utc>,
    variables: Vec<String>,
    #[serde(deserialize_with = "deserialize_utc_datetimes")]
    valid_times: Vec<DateTime<Utc>>,
}

fn deserialize_utc_datetimes<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|parsed| parsed.with_timezone(&Utc))
                .or_else(|_| {
                    NaiveDateTime::parse_from_str(&value, "%Y-%m-%dT%H:%MZ")
                        .map(|parsed| parsed.and_utc())
                })
                .map_err(serde::de::Error::custom)
        })
        .collect()
}

fn read_exact_at(file: &File, offset: u64, size: usize) -> Result<Vec<u8>> {
    let mut output = vec![0_u8; size];
    file.read_exact_at(&mut output, offset)?;
    Ok(output)
}

fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        data.get(offset..offset + 2)
            .context("OM metadata u16 exceeds bounds")?
            .try_into()?,
    ))
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .context("OM metadata u32 exceeds bounds")?
            .try_into()?,
    ))
}

fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset + 8)
            .context("OM metadata u64 exceeds bounds")?
            .try_into()?,
    ))
}

fn f32_at(data: &[u8], offset: usize) -> Result<f32> {
    Ok(f32::from_le_bytes(
        data.get(offset..offset + 4)
            .context("OM metadata f32 exceeds bounds")?
            .try_into()?,
    ))
}

pub fn read_native_array_metadata(file: &File) -> Result<ArrayMetadata> {
    let size = file.metadata()?.len();
    if size < OM_TRAILER_SIZE + 3 {
        bail!("OM file is too small");
    }
    let header = read_exact_at(file, 0, 3)?;
    if &header[0..2] != b"OM" || header[2] != 3 {
        bail!("native runtime file is not OM v3");
    }
    let trailer = read_exact_at(file, size - OM_TRAILER_SIZE, OM_TRAILER_SIZE as usize)?;
    if &trailer[0..2] != b"OM" || trailer[2] != 3 {
        bail!("invalid OM v3 trailer");
    }
    let root_offset = u64_at(&trailer, 8)?;
    let root_size = u64_at(&trailer, 16)?;
    if root_size > 1024 * 1024
        || root_offset
            .checked_add(root_size)
            .is_none_or(|end| end > size)
    {
        bail!("invalid OM root metadata range");
    }
    let root = read_exact_at(file, root_offset, root_size as usize)?;
    if root.len() < 40 {
        bail!("OM root array metadata is truncated");
    }
    let data_type = root[0];
    let compression = root[1];
    if !(12..=21).contains(&data_type) {
        bail!("OM root variable is not an array");
    }
    let name_size = u16_at(&root, 2)? as usize;
    let child_count = u32_at(&root, 4)? as usize;
    let lut_size = u64_at(&root, 8)?;
    let lut_offset = u64_at(&root, 16)?;
    let dimension_count = u64_at(&root, 24)? as usize;
    let scale_factor = f32_at(&root, 32)?;
    let add_offset = f32_at(&root, 36)?;
    let mut cursor = 40_usize
        .checked_add(
            child_count
                .checked_mul(16)
                .context("OM child metadata overflow")?,
        )
        .context("OM metadata overflow")?;
    let mut dimensions = Vec::with_capacity(dimension_count);
    for _ in 0..dimension_count {
        dimensions.push(u64_at(&root, cursor)?);
        cursor += 8;
    }
    let mut chunks = Vec::with_capacity(dimension_count);
    for _ in 0..dimension_count {
        chunks.push(u64_at(&root, cursor)?);
        cursor += 8;
    }
    if cursor
        .checked_add(name_size)
        .is_none_or(|end| end > root.len())
    {
        bail!("OM root name exceeds metadata bounds");
    }
    Ok(ArrayMetadata {
        data_type,
        compression,
        dimensions,
        chunks,
        lut_offset: Some(lut_offset),
        lut_size: Some(lut_size),
        scale_factor: Some(scale_factor),
        add_offset: Some(add_offset),
    })
}

fn safe_relative_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute() {
        bail!("absolute native coverage path is not allowed");
    }
    let mut output = root.to_path_buf();
    for component in path.components() {
        match component {
            Component::Normal(value) => output.push(value),
            _ => bail!("unsafe native coverage path: {relative}"),
        }
    }
    Ok(output)
}

fn run_relative_path(run: &str) -> Result<PathBuf> {
    let parsed = DateTime::parse_from_str(&format!("{run}00 +0000"), "%Y%m%d%H%M %z")?;
    Ok(PathBuf::from(parsed.format("%Y/%m/%d/%H00Z").to_string()))
}

fn parse_run(run: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_str(&format!("{run}00 +0000"), "%Y%m%d%H%M %z")?.with_timezone(&Utc))
}

fn product_accepts_variable(product: &str, variable: &str) -> bool {
    match product {
        "gfs_pressure_profile" => variable.ends_with("hPa"),
        "gfs025" => !variable.ends_with("hPa"),
        _ => true,
    }
}

fn native_time_indices(
    runtime_domain: &str,
    variable: &str,
    meta: &NativeRunMeta,
    stored_time_count: usize,
) -> Result<Vec<usize>> {
    if stored_time_count == 0 || meta.valid_times.is_empty() {
        bail!("native OM time axis must not be empty");
    }
    if stored_time_count == meta.valid_times.len() {
        return Ok((0..stored_time_count).collect());
    }
    if stored_time_count + 1 == meta.valid_times.len()
        && meta.valid_times.first() == Some(&meta.reference_time)
    {
        return Ok((1..meta.valid_times.len()).collect());
    }
    if runtime_domain == "ecmwf_ifs025" && variable == "wind_gusts_10m" {
        let indices = meta
            .valid_times
            .iter()
            .enumerate()
            .filter_map(|(index, valid_time)| {
                let forecast_hour = (*valid_time - meta.reference_time).num_hours();
                (forecast_hour > 0 && (forecast_hour <= 90 || forecast_hour >= 150))
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        if indices.len() == stored_time_count {
            return Ok(indices);
        }
    }
    for (index, valid_time) in meta.valid_times.iter().enumerate() {
        if *valid_time - meta.reference_time != Duration::hours(index as i64) {
            bail!("native run metadata must contain a continuous hourly time axis");
        }
    }
    let indices = if runtime_domain.starts_with("ncep_gfs") {
        (0..meta.valid_times.len())
            .filter(|forecast_hour| *forecast_hour <= 120 || *forecast_hour % 3 == 0)
            .collect::<Vec<_>>()
    } else if runtime_domain.starts_with("cams_global") {
        (0..meta.valid_times.len())
            .filter(|forecast_hour| *forecast_hour % 3 == 0)
            .collect::<Vec<_>>()
    } else {
        bail!("unsupported sparse native time axis for {runtime_domain}");
    };
    if indices.len() != stored_time_count {
        bail!(
            "native OM stored time count {} does not match {} source schedule {}",
            stored_time_count,
            runtime_domain,
            indices.len()
        );
    }
    Ok(indices)
}

fn expected_forecast_hours(runtime_domain: &str, max_forecast_hour: i64) -> Vec<i64> {
    if runtime_domain.starts_with("ncep_gfs") {
        (0..=max_forecast_hour.min(120))
            .chain((123..=max_forecast_hour).filter(|forecast_hour| forecast_hour % 3 == 0))
            .collect()
    } else if runtime_domain == "ncep_gefs025" {
        (3..=max_forecast_hour).step_by(3).collect()
    } else if runtime_domain == "ncep_gefs05" {
        if max_forecast_hour < 240 {
            (3..=max_forecast_hour).step_by(3).collect()
        } else {
            (3..240)
                .step_by(3)
                .chain((240..=max_forecast_hour).step_by(6))
                .collect()
        }
    } else if runtime_domain == "ecmwf_ifs025" && max_forecast_hour == 186 {
        (3..=max_forecast_hour.min(90))
            .step_by(3)
            .chain((150..=max_forecast_hour).step_by(6))
            .collect()
    } else if runtime_domain.starts_with("ecmwf_ifs025") {
        let start = if runtime_domain.ends_with("_ensemble") {
            3
        } else {
            0
        };
        (start..=max_forecast_hour.min(144))
            .filter(|forecast_hour| forecast_hour % 3 == 0)
            .chain((150..=max_forecast_hour).filter(|forecast_hour| forecast_hour % 6 == 0))
            .collect()
    } else if runtime_domain == "cams_global_greenhouse_gases" {
        (0..=max_forecast_hour).step_by(3).collect()
    } else {
        (0..=max_forecast_hour).collect()
    }
}

fn validate_run_time_axis(
    runtime_domain: &str,
    meta: &NativeRunMeta,
    max_forecast_hour: i64,
) -> Result<()> {
    let expected_source = expected_forecast_hours(runtime_domain, max_forecast_hour)
        .into_iter()
        .map(|hour| meta.reference_time + Duration::hours(hour))
        .collect::<Vec<_>>();
    let is_dense_gfs = runtime_domain.starts_with("ncep_gfs")
        && meta.valid_times
            == (0..=max_forecast_hour)
                .map(|hour| meta.reference_time + Duration::hours(hour))
                .collect::<Vec<_>>();
    if meta.valid_times != expected_source && !is_dense_gfs {
        bail!(
            "native run time axis does not match {} 0...{}h contract",
            runtime_domain,
            max_forecast_hour
        );
    }
    Ok(())
}

fn product_source_runs<'a>(
    ready: &'a NativeReady,
    product: &str,
    product_ready: &'a NativeProductReady,
) -> &'a [String] {
    if !product_ready.source_runs.is_empty() {
        return &product_ready.source_runs;
    }
    if product == "cams_global_greenhouse_gases" && !ready.greenhouse_source_runs.is_empty() {
        return &ready.greenhouse_source_runs;
    }
    &ready.source_runs
}

fn run_horizon(
    ready: &NativeReady,
    product: &str,
    product_ready: &NativeProductReady,
    source_run: &str,
) -> Result<i64> {
    let source_runs = product_source_runs(ready, product, product_ready);
    let index = source_runs
        .iter()
        .position(|run| run == source_run)
        .with_context(|| format!("source run is not declared by marker: {source_run}"))?;
    if !product_ready.source_run_max_forecast_hours.is_empty() {
        return product_ready
            .source_run_max_forecast_hours
            .get(index)
            .copied()
            .context("native product has no horizon for source run");
    }
    if product == "cams_global_greenhouse_gases" {
        return Ok(120);
    }
    if ready.group == "gfs" || ready.group == "ecmwf" {
        return ready
            .source_run_max_forecast_hours
            .get(index)
            .copied()
            .with_context(|| {
                format!(
                    "{} marker has no horizon for source run",
                    ready.group.to_uppercase()
                )
            });
    }
    Ok(120)
}

fn attach_static_elevation(
    coverage_root: &Path,
    ready: &NativeReady,
    product_ready: &NativeProductReady,
    static_entries: &mut HashMap<String, BundleEntry>,
    native_handles: &mut HashMap<String, Arc<File>>,
) -> Result<()> {
    let path = coverage_root
        .join(&product_ready.runtime_domain)
        .join("static")
        .join("HSURF.om");
    if !path.is_file() {
        return Ok(());
    }
    let handle = Arc::new(
        File::open(&path).with_context(|| format!("open native static OM {}", path.display()))?,
    );
    let array = read_native_array_metadata(&handle)
        .with_context(|| format!("parse native static OM {}", path.display()))?;
    if array.dimensions != [product_ready.grid.ny, product_ready.grid.nx] || array.chunks.len() != 2
    {
        bail!("native HSURF dimensions do not match regional grid");
    }
    let relative = path
        .strip_prefix(coverage_root)?
        .to_string_lossy()
        .replace('\\', "/");
    native_handles.insert(relative.clone(), handle);
    static_entries.insert(
        "surface_elevation".to_string(),
        BundleEntry {
            variable: "surface_elevation".to_string(),
            variable_path: Some("HSURF".to_string()),
            valid_time_utc: ready.public_start_utc,
            source_run: ready.latest_complete_run.clone(),
            forecast_hour: 0,
            coverage_source_run: None,
            coverage_forecast_hour: None,
            interpolation_support: false,
            source_url: None,
            selection_ranges: vec![[0, product_ready.grid.ny], [0, product_ready.grid.nx]],
            array,
            lut_byte_ranges: Vec::new(),
            data_byte_ranges: Vec::new(),
            lut_bytes_read: 0,
            byte_ranges: Vec::new(),
            bundle_offset: 0,
            bundle_bytes: path.metadata()?.len(),
            native_file_path: Some(relative),
            native_time_index: None,
            native_grid: Some(product_ready.grid.clone()),
        },
    );
    Ok(())
}

fn product_uses_static_elevation(product: &str) -> bool {
    matches!(
        product,
        "gfs013_surface" | "gfs025" | "gfs_pressure_profile" | "ecmwf_ifs025"
    )
}

fn load_native_product_run(
    coverage_root: &Path,
    ready: &NativeReady,
    product: &str,
    product_ready: &NativeProductReady,
    source_run: &str,
    include_static: bool,
) -> Result<ProductSnapshot> {
    let reference_time = parse_run(source_run)?;
    let run_root = coverage_root
        .join("data_run")
        .join(&product_ready.runtime_domain)
        .join(run_relative_path(source_run)?);
    let meta_path = run_root.join("meta.json");
    let meta: NativeRunMeta = serde_json::from_slice(
        &std::fs::read(&meta_path)
            .with_context(|| format!("read native run metadata {}", meta_path.display()))?,
    )?;
    if meta.reference_time != reference_time {
        bail!("native run reference time mismatch: {source_run}");
    }
    validate_run_time_axis(
        &product_ready.runtime_domain,
        &meta,
        run_horizon(ready, product, product_ready, source_run)?,
    )?;

    let mut entries = HashMap::new();
    let mut static_entries = HashMap::new();
    let mut native_handles = HashMap::new();
    for variable in meta
        .variables
        .iter()
        .filter(|variable| product_accepts_variable(product, variable))
    {
        let file_path = run_root.join(format!("{variable}.om"));
        let handle = Arc::new(
            File::open(&file_path)
                .with_context(|| format!("open native OM file {}", file_path.display()))?,
        );
        let array = read_native_array_metadata(&handle)
            .with_context(|| format!("parse native OM file {}", file_path.display()))?;
        if array.dimensions.len() != 3
            || array.chunks.len() != 3
            || array.dimensions[0] != product_ready.grid.ny
            || array.dimensions[1] != product_ready.grid.nx
        {
            bail!(
                "native OM dimensions do not match grid: {}",
                file_path.display()
            );
        }
        let time_indices = native_time_indices(
            &product_ready.runtime_domain,
            variable,
            &meta,
            usize::try_from(array.dimensions[2])?,
        )
        .with_context(|| {
            format!(
                "map native time indices for {} {source_run} {variable}",
                product_ready.runtime_domain
            )
        })?;
        let relative = file_path
            .strip_prefix(coverage_root)?
            .to_string_lossy()
            .replace('\\', "/");
        native_handles.insert(relative.clone(), handle);
        for (time_index, valid_time_index) in time_indices.into_iter().enumerate() {
            let valid_time = meta.valid_times[valid_time_index];
            entries.insert(
                EntryKey {
                    variable: variable.clone(),
                    valid_time_utc: valid_time,
                },
                BundleEntry {
                    variable: variable.clone(),
                    variable_path: Some(variable.clone()),
                    valid_time_utc: valid_time,
                    source_run: source_run.to_string(),
                    forecast_hour: (valid_time - reference_time).num_hours(),
                    coverage_source_run: None,
                    coverage_forecast_hour: None,
                    interpolation_support: false,
                    source_url: None,
                    selection_ranges: vec![[0, product_ready.grid.ny], [0, product_ready.grid.nx]],
                    array: array.clone(),
                    lut_byte_ranges: Vec::new(),
                    data_byte_ranges: Vec::new(),
                    lut_bytes_read: 0,
                    byte_ranges: Vec::new(),
                    bundle_offset: 0,
                    bundle_bytes: file_path.metadata()?.len(),
                    native_file_path: Some(relative.clone()),
                    native_time_index: Some(time_index as u64),
                    native_grid: Some(product_ready.grid.clone()),
                },
            );
        }
    }
    if entries.is_empty() {
        bail!("native product has no entries: {product} {source_run}");
    }
    if include_static && product_uses_static_elevation(product) {
        attach_static_elevation(
            coverage_root,
            ready,
            product_ready,
            &mut static_entries,
            &mut native_handles,
        )?;
    }

    let manifest_path = coverage_root.join("coverage.json");
    let bundle_handle = Arc::new(File::open(&manifest_path)?);
    let bundle_file = ManifestFile {
        path: "coverage.json".to_string(),
        bytes: manifest_path.metadata()?.len(),
        sha256: None,
        entries: Vec::new(),
    };
    let entries_by_source_run = HashMap::from([(
        source_run.to_string(),
        entries.values().cloned().collect::<Vec<_>>(),
    )]);
    let coverage_plan = meta
        .valid_times
        .iter()
        .map(|valid_time| CoveragePlanEntry {
            valid_time_utc: *valid_time,
            source_run: source_run.to_string(),
            forecast_hour: (*valid_time - reference_time).num_hours(),
        })
        .collect();
    Ok(ProductSnapshot {
        product: product.to_string(),
        product_root: coverage_root.to_path_buf(),
        manifest: ProductManifest {
            model: product.to_string(),
            coverage_id: format!("{}@{}", ready.coverage_id, source_run),
            status: "complete".to_string(),
            latest_complete_run: Some(source_run.to_string()),
            config_fingerprint: None,
            public_start_utc: Some(ready.public_start_utc),
            files: vec![bundle_file.clone()],
            coverage_plan,
        },
        bundle_file,
        bundle_path: manifest_path,
        bundle_handle,
        entries,
        entries_by_source_run,
        static_entries,
        native_handles,
    })
}

fn merge_native_support_run(current: &mut ProductSnapshot, support: ProductSnapshot) {
    // Native ECMWF production writes one immutable directory per source run,
    // whereas an official prepared coverage exposes all retained source runs
    // through one snapshot. Keep the latest run authoritative for overlapping
    // valid times, but retain older values and their file handles so ECMWF's
    // run-stitching interpolation can see the complete retained run stack.
    for (key, entry) in support.entries {
        current.entries.entry(key).or_insert(entry);
    }
    for (source_run, entries) in support.entries_by_source_run {
        current.entries_by_source_run.insert(source_run, entries);
    }
    for (variable, entry) in support.static_entries {
        current.static_entries.entry(variable).or_insert(entry);
    }
    current.native_handles.extend(support.native_handles);
    current
        .manifest
        .coverage_plan
        .extend(support.manifest.coverage_plan);
    current.manifest.coverage_plan.sort_by(|left, right| {
        left.valid_time_utc
            .cmp(&right.valid_time_utc)
            .then_with(|| left.source_run.cmp(&right.source_run))
            .then_with(|| left.forecast_hour.cmp(&right.forecast_hour))
    });
}

fn validate_ready(ready: &NativeReady, group: &str) -> Result<()> {
    if ready.group != group {
        bail!(
            "native marker group mismatch: expected {group}, got {}",
            ready.group
        );
    }
    let ecmwf_rolling_gust = group == "ecmwf"
        && ready.gust_support_run_count == Some(5)
        && ready.gust_support_max_forecast_hour == Some(186);
    let (expected_runs, expected_cadence_hours): (usize, &[i64]) = match group {
        "gfs" => (5, &[6, 6, 6, 6]),
        "cams" => (3, &[12, 12]),
        "cams_greenhouse" => (3, &[24, 24]),
        // ECMWF retains five bounded older gust runs, the previous complete
        // run, the adjacent short cycle, and the target complete run. This is
        // the source-run stack used by the official rolling gust database.
        "ecmwf" if ecmwf_rolling_gust => (8, &[12, 12, 12, 12, 12, 6, 6]),
        // One production restart must remain possible while the previous
        // immutable five-run coverage is still current. After the producer
        // atomically publishes the rolling-gust marker, reload switches to the
        // strict eight-run contract above without an alternate service/root.
        "ecmwf" => (5, &[6, 6, 6, 12]),
        _ => bail!("unsupported native group: {group}"),
    };
    if ready.source_runs.len() != expected_runs {
        bail!("native {group} marker must contain {expected_runs} source runs");
    }
    let parsed = ready
        .source_runs
        .iter()
        .map(|run| parse_run(run))
        .collect::<Result<Vec<_>>>()?;
    if parsed
        .windows(2)
        .zip(expected_cadence_hours)
        .any(|(pair, cadence_hours)| pair[1] - pair[0] != Duration::hours(*cadence_hours))
    {
        bail!("native {group} source runs do not match the retained cadence");
    }
    if ready.source_runs.last() != Some(&ready.latest_complete_run) {
        bail!("native latest_complete_run is not the final source run");
    }
    if group == "cams_greenhouse" && parsed.iter().any(|run| run.hour() != 0) {
        bail!("native CAMS greenhouse runs must use the daily 00 UTC cycle");
    }
    let expected_public_start = if ecmwf_rolling_gust {
        match (ready.generated_at, ready.local_utc_offset_hours) {
            (Some(generated_at), Some(offset_hours)) => {
                if !(-23..=23).contains(&offset_hours) {
                    bail!("native ECMWF local UTC offset is invalid");
                }
                let offset = Duration::hours(offset_hours);
                let local_midnight = (generated_at + offset)
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .context("native ECMWF local midnight is invalid")?;
                DateTime::<Utc>::from_naive_utc_and_offset(local_midnight, Utc) - offset
            }
            // Transitional markers published by the first local-day rollout
            // already contain generated_at and the UTC+8 public boundary, but
            // predate the explicit local_utc_offset_hours field. Accept that
            // one historical contract without weakening validation for new
            // markers; the producer now always writes the offset.
            (Some(generated_at), None) => {
                let offset = Duration::hours(8);
                let local_midnight = (generated_at + offset)
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .context("native ECMWF transitional local midnight is invalid")?;
                DateTime::<Utc>::from_naive_utc_and_offset(local_midnight, Utc) - offset
            }
            (None, None) => *parsed
                .last()
                .context("native ECMWF source run list is empty")?,
            (None, Some(_)) => bail!("native ECMWF local-day metadata is incomplete"),
        }
    } else {
        parsed[0]
    };
    if ready.public_start_utc != expected_public_start {
        bail!("native public_start_utc does not match the public run boundary");
    }
    if group == "gfs"
        && (ready.short_run_count != Some(3)
            || ready.full_run_count != Some(2)
            || ready.source_run_max_forecast_hours != [5, 5, 5, 384, 384])
    {
        bail!("native GFS marker must declare three short and two complete runs");
    }
    if group == "ecmwf"
        && !ecmwf_rolling_gust
        && (ready.short_run_count != Some(3)
            || ready.full_run_count != Some(2)
            || ready.source_run_max_forecast_hours != [6, 6, 6, 360, 360]
            || !ready.source_run_roles.is_empty()
            || ready.gust_support_run_count.is_some()
            || ready.gust_support_max_forecast_hour.is_some())
    {
        bail!("native ECMWF legacy production marker is invalid");
    }
    if ecmwf_rolling_gust
        && (ready.short_run_count != Some(1)
            || ready.full_run_count != Some(2)
            || ready.gust_support_run_count != Some(5)
            || ready.gust_support_max_forecast_hour != Some(186)
            || ready.source_run_max_forecast_hours != [186, 186, 186, 186, 186, 360, 6, 360]
            || ready.source_run_roles
                != [
                    "gust-support",
                    "gust-support",
                    "gust-support",
                    "gust-support",
                    "gust-support",
                    "previous-complete",
                    "short-history",
                    "target",
                ])
    {
        bail!("native ECMWF marker does not match the rolling gust support contract");
    }
    if group == "cams" && ready.products.contains_key("cams_global_greenhouse_gases") {
        if ready.greenhouse_source_runs.len() != 3 {
            bail!("native CAMS greenhouse marker must contain three source runs");
        }
        let greenhouse = ready
            .greenhouse_source_runs
            .iter()
            .map(|run| parse_run(run))
            .collect::<Result<Vec<_>>>()?;
        if greenhouse.iter().any(|run| run.hour() != 0)
            || greenhouse
                .windows(2)
                .any(|pair| pair[1] - pair[0] != Duration::hours(24))
        {
            bail!("native CAMS greenhouse runs must be consecutive daily 00 UTC runs");
        }
    }
    for (product, product_ready) in &ready.products {
        if product_ready.source_runs.is_empty()
            != product_ready.source_run_max_forecast_hours.is_empty()
        {
            bail!("native product {product} must declare source runs and horizons together");
        }
        if product_ready.source_runs.is_empty() {
            continue;
        }
        if product_ready.source_runs.len() != product_ready.source_run_max_forecast_hours.len() {
            bail!("native product {product} source run and horizon counts differ");
        }
        let product_runs = product_ready
            .source_runs
            .iter()
            .map(|run| parse_run(run))
            .collect::<Result<Vec<_>>>()?;
        if product_runs
            .windows(2)
            .any(|pair| pair[1] - pair[0] != Duration::hours(6))
        {
            bail!("native product {product} source runs are not consecutive");
        }
        if product_ready
            .source_run_max_forecast_hours
            .iter()
            .any(|horizon| *horizon < 0)
        {
            bail!("native product {product} contains a negative horizon");
        }
    }
    Ok(())
}

fn validate_dem(coverage_root: &Path, ready: &NativeReady) -> Result<()> {
    let Some(dem) = ready.static_sources.get("copernicus_dem90") else {
        return Ok(());
    };
    if dem.source != "copernicus_dem90"
        || dem.runtime_path != "copernicus_dem90/static"
        || dem.latitude_chunk_min != Some(0)
        || dem.latitude_chunk_max != Some(58)
        || dem.file_count != Some(59)
        || dem.bytes.is_some()
        || dem.sha256.is_some()
    {
        bail!("native Copernicus DEM90 contract does not match Singapore region");
    }
    let runtime_root = match dem.storage.as_deref() {
        None => coverage_root.to_path_buf(),
        Some("external_env") if dem.environment.as_deref() == Some("OM_DEM_ROOT") => {
            let root = std::env::var_os("OM_DEM_ROOT")
                .map(PathBuf::from)
                .context("external Copernicus DEM90 requires OM_DEM_ROOT")?;
            if !root.is_absolute() {
                bail!("OM_DEM_ROOT must be absolute for external Copernicus DEM90");
            }
            root
        }
        _ => bail!("unsupported native Copernicus DEM90 storage contract"),
    };
    for latitude in dem.latitude_chunk_min.unwrap()..=dem.latitude_chunk_max.unwrap() {
        let path = runtime_root
            .join(&dem.runtime_path)
            .join(format!("lat_{latitude}.om"));
        if !path.is_file() || path.metadata()?.len() == 0 {
            bail!(
                "required Copernicus DEM90 chunk is missing: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn validate_gfs_model_static(ready: &NativeReady) -> Result<()> {
    let expected = [
        (
            "ncep_gfs013_hsurf",
            GFS013_HSURF_RELATIVE_PATH,
            GFS013_HSURF_BYTES,
            GFS013_HSURF_SHA256,
        ),
        (
            "ncep_gfs025_hsurf",
            GFS025_HSURF_RELATIVE_PATH,
            GFS025_HSURF_BYTES,
            GFS025_HSURF_SHA256,
        ),
    ];
    let declared = expected
        .iter()
        .filter(|(key, _, _, _)| ready.static_sources.contains_key(*key))
        .count();
    if declared == 0 {
        // Backwards compatibility for immutable v3/v4 coverages, where HSURF
        // lived inside the coverage and only DEM90 appeared in this marker.
        return Ok(());
    }
    if declared != expected.len() {
        bail!("native GFS marker must declare both external model elevation files");
    }
    let root = std::env::var_os("OM_MODEL_STATIC_ROOT")
        .map(PathBuf::from)
        .context("external GFS model elevation requires OM_MODEL_STATIC_ROOT")?;
    if !root.is_absolute() {
        bail!("OM_MODEL_STATIC_ROOT must be absolute for external GFS model elevation");
    }
    for (key, relative_path, expected_bytes, expected_sha256) in expected {
        let source = ready
            .static_sources
            .get(key)
            .with_context(|| format!("native GFS marker has no {key} contract"))?;
        if source.source != key
            || source.runtime_path != relative_path
            || source.storage.as_deref() != Some("external_env")
            || source.environment.as_deref() != Some("OM_MODEL_STATIC_ROOT")
            || source.latitude_chunk_min.is_some()
            || source.latitude_chunk_max.is_some()
            || source.file_count != Some(1)
            || source.bytes != Some(expected_bytes)
            || source.sha256.as_deref() != Some(expected_sha256)
        {
            bail!("native GFS external model elevation contract is invalid for {key}");
        }
        let path = root.join(relative_path);
        if !path.is_file() || path.metadata()?.len() != expected_bytes {
            bail!(
                "native GFS external model elevation file is invalid: {}",
                path.display()
            );
        }
    }
    Ok(())
}

pub fn load_native_group_products(
    data_root: &Path,
    group: &str,
    group_products: &[&str],
    products: &mut HashMap<String, Arc<ProductSnapshot>>,
    historical_products: &mut HashMap<String, Vec<Arc<ProductSnapshot>>>,
) -> Result<bool> {
    let marker_path = data_root
        .join("groups")
        .join(group)
        .join("current")
        .join("ready_for_processing.json");
    if !marker_path.exists() {
        return Ok(false);
    }
    let ready: NativeReady = match serde_json::from_slice(&std::fs::read(&marker_path)?) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    if ready.runtime_format != "openmeteo-native-v1" {
        return Ok(false);
    }
    if ready.status != "complete" {
        return Ok(true);
    }
    validate_ready(&ready, group)?;
    let coverage_root = safe_relative_path(data_root, &ready.coverage_path)?.canonicalize()?;
    let expected_parent = data_root.join("coverages").join(group).canonicalize()?;
    if coverage_root.parent() != Some(expected_parent.as_path())
        || coverage_root.file_name().and_then(|value| value.to_str())
            != Some(ready.coverage_id.as_str())
    {
        bail!("native coverage path does not match marker identity");
    }
    // The ready marker is the authoritative atomic publication point.  The
    // convenience `current/<group>` symlink is updated immediately before the
    // marker, so a process starting in that tiny window must continue loading
    // the immutable coverage named by the old marker instead of failing startup.
    if group == "gfs" {
        validate_dem(&coverage_root, &ready)?;
        validate_gfs_model_static(&ready)?;
    }

    for product in group_products {
        let Some(product_ready) = ready.products.get(*product) else {
            continue;
        };
        let source_runs = product_source_runs(&ready, product, product_ready);
        let latest = source_runs
            .last()
            .with_context(|| format!("native product has no source runs: {product}"))?;
        let mut current =
            load_native_product_run(&coverage_root, &ready, product, product_ready, latest, true)
                .with_context(|| format!("load native current {product} {latest}"))?;

        if group == "ecmwf" {
            for source_run in source_runs[..source_runs.len() - 1].iter().rev() {
                let support = load_native_product_run(
                    &coverage_root,
                    &ready,
                    product,
                    product_ready,
                    source_run,
                    false,
                )
                .with_context(|| format!("load native support {product} {source_run}"))?;
                merge_native_support_run(&mut current, support);
            }
            products.insert((*product).to_string(), Arc::new(current));
            continue;
        }

        products.insert((*product).to_string(), Arc::new(current));

        let history = historical_products
            .entry((*product).to_string())
            .or_default();
        for source_run in source_runs[..source_runs.len() - 1].iter().rev() {
            let candidate = load_native_product_run(
                &coverage_root,
                &ready,
                product,
                product_ready,
                source_run,
                false,
            )
            .with_context(|| format!("load native history {product} {source_run}"))?;
            history.push(Arc::new(candidate));
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn attaches_regional_static_elevation_to_every_gfs_product() {
        assert!(product_uses_static_elevation("gfs013_surface"));
        assert!(product_uses_static_elevation("gfs025"));
        assert!(product_uses_static_elevation("gfs_pressure_profile"));
        assert!(!product_uses_static_elevation("cams_global"));
    }
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn write_fake_om(path: &Path, dimensions: [u64; 3]) {
        let mut file = File::create(path).unwrap();
        file.write_all(b"OM\x03").unwrap();
        file.seek(SeekFrom::Start(64)).unwrap();
        let mut root = Vec::new();
        root.extend([20, 1]);
        root.extend(0_u16.to_le_bytes());
        root.extend(0_u32.to_le_bytes());
        root.extend(128_u64.to_le_bytes());
        root.extend(256_u64.to_le_bytes());
        root.extend(3_u64.to_le_bytes());
        root.extend(10_f32.to_le_bytes());
        root.extend(0_f32.to_le_bytes());
        for value in dimensions {
            root.extend(value.to_le_bytes());
        }
        for value in [1_u64, dimensions[1], dimensions[2]] {
            root.extend(value.to_le_bytes());
        }
        file.write_all(&root).unwrap();
        file.seek(SeekFrom::Start(512)).unwrap();
        file.write_all(b"OM\x03\x00").unwrap();
        file.write_all(&0_u32.to_le_bytes()).unwrap();
        file.write_all(&64_u64.to_le_bytes()).unwrap();
        file.write_all(&(root.len() as u64).to_le_bytes()).unwrap();
    }

    #[test]
    fn maps_official_gfs_384_hour_schedule() {
        let reference_time = "2026-07-12T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let hours = (0..=120).chain((123..=384).step_by(3)).collect::<Vec<_>>();
        let meta = NativeRunMeta {
            reference_time,
            variables: vec!["temperature_2m".to_string()],
            valid_times: hours
                .iter()
                .map(|hour| reference_time + Duration::hours(*hour))
                .collect(),
        };
        assert_eq!(
            native_time_indices("ncep_gfs013", "temperature_2m", &meta, 209).unwrap(),
            (0..209).collect::<Vec<_>>()
        );
    }

    #[test]
    fn maps_dense_interpolated_gfs_384_hour_schedule() {
        let reference_time = "2026-07-12T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let meta = NativeRunMeta {
            reference_time,
            variables: vec!["temperature_2m".to_string()],
            valid_times: (0..=384)
                .map(|hour| reference_time + Duration::hours(hour))
                .collect(),
        };
        assert_eq!(
            native_time_indices("ncep_gfs013", "temperature_2m", &meta, 385).unwrap(),
            (0..385).collect::<Vec<_>>()
        );
        validate_run_time_axis("ncep_gfs013", &meta, 384).unwrap();
    }

    #[test]
    fn maps_sparse_cams_frames_to_three_hour_times() {
        let reference_time = "2026-07-12T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let meta = NativeRunMeta {
            reference_time,
            variables: vec!["dust".to_string()],
            valid_times: (0..=120)
                .map(|hour| reference_time + Duration::hours(hour))
                .collect(),
        };
        assert_eq!(
            native_time_indices("cams_global", "dust", &meta, 41).unwrap(),
            (0..=120).step_by(3).collect::<Vec<_>>()
        );
    }

    #[test]
    fn loads_independent_cams_greenhouse_source_runs() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let coverage_id = "cams_greenhouse_native_2026073000_independent-v1";
        let coverage = root.join("coverages/cams_greenhouse").join(coverage_id);
        fs::create_dir_all(&coverage).unwrap();
        fs::write(coverage.join("coverage.json"), b"{}").unwrap();
        let source_runs = ["2026072800", "2026072900", "2026073000"];
        let forecast_hours = expected_forecast_hours("cams_global_greenhouse_gases", 120);
        for run in source_runs {
            let reference = parse_run(run).unwrap();
            let run_root = coverage
                .join("data_run/cams_global_greenhouse_gases")
                .join(run_relative_path(run).unwrap());
            fs::create_dir_all(&run_root).unwrap();
            write_fake_om(
                &run_root.join("carbon_monoxide.om"),
                [2, 3, forecast_hours.len() as u64],
            );
            fs::write(
                run_root.join("meta.json"),
                serde_json::to_vec(&json!({
                    "reference_time": reference,
                    "variables": ["carbon_monoxide"],
                    "valid_times": forecast_hours
                        .iter()
                        .map(|hour| reference + Duration::hours(*hour))
                        .collect::<Vec<_>>(),
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let marker = json!({
            "status": "complete",
            "runtime_format": "openmeteo-native-v1",
            "group": "cams_greenhouse",
            "coverage_id": coverage_id,
            "latest_complete_run": "2026073000",
            "source_runs": source_runs,
            "public_start_utc": "2026-07-28T00:00:00Z",
            "coverage_path": format!("coverages/cams_greenhouse/{coverage_id}"),
            "products": {
                "cams_global_greenhouse_gases": {
                    "runtime_domain": "cams_global_greenhouse_gases",
                    "grid": {
                        "nx": 3, "ny": 2,
                        "lon_min": 70.0, "lat_min": 0.0,
                        "dx": 0.1, "dy": 0.1,
                        "dt_seconds": 10800,
                        "om_file_length": 72
                    }
                }
            }
        });
        let marker_path = root.join("groups/cams_greenhouse/current/ready_for_processing.json");
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();

        let mut products = HashMap::new();
        let mut history = HashMap::new();
        assert!(load_native_group_products(
            root,
            "cams_greenhouse",
            &["cams_global_greenhouse_gases"],
            &mut products,
            &mut history,
        )
        .unwrap());

        let current = products.get("cams_global_greenhouse_gases").unwrap();
        assert_eq!(
            current.manifest.latest_complete_run.as_deref(),
            Some("2026073000")
        );
        assert_eq!(current.entries.len(), 41);
        assert_eq!(
            history["cams_global_greenhouse_gases"]
                .iter()
                .map(|candidate| candidate.manifest.latest_complete_run.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["2026072900", "2026072800"]
        );
    }

    #[test]
    fn maps_official_ecmwf_gust_gap_without_relaxing_other_variables() {
        let reference_time = "2026-07-31T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let hours = expected_forecast_hours("ecmwf_ifs025", 360);
        let meta = NativeRunMeta {
            reference_time,
            variables: vec!["wind_gusts_10m".to_string()],
            valid_times: hours
                .iter()
                .map(|hour| reference_time + Duration::hours(*hour))
                .collect(),
        };
        let expected = hours
            .iter()
            .enumerate()
            .filter_map(|(index, forecast_hour)| {
                (*forecast_hour > 0 && (*forecast_hour <= 90 || *forecast_hour >= 150))
                    .then_some(index)
            })
            .collect::<Vec<_>>();

        assert_eq!(expected.len(), 66);
        assert_eq!(
            native_time_indices("ecmwf_ifs025", "wind_gusts_10m", &meta, 66).unwrap(),
            expected
        );
        assert!(native_time_indices("ecmwf_ifs025", "temperature_2m", &meta, 66).is_err());
    }

    #[test]
    fn validates_native_probability_source_schedules() {
        assert_eq!(expected_forecast_hours("ncep_gefs025", 6), vec![3, 6]);
        assert_eq!(expected_forecast_hours("ncep_gefs05", 6), vec![3, 6]);
        assert_eq!(
            expected_forecast_hours("ncep_gefs025", 240),
            (3..=240).step_by(3).collect::<Vec<_>>()
        );
        assert_eq!(
            expected_forecast_hours("ncep_gefs05", 384),
            (3..240)
                .step_by(3)
                .chain((240..=384).step_by(6))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            expected_forecast_hours("ecmwf_ifs025", 360),
            (0..=144)
                .step_by(3)
                .chain((150..=360).step_by(6))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            expected_forecast_hours("ecmwf_ifs025_ensemble", 144),
            (3..=144).step_by(3).collect::<Vec<_>>()
        );
    }

    #[test]
    fn product_specific_runs_override_group_history() {
        let ready: NativeReady = serde_json::from_value(json!({
            "status": "complete",
            "runtime_format": "openmeteo-native-v1",
            "group": "gfs",
            "coverage_id": "gfs_native_2026073000",
            "latest_complete_run": "2026073000",
            "source_runs": [
                "2026072900",
                "2026072906",
                "2026072912",
                "2026072918",
                "2026073000"
            ],
            "public_start_utc": "2026-07-29T00:00:00Z",
            "coverage_path": "coverages/gfs/gfs_native_2026073000",
            "products": {
                "ncep_gefs025": {
                    "runtime_domain": "ncep_gefs025",
                    "grid": {
                        "grid_type": "regional_regular_lat_lon",
                        "nx": 289,
                        "ny": 241,
                        "lon_min": 69.0,
                        "lat_min": -1.0,
                        "dx": 0.25,
                        "dy": 0.25,
                        "dt_seconds": 10800,
                        "om_file_length": 481
                    },
                    "source_runs": ["2026073000"],
                    "source_run_max_forecast_hours": [240]
                }
            },
            "source_run_max_forecast_hours": [5, 5, 5, 384, 384]
        }))
        .unwrap();
        let product = ready.products.get("ncep_gefs025").unwrap();

        assert_eq!(
            product_source_runs(&ready, "ncep_gefs025", product),
            ["2026073000"]
        );
        assert_eq!(
            run_horizon(&ready, "ncep_gefs025", product, "2026073000").unwrap(),
            240
        );
    }

    #[test]
    fn ecmwf_deterministic_runs_use_group_specific_horizons() {
        let ready: NativeReady = serde_json::from_value(json!({
            "status": "complete",
            "runtime_format": "openmeteo-native-v1",
            "group": "ecmwf",
            "coverage_id": "ecmwf_native_2026073000",
            "latest_complete_run": "2026073000",
            "source_runs": [
                "2026072700",
                "2026072712",
                "2026072800",
                "2026072812",
                "2026072900",
                "2026072912",
                "2026072918",
                "2026073000"
            ],
            "source_run_roles": [
                "gust-support",
                "gust-support",
                "gust-support",
                "gust-support",
                "gust-support",
                "previous-complete",
                "short-history",
                "target"
            ],
            "public_start_utc": "2026-07-30T00:00:00Z",
            "coverage_path": "coverages/ecmwf/ecmwf_native_2026073000",
            "short_run_count": 1,
            "full_run_count": 2,
            "gust_support_run_count": 5,
            "gust_support_max_forecast_hour": 186,
            "products": {
                "ecmwf_ifs025": {
                    "runtime_domain": "ecmwf_ifs025",
                    "grid": {
                        "grid_type": "regional_regular_lat_lon",
                        "nx": 297,
                        "ny": 249,
                        "lon_min": 68.0,
                        "lat_min": -2.0,
                        "dx": 0.25,
                        "dy": 0.25,
                        "dt_seconds": 10800,
                        "om_file_length": 104
                    }
                }
            },
            "source_run_max_forecast_hours": [186, 186, 186, 186, 186, 360, 6, 360]
        }))
        .unwrap();
        let product = ready.products.get("ecmwf_ifs025").unwrap();

        assert_eq!(
            run_horizon(&ready, "ecmwf_ifs025", product, "2026072700").unwrap(),
            186
        );
        assert_eq!(
            run_horizon(&ready, "ecmwf_ifs025", product, "2026072912").unwrap(),
            360
        );
        assert_eq!(
            run_horizon(&ready, "ecmwf_ifs025", product, "2026072918").unwrap(),
            6
        );
        assert_eq!(
            run_horizon(&ready, "ecmwf_ifs025", product, "2026073000").unwrap(),
            360
        );
        validate_ready(&ready, "ecmwf").unwrap();
    }

    #[test]
    fn accepts_only_the_exact_previous_ecmwf_production_marker_during_rollover() {
        let ready: NativeReady = serde_json::from_value(json!({
            "status": "complete",
            "runtime_format": "openmeteo-native-v1",
            "group": "ecmwf",
            "coverage_id": "ecmwf_native_2026073012_7ea06487fb66",
            "latest_complete_run": "2026073012",
            "source_runs": [
                "2026072906",
                "2026072912",
                "2026072918",
                "2026073000",
                "2026073012"
            ],
            "source_run_max_forecast_hours": [6, 6, 6, 360, 360],
            "public_start_utc": "2026-07-29T06:00:00Z",
            "coverage_path": "coverages/ecmwf/ecmwf_native_2026073012_7ea06487fb66",
            "short_run_count": 3,
            "full_run_count": 2,
            "products": {}
        }))
        .unwrap();

        validate_ready(&ready, "ecmwf").unwrap();
    }

    #[test]
    fn deserializes_heterogeneous_external_static_contracts() {
        let ready: NativeReady = serde_json::from_value(json!({
            "status": "complete",
            "runtime_format": "openmeteo-native-v1",
            "group": "gfs",
            "coverage_id": "gfs_native_2026071300",
            "latest_complete_run": "2026071300",
            "source_runs": [
                "2026071200",
                "2026071206",
                "2026071212",
                "2026071218",
                "2026071300"
            ],
            "public_start_utc": "2026-07-12T00:00:00Z",
            "coverage_path": "coverages/gfs/gfs_native_2026071300",
            "products": {},
            "static_sources": {
                "copernicus_dem90": {
                    "source": "copernicus_dem90",
                    "runtime_path": "copernicus_dem90/static",
                    "storage": "external_env",
                    "environment": "OM_DEM_ROOT",
                    "latitude_chunk_min": 0,
                    "latitude_chunk_max": 58,
                    "file_count": 59
                },
                "ncep_gfs013_hsurf": {
                    "source": "ncep_gfs013_hsurf",
                    "runtime_path": GFS013_HSURF_RELATIVE_PATH,
                    "storage": "external_env",
                    "environment": "OM_MODEL_STATIC_ROOT",
                    "file_count": 1,
                    "bytes": GFS013_HSURF_BYTES,
                    "sha256": GFS013_HSURF_SHA256
                },
                "ncep_gfs025_hsurf": {
                    "source": "ncep_gfs025_hsurf",
                    "runtime_path": GFS025_HSURF_RELATIVE_PATH,
                    "storage": "external_env",
                    "environment": "OM_MODEL_STATIC_ROOT",
                    "file_count": 1,
                    "bytes": GFS025_HSURF_BYTES,
                    "sha256": GFS025_HSURF_SHA256
                }
            }
        }))
        .unwrap();

        let dem = ready.static_sources.get("copernicus_dem90").unwrap();
        assert_eq!(dem.latitude_chunk_min, Some(0));
        assert_eq!(dem.latitude_chunk_max, Some(58));
        assert_eq!(dem.bytes, None);
        let gfs013 = ready.static_sources.get("ncep_gfs013_hsurf").unwrap();
        assert_eq!(gfs013.latitude_chunk_min, None);
        assert_eq!(gfs013.bytes, Some(GFS013_HSURF_BYTES));
        assert_eq!(gfs013.sha256.as_deref(), Some(GFS013_HSURF_SHA256));
    }

    #[test]
    fn loads_three_short_and_two_complete_gfs_snapshots_in_fallback_order() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let coverage_id = "gfs_native_2026071300";
        let coverage = root.join("coverages/gfs").join(coverage_id);
        fs::create_dir_all(&coverage).unwrap();
        fs::write(coverage.join("coverage.json"), b"{}").unwrap();
        let source_runs = [
            "2026071200",
            "2026071206",
            "2026071212",
            "2026071218",
            "2026071300",
        ];
        let horizons = [5_i64, 5, 5, 384, 384];
        for (run, horizon) in source_runs.iter().zip(horizons) {
            let reference = parse_run(run).unwrap();
            let forecast_hours = expected_forecast_hours("ncep_gfs013", horizon);
            let run_root = coverage
                .join("data_run/ncep_gfs013")
                .join(run_relative_path(run).unwrap());
            fs::create_dir_all(&run_root).unwrap();
            write_fake_om(
                &run_root.join("temperature_2m.om"),
                [2, 3, forecast_hours.len() as u64],
            );
            fs::write(
                run_root.join("meta.json"),
                serde_json::to_vec(&json!({
                    "reference_time": reference,
                    "variables": ["temperature_2m"],
                    "valid_times": forecast_hours
                        .iter()
                        .map(|hour| reference + Duration::hours(*hour))
                        .collect::<Vec<_>>(),
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let marker = json!({
            "status": "complete",
            "runtime_format": "openmeteo-native-v1",
            "group": "gfs",
            "coverage_id": coverage_id,
            "latest_complete_run": "2026071300",
            "source_runs": source_runs,
            "short_run_count": 3,
            "full_run_count": 2,
            "source_run_max_forecast_hours": horizons,
            "public_start_utc": "2026-07-12T00:00:00Z",
            "coverage_path": format!("coverages/gfs/{coverage_id}"),
            "products": {
                "gfs013_surface": {
                    "runtime_domain": "ncep_gfs013",
                    "grid": {
                        "nx": 3, "ny": 2,
                        "lon_min": 70.0, "lat_min": 0.0,
                        "dx": 0.25, "dy": 0.25,
                        "dt_seconds": 3600,
                        "om_file_length": 481
                    }
                }
            }
        });
        let marker_path = root.join("groups/gfs/current/ready_for_processing.json");
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        fs::create_dir_all(root.join("current")).unwrap();
        symlink(&coverage, root.join("current/gfs")).unwrap();

        let mut products = HashMap::new();
        let mut history = HashMap::new();
        assert!(load_native_group_products(
            root,
            "gfs",
            &["gfs013_surface"],
            &mut products,
            &mut history,
        )
        .unwrap());

        let current = products.get("gfs013_surface").unwrap();
        assert_eq!(
            current.manifest.latest_complete_run.as_deref(),
            Some("2026071300")
        );
        assert_eq!(current.entries.len(), 209);
        assert_eq!(current.manifest.coverage_plan.len(), 209);
        assert_eq!(
            current
                .manifest
                .coverage_plan
                .first()
                .map(|entry| entry.forecast_hour),
            Some(0)
        );
        assert_eq!(
            current
                .manifest
                .coverage_plan
                .last()
                .map(|entry| entry.forecast_hour),
            Some(384)
        );
        let candidates = history.get("gfs013_surface").unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.manifest.latest_complete_run.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["2026071218", "2026071212", "2026071206", "2026071200"]
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| {
                    candidate
                        .entries
                        .values()
                        .map(|entry| entry.forecast_hour)
                        .max()
                        .unwrap()
                })
                .collect::<Vec<_>>(),
            [384, 5, 5, 5]
        );
    }

    #[test]
    fn merges_ecmwf_native_runs_into_one_stitchable_snapshot() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let coverage_id = "ecmwf_native_2026073012";
        let coverage = root.join("coverages/ecmwf").join(coverage_id);
        fs::create_dir_all(&coverage).unwrap();
        fs::write(coverage.join("coverage.json"), b"{}").unwrap();
        let source_runs = [
            "2026072712",
            "2026072800",
            "2026072812",
            "2026072900",
            "2026072912",
            "2026073000",
            "2026073006",
            "2026073012",
        ];
        let horizons = [186_i64, 186, 186, 186, 186, 360, 6, 360];
        for (run, horizon) in source_runs.iter().zip(horizons) {
            let reference = parse_run(run).unwrap();
            let forecast_hours = expected_forecast_hours("ecmwf_ifs025", horizon);
            let run_root = coverage
                .join("data_run/ecmwf_ifs025")
                .join(run_relative_path(run).unwrap());
            fs::create_dir_all(&run_root).unwrap();
            let stored_time_count = if horizon == 186 {
                forecast_hours.len()
            } else {
                forecast_hours.len() - 1
            };
            write_fake_om(
                &run_root.join("wind_gusts_10m.om"),
                [2, 3, stored_time_count as u64],
            );
            fs::write(
                run_root.join("meta.json"),
                serde_json::to_vec(&json!({
                    "reference_time": reference,
                    "variables": ["wind_gusts_10m"],
                    "valid_times": forecast_hours
                        .iter()
                        .map(|hour| reference + Duration::hours(*hour))
                        .collect::<Vec<_>>(),
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let marker = json!({
            "status": "complete",
            "runtime_format": "openmeteo-native-v1",
            "group": "ecmwf",
            "coverage_id": coverage_id,
            "latest_complete_run": "2026073012",
            "source_runs": source_runs,
            "source_run_roles": [
                "gust-support",
                "gust-support",
                "gust-support",
                "gust-support",
                "gust-support",
                "previous-complete",
                "short-history",
                "target"
            ],
            "short_run_count": 1,
            "full_run_count": 2,
            "gust_support_run_count": 5,
            "gust_support_max_forecast_hour": 186,
            "source_run_max_forecast_hours": horizons,
            "public_start_utc": "2026-07-30T16:00:00Z",
            "generated_at": "2026-07-30T20:00:00Z",
            "local_utc_offset_hours": 8,
            "coverage_path": format!("coverages/ecmwf/{coverage_id}"),
            "products": {
                "ecmwf_ifs025": {
                    "runtime_domain": "ecmwf_ifs025",
                    "grid": {
                        "nx": 3, "ny": 2,
                        "lon_min": 70.0, "lat_min": 0.0,
                        "dx": 0.25, "dy": 0.25,
                        "dt_seconds": 10800,
                        "om_file_length": 121
                    }
                }
            }
        });
        let marker_path = root.join("groups/ecmwf/current/ready_for_processing.json");
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();

        let mut products = HashMap::new();
        let mut history = HashMap::new();
        assert!(load_native_group_products(
            root,
            "ecmwf",
            &["ecmwf_ifs025"],
            &mut products,
            &mut history,
        )
        .unwrap());

        let current = products.get("ecmwf_ifs025").unwrap();
        let mut loaded_runs = current
            .entries_by_source_run
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        loaded_runs.sort_unstable();
        assert_eq!(loaded_runs, source_runs);
        assert_eq!(current.manifest.coverage_plan.len(), 358);
        assert_eq!(current.native_handles.len(), source_runs.len());
        assert!(!history.contains_key("ecmwf_ifs025"));

        let mut transitional_marker = marker;
        transitional_marker
            .as_object_mut()
            .unwrap()
            .remove("local_utc_offset_hours");
        let transitional_ready: NativeReady = serde_json::from_value(transitional_marker).unwrap();
        validate_ready(&transitional_ready, "ecmwf").unwrap();

        let latest_time = parse_run("2026073012").unwrap();
        let latest = current
            .entries
            .get(&EntryKey {
                variable: "wind_gusts_10m".to_string(),
                valid_time_utc: latest_time,
            })
            .unwrap();
        assert_eq!(latest.source_run, "2026073006");
        let oldest_time = parse_run("2026072712").unwrap() + Duration::hours(3);
        let oldest = current
            .entries
            .get(&EntryKey {
                variable: "wind_gusts_10m".to_string(),
                valid_time_utc: oldest_time,
            })
            .unwrap();
        assert_eq!(oldest.source_run, "2026072712");
    }
}
