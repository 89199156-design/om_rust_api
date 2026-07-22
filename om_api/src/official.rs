use anyhow::{bail, Context, Result};
use libloading::Library;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct OmRange {
    lower_bound: u64,
    upper_bound: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct OmDecoderIndexRead {
    offset: u64,
    count: u64,
    index_range: OmRange,
    chunk_index: OmRange,
    next_chunk: OmRange,
}

type OmDecoderDataRead = OmDecoderIndexRead;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct OmDecoder {
    dimensions_count: u64,
    io_size_merge: u64,
    io_size_max: u64,
    lut_chunk_length: u64,
    lut_start: u64,
    number_of_chunks: u64,
    dimensions: *const u64,
    chunks: *const u64,
    read_offset: *const u64,
    read_count: *const u64,
    cube_dimensions: *const u64,
    cube_offset: *const u64,
    scale_factor: f32,
    add_offset: f32,
    data_type: u8,
    compression: u8,
    bytes_per_element: u8,
    bytes_per_element_compressed: u8,
}

type OmVariableInit = unsafe extern "C" fn(*const c_void) -> *const c_void;
type OmDecoderInit = unsafe extern "C" fn(
    *mut OmDecoder,
    *const c_void,
    u64,
    *const u64,
    *const u64,
    *const u64,
    *const u64,
    u64,
    u64,
) -> u32;
type OmDecoderInitIndexRead = unsafe extern "C" fn(*const OmDecoder, *mut OmDecoderIndexRead);
type OmDecoderNextIndexRead =
    unsafe extern "C" fn(*const OmDecoder, *mut OmDecoderIndexRead) -> bool;
type OmDecoderInitDataRead =
    unsafe extern "C" fn(*mut OmDecoderDataRead, *const OmDecoderIndexRead);
type OmDecoderNextDataRead = unsafe extern "C" fn(
    *const OmDecoder,
    *mut OmDecoderDataRead,
    *const c_void,
    u64,
    *mut u32,
) -> bool;
type OmDecoderReadBufferSize = unsafe extern "C" fn(*const OmDecoder) -> u64;
type OmDecoderDecodeChunks = unsafe extern "C" fn(
    *const OmDecoder,
    OmRange,
    *const c_void,
    u64,
    *mut c_void,
    *mut c_void,
    *mut u32,
) -> bool;
type OmErrorString = unsafe extern "C" fn(u32) -> *const c_char;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct OmEncoder {
    dimensions: *const u64,
    chunks: *const u64,
    dimension_count: u64,
    scale_factor: f32,
    add_offset: f32,
    data_type: u8,
    compression: u8,
    bytes_per_element: u8,
    bytes_per_element_compressed: u8,
}

type OmEncoderInit = unsafe extern "C" fn(
    *mut OmEncoder,
    f32,
    f32,
    c_int,
    c_int,
    *const u64,
    *const u64,
    u64,
) -> c_int;
type OmEncoderCountChunks = unsafe extern "C" fn(*const OmEncoder) -> u64;
type OmEncoderCountChunksInArray = unsafe extern "C" fn(*const OmEncoder, *const u64) -> u64;
type OmEncoderBufferSize = unsafe extern "C" fn(*const OmEncoder) -> u64;
type OmEncoderCompressChunk = unsafe extern "C" fn(
    *const OmEncoder,
    *const c_void,
    *const u64,
    *const u64,
    *const u64,
    u64,
    u64,
    *mut u8,
    *mut u8,
) -> u64;
type OmEncoderLutBufferSize = unsafe extern "C" fn(*const u64, u64) -> u64;
type OmEncoderCompressLut = unsafe extern "C" fn(*const u64, u64, *mut u8, u64) -> u64;
type OmHeaderWriteSize = unsafe extern "C" fn() -> usize;
type OmHeaderWrite = unsafe extern "C" fn(*mut c_void);
type OmTrailerSize = unsafe extern "C" fn() -> usize;
type OmTrailerWrite = unsafe extern "C" fn(*mut c_void, u64, u64);
type OmVariableWriteNumericArraySize = unsafe extern "C" fn(u16, u32, u64) -> usize;
type OmVariableWriteNumericArray = unsafe extern "C" fn(
    *mut c_void,
    u16,
    u32,
    *const u64,
    *const u64,
    *const c_char,
    c_int,
    c_int,
    f32,
    f32,
    u64,
    *const u64,
    *const u64,
    u64,
    u64,
);

#[derive(Clone)]
pub struct OfficialDecoder {
    inner: Arc<OfficialDecoderInner>,
}

struct OfficialDecoderInner {
    _library: Library,
    om_variable_init: OmVariableInit,
    om_decoder_init: OmDecoderInit,
    om_decoder_init_index_read: OmDecoderInitIndexRead,
    om_decoder_next_index_read: OmDecoderNextIndexRead,
    om_decoder_init_data_read: OmDecoderInitDataRead,
    om_decoder_next_data_read: OmDecoderNextDataRead,
    om_decoder_read_buffer_size: OmDecoderReadBufferSize,
    om_decoder_decode_chunks: OmDecoderDecodeChunks,
    om_error_string: OmErrorString,
    encoder: Option<OfficialEncoderFunctions>,
}

#[derive(Clone, Copy)]
struct OfficialEncoderFunctions {
    om_encoder_init: OmEncoderInit,
    om_encoder_count_chunks: OmEncoderCountChunks,
    om_encoder_count_chunks_in_array: OmEncoderCountChunksInArray,
    om_encoder_chunk_buffer_size: OmEncoderBufferSize,
    om_encoder_compressed_chunk_buffer_size: OmEncoderBufferSize,
    om_encoder_compress_chunk: OmEncoderCompressChunk,
    om_encoder_lut_buffer_size: OmEncoderLutBufferSize,
    om_encoder_compress_lut: OmEncoderCompressLut,
    om_header_write_size: OmHeaderWriteSize,
    om_header_write: OmHeaderWrite,
    om_trailer_size: OmTrailerSize,
    om_trailer_write: OmTrailerWrite,
    om_variable_write_numeric_array_size: OmVariableWriteNumericArraySize,
    om_variable_write_numeric_array: OmVariableWriteNumericArray,
}

unsafe impl Send for OfficialDecoderInner {}
unsafe impl Sync for OfficialDecoderInner {}

pub trait BundleRangeReader {
    fn read_original_range(&self, start: u64, count: u64) -> Result<Vec<u8>>;
}

impl OfficialDecoder {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let library = unsafe { Library::new(path.as_ref()) }
            .with_context(|| format!("failed to load {}", path.as_ref().display()))?;
        let inner = unsafe {
            let om_variable_init = *library.get::<OmVariableInit>(b"om_variable_init\0")?;
            let om_decoder_init = *library.get::<OmDecoderInit>(b"om_decoder_init\0")?;
            let om_decoder_init_index_read =
                *library.get::<OmDecoderInitIndexRead>(b"om_decoder_init_index_read\0")?;
            let om_decoder_next_index_read =
                *library.get::<OmDecoderNextIndexRead>(b"om_decoder_next_index_read\0")?;
            let om_decoder_init_data_read =
                *library.get::<OmDecoderInitDataRead>(b"om_decoder_init_data_read\0")?;
            let om_decoder_next_data_read =
                *library.get::<OmDecoderNextDataRead>(b"om_decoder_next_data_read\0")?;
            let om_decoder_read_buffer_size =
                *library.get::<OmDecoderReadBufferSize>(b"om_decoder_read_buffer_size\0")?;
            let om_decoder_decode_chunks =
                *library.get::<OmDecoderDecodeChunks>(b"om_decoder_decode_chunks\0")?;
            let om_error_string = *library.get::<OmErrorString>(b"om_error_string\0")?;
            // Decoding remains usable with a decoder-only library. Production
            // materialisation explicitly requires the complete encoder ABI and
            // reports that at writer creation time.
            let encoder = (|| -> Result<OfficialEncoderFunctions> {
                Ok(OfficialEncoderFunctions {
                    om_encoder_init: *library.get::<OmEncoderInit>(b"om_encoder_init\0")?,
                    om_encoder_count_chunks: *library
                        .get::<OmEncoderCountChunks>(b"om_encoder_count_chunks\0")?,
                    om_encoder_count_chunks_in_array: *library.get::<OmEncoderCountChunksInArray>(
                        b"om_encoder_count_chunks_in_array\0",
                    )?,
                    om_encoder_chunk_buffer_size: *library
                        .get::<OmEncoderBufferSize>(b"om_encoder_chunk_buffer_size\0")?,
                    om_encoder_compressed_chunk_buffer_size: *library
                        .get::<OmEncoderBufferSize>(b"om_encoder_compressed_chunk_buffer_size\0")?,
                    om_encoder_compress_chunk: *library
                        .get::<OmEncoderCompressChunk>(b"om_encoder_compress_chunk\0")?,
                    om_encoder_lut_buffer_size: *library
                        .get::<OmEncoderLutBufferSize>(b"om_encoder_lut_buffer_size\0")?,
                    om_encoder_compress_lut: *library
                        .get::<OmEncoderCompressLut>(b"om_encoder_compress_lut\0")?,
                    om_header_write_size: *library
                        .get::<OmHeaderWriteSize>(b"om_header_write_size\0")?,
                    om_header_write: *library.get::<OmHeaderWrite>(b"om_header_write\0")?,
                    om_trailer_size: *library.get::<OmTrailerSize>(b"om_trailer_size\0")?,
                    om_trailer_write: *library.get::<OmTrailerWrite>(b"om_trailer_write\0")?,
                    om_variable_write_numeric_array_size: *library
                        .get::<OmVariableWriteNumericArraySize>(
                        b"om_variable_write_numeric_array_size\0",
                    )?,
                    om_variable_write_numeric_array: *library
                        .get::<OmVariableWriteNumericArray>(b"om_variable_write_numeric_array\0")?,
                })
            })()
            .ok();
            OfficialDecoderInner {
                _library: library,
                om_variable_init,
                om_decoder_init,
                om_decoder_init_index_read,
                om_decoder_next_index_read,
                om_decoder_init_data_read,
                om_decoder_next_data_read,
                om_decoder_read_buffer_size,
                om_decoder_decode_chunks,
                om_error_string,
                encoder,
            }
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn decode_point(
        &self,
        variable_metadata: &[u64],
        reader: &dyn BundleRangeReader,
        read_offset: &[u64],
    ) -> Result<f32> {
        let read_count = vec![1_u64; read_offset.len()];
        Ok(self.decode_grid(variable_metadata, reader, read_offset, &read_count)?[0])
    }

    pub fn decode_grid(
        &self,
        variable_metadata: &[u64],
        reader: &dyn BundleRangeReader,
        read_offset: &[u64],
        read_count: &[u64],
    ) -> Result<Vec<f32>> {
        if read_offset.len() != read_count.len() || read_offset.is_empty() {
            bail!("read_offset and read_count dimensions must match");
        }
        let n_dimensions = read_offset.len();
        let cube_offset = vec![0_u64; n_dimensions];
        let cube_dimensions = read_count.to_vec();
        let io_size_merge = if read_count.iter().all(|value| *value == 1) {
            512
        } else {
            0
        };
        let variable_ptr =
            unsafe { (self.inner.om_variable_init)(variable_metadata.as_ptr() as *const c_void) };
        let mut decoder = OmDecoder {
            dimensions_count: 0,
            io_size_merge: 0,
            io_size_max: 0,
            lut_chunk_length: 0,
            lut_start: 0,
            number_of_chunks: 0,
            dimensions: std::ptr::null(),
            chunks: std::ptr::null(),
            read_offset: std::ptr::null(),
            read_count: std::ptr::null(),
            cube_dimensions: std::ptr::null(),
            cube_offset: std::ptr::null(),
            scale_factor: 1.0,
            add_offset: 0.0,
            data_type: 0,
            compression: 0,
            bytes_per_element: 0,
            bytes_per_element_compressed: 0,
        };
        let error = unsafe {
            (self.inner.om_decoder_init)(
                &mut decoder,
                variable_ptr,
                n_dimensions as u64,
                read_offset.as_ptr(),
                read_count.as_ptr(),
                cube_offset.as_ptr(),
                cube_dimensions.as_ptr(),
                io_size_merge,
                1024 * 1024 * 64,
            )
        };
        self.ensure_ok(error)?;

        let mut index_read = OmDecoderIndexRead {
            offset: 0,
            count: 0,
            index_range: OmRange {
                lower_bound: 0,
                upper_bound: 0,
            },
            chunk_index: OmRange {
                lower_bound: 0,
                upper_bound: 0,
            },
            next_chunk: OmRange {
                lower_bound: 0,
                upper_bound: 0,
            },
        };
        let output_count = read_count.iter().try_fold(1_usize, |total, value| {
            total
                .checked_mul(*value as usize)
                .ok_or_else(|| anyhow::anyhow!("decoder output size overflow"))
        })?;
        let mut output = vec![f32::NAN; output_count];
        let mut chunk_buffer =
            vec![0_u8; unsafe { (self.inner.om_decoder_read_buffer_size)(&decoder) } as usize];

        unsafe {
            (self.inner.om_decoder_init_index_read)(&decoder, &mut index_read);
        }

        while unsafe { (self.inner.om_decoder_next_index_read)(&decoder, &mut index_read) } {
            let index_data = reader.read_original_range(index_read.offset, index_read.count)?;
            let mut data_read = OmDecoderDataRead {
                offset: 0,
                count: 0,
                index_range: index_read.index_range,
                chunk_index: OmRange {
                    lower_bound: 0,
                    upper_bound: 0,
                },
                next_chunk: index_read.chunk_index,
            };
            unsafe {
                (self.inner.om_decoder_init_data_read)(&mut data_read, &index_read);
            }
            let mut error = 0_u32;
            while unsafe {
                (self.inner.om_decoder_next_data_read)(
                    &decoder,
                    &mut data_read,
                    index_data.as_ptr() as *const c_void,
                    index_data.len() as u64,
                    &mut error,
                )
            } {
                self.ensure_ok(error)?;
                let data = reader.read_original_range(data_read.offset, data_read.count)?;
                let ok = unsafe {
                    (self.inner.om_decoder_decode_chunks)(
                        &decoder,
                        data_read.chunk_index,
                        data.as_ptr() as *const c_void,
                        data.len() as u64,
                        output.as_mut_ptr() as *mut c_void,
                        chunk_buffer.as_mut_ptr() as *mut c_void,
                        &mut error,
                    )
                };
                if !ok {
                    self.ensure_ok(error)?;
                    bail!("official OM decoder failed without an error code");
                }
            }
            self.ensure_ok(error)?;
        }
        Ok(output)
    }

    fn ensure_ok(&self, error: u32) -> Result<()> {
        if error == 0 {
            return Ok(());
        }
        let message = unsafe {
            let ptr = (self.inner.om_error_string)(error);
            if ptr.is_null() {
                format!("OM decoder error {}", error)
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        bail!(message)
    }

    pub fn create_array_writer(
        &self,
        path: impl AsRef<Path>,
        dimensions: Vec<u64>,
        chunks: Vec<u64>,
        scale_factor: f32,
        add_offset: f32,
        data_type: u8,
        compression: u8,
    ) -> Result<OfficialArrayWriter> {
        OfficialArrayWriter::create(
            self.clone(),
            path.as_ref(),
            dimensions,
            chunks,
            scale_factor,
            add_offset,
            data_type,
            compression,
        )
    }
}

/// Streaming writer for a single OM v3 root array.
///
/// The implementation follows the pinned Open-Meteo C/Swift writer contract:
/// data chunks, compressed LUT, 64-byte-aligned root metadata, then trailer.
/// Input blocks must be supplied in global chunk order (normally y-major with
/// the complete x/time extent in each block).
pub struct OfficialArrayWriter {
    // Keeps the dynamic library loaded for every copied encoder function.
    _decoder: OfficialDecoder,
    functions: OfficialEncoderFunctions,
    file: Option<File>,
    target_path: PathBuf,
    temporary_path: PathBuf,
    dimensions: Vec<u64>,
    chunks: Vec<u64>,
    encoder: OmEncoder,
    total_chunks: u64,
    chunk_index: u64,
    lookup_table: Vec<u64>,
    chunk_buffer: Vec<u8>,
    compressed_buffer: Vec<u8>,
    bytes_written: u64,
    scale_factor: f32,
    add_offset: f32,
    data_type: u8,
    compression: u8,
    finished: bool,
}

impl OfficialArrayWriter {
    #[allow(clippy::too_many_arguments)]
    fn create(
        decoder: OfficialDecoder,
        target_path: &Path,
        dimensions: Vec<u64>,
        chunks: Vec<u64>,
        scale_factor: f32,
        add_offset: f32,
        data_type: u8,
        compression: u8,
    ) -> Result<Self> {
        if dimensions.is_empty() || dimensions.len() != chunks.len() {
            bail!("OM writer dimensions and chunks must be non-empty and equal length");
        }
        if !scale_factor.is_finite() || scale_factor <= 0.0 || !add_offset.is_finite() {
            bail!("OM writer scale factor/offset are invalid");
        }
        let parent = target_path
            .parent()
            .context("OM writer target has no parent directory")?;
        fs::create_dir_all(parent)?;
        let file_name = target_path
            .file_name()
            .and_then(|value| value.to_str())
            .context("OM writer target filename is not UTF-8")?;
        let temporary_path = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("worker")
        ));
        let functions = decoder
            .inner
            .encoder
            .context("official OM library does not export the complete encoder ABI")?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
            .with_context(|| format!("create OM staging file {}", temporary_path.display()))?;

        let mut header = vec![0_u8; unsafe { (functions.om_header_write_size)() }];
        unsafe {
            (functions.om_header_write)(header.as_mut_ptr() as *mut c_void);
        }
        file.write_all(&header)?;
        let bytes_written = u64::try_from(header.len())?;

        let mut encoder = OmEncoder {
            dimensions: std::ptr::null(),
            chunks: std::ptr::null(),
            dimension_count: 0,
            scale_factor,
            add_offset,
            data_type,
            compression,
            bytes_per_element: 0,
            bytes_per_element_compressed: 0,
        };
        let error = unsafe {
            (functions.om_encoder_init)(
                &mut encoder,
                scale_factor,
                add_offset,
                compression.into(),
                data_type.into(),
                dimensions.as_ptr(),
                chunks.as_ptr(),
                dimensions.len() as u64,
            )
        };
        decoder.ensure_ok(error as u32)?;
        let total_chunks = unsafe { (functions.om_encoder_count_chunks)(&encoder) };
        if total_chunks == 0 {
            bail!("OM writer calculated zero chunks");
        }
        let chunk_buffer_size = unsafe { (functions.om_encoder_chunk_buffer_size)(&encoder) };
        let compressed_buffer_size =
            unsafe { (functions.om_encoder_compressed_chunk_buffer_size)(&encoder) };
        let lookup_table = vec![0_u64; usize::try_from(total_chunks + 1)?];

        let mut writer = Self {
            _decoder: decoder,
            functions,
            file: Some(file),
            target_path: target_path.to_path_buf(),
            temporary_path,
            dimensions,
            chunks,
            encoder,
            total_chunks,
            chunk_index: 0,
            lookup_table,
            chunk_buffer: vec![0_u8; usize::try_from(chunk_buffer_size)?],
            compressed_buffer: vec![0_u8; usize::try_from(compressed_buffer_size)?],
            bytes_written,
            scale_factor,
            add_offset,
            data_type,
            compression,
            finished: false,
        };
        // OmEncoder retains pointers to these heap allocations. Moving Vec or
        // the writer does not move their allocated buffers; never resize them.
        writer.encoder.dimensions = writer.dimensions.as_ptr();
        writer.encoder.chunks = writer.chunks.as_ptr();
        writer.lookup_table[0] = writer.bytes_written;
        Ok(writer)
    }

    pub fn write_f32_block(&mut self, data: &[f32], block_dimensions: &[u64]) -> Result<()> {
        if self.data_type != 20 {
            bail!("write_f32_block requires DATA_TYPE_FLOAT_ARRAY");
        }
        if block_dimensions.len() != self.dimensions.len() {
            bail!("OM writer block dimension count mismatch");
        }
        let expected = block_dimensions.iter().try_fold(1_usize, |total, value| {
            total
                .checked_mul(usize::try_from(*value)?)
                .context("OM writer block size overflow")
        })?;
        if expected != data.len() {
            bail!(
                "OM writer block contains {} values, expected {}",
                data.len(),
                expected
            );
        }
        if block_dimensions
            .iter()
            .zip(&self.dimensions)
            .any(|(block, total)| block == &0 || block > total)
        {
            bail!("OM writer block dimensions exceed target dimensions");
        }
        let block_chunks = unsafe {
            (self.functions.om_encoder_count_chunks_in_array)(
                &self.encoder,
                block_dimensions.as_ptr(),
            )
        };
        if self
            .chunk_index
            .checked_add(block_chunks)
            .is_none_or(|end| end > self.total_chunks)
        {
            bail!("OM writer block exceeds target chunk count");
        }
        let offsets = vec![0_u64; block_dimensions.len()];
        for local_chunk in 0..block_chunks {
            let size = unsafe {
                (self.functions.om_encoder_compress_chunk)(
                    &self.encoder,
                    data.as_ptr() as *const c_void,
                    block_dimensions.as_ptr(),
                    offsets.as_ptr(),
                    block_dimensions.as_ptr(),
                    self.chunk_index,
                    local_chunk,
                    self.compressed_buffer.as_mut_ptr(),
                    self.chunk_buffer.as_mut_ptr(),
                )
            };
            let size = usize::try_from(size)?;
            if size > self.compressed_buffer.len() {
                bail!("OM encoder exceeded its advertised compressed buffer size");
            }
            self.file
                .as_mut()
                .context("OM writer is already closed")?
                .write_all(&self.compressed_buffer[..size])?;
            self.bytes_written = self
                .bytes_written
                .checked_add(size as u64)
                .context("OM writer file size overflow")?;
            self.chunk_index += 1;
            self.lookup_table[usize::try_from(self.chunk_index)?] = self.bytes_written;
        }
        Ok(())
    }

    pub fn finish(mut self, root_name: &str) -> Result<u64> {
        if self.chunk_index != self.total_chunks {
            bail!(
                "OM writer received {} chunks, expected {}",
                self.chunk_index,
                self.total_chunks
            );
        }
        let lut_offset = self.bytes_written;
        let lut_buffer_size = unsafe {
            (self.functions.om_encoder_lut_buffer_size)(
                self.lookup_table.as_ptr(),
                self.lookup_table.len() as u64,
            )
        };
        let mut lut_buffer = vec![0_u8; usize::try_from(lut_buffer_size)?];
        let lut_size = unsafe {
            (self.functions.om_encoder_compress_lut)(
                self.lookup_table.as_ptr(),
                self.lookup_table.len() as u64,
                lut_buffer.as_mut_ptr(),
                lut_buffer_size,
            )
        };
        if lut_size > lut_buffer_size {
            bail!("OM encoder exceeded its advertised LUT buffer size");
        }
        self.file_mut()?
            .write_all(&lut_buffer[..usize::try_from(lut_size)?])?;
        self.bytes_written += lut_size;
        self.align_64()?;

        let name = root_name.as_bytes();
        let name_size = u16::try_from(name.len()).context("OM root variable name is too long")?;
        let root_size = unsafe {
            (self.functions.om_variable_write_numeric_array_size)(
                name_size,
                0,
                self.dimensions.len() as u64,
            )
        };
        let root_offset = self.bytes_written;
        let mut root = vec![0_u8; root_size];
        unsafe {
            (self.functions.om_variable_write_numeric_array)(
                root.as_mut_ptr() as *mut c_void,
                name_size,
                0,
                std::ptr::null(),
                std::ptr::null(),
                name.as_ptr() as *const c_char,
                self.data_type.into(),
                self.compression.into(),
                self.scale_factor,
                self.add_offset,
                self.dimensions.len() as u64,
                self.dimensions.as_ptr(),
                self.chunks.as_ptr(),
                lut_size,
                lut_offset,
            );
        }
        self.file_mut()?.write_all(&root)?;
        self.bytes_written += u64::try_from(root.len())?;
        self.align_64()?;

        let mut trailer = vec![0_u8; unsafe { (self.functions.om_trailer_size)() }];
        unsafe {
            (self.functions.om_trailer_write)(
                trailer.as_mut_ptr() as *mut c_void,
                root_offset,
                u64::try_from(root_size)?,
            );
        }
        self.file_mut()?.write_all(&trailer)?;
        self.bytes_written += u64::try_from(trailer.len())?;
        let file = self.file.take().context("OM writer file disappeared")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.temporary_path, &self.target_path).with_context(|| {
            format!(
                "promote OM staging file {} to {}",
                self.temporary_path.display(),
                self.target_path.display()
            )
        })?;
        if let Some(parent) = self.target_path.parent() {
            File::open(parent)?.sync_all()?;
        }
        self.finished = true;
        Ok(self.bytes_written)
    }

    fn align_64(&mut self) -> Result<()> {
        let padding = (64 - self.bytes_written % 64) % 64;
        if padding > 0 {
            self.file_mut()?
                .write_all(&[0_u8; 64][..usize::try_from(padding)?])?;
            self.bytes_written += padding;
        }
        Ok(())
    }

    fn file_mut(&mut self) -> Result<&mut File> {
        self.file.as_mut().context("OM writer is already closed")
    }
}

impl Drop for OfficialArrayWriter {
    fn drop(&mut self) {
        if !self.finished {
            self.file.take();
            let _ = fs::remove_file(&self.temporary_path);
        }
    }
}

pub fn build_v3_array_metadata_blob(
    name: &str,
    data_type: u8,
    compression: u8,
    dimensions: &[u64],
    chunks: &[u64],
    lut_size: u64,
    lut_offset: u64,
    scale_factor: f32,
    add_offset: f32,
) -> Vec<u64> {
    let mut bytes = Vec::with_capacity(40 + dimensions.len() * 16 + name.len());
    bytes.push(data_type);
    bytes.push(compression);
    bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&lut_size.to_le_bytes());
    bytes.extend_from_slice(&lut_offset.to_le_bytes());
    bytes.extend_from_slice(&(dimensions.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&scale_factor.to_le_bytes());
    bytes.extend_from_slice(&add_offset.to_le_bytes());
    for value in dimensions {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for value in chunks {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(name.as_bytes());
    let words = (bytes.len() + 7) / 8;
    let mut aligned = vec![0_u64; words];
    let aligned_bytes =
        unsafe { std::slice::from_raw_parts_mut(aligned.as_mut_ptr() as *mut u8, words * 8) };
    aligned_bytes[..bytes.len()].copy_from_slice(&bytes);
    aligned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::read_native_array_metadata;

    struct FileRangeReader(std::sync::Mutex<File>);

    impl BundleRangeReader for FileRangeReader {
        fn read_original_range(&self, start: u64, count: u64) -> Result<Vec<u8>> {
            let mut output = vec![0_u8; usize::try_from(count)?];
            let mut file = self.0.lock().expect("test file reader lock poisoned");
            std::io::Seek::seek(&mut *file, std::io::SeekFrom::Start(start))?;
            std::io::Read::read_exact(&mut *file, &mut output)?;
            Ok(output)
        }
    }

    #[test]
    fn writer_round_trips_with_pinned_official_codec_when_configured() {
        let Some(library) = std::env::var_os("OM_TEST_OMFILE_LIB") else {
            return;
        };
        let decoder = OfficialDecoder::load(library).unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("roundtrip.om");
        let mut values = (0..60)
            .map(|index| index as f32 / 10.0 - 2.5)
            .collect::<Vec<_>>();
        values[17] = f32::NAN;
        let mut writer = decoder
            .create_array_writer(&path, vec![3, 4, 5], vec![1, 2, 5], 10.0, 0.0, 20, 0)
            .unwrap();
        writer.write_f32_block(&values[..40], &[2, 4, 5]).unwrap();
        writer.write_f32_block(&values[40..], &[1, 4, 5]).unwrap();
        writer.finish("temperature_2m").unwrap();

        let file = Arc::new(File::open(&path).unwrap());
        let array = read_native_array_metadata(&file).unwrap();
        assert_eq!(array.dimensions, [3, 4, 5]);
        assert_eq!(array.chunks, [1, 2, 5]);
        assert_eq!(array.scale_factor, Some(10.0));
        let metadata = build_v3_array_metadata_blob(
            "temperature_2m",
            array.data_type,
            array.compression,
            &array.dimensions,
            &array.chunks,
            array.lut_size.unwrap(),
            array.lut_offset.unwrap(),
            array.scale_factor.unwrap(),
            array.add_offset.unwrap(),
        );
        let range_reader = FileRangeReader(std::sync::Mutex::new(File::open(&path).unwrap()));
        let decoded = decoder
            .decode_grid(&metadata, &range_reader, &[0, 0, 0], &[3, 4, 5])
            .unwrap();
        assert_eq!(decoded.len(), values.len());
        for (actual, expected) in decoded.iter().zip(values) {
            if expected.is_nan() {
                assert!(actual.is_nan());
            } else {
                assert!((actual - expected).abs() < 1e-6);
            }
        }
    }
}
