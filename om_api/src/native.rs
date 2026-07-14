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
}

#[derive(Debug, Deserialize)]
struct NativeProductReady {
    runtime_domain: String,
    grid: NativeGridMetadata,
}

#[derive(Debug, Deserialize)]
struct FullRunMeta {
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
    let values = Vec::<String>::deserialize(deserializer)?;
    values
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
    let mut out = vec![0_u8; size];
    file.read_exact_at(&mut out, offset)?;
    Ok(out)
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
    let mut out = root.to_path_buf();
    for component in path.components() {
        match component {
            Component::Normal(value) => out.push(value),
            _ => bail!("unsafe native coverage path: {}", relative),
        }
    }
    Ok(out)
}

fn run_relative_path(run: &str) -> Result<PathBuf> {
    let parsed = DateTime::parse_from_str(&format!("{run}00 +0000"), "%Y%m%d%H%M %z")?;
    Ok(PathBuf::from(parsed.format("%Y/%m/%d/%H00Z").to_string()))
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
    meta: &FullRunMeta,
    stored_time_count: usize,
) -> Result<Vec<usize>> {
    if stored_time_count == 0 || meta.valid_times.is_empty() {
        bail!("native OM time axis must not be empty");
    }
    if stored_time_count == meta.valid_times.len() {
        // GFS meta.json already carries the official sparse 0..120 hourly /
        // 123..384 three-hourly axis. Hourly CAMS variables also land here.
        return Ok((0..stored_time_count).collect());
    }
    if stored_time_count + 1 == meta.valid_times.len()
        && meta.valid_times.first() == Some(&meta.reference_time)
    {
        // Several GFS accumulated/flux/cloud variables intentionally skip
        // forecast hour zero. Their first stored frame belongs to meta index 1.
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
        bail!(
            "unsupported sparse native time axis for domain {}",
            runtime_domain
        );
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

fn load_native_product(
    coverage_root: &Path,
    ready: &NativeReady,
    product: &str,
    product_ready: &NativeProductReady,
) -> Result<ProductSnapshot> {
    let mut entries = HashMap::new();
    let mut static_entries = HashMap::new();
    let mut native_handles = HashMap::new();
    let source_runs = if product == "cams_global_greenhouse_gases" {
        &ready.greenhouse_source_runs
    } else {
        &ready.source_runs
    };
    let latest_source_run = source_runs
        .last()
        .with_context(|| format!("native product has no source runs: {product}"))?;
    let latest_reference =
        DateTime::parse_from_str(&format!("{}00 +0000", latest_source_run), "%Y%m%d%H%M %z")?
            .with_timezone(&Utc);
    for source_run in source_runs {
        let run_relative = run_relative_path(source_run)?;
        let run_root = coverage_root
            .join("data_run")
            .join(&product_ready.runtime_domain)
            .join(&run_relative);
        let meta_path = run_root.join("meta.json");
        let meta: FullRunMeta = serde_json::from_slice(
            &std::fs::read(&meta_path)
                .with_context(|| format!("read native run metadata {}", meta_path.display()))?,
        )?;
        let expected_reference =
            DateTime::parse_from_str(&format!("{source_run}00 +0000"), "%Y%m%d%H%M %z")?
                .with_timezone(&Utc);
        if meta.reference_time != expected_reference {
            bail!("native run reference time mismatch: {}", source_run);
        }
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
                    "native OM dimensions do not match grid metadata: {}",
                    file_path.display()
                );
            }
            let time_indices = native_time_indices(
                &product_ready.runtime_domain,
                &meta,
                usize::try_from(array.dimensions[2])?,
            )
            .with_context(|| format!("map native OM time axis {}", file_path.display()))?;
            let relative_file = file_path
                .strip_prefix(coverage_root)?
                .to_string_lossy()
                .replace('\\', "/");
            native_handles.insert(relative_file.clone(), handle);
            for (time_index, valid_time_index) in time_indices.into_iter().enumerate() {
                let valid_time = meta.valid_times[valid_time_index];
                let forecast_hour = (valid_time - meta.reference_time).num_hours();
                let key = EntryKey {
                    variable: variable.clone(),
                    valid_time_utc: valid_time,
                };
                let replace = should_replace_overlapping_native_entry(
                    &product_ready.runtime_domain,
                    valid_time,
                    latest_reference,
                    entries.contains_key(&key),
                );
                if replace {
                    entries.insert(
                        key,
                        BundleEntry {
                            variable: variable.clone(),
                            variable_path: Some(variable.clone()),
                            valid_time_utc: valid_time,
                            source_run: source_run.clone(),
                            forecast_hour,
                            source_url: None,
                            selection_ranges: vec![
                                [0, product_ready.grid.ny],
                                [0, product_ready.grid.nx],
                            ],
                            array: array.clone(),
                            lut_byte_ranges: Vec::new(),
                            data_byte_ranges: Vec::new(),
                            lut_bytes_read: 0,
                            byte_ranges: Vec::new(),
                            bundle_offset: 0,
                            bundle_bytes: file_path.metadata()?.len(),
                            native_file_path: Some(relative_file.clone()),
                            native_time_index: Some(time_index as u64),
                            native_grid: Some(product_ready.grid.clone()),
                        },
                    );
                }
            }
        }
    }
    if entries.is_empty() {
        bail!("native product has no entries: {}", product);
    }
    if product == "gfs013_surface" {
        let static_path = coverage_root
            .join(&product_ready.runtime_domain)
            .join("static")
            .join("HSURF.om");
        if static_path.exists() {
            let handle = Arc::new(File::open(&static_path).with_context(|| {
                format!("open native static OM file {}", static_path.display())
            })?);
            let array = read_native_array_metadata(&handle).with_context(|| {
                format!("parse native static OM file {}", static_path.display())
            })?;
            if array.dimensions != [product_ready.grid.ny, product_ready.grid.nx]
                || array.chunks.len() != 2
            {
                bail!(
                    "native surface elevation dimensions do not match grid metadata: {}",
                    static_path.display()
                );
            }
            let relative_file = static_path
                .strip_prefix(coverage_root)?
                .to_string_lossy()
                .replace('\\', "/");
            native_handles.insert(relative_file.clone(), handle);
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
                    bundle_bytes: static_path.metadata()?.len(),
                    native_file_path: Some(relative_file),
                    native_time_index: None,
                    native_grid: Some(product_ready.grid.clone()),
                },
            );
        }
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
            coverage_id: ready.coverage_id.clone(),
            status: "complete".to_string(),
            latest_complete_run: Some(ready.latest_complete_run.clone()),
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

fn should_replace_overlapping_native_entry(
    runtime_domain: &str,
    valid_time: DateTime<Utc>,
    latest_reference: DateTime<Utc>,
    already_present: bool,
) -> bool {
    // Shanghai's current CAMS coverage starts at the latest run and resolves
    // the pre-run AQI window from the retained coverage with the same cycle
    // on the previous day. Singapore retains an additional 12-hour run for
    // availability, but that middle run must not overwrite this history or
    // the 24-hour rolling AQI values diverge. At and after initialization the
    // ascending load order still makes the latest run authoritative.
    if runtime_domain == "cams_global" && valid_time < latest_reference {
        !already_present
    } else {
        true
    }
}

pub fn load_native_group_products(
    data_root: &Path,
    group: &str,
    group_products: &[&str],
    products: &mut HashMap<String, Arc<ProductSnapshot>>,
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
    if ready.group != group {
        bail!(
            "native marker group mismatch: expected {}, got {}",
            group,
            ready.group
        );
    }
    let (expected_runs, cadence_hours) = match group {
        "gfs" => (5, 6),
        "cams" => (3, 12),
        _ => bail!("unsupported native group: {}", group),
    };
    if ready.source_runs.len() != expected_runs {
        bail!(
            "native {} marker must contain {} source runs",
            group,
            expected_runs
        );
    }
    let parsed_runs = ready
        .source_runs
        .iter()
        .map(|run| {
            DateTime::parse_from_str(&format!("{run}00 +0000"), "%Y%m%d%H%M %z")
                .map(|value| value.with_timezone(&Utc))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if parsed_runs
        .windows(2)
        .any(|pair| pair[1] - pair[0] != Duration::hours(cadence_hours))
    {
        bail!("native {} source runs are not consecutive", group);
    }
    if ready.source_runs.last() != Some(&ready.latest_complete_run) {
        bail!("native latest_complete_run is not the final source run");
    }
    if group == "cams" && ready.products.contains_key("cams_global_greenhouse_gases") {
        if ready.greenhouse_source_runs.len() != 3 {
            bail!("native CAMS greenhouse marker must contain 3 source runs");
        }
        let greenhouse_runs = ready
            .greenhouse_source_runs
            .iter()
            .map(|run| {
                DateTime::parse_from_str(&format!("{run}00 +0000"), "%Y%m%d%H%M %z")
                    .map(|value| value.with_timezone(&Utc))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if greenhouse_runs.iter().any(|run| run.hour() != 0)
            || greenhouse_runs
                .windows(2)
                .any(|pair| pair[1] - pair[0] != Duration::hours(24))
        {
            bail!("native CAMS greenhouse source runs are not consecutive daily 00 UTC runs");
        }
        let expected_latest = parsed_runs
            .last()
            .context("native CAMS marker has no latest source run")?
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .context("build CAMS greenhouse latest reference")?
            .and_utc();
        if greenhouse_runs.last() != Some(&expected_latest) {
            bail!("native CAMS greenhouse latest run does not match CAMS day");
        }
    }
    if ready.public_start_utc != parsed_runs[0] {
        bail!("native public_start_utc is not the oldest retained source run");
    }
    let coverage_root = safe_relative_path(data_root, &ready.coverage_path)?.canonicalize()?;
    let expected_parent = data_root.join("coverages").join(group).canonicalize()?;
    if coverage_root.parent() != Some(expected_parent.as_path()) {
        bail!("native coverage resolves outside group root");
    }
    if coverage_root.file_name().and_then(|value| value.to_str())
        != Some(ready.coverage_id.as_str())
    {
        bail!("native coverage id does not match coverage path");
    }
    let current = data_root.join("current").join(group).canonicalize()?;
    if current != coverage_root {
        bail!("native current pointer does not match ready marker");
    }
    for product in group_products {
        let Some(product_ready) = ready.products.get(*product) else {
            continue;
        };
        let snapshot = load_native_product(&coverage_root, &ready, product, product_ready)
            .with_context(|| format!("load native product {}", product))?;
        products.insert((*product).to_string(), Arc::new(snapshot));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use tempfile::NamedTempFile;
    use tempfile::TempDir;

    #[test]
    fn full_run_meta_accepts_open_meteo_minute_precision_valid_times() {
        let meta: FullRunMeta = serde_json::from_str(
            r#"{
                "reference_time": "2026-07-11T18:00:00Z",
                "variables": ["temperature_2m"],
                "valid_times": ["2026-07-11T18:00Z", "2026-07-11T19:00:00Z"]
            }"#,
        )
        .unwrap();

        assert_eq!(meta.valid_times[0], meta.reference_time);
        assert_eq!(
            meta.valid_times[1] - meta.valid_times[0],
            Duration::hours(1)
        );
    }

    #[test]
    fn cams_overlap_preserves_previous_same_cycle_history_but_replaces_forecast() {
        let latest = "2026-07-13T00:00:00Z".parse::<DateTime<Utc>>().unwrap();

        assert!(should_replace_overlapping_native_entry(
            "cams_global",
            latest - Duration::hours(1),
            latest,
            false,
        ));
        assert!(!should_replace_overlapping_native_entry(
            "cams_global",
            latest - Duration::hours(1),
            latest,
            true,
        ));
        assert!(should_replace_overlapping_native_entry(
            "cams_global",
            latest,
            latest,
            true,
        ));
        assert!(should_replace_overlapping_native_entry(
            "ncep_gfs013",
            latest - Duration::hours(1),
            latest,
            true,
        ));
        assert!(should_replace_overlapping_native_entry(
            "cams_global_greenhouse_gases",
            latest - Duration::hours(1),
            latest,
            true,
        ));
    }

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

    fn hourly_meta(hours: usize) -> FullRunMeta {
        let reference_time = "2026-07-12T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        FullRunMeta {
            reference_time,
            variables: vec!["temperature_2m".to_string()],
            valid_times: (0..hours)
                .map(|hour| reference_time + Duration::hours(hour as i64))
                .collect(),
        }
    }

    #[test]
    fn maps_real_cams_41_frame_axis_to_three_hour_source_times() {
        let indices = native_time_indices("cams_global", &hourly_meta(121), 41).unwrap();
        assert_eq!(indices, (0..=120).step_by(3).collect::<Vec<_>>());
    }

    #[test]
    fn maps_gfs_209_frame_axis_to_official_384_hour_schedule() {
        let indices = native_time_indices("ncep_gfs013", &hourly_meta(385), 209).unwrap();
        assert_eq!(&indices[..=120], &(0..=120).collect::<Vec<_>>());
        assert_eq!(&indices[121..], &(123..=384).step_by(3).collect::<Vec<_>>());
    }

    #[test]
    fn accepts_gfs_meta_that_already_contains_the_sparse_source_axis() {
        let reference_time = "2026-07-12T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let hours = (0..=120).chain((123..=384).step_by(3)).collect::<Vec<_>>();
        let meta = FullRunMeta {
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
    fn maps_gfs_variable_that_skips_forecast_hour_zero() {
        let reference_time = "2026-07-12T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let hours = (0..=120).chain((123..=384).step_by(3)).collect::<Vec<_>>();
        let meta = FullRunMeta {
            reference_time,
            variables: vec!["precipitation".to_string()],
            valid_times: hours
                .iter()
                .map(|hour| reference_time + Duration::hours(*hour))
                .collect(),
        };

        let indices = native_time_indices("ncep_gfs013", &meta, 208).unwrap();
        assert_eq!(indices[0], 1);
        assert_eq!(indices[1], 2);
        assert_eq!(*indices.last().unwrap(), 208);
    }

    #[test]
    fn rejects_sparse_native_axis_that_matches_no_source_schedule() {
        let error = native_time_indices("cams_global", &hourly_meta(121), 40).unwrap_err();
        assert!(error.to_string().contains("stored time count"));
    }

    #[test]
    fn parses_native_root_array_metadata_without_reading_full_data() {
        let mut file = NamedTempFile::new().unwrap();
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
        for value in [146_u64, 176, 121] {
            root.extend(value.to_le_bytes());
        }
        for value in [1_u64, 32, 121] {
            root.extend(value.to_le_bytes());
        }
        file.write_all(&root).unwrap();
        file.seek(SeekFrom::Start(512)).unwrap();
        let mut trailer = Vec::new();
        trailer.extend(b"OM");
        trailer.extend([3, 0]);
        trailer.extend(0_u32.to_le_bytes());
        trailer.extend(64_u64.to_le_bytes());
        trailer.extend((root.len() as u64).to_le_bytes());
        file.write_all(&trailer).unwrap();
        let parsed = read_native_array_metadata(file.as_file()).unwrap();
        assert_eq!(parsed.dimensions, vec![146, 176, 121]);
        assert_eq!(parsed.chunks, vec![1, 32, 121]);
        assert_eq!(parsed.lut_offset, Some(256));
        assert_eq!(parsed.scale_factor, Some(10.0));
    }

    #[test]
    #[ignore = "requires OM_REAL_NATIVE_TEST_FILE and OM_REAL_NATIVE_EXPECTED_DIMENSIONS"]
    fn parses_real_openmeteo_data_run_file() {
        let path = std::env::var("OM_REAL_NATIVE_TEST_FILE")
            .expect("OM_REAL_NATIVE_TEST_FILE must point to a real .om file");
        let expected = std::env::var("OM_REAL_NATIVE_EXPECTED_DIMENSIONS")
            .expect("OM_REAL_NATIVE_EXPECTED_DIMENSIONS must be comma-separated")
            .split(',')
            .map(|value| value.parse::<u64>().unwrap())
            .collect::<Vec<_>>();
        let file = File::open(path).unwrap();
        let parsed = read_native_array_metadata(&file).unwrap();

        assert_eq!(parsed.dimensions, expected);
        assert_eq!(parsed.chunks.len(), 3);
        assert!(parsed.chunks.iter().all(|value| *value > 0));
        assert!(parsed.scale_factor.is_some());
    }

    #[test]
    #[ignore = "requires a real CAMS .om file and compiled official decoder"]
    fn decodes_real_sparse_cams_file_through_native_snapshot() {
        use crate::official::OfficialDecoder;
        use crate::query::read_variable_value;
        use crate::snapshot::OmDataSnapshot;

        let real_file = std::env::var("OM_REAL_NATIVE_TEST_FILE")
            .expect("OM_REAL_NATIVE_TEST_FILE must point to a real CAMS .om file");
        let decoder_path = std::env::var("OM_REAL_DECODER_LIB")
            .expect("OM_REAL_DECODER_LIB must point to libomfileformat.so");
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let coverage_id = "cams_native_real_decode";
        let coverage = root.join("coverages/cams").join(coverage_id);
        fs::create_dir_all(&coverage).unwrap();
        fs::write(coverage.join("coverage.json"), b"{}").unwrap();
        let source_runs = ["2026071100", "2026071112", "2026071200"];
        for run in source_runs {
            let reference_time =
                DateTime::parse_from_str(&format!("{run}00 +0000"), "%Y%m%d%H%M %z")
                    .unwrap()
                    .with_timezone(&Utc);
            let run_root = coverage
                .join("data_run/cams_global")
                .join(reference_time.format("%Y/%m/%d/%H00Z").to_string());
            fs::create_dir_all(&run_root).unwrap();
            fs::copy(&real_file, run_root.join("dust.om")).unwrap();
            fs::write(
                run_root.join("meta.json"),
                serde_json::to_vec(&json!({
                    "reference_time": reference_time,
                    "variables": ["dust"],
                    "valid_times": (0..=120)
                        .map(|hour| reference_time + Duration::hours(hour))
                        .collect::<Vec<_>>(),
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let marker = json!({
            "status": "complete",
            "runtime_format": "openmeteo-native-v1",
            "group": "cams",
            "coverage_id": coverage_id,
            "latest_complete_run": "2026071200",
            "source_runs": source_runs,
            "public_start_utc": "2026-07-11T00:00:00Z",
            "coverage_path": format!("coverages/cams/{coverage_id}"),
            "products": {
                "cams_global": {
                    "runtime_domain": "cams_global",
                    "grid": {
                        "nx": 176, "ny": 146,
                        "lon_min": 70.0, "lat_min": 0.0,
                        "dx": 0.4, "dy": 0.4,
                        "dt_seconds": 3600,
                        "om_file_length": 217
                    }
                }
            }
        });
        let marker_path = root.join("groups/cams/current/ready_for_processing.json");
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        fs::create_dir_all(root.join("current")).unwrap();
        symlink(
            Path::new("../coverages/cams").join(coverage_id),
            root.join("current/cams"),
        )
        .unwrap();

        let snapshot = OmDataSnapshot::load(root).unwrap();
        let decoder = OfficialDecoder::load(decoder_path).unwrap();
        for hour in [0, 1, 2, 3] {
            let value = read_variable_value(
                &snapshot,
                Some(&decoder),
                "dust",
                "2026-07-12T00:00:00Z".parse::<DateTime<Utc>>().unwrap() + Duration::hours(hour),
                31.2,
                121.5,
            )
            .unwrap();
            assert!(value.is_finite());
            assert!(value >= 0.0);
        }
    }

    #[test]
    fn loads_cams_greenhouse_from_its_independent_daily_source_runs() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let coverage_id = "cams_native_2026071312_greenhouse-v1";
        let coverage = root.join("coverages/cams").join(coverage_id);
        fs::create_dir_all(&coverage).unwrap();
        fs::write(coverage.join("coverage.json"), b"{}").unwrap();

        let source_runs = ["2026071212", "2026071300", "2026071312"];
        for run in source_runs {
            let parsed = DateTime::parse_from_str(&format!("{run}00 +0000"), "%Y%m%d%H%M %z")
                .unwrap()
                .with_timezone(&Utc);
            let run_root = coverage
                .join("data_run/cams_global")
                .join(parsed.format("%Y/%m/%d/%H00Z").to_string());
            fs::create_dir_all(&run_root).unwrap();
            write_fake_om(&run_root.join("pm2_5.om"), [2, 3, 121]);
            fs::write(
                run_root.join("meta.json"),
                serde_json::to_vec(&json!({
                    "reference_time": parsed,
                    "variables": ["pm2_5"],
                    "valid_times": (0..=120)
                        .map(|hour| parsed + Duration::hours(hour))
                        .collect::<Vec<_>>(),
                }))
                .unwrap(),
            )
            .unwrap();
        }

        let greenhouse_source_runs = ["2026071100", "2026071200", "2026071300"];
        for run in greenhouse_source_runs {
            let parsed = DateTime::parse_from_str(&format!("{run}00 +0000"), "%Y%m%d%H%M %z")
                .unwrap()
                .with_timezone(&Utc);
            let run_root = coverage
                .join("data_run/cams_global_greenhouse_gases")
                .join(parsed.format("%Y/%m/%d/%H00Z").to_string());
            fs::create_dir_all(&run_root).unwrap();
            write_fake_om(&run_root.join("carbon_monoxide.om"), [2, 3, 41]);
            fs::write(
                run_root.join("meta.json"),
                serde_json::to_vec(&json!({
                    "reference_time": parsed,
                    "variables": ["carbon_monoxide"],
                    "valid_times": (0..=120)
                        .step_by(3)
                        .map(|hour| parsed + Duration::hours(hour))
                        .collect::<Vec<_>>(),
                }))
                .unwrap(),
            )
            .unwrap();
        }

        let marker = json!({
            "status": "complete",
            "runtime_format": "openmeteo-native-v1",
            "group": "cams",
            "coverage_id": coverage_id,
            "latest_complete_run": "2026071312",
            "source_runs": source_runs,
            "greenhouse_source_runs": greenhouse_source_runs,
            "public_start_utc": "2026-07-12T12:00:00Z",
            "coverage_path": format!("coverages/cams/{coverage_id}"),
            "products": {
                "cams_global": {
                    "runtime_domain": "cams_global",
                    "grid": {
                        "nx": 3, "ny": 2,
                        "lon_min": 70.0, "lat_min": 0.0,
                        "dx": 0.4, "dy": 0.4,
                        "dt_seconds": 3600,
                        "om_file_length": 217
                    }
                },
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
        let marker_path = root.join("groups/cams/current/ready_for_processing.json");
        fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
        fs::write(marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
        fs::create_dir_all(root.join("current")).unwrap();
        symlink(&coverage, root.join("current/cams")).unwrap();

        let mut products = HashMap::new();
        assert!(load_native_group_products(
            root,
            "cams",
            &["cams_global", "cams_global_greenhouse_gases"],
            &mut products,
        )
        .unwrap());
        let greenhouse = products.get("cams_global_greenhouse_gases").unwrap();
        let day_two = EntryKey {
            variable: "carbon_monoxide".to_string(),
            valid_time_utc: "2026-07-12T03:00:00Z".parse().unwrap(),
        };
        let latest = EntryKey {
            variable: "carbon_monoxide".to_string(),
            valid_time_utc: "2026-07-13T00:00:00Z".parse().unwrap(),
        };
        assert_eq!(greenhouse.entries[&day_two].source_run, "2026071200");
        assert_eq!(greenhouse.entries[&latest].source_run, "2026071300");
        assert_eq!(greenhouse.native_handles.len(), 3);
    }

    #[test]
    fn loads_five_run_native_marker_into_existing_product_snapshot_contract() {
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
        for run in source_runs {
            let parsed = DateTime::parse_from_str(&format!("{run}00 +0000"), "%Y%m%d%H%M %z")
                .unwrap()
                .with_timezone(&Utc);
            let run_root = coverage
                .join("data_run/ncep_gfs013")
                .join(parsed.format("%Y/%m/%d/%H00Z").to_string());
            fs::create_dir_all(&run_root).unwrap();
            write_fake_om(&run_root.join("temperature_2m.om"), [2, 3, 1]);
            fs::write(
                run_root.join("meta.json"),
                serde_json::to_vec(&json!({
                    "reference_time": parsed,
                    "variables": ["temperature_2m"],
                    "valid_times": [parsed],
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
            "public_start_utc": "2026-07-12T00:00:00Z",
            "coverage_path": format!("coverages/gfs/{coverage_id}"),
            "products": {
                "gfs013_surface": {
                    "runtime_domain": "ncep_gfs013",
                    "grid": {
                        "nx": 3,
                        "ny": 2,
                        "lon_min": 70.0,
                        "lat_min": 0.0,
                        "dx": 0.25,
                        "dy": 0.25,
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
        assert!(
            load_native_group_products(root, "gfs", &["gfs013_surface"], &mut products,).unwrap()
        );
        let product = products.get("gfs013_surface").unwrap();
        assert_eq!(product.entries.len(), 5);
        assert_eq!(product.native_handles.len(), 5);
        assert!(product.entries.values().all(|entry| {
            entry.native_time_index == Some(0)
                && entry.native_grid.as_ref().is_some_and(|grid| grid.nx == 3)
        }));
    }
}
