//! Partition Manager platform layer: the only crate that talks to the operating system.
//!
//! Engines, the GUI and the CLI consume the [`Platform`] trait. [`default_platform`] returns the
//! real implementation (Windows; Linux and macOS follow) or [`NullPlatform`] elsewhere; [`MockPlatform`] backs the tests.

pub mod aligned;
pub mod console;
pub mod file_platform;
pub mod mock;
#[cfg(unix)]
pub mod posix;
pub mod shell;
pub mod traits;
#[cfg(windows)]
pub mod windows;

pub use file_platform::{FilePlatform, NO_SYSTEM_DISK};
pub use mock::MockPlatform;
pub use traits::*;
#[cfg(unix)]
pub use posix::{BenchFile, FileBenchTarget};
#[cfg(windows)]
pub use windows::{
    friendly_error, parse_ata_smart, parse_nvme_smart_log, BenchFile, FileBenchTarget, WinDevice, WindowsPlatform,
};

use pm_core::{BlockDevice, DiskInfo, Error, FsKind, ProgressSink, Result, VolumeId, VolumeInfo};
use std::sync::Arc;

/// The platform for the current OS: [`WindowsPlatform`] on Windows, [`NullPlatform`] elsewhere.
pub fn default_platform() -> Arc<dyn Platform> {
    #[cfg(windows)]
    {
        Arc::new(WindowsPlatform::new())
    }
    #[cfg(not(windows))]
    {
        Arc::new(NullPlatform)
    }
}

/// Copy volume details (id, mount points, label, fs, used = total - free) onto the partition of
/// `disk` that starts at the volume's start offset.
pub fn merge_volumes(disk: &mut DiskInfo, volumes: &[VolumeInfo]) {
    for v in volumes.iter().filter(|v| v.disk == Some(disk.index)) {
        if let Some(p) = disk.partitions.iter_mut().find(|p| p.start == v.start) {
            p.volume = Some(v.id.clone());
            p.mount_points = v.mount_points.clone();
            p.label = v.label.clone();
            p.fs = v.fs;
            p.used = (v.total > 0).then(|| v.total.saturating_sub(v.free));
        }
    }
}

/// Fill `fs` and `label` of every partition of `disk` whose filesystem is still unknown (no
/// mounted volume reported one) from the on-disk signature at its start ([`pm_fs::detect`]).
/// `used` is left alone: the fill level stays unknown, so the planner's "unknown fill level"
/// rule keeps shrinks of such a filesystem away. MBR extended containers are skipped. A read
/// that fails (e.g. a query-only handle) leaves the partition unchanged.
pub fn detect_filesystems(disk: &mut DiskInfo, dev: &mut dyn BlockDevice) {
    let mbr = disk.table == pm_core::TableKind::Mbr;
    for p in disk.partitions.iter_mut().filter(|p| p.fs.is_none()) {
        if mbr && !p.logical && p.ptype.is_some_and(|t| t.is_extended_mbr()) {
            continue;
        }
        if let Some(probe) = pm_fs::detect(dev, p.start) {
            if probe.kind != FsKind::Unknown {
                p.fs = Some(probe.kind);
                if p.label.is_empty() {
                    p.label = probe.label;
                }
            }
        }
    }
}

/// Platform for unsupported operating systems: every operation fails with `Error::Unsupported`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullPlatform;

fn unsupported<T>() -> Result<T> {
    Err(Error::Unsupported(
        "this operating system is not supported by the platform layer".into(),
    ))
}

impl Platform for NullPlatform {
    fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
        unsupported()
    }
    fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>> {
        unsupported()
    }
    fn open_disk(&self, _index: u32, _mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        unsupported()
    }
    fn open_volume(&self, _id: &VolumeId, _mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        unsupported()
    }
    fn begin_write(&self, _disk: u32) -> Result<Box<dyn WriteSession>> {
        unsupported()
    }
    fn mount(&self, _id: &VolumeId, _at: &MountPoint) -> Result<()> {
        unsupported()
    }
    fn unmount(&self, _id: &VolumeId, _at: &MountPoint) -> Result<()> {
        unsupported()
    }
    fn set_offline(&self, _disk: u32, _offline: bool) -> Result<()> {
        unsupported()
    }
    fn rescan(&self, _disk: u32) -> Result<()> {
        unsupported()
    }
    fn format(
        &self,
        _id: &VolumeId,
        _fs: FsKind,
        _label: &str,
        _cluster: Option<u32>,
        _quick: bool,
        _progress: &dyn ProgressSink,
    ) -> Result<()> {
        unsupported()
    }
    fn shrink_volume(&self, _id: &VolumeId, _new_len: u64) -> Result<()> {
        unsupported()
    }
    fn extend_volume(&self, _id: &VolumeId, _new_len: u64) -> Result<()> {
        unsupported()
    }
    fn smart(&self, _disk: u32) -> Result<Option<SmartData>> {
        unsupported()
    }
    fn is_elevated(&self) -> bool {
        false
    }
    fn system_disk(&self) -> Option<u32> {
        None
    }
    fn free_letters(&self) -> Vec<char> {
        Vec::new()
    }
    fn os_name(&self) -> &'static str {
        "none"
    }
    fn run_tool(&self, _tool: ExternalTool, _args: &[String]) -> Result<String> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::NullProgress;

    #[test]
    fn null_platform_is_unsupported() {
        let p = NullPlatform;
        assert!(matches!(p.enumerate_disks(), Err(Error::Unsupported(_))));
        assert!(matches!(p.begin_write(0), Err(Error::Unsupported(_))));
        assert!(matches!(
            p.format(&VolumeId("x".into()), FsKind::Ntfs, "", None, true, &NullProgress),
            Err(Error::Unsupported(_))
        ));
        assert!(!p.is_elevated());
        assert!(p.free_letters().is_empty());
        assert_eq!(p.system_disk(), None);
    }

    #[test]
    fn detect_filesystems_fills_unmounted_partitions_only() {
        use pm_core::{MemDevice, PartitionInfo, PartitionType, TableKind, MIB};
        let mut dev = MemDevice::new(8 * MIB);
        // an NTFS boot sector at 1 MiB, nothing at 3 MiB, an "NTFS" EBR-container head at 5 MiB
        for off in [MIB, 5 * MIB] {
            dev.write_at(off + 3, b"NTFS    ").unwrap();
        }
        let part = |number, start, ptype, logical| PartitionInfo {
            number,
            start,
            len: MIB,
            ptype: Some(PartitionType::Mbr(ptype)),
            logical,
            ..Default::default()
        };
        let mut d = DiskInfo {
            table: TableKind::Mbr,
            partitions: vec![part(1, MIB, 0x07, false), part(2, 3 * MIB, 0x07, false), part(3, 5 * MIB, 0x0F, false)],
            ..Default::default()
        };
        let mut mounted = d.clone();
        detect_filesystems(&mut d, &mut dev);
        assert_eq!(d.partitions[0].fs, Some(FsKind::Ntfs));
        assert_eq!(d.partitions[0].used, None, "the fill level stays unknown");
        assert_eq!(d.partitions[1].fs, None, "no signature: raw");
        assert_eq!(d.partitions[2].fs, None, "extended containers are not probed");
        // a filesystem a mounted volume reported is kept
        mounted.partitions[0].fs = Some(FsKind::ReFs);
        detect_filesystems(&mut mounted, &mut dev);
        assert_eq!(mounted.partitions[0].fs, Some(FsKind::ReFs));
    }

    #[test]
    fn mount_point_path_strings() {
        assert_eq!(MountPoint::Letter('e').to_path_string(), r"E:\");
        assert_eq!(MountPoint::Folder(r"C:\mnt\x".into()).to_path_string(), r"C:\mnt\x\");
        assert_eq!(MountPoint::Folder(r"C:\mnt\y\".into()).to_path_string(), r"C:\mnt\y\");
    }

    #[test]
    fn fs_names_for_format() {
        assert_eq!(fs_format_name(FsKind::Ntfs), Some("NTFS"));
        assert_eq!(fs_format_name(FsKind::Fat32), Some("FAT32"));
        assert_eq!(fs_format_name(FsKind::Fat16), Some("FAT"));
        assert_eq!(fs_format_name(FsKind::ExFat), Some("exFAT"));
        assert_eq!(fs_format_name(FsKind::ReFs), Some("ReFS"));
        assert_eq!(fs_format_name(FsKind::Ext4), None);
    }
}
