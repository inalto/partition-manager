//! Raw disk access on macOS: `/dev/rdiskN` and `/dev/rdiskNsM` through
//! [`crate::posix::FdDevice`], always behind [`AlignedDevice`].
//!
//! Raw (character) disk nodes reject I/O whose offset or length is not a multiple of the
//! block size (`EINVAL`), so every byte-granular request is bounced through
//! [`crate::aligned`]. [`AlignedDevice`] itself is OS-neutral (tested with a strict fake).

use crate::aligned::{read_unaligned, write_unaligned, RawSectorIo};
use pm_core::{BlockDevice, Error, Result};

/// A [`BlockDevice`] whose inner device only accepts sector-aligned I/O; reads and writes at
/// any offset and length are turned into aligned ones (read-modify-write at the edges).
pub struct AlignedDevice<D: BlockDevice> {
    inner: D,
}

/// The inner device seen as [`RawSectorIo`].
struct Raw<'a, D: BlockDevice>(&'a mut D);

impl<D: BlockDevice> RawSectorIo for Raw<'_, D> {
    fn sector_size(&self) -> u32 {
        self.0.sector_size()
    }
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn read_aligned(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_at(offset, buf)
    }
    fn write_aligned(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.0.write_at(offset, buf)
    }
}

impl<D: BlockDevice> AlignedDevice<D> {
    pub fn new(inner: D) -> Self {
        AlignedDevice { inner }
    }
    pub fn inner(&self) -> &D {
        &self.inner
    }
    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: BlockDevice> BlockDevice for AlignedDevice<D> {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        read_unaligned(&mut Raw(&mut self.inner), offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.inner.is_read_only() {
            return Err(Error::Invalid("read-only".into()));
        }
        write_unaligned(&mut Raw(&mut self.inner), offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
}

#[cfg(target_os = "macos")]
pub use sys::*;

#[cfg(target_os = "macos")]
mod sys {
    use super::AlignedDevice;
    use crate::posix::FdDevice;
    use crate::traits::OpenMode;
    use pm_core::Result;
    use std::path::Path;

    /// An open raw disk or slice node.
    pub type MacDevice = AlignedDevice<FdDevice>;

    /// Open a raw node (`/dev/rdiskN`, `/dev/rdiskNsM`); length and block size from
    /// `DKIOCGETBLOCKCOUNT` × `DKIOCGETBLOCKSIZE`.
    pub fn open_raw(path: &str, mode: OpenMode) -> Result<MacDevice> {
        let fd = FdDevice::open_with(Path::new(path), mode == OpenMode::ReadOnly, 0, super::super::ioctl::geometry)?;
        Ok(AlignedDevice::new(fd))
    }
}
