use crate::official::OfficialDecoder;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const FORMAT: &str = "weather-region-pack-v1";
const MODEL: &str = "ecmwf_ifs9km_omdata";
const GROUP: &str = "ecmwf_ifs9km";
const O1280_LATITUDE_COUNT: usize = 2_560;
const O1280_POINT_COUNT: u64 = 6_599_680;
const SOURCE_CHUNK_POINTS: u64 = 1_024;
const MAX_HEADER_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub struct RegionBounds {
    pub west: f64,
    pub east: f64,
    pub south: f64,
    pub north: f64,
}

#[derive(Debug, Deserialize)]
struct GroupMarker {
    status: String,
    runtime_format: String,
    latest_complete_run: String,
    public_start_utc: String,
    batch_id: String,
    batch_ready_sha256: String,
    artifact_root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct BatchReady {
    schema_version: u32,
    batch_id: String,
    model: String,
    source_run: String,
    metadata: BatchMetadata,
    artifacts: Vec<BatchArtifact>,
}

#[derive(Debug, Deserialize)]
struct BatchMetadata {
    artifact_format: String,
    runs_newest_to_oldest: Vec<String>,
    grid: BatchGrid,
}

#[derive(Debug, Deserialize)]
struct BatchGrid {
    name: String,
    global_points: u64,
    bounds: RegionBounds,
}

#[derive(Debug, Deserialize)]
struct BatchArtifact {
    key: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct PackHeaderWire {
    schema_version: u32,
    format: String,
    source: PackSourceWire,
    forecast: PackForecastWire,
    grid: PackGridWire,
    spatial_chunks: Vec<SpatialChunkWire>,
    variables: Vec<PackVariableWire>,
}

#[derive(Debug, Deserialize)]
struct PackSourceWire {
    license: String,
    modified: bool,
}

#[derive(Debug, Deserialize)]
struct PackForecastWire {
    reference_time: String,
    valid_time: String,
}

#[derive(Debug, Deserialize)]
struct PackGridWire {
    name: String,
    latitude_count: usize,
    global_point_count: u64,
    source_chunk_points: u64,
    bounds: RegionBounds,
}

#[derive(Debug, Deserialize)]
struct SelectedSpanWire {
    offset: u16,
    count: u16,
}

#[derive(Debug, Deserialize)]
struct SpatialChunkWire {
    source_chunk_index: u64,
    selected_spans: Vec<SelectedSpanWire>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct PayloadChunk {
    offset: u64,
    length: u64,
}

#[derive(Debug, Deserialize)]
struct PackVariableWire {
    name: String,
    source_data_type: u8,
    source_compression: u8,
    scale_factor: f32,
    add_offset: f32,
    dimensions: Vec<u64>,
    chunks: Vec<u64>,
    payload_chunks: Vec<PayloadChunk>,
}

#[derive(Debug)]
struct PackVariable {
    data_type: u8,
    compression: u8,
    scale_factor: f32,
    add_offset: f32,
    dimensions: Vec<u64>,
    chunks: Vec<u64>,
    payload_chunks: Vec<PayloadChunk>,
}

#[derive(Debug)]
struct PackHeader {
    data_offset: u64,
    spatial_chunks: Vec<SpatialChunkWire>,
    variables: HashMap<String, PackVariable>,
}

#[derive(Debug)]
pub(crate) struct RegionPackFile {
    path: PathBuf,
    expected_size: u64,
    expected_reference_time: DateTime<Utc>,
    expected_valid_time: DateTime<Utc>,
    header: Mutex<Option<Arc<PackHeader>>>,
}

#[derive(Debug)]
pub(crate) struct RegionPackRun {
    pub(crate) source_run: String,
    pub(crate) reference_time: DateTime<Utc>,
    pub(crate) frames: BTreeMap<DateTime<Utc>, Arc<RegionPackFile>>,
}

#[derive(Debug, Clone)]
pub(crate) struct RegionPackSamplingPlan {
    samples: Vec<Sample>,
    offsets_by_chunk: BTreeMap<u64, Vec<usize>>,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    source_chunk_index: u64,
    offset: usize,
    latitude: f64,
    longitude: f64,
}

#[derive(Debug)]
pub struct RegionPackSnapshot {
    batch_id: String,
    latest_complete_run: String,
    public_start_utc: DateTime<Utc>,
    bounds: RegionBounds,
    runs_oldest_to_newest: Vec<RegionPackRun>,
    available_variables: Vec<String>,
}

impl RegionPackSnapshot {
    pub fn load(data_root: &Path) -> Result<Option<Self>> {
        let marker_path = data_root
            .join("groups")
            .join(GROUP)
            .join("current")
            .join("ready_for_processing.json");
        let marker_bytes = match fs::read(&marker_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", marker_path.display()))
            }
        };
        let marker: GroupMarker = serde_json::from_slice(&marker_bytes)
            .with_context(|| format!("parse {}", marker_path.display()))?;
        validate_identifier(&marker.batch_id, "EC9 batch id")?;
        validate_run(&marker.latest_complete_run)?;
        if marker.status != "complete" || marker.runtime_format != FORMAT {
            bail!("EC9 group marker is not a complete {FORMAT} release");
        }
        let public_start_utc = parse_rfc3339(&marker.public_start_utc)?;
        if !marker.artifact_root.is_absolute() {
            bail!("EC9 artifact_root must be absolute");
        }
        let artifact_root = marker.artifact_root.canonicalize().with_context(|| {
            format!(
                "resolve EC9 artifact root {}",
                marker.artifact_root.display()
            )
        })?;
        let ready_path = artifact_root
            .join("_batches")
            .join(&marker.batch_id)
            .join("ready.json");
        let resolved_ready = ready_path
            .canonicalize()
            .with_context(|| format!("resolve EC9 batch marker {}", ready_path.display()))?;
        if !resolved_ready.starts_with(&artifact_root) {
            bail!("EC9 batch marker escapes artifact_root");
        }
        let ready_bytes = fs::read(&resolved_ready)?;
        let actual_ready_sha = format!("{:x}", Sha256::digest(&ready_bytes));
        if marker.batch_ready_sha256 != actual_ready_sha {
            bail!("EC9 batch marker SHA-256 does not match the published group marker");
        }
        let ready: BatchReady = serde_json::from_slice(&ready_bytes)
            .with_context(|| format!("parse {}", resolved_ready.display()))?;
        if ready.schema_version != 1
            || ready.batch_id != marker.batch_id
            || ready.model != MODEL
            || ready.metadata.artifact_format != FORMAT
            || ready.source_run != marker.latest_complete_run
        {
            bail!("EC9 batch identity does not match the selected group marker");
        }
        if ready.metadata.grid.name != "O1280 reduced Gaussian"
            || ready.metadata.grid.global_points != O1280_POINT_COUNT
        {
            bail!("EC9 batch has an unsupported source grid");
        }
        let newest = ready
            .metadata
            .runs_newest_to_oldest
            .first()
            .context("EC9 batch has no source runs")?;
        if newest != &marker.latest_complete_run {
            bail!("EC9 batch run ordering does not start with latest_complete_run");
        }
        let expected_runs = ready
            .metadata
            .runs_newest_to_oldest
            .iter()
            .map(|run| {
                validate_run(run)?;
                Ok(run.clone())
            })
            .collect::<Result<BTreeSet<_>>>()?;
        if expected_runs.len() != ready.metadata.runs_newest_to_oldest.len() {
            bail!("EC9 batch source runs are duplicated");
        }

        let mut frames_by_run: BTreeMap<String, BTreeMap<DateTime<Utc>, Arc<RegionPackFile>>> =
            BTreeMap::new();
        for artifact in ready.artifacts {
            validate_sha256(&artifact.sha256, "EC9 artifact SHA-256")?;
            let (source_run, valid_time) = parse_artifact_key(&artifact.key)?;
            if !expected_runs.contains(&source_run) {
                bail!("EC9 artifact belongs to an unselected source run: {source_run}");
            }
            let relative = Path::new(&artifact.key);
            let path = artifact_root.join(relative);
            let resolved = path
                .canonicalize()
                .with_context(|| format!("resolve EC9 artifact {}", path.display()))?;
            if !resolved.starts_with(&artifact_root) {
                bail!("EC9 artifact escapes artifact_root");
            }
            let actual_size = resolved.metadata()?.len();
            if actual_size != artifact.size {
                bail!("EC9 artifact size mismatch: {}", resolved.display());
            }
            let reference_time = parse_compact_time(&source_run)?;
            let file = Arc::new(RegionPackFile {
                path: resolved,
                expected_size: artifact.size,
                expected_reference_time: reference_time,
                expected_valid_time: valid_time,
                header: Mutex::new(None),
            });
            if frames_by_run
                .entry(source_run)
                .or_default()
                .insert(valid_time, file)
                .is_some()
            {
                bail!("EC9 batch contains a duplicate run/valid-time artifact");
            }
        }
        if frames_by_run.keys().cloned().collect::<BTreeSet<_>>() != expected_runs {
            bail!("EC9 batch artifact runs do not match its declared source runs");
        }
        let mut runs_oldest_to_newest = Vec::new();
        for source_run in ready.metadata.runs_newest_to_oldest.iter().rev() {
            let frames = frames_by_run
                .remove(source_run)
                .context("EC9 source run has no artifacts")?;
            runs_oldest_to_newest.push(RegionPackRun {
                source_run: source_run.clone(),
                reference_time: parse_compact_time(source_run)?,
                frames,
            });
        }
        let mut snapshot = Self {
            batch_id: marker.batch_id,
            latest_complete_run: marker.latest_complete_run,
            public_start_utc,
            bounds: ready.metadata.grid.bounds,
            runs_oldest_to_newest,
            available_variables: Vec::new(),
        };
        snapshot.available_variables = snapshot.discover_variables()?;
        Ok(Some(snapshot))
    }

    pub fn batch_id(&self) -> &str {
        &self.batch_id
    }

    pub fn latest_complete_run(&self) -> &str {
        &self.latest_complete_run
    }

    pub fn public_start_utc(&self) -> DateTime<Utc> {
        self.public_start_utc
    }

    pub fn bounds(&self) -> RegionBounds {
        self.bounds
    }

    pub fn available_variables(&self) -> &[String] {
        &self.available_variables
    }

    pub fn time_bounds(&self) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        let first = self
            .runs_oldest_to_newest
            .iter()
            .filter_map(|run| run.frames.keys().next().copied())
            .min()
            .context("EC9 snapshot has no first valid time")?;
        let last = self
            .runs_oldest_to_newest
            .iter()
            .filter_map(|run| run.frames.keys().next_back().copied())
            .max()
            .context("EC9 snapshot has no final valid time")?;
        if self.public_start_utc < first || self.public_start_utc > last {
            bail!("EC9 public_start_utc is outside the retained batch coverage");
        }
        Ok((first, last))
    }

    pub(crate) fn runs_oldest_to_newest(&self) -> &[RegionPackRun] {
        &self.runs_oldest_to_newest
    }

    pub(crate) fn sampling_plan(
        &self,
        latitudes: &[f64],
        longitudes: &[f64],
    ) -> Result<RegionPackSamplingPlan> {
        if latitudes.is_empty() || longitudes.is_empty() {
            bail!("EC9 sampling coordinates must not be empty");
        }
        let source_latitudes = o1280_latitudes();
        let row_starts = o1280_row_starts();
        let mut samples = Vec::with_capacity(latitudes.len() * longitudes.len());
        let mut offsets_by_chunk = BTreeMap::<u64, BTreeSet<usize>>::new();
        for &latitude in latitudes {
            if !latitude.is_finite() || latitude < self.bounds.south || latitude > self.bounds.north
            {
                bail!("EC9 latitude is outside the downloaded region: {latitude}");
            }
            let row = nearest_descending(source_latitudes, latitude);
            let points = row_point_count(row);
            let step = 360.0 / points as f64;
            for &longitude in longitudes {
                let normalized = longitude.rem_euclid(360.0);
                if !longitude.is_finite()
                    || normalized < self.bounds.west
                    || normalized > self.bounds.east
                {
                    bail!("EC9 longitude is outside the downloaded region: {longitude}");
                }
                let column = ((normalized / step).round() as u64) % points;
                let global_index = row_starts[row] + column;
                let source_chunk_index = global_index / SOURCE_CHUNK_POINTS;
                let offset = usize::try_from(global_index % SOURCE_CHUNK_POINTS)?;
                offsets_by_chunk
                    .entry(source_chunk_index)
                    .or_default()
                    .insert(offset);
                samples.push(Sample {
                    source_chunk_index,
                    offset,
                    latitude: source_latitudes[row],
                    longitude: column as f64 * step,
                });
            }
        }
        Ok(RegionPackSamplingPlan {
            samples,
            offsets_by_chunk: offsets_by_chunk
                .into_iter()
                .map(|(chunk, offsets)| (chunk, offsets.into_iter().collect()))
                .collect(),
        })
    }

    pub(crate) fn nearest_coordinate(&self, latitude: f64, longitude: f64) -> Result<(f64, f64)> {
        let plan = self.sampling_plan(&[latitude], &[longitude])?;
        let sample = plan.samples[0];
        Ok((sample.latitude, sample.longitude))
    }

    fn discover_variables(&self) -> Result<Vec<String>> {
        let mut variables = BTreeSet::new();
        for run in &self.runs_oldest_to_newest {
            let frame = run
                .frames
                .values()
                // The analysis (lead-zero) frame intentionally omits accumulated
                // fields such as precipitation, showers, and radiation. A later
                // forecast frame carries the complete inventory for this run.
                .next_back()
                .with_context(|| format!("EC9 source run {} has no frames", run.source_run))?;
            let header = frame.header(self.bounds)?;
            variables.extend(header.variables.keys().cloned());
        }
        if variables.is_empty() {
            bail!("EC9 snapshot has no variables");
        }
        // IFS 00Z/12Z long cycles expose more fields than 06Z/18Z short
        // cycles.  The retained-run union is intentional: a variable absent
        // from the latest short cycle (for example snow_depth) is served from
        // the newest older run that structurally covers the requested time.
        Ok(variables.into_iter().collect())
    }
}

impl RegionPackFile {
    fn header(&self, expected_bounds: RegionBounds) -> Result<Arc<PackHeader>> {
        if let Some(header) = self
            .header
            .lock()
            .map_err(|_| anyhow::anyhow!("EC9 header cache lock is poisoned"))?
            .as_ref()
            .cloned()
        {
            return Ok(header);
        }
        let parsed = Arc::new(self.parse_header(expected_bounds)?);
        let mut guard = self
            .header
            .lock()
            .map_err(|_| anyhow::anyhow!("EC9 header cache lock is poisoned"))?;
        Ok(guard.get_or_insert_with(|| parsed.clone()).clone())
    }

    fn parse_header(&self, expected_bounds: RegionBounds) -> Result<PackHeader> {
        let mut file = File::open(&self.path)?;
        let mut prefix = [0_u8; 8];
        file.read_exact(&mut prefix)?;
        if &prefix[..4] != b"WRP1" {
            bail!("invalid EC9 RegionPack magic: {}", self.path.display());
        }
        let header_len = usize::try_from(u32::from_le_bytes(prefix[4..8].try_into().unwrap()))?;
        if header_len == 0 || header_len > MAX_HEADER_BYTES {
            bail!(
                "invalid EC9 RegionPack header length: {}",
                self.path.display()
            );
        }
        let mut bytes = vec![0_u8; header_len];
        file.read_exact(&mut bytes)?;
        let wire: PackHeaderWire = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse RegionPack header {}", self.path.display()))?;
        let reference_time = parse_rfc3339(&wire.forecast.reference_time)?;
        let valid_time = parse_rfc3339(&wire.forecast.valid_time)?;
        if wire.schema_version != 1
            || wire.format != FORMAT
            || wire.source.license != "CC-BY-4.0"
            || !wire.source.modified
            || reference_time != self.expected_reference_time
            || valid_time != self.expected_valid_time
            || wire.grid.name != "O1280 reduced Gaussian"
            || wire.grid.latitude_count != O1280_LATITUDE_COUNT
            || wire.grid.global_point_count != O1280_POINT_COUNT
            || wire.grid.source_chunk_points != SOURCE_CHUNK_POINTS
            || wire.grid.bounds != expected_bounds
        {
            bail!(
                "EC9 RegionPack identity does not match its batch manifest: {}",
                self.path.display()
            );
        }
        if wire.spatial_chunks.is_empty()
            || wire
                .spatial_chunks
                .windows(2)
                .any(|pair| pair[0].source_chunk_index >= pair[1].source_chunk_index)
        {
            bail!("EC9 RegionPack spatial chunk directory is invalid");
        }
        for spatial in &wire.spatial_chunks {
            if spatial.source_chunk_index >= O1280_POINT_COUNT.div_ceil(SOURCE_CHUNK_POINTS)
                || spatial.selected_spans.is_empty()
            {
                bail!("EC9 RegionPack spatial chunk exceeds the source grid");
            }
            let mut previous_end = 0_u32;
            for span in &spatial.selected_spans {
                let start = u32::from(span.offset);
                let end = start + u32::from(span.count);
                if span.count == 0 || start < previous_end || end > SOURCE_CHUNK_POINTS as u32 {
                    bail!("EC9 RegionPack selected spans are invalid");
                }
                previous_end = end;
            }
        }
        let data_offset = 8_u64 + u64::try_from(header_len)?;
        let payload_bytes = self
            .expected_size
            .checked_sub(data_offset)
            .context("EC9 RegionPack header exceeds file size")?;
        let mut variables = HashMap::new();
        let mut previous_payload_end = 0_u64;
        for variable in wire.variables {
            let normalized_shape =
                normalize_source_spatial_shape(&variable.dimensions, &variable.chunks);
            if variable.name.is_empty()
                || normalized_shape.is_none()
                || variable.source_data_type != 20
                || variable.payload_chunks.len() != wire.spatial_chunks.len()
            {
                bail!(
                    "EC9 RegionPack variable metadata is unsupported: {}",
                    variable.name
                );
            }
            let (dimensions, chunks) = normalized_shape.expect("shape was validated above");
            for payload in &variable.payload_chunks {
                if payload.length == 0
                    || payload.offset < previous_payload_end
                    || payload
                        .offset
                        .checked_add(payload.length)
                        .is_none_or(|end| end > payload_bytes)
                {
                    bail!("EC9 RegionPack payload directory is invalid");
                }
                previous_payload_end = payload.offset + payload.length;
            }
            let compact = PackVariable {
                data_type: variable.source_data_type,
                compression: variable.source_compression,
                scale_factor: variable.scale_factor,
                add_offset: variable.add_offset,
                dimensions,
                chunks,
                payload_chunks: variable.payload_chunks,
            };
            if variables.insert(variable.name, compact).is_some() {
                bail!("EC9 RegionPack contains a duplicate variable");
            }
        }
        if variables.is_empty() || previous_payload_end != payload_bytes {
            bail!("EC9 RegionPack payload is incomplete");
        }
        Ok(PackHeader {
            data_offset,
            spatial_chunks: wire.spatial_chunks,
            variables,
        })
    }

    pub(crate) fn decode(
        &self,
        decoder: &OfficialDecoder,
        expected_bounds: RegionBounds,
        variable: &str,
        plan: &RegionPackSamplingPlan,
    ) -> Result<Option<Vec<f32>>> {
        let header = self.header(expected_bounds)?;
        let Some(variable) = header.variables.get(variable) else {
            return Ok(None);
        };
        let mut decoded: BTreeMap<u64, Vec<f32>> = BTreeMap::new();
        let mut file = File::open(&self.path)?;
        for (&source_chunk_index, offsets) in &plan.offsets_by_chunk {
            let position = header
                .spatial_chunks
                .binary_search_by_key(&source_chunk_index, |chunk| chunk.source_chunk_index)
                .map_err(|_| anyhow::anyhow!("requested EC9 point is outside the retained crop"))?;
            let spatial = &header.spatial_chunks[position];
            let payload = variable.payload_chunks[position];
            let mut bytes = vec![0_u8; usize::try_from(payload.length)?];
            file.seek(SeekFrom::Start(header.data_offset + payload.offset))?;
            file.read_exact(&mut bytes)?;
            let values = decoder.decode_compressed_array_chunk(
                &variable.dimensions,
                &variable.chunks,
                variable.scale_factor,
                variable.add_offset,
                variable.data_type,
                variable.compression,
                source_chunk_index,
                &bytes,
            )?;
            for &offset in offsets {
                let retained = spatial.selected_spans.iter().any(|span| {
                    let start = usize::from(span.offset);
                    offset >= start && offset < start + usize::from(span.count)
                });
                if !retained {
                    bail!("requested EC9 point is outside an edge-chunk selected span");
                }
            }
            decoded.insert(source_chunk_index, values);
        }
        let mut output = Vec::with_capacity(plan.samples.len());
        for sample in &plan.samples {
            output.push(
                decoded
                    .get(&sample.source_chunk_index)
                    .and_then(|values| values.get(sample.offset))
                    .copied()
                    .context("decoded EC9 chunk does not contain the requested point")?,
            );
        }
        Ok(Some(output))
    }
}

fn normalize_source_spatial_shape(
    dimensions: &[u64],
    chunks: &[u64],
) -> Option<(Vec<u64>, Vec<u64>)> {
    // ECMWF spatial files carry a leading singleton time dimension. Each
    // RegionPack frame contains exactly that one time slice, so normalize the
    // retained compressed chunks to the equivalent one-dimensional array used
    // by the direct official decoder.
    if dimensions == [1, O1280_POINT_COUNT] && chunks == [1, SOURCE_CHUNK_POINTS] {
        Some((vec![O1280_POINT_COUNT], vec![SOURCE_CHUNK_POINTS]))
    } else {
        None
    }
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid {label}");
    }
    Ok(())
}

fn validate_run(value: &str) -> Result<()> {
    if value.len() != 10 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid EC9 source run: {value}");
    }
    parse_compact_time(value).map(|_| ())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid {label}");
    }
    Ok(())
}

fn parse_artifact_key(key: &str) -> Result<(String, DateTime<Utc>)> {
    let path = Path::new(key);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("unsafe EC9 artifact key");
    }
    let components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if components.len() != 4 || components[0] != MODEL || components[2] != "frames" {
        bail!("unexpected EC9 artifact key layout: {key}");
    }
    validate_run(&components[1])?;
    let valid_text = components[3]
        .strip_suffix(".regionpack")
        .context("EC9 artifact is not a RegionPack")?;
    if valid_text.len() != 12 || !valid_text.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid EC9 artifact valid time");
    }
    Ok((
        components[1].clone(),
        NaiveDateTime::parse_from_str(valid_text, "%Y%m%d%H%M")?.and_utc(),
    ))
}

fn parse_compact_time(value: &str) -> Result<DateTime<Utc>> {
    Ok(NaiveDateTime::parse_from_str(&format!("{value}00"), "%Y%m%d%H%M")?.and_utc())
}

fn parse_rfc3339(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn o1280_latitudes() -> &'static [f64] {
    static LATITUDES: OnceLock<Vec<f64>> = OnceLock::new();
    LATITUDES.get_or_init(|| {
        let order = O1280_LATITUDE_COUNT;
        let mut north = Vec::with_capacity(order / 2);
        for root_index in 0..order / 2 {
            let mut root =
                (std::f64::consts::PI * (root_index as f64 + 0.75) / (order as f64 + 0.5)).cos();
            for _ in 0..12 {
                let mut previous = 1.0_f64;
                let mut current = root;
                for degree in 2..=order {
                    let next = ((2 * degree - 1) as f64 * root * current
                        - (degree - 1) as f64 * previous)
                        / degree as f64;
                    previous = current;
                    current = next;
                }
                let derivative = order as f64 * (root * current - previous) / (root * root - 1.0);
                let next = root - current / derivative;
                if (next - root).abs() < 2.0e-15 {
                    root = next;
                    break;
                }
                root = next;
            }
            north.push(root.asin().to_degrees());
        }
        let mut latitudes = north.clone();
        latitudes.extend(north.iter().rev().map(|value| -*value));
        latitudes
    })
}

fn row_point_count(row: usize) -> u64 {
    let pole_distance = row.min(O1280_LATITUDE_COUNT - 1 - row) as u64;
    20 + 4 * pole_distance
}

fn o1280_row_starts() -> &'static [u64] {
    static ROW_STARTS: OnceLock<Vec<u64>> = OnceLock::new();
    ROW_STARTS.get_or_init(|| {
        let mut starts = Vec::with_capacity(O1280_LATITUDE_COUNT);
        let mut offset = 0_u64;
        for row in 0..O1280_LATITUDE_COUNT {
            starts.push(offset);
            offset += row_point_count(row);
        }
        assert_eq!(offset, O1280_POINT_COUNT);
        starts
    })
}

fn nearest_descending(values: &[f64], target: f64) -> usize {
    let insertion = values.partition_point(|value| *value > target);
    if insertion == 0 {
        return 0;
    }
    if insertion == values.len() {
        return values.len() - 1;
    }
    let north = insertion - 1;
    let south = insertion;
    if (values[north] - target).abs() <= (values[south] - target).abs() {
        north
    } else {
        south
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o1280_geometry_matches_the_source_contract() {
        assert_eq!(o1280_latitudes().len(), O1280_LATITUDE_COUNT);
        assert_eq!(
            o1280_row_starts()[O1280_LATITUDE_COUNT - 1]
                + row_point_count(O1280_LATITUDE_COUNT - 1),
            O1280_POINT_COUNT
        );
        let row = nearest_descending(o1280_latitudes(), 31.2304);
        assert!((o1280_latitudes()[row] - 31.2304).abs() < 0.1);
    }

    #[test]
    fn artifact_keys_cannot_escape_the_private_artifact_root() {
        assert!(parse_artifact_key(
            "ecmwf_ifs9km_omdata/2026090112/frames/202609020000.regionpack"
        )
        .is_ok());
        assert!(parse_artifact_key("../secret.regionpack").is_err());
        assert!(
            parse_artifact_key("ecmwf_ifs9km_omdata/2026090112/../202609020000.regionpack")
                .is_err()
        );
    }

    #[test]
    fn source_singleton_time_dimension_is_normalized_for_direct_decoding() {
        assert_eq!(
            normalize_source_spatial_shape(&[1, O1280_POINT_COUNT], &[1, SOURCE_CHUNK_POINTS]),
            Some((vec![O1280_POINT_COUNT], vec![SOURCE_CHUNK_POINTS]))
        );
        assert!(
            normalize_source_spatial_shape(&[2, O1280_POINT_COUNT], &[1, SOURCE_CHUNK_POINTS])
                .is_none()
        );
    }
}
