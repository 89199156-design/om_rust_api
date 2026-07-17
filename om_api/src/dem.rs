use crate::official::{BundleRangeReader, OfficialDecoder};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const MAX_CACHED_POINTS: usize = 100_000;
const OM_TRAILER_SIZE: u64 = 24;
const OM_LEGACY_HEADER_SIZE: u64 = 40;

type PointCache = HashMap<(PathBuf, i32, u64, u64), f32>;
type FileCache = HashMap<(PathBuf, i32), Arc<LocalOmFile>>;

static POINT_CACHE: OnceLock<Mutex<PointCache>> = OnceLock::new();
static FILE_CACHE: OnceLock<Mutex<FileCache>> = OnceLock::new();

#[derive(Debug)]
struct LocalOmFile {
    file: File,
    size: u64,
    metadata: Vec<u64>,
}

impl LocalOmFile {
    fn open(path: PathBuf) -> Result<Self> {
        let file = File::open(&path)
            .with_context(|| format!("failed to open local DEM file {}", path.display()))?;
        let size = file
            .metadata()
            .with_context(|| format!("failed to stat local DEM file {}", path.display()))?
            .len();
        let mut dem = Self {
            file,
            size,
            metadata: Vec::new(),
        };
        dem.metadata = dem.read_root_metadata()?;
        Ok(dem)
    }

    fn read_root_metadata(&self) -> Result<Vec<u64>> {
        if self.size < OM_LEGACY_HEADER_SIZE {
            bail!("DEM OM file is too small");
        }
        let header = self.read_original_range(0, OM_LEGACY_HEADER_SIZE)?;
        if header.get(0..2) != Some(b"OM") {
            bail!("DEM file is not an OM file");
        }
        if matches!(header[2], 1 | 2) {
            return Ok(align_metadata(header));
        }
        if header[2] != 3 {
            bail!("DEM OM file has an unsupported version");
        }

        let trailer = self.read_original_range(self.size - OM_TRAILER_SIZE, OM_TRAILER_SIZE)?;
        if trailer.len() != OM_TRAILER_SIZE as usize
            || trailer.get(0..2) != Some(b"OM")
            || trailer[2] != 3
        {
            bail!("DEM OM file has an invalid version-3 trailer");
        }
        let root_offset = u64::from_le_bytes(
            trailer[8..16]
                .try_into()
                .expect("validated OM trailer length"),
        );
        let root_size = u64::from_le_bytes(
            trailer[16..24]
                .try_into()
                .expect("validated OM trailer length"),
        );
        if root_size < 40 || root_offset.saturating_add(root_size) > self.size {
            bail!("DEM OM file has invalid root metadata bounds");
        }
        let bytes = self.read_original_range(root_offset, root_size)?;
        let data_type = bytes[0];
        if !(12..=21).contains(&data_type) {
            bail!("DEM OM root variable is not an array");
        }
        Ok(align_metadata(bytes))
    }
}

fn align_metadata(bytes: Vec<u8>) -> Vec<u64> {
    let words = (bytes.len() + 7) / 8;
    let mut aligned = vec![0_u64; words];
    let aligned_bytes =
        unsafe { std::slice::from_raw_parts_mut(aligned.as_mut_ptr() as *mut u8, words * 8) };
    aligned_bytes[..bytes.len()].copy_from_slice(&bytes);
    aligned
}

fn read_local_range(file: &File, start: u64, count: u64) -> Result<Vec<u8>> {
    let mut output = vec![0_u8; count as usize];
    file.read_exact_at(&mut output, start)
        .context("failed to read local DEM byte range")?;
    Ok(output)
}

impl BundleRangeReader for LocalOmFile {
    fn read_original_range(&self, start: u64, count: u64) -> Result<Vec<u8>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let end = start
            .checked_add(count)
            .context("DEM byte-range overflow")?;
        if end > self.size {
            bail!("DEM byte range exceeds file size");
        }
        read_local_range(&self.file, start, count)
    }
}

fn pixels_per_longitude(latitude: i32) -> u64 {
    match latitude {
        value if value < -85 => 120,
        value if value < -80 => 240,
        value if value < -70 => 400,
        value if value < -60 => 600,
        value if value < -50 => 800,
        value if value < 50 => 1200,
        value if value < 60 => 800,
        value if value < 70 => 600,
        value if value < 80 => 400,
        value if value < 85 => 240,
        _ => 120,
    }
}

fn dem_file_path(root: &Path, latitude_file: i32) -> PathBuf {
    root.join("copernicus_dem90")
        .join("static")
        .join(format!("lat_{latitude_file}.om"))
}

pub fn read_dem90(
    decoder: &OfficialDecoder,
    snapshot_root: &Path,
    latitude: f64,
    longitude: f64,
) -> Result<f32> {
    let latitude = latitude as f32;
    let longitude = longitude as f32;
    if !(-90.0..90.0).contains(&latitude) || !(-180.0..180.0).contains(&longitude) {
        return Ok(f32::NAN);
    }
    let latitude_file = if latitude < 0.0 {
        latitude as i32 - 1
    } else {
        latitude as i32
    };
    let latitude_row = ((latitude * 1200.0 + 90.0 * 1200.0) as u64) % 1200;
    let pixels = pixels_per_longitude(latitude_file);
    let longitude_row = ((longitude + 180.0) * pixels as f32) as u64;
    let root = std::env::var_os("OM_DEM_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| snapshot_root.to_path_buf());
    let key = (root.clone(), latitude_file, latitude_row, longitude_row);
    let points = POINT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(value) = points
        .lock()
        .map_err(|_| anyhow!("DEM point cache poisoned"))?
        .get(&key)
        .copied()
    {
        return Ok(value);
    }

    let files = FILE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // Drop the cache guard before file I/O and before taking the lock for insertion.
    let cached_file = {
        let files = files
            .lock()
            .map_err(|_| anyhow!("DEM file cache poisoned"))?;
        files.get(&(root.clone(), latitude_file)).cloned()
    };
    let file = if let Some(file) = cached_file {
        file
    } else {
        let path = dem_file_path(&root, latitude_file);
        let file = Arc::new(LocalOmFile::open(path)?);
        files
            .lock()
            .map_err(|_| anyhow!("DEM file cache poisoned"))?
            .insert((root, latitude_file), file.clone());
        file
    };
    let value = decoder.decode_point(
        &file.metadata,
        file.as_ref(),
        &[latitude_row, longitude_row],
    )?;
    let mut points = points
        .lock()
        .map_err(|_| anyhow!("DEM point cache poisoned"))?;
    if points.len() >= MAX_CACHED_POINTS {
        points.clear();
    }
    points.insert(key, value);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_exact_local_byte_range() {
        let path =
            std::env::temp_dir().join(format!("om-api-dem-range-{}.bin", std::process::id()));
        let bytes = (0_u8..64).collect::<Vec<_>>();
        std::fs::write(&path, &bytes).expect("write test DEM file");
        let file = File::open(&path).expect("open test DEM file");

        let actual = read_local_range(&file, 11, 17).expect("read local byte range");

        assert_eq!(actual, bytes[11..28]);
        std::fs::remove_file(path).expect("remove test DEM file");
    }

    #[test]
    fn resolves_dem_file_below_snapshot_root() {
        let root = Path::new("/srv/weather/coverages/gfs/gfs_native_2026071600");

        assert_eq!(
            dem_file_path(root, 31),
            root.join("copernicus_dem90/static/lat_31.om")
        );
    }
}
