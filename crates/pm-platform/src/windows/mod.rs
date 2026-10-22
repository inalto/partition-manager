//! Windows implementation of [`Platform`].
//!
//! Elevation requirements: `enumerate_disks` / `enumerate_volumes` / `system_disk` work without
//! elevation (query-only handles); opening disks or volumes for I/O, write sessions, mounting,
//! formatting and resizing need administrator rights.

mod bench;
mod device;
mod disk;
mod elevation;
mod error;
mod format;
mod ioctl;
pub mod layout;
mod mount;
mod session;
mod smart;
mod volume;

pub use bench::{BenchFile, FileBenchTarget};
pub use device::WinDevice;
pub use error::friendly_error;
pub use session::WindowsWriteSession;
pub use smart::{parse_ata_smart, parse_nvme_smart_log};

use crate::traits::{ExternalTool, MountPoint, OpenMode, Platform, SmartData, WriteSession};
use pm_core::{BlockDevice, DiskInfo, Error, FsKind, ProgressSink, Result, VolumeId, VolumeInfo};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// `CREATE_NO_WINDOW`: keep console tools from flashing a window under the GUI.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The real Windows platform.
#[derive(Default)]
pub struct WindowsPlatform {
    system_disk: OnceLock<Option<u32>>,
}

impl WindowsPlatform {
    pub fn new() -> Self {
        Self::default()
    }
}

/// `<System32>\<exe>`, with System32 from `GetSystemDirectoryW`. The environment
/// (`%SystemRoot%`, `PATH`) is deliberately not used: a medium-integrity user can change it,
/// and this process runs elevated.
fn system32_path(exe: &str) -> Result<PathBuf> {
    let dir = ioctl::system_directory().map_err(|c| error::platform_error(c, "locate System32"))?;
    Ok(PathBuf::from(dir).join(exe))
}

/// Run a system tool, returning stdout followed by stderr; non-zero exit → `Error::Platform`.
pub(crate) fn run_tool(tool: ExternalTool, args: &[String]) -> Result<String> {
    run_exe(&system32_path(tool.exe_name())?, tool.exe_name(), args)
}

/// Run `exe` (absolute path) with `args`; see [`run_tool`].
fn run_exe(exe: &std::path::Path, name: &str, args: &[String]) -> Result<String> {
    let out = Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| Error::Platform {
            code: e.raw_os_error().unwrap_or(0) as u32,
            msg: format!("cannot run {}: {e}", exe.display()),
        })?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text)
    } else {
        Err(Error::Platform {
            code: out.status.code().unwrap_or(-1) as u32,
            msg: format!("{name} failed: {}", text.trim()),
        })
    }
}

impl Platform for WindowsPlatform {
    fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
        let volumes = volume::enumerate_volumes().unwrap_or_else(|e| {
            log::warn!("volume enumeration failed: {e}");
            Vec::new()
        });
        Ok(disk::enumerate_disks(&volumes, self.system_disk()))
    }

    fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>> {
        volume::enumerate_volumes()
    }

    fn open_disk(&self, index: u32, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(WinDevice::open(&disk::disk_path(index), mode)?))
    }

    fn open_volume(&self, id: &VolumeId, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(WinDevice::open(&volume::device_path(id), mode)?))
    }

    fn begin_write(&self, disk: u32) -> Result<Box<dyn WriteSession>> {
        session::begin_write(disk, self.system_disk())
    }

    fn mount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
        mount::mount(id, at)
    }

    fn unmount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
        mount::unmount(id, at)
    }

    fn set_offline(&self, disk: u32, offline: bool) -> Result<()> {
        mount::set_offline(disk, offline)
    }

    fn rescan(&self, disk: u32) -> Result<()> {
        mount::rescan(disk)
    }

    fn format(
        &self,
        id: &VolumeId,
        fs: FsKind,
        label: &str,
        cluster: Option<u32>,
        quick: bool,
        progress: &dyn ProgressSink,
    ) -> Result<()> {
        // FormatEx wants to know whether the volume is on removable media
        let removable = self
            .enumerate_volumes()
            .ok()
            .and_then(|vs| vs.into_iter().find(|v| v.id == *id))
            .and_then(|v| v.disk)
            .and_then(disk::is_removable)
            .unwrap_or(false);
        format::format(id, fs, label, cluster, quick, removable, progress)
    }

    fn shrink_volume(&self, id: &VolumeId, new_len: u64) -> Result<()> {
        format::shrink_volume(id, new_len)
    }

    fn extend_volume(&self, id: &VolumeId, new_len: u64) -> Result<()> {
        format::extend_volume(id, new_len)
    }

    fn smart(&self, disk: u32) -> Result<Option<SmartData>> {
        smart::smart(disk)
    }

    fn is_elevated(&self) -> bool {
        elevation::is_elevated()
    }

    fn system_disk(&self) -> Option<u32> {
        *self.system_disk.get_or_init(elevation::find_system_disk)
    }

    fn free_letters(&self) -> Vec<char> {
        mount::free_letters()
    }

    fn run_tool(&self, tool: ExternalTool, args: &[String]) -> Result<String> {
        run_tool(tool, args)
    }
    fn os_name(&self) -> &'static str {
        "windows"
    }
    fn elevation_hint(&self) -> &'static str {
        crate::ELEVATION_HINT_WINDOWS
    }
    fn disk_of_path(&self, path: &std::path::Path) -> Option<(u32, Option<u32>)> {
        let full = crate::canonical_existing(path)?;
        let volumes = volume::enumerate_volumes().ok()?;
        let v = volume::volume_of_path(&full.to_string_lossy(), &volumes)?;
        let disk = v.disk?;
        let number = disk::disk_info(disk)
            .and_then(|d| d.partitions.iter().find(|p| p.start == v.start).map(|p| p.number));
        Some((disk, number))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_resolve_in_system32() {
        // resolution only; nothing is executed
        let p = system32_path(ExternalTool::FormatCom.exe_name()).unwrap();
        assert!(
            p.to_string_lossy().to_ascii_lowercase().ends_with(r"system32\format.com"),
            "{}",
            p.display()
        );
        assert!(p.is_absolute());
        assert!(p.exists());
        assert!(system32_path(ExternalTool::BcdBoot.exe_name()).unwrap().exists());
    }

    #[test]
    fn disk_of_path_finds_the_windows_folder_on_the_system_disk() {
        // query-only: works without elevation
        let p = WindowsPlatform::new();
        let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let found = p.disk_of_path(std::path::Path::new(&windir));
        if let Some(system) = p.system_disk() {
            let (disk, number) = found.expect("the Windows folder is on a disk");
            assert_eq!(disk, system);
            assert!(number.is_some_and(|n| n >= 1), "{number:?}");
        }
        // a file that does not exist yet resolves through its existing parent
        let new_file = std::path::Path::new(&windir).join("pm-no-such-dir").join("x.img");
        assert_eq!(p.disk_of_path(&new_file), found);
        assert_eq!(p.disk_of_path(std::path::Path::new(r"\\no-such-server\share\x")), None);
    }

    fn cmd(args: &[&str]) -> Result<String> {
        let exe = system32_path("cmd.exe").unwrap();
        run_exe(&exe, "cmd.exe", &args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn run_exe_captures_output_and_exit_code() {
        // benign stand-in for the real tools: `cmd /c ...`
        assert_eq!(cmd(&["/c", "echo hello"]).unwrap().trim(), "hello");
        match cmd(&["/c", "echo oops 1>&2 & exit 3"]) {
            Err(Error::Platform { code, msg }) => {
                assert_eq!(code, 3);
                assert!(msg.starts_with("cmd.exe failed: ") && msg.contains("oops"), "{msg}");
            }
            other => panic!("expected exit-code failure, got {other:?}"),
        }
    }
}
