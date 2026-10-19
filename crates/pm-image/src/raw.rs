//! Raw (flat, headerless) disk images: the file content is the disk.

use pm_core::{BlockDevice, FileDevice, Result};
use std::path::Path;

/// Create a zero-filled raw image. Making the file sparse on NTFS is a later concern;
/// `set_len` already avoids writing the zeros on most filesystems.
pub(crate) fn create(path: &Path, capacity: u64) -> Result<Box<dyn BlockDevice>> {
    Ok(Box::new(FileDevice::create(path, capacity)?))
}

pub(crate) fn open(path: &Path, read_only: bool) -> Result<Box<dyn BlockDevice>> {
    Ok(Box::new(FileDevice::open(path, read_only)?))
}
