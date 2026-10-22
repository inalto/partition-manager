//! `<sys/disk.h>` ioctls for `/dev/rdiskN`: block size, block count, cache flush.
//!
//! Request numbers use the BSD `_IOR` / `_IO` encoding (`IOC_OUT | len << 16 | group << 8 |
//! nr`); they are plain constants (checked on every OS), the calls are macOS-only.

/// `IOC_VOID`: no parameter.
const IOC_VOID: u64 = 0x2000_0000;
/// `IOC_OUT`: the kernel copies a value out.
const IOC_OUT: u64 = 0x4000_0000;
/// `IOCPARM_MASK`: parameter length bits.
const IOCPARM_MASK: u64 = 0x1fff;

/// `_IOR(group, nr, len)`.
pub const fn ior(group: u8, nr: u8, len: usize) -> u64 {
    IOC_OUT | ((len as u64 & IOCPARM_MASK) << 16) | ((group as u64) << 8) | nr as u64
}

/// `_IO(group, nr)`.
pub const fn io(group: u8, nr: u8) -> u64 {
    IOC_VOID | ((group as u64) << 8) | nr as u64
}

/// `DKIOCGETBLOCKSIZE`: logical block size (`uint32_t`).
pub const DKIOCGETBLOCKSIZE: u64 = ior(b'd', 24, 4);
/// `DKIOCGETBLOCKCOUNT`: number of logical blocks (`uint64_t`).
pub const DKIOCGETBLOCKCOUNT: u64 = ior(b'd', 25, 8);
/// `DKIOCSYNCHRONIZECACHE`: flush the drive's write cache.
pub const DKIOCSYNCHRONIZECACHE: u64 = io(b'd', 22);

#[cfg(target_os = "macos")]
mod sys {
    use super::*;
    use std::io;
    use std::os::fd::{AsRawFd, BorrowedFd};

    /// `ioctl(fd, request, &mut T)`.
    fn ioctl_out<T: Default>(fd: BorrowedFd<'_>, request: u64) -> io::Result<T> {
        let mut v = T::default();
        // SAFETY: `fd` is a valid open descriptor for the duration of the borrow and `v` is a
        // writable value of exactly the size encoded in `request` (u32 / u64 per the header).
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), request as libc::c_ulong, &mut v as *mut T) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(v)
    }

    /// Logical block size of a disk device.
    pub fn block_size(fd: BorrowedFd<'_>) -> io::Result<u32> {
        ioctl_out::<u32>(fd, DKIOCGETBLOCKSIZE)
    }

    /// Number of logical blocks of a disk device.
    pub fn block_count(fd: BorrowedFd<'_>) -> io::Result<u64> {
        ioctl_out::<u64>(fd, DKIOCGETBLOCKCOUNT)
    }

    /// `(length in bytes, logical block size)` for [`crate::posix::FdDevice::open_with`].
    pub fn geometry(fd: BorrowedFd<'_>) -> io::Result<(u64, u32)> {
        let bs = block_size(fd)?;
        let count = block_count(fd)?;
        let len = count
            .checked_mul(bs as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "device size overflows"))?;
        Ok((len, bs))
    }

    /// Ask the drive to flush its write cache (`fsync` on a raw node does not).
    pub fn synchronize_cache(fd: BorrowedFd<'_>) -> io::Result<()> {
        // SAFETY: `fd` is a valid open descriptor for the duration of the borrow; the request
        // takes no argument.
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), DKIOCSYNCHRONIZECACHE as libc::c_ulong) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub use sys::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_numbers_match_sys_disk_h() {
        assert_eq!(DKIOCGETBLOCKSIZE, 0x4004_6418);
        assert_eq!(DKIOCGETBLOCKCOUNT, 0x4008_6419);
        assert_eq!(DKIOCSYNCHRONIZECACHE, 0x2000_6416);
    }
}
