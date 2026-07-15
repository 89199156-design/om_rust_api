use crate::official::{BundleRangeReader, OfficialDecoder};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const DEFAULT_DEM_BASE_URL: &str = "https://openmeteo.s3.amazonaws.com/data";
const HTTP_BLOCK_SIZE: u64 = 64 * 1024;
const MAX_CACHED_BLOCKS: usize = 512;
const MAX_CACHED_POINTS: usize = 100_000;
const OM_TRAILER_SIZE: u64 = 24;
const OM_LEGACY_HEADER_SIZE: u64 = 40;

type BlockCache = HashMap<(String, u64), Arc<Vec<u8>>>;
type PointCache = HashMap<(i32, u64, u64), f32>;
type FileCache = HashMap<i32, Arc<RemoteOmFile>>;

static BLOCK_CACHE: OnceLock<Mutex<BlockCache>> = OnceLock::new();
static POINT_CACHE: OnceLock<Mutex<PointCache>> = OnceLock::new();
static FILE_CACHE: OnceLock<Mutex<FileCache>> = OnceLock::new();

#[derive(Debug)]
struct RemoteOmFile {
    agent: ureq::Agent,
    url: String,
    size: u64,
    metadata: Vec<u64>,
}

impl RemoteOmFile {
    fn open(url: String) -> Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(8))
            .timeout_read(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(10))
            .build();
        let probe = agent
            .get(&url)
            .set("Range", "bytes=0-0")
            .call()
            .map_err(|error| anyhow!("failed to open DEM file {url}: {error}"))?;
        if probe.status() != 206 {
            bail!(
                "DEM server did not honor byte ranges for {}: HTTP {}",
                url,
                probe.status()
            );
        }
        let size = probe
            .header("Content-Range")
            .and_then(|value| value.rsplit('/').next())
            .context("DEM range response has no total file size")?
            .parse::<u64>()
            .context("DEM range response has an invalid total file size")?;
        let mut probe_body = Vec::new();
        probe
            .into_reader()
            .take(2)
            .read_to_end(&mut probe_body)
            .context("failed to read DEM range probe")?;
        if probe_body.len() != 1 {
            bail!("DEM range probe returned {} bytes", probe_body.len());
        }

        let mut file = Self {
            agent,
            url,
            size,
            metadata: Vec::new(),
        };
        file.metadata = file.read_root_metadata()?;
        Ok(file)
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

    fn fetch_block(&self, block: u64) -> Result<Arc<Vec<u8>>> {
        let key = (self.url.clone(), block);
        let cache = BLOCK_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(value) = cache
            .lock()
            .map_err(|_| anyhow!("DEM HTTP block cache poisoned"))?
            .get(&key)
            .cloned()
        {
            return Ok(value);
        }

        let start = block * HTTP_BLOCK_SIZE;
        if start >= self.size {
            bail!("DEM byte-range block starts beyond end of file");
        }
        let end = (start + HTTP_BLOCK_SIZE).min(self.size) - 1;
        let response = self
            .agent
            .get(&self.url)
            .set("Range", &format!("bytes={start}-{end}"))
            .call()
            .map_err(|error| anyhow!("failed to read DEM byte range: {error}"))?;
        if response.status() != 206 {
            bail!(
                "DEM server did not honor byte range {}-{}: HTTP {}",
                start,
                end,
                response.status()
            );
        }
        let expected = (end - start + 1) as usize;
        let mut bytes = Vec::with_capacity(expected);
        response
            .into_reader()
            .take(expected as u64 + 1)
            .read_to_end(&mut bytes)
            .context("failed to read DEM byte-range response")?;
        if bytes.len() != expected {
            bail!(
                "DEM byte-range response length mismatch: expected {}, got {}",
                expected,
                bytes.len()
            );
        }
        let value = Arc::new(bytes);
        let mut cache = cache
            .lock()
            .map_err(|_| anyhow!("DEM HTTP block cache poisoned"))?;
        if cache.len() >= MAX_CACHED_BLOCKS {
            cache.clear();
        }
        cache.insert(key, value.clone());
        Ok(value)
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

impl BundleRangeReader for RemoteOmFile {
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
        let mut output = Vec::with_capacity(count as usize);
        let first_block = start / HTTP_BLOCK_SIZE;
        let last_block = (end - 1) / HTTP_BLOCK_SIZE;
        for block in first_block..=last_block {
            let bytes = self.fetch_block(block)?;
            let block_start = block * HTTP_BLOCK_SIZE;
            let from = start.max(block_start) - block_start;
            let to = end.min(block_start + bytes.len() as u64) - block_start;
            output.extend_from_slice(&bytes[from as usize..to as usize]);
        }
        if output.len() != count as usize {
            bail!("assembled DEM byte range has an invalid length");
        }
        Ok(output)
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

pub fn read_dem90(decoder: &OfficialDecoder, latitude: f64, longitude: f64) -> Result<f32> {
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
    let key = (latitude_file, latitude_row, longitude_row);
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
    // Drop the cache guard before network I/O and before taking the lock for insertion.
    let cached_file = {
        let files = files
            .lock()
            .map_err(|_| anyhow!("DEM file cache poisoned"))?;
        files.get(&latitude_file).cloned()
    };
    let file = if let Some(file) = cached_file {
        file
    } else {
        let base =
            std::env::var("OM_DEM_BASE_URL").unwrap_or_else(|_| DEFAULT_DEM_BASE_URL.to_string());
        let url = format!(
            "{}/copernicus_dem90/static/lat_{}.om",
            base.trim_end_matches('/'),
            latitude_file
        );
        let file = Arc::new(RemoteOmFile::open(url)?);
        files
            .lock()
            .map_err(|_| anyhow!("DEM file cache poisoned"))?
            .insert(latitude_file, file.clone());
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
