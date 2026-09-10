use anyhow::{bail, Context, Result};
use image::ImageReader;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

const DETAILS_ENCODING: &str = "stargazing-details-rgb24-v1";

#[derive(Debug, Clone, Deserialize)]
pub struct StargazingDetailsQuery {
    pub source: String,
    pub timestamp: i64,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StargazingDetailsResponse {
    pub source: String,
    pub release_id: String,
    pub timestamp: i64,
    pub latitude: f64,
    pub longitude: f64,
    pub grid_latitude: f64,
    pub grid_longitude: f64,
    pub score: u8,
    pub total_sky_magnitude: f64,
    pub weather_retention_percent: u8,
    pub weather_loss_percent: u8,
}

#[derive(Debug, Deserialize)]
struct Pointer {
    status: String,
    release_id: String,
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    batch: i64,
    file_pattern: String,
    files: Vec<i64>,
    grid: Grid,
    layers: Layers,
}

#[derive(Debug, Deserialize)]
struct Layers {
    details: DetailsLayer,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailsLayer {
    subdir: String,
    encoding: String,
    magnitude_min: f64,
    magnitude_step: f64,
}

#[derive(Debug, Deserialize)]
struct Grid {
    width: u32,
    height: u32,
    sample_bounds: Bounds,
    display_bounds: Bounds,
    row_order: String,
}

#[derive(Debug, Deserialize)]
struct Bounds {
    lon_min: f64,
    lat_min: f64,
    lon_max: f64,
    lat_max: f64,
}

fn source_contract(source: &str) -> Result<(&'static str, &'static str)> {
    match source.to_ascii_lowercase().as_str() {
        "gfs" => Ok(("stargazing_gfs", "GFS+CAMS")),
        "ec9" => Ok(("stargazing_ec9", "EC9+CAMS")),
        _ => bail!("source must be gfs or ec9"),
    }
}

fn checked_release(root: &Path, pointer: &Pointer) -> Result<PathBuf> {
    if pointer.status != "complete" {
        bail!("stargazing release is not complete");
    }
    let releases = root
        .join("releases")
        .canonicalize()
        .context("stargazing releases root is unavailable")?;
    let release = pointer.path.canonicalize().with_context(|| {
        format!(
            "stargazing release is unavailable: {}",
            pointer.path.display()
        )
    })?;
    if !release.starts_with(&releases) {
        bail!("stargazing release path is outside the configured root");
    }
    Ok(release)
}

fn nearest_index(value: f64, minimum: f64, maximum: f64, count: u32) -> Result<u32> {
    if !value.is_finite() || !minimum.is_finite() || !maximum.is_finite() || count == 0 {
        bail!("invalid stargazing grid coordinate");
    }
    if count == 1 {
        return Ok(0);
    }
    let ratio = ((value - minimum) / (maximum - minimum)).clamp(0.0, 1.0);
    Ok((ratio * f64::from(count - 1)).round() as u32)
}

fn grid_coordinate(index: u32, minimum: f64, maximum: f64, count: u32) -> f64 {
    if count <= 1 {
        minimum
    } else {
        minimum + f64::from(index) * (maximum - minimum) / f64::from(count - 1)
    }
}

pub fn load_details(
    root: &Path,
    query: &StargazingDetailsQuery,
) -> Result<StargazingDetailsResponse> {
    let (product, display_source) = source_contract(&query.source)?;
    if !query.latitude.is_finite() || !query.longitude.is_finite() {
        bail!("latitude and longitude must be finite");
    }
    let pointer_path = root.join("current").join(format!("{product}.json"));
    let pointer: Pointer = serde_json::from_slice(
        &fs::read(&pointer_path).with_context(|| format!("read {}", pointer_path.display()))?,
    )
    .with_context(|| format!("parse {}", pointer_path.display()))?;
    let release = checked_release(root, &pointer)?;
    let product_root = release.join(product);
    let manifest_path = product_root.join(format!("{product}_data.json"));
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(&manifest_path).with_context(|| format!("read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parse {}", manifest_path.display()))?;
    if manifest.layers.details.encoding != DETAILS_ENCODING {
        bail!("stargazing details encoding is unsupported");
    }
    if manifest.grid.row_order != "north_to_south" {
        bail!("stargazing grid row order is unsupported");
    }
    if !manifest.files.contains(&query.timestamp) {
        bail!("requested timestamp is not present in the current stargazing release");
    }
    let bounds = &manifest.grid.display_bounds;
    if query.longitude < bounds.lon_min
        || query.longitude > bounds.lon_max
        || query.latitude < bounds.lat_min
        || query.latitude > bounds.lat_max
    {
        bail!("coordinate is outside the current stargazing coverage");
    }

    let sample = &manifest.grid.sample_bounds;
    let column = nearest_index(
        query.longitude,
        sample.lon_min,
        sample.lon_max,
        manifest.grid.width,
    )?;
    let south_index = nearest_index(
        query.latitude,
        sample.lat_min,
        sample.lat_max,
        manifest.grid.height,
    )?;
    let row = manifest.grid.height - 1 - south_index;
    let file_name = manifest
        .file_pattern
        .replace("{timestamp}", &query.timestamp.to_string())
        .replace("{batch}", &manifest.batch.to_string());
    let detail_path = product_root
        .join(&manifest.layers.details.subdir)
        .join(file_name);
    let bytes =
        fs::read(&detail_path).with_context(|| format!("read {}", detail_path.display()))?;
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("detect stargazing details image format")?
        .decode()
        .context("decode stargazing details image")?
        .to_rgb8();
    if image.width() != manifest.grid.width || image.height() != manifest.grid.height {
        bail!("stargazing details image does not match its grid contract");
    }
    let pixel = image.get_pixel(column, row).0;
    let packed = (u32::from(pixel[0]) << 16) | (u32::from(pixel[1]) << 8) | u32::from(pixel[2]);
    if packed == 0 {
        bail!("stargazing details are unavailable at this coordinate");
    }
    let magnitude_code = packed >> 14;
    let weather_retention = ((packed >> 7) & 0x7f) as u8;
    let score = (packed & 0x7f) as u8;
    if magnitude_code == 0 || weather_retention > 100 || score > 100 {
        bail!("stargazing details pixel is invalid");
    }
    let total_sky_magnitude = manifest.layers.details.magnitude_min
        + f64::from(magnitude_code - 1) * manifest.layers.details.magnitude_step;
    Ok(StargazingDetailsResponse {
        source: display_source.to_string(),
        release_id: pointer.release_id,
        timestamp: query.timestamp,
        latitude: query.latitude,
        longitude: query.longitude,
        grid_latitude: grid_coordinate(
            south_index,
            sample.lat_min,
            sample.lat_max,
            manifest.grid.height,
        ),
        grid_longitude: grid_coordinate(
            column,
            sample.lon_min,
            sample.lon_max,
            manifest.grid.width,
        ),
        score,
        total_sky_magnitude,
        weather_retention_percent: weather_retention,
        weather_loss_percent: 100 - weather_retention,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use tempfile::TempDir;

    #[test]
    fn loads_the_exact_map_cell_from_the_current_release() {
        let root = TempDir::new().unwrap();
        let release = root.path().join("releases/stargazing_gfs-test");
        let product = release.join("stargazing_gfs");
        fs::create_dir_all(product.join("details")).unwrap();
        fs::create_dir_all(root.path().join("current")).unwrap();
        fs::write(
            root.path().join("current/stargazing_gfs.json"),
            serde_json::to_vec(&serde_json::json!({
                "status": "complete", "release_id": "stargazing_gfs-test", "path": release
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            product.join("stargazing_gfs_data.json"),
            serde_json::to_vec(&serde_json::json!({
                "batch": 7,
                "file_pattern": "{timestamp}_{batch}.webp",
                "files": [1234],
                "grid": {
                    "width": 3, "height": 2, "row_order": "north_to_south",
                    "sample_bounds": {"lon_min": 100.0, "lat_min": 20.0, "lon_max": 102.0, "lat_max": 21.0},
                    "display_bounds": {"lon_min": 99.5, "lat_min": 19.5, "lon_max": 102.5, "lat_max": 21.5}
                },
                "layers": {"details": {
                    "subdir": "details", "encoding": DETAILS_ENCODING,
                    "magnitudeMin": 5.0, "magnitudeStep": 0.02
                }}
            })).unwrap(),
        ).unwrap();
        let magnitude_code = 817_u32;
        let packed = (magnitude_code << 14) | (53_u32 << 7) | 28_u32;
        let mut raster = RgbImage::new(3, 2);
        raster.put_pixel(
            1,
            0,
            Rgb([
                ((packed >> 16) & 255) as u8,
                ((packed >> 8) & 255) as u8,
                (packed & 255) as u8,
            ]),
        );
        raster.save(product.join("details/1234_7.webp")).unwrap();

        let result = load_details(
            root.path(),
            &StargazingDetailsQuery {
                source: "gfs".into(),
                timestamp: 1234,
                latitude: 21.0,
                longitude: 101.0,
            },
        )
        .unwrap();
        assert_eq!(result.score, 28);
        assert_eq!(result.weather_retention_percent, 53);
        assert_eq!(result.weather_loss_percent, 47);
        assert!((result.total_sky_magnitude - 21.32).abs() < 0.001);
        assert_eq!(result.grid_latitude, 21.0);
        assert_eq!(result.grid_longitude, 101.0);
    }

    #[test]
    fn rejects_unknown_sources_and_outside_coordinates() {
        assert!(source_contract("other").is_err());
        assert_eq!(nearest_index(101.4, 100.0, 102.0, 3).unwrap(), 1);
        assert_eq!(nearest_index(101.6, 100.0, 102.0, 3).unwrap(), 2);
    }
}
