//! macOS platform: `diskutil` for enumeration, mounting and formatting, raw `/dev/rdiskN`
//! nodes for I/O.
//!
//! The parsers ([`diskutil`]), the enumeration ([`enumerate`]), the system-disk lookup
//! ([`system_disk`]), the target checks ([`guard`]), the take-for-writing sequence ([`write`])
//! and the `diskutil` mount/format calls go through a [`DiskutilRunner`] and compile on every
//! OS, so the fixtures in `tests/fixtures/macos/` exercise them anywhere. [`MacPlatform`]
//! (whose runner executes `/usr/sbin/diskutil` by absolute path), the raw device, the write
//! session and S.M.A.R.T. are macOS-only.

pub mod device;
pub mod diskutil;
pub mod enumerate;
pub mod format;
pub mod guard;
pub mod ioctl;
pub mod mount;
#[cfg(target_os = "macos")]
pub mod session;
#[cfg(target_os = "macos")]
pub mod smart;
pub mod system_disk;
pub mod write;

pub use device::AlignedDevice;
pub use diskutil::{
    parse_apfs_containers, parse_info, parse_list, ApfsContainer, DeviceInfo, DiskutilRunner, ListedDisk,
    ListedPartition, WholeDisk,
};
pub use enumerate::{enumerate, DeviceOpener, Enumerated};

#[cfg(target_os = "macos")]
pub use platform::{MacPlatform, SystemDiskutil};

#[cfg(target_os = "macos")]
mod platform {
    use super::diskutil::{canonical_volume_id, raw_node, DiskutilRunner};
    use super::{device, enumerate, format, guard, mount, session, smart, system_disk};
    use crate::posix;
    use crate::traits::{ExternalTool, MountPoint, OpenMode, Platform, SmartData, WriteSession, ELEVATION_HINT_UNIX};
    use pm_core::{BlockDevice, DiskInfo, Error, FsKind, ProgressSink, Result, VolumeId, VolumeInfo};
    use std::path::Path;

    /// Absolute path: under `sudo` macOS keeps the user's `PATH`, so a `diskutil` found
    /// through it could be any program running as root.
    const DISKUTIL: &str = "/usr/sbin/diskutil";

    /// Runs `/usr/sbin/diskutil` by absolute path (never looked up on `PATH`; no shell,
    /// `LC_ALL=C`); a non-zero exit is `Error::Platform` with diskutil's message.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct SystemDiskutil;

    impl DiskutilRunner for SystemDiskutil {
        fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
            Ok(posix::cmd::run(DISKUTIL, args)?.stdout)
        }
    }

    /// The macOS [`Platform`].
    pub struct MacPlatform {
        runner: Box<dyn DiskutilRunner>,
    }

    impl Default for MacPlatform {
        fn default() -> Self {
            Self::new()
        }
    }

    fn open_read_only(path: &str) -> Option<Box<dyn BlockDevice>> {
        match device::open_raw(path, OpenMode::ReadOnly) {
            Ok(d) => Some(Box::new(d)),
            Err(e) => {
                log::debug!("{path}: {e}");
                None
            }
        }
    }

    impl MacPlatform {
        pub fn new() -> Self {
            Self::with_runner(Box::new(SystemDiskutil))
        }

        /// A platform whose `diskutil` calls go to `runner`.
        pub fn with_runner(runner: Box<dyn DiskutilRunner>) -> Self {
            MacPlatform { runner }
        }

        fn enumerate_all(&self) -> Result<enumerate::Enumerated> {
            enumerate::enumerate(self.runner.as_ref(), &open_read_only, self.system_disk(), posix::is_root())
        }
    }

    impl Platform for MacPlatform {
        fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
            Ok(self.enumerate_all()?.disks)
        }

        fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>> {
            Ok(self.enumerate_all()?.volumes)
        }

        fn open_disk(&self, index: u32, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
            Ok(Box::new(device::open_raw(&format!("/dev/rdisk{index}"), mode)?))
        }

        fn open_volume(&self, id: &VolumeId, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
            let slice = guard::checked_slice(self.runner.as_ref(), id)?;
            let node = raw_node(&slice).ok_or_else(|| Error::NotFound(format!("{} is not a macOS disk slice", id.0)))?;
            Ok(Box::new(device::open_raw(&node, mode)?))
        }

        fn begin_write(&self, disk: u32) -> Result<Box<dyn WriteSession>> {
            session::begin_write(self.runner.as_ref(), disk)
        }

        fn mount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
            mount::mount(self.runner.as_ref(), id, at)
        }

        fn unmount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
            mount::unmount(self.runner.as_ref(), id, at)
        }

        /// No offline state on macOS: the write session unmounts the disk instead.
        fn set_offline(&self, _disk: u32, _offline: bool) -> Result<()> {
            Ok(())
        }

        /// The kernel re-reads the partition map when the raw node is closed.
        fn rescan(&self, _disk: u32) -> Result<()> {
            Ok(())
        }

        fn format(
            &self,
            id: &VolumeId,
            fs: FsKind,
            label: &str,
            cluster: Option<u32>,
            quick: bool,
            _progress: &dyn ProgressSink,
        ) -> Result<()> {
            format::format(self.runner.as_ref(), id, fs, label, cluster, quick)
        }

        fn shrink_volume(&self, _id: &VolumeId, _new_len: u64) -> Result<()> {
            Err(Error::Unsupported("resizing filesystems is not supported on macOS yet".into()))
        }

        fn extend_volume(&self, _id: &VolumeId, _new_len: u64) -> Result<()> {
            Err(Error::Unsupported("resizing filesystems is not supported on macOS yet".into()))
        }

        fn smart(&self, disk: u32) -> Result<Option<SmartData>> {
            smart::smart(disk)
        }

        fn is_elevated(&self) -> bool {
            posix::is_root()
        }

        fn system_disk(&self) -> Option<u32> {
            system_disk::system_disk(self.runner.as_ref())
        }

        fn free_letters(&self) -> Vec<char> {
            Vec::new()
        }

        fn run_tool(&self, tool: ExternalTool, _args: &[String]) -> Result<String> {
            Err(Error::Unsupported(format!("{} is a Windows tool", tool.exe_name())))
        }

        fn os_name(&self) -> &'static str {
            "macos"
        }

        fn elevation_hint(&self) -> &'static str {
            ELEVATION_HINT_UNIX
        }

        /// `/dev/rdiskNsM` and `diskNsM` → `/dev/diskNsM`; anything else unchanged.
        fn canonical_volume_id(&self, id: &VolumeId) -> VolumeId {
            canonical_volume_id(&id.0).map(VolumeId).unwrap_or_else(|| id.clone())
        }

        fn disk_of_path(&self, path: &Path) -> Option<(u32, Option<u32>)> {
            system_disk::disk_of_path(self.runner.as_ref(), &crate::canonical_existing(path)?)
        }
    }
}
