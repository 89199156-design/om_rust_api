use crate::manifest::{
    ArrayMetadata, BundleEntry, EntryKey, ManifestFile, NativeGridMetadata, ProductManifest,
    ProductSnapshot,
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
}

#[derive(Debug, Deserialize)]
struct NativeProductReady {
    runtime_domain: String,
    grid: NativeGridMetadata,
}

#[derive(Debug, Deserialize)]
struct NativeStaticSourceReady {
    source: String,
    runtime_path: String,
    latitude_chunk_min: i32,
    latitude_chunk_max: i32,
    file_count: usize,
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
    let expected = expected_forecast_hours(runtime_domain, max_forecast_hour)
        .into_iter()
        .map(|hour| meta.reference_time + Duration::hours(hour))
        .collect::<Vec<_>>();
    if meta.valid_times != expected {
        bail!(
            "native run time axis does not match {} 0...{}h contract",
            runtime_domain,
            max_forecast_hour
        );
    }
    Ok(())
}

fn run_horizon(ready: &NativeReady, product: &str, source_run: &str) -> Result<i64> {
    if product == "cams_global_greenhouse_gases" {
        return Ok(120);
    }
    let index = ready
        .source_runs
        .iter()
        .position(|run| run == source_run)
        .with_context(|| format!("source run is not declared by marker: {source_run}"))?;
    if ready.group == "gfs" {
        return ready
            .source_run_max_forecast_hours
            .get(index)
            .copied()
            .context("GFS marker has no horizon for source run");
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
        "gfs013_surface" | "gfs025" | "gfs_pressure_profile"
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
        run_horizon(ready, product, source_run)?,
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
            &meta,
            usize::try_from(array.dimensions[2])?,
        )?;
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
        },
        bundle_file,
        bundle_path: manifest_path,
        bundle_handle,
        entries,
        static_entries,
        native_handles,
    })
}

fn validate_ready(ready: &NativeReady, group: &str) -> Result<()> {
    if ready.group != group {
        bail!(
            "native marker group mismatch: expected {group}, got {}",
            ready.group
        );
    }
    let (expected_runs, cadence_hours) = match group {
        "gfs" => (5, 6),
        "cams" => (3, 12),
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
        .any(|pair| pair[1] - pair[0] != Duration::hours(cadence_hours))
    {
        bail!("native {group} source runs are not consecutive");
    }
    if ready.source_runs.last() != Some(&ready.latest_complete_run) {
        bail!("native latest_complete_run is not the final source run");
    }
    if ready.public_start_utc != parsed[0] {
        bail!("native public_start_utc is not the oldest retained source run");
    }
    if group == "gfs"
        && (ready.short_run_count != Some(3)
            || ready.full_run_count != Some(2)
            || ready.source_run_max_forecast_hours != [5, 5, 5, 384, 384])
    {
        bail!("native GFS marker must declare three short and two complete runs");
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
    Ok(())
}

fn validate_dem(coverage_root: &Path, ready: &NativeReady) -> Result<()> {
    let Some(dem) = ready.static_sources.get("copernicus_dem90") else {
        return Ok(());
    };
    if dem.source != "copernicus_dem90"
        || dem.runtime_path != "copernicus_dem90/static"
        || dem.latitude_chunk_min != 0
        || dem.latitude_chunk_max != 58
        || dem.file_count != 59
    {
        bail!("native Copernicus DEM90 contract does not match Singapore region");
    }
    for latitude in dem.latitude_chunk_min..=dem.latitude_chunk_max {
        let path = coverage_root
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
    if data_root.join("current").join(group).canonicalize()? != coverage_root {
        bail!("native current pointer does not match ready marker");
    }
    if group == "gfs" {
        validate_dem(&coverage_root, &ready)?;
    }

    for product in group_products {
        let Some(product_ready) = ready.products.get(*product) else {
            continue;
        };
        let source_runs = if *product == "cams_global_greenhouse_gases" {
            &ready.greenhouse_source_runs
        } else {
            &ready.source_runs
        };
        let latest = source_runs
            .last()
            .with_context(|| format!("native product has no source runs: {product}"))?;
        let current =
            load_native_product_run(&coverage_root, &ready, product, product_ready, latest, true)
                .with_context(|| format!("load native current {product} {latest}"))?;
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
            native_time_indices("ncep_gfs013", &meta, 209).unwrap(),
            (0..209).collect::<Vec<_>>()
        );
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
            native_time_indices("cams_global", &meta, 41).unwrap(),
            (0..=120).step_by(3).collect::<Vec<_>>()
        );
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
}
