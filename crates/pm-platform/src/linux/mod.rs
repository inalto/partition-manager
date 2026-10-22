//! Linux implementation of [`Platform`].
//!
//! Disks come from sysfs (`/sys/block`, see [`sysfs`]), mounts from `/proc/self/mounts`
//! ([`mounts`]), raw I/O goes through [`FdDevice`] on the `/dev` nodes. A [`VolumeId`] is the
//! partition's device node (`/dev/sdb1`). Reading tables and filesystem signatures needs read
//! access to the nodes (root, or the `disk` group); without it disks are listed from sysfs alone
//! (`TableKind::Unknown`, no partition types). Writes, mounting, formatting and resizing need
//! root. Disk indexes are positions in the sorted list of disks (loop devices last, so
//! attaching an image does not renumber the real disks).
//!
//! Filesystems are created and resized with the system tools (`mkfs.*`, `ntfsresize`,
//! `e2fsck` + `resize2fs`); S.M.A.R.T. comes from `smartctl`. A missing tool gives
//! `Error::Unsupported` naming it. Boot repair is Windows-only.

pub mod format;
pub mod ioctl;
pub mod mount;
pub mod mounts;
pub mod resize;
pub mod session;
pub mod smart;
pub mod sysfs;
pub mod system_disk;

pub use mounts::{parse_proc_mounts, MountEntry};
pub use session::LinuxWriteSession;
pub use sysfs::parse_block_dir;
pub use system_disk::system_disk_from;

use crate::posix::{is_root, os_error, statvfs, FdDevice};
use crate::traits::{ExternalTool, MountPoint, OpenMode, Platform, SmartData, WriteSession};
use pm_core::{BlockDevice, DiskInfo, Error, FsKind, ProgressSink, Result, TableKind, VolumeId, VolumeInfo};
use pm_parttable::read_table;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

/// The real Linux platform.
pub struct LinuxPlatform {
    /// `/sys/block`.
    sys_block: PathBuf,
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new()
    }
}

/// Open a block device node (or image file) with the right geometry.
pub fn open_node(path: &str, mode: OpenMode) -> Result<FdDevice> {
    FdDevice::open_with(Path::new(path), mode == OpenMode::ReadOnly, 0, ioctl::geometry)
}

/// Table kind, partition types / GUIDs / names and partition numbers (table-slot order) from
/// the on-disk table, then filesystem kinds and labels from their signatures. Needs read
/// access; without it `d` keeps the sysfs view.
fn enrich_from_device(d: &mut DiskInfo) {
    let mut dev = match open_node(&d.path, OpenMode::ReadOnly) {
        Ok(dev) => dev,
        Err(e) => {
            log::debug!("{}: {e}; partitions from sysfs only", d.path);
            return;
        }
    };
    match read_table(&mut dev) {
        Ok(t) if matches!(t.kind, TableKind::Mbr | TableKind::Gpt) => {
            let kernel = std::mem::take(&mut d.partitions);
            let base = std::mem::take(d);
            *d = t.to_disk_info(base);
            for p in &mut d.partitions {
                if p.ptype.is_some_and(|t| t.is_extended_mbr()) && !p.logical {
                    continue; // the extended container is not a volume
                }
                if let Some(k) = kernel.iter().find(|k| k.start == p.start) {
                    p.volume = k.volume.clone();
                    p.flags.read_only |= k.flags.read_only;
                }
            }
        }
        Ok(t) => {
            d.table = t.kind;
            d.partitions.clear();
        }
        Err(e) => log::debug!("{}: read_table: {e}", d.path),
    }
    if d.partitions.is_empty() {
        // a filesystem (or swap) on the whole disk, without a partition table
        if let Some(probe) = pm_fs::detect(&mut dev, 0).filter(|p| p.kind != FsKind::Unknown) {
            d.partitions.push(whole_disk_partition(d, Some(probe.kind), probe.label));
        }
    }
    crate::detect_filesystems(d, &mut dev);
}

/// A pseudo-partition covering a disk that holds a filesystem directly ("superfloppy" USB
/// sticks, WSL's virtual disks): its volume is the disk node itself. Without it the disk would
/// look blank ("Uninitialized", all unallocated) and could be initialized over its data; with
/// it the planner refuses to initialize until the disk is wiped explicitly.
fn whole_disk_partition(d: &DiskInfo, fs: Option<FsKind>, label: String) -> pm_core::PartitionInfo {
    pm_core::PartitionInfo {
        number: 1,
        start: 0,
        len: d.size,
        fs,
        label,
        volume: Some(VolumeId(d.path.clone())),
        ..Default::default()
    }
}

/// `(disk index, partition number)` of the device node `node` among `disks`.
fn locate(disks: &[DiskInfo], node: &str) -> Option<(u32, Option<u32>)> {
    disks.iter().find_map(|d| {
        if d.path == node {
            return Some((d.index, None));
        }
        d.partitions
            .iter()
            .find(|p| p.volume.as_ref().is_some_and(|v| v.0 == node))
            .map(|p| (d.index, Some(p.number)))
    })
}

/// The block device node holding `path`, from its `st_dev` (`/sys/dev/block/M:m`). `None` for
/// filesystems with anonymous device numbers (btrfs subvolumes, overlay, tmpfs, ...).
fn node_of_st_dev(path: &Path) -> Option<String> {
    let meta = path.ancestors().find_map(|a| std::fs::metadata(a).ok())?;
    let (major, minor) = ioctl::major_minor(meta.dev());
    let sys = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()?;
    Some(format!("/dev/{}", sys.file_name()?.to_string_lossy()))
}

impl LinuxPlatform {
    pub fn new() -> Self {
        LinuxPlatform { sys_block: PathBuf::from("/sys/block") }
    }

    /// Disks as sysfs describes them (cheap: no device I/O).
    fn sysfs_disks(&self) -> Vec<DiskInfo> {
        sysfs::parse_block_dir(&self.sys_block)
    }

    fn sysfs_disk(&self, index: u32) -> Result<DiskInfo> {
        self.sysfs_disks()
            .into_iter()
            .find(|d| d.index == index)
            .ok_or_else(|| Error::NotFound(format!("disk {index}")))
    }

    /// Disks with tables, filesystems, mounts and usage, plus the matching volumes.
    fn scan(&self) -> (Vec<DiskInfo>, Vec<VolumeInfo>) {
        let mut disks = self.sysfs_disks();
        let mounts = mounts::read_mounts();
        let swaps = mounts::read_swaps();
        let system = system_disk_from(&mounts, &disks);
        let mut volumes = Vec::new();
        for d in &mut disks {
            enrich_from_device(d);
            d.flags.system = system == Some(d.index);
            if d.partitions.is_empty() {
                // unreadable (not root) but mounted or swapped on as a whole
                if let Some(e) = mounts.iter().find(|e| e.device == d.path) {
                    let fs = mounts::fs_from_mount_type(&e.fstype);
                    d.partitions.push(whole_disk_partition(d, fs, String::new()));
                } else if swaps.contains(&d.path) {
                    d.partitions.push(whole_disk_partition(d, Some(FsKind::LinuxSwap), String::new()));
                }
            }
            for p in &mut d.partitions {
                let Some(id) = p.volume.clone() else { continue };
                let mine: Vec<&MountEntry> = mounts.iter().filter(|e| e.device == id.0).collect();
                p.mount_points = mine.iter().map(|e| e.mount_point.clone()).collect();
                if p.fs.is_none() {
                    p.fs = mine.first().and_then(|e| mounts::fs_from_mount_type(&e.fstype));
                }
                p.flags.boot |= p.mount_points.iter().any(|m| m == "/");
                let (total, free) = p
                    .mount_points
                    .first()
                    .and_then(|m| statvfs::usage(Path::new(m)).ok())
                    .unwrap_or((0, 0));
                if total > 0 {
                    p.used = Some(total.saturating_sub(free));
                }
                volumes.push(VolumeInfo {
                    id,
                    disk: Some(d.index),
                    start: p.start,
                    len: p.len,
                    fs: p.fs,
                    label: p.label.clone(),
                    mount_points: p.mount_points.clone(),
                    total,
                    free,
                });
            }
        }
        (disks, volumes)
    }
}

impl Platform for LinuxPlatform {
    fn enumerate_disks(&self) -> Result<Vec<DiskInfo>> {
        Ok(self.scan().0)
    }

    fn enumerate_volumes(&self) -> Result<Vec<VolumeInfo>> {
        Ok(self.scan().1)
    }

    fn open_disk(&self, index: u32, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(open_node(&self.sysfs_disk(index)?.path, mode)?))
    }

    fn open_volume(&self, id: &VolumeId, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        let node = mount::node(id)?;
        let meta = std::fs::metadata(node).map_err(|e| os_error(&e, node))?;
        if !meta.file_type().is_block_device() {
            return Err(Error::NotFound(format!("{node} is not a block device")));
        }
        Ok(Box::new(open_node(node, mode)?))
    }

    fn begin_write(&self, disk: u32) -> Result<Box<dyn WriteSession>> {
        let d = self.sysfs_disk(disk)?;
        let name = d.path.trim_start_matches("/dev/").to_string();
        session::begin_write(
            disk,
            &d,
            &self.sys_block.join(name),
            &mounts::read_mounts(),
            &mounts::read_swaps(),
            &session::SystemMounter,
        )
    }

    fn mount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
        mount::mount(id, at)
    }

    fn unmount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
        mount::unmount(id, at)
    }

    fn set_offline(&self, disk: u32, offline: bool) -> Result<()> {
        log::info!("disk {disk}: Linux has no offline state; set_offline({offline}) ignored");
        Ok(())
    }

    fn rescan(&self, disk: u32) -> Result<()> {
        let path = self.sysfs_disk(disk)?.path;
        let dev = open_node(&path, OpenMode::ReadOnly)?;
        match ioctl::reread_partitions(dev.fd()) {
            Ok(()) => Ok(()),
            Err(e) if matches!(e.raw_os_error(), Some(libc::EBUSY) | Some(libc::EINVAL)) => {
                log::info!("{path}: partition table not re-read ({e})");
                Ok(())
            }
            Err(e) => Err(os_error(&e, &format!("re-read partition table of {path}"))),
        }
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
        format::format(id, fs, label, cluster, quick, progress)
    }

    fn shrink_volume(&self, id: &VolumeId, new_len: u64) -> Result<()> {
        resize::resize(id, new_len)
    }

    fn extend_volume(&self, id: &VolumeId, new_len: u64) -> Result<()> {
        resize::resize(id, new_len)
    }

    fn smart(&self, disk: u32) -> Result<Option<SmartData>> {
        smart::smart(&self.sysfs_disk(disk)?.path)
    }

    fn is_elevated(&self) -> bool {
        is_root()
    }

    fn system_disk(&self) -> Option<u32> {
        // not cached: indexes are positions and change when disks come and go
        system_disk_from(&mounts::read_mounts(), &self.sysfs_disks())
    }

    fn free_letters(&self) -> Vec<char> {
        Vec::new()
    }

    fn run_tool(&self, tool: ExternalTool, _args: &[String]) -> Result<String> {
        Err(Error::Unsupported(format!(
            "boot repair is Windows-only ({} does not exist on Linux)",
            tool.exe_name()
        )))
    }

    fn os_name(&self) -> &'static str {
        "linux"
    }

    fn elevation_hint(&self) -> &'static str {
        crate::ELEVATION_HINT_UNIX
    }

    fn disk_of_path(&self, path: &Path) -> Option<(u32, Option<u32>)> {
        let full = crate::canonical_existing(path)?;
        let disks = self.sysfs_disks();
        if let Some(found) = node_of_st_dev(&full).and_then(|n| locate(&disks, &n)) {
            return Some(found);
        }
        let mounts = mounts::read_mounts();
        let entry = mounts::longest_mount(&mounts, &full.to_string_lossy())?;
        locate(&disks, &entry.device)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::PartitionInfo;

    #[test]
    fn locate_nodes() {
        let part = |n: u32, node: &str| PartitionInfo { number: n, volume: Some(VolumeId(node.into())), ..Default::default() };
        let disks = vec![
            DiskInfo { index: 0, path: "/dev/sda".into(), partitions: vec![part(1, "/dev/sda1"), part(2, "/dev/sda2")], ..Default::default() },
            DiskInfo { index: 1, path: "/dev/nvme0n1".into(), partitions: vec![part(3, "/dev/nvme0n1p3")], ..Default::default() },
        ];
        assert_eq!(locate(&disks, "/dev/sda2"), Some((0, Some(2))));
        assert_eq!(locate(&disks, "/dev/nvme0n1"), Some((1, None)));
        assert_eq!(locate(&disks, "/dev/nvme0n1p3"), Some((1, Some(3))));
        assert_eq!(locate(&disks, "/dev/dm-0"), None);
    }

    #[test]
    fn platform_identity() {
        let p = LinuxPlatform::new();
        assert_eq!(p.os_name(), "linux");
        assert!(p.elevation_hint().contains("sudo"));
        assert!(p.free_letters().is_empty());
        assert!(matches!(p.run_tool(ExternalTool::BcdBoot, &[]), Err(Error::Unsupported(_))));
        assert_eq!(p.is_elevated(), crate::posix::euid() == 0);
        assert!(p.set_offline(0, true).is_ok());
        // enumeration never fails, even unprivileged
        let disks = p.enumerate_disks().unwrap();
        for (i, d) in disks.iter().enumerate() {
            assert_eq!(d.index, i as u32);
            assert!(d.path.starts_with("/dev/"));
        }
        assert!(matches!(
            p.mount(&VolumeId("/dev/sda1".into()), &MountPoint::Letter('E')),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(p.open_disk(9999, OpenMode::ReadOnly), Err(Error::NotFound(_))));
    }
}
