//! Block device abstraction with in-memory, file-backed and wrapper implementations.

use crate::error::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Random-access byte device (disk, image file, partition slice, ...).
pub trait BlockDevice: Send {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn sector_size(&self) -> u32 {
        512
    }

    /// Read exactly `buf.len()` bytes at `offset`; errors if the range exceeds the device.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Write all of `buf` at `offset`; errors if the range exceeds the device.
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()>;

    fn flush(&mut self) -> Result<()>;

    fn is_read_only(&self) -> bool;
}

impl<D: BlockDevice + ?Sized> BlockDevice for Box<D> {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn sector_size(&self) -> u32 {
        (**self).sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
    fn is_read_only(&self) -> bool {
        (**self).is_read_only()
    }
}

impl<D: BlockDevice + ?Sized> BlockDevice for &mut D {
    fn len(&self) -> u64 {
        (**self).len()
    }
    fn sector_size(&self) -> u32 {
        (**self).sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
    fn is_read_only(&self) -> bool {
        (**self).is_read_only()
    }
}

/// Verify `[offset, offset + len)` lies within a device of size `dev_len`.
fn check_range(offset: u64, len: usize, dev_len: u64) -> Result<()> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= dev_len => Ok(()),
        _ => Err(Error::io(
            offset,
            len,
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("range out of bounds (device length {dev_len})"),
            ),
        )),
    }
}

/// Device backed by a `Vec<u8>`; used for tests and in-memory images.
pub struct MemDevice {
    data: Vec<u8>,
    sector_size: u32,
}

impl MemDevice {
    pub fn new(len: u64) -> Self {
        Self::from_vec(vec![0u8; len as usize])
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        MemDevice {
            data,
            sector_size: 512,
        }
    }

    pub fn with_sector_size(mut self, sector_size: u32) -> Self {
        self.sector_size = sector_size;
        self
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }
}

impl BlockDevice for MemDevice {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }
    fn sector_size(&self) -> u32 {
        self.sector_size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len())?;
        let start = offset as usize;
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len())?;
        let start = offset as usize;
        self.data[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

#[cfg(windows)]
fn raw_read(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, off)
}
#[cfg(windows)]
fn raw_write(f: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(f, buf, off)
}
#[cfg(unix)]
fn raw_read(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, off)
}
#[cfg(unix)]
fn raw_write(f: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(f, buf, off)
}

/// Device backed by a file (disk image) or a raw handle such as `\\.\PhysicalDriveN`.
pub struct FileDevice {
    file: File,
    len: u64,
    sector_size: u32,
    read_only: bool,
}

impl FileDevice {
    /// Open an existing file. The length is taken from file metadata; for raw device
    /// handles (metadata length 0) use [`FileDevice::with_len`].
    pub fn open(path: impl AsRef<Path>, read_only: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path.as_ref())
            .map_err(|e| Error::io(0, 0, e))?;
        let len = file.metadata().map_err(|e| Error::io(0, 0, e))?.len();
        Ok(FileDevice {
            file,
            len,
            sector_size: 512,
            read_only,
        })
    }

    /// Create (or truncate) a file of exactly `len` bytes, opened read-write.
    pub fn create(path: impl AsRef<Path>, len: u64) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path.as_ref())
            .map_err(|e| Error::io(0, 0, e))?;
        file.set_len(len).map_err(|e| Error::io(0, 0, e))?;
        Ok(FileDevice {
            file,
            len,
            sector_size: 512,
            read_only: false,
        })
    }

    /// Override the device length (needed for raw handles whose metadata length is 0).
    pub fn with_len(mut self, len: u64) -> Self {
        self.len = len;
        self
    }

    pub fn with_sector_size(mut self, sector_size: u32) -> Self {
        self.sector_size = sector_size;
        self
    }
}

impl BlockDevice for FileDevice {
    fn len(&self) -> u64 {
        self.len
    }
    fn sector_size(&self) -> u32 {
        self.sector_size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            match raw_read(&self.file, &mut buf[done..], pos) {
                Ok(0) => {
                    return Err(Error::io(
                        offset,
                        buf.len(),
                        io::Error::from(io::ErrorKind::UnexpectedEof),
                    ))
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::io(offset, buf.len(), e)),
            }
        }
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::Invalid("read-only".into()));
        }
        check_range(offset, buf.len(), self.len)?;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            match raw_write(&self.file, &buf[done..], pos) {
                Ok(0) => {
                    return Err(Error::io(
                        offset,
                        buf.len(),
                        io::Error::from(io::ErrorKind::WriteZero),
                    ))
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::io(offset, buf.len(), e)),
            }
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        self.file.sync_all().map_err(|e| Error::io(0, 0, e))
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// A bounds-checked window `[offset, offset + len)` onto another device.
pub struct SliceDevice<D: BlockDevice> {
    inner: D,
    offset: u64,
    len: u64,
}

impl<D: BlockDevice> SliceDevice<D> {
    pub fn new(inner: D, offset: u64, len: u64) -> Result<Self> {
        match offset.checked_add(len) {
            Some(end) if end <= inner.len() => Ok(SliceDevice { inner, offset, len }),
            _ => Err(Error::Invalid(format!(
                "slice {offset}+{len} exceeds device length {}",
                inner.len()
            ))),
        }
    }

    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: BlockDevice> BlockDevice for SliceDevice<D> {
    fn len(&self) -> u64 {
        self.len
    }
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        self.inner.read_at(self.offset + offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        self.inner.write_at(self.offset + offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
}

/// Wrapper that makes any device read-only.
pub struct ReadOnly<D: BlockDevice>(pub D);

impl<D: BlockDevice> BlockDevice for ReadOnly<D> {
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn sector_size(&self) -> u32 {
        self.0.sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_at(offset, buf)
    }
    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> Result<()> {
        Err(Error::Invalid("read-only".into()))
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        true
    }
}

/// Copy `len` bytes from `src` at `src_off` to `dst` at `dst_off` in chunks of `buf_size`.
///
/// For raw devices choose `buf_size` as a multiple of the sector size.
pub fn copy_range(
    src: &mut dyn BlockDevice,
    dst: &mut dyn BlockDevice,
    src_off: u64,
    dst_off: u64,
    len: u64,
    buf_size: usize,
) -> Result<()> {
    if buf_size == 0 {
        return Err(Error::Invalid("copy buffer size must be non-zero".into()));
    }
    let in_range = |off: u64, dev_len: u64| off.checked_add(len).map_or(false, |end| end <= dev_len);
    if !in_range(src_off, src.len()) || !in_range(dst_off, dst.len()) {
        return Err(Error::Invalid(format!(
            "copy range out of bounds: src {src_off}+{len} (of {}), dst {dst_off}+{len} (of {})",
            src.len(),
            dst.len()
        )));
    }
    let mut buf = vec![0u8; (buf_size as u64).min(len) as usize];
    let mut copied = 0u64;
    while copied < len {
        let n = (buf.len() as u64).min(len - copied) as usize;
        src.read_at(src_off + copied, &mut buf[..n])?;
        dst.write_at(dst_off + copied, &buf[..n])?;
        copied += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn mem_device_roundtrip() {
        let mut d = MemDevice::new(4096);
        assert_eq!(d.len(), 4096);
        assert_eq!(d.sector_size(), 512);
        assert!(!d.is_read_only());
        d.write_at(100, b"hello").unwrap();
        let mut b = [0u8; 5];
        d.read_at(100, &mut b).unwrap();
        assert_eq!(&b, b"hello");
        assert_eq!(&d.as_slice()[100..105], b"hello");
        d.flush().unwrap();
        let d2 = MemDevice::from_vec(vec![1, 2, 3]).with_sector_size(4096);
        assert_eq!(d2.len(), 3);
        assert_eq!(d2.sector_size(), 4096);
    }

    #[test]
    fn read_past_end_is_error() {
        let mut d = MemDevice::new(16);
        let mut b = [0u8; 8];
        assert!(d.read_at(9, &mut b).is_err());
        assert!(d.read_at(u64::MAX - 2, &mut b).is_err());
        assert!(d.write_at(9, &b).is_err());
        assert!(d.read_at(8, &mut b).is_ok());
    }

    #[test]
    fn slice_device_offsets_and_bounds() {
        let mut base = MemDevice::new(1024);
        base.write_at(100, b"abc").unwrap();
        let mut s = SliceDevice::new(&mut base, 100, 50).unwrap();
        assert_eq!(s.len(), 50);
        let mut b = [0u8; 3];
        s.read_at(0, &mut b).unwrap();
        assert_eq!(&b, b"abc");
        s.write_at(47, b"xyz").unwrap();
        assert!(s.write_at(48, b"xyz").is_err());
        assert!(s.read_at(50, &mut [0u8; 1]).is_err());
        drop(s);
        assert_eq!(&base.as_slice()[147..150], b"xyz");
        assert!(SliceDevice::new(MemDevice::new(10), 5, 6).is_err());
        assert!(SliceDevice::new(MemDevice::new(10), 5, 5).is_ok());
    }

    #[test]
    fn readonly_rejects_write() {
        let mut d = ReadOnly(MemDevice::new(16));
        assert!(d.is_read_only());
        assert!(matches!(d.write_at(0, b"x"), Err(Error::Invalid(_))));
        let mut b = [0u8; 1];
        d.read_at(0, &mut b).unwrap();
    }

    #[test]
    fn file_device_create_open_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        {
            let mut d = FileDevice::create(&path, 1 << 20).unwrap();
            assert_eq!(d.len(), 1 << 20);
            d.write_at(4096, b"payload").unwrap();
            d.write_at((1 << 20) - 3, b"end").unwrap();
            d.flush().unwrap();
            assert!(d.write_at(1 << 20, b"x").is_err());
        }
        let mut d = FileDevice::open(&path, true).unwrap();
        assert!(d.is_read_only());
        assert_eq!(d.len(), 1 << 20);
        let mut b = [0u8; 7];
        d.read_at(4096, &mut b).unwrap();
        assert_eq!(&b, b"payload");
        assert!(d.write_at(0, b"x").is_err());
        assert!(d.read_at((1 << 20) - 2, &mut [0u8; 4]).is_err());
        let mut d = FileDevice::open(&path, false).unwrap();
        assert!(!d.is_read_only());
        d.write_at(0, b"Z").unwrap();
        let d = FileDevice::open(&path, true).unwrap().with_len(10);
        assert_eq!(d.len(), 10);
        assert!(FileDevice::open(dir.path().join("nope"), true).is_err());
    }

    #[test]
    fn copy_range_copies_exact_bytes() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let mut src = MemDevice::from_vec(data.clone());
        let mut dst = MemDevice::new(20_000);
        copy_range(&mut src, &mut dst, 1000, 5000, 7777, 1024).unwrap();
        assert_eq!(&dst.as_slice()[5000..5000 + 7777], &data[1000..1000 + 7777]);
        assert!(dst.as_slice()[..5000].iter().all(|&b| b == 0));
        assert!(dst.as_slice()[5000 + 7777..].iter().all(|&b| b == 0));
        // zero length is fine, out-of-range source errors
        copy_range(&mut src, &mut dst, 0, 0, 0, 16).unwrap();
        assert!(copy_range(&mut src, &mut dst, 9000, 0, 2000, 512).is_err());
        assert!(copy_range(&mut src, &mut dst, 0, 0, 10, 0).is_err());
    }

    #[test]
    fn copy_range_validates_bounds_before_writing() {
        let mut src = MemDevice::from_vec(vec![0xAA; 1000]);
        let mut dst = MemDevice::new(1000);
        // destination too small: nothing may be written
        assert!(matches!(
            copy_range(&mut src, &mut dst, 0, 600, 500, 100),
            Err(Error::Invalid(_))
        ));
        assert!(dst.as_slice().iter().all(|&b| b == 0));
        // source range exceeds source
        assert!(matches!(
            copy_range(&mut src, &mut dst, 600, 0, 500, 100),
            Err(Error::Invalid(_))
        ));
        // offset + len overflows u64
        assert!(matches!(
            copy_range(&mut src, &mut dst, u64::MAX, 0, 2, 100),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            copy_range(&mut src, &mut dst, 0, u64::MAX, 2, 100),
            Err(Error::Invalid(_))
        ));
        assert!(dst.as_slice().iter().all(|&b| b == 0));
    }

    #[test]
    fn boxed_and_ref_forwarding() {
        let mut b: Box<dyn BlockDevice> = Box::new(MemDevice::new(32));
        b.write_at(0, b"hi").unwrap();
        let r: &mut dyn BlockDevice = &mut b;
        let mut buf = [0u8; 2];
        r.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"hi");
        assert_eq!(r.len(), 32);
    }
}
