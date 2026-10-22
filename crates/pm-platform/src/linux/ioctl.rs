//! Linux system calls on block devices and mounts, each wrapped in a small safe function.
//!
//! `BLKGETSIZE64` (bytes), `BLKSSZGET` (logical sector), `BLKPBSZGET` (physical sector),
//! `BLKROGET` (read-only) and `BLKRRPART` (re-read the partition table), plus `umount2(2)` and
//! `mount(2)`. `errno` is read with `io::Error::last_os_error()` immediately after the failing
//! call.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr;

/// `_IOC` direction bits: generic architectures (x86, arm, riscv, ...) and the ones with the
/// older 3-bit direction field.
#[cfg(any(
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "sparc",
    target_arch = "sparc64"
))]
const DIR: (u32, u32) = (1 << 29, 2 << 29);
#[cfg(not(any(
    target_arch = "powerpc",
    target_arch = "powerpc64",
    target_arch = "mips",
    target_arch = "mips64",
    target_arch = "sparc",
    target_arch = "sparc64"
)))]
const DIR: (u32, u32) = (0, 2 << 30);

/// `_IO(0x12, nr)`.
const fn blk_io(nr: u32) -> u32 {
    DIR.0 | (0x12 << 8) | nr
}

/// `_IOR(0x12, nr, size)`.
const fn blk_ior(nr: u32, size: usize) -> u32 {
    DIR.1 | ((size as u32) << 16) | (0x12 << 8) | nr
}

pub const BLKROGET: u32 = blk_io(94);
pub const BLKRRPART: u32 = blk_io(95);
pub const BLKSSZGET: u32 = blk_io(104);
pub const BLKPBSZGET: u32 = blk_io(123);
pub const BLKGETSIZE64: u32 = blk_ior(114, std::mem::size_of::<usize>());

/// `ioctl(fd, req, &mut out)` for a request that writes a `T`.
fn ioctl_out<T: Copy + Default>(fd: BorrowedFd<'_>, req: u32) -> io::Result<T> {
    let mut out = T::default();
    // SAFETY: `fd` is valid for the borrow; `req` is a read-type request whose kernel handler
    // writes at most `size_of::<T>()` bytes to the pointer, which points to a live `T`.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as libc::Ioctl, &mut out as *mut T) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(out)
    }
}

/// `ioctl(fd, req)` for a request without an argument.
fn ioctl_none(fd: BorrowedFd<'_>, req: u32) -> io::Result<()> {
    // SAFETY: `fd` is valid for the borrow; the request takes no argument and touches no
    // memory of ours.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as libc::Ioctl, 0) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Device size in bytes.
pub fn size_bytes(fd: BorrowedFd<'_>) -> io::Result<u64> {
    ioctl_out::<u64>(fd, BLKGETSIZE64)
}

/// Logical sector size.
pub fn logical_sector(fd: BorrowedFd<'_>) -> io::Result<u32> {
    ioctl_out::<libc::c_int>(fd, BLKSSZGET).map(|v| v.max(0) as u32)
}

/// Physical sector size.
pub fn physical_sector(fd: BorrowedFd<'_>) -> io::Result<u32> {
    ioctl_out::<libc::c_uint>(fd, BLKPBSZGET)
}

/// Whether the block device is read-only.
pub fn read_only(fd: BorrowedFd<'_>) -> io::Result<bool> {
    ioctl_out::<libc::c_int>(fd, BLKROGET).map(|v| v != 0)
}

/// Ask the kernel to re-read the partition table (fails with `EBUSY` while a partition of
/// the disk is open or mounted).
pub fn reread_partitions(fd: BorrowedFd<'_>) -> io::Result<()> {
    ioctl_none(fd, BLKRRPART)
}

/// Drop the clean cached pages of the device (`posix_fadvise(DONTNEED)`). A disk and its
/// partitions are separate block devices with separate page caches: without this, reading the
/// disk after `mkfs` wrote through the partition node can return stale data.
pub fn drop_cache(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: `fd` is valid for the borrow; the advice only affects the kernel's page cache.
    let r = unsafe { libc::posix_fadvise(fd.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if r != 0 {
        // posix_fadvise returns the error number instead of setting errno
        Err(io::Error::from_raw_os_error(r))
    } else {
        Ok(())
    }
}

/// `(length, logical sector size)` of a block device, whose stale page cache is dropped on
/// the way ([`drop_cache`]); regular files (images) fall back to their `fstat` size and
/// 512-byte sectors.
pub fn geometry(fd: BorrowedFd<'_>) -> io::Result<(u64, u32)> {
    match size_bytes(fd) {
        Ok(len) => {
            if let Err(e) = drop_cache(fd) {
                log::debug!("posix_fadvise(DONTNEED): {e}");
            }
            Ok((len, logical_sector(fd).unwrap_or(512)))
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOTTY) => Ok((crate::posix::fd_device::fstat_len(fd)?, 512)),
        Err(e) => Err(e),
    }
}

fn cstring(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// `umount2(target, flags)`.
pub fn umount2(target: &str, flags: libc::c_int) -> io::Result<()> {
    let t = cstring(target)?;
    // SAFETY: `t` is a NUL-terminated string alive for the whole call.
    let r = unsafe { libc::umount2(t.as_ptr(), flags) };
    if r != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `mount(source, target, fstype, flags, data)`.
pub fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
    let (s, t, f) = (cstring(source)?, cstring(target)?, cstring(fstype)?);
    let d = data.map(cstring).transpose()?;
    let d_ptr = d.as_ref().map_or(ptr::null(), |d| d.as_ptr().cast::<libc::c_void>());
    // SAFETY: every pointer is either null (data) or a NUL-terminated string alive for the
    // whole call; the kernel only reads them.
    let r = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, d_ptr) };
    if r != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `(major, minor)` of a `dev_t` (glibc encoding).
pub fn major_minor(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_numbers_match_the_kernel_headers() {
        // the encoding agrees with libc's own constants for this architecture
        assert_eq!(BLKSSZGET as libc::Ioctl, libc::BLKSSZGET);
        assert_eq!(BLKPBSZGET as libc::Ioctl, libc::BLKPBSZGET);
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            assert_eq!(BLKRRPART, 0x125F);
            assert_eq!(BLKROGET, 0x125E);
            assert_eq!(BLKGETSIZE64, 0x8008_1272);
        }
    }

    #[test]
    fn dev_t_split() {
        assert_eq!(major_minor(0x0801), (8, 1)); // sda1
        assert_eq!(major_minor((259 << 8) | 3), (259, 3)); // nvme partition, small minor
        // large minor: 0x12345 -> low byte 0x45 + bits 8.. at 20..
        let dev = (0x45u64) | (0x07u64 << 8) | (0x123u64 << 20);
        assert_eq!(major_minor(dev), (7, 0x12345));
    }

    #[test]
    fn regular_files_fall_back_to_their_size() {
        let f = tempfile::NamedTempFile::new().unwrap();
        f.as_file().set_len(12345).unwrap();
        use std::os::fd::AsFd;
        assert_eq!(geometry(f.as_file().as_fd()).unwrap(), (12345, 512));
        assert!(reread_partitions(f.as_file().as_fd()).is_err());
    }
}
