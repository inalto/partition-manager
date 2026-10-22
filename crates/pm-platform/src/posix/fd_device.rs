//! [`FdDevice`]: a [`BlockDevice`] over a raw Unix device node or file descriptor.
//!
//! Reads and writes are `pread`/`pwrite` loops at the requested byte offset (no alignment is
//! imposed here: Linux block devices opened without `O_DIRECT` accept any offset; macOS raw
//! `/dev/rdiskN` nodes need sector-aligned I/O and are wrapped in [`crate::aligned`] by the
//! caller). The device length and logical sector size come from a closure the caller supplies
//! at open, because the query is OS-specific (`BLKGETSIZE64` / `DKIOCGETBLOCKCOUNT`).

use super::os_error;
use pm_core::{BlockDevice, Error, Result};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Raw device node (or regular file) opened with `open(2)`; the descriptor is closed on drop.
pub struct FdDevice {
    fd: OwnedFd,
    path: String,
    len: u64,
    sector_size: u32,
    read_only: bool,
}

/// Size of an open regular file (`fstat`); 0 for device nodes.
pub fn fstat_len(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let file = std::fs::File::from(fd.try_clone_to_owned()?);
    Ok(file.metadata()?.len())
}

impl FdDevice {
    /// Open `path` with `O_RDONLY` or `O_RDWR` (and `O_CLOEXEC`); the length is the `fstat`
    /// size and the sector size 512. Suitable for regular files; device nodes use
    /// [`FdDevice::open_with`] and an OS-specific geometry query.
    pub fn open(path: &Path, read_only: bool) -> Result<FdDevice> {
        Self::open_with(path, read_only, 0, |fd| Ok((fstat_len(fd)?, 512)))
    }

    /// Open `path` read-only or read-write with `extra_flags` (e.g. `O_EXCL`) added to
    /// `O_CLOEXEC`, then ask `geometry` for `(length in bytes, logical sector size)`.
    pub fn open_with(
        path: &Path,
        read_only: bool,
        extra_flags: libc::c_int,
        geometry: impl FnOnce(BorrowedFd<'_>) -> io::Result<(u64, u32)>,
    ) -> Result<FdDevice> {
        let shown = path.display().to_string();
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .custom_flags(libc::O_CLOEXEC | extra_flags)
            .open(path)
            .map_err(|e| os_error(&e, &format!("open {shown}")))?;
        let fd = OwnedFd::from(file);
        let (len, sector_size) =
            geometry(fd.as_fd()).map_err(|e| os_error(&e, &format!("query size of {shown}")))?;
        Ok(FdDevice {
            fd,
            path: shown,
            len,
            sector_size: if sector_size == 0 { 512 } else { sector_size },
            read_only,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// The open descriptor (for ioctls).
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// `flock(LOCK_EX | LOCK_NB)`: `Ok(true)` when the lock was taken, `Ok(false)` when another
    /// open file description holds a lock on the node.
    pub fn try_lock_exclusive(&self) -> io::Result<bool> {
        match flock(self.fd(), libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(true),
            Err(e) if e.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Release a lock taken by [`FdDevice::try_lock_exclusive`] (closing the descriptor does
    /// too).
    pub fn unlock(&self) -> io::Result<()> {
        flock(self.fd(), libc::LOCK_UN)
    }

    /// `fsync(2)`.
    pub fn sync(&self) -> io::Result<()> {
        fsync(self.fd())
    }

    fn check_range(&self, offset: u64, len: usize) -> Result<()> {
        match offset.checked_add(len as u64) {
            Some(end) if end <= self.len => Ok(()),
            _ => Err(Error::io(
                offset,
                len,
                io::Error::new(io::ErrorKind::UnexpectedEof, format!("beyond the end of {}", self.path)),
            )),
        }
    }
}

fn flock(fd: BorrowedFd<'_>, op: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: `fd` is a valid open descriptor for the duration of the borrow; flock only
        // changes the advisory lock state of the open file description.
        let r = unsafe { libc::flock(fd.as_raw_fd(), op) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

fn fsync(fd: BorrowedFd<'_>) -> io::Result<()> {
    loop {
        // SAFETY: `fd` is a valid open descriptor for the duration of the borrow.
        let r = unsafe { libc::fsync(fd.as_raw_fd()) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// One `pread` of up to `buf.len()` bytes at `offset`; returns the byte count.
fn pread_once(fd: BorrowedFd<'_>, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let off = i64::try_from(offset).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `buf` is a valid writable region of `buf.len()` bytes for the whole call and `fd`
    // is a valid open descriptor for the duration of the borrow.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let n = unsafe { libc::pread64(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), off) };
    // SAFETY: as above; `off_t` is 64-bit on the other supported Unixes (macOS).
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), off as libc::off_t) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// One `pwrite` of up to `buf.len()` bytes at `offset`; returns the byte count.
fn pwrite_once(fd: BorrowedFd<'_>, buf: &[u8], offset: u64) -> io::Result<usize> {
    let off = i64::try_from(offset).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `buf` is a valid readable region of `buf.len()` bytes for the whole call and `fd`
    // is a valid open descriptor for the duration of the borrow.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let n = unsafe { libc::pwrite64(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len(), off) };
    // SAFETY: as above; `off_t` is 64-bit on the other supported Unixes (macOS).
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let n = unsafe { libc::pwrite(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len(), off as libc::off_t) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

/// Read all of `buf` at `offset` (EINTR retried; a 0-byte read is `UnexpectedEof`).
pub fn read_exact_at(fd: BorrowedFd<'_>, offset: u64, buf: &mut [u8]) -> Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        match pread_once(fd, &mut buf[done..], offset + done as u64) {
            Ok(0) => return Err(Error::io(offset, buf.len(), io::Error::from(io::ErrorKind::UnexpectedEof))),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::io(offset, buf.len(), e)),
        }
    }
    Ok(())
}

/// Write all of `buf` at `offset` (EINTR retried; a 0-byte write is `WriteZero`).
pub fn write_all_at(fd: BorrowedFd<'_>, offset: u64, buf: &[u8]) -> Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        match pwrite_once(fd, &buf[done..], offset + done as u64) {
            Ok(0) => return Err(Error::io(offset, buf.len(), io::Error::from(io::ErrorKind::WriteZero))),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::io(offset, buf.len(), e)),
        }
    }
    Ok(())
}

impl BlockDevice for FdDevice {
    fn len(&self) -> u64 {
        self.len
    }
    fn sector_size(&self) -> u32 {
        self.sector_size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check_range(offset, buf.len())?;
        read_exact_at(self.fd(), offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::Invalid("read-only".into()));
        }
        self.check_range(offset, buf.len())?;
        write_all_at(self.fd(), offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        self.sync().map_err(|e| os_error(&e, &format!("fsync {}", self.path)))
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(len: usize) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        f.as_file().set_len(len as u64).unwrap();
        f
    }

    #[test]
    fn reads_and_writes_at_any_offset() {
        let f = temp_file(8192);
        let mut d = FdDevice::open(f.path(), false).unwrap();
        assert_eq!((d.len(), d.sector_size(), d.is_read_only()), (8192, 512, false));
        d.write_at(1000, b"hello world").unwrap();
        d.flush().unwrap();
        let mut b = [0u8; 11];
        d.read_at(1000, &mut b).unwrap();
        assert_eq!(&b, b"hello world");
        // the bytes really are in the file
        assert_eq!(&std::fs::read(f.path()).unwrap()[1000..1011], b"hello world");
    }

    #[test]
    fn range_and_mode_are_enforced() {
        let f = temp_file(4096);
        let mut ro = FdDevice::open(f.path(), true).unwrap();
        assert!(ro.is_read_only());
        assert!(matches!(ro.write_at(0, b"x"), Err(Error::Invalid(_))));
        ro.flush().unwrap();
        let mut b = [0u8; 16];
        match ro.read_at(4090, &mut b) {
            Err(Error::Io { offset, len, source }) => {
                assert_eq!((offset, len), (4090, 16));
                assert_eq!(source.kind(), io::ErrorKind::UnexpectedEof);
            }
            other => panic!("{other:?}"),
        }
        assert!(ro.read_at(u64::MAX - 2, &mut b).is_err());
    }

    #[test]
    fn short_file_behind_a_larger_geometry_reports_eof() {
        // the geometry says 8 KiB but the file holds 4 KiB: pread returns 0 → UnexpectedEof
        let f = temp_file(4096);
        let mut d = FdDevice::open_with(f.path(), true, 0, |_| Ok((8192, 512))).unwrap();
        let mut b = [0u8; 512];
        match d.read_at(4096, &mut b) {
            Err(Error::Io { offset, source, .. }) => {
                assert_eq!(offset, 4096);
                assert_eq!(source.kind(), io::ErrorKind::UnexpectedEof);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn geometry_closure_and_open_errors() {
        let f = temp_file(4096);
        let d = FdDevice::open_with(f.path(), true, 0, |_| Ok((1 << 20, 4096))).unwrap();
        assert_eq!((d.len(), d.sector_size()), (1 << 20, 4096));
        let d = FdDevice::open_with(f.path(), true, 0, |_| Ok((10, 0))).unwrap();
        assert_eq!(d.sector_size(), 512, "0 falls back to 512");
        match FdDevice::open(Path::new("/nonexistent/pm-test"), true) {
            Err(Error::Platform { code, msg }) => {
                assert_eq!(code, libc::ENOENT as u32);
                assert!(msg.starts_with("open /nonexistent/pm-test"), "{msg}");
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("opened a missing file"),
        }
        let failing = FdDevice::open_with(f.path(), true, 0, |_| Err(io::Error::from_raw_os_error(libc::ENOTTY)));
        assert!(matches!(failing, Err(Error::Platform { code, .. }) if code == libc::ENOTTY as u32));
    }

    #[test]
    fn flock_is_exclusive_between_open_descriptions() {
        let f = temp_file(512);
        let a = FdDevice::open(f.path(), false).unwrap();
        let b = FdDevice::open(f.path(), false).unwrap();
        assert!(a.try_lock_exclusive().unwrap());
        assert!(!b.try_lock_exclusive().unwrap(), "held by a");
        a.unlock().unwrap();
        assert!(b.try_lock_exclusive().unwrap());
        drop(b);
        assert!(a.try_lock_exclusive().unwrap(), "closing b released its lock");
    }
}
