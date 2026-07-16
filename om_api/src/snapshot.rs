use crate::manifest::{load_product_snapshot_for_coverage, ProductSnapshot};
use crate::native::load_native_group_products;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const GFS_PRODUCTS: &[&str] = &["gfs013_surface", "gfs025", "gfs_pressure_profile"];
pub const CAMS_PRODUCTS: &[&str] = &["cams_global", "cams_global_greenhouse_gases"];

#[derive(Debug)]
pub struct OmDataSnapshot {
    pub data_root: PathBuf,
    products: HashMap<String, Arc<ProductSnapshot>>,
    historical_products: HashMap<String, Vec<Arc<ProductSnapshot>>>,
}

impl OmDataSnapshot {
    pub fn load(data_root: impl AsRef<Path>) -> Result<Self> {
        let data_root = data_root.as_ref().to_path_buf();
        let mut products = HashMap::new();
        let mut historical_products = HashMap::new();
        let gfs_native = load_native_group_products(
            &data_root,
            "gfs",
            GFS_PRODUCTS,
            &mut products,
            &mut historical_products,
        )?;
        let cams_native = load_native_group_products(
            &data_root,
            "cams",
            CAMS_PRODUCTS,
            &mut products,
            &mut historical_products,
        )?;
        if !gfs_native {
            load_group_products(&data_root, "gfs", GFS_PRODUCTS, &mut products)?;
            load_group_release_history(
                &data_root,
                "gfs",
                GFS_PRODUCTS,
                &products,
                &mut historical_products,
            )?;
        }
        if !cams_native {
            load_group_products(&data_root, "cams", CAMS_PRODUCTS, &mut products)?;
            load_group_release_history(
                &data_root,
                "cams",
                CAMS_PRODUCTS,
                &products,
                &mut historical_products,
            )?;
        }
        Ok(Self {
            data_root,
            products,
            historical_products,
        })
    }

    pub fn product(&self, name: &str) -> Option<Arc<ProductSnapshot>> {
        self.products.get(name).cloned()
    }

    pub fn require_product(&self, name: &str) -> anyhow::Result<Arc<ProductSnapshot>> {
        self.product(name)
            .ok_or_else(|| anyhow::anyhow!("product is not available: {}", name))
    }

    pub fn product_snapshots(&self, name: &str) -> Vec<Arc<ProductSnapshot>> {
        let mut snapshots = Vec::new();
        if let Some(current) = self.product(name) {
            snapshots.push(current);
        }
        if let Some(history) = self.historical_products.get(name) {
            snapshots.extend(history.iter().cloned());
        }
        snapshots
    }
}

#[derive(Debug, Deserialize)]
struct GroupReady {
    status: String,
    #[serde(default)]
    latest_complete_run: String,
    product_manifests: HashMap<String, ProductReady>,
}

#[derive(Debug, Deserialize)]
struct ProductReady {
    coverage_id: String,
}

fn load_group_products(
    data_root: &Path,
    group: &str,
    group_products: &[&str],
    products: &mut HashMap<String, Arc<ProductSnapshot>>,
) -> Result<()> {
    let group_ready_path = data_root
        .join("groups")
        .join(group)
        .join("current")
        .join("ready_for_processing.json");
    if !group_ready_path.exists() {
        return Ok(());
    }
    let ready: GroupReady = load_manifest_like(&group_ready_path)?;
    if ready.status != "complete" {
        return Ok(());
    }
    for product in group_products {
        if let Some(product_ready) = ready.product_manifests.get(*product) {
            if data_root.join(product).exists() {
                let snapshot = load_product_snapshot_for_coverage(
                    data_root,
                    product,
                    &product_ready.coverage_id,
                )
                .with_context(|| {
                    format!(
                        "failed to load {} coverage {} selected by group {}",
                        product, product_ready.coverage_id, group
                    )
                })?;
                products.insert((*product).to_string(), Arc::new(snapshot));
            }
        }
    }
    Ok(())
}

fn load_group_release_history(
    data_root: &Path,
    group: &str,
    group_products: &[&str],
    current_products: &HashMap<String, Arc<ProductSnapshot>>,
    historical_products: &mut HashMap<String, Vec<Arc<ProductSnapshot>>>,
) -> Result<()> {
    let releases_root = data_root.join("groups").join(group).join("releases");
    if !releases_root.exists() {
        return Ok(());
    }

    let mut releases = fs::read_dir(&releases_root)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("json"))
        .map(|entry| {
            let path = entry.path();
            let ready: GroupReady = load_manifest_like(&path)
                .with_context(|| format!("failed to load group release: {}", path.display()))?;
            Ok((path, ready))
        })
        .collect::<Result<Vec<_>>>()?;
    releases.sort_by(|left, right| right.1.latest_complete_run.cmp(&left.1.latest_complete_run));

    for (_, release) in releases {
        if release.status != "complete" {
            continue;
        }
        for product in group_products {
            let Some(product_ready) = release.product_manifests.get(*product) else {
                continue;
            };
            let Some(current) = current_products.get(*product) else {
                continue;
            };
            if current.manifest.coverage_id == product_ready.coverage_id {
                continue;
            }
            let already_loaded = historical_products.get(*product).is_some_and(|snapshots| {
                snapshots
                    .iter()
                    .any(|snapshot| snapshot.manifest.coverage_id == product_ready.coverage_id)
            });
            if already_loaded {
                continue;
            }
            let coverage_root = data_root
                .join(product)
                .join("coverages")
                .join(&product_ready.coverage_id);
            if !coverage_root.exists() {
                // A retention manifest can outlive a manually removed old
                // coverage. Current data remains valid; omit only the daily
                // values whose history window is no longer complete.
                continue;
            }
            let snapshot =
                load_product_snapshot_for_coverage(data_root, product, &product_ready.coverage_id)
                    .with_context(|| {
                        format!(
                            "failed to load historical {} coverage {} selected by group {}",
                            product, product_ready.coverage_id, group
                        )
                    })?;
            historical_products
                .entry((*product).to_string())
                .or_default()
                .push(Arc::new(snapshot));
        }
    }
    Ok(())
}

fn load_manifest_like<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}
