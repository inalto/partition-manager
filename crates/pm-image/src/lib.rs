//! Virtual disk images (raw, VHD fixed/dynamic, VMDK flat/sparse, qcow2 v3) exposed as
//! [`BlockDevice`]s whose `len()` is the virtual disk capacity, plus format
//! identification and conversion.

mod format;
mod qcow2;
mod raw;
mod sparse;
mod vhd;
mod vmdk;

pub use format::ImageFormat;
use pm_core::{BlockDevice, CancelToken, Error, ProgressSink, ProgressTracker, Result};
use std::path::Path;

/// Options for [`create`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateOpts {
    pub format: ImageFormat,
    /// Virtual disk size in bytes; must be a non-zero multiple of 512.
    pub capacity: u64,
    /// Raw images only: request a sparse file (currently ignored).
    pub sparse: bool,
}

const CONVERT_CHUNK: usize = 1 << 20;

/// Create a new image (truncating any existing file) and open it read-write.
pub fn create(path: &Path, opts: &CreateOpts) -> Result<Box<dyn BlockDevice>> {
    let cap = opts.capacity;
    if cap == 0 || cap % 512 != 0 {
        return Err(Error::Invalid(format!(
            "image capacity must be a non-zero multiple of 512 bytes (got {cap})"
        )));
    }
    match opts.format {
        ImageFormat::Raw => raw::create(path, cap),
        ImageFormat::VhdFixed => vhd::create_fixed(path, cap),
        ImageFormat::VhdDynamic => vhd::create_dynamic(path, cap),
        ImageFormat::VmdkFlat => vmdk::create_flat(path, cap),
        ImageFormat::VmdkSparse => vmdk::create_sparse(path, cap),
        ImageFormat::Qcow2 => qcow2::create(path, cap),
    }
}

/// Open an existing image of any supported format (detected by content).
pub fn open(path: &Path, read_only: bool) -> Result<Box<dyn BlockDevice>> {
    match format::sniff(path)? {
        ImageFormat::Raw => raw::open(path, read_only),
        ImageFormat::VhdFixed | ImageFormat::VhdDynamic => vhd::open(path, read_only),
        ImageFormat::VmdkFlat => vmdk::open_flat(path, read_only),
        ImageFormat::VmdkSparse => vmdk::open_sparse(path, read_only),
        ImageFormat::Qcow2 => qcow2::open(path, read_only),
    }
}

/// Detect the format of an image file without opening it for I/O.
pub fn identify(path: &Path) -> Result<ImageFormat> {
    format::sniff(path)
}

/// Copy the image `src` (any format) into a new image `dst` of format `fmt` with the
/// same capacity, in 1 MiB chunks. All-zero chunks are not written to sparse formats so
/// they stay unallocated. On cancellation the partial destination is left in place.
pub fn convert(
    src: &Path,
    dst: &Path,
    fmt: ImageFormat,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<()> {
    cancel.check()?;
    let mut input = open(src, true)?;
    let capacity = input.len();
    let mut output = create(
        dst,
        &CreateOpts {
            format: fmt,
            capacity,
            sparse: true,
        },
    )?;
    let skip_zero = fmt.is_sparse();
    let mut tracker = ProgressTracker::new(progress, "Converting", capacity);
    let mut buf = vec![0u8; CONVERT_CHUNK];
    let mut off = 0u64;
    while off < capacity {
        cancel.check()?;
        let n = (capacity - off).min(CONVERT_CHUNK as u64) as usize;
        input.read_at(off, &mut buf[..n])?;
        if !(skip_zero && buf[..n].iter().all(|&b| b == 0)) {
            output.write_at(off, &buf[..n])?;
        }
        off += n as u64;
        tracker.add(n as u64);
    }
    output.flush()?;
    tracker.finish();
    Ok(())
}
