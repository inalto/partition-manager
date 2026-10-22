//! Exclusive write sessions on a Linux disk.
//!
//! `begin_write`:
//! 1. refuse (`Error::Busy`) when a partition of the disk is held by device-mapper / md
//!    (`holders/`) or is an active swap area;
//! 2. unmount every filesystem of the disk listed in `/proc/mounts`, deepest mount point
//!    first. On the FIRST failure everything already unmounted is mounted again and the
//!    session is refused with `Error::Busy` — a failed `begin_write` never leaves a filesystem
//!    unmounted ([`unmount_all`]);
//! 3. open the disk `O_RDWR | O_EXCL` (the kernel refuses while anything still mounts or
//!    claims it, e.g. a mount in another namespace) and take `flock(LOCK_EX | LOCK_NB)` (the
//!    lock udev and `systemd-repart` honour). Failure: re-mount, `Error::Busy`.
//!
//! `commit_table` writes the table, `fsync`s and re-reads it with `BLKRRPART` (`EBUSY` retried
//! 3 times 300 ms apart, then `partprobe`). `finish` (and `Drop`) `fsync`, unlock, close and
//! re-read the table once more. Filesystems unmounted by the session are deliberately NOT
//! mounted again after a successful session: the layout may have changed underneath them; a
//! refresh shows them unmounted and the user mounts what they want.

use super::ioctl;
use super::mounts::MountEntry;
use crate::posix::{cmd, os_error, FdDevice};
use crate::traits::WriteSession;
use pm_core::{BlockDevice, Error, Result};
use pm_parttable::{write_table, Table};
use std::io;
use std::path::Path;
use std::time::Duration;

const REREAD_ATTEMPTS: u32 = 3;
const REREAD_DELAY: Duration = Duration::from_millis(300);

/// Unmounting and mounting, injectable so the rollback logic can be unit-tested.
pub trait Mounter {
    fn unmount(&self, mount_point: &str) -> io::Result<()>;
    /// Mount `e.device` at `e.mount_point` again with `e.fstype` (read-only if it was).
    fn remount(&self, e: &MountEntry) -> io::Result<()>;
}

/// The real system calls (`umount2`, `mount(2)`), with the `mount` command as a fallback for
/// filesystems the kernel cannot mount by itself (FUSE: `ntfs-3g`, `fuseblk`).
pub struct SystemMounter;

impl Mounter for SystemMounter {
    fn unmount(&self, mount_point: &str) -> io::Result<()> {
        ioctl::umount2(mount_point, 0)
    }

    fn remount(&self, e: &MountEntry) -> io::Result<()> {
        let flags = if e.read_only() { libc::MS_RDONLY } else { 0 };
        let first = match ioctl::mount(&e.device, &e.mount_point, &e.fstype, flags, None) {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };
        let mut args: Vec<&str> = Vec::new();
        if !e.fstype.starts_with("fuse") {
            args.extend(["-t", e.fstype.as_str()]);
        }
        if e.read_only() {
            args.extend(["-o", "ro"]);
        }
        args.extend([e.device.as_str(), e.mount_point.as_str()]);
        match cmd::run("mount", &args) {
            Ok(_) => Ok(()),
            Err(cmd_err) => {
                log::warn!("re-mount {} at {}: {first}; mount command: {cmd_err}", e.device, e.mount_point);
                Err(first)
            }
        }
    }
}

/// Mount the entries of `unmounted` again, in reverse order (parents before children); returns
/// a description of every entry that could not be mounted.
pub fn remount_all(m: &dyn Mounter, unmounted: &[MountEntry]) -> Vec<String> {
    unmounted
        .iter()
        .rev()
        .filter_map(|e| {
            m.remount(e)
                .err()
                .map(|err| format!("{} on {} ({err})", e.device, e.mount_point))
        })
        .collect()
}

/// `Error::Busy` after a failed step, saying what was rolled back and what could not be.
fn busy(what: String, unmounted: usize, not_remounted: Vec<String>) -> Error {
    let mut msg = what;
    if unmounted > 0 {
        if not_remounted.is_empty() {
            msg.push_str(&format!("; the {unmounted} filesystem(s) already unmounted were mounted again"));
        } else {
            msg.push_str(&format!("; could NOT mount again: {}", not_remounted.join(", ")));
        }
    }
    Error::Busy(msg)
}

/// Unmount every entry of `mounts`, deepest mount point first. On the first failure the
/// entries already unmounted are mounted again and `Error::Busy` names the busy filesystem;
/// on success the unmounted entries are returned (in unmount order).
pub fn unmount_all(m: &dyn Mounter, mounts: &[MountEntry]) -> Result<Vec<MountEntry>> {
    let mut order: Vec<MountEntry> = mounts.to_vec();
    order.sort_by_key(|e| std::cmp::Reverse(Path::new(&e.mount_point).components().count()));
    let mut done: Vec<MountEntry> = Vec::new();
    for e in order {
        if let Err(err) = m.unmount(&e.mount_point) {
            let failed = remount_all(m, &done);
            return Err(busy(
                format!("{} on {} cannot be unmounted ({err})", e.device, e.mount_point),
                done.len(),
                failed,
            ));
        }
        log::info!("unmounted {} from {}", e.device, e.mount_point);
        done.push(e);
    }
    Ok(done)
}

/// What still uses the disk although nothing of it is mounted: device-mapper / md holders of
/// the disk or a partition (`<sys>/<dev>/holders`, `<sys>/<dev>/<part>/holders`) and active
/// swap areas. Empty when the disk is free.
pub fn users_of_disk(sys_disk_dir: &Path, nodes: &[String], swaps: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let holders = |dir: &Path, name: &str, out: &mut Vec<String>| {
        if let Ok(rd) = std::fs::read_dir(dir.join("holders")) {
            for h in rd.filter_map(|e| e.ok()) {
                out.push(format!("{name} is held by {} (LVM / RAID / encryption)", h.file_name().to_string_lossy()));
            }
        }
    };
    let disk_name = sys_disk_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    holders(sys_disk_dir, &disk_name, &mut out);
    if let Ok(rd) = std::fs::read_dir(sys_disk_dir) {
        for e in rd.filter_map(|e| e.ok()) {
            if e.path().join("partition").exists() {
                holders(&e.path(), &e.file_name().to_string_lossy(), &mut out);
            }
        }
    }
    for s in swaps.iter().filter(|s| nodes.contains(s)) {
        out.push(format!("{s} is an active swap area (run swapoff first)"));
    }
    out
}

/// Re-read the partition table of the open disk: `BLKRRPART`, `EBUSY` retried, then
/// `partprobe`. A loop device attached without partition scanning (`losetup` without `-P`)
/// rejects the request with `EINVAL`; that is logged, not an error.
pub fn reread(fd: std::os::fd::BorrowedFd<'_>, path: &str) -> Result<()> {
    let mut last = None;
    for attempt in 0..REREAD_ATTEMPTS {
        match ioctl::reread_partitions(fd) {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                last = Some(e);
                if attempt + 1 < REREAD_ATTEMPTS {
                    std::thread::sleep(REREAD_DELAY);
                }
            }
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) && path.starts_with("/dev/loop") => {
                log::warn!("{path}: partition scanning is off for this loop device (attach it with losetup -P)");
                return Ok(());
            }
            Err(e) => return Err(os_error(&e, &format!("re-read partition table of {path}"))),
        }
    }
    match cmd::run("partprobe", &[path]) {
        Ok(_) => Ok(()),
        Err(e) => {
            let busy = last.unwrap_or_else(|| io::Error::from_raw_os_error(libc::EBUSY));
            log::warn!("{path}: partprobe after BLKRRPART ({busy}): {e}");
            Err(os_error(&busy, &format!("re-read partition table of {path} (partprobe: {e})")))
        }
    }
}

/// Open `path` for the session: `O_RDWR | O_EXCL` plus a non-blocking exclusive `flock`.
fn open_exclusive(path: &str) -> Result<FdDevice> {
    let dev = FdDevice::open_with(Path::new(path), false, libc::O_EXCL, ioctl::geometry).map_err(|e| match e {
        Error::Platform { code, .. } if code == libc::EBUSY as u32 => Error::Busy(format!(
            "{path} is in use (mounted in another mount namespace, or claimed by LVM / RAID / another program)"
        )),
        other => other,
    })?;
    match dev.try_lock_exclusive() {
        Ok(true) => Ok(dev),
        Ok(false) => Err(Error::Busy(format!("{path} is locked by another program (flock)"))),
        Err(e) => Err(os_error(&e, &format!("lock {path}"))),
    }
}

/// Start a write session on disk `index` (`d` is its current description, `sys_disk_dir` its
/// `/sys/block/<dev>` directory); see the module documentation.
pub fn begin_write(
    index: u32,
    d: &pm_core::DiskInfo,
    sys_disk_dir: &Path,
    mounts: &[MountEntry],
    swaps: &[String],
    mounter: &dyn Mounter,
) -> Result<Box<dyn WriteSession>> {
    let mut nodes = vec![d.path.clone()];
    nodes.extend(d.partitions.iter().filter_map(|p| p.volume.as_ref().map(|v| v.0.clone())));
    let users = users_of_disk(sys_disk_dir, &nodes, swaps);
    if !users.is_empty() {
        return Err(Error::Busy(users.join("; ")));
    }
    let mine: Vec<MountEntry> = mounts.iter().filter(|e| nodes.contains(&e.device)).cloned().collect();
    let unmounted = unmount_all(mounter, &mine)?;
    match open_exclusive(&d.path) {
        Ok(dev) => Ok(Box::new(LinuxWriteSession::new(index, dev))),
        Err(e) => {
            let failed = remount_all(mounter, &unmounted);
            Err(match e {
                Error::Busy(msg) => busy(msg, unmounted.len(), failed),
                other if failed.is_empty() => other,
                other => busy(other.to_string(), unmounted.len(), failed),
            })
        }
    }
}

/// Write session on one Linux disk; see the module documentation.
pub struct LinuxWriteSession {
    disk: u32,
    path: String,
    device: Option<FdDevice>,
}

impl LinuxWriteSession {
    /// Wrap an opened, locked disk.
    pub fn new(disk: u32, device: FdDevice) -> Self {
        LinuxWriteSession { disk, path: device.path().to_string(), device: Some(device) }
    }

    /// fsync, unlock, close, re-read the table. Every step runs; the first error is returned.
    fn restore(&mut self) -> Result<()> {
        let Some(dev) = self.device.take() else { return Ok(()) };
        let mut first: Option<Error> = None;
        if let Err(e) = dev.sync() {
            first.get_or_insert(os_error(&e, &format!("fsync {}", self.path)));
        }
        if let Err(e) = dev.unlock() {
            log::debug!("{}: unlock: {e}", self.path);
        }
        drop(dev);
        match FdDevice::open_with(Path::new(&self.path), true, 0, ioctl::geometry) {
            Ok(ro) => {
                if let Err(e) = reread(ro.fd(), &self.path) {
                    first.get_or_insert(e);
                }
            }
            Err(e) => {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }
}

impl WriteSession for LinuxWriteSession {
    fn disk(&self) -> u32 {
        self.disk
    }

    fn device(&mut self) -> &mut dyn BlockDevice {
        self.device.as_mut().expect("write session is open until finish")
    }

    fn commit_table(&mut self, t: &Table) -> Result<()> {
        let dev = self.device.as_mut().expect("write session is open until finish");
        write_table(dev, t)?;
        dev.flush()?;
        reread(dev.fd(), &self.path)
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.restore()
    }
}

impl Drop for LinuxWriteSession {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            log::warn!("disk {}: closing the write session: {e}", self.disk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records calls; unmounting a mount point listed in `busy` fails with EBUSY, re-mounting
    /// one listed in `stuck` fails with EIO.
    #[derive(Default)]
    struct MockMounter {
        busy: Vec<&'static str>,
        stuck: Vec<&'static str>,
        log: RefCell<Vec<String>>,
    }

    impl Mounter for MockMounter {
        fn unmount(&self, mp: &str) -> io::Result<()> {
            self.log.borrow_mut().push(format!("umount {mp}"));
            if self.busy.contains(&mp) {
                Err(io::Error::from_raw_os_error(libc::EBUSY))
            } else {
                Ok(())
            }
        }
        fn remount(&self, e: &MountEntry) -> io::Result<()> {
            self.log.borrow_mut().push(format!("mount {} {} {}", e.device, e.mount_point, e.fstype));
            if self.stuck.contains(&e.mount_point.as_str()) {
                Err(io::Error::from_raw_os_error(libc::EIO))
            } else {
                Ok(())
            }
        }
    }

    fn entry(dev: &str, mp: &str, fstype: &str) -> MountEntry {
        MountEntry { device: dev.into(), mount_point: mp.into(), fstype: fstype.into(), options: "rw".into() }
    }

    #[test]
    fn all_unmounted_deepest_first() {
        let m = MockMounter::default();
        let mounts = [entry("/dev/loop0p1", "/mnt/a", "vfat"), entry("/dev/loop0p2", "/mnt/a/b", "ext4")];
        let done = unmount_all(&m, &mounts).unwrap();
        assert_eq!(done.len(), 2);
        assert_eq!(*m.log.borrow(), ["umount /mnt/a/b", "umount /mnt/a"]);
    }

    #[test]
    fn second_busy_partition_remounts_the_first_and_refuses() {
        // Review Focus 1: two mounted partitions, the second one busy
        let m = MockMounter { busy: vec!["/mnt/p2"], ..Default::default() };
        let mounts = [entry("/dev/loop0p1", "/mnt/p1", "vfat"), entry("/dev/loop0p2", "/mnt/p2", "ext4")];
        match unmount_all(&m, &mounts) {
            Err(Error::Busy(msg)) => {
                assert!(msg.starts_with("/dev/loop0p2 on /mnt/p2 cannot be unmounted"), "{msg}");
                assert!(msg.contains("1 filesystem(s) already unmounted were mounted again"), "{msg}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            *m.log.borrow(),
            ["umount /mnt/p1", "umount /mnt/p2", "mount /dev/loop0p1 /mnt/p1 vfat"],
            "nothing is left unmounted"
        );
    }

    #[test]
    fn first_busy_unmounts_nothing_and_failed_remounts_are_reported() {
        let m = MockMounter { busy: vec!["/mnt/p1"], ..Default::default() };
        let mounts = [entry("/dev/sdb1", "/mnt/p1", "vfat"), entry("/dev/sdb2", "/mnt/p2", "ext4")];
        assert!(matches!(unmount_all(&m, &mounts), Err(Error::Busy(_))));
        assert_eq!(*m.log.borrow(), ["umount /mnt/p1"]);

        let m = MockMounter { busy: vec!["/mnt/p3"], stuck: vec!["/mnt/p1"], ..Default::default() };
        let mounts = [
            entry("/dev/sdb1", "/mnt/p1", "vfat"),
            entry("/dev/sdb2", "/mnt/p2", "ext4"),
            entry("/dev/sdb3", "/mnt/p3", "xfs"),
        ];
        match unmount_all(&m, &mounts) {
            Err(Error::Busy(msg)) => assert!(msg.contains("could NOT mount again: /dev/sdb1 on /mnt/p1"), "{msg}"),
            other => panic!("{other:?}"),
        }
        // re-mounted in reverse unmount order
        assert_eq!(m.log.borrow()[3..], ["mount /dev/sdb2 /mnt/p2 ext4", "mount /dev/sdb1 /mnt/p1 vfat"]);
    }

    #[test]
    fn failure_to_open_after_unmounting_remounts() {
        // the "disk" is a regular file; a first session holds the flock, so the second one
        // unmounts, fails to lock, and must mount everything again
        let t = tempfile::tempdir().unwrap();
        let img = t.path().join("disk.img");
        std::fs::write(&img, vec![0u8; 1 << 20]).unwrap();
        let path = img.to_string_lossy().into_owned();
        let d = pm_core::DiskInfo {
            path: path.clone(),
            partitions: vec![pm_core::PartitionInfo {
                volume: Some(pm_core::VolumeId(format!("{path}1"))),
                ..Default::default()
            }],
            ..Default::default()
        };
        let sys = t.path().join("sys-disk");
        std::fs::create_dir_all(&sys).unwrap();
        let mounts = [entry(&format!("{path}1"), "/mnt/x", "vfat"), entry("/dev/other1", "/mnt/y", "ext4")];

        let m = MockMounter::default();
        let first = begin_write(3, &d, &sys, &mounts, &[], &m).unwrap();
        assert_eq!(first.disk(), 3);
        assert_eq!(*m.log.borrow(), ["umount /mnt/x"], "only the disk's own mounts");

        let m2 = MockMounter::default();
        match begin_write(3, &d, &sys, &mounts, &[], &m2) {
            Err(Error::Busy(msg)) => {
                assert!(msg.contains("locked by another program"), "{msg}");
                assert!(msg.contains("mounted again"), "{msg}");
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("second session on a locked disk"),
        }
        assert_eq!(*m2.log.borrow(), [format!("umount /mnt/x"), format!("mount {path}1 /mnt/x vfat")]);
        drop(first); // BLKRRPART on a regular file fails: logged by Drop
        // swap / holders refuse before anything is unmounted
        let m3 = MockMounter::default();
        let refused = begin_write(3, &d, &sys, &mounts, &[format!("{path}1")], &m3);
        assert!(matches!(refused, Err(Error::Busy(msg)) if msg.contains("swap")));
        assert!(m3.log.borrow().is_empty());
    }

    #[test]
    fn holders_and_swap_block_the_disk() {
        let t = tempfile::tempdir().unwrap();
        let disk = t.path().join("sdb");
        std::fs::create_dir_all(disk.join("holders")).unwrap();
        std::fs::create_dir_all(disk.join("sdb1/holders/dm-0")).unwrap();
        std::fs::write(disk.join("sdb1/partition"), "1\n").unwrap();
        std::fs::create_dir_all(disk.join("sdb2/holders")).unwrap();
        std::fs::write(disk.join("sdb2/partition"), "2\n").unwrap();
        let nodes = vec!["/dev/sdb".to_string(), "/dev/sdb1".into(), "/dev/sdb2".into()];
        let users = users_of_disk(&disk, &nodes, &["/dev/sdb2".into(), "/dev/sda3".into()]);
        assert_eq!(users.len(), 2, "{users:?}");
        assert!(users[0].starts_with("sdb1 is held by dm-0"));
        assert!(users[1].starts_with("/dev/sdb2 is an active swap area"));
        std::fs::remove_dir(disk.join("sdb1/holders/dm-0")).unwrap();
        assert!(users_of_disk(&disk, &nodes, &[]).is_empty());
    }
}
