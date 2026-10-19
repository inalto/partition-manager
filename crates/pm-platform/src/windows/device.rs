//! [`WinDevice`]: a [`BlockDevice`] over a raw Windows disk or volume handle.

use super::error::{io_error, platform_error};
use super::ioctl::{self, Handle, GENERIC_READ, GENERIC_WRITE};
use crate::aligned::{read_unaligned, write_unaligned, RawSectorIo};
use crate::traits::OpenMode;
use pm_core::{BlockDevice, Error, Result};
use std::io;
use windows_sys::Win32::System::Ioctl::FSCTL_ALLOW_EXTENDED_DASD_IO;

/// True for a volume path (`\\.\X:`, `\\?\Volume{..}`, with or without a trailing
/// backslash), false for a physical disk (`\\.\PhysicalDriveN`) or anything else.
pub(crate) fn is_volume_path(path: &str) -> bool {
    let p = path.trim_end_matches('\\');
    let Some(rest) = p.strip_prefix(r"\\.\").or_else(|| p.strip_prefix(r"\\?\")) else {
        return false;
    };
    let b = rest.as_bytes();
    let letter = b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    let guid = rest.get(..7).is_some_and(|h| h.eq_ignore_ascii_case("Volume{"));
    letter || guid
}

/// Let a volume handle read and write past the end the filesystem reports (the whole
/// partition: e.g. the NTFS backup boot sector in the last sector, or a filesystem being
/// grown or shrunk), with `FSCTL_ALLOW_EXTENDED_DASD_IO`. Failure is only logged: the handle
/// still works inside the filesystem's bounds.
fn allow_extended_dasd_io(handle: &Handle, path: &str) {
    if let Err(c) = ioctl::ioctl(handle, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], &mut []) {
        log::warn!(
            "{path}: FSCTL_ALLOW_EXTENDED_DASD_IO failed ({}); access stays within the filesystem",
            super::friendly_error(c)
        );
    }
}

/// Raw disk (`\\.\PhysicalDriveN`) or volume (`\\?\Volume{..}`, `\\.\X:`) opened with
/// `CreateFileW`. Reads and writes at any byte offset are bounced through sector-aligned
/// buffers (see [`crate::aligned`]). The handle is closed on drop.
pub struct WinDevice {
    handle: Handle,
    path: String,
    len: u64,
    sector_size: u32,
    read_only: bool,
}

impl WinDevice {
    /// Open `path` read-only or read-write (shared read/write, `OPEN_EXISTING`).
    pub fn open(path: &str, mode: OpenMode) -> Result<Self> {
        let access = match mode {
            OpenMode::ReadOnly => GENERIC_READ,
            OpenMode::ReadWrite => GENERIC_READ | GENERIC_WRITE,
        };
        let handle = ioctl::open(path, access).map_err(|c| platform_error(c, &format!("open {path}")))?;
        if is_volume_path(path) {
            allow_extended_dasd_io(&handle, path);
        }
        let geometry = ioctl::geometry(&handle).ok();
        let len = match ioctl::length_info(&handle) {
            Ok(len) => len,
            Err(c) => match geometry {
                Some((_, size)) => size,
                None => return Err(platform_error(c, &format!("query length of {path}"))),
            },
        };
        let sector_size = geometry.map(|(bps, _)| bps).filter(|&b| b != 0).unwrap_or(512);
        Ok(WinDevice {
            handle,
            path: path.to_string(),
            len,
            sector_size,
            read_only: mode == OpenMode::ReadOnly,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn handle(&self) -> &Handle {
        &self.handle
    }
}

impl RawSectorIo for WinDevice {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }
    fn len(&self) -> u64 {
        self.len
    }
    fn read_aligned(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            match ioctl::read_file_at(&self.handle, pos, &mut buf[done..]) {
                Ok(0) => {
                    return Err(Error::io(offset, buf.len(), io::Error::from(io::ErrorKind::UnexpectedEof)))
                }
                Ok(n) => done += n,
                Err(c) => return Err(io_error(c, offset, buf.len())),
            }
        }
        Ok(())
    }
    fn write_aligned(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            match ioctl::write_file_at(&self.handle, pos, &buf[done..]) {
                Ok(0) => {
                    return Err(Error::io(offset, buf.len(), io::Error::from(io::ErrorKind::WriteZero)))
                }
                Ok(n) => done += n,
                Err(c) => return Err(io_error(c, offset, buf.len())),
            }
        }
        Ok(())
    }
}

impl BlockDevice for WinDevice {
    fn len(&self) -> u64 {
        self.len
    }
    fn sector_size(&self) -> u32 {
        self.sector_size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        read_unaligned(self, offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::Invalid("read-only".into()));
        }
        write_unaligned(self, offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        ioctl::flush(&self.handle).map_err(|c| platform_error(c, &format!("flush {}", self.path)))
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_dasd_io_is_requested_for_volume_paths_only() {
        assert_eq!(FSCTL_ALLOW_EXTENDED_DASD_IO, 0x0009_0083);
        for v in [r"\\.\E:", r"\\.\e:\", r"\\?\Volume{12345678-1234-1234-1234-123456789abc}", r"\\?\volume{x}\"] {
            assert!(is_volume_path(v), "{v}");
        }
        for d in [r"\\.\PhysicalDrive0", r"\\?\PhysicalDrive3", r"C:\t\x.img", r"\\.\EE:", ""] {
            assert!(!is_volume_path(d), "{d}");
        }
    }
}
