//! Linux implementation of [`Platform`].
//!
//! Disks come from sysfs (`/sys/block`, see [`sysfs`]), mounts from `/proc/self/mountinfo`
//! ([`mounts`]), raw I/O goes through [`FdDevice`] on the `/dev` nodes. A [`VolumeId`] is the
//! partition's device node (`/dev/sdb1`), validated before every volume operation
//! ([`mount::node`]). Reading tables and filesystem signatures needs read access to the nodes
//! (root, or the `disk` group); without it disks are listed from sysfs alone
//! (`TableKind::Unknown`, no partition types). Writes, mounting, formatting and resizing need
//! root.
//!
//! Disk indexes are stable for the life of the process: the first enumeration numbers the disks
//! in sorted order (loop devices last), and from then on a disk (kernel name + disk sequence
//! number, or + size on kernels before 5.15) keeps its index, a newly attached disk gets the
//! next unused one, and the index of a disk that went away is never reused — so indexes can
//! have gaps. `open_disk`, `begin_write`, `rescan` and `smart` refuse an index whose disk is
//! gone or was replaced under the same name ("the disk list changed, refresh and retry"), so a
//! hot-plug between listing and writing, or an enumeration in the middle of an operation, can
//! never redirect a write. Volume operations likewise check that the volume's disk is still
//! the enumerated one and that the partition still starts (and ends) where it was seen.
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

pub use mounts::{mount_conflicts, parse_mountinfo, parse_proc_mounts, MountEntry};
pub use session::LinuxWriteSession;
pub use sysfs::{classify_node, parse_block_dir, parse_block_dir_with, parse_vpd_pg80, NodeKind};
pub use system_disk::system_disk_from;

use crate::posix::{is_root, os_error, statvfs, FdDevice};
use crate::traits::{ExternalTool, MountPoint, OpenMode, Platform, SmartData, WriteSession};
use pm_core::{BlockDevice, DiskInfo, Error, FsKind, ProgressSink, Result, TableKind, VolumeId, VolumeInfo};
use pm_parttable::read_table;
use std::collections::HashMap;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// What identifies an enumerated disk: its kernel name, its disk sequence number (unique per
/// attached medium since boot; Linux 5.15+) and its size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskIdentity {
    pub name: String,
    pub diskseq: Option<u64>,
    pub size: u64,
}

/// `Ok` when the disk now behind the name is the enumerated one: same disk sequence number
/// when both are known, otherwise the same size.
pub fn check_identity(index: u32, expected: &DiskIdentity, diskseq: Option<u64>, size: u64) -> Result<()> {
    let same = match (expected.diskseq, diskseq) {
        (Some(a), Some(b)) => a == b,
        _ => expected.size == size,
    };
    if same {
        Ok(())
    } else {
        Err(changed(index, &expected.name))
    }
}

fn changed(index: u32, name: &str) -> Error {
    Error::Invalid(format!(
        "disk {index} (/dev/{name}) is not the disk that was listed under this number any more \
         (the disk list changed): refresh the disk list and retry"
    ))
}

/// Whether two identities name the same attached disk: same name and disk sequence number,
/// or same name and size when a sequence number is missing.
fn same_disk(a: &DiskIdentity, b: &DiskIdentity) -> bool {
    a.name == b.name
        && match (a.diskseq, b.diskseq) {
            (Some(x), Some(y)) => x == y,
            _ => a.size == b.size,
        }
}

/// The index map and what the last enumeration showed of each volume.
#[derive(Default)]
struct Cache {
    /// Every disk shown since the process started, at its index (append-only).
    ///
    /// Limit on kernels before 5.15 (no `diskseq`): the key is name + size, so another disk of
    /// exactly the same size attached under the same name keeps the old index and is not
    /// detected here. Only the caller's own identity check (serial number / disk id of the
    /// enumerated disk against the live one, e.g. the executor's same-disk check) catches that
    /// swap.
    disks: Vec<DiskIdentity>,
    /// Volume node -> (start, len, disk index) from the last enumeration.
    volumes: HashMap<String, (u64, u64, u32)>,
}

/// The real Linux platform.
pub struct LinuxPlatform {
    /// `/sys/block`.
    sys_block: PathBuf,
    cache: Mutex<Cache>,
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

/// Check the opened disk `fd` against `expected` (`BLKGETDISKSEQ`, or the size on kernels
/// without it).
fn verify_fd(index: u32, expected: &DiskIdentity, fd: BorrowedFd<'_>) -> Result<()> {
    let seq = ioctl::diskseq(fd).ok();
    let size = ioctl::size_bytes(fd).map_err(|e| os_error(&e, &format!("query size of /dev/{}", expected.name)))?;
    check_identity(index, expected, seq, size)
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
                // the kernel's node only when it covers exactly this entry (a table changed
                // behind the kernel's back must not lend a node to a different range)
                if let Some(k) = kernel.iter().find(|k| k.start == p.start && k.len == p.len) {
                    p.volume = k.volume.clone();
                    p.flags.read_only |= k.flags.read_only;
                }
            }
        }
        Ok(t) => {
            if d.partitions.is_empty() {
                d.table = t.kind;
            } else {
                // the kernel knows partitions the table reader does not see: keep them
                d.table = TableKind::Unknown;
            }
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

fn name_of(d: &DiskInfo) -> &str {
    d.path.trim_start_matches("/dev/")
}

impl LinuxPlatform {
    pub fn new() -> Self {
        Self::with_sys_block(PathBuf::from("/sys/block"))
    }

    /// A platform reading disks from `sys_block` instead of `/sys/block` (fixture tests).
    pub fn with_sys_block(sys_block: PathBuf) -> Self {
        LinuxPlatform { sys_block, cache: Mutex::new(Cache::default()) }
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Disks as sysfs describes them now (cheap: no device I/O), indexed by position.
    fn sysfs_disks(&self) -> Vec<DiskInfo> {
        sysfs::parse_block_dir(&self.sys_block)
    }

    fn identity(&self, d: &DiskInfo) -> DiskIdentity {
        let name = name_of(d).to_string();
        DiskIdentity { diskseq: sysfs::diskseq(&self.sys_block, &name), name, size: d.size }
    }

    /// Number `fresh` through the process-wide index map: a disk seen before (same name and
    /// disk sequence number, or same name and size without one) keeps its index for the life
    /// of the process; a new disk gets the next unused index; the index of a disk that went
    /// away is never given to another one. Returned in index order.
    fn number(&self, fresh: Vec<DiskInfo>) -> Vec<DiskInfo> {
        let ids: Vec<DiskIdentity> = fresh.iter().map(|d| self.identity(d)).collect();
        let mut cache = self.cache();
        let mut out: Vec<DiskInfo> = fresh
            .into_iter()
            .zip(ids)
            .map(|(mut d, id)| {
                let i = match cache.disks.iter().position(|k| same_disk(k, &id)) {
                    Some(i) => i,
                    None => {
                        cache.disks.push(id);
                        cache.disks.len() - 1
                    }
                };
                d.index = i as u32;
                d
            })
            .collect();
        out.sort_by_key(|d| d.index);
        out
    }

    /// The current disks with their stable indexes (see [`LinuxPlatform::number`]).
    fn indexed(&self) -> Vec<DiskInfo> {
        self.number(self.sysfs_disks())
    }

    /// The disk known as `index`, after checking that the disk remembered under that index is
    /// still attached (same name and disk sequence number, or size). A disk that went away, or
    /// was replaced by another under the same name, is refused ("refresh and retry"). Without
    /// any enumeration yet the current disks are numbered first.
    fn resolve(&self, index: u32) -> Result<(DiskInfo, DiskIdentity)> {
        let current = self.sysfs_disks();
        if self.cache().disks.is_empty() {
            self.number(current.clone());
        }
        let expected = self
            .cache()
            .disks
            .get(index as usize)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("disk {index}")))?;
        let mut d = current
            .into_iter()
            .find(|d| name_of(d) == expected.name)
            .ok_or_else(|| changed(index, &expected.name))?;
        let now = self.identity(&d);
        check_identity(index, &expected, now.diskseq, now.size)?;
        d.index = index;
        Ok((d, expected))
    }

    /// `(start, len)` the last enumeration reported for volume `id`, after checking that its
    /// disk is still the one enumerated (`Err` "refresh and retry" otherwise); `None` for a
    /// volume no enumeration has shown yet.
    fn expected(&self, id: &VolumeId) -> Result<Option<(u64, u64)>> {
        let Some((start, len, disk)) = self.cache().volumes.get(&id.0).copied() else { return Ok(None) };
        self.resolve(disk)?;
        Ok(Some((start, len)))
    }

    /// Disks with tables, filesystems, mounts and usage, plus the matching volumes; remembered
    /// as what the caller sees.
    fn scan(&self) -> (Vec<DiskInfo>, Vec<VolumeInfo>) {
        let mut disks = self.indexed();
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
        self.cache().volumes = volumes
            .iter()
            .filter_map(|v| Some((v.id.0.clone(), (v.start, v.len, v.disk?))))
            .collect();
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
        let (d, expected) = self.resolve(index)?;
        let dev = open_node(&d.path, mode)?;
        verify_fd(index, &expected, dev.fd())?;
        Ok(Box::new(dev))
    }

    fn open_volume(&self, id: &VolumeId, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
        let node = mount::node(id)?;
        Ok(Box::new(open_node(&node, mode)?))
    }

    fn begin_write(&self, disk: u32) -> Result<Box<dyn WriteSession>> {
        let (d, expected) = self.resolve(disk)?;
        let dir = self.sys_block.join(name_of(&d));
        session::begin_write(
            disk,
            &d,
            &dir,
            &mounts::read_mounts(),
            &mounts::read_swaps(),
            Arc::new(session::SystemMounter),
            &|fd| verify_fd(disk, &expected, fd),
        )
    }

    fn mount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
        mount::mount(id, at, self.expected(id)?.map(|(s, l)| (s, Some(l))))
    }

    fn unmount(&self, id: &VolumeId, at: &MountPoint) -> Result<()> {
        mount::unmount(id, at)
    }

    fn set_offline(&self, disk: u32, offline: bool) -> Result<()> {
        log::info!("disk {disk}: Linux has no offline state; set_offline({offline}) ignored");
        Ok(())
    }

    fn rescan(&self, disk: u32) -> Result<()> {
        let path = self.resolve(disk)?.0.path;
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
        let expected = self.expected(id)?.map(|(s, l)| (s, Some(l)));
        format::format(id, expected, fs, label, cluster, quick, progress)
    }

    fn shrink_volume(&self, id: &VolumeId, new_len: u64) -> Result<()> {
        resize::resize(id, self.expected(id)?.map(|(s, _)| s), new_len)
    }

    fn extend_volume(&self, id: &VolumeId, new_len: u64) -> Result<()> {
        resize::resize(id, self.expected(id)?.map(|(s, _)| s), new_len)
    }

    fn smart(&self, disk: u32) -> Result<Option<SmartData>> {
        smart::smart(&self.resolve(disk)?.0.path)
    }

    fn is_elevated(&self) -> bool {
        is_root()
    }

    fn system_disk(&self) -> Option<u32> {
        // not cached: computed with the indexes the caller was shown
        system_disk_from(&mounts::read_mounts(), &self.indexed())
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

    /// The disk (and partition) holding `path`: its `st_dev` through `/sys/dev/block`, else the
    /// device of the longest `/proc/self/mountinfo` mount point above it. `None` means UNKNOWN
    /// (tmpfs, network or FUSE filesystems, LVM / device-mapper / md volumes, btrfs subvolumes
    /// whose device is not found): a caller guarding against writing onto the disk being read
    /// must refuse in that case, not proceed.
    fn disk_of_path(&self, path: &Path) -> Option<(u32, Option<u32>)> {
        let full = crate::canonical_existing(path)?;
        let disks = self.indexed();
        if let Some(found) = mounts::node_of_st_dev(&full).and_then(|n| locate(&disks, &n)) {
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
    fn identity_by_sequence_number_else_size() {
        let id = |seq| DiskIdentity { name: "sdb".into(), diskseq: seq, size: 16 << 30 };
        assert!(check_identity(2, &id(Some(7)), Some(7), 1).is_ok(), "seq decides when both known");
        match check_identity(2, &id(Some(7)), Some(9), 16 << 30) {
            Err(Error::Invalid(m)) => assert!(m.contains("disk 2 (/dev/sdb)") && m.contains("refresh"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(check_identity(2, &id(None), Some(9), 16 << 30).is_ok(), "old kernel: size");
        assert!(check_identity(2, &id(None), None, 8 << 30).is_err());
    }

    /// A sysfs disk `name` of `size512` sectors, with a `diskseq` file when `seq` is given.
    fn sys_disk(root: &Path, name: &str, size512: u64, seq: Option<u64>) {
        let d = root.join(name);
        std::fs::create_dir_all(d.join("queue")).unwrap();
        std::fs::write(d.join("size"), format!("{size512}\n")).unwrap();
        if let Some(s) = seq {
            std::fs::write(d.join("diskseq"), format!("{s}\n")).unwrap();
        }
    }

    #[test]
    fn indexes_survive_hot_plug_and_are_never_reused() {
        // names that do not exist under /dev here: nothing below opens a device
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        sys_disk(&root, "sdx", 1000, Some(10));
        sys_disk(&root, "sdy", 2000, Some(11));
        let p = LinuxPlatform::with_sys_block(root.clone());
        let idx = |p: &LinuxPlatform| p.indexed().iter().map(|d| (d.path.clone(), d.index)).collect::<Vec<_>>();
        assert_eq!(idx(&p), [("/dev/sdx".to_string(), 0), ("/dev/sdy".to_string(), 1)]);

        // hot-plug: sdy goes away, sdw arrives (it sorts first: positional indexes would shift)
        std::fs::remove_dir_all(root.join("sdy")).unwrap();
        sys_disk(&root, "sdw", 3000, Some(12));
        assert_eq!(idx(&p), [("/dev/sdx".to_string(), 0), ("/dev/sdw".to_string(), 2)]);
        assert!(matches!(p.resolve(1), Err(Error::Invalid(m)) if m.contains("refresh")), "sdy is gone");
        assert_eq!(p.resolve(0).unwrap().0.path, "/dev/sdx");
        assert_eq!(p.resolve(2).unwrap().0.path, "/dev/sdw");

        // another stick under the old name: a new index, the old one stays refused
        sys_disk(&root, "sdy", 2000, Some(13));
        assert_eq!(idx(&p).last(), Some(&("/dev/sdy".to_string(), 3)));
        assert!(p.resolve(1).is_err());
        assert_eq!(p.resolve(3).unwrap().0.path, "/dev/sdy");
        assert!(matches!(p.resolve(4), Err(Error::NotFound(_))));

        // without disk sequence numbers the size tells disks apart
        let t2 = tempfile::tempdir().unwrap();
        sys_disk(t2.path(), "sdx", 1000, None);
        let q = LinuxPlatform::with_sys_block(t2.path().to_path_buf());
        assert_eq!(q.resolve(0).unwrap().0.path, "/dev/sdx", "the first use numbers the disks");
        sys_disk(t2.path(), "sdx", 4000, None);
        assert!(q.resolve(0).is_err(), "a different size under the same name");
        assert_eq!(idx(&q), [("/dev/sdx".to_string(), 1)]);
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
        // the remembered list resolves to the same disks
        for d in &disks {
            assert_eq!(p.resolve(d.index).unwrap().0.path, d.path);
        }
    }
}
