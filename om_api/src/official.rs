use anyhow::{bail, Context, Result};
use libloading::Library;
use std::ffi::{c_char, c_void, CStr};
use std::path::Path;
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
