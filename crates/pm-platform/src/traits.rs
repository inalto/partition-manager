//! The OS abstraction consumed by every engine, the GUI and the CLI.

use pm_core::{BlockDevice, DiskInfo, FsKind, ProgressSink, Result, VolumeId, VolumeInfo};
use pm_parttable::Table;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How a disk or volume is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    ReadOnly,
    ReadWrite,
}

/// Where a volume is (or should be) mounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountPoint {
    /// Drive letter, e.g. `Letter('E')` for `E:\`.
    Letter(char),
    /// Empty NTFS folder used as a mount point.
    Folder(PathBuf),
}

impl MountPoint {
    /// The mount point as a path string with a trailing backslash (`E:\`, `C:\mnt\x\`),
    /// which is the form Windows reports in volume path names.
    pub fn to_path_string(&self) -> String {
        match self {
            MountPoint::Letter(c) => format!("{}:\\", c.to_ascii_uppercase()),
            MountPoint::Folder(p) => {
                let mut s = p.to_string_lossy().into_owned();
                if !s.ends_with('\\') && !s.ends_with('/') {
                    s.push('\\');
                }
                s
            }
        }
    }
}

/// Exclusive write access to one disk: its volumes are locked/dismounted (and, for non-system
/// disks, the disk is taken offline) until [`WriteSession::finish`] is called or the session is
/// dropped.
pub trait WriteSession: Send {
    fn disk(&self) -> u32;
    /// Raw read-write access to the whole disk.
    fn device(&mut self) -> &mut dyn BlockDevice;
    /// Write `t` via `pm_parttable::write_table`, then ask the OS to re-read the layout.
    fn commit_table(&mut self, t: &Table) -> Result<()>;
    /// Bring the disk online again, re-read properties, unlock the volumes.
    fn finish(self: Box<Self>) -> Result<()>;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SmartAttr {
    pub id: u8,
    pub name: String,
    pub value: u8,
    pub worst: u8,
    pub threshold: u8,
    pub raw: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SmartData {
    pub healthy: Option<bool>,
    pub temperature_c: Option<u32>,
    pub power_on_hours: Option<u64>,
    pub attrs: Vec<SmartAttr>,
    pub nvme_percent_used: Option<u8>,
}

/// External system tools that [`Platform::run_tool`] can launch.
#[derive(Clone, Copy, Debug)]
pub enum ExternalTool {
    BcdBoot,
    BootSect,
    Mbr2Gpt,
    FormatCom,
    Diskpart,
}

impl ExternalTool {
    /// Executable file name (resolved in `%SystemRoot%\System32` on Windows).
    pub fn exe_name(&self) -> &'static str {
        match self {
            ExternalTool::BcdBoot => "bcdboot.exe",
            ExternalTool::BootSect => "bootsect.exe",
            ExternalTool::Mbr2Gpt => "mbr2gpt.exe",
            ExternalTool::FormatCom => "format.com",
            ExternalTool::Diskpart => "diskpart.exe",
        }
    }
}

/// Everything that touches the operating system. All sizes and offsets are in bytes.
pub trait Platform: Send + Sync {
    /// Disks with their partitions; volumes are merged in (mount points, label, used, fs).
    fn enumerate_disks(&self) -> Result<Vec<DiskInfo>>;
    fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>>;
    fn open_disk(&self, index: u32, mode: OpenMode) -> Result<Box<dyn BlockDevice>>;
    fn open_volume(&self, id: &VolumeId, mode: OpenMode) -> Result<Box<dyn BlockDevice>>;
    fn begin_write(&self, disk: u32) -> Result<Box<dyn WriteSession>>;
    fn mount(&self, id: &VolumeId, at: &MountPoint) -> Result<()>;
    fn unmount(&self, id: &VolumeId, at: &MountPoint) -> Result<()>;
    fn set_offline(&self, disk: u32, offline: bool) -> Result<()>;
    fn rescan(&self, disk: u32) -> Result<()>;
    fn format(
        &self,
        id: &VolumeId,
        fs: FsKind,
        label: &str,
        cluster: Option<u32>,
        quick: bool,
        progress: &dyn ProgressSink,
    ) -> Result<()>;
    fn shrink_volume(&self, id: &VolumeId, new_len: u64) -> Result<()>;
    /// Grow the filesystem to `new_len`; the caller grows the partition first.
    fn extend_volume(&self, id: &VolumeId, new_len: u64) -> Result<()>;
    fn smart(&self, disk: u32) -> Result<Option<SmartData>>;
    fn is_elevated(&self) -> bool;
    fn system_disk(&self) -> Option<u32>;
    fn free_letters(&self) -> Vec<char>;
    /// Run a system tool, returning its stdout+stderr; non-zero exit → `Error::Platform`.
    fn run_tool(&self, tool: ExternalTool, args: &[String]) -> Result<String>;
    /// Lower-case name of the operating system this platform drives (`"windows"`, `"linux"`,
    /// `"macos"`). Platforms that are not tied to an OS (images, mocks) report the host.
    fn os_name(&self) -> &'static str {
        host_os_name()
    }
    /// How to run a command with the rights this platform needs, for error hints.
    fn elevation_hint(&self) -> &'static str {
        host_elevation_hint()
    }
}

/// Format name understood by `FormatEx` / `format.com` for `fs`.
pub fn fs_format_name(fs: FsKind) -> Option<&'static str> {
    match fs {
        FsKind::Ntfs => Some("NTFS"),
        FsKind::Fat32 => Some("FAT32"),
        FsKind::Fat12 | FsKind::Fat16 => Some("FAT"),
        FsKind::ExFat => Some("exFAT"),
        FsKind::ReFs => Some("ReFS"),
        _ => None,
    }
}

/// Hint shown when an operation fails for lack of rights on Windows.
pub const ELEVATION_HINT_WINDOWS: &str = "run this command from an elevated prompt";
/// Hint shown when an operation fails for lack of rights on Linux and macOS.
pub const ELEVATION_HINT_UNIX: &str = "run with sudo (root)";

/// Name of the OS this binary was built for: `"windows"`, `"linux"` or `"macos"` (otherwise
/// `std::env::consts::OS`).
pub fn host_os_name() -> &'static str {
    std::env::consts::OS
}

/// The elevation hint matching [`host_os_name`].
pub fn host_elevation_hint() -> &'static str {
    if cfg!(windows) {
        ELEVATION_HINT_WINDOWS
    } else {
        ELEVATION_HINT_UNIX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_match_the_host() {
        assert_eq!(host_os_name(), std::env::consts::OS);
        if cfg!(windows) {
            assert!(host_elevation_hint().contains("elevated prompt"));
        } else {
            assert!(host_elevation_hint().contains("sudo"));
        }
        assert!(ELEVATION_HINT_UNIX.contains("sudo") && ELEVATION_HINT_WINDOWS.contains("elevated"));
    }
}
