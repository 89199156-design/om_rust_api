use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize)]
pub struct ProductManifest {
    pub model: String,
    pub coverage_id: String,
    pub status: String,
    #[serde(default)]
    pub latest_complete_run: Option<String>,
    #[serde(default)]
    pub config_fingerprint: Option<String>,
    #[serde(default)]
    pub public_start_utc: Option<DateTime<Utc>>,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub bytes: u64,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub entries: Vec<BundleEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BundleEntry {
    pub variable: String,
    #[serde(default)]
    pub variable_path: Option<String>,
    pub valid_time_utc: DateTime<Utc>,
    pub source_run: String,
    pub forecast_hour: i64,
    #[serde(default)]
    pub source_url: Option<String>,
    pub selection_ranges: Vec<[u64; 2]>,
    pub array: ArrayMetadata,
    #[serde(default)]
    pub lut_byte_ranges: Vec<[u64; 2]>,
    #[serde(default)]
    pub data_byte_ranges: Vec<[u64; 2]>,
    #[serde(default)]
    pub lut_bytes_read: u64,
    pub byte_ranges: Vec<[u64; 2]>,
    pub bundle_offset: u64,
    pub bundle_bytes: u64,
    #[serde(default)]
    pub native_file_path: Option<String>,
    #[serde(default)]
    pub native_time_index: Option<u64>,
    #[serde(default)]
    pub native_grid: Option<NativeGridMetadata>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NativeGridMetadata {
    pub nx: u64,
    pub ny: u64,
    pub lon_min: f64,
    pub lat_min: f64,
    pub dx: f64,
    pub dy: f64,
    pub dt_seconds: i64,
    pub om_file_length: u64,
    #[serde(default)]
    pub full_nx: Option<u64>,
    #[serde(default)]
    pub full_ny: Option<u64>,
    #[serde(default)]
    pub x0: Option<u64>,
    #[serde(default)]
    pub y0: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArrayMetadata {
    pub data_type: u8,
    pub compression: u8,
    pub dimensions: Vec<u64>,
    pub chunks: Vec<u64>,
    #[serde(default)]
    pub lut_offset: Option<u64>,
    #[serde(default)]
    pub lut_size: Option<u64>,
    #[serde(default)]
    pub scale_factor: Option<f32>,
    #[serde(default)]
    pub add_offset: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryKey {
    pub variable: String,
    pub valid_time_utc: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ProductSnapshot {
    pub product: String,
    pub product_root: PathBuf,
    pub manifest: ProductManifest,
    pub bundle_file: ManifestFile,
    pub bundle_path: PathBuf,
    pub bundle_handle: Arc<File>,
    pub entries: HashMap<EntryKey, BundleEntry>,
    pub static_entries: HashMap<String, BundleEntry>,
    pub native_handles: HashMap<String, Arc<File>>,
}

pub fn load_product_snapshot(data_root: &Path, product: &str) -> Result<ProductSnapshot> {
    let product_root = data_root.join(product);
    let manifest_path = product_root.join("current").join("latest.json");
    let manifest_path = if manifest_path.exists() {
        manifest_path
    } else {
        product_root.join("latest.json")
    };
    load_product_snapshot_from_manifest_path(&product_root, product, &manifest_path)
}

pub fn load_product_snapshot_for_coverage(
    data_root: &Path,
    product: &str,
    coverage_id: &str,
) -> Result<ProductSnapshot> {
    if coverage_id.is_empty()
        || Path::new(coverage_id).is_absolute()
        || coverage_id.contains("..")
        || coverage_id.contains('/')
        || coverage_id.contains('\\')
    {
        bail!("unsafe coverage_id for {}: {}", product, coverage_id);
    }
    let product_root = data_root.join(product);
    let manifest_path = product_root
        .join("coverages")
        .join(coverage_id)
        .join("latest.json");
    load_product_snapshot_from_manifest_path(&product_root, product, &manifest_path)
}

fn load_product_snapshot_from_manifest_path(
    product_root: &Path,
    product: &str,
    manifest_path: &Path,
) -> Result<ProductSnapshot> {
    let manifest = load_manifest(manifest_path)
        .with_context(|| format!("failed to load manifest {}", manifest_path.display()))?;
    validate_manifest(product, &manifest)?;
    let bundle_file = manifest
        .files
        .iter()
        .find(|file| file.path.ends_with(".omranges"))
        .cloned()
        .context("complete manifest does not contain an .omranges bundle")?;
    let bundle_path = safe_join(&product_root, &bundle_file.path)?;
    if !bundle_path.exists() {
        bail!("bundle file does not exist: {}", bundle_path.display());
    }
    let size = bundle_path.metadata()?.len();
    if size != bundle_file.bytes {
        bail!(
            "bundle file size mismatch for {}: got {}, manifest {}",
            bundle_path.display(),
            size,
            bundle_file.bytes
        );
    }
    let bundle_handle = Arc::new(
        File::open(&bundle_path)
            .with_context(|| format!("failed to open bundle {}", bundle_path.display()))?,
    );

    let mut entries = HashMap::new();
    for entry in &bundle_file.entries {
        entries.insert(
            EntryKey {
                variable: entry.variable.clone(),
                valid_time_utc: entry.valid_time_utc,
            },
            entry.clone(),
        );
    }
    if entries.is_empty() {
        bail!(
            "bundle manifest has no entries: {}",
            manifest_path.display()
        );
    }

    Ok(ProductSnapshot {
        product: product.to_string(),
        product_root: product_root.to_path_buf(),
        manifest,
        bundle_file,
        bundle_path,
        bundle_handle,
        entries,
        static_entries: HashMap::new(),
        native_handles: HashMap::new(),
    })
}

pub fn load_manifest(path: &Path) -> Result<ProductManifest> {
    let text = fs::read_to_string(path)?;
    let manifest: ProductManifest = serde_json::from_str(&text)?;
    Ok(manifest)
}

fn validate_manifest(product: &str, manifest: &ProductManifest) -> Result<()> {
    if manifest.model != product {
        bail!(
            "manifest model mismatch: expected {}, got {}",
            product,
            manifest.model
        );
    }
    if manifest.status != "complete" {
        bail!("manifest is not complete: {}", manifest.status);
    }
    if manifest.coverage_id.is_empty() {
        bail!("manifest coverage_id is empty");
    }
    if manifest.files.is_empty() {
        bail!("manifest files is empty");
    }
    Ok(())
}

pub fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute() {
        bail!("absolute manifest path is not allowed: {}", relative);
    }
    let mut out = root.to_path_buf();
    for part in path.components() {
        match part {
            std::path::Component::Normal(value) => out.push(value),
            _ => bail!("unsafe manifest path: {}", relative),
        }
    }
    Ok(out)
}
