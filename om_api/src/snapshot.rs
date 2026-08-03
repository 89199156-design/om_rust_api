use crate::manifest::{load_product_snapshot_for_coverage, ProductSnapshot};
use crate::native::load_native_group_products;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const GFS_SIDECAR_PRODUCTS: &[&str] = &["ncep_gefs025", "ncep_gefs05"];
pub const GFS_PRODUCTS: &[&str] = &[
    "gfs013_surface",
    "gfs025",
    "gfs_pressure_profile",
    "ncep_gefs025",
    "ncep_gefs05",
];
pub const CAMS_PRODUCTS: &[&str] = &["cams_global", "cams_global_greenhouse_gases"];
pub const CAMS_GREENHOUSE_PRODUCTS: &[&str] = &["cams_global_greenhouse_gases"];
pub const ECMWF_PRODUCTS: &[&str] = &["ecmwf_ifs025", "ecmwf_ifs025_ensemble"];

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
        let gfs_source_runs = selected_group_source_runs(&data_root, "gfs")?;
        let cams_source_runs = selected_group_source_runs(&data_root, "cams")?;
        let ecmwf_source_runs = selected_group_source_runs(&data_root, "ecmwf")?;
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
                &gfs_source_runs,
                &mut historical_products,
            )?;
        } else {
            // A Swift-produced native marker can embed deterministic and GEFS
            // products in one immutable coverage. A materialized official
            // marker can instead keep GEFS in the exact source release named
            // by that marker. Load that selected release as a compatibility
            // path, never an independently advancing "latest" directory.
            load_selected_source_release_products(
                &data_root,
                "gfs",
                GFS_SIDECAR_PRODUCTS,
                &mut products,
            )?;
            load_group_release_history(
                &data_root,
                "gfs",
                GFS_SIDECAR_PRODUCTS,
                &products,
                &gfs_source_runs,
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
                &cams_source_runs,
                &mut historical_products,
            )?;
        }
        // CAMS ADS publishes greenhouse gases independently from the main
        // CAMS cycle. Load that authoritative namespace last so a retained
        // legacy combined marker cannot overwrite it during migration or
        // rollback.
        if data_root
            .join("groups/cams_greenhouse/current/ready_for_processing.json")
            .is_file()
        {
            products.remove("cams_global_greenhouse_gases");
            historical_products.remove("cams_global_greenhouse_gases");
            load_native_group_products(
                &data_root,
                "cams_greenhouse",
                CAMS_GREENHOUSE_PRODUCTS,
                &mut products,
                &mut historical_products,
            )?;
        }
        let ecmwf_native = load_native_group_products(
            &data_root,
            "ecmwf",
            ECMWF_PRODUCTS,
            &mut products,
            &mut historical_products,
        )?;
        if !ecmwf_native {
            load_group_products(&data_root, "ecmwf", ECMWF_PRODUCTS, &mut products)?;
            load_group_release_history(
                &data_root,
                "ecmwf",
                ECMWF_PRODUCTS,
                &products,
                &ecmwf_source_runs,
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
    #[serde(default)]
    group: String,
    status: String,
    #[serde(default)]
    release_id: String,
    #[serde(default)]
    latest_complete_run: String,
    #[serde(default)]
    source_runs: Vec<String>,
    #[serde(default)]
    product_manifests: HashMap<String, ProductReady>,
}

#[derive(Debug, Deserialize)]
struct ProductReady {
    coverage_id: String,
}

fn retained_product_run_is_not_newer(
    current_run: Option<&str>,
    retained_run: Option<&str>,
) -> bool {
    match (
        current_run.filter(|value| !value.is_empty()),
        retained_run.filter(|value| !value.is_empty()),
    ) {
        (Some(current), Some(retained)) => retained <= current,
        _ => true,
    }
}

fn retained_product_run_is_selected(
    selected_source_runs: &[String],
    retained_run: Option<&str>,
) -> bool {
    selected_source_runs.is_empty()
        || retained_run
            .is_some_and(|run| selected_source_runs.iter().any(|selected| selected == run))
}

fn selected_group_source_runs(data_root: &Path, group: &str) -> Result<Vec<String>> {
    let current_path = data_root
        .join("groups")
        .join(group)
        .join("current")
        .join("ready_for_processing.json");
    if !current_path.is_file() {
        return Ok(Vec::new());
    }
    let current: GroupReady = load_manifest_like(&current_path)?;
    Ok(current.source_runs)
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
    load_products_from_ready(data_root, group, group_products, &ready, products)?;
    Ok(())
}

fn load_selected_source_release_products(
    data_root: &Path,
    group: &str,
    group_products: &[&str],
    products: &mut HashMap<String, Arc<ProductSnapshot>>,
) -> Result<()> {
    let current_path = data_root
        .join("groups")
        .join(group)
        .join("current")
        .join("ready_for_processing.json");
    if !current_path.is_file() {
        return Ok(());
    }
    let current: GroupReady = load_manifest_like(&current_path)?;
    if current.status != "complete" || current.release_id.is_empty() {
        return Ok(());
    }
    if !current
        .release_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("group current marker contains an unsafe release_id");
    }
    let release_path = data_root
        .join("groups")
        .join(group)
        .join("releases")
        .join(format!("{}.json", current.release_id));
    if !release_path.is_file() {
        return Ok(());
    }
    let release: GroupReady = load_manifest_like(&release_path)?;
    if release.status != "complete"
        || (!release.group.is_empty() && release.group != group)
        || release.release_id != current.release_id
        || release.latest_complete_run != current.latest_complete_run
    {
        anyhow::bail!(
            "source release {} does not match current {} marker",
            current.release_id,
            group
        );
    }
    load_products_from_ready(data_root, group, group_products, &release, products)
}

fn load_products_from_ready(
    data_root: &Path,
    group: &str,
    group_products: &[&str],
    ready: &GroupReady,
    products: &mut HashMap<String, Arc<ProductSnapshot>>,
) -> Result<()> {
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
    selected_source_runs: &[String],
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
            if !retained_product_run_is_not_newer(
                current.manifest.latest_complete_run.as_deref(),
                snapshot.manifest.latest_complete_run.as_deref(),
            ) {
                // A deliberately frozen mixed ECMWF group can coexist with a
                // release captured later for just one independently published
                // product. Such a release is future data, not history, and
                // must never extend the frozen public horizon.
                continue;
            }
            if !retained_product_run_is_selected(
                selected_source_runs,
                snapshot.manifest.latest_complete_run.as_deref(),
            ) {
                // Native group markers are the authoritative retained-run
                // contract. An older rollback release may stay on disk, but
                // it must not silently become interpolation support outside
                // the declared production window.
                continue;
            }
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

#[cfg(test)]
mod tests {
    use super::{retained_product_run_is_not_newer, retained_product_run_is_selected};

    #[test]
    fn frozen_product_history_never_loads_a_newer_independent_run() {
        assert!(!retained_product_run_is_not_newer(
            Some("2026072818"),
            Some("2026072900")
        ));
        assert!(retained_product_run_is_not_newer(
            Some("2026072818"),
            Some("2026072812")
        ));
        assert!(retained_product_run_is_not_newer(
            Some("2026072818"),
            Some("2026072818")
        ));
    }

    #[test]
    fn native_marker_source_runs_exclude_older_rollback_support() {
        let selected = vec![
            "2026080106".to_string(),
            "2026080112".to_string(),
            "2026080118".to_string(),
            "2026080200".to_string(),
            "2026080206".to_string(),
        ];
        assert!(retained_product_run_is_selected(
            &selected,
            Some("2026080200")
        ));
        assert!(!retained_product_run_is_selected(
            &selected,
            Some("2026073006")
        ));
        assert!(!retained_product_run_is_selected(&selected, None));
        assert!(retained_product_run_is_selected(&[], Some("2026073006")));
    }

    #[test]
    fn legacy_missing_run_metadata_remains_loadable() {
        assert!(retained_product_run_is_not_newer(Some("2026072818"), None));
        assert!(retained_product_run_is_not_newer(None, Some("2026072900")));
    }
}
