//! Exclusive write sessions on a Linux disk.
//!
//! `begin_write`:
//! 1. refuse (`Error::Busy`) when a partition of the disk is held by device-mapper / md
//!    (`holders/`) or is an active swap area, or when one of the disk's mounts could not be
//!    restored faithfully: a bind mount or btrfs subvolume (its root is not `/`) or a mount
//!    hidden under a later mount ([`mount_conflicts`]);
//! 2. unmount every filesystem of the disk (`/proc/self/mountinfo`), deepest mount point
//!    first. On the FIRST failure everything already unmounted is mounted again — same device,
//!    type, per-mount flags and filesystem options, mount-point directory recreated if needed —
//!    and the session is refused with `Error::Busy`: a failed `begin_write` never leaves a
//!    filesystem unmounted ([`unmount_all`]);
//! 3. open the disk `O_RDWR | O_EXCL` (the kernel refuses while anything still mounts or
//!    claims it, e.g. a mount in another namespace) and take `flock(LOCK_EX | LOCK_NB)` (the
//!    lock udev and `systemd-repart` honour), retrying for ~2 s because udev holds the lock
//!    briefly while it probes; then check that the opened disk is still the one the caller
//!    enumerated (the `verify` callback: disk sequence number, or name + size). Failure:
//!    re-mount, error.
//!
//! `commit_table` writes the table, `fsync`s and re-reads it with `BLKRRPART` (`EBUSY` retried
//! 3 times 300 ms apart, then `partprobe`). `finish` (and `Drop`) `fsync`, unlock, close and,
//! when something was written, re-read the table once more (holding the lock, so udev does not
//! re-read it a second time behind our back), then wait for udev (`udevadm settle`) and mount
//! the unmounted filesystems again: all of them when the session wrote nothing, otherwise those
//! the session did not write into whose partition node still exists with the same start and
//! length (like Windows, which remounts the volumes it finds after the disk comes back; a
//! partition whose content was replaced is left unmounted for the user or the next step).

use super::ioctl;
use super::mounts::{mount_conflicts, MountEntry};
use crate::posix::{cmd, os_error, FdDevice};
use crate::traits::WriteSession;
use pm_core::{BlockDevice, DiskInfo, Error, Result};
use pm_parttable::{write_table, Table};
use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const REREAD_ATTEMPTS: u32 = 3;
const REREAD_DELAY: Duration = Duration::from_millis(300);

/// Unmounting and mounting, injectable so the rollback logic can be unit-tested.
pub trait Mounter: Send + Sync {
    fn unmount(&self, mount_point: &str) -> io::Result<()>;
    /// Mount `e.device` at `e.mount_point` again as it was: `e.fstype`, the per-mount flags
    /// of `e.options` and the filesystem options of `e.super_options`.
    fn remount(&self, e: &MountEntry) -> io::Result<()>;
}

/// `MS_*` flags for the per-mount options of a mountinfo line.
pub fn vfs_flags(options: &str) -> libc::c_ulong {
    options.split(',').fold(0, |f, o| {
        f | match o {
            "ro" => libc::MS_RDONLY,
            "nosuid" => libc::MS_NOSUID,
            "nodev" => libc::MS_NODEV,
            "noexec" => libc::MS_NOEXEC,
            "noatime" => libc::MS_NOATIME,
            "nodiratime" => libc::MS_NODIRATIME,
            "relatime" => libc::MS_RELATIME,
            "strictatime" => libc::MS_STRICTATIME,
            "sync" => libc::MS_SYNCHRONOUS,
            "dirsync" => libc::MS_DIRSYNC,
            _ => 0,
        }
    })
}

/// The filesystem options to pass as `mount(2)` data: the superblock options without the
/// read-only / read-write state (carried by the flags) and SELinux's `seclabel` marker.
pub fn fs_data(super_options: &str) -> String {
    super_options
        .split(',')
        .filter(|o| !o.is_empty() && !matches!(*o, "rw" | "ro" | "seclabel"))
        .collect::<Vec<_>>()
        .join(",")
}

/// `-o` for the `mount` command: per-mount options followed by the filesystem options.
pub fn command_options(e: &MountEntry) -> String {
    let mut v: Vec<&str> = e.options.split(',').filter(|o| !o.is_empty()).collect();
    let data = fs_data(&e.super_options);
    v.extend(data.split(',').filter(|o| !o.is_empty()));
    v.join(",")
}

/// The real system calls (`umount2`, `mount(2)`), with the `mount` command as a fallback for
/// filesystems the kernel cannot mount by itself (FUSE: `ntfs-3g`, `fuseblk`).
pub struct SystemMounter;

impl Mounter for SystemMounter {
    fn unmount(&self, mount_point: &str) -> io::Result<()> {
        ioctl::umount2(mount_point, 0)
    }

    fn remount(&self, e: &MountEntry) -> io::Result<()> {
        // a desktop automounter may have removed its directory under /media
        if !Path::new(&e.mount_point).exists() {
            std::fs::create_dir_all(&e.mount_point)?;
        }
        let data = fs_data(&e.super_options);
        let data = (!data.is_empty()).then_some(data.as_str());
        let first = match ioctl::mount(&e.device, &e.mount_point, &e.fstype, vfs_flags(&e.options), data) {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };
        let opts = command_options(e);
        let attempt = |opts: &str| {
            let mut args: Vec<&str> = Vec::new();
            if !e.fstype.starts_with("fuse") {
                args.extend(["-t", e.fstype.as_str()]);
            }
            if !opts.is_empty() {
                args.extend(["-o", opts]);
            }
            args.extend([e.device.as_str(), e.mount_point.as_str()]);
            cmd::run("mount", &args)
        };
        let mut result = attempt(&opts);
        if result.is_err() && e.fstype.starts_with("fuse") {
            // FUSE reports options of its own (user_id=, blksize=) the helper may not accept
            result = attempt(&e.options);
        }
        let cmd_err = match result {
            Ok(_) => return Ok(()),
            Err(cmd_err) => cmd_err,
        };
        // last resort: same device, type and per-mount flags, without the filesystem options
        match ioctl::mount(&e.device, &e.mount_point, &e.fstype, vfs_flags(&e.options), None) {
            Ok(()) => {
                log::warn!(
                    "re-mounted {} at {} without its filesystem options `{}` ({first}; mount command: {cmd_err})",
                    e.device,
                    e.mount_point,
                    fs_data(&e.super_options)
                );
                Ok(())
            }
            Err(last) => {
                log::warn!(
                    "re-mount {} at {}: {first}; mount command: {cmd_err}; without options: {last}",
                    e.device,
                    e.mount_point
                );
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
pub fn reread(fd: BorrowedFd<'_>, path: &str) -> Result<()> {
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

/// Take `flock(LOCK_EX)` on `dev`, retrying for about a second (udev holds the lock briefly
/// while it probes a disk).
fn lock_with_retry(dev: &FdDevice) -> bool {
    for _ in 0..20 {
        if dev.try_lock_exclusive().unwrap_or(false) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Wait (at most 10 s) until udev has processed the queued device events, so partition
/// nodes and `/dev/disk/by-*` links are in place. No-op without `udevadm`.
pub fn udev_settle() {
    if cmd::has_tool("udevadm") {
        if let Err(e) = cmd::run("udevadm", &["settle", "--timeout=10"]) {
            log::debug!("udevadm settle: {e}");
        }
    }
}

/// Open `path` for the session: `O_RDWR | O_EXCL` plus a non-blocking exclusive `flock`.
///
/// Both can fail transiently while udev probes the disk (it takes the lock for a moment), so
/// each attempt is repeated for about two seconds before the disk is reported busy.
fn open_exclusive(path: &str) -> Result<FdDevice> {
    const ATTEMPTS: u32 = 20;
    const DELAY: Duration = Duration::from_millis(100);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let busy = match FdDevice::open_with(Path::new(path), false, libc::O_EXCL, ioctl::geometry) {
            Ok(dev) => match dev.try_lock_exclusive() {
                Ok(true) => return Ok(dev),
                Ok(false) => format!("{path} is locked by another program (flock)"),
                Err(e) => return Err(os_error(&e, &format!("lock {path}"))),
            },
            Err(Error::Platform { code, .. }) if code == libc::EBUSY as u32 => format!(
                "{path} is in use (mounted in another mount namespace, or claimed by LVM / RAID / another program)"
            ),
            Err(e) => return Err(e),
        };
        if attempt >= ATTEMPTS {
            return Err(Error::Busy(busy));
        }
        std::thread::sleep(DELAY);
    }
}

/// A filesystem the session unmounted, with the byte range of its partition (`None` for a
/// filesystem on the whole disk).
#[derive(Clone, Debug)]
pub struct Unmounted {
    pub entry: MountEntry,
    pub extent: Option<(u64, u64)>,
}

/// Start a write session on disk `index` (`d` is its current description with the kernel's
/// partitions, `sys_disk_dir` its `/sys/block/<dev>` directory, `mounts` the mount table in
/// kernel order); `verify` checks that the opened disk is the one the caller enumerated. See
/// the module documentation.
pub fn begin_write(
    index: u32,
    d: &DiskInfo,
    sys_disk_dir: &Path,
    mounts: &[MountEntry],
    swaps: &[String],
    mounter: Arc<dyn Mounter>,
    verify: &dyn Fn(BorrowedFd<'_>) -> Result<()>,
) -> Result<Box<dyn WriteSession>> {
    let mut nodes = vec![d.path.clone()];
    nodes.extend(d.partitions.iter().filter_map(|p| p.volume.as_ref().map(|v| v.0.clone())));
    let users = users_of_disk(sys_disk_dir, &nodes, swaps);
    if !users.is_empty() {
        return Err(Error::Busy(users.join("; ")));
    }
    let mine: Vec<MountEntry> = mounts.iter().filter(|e| nodes.contains(&e.device)).cloned().collect();
    let conflicts = mount_conflicts(&mine, mounts);
    if !conflicts.is_empty() {
        return Err(Error::Busy(conflicts.join("; ")));
    }
    let unmounted = unmount_all(&*mounter, &mine)?;
    if !unmounted.is_empty() {
        udev_settle(); // unmounting makes udev probe the partitions (and lock the disk)
    }
    let opened = open_exclusive(&d.path).and_then(|dev| verify(dev.fd()).map(|()| dev));
    match opened {
        Ok(dev) => {
            let unmounted = unmounted
                .into_iter()
                .map(|entry| {
                    let extent = d
                        .partitions
                        .iter()
                        .find(|p| p.volume.as_ref().is_some_and(|v| v.0 == entry.device))
                        .map(|p| (p.start, p.len));
                    Unmounted { entry, extent }
                })
                .collect();
            Ok(Box::new(LinuxWriteSession::new(index, dev, unmounted, mounter)))
        }
        Err(e) => {
            let failed = remount_all(&*mounter, &unmounted);
            Err(match e {
                Error::Busy(msg) => busy(msg, unmounted.len(), failed),
                other if failed.is_empty() => other,
                other => busy(other.to_string(), unmounted.len(), failed),
            })
        }
    }
}

/// Byte ranges `[start, end)` written so far, merged and sorted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Written(Vec<(u64, u64)>);

impl Written {
    /// Record a write of `len` bytes at `offset`.
    pub fn add(&mut self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        let (mut start, mut end) = (offset, offset.saturating_add(len));
        let mut kept = Vec::with_capacity(self.0.len() + 1);
        for &(s, e) in &self.0 {
            if e < start || s > end {
                kept.push((s, e));
            } else {
                start = start.min(s);
                end = end.max(e);
            }
        }
        kept.push((start, end));
        kept.sort_unstable();
        self.0 = kept;
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether any written byte lies in `[start, start + len)`.
    pub fn overlaps(&self, (start, len): (u64, u64)) -> bool {
        let end = start.saturating_add(len);
        self.0.iter().any(|&(s, e)| s < end && start < e)
    }

    pub fn ranges(&self) -> &[(u64, u64)] {
        &self.0
    }
}

/// The session's device: records which byte ranges were written.
struct Tracked {
    dev: FdDevice,
    written: Written,
}

impl BlockDevice for Tracked {
    fn len(&self) -> u64 {
        self.dev.len()
    }
    fn sector_size(&self) -> u32 {
        self.dev.sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.dev.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        // recorded before the write: a failed write may still have changed some sectors
        self.written.add(offset, buf.len() as u64);
        self.dev.write_at(offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        self.dev.flush()
    }
    fn is_read_only(&self) -> bool {
        self.dev.is_read_only()
    }
}

/// After a write, whether to mount a filesystem again: only a partition (not a whole-disk
/// filesystem) that the session did not write into (`written` does not overlap its extent: a
/// table edit writes outside every partition, a wipe / restore / clone writes inside) and whose
/// node still covers the same range (`still_there`).
pub fn remount_after_write(
    written: &Written,
    extent: Option<(u64, u64)>,
    still_there: impl Fn((u64, u64)) -> bool,
) -> bool {
    extent.is_some_and(|x| !written.overlaps(x) && still_there(x))
}

/// Whether partition node `node` still exists with byte range `extent`.
fn still_there(node: &str, extent: (u64, u64)) -> bool {
    Path::new(node).exists() && super::mount::kernel_extent(node.trim_start_matches("/dev/")) == Some(extent)
}

/// Write session on one Linux disk; see the module documentation.
pub struct LinuxWriteSession {
    disk: u32,
    path: String,
    device: Option<Tracked>,
    unmounted: Vec<Unmounted>,
    mounter: Arc<dyn Mounter>,
}

impl LinuxWriteSession {
    /// Wrap an opened, locked disk; `unmounted` (in unmount order) is mounted again at the end.
    pub fn new(disk: u32, device: FdDevice, unmounted: Vec<Unmounted>, mounter: Arc<dyn Mounter>) -> Self {
        LinuxWriteSession {
            disk,
            path: device.path().to_string(),
            device: Some(Tracked { dev: device, written: Written::default() }),
            unmounted,
            mounter,
        }
    }

    /// fsync, unlock, close, re-read the table, mount again. Every step runs; the first error
    /// is returned.
    ///
    /// The lock is handed over to a read-only descriptor before the writable one is closed:
    /// closing a written disk makes udev re-read its partition table on its own (removing and
    /// re-adding every partition node, racing with whoever looks at them next); udev skips
    /// that while the disk is locked and only sends change events. The table is then re-read
    /// once, here; after the lock is released udev's queue is drained (`udevadm settle`).
    fn restore(&mut self) -> Result<()> {
        let Some(Tracked { dev, written }) = self.device.take() else { return Ok(()) };
        let wrote = !written.is_empty();
        let mut first: Option<Error> = None;
        if let Err(e) = dev.sync() {
            first.get_or_insert(os_error(&e, &format!("fsync {}", self.path)));
        }
        let ro = FdDevice::open_with(Path::new(&self.path), true, 0, ioctl::geometry);
        if let Err(e) = dev.unlock() {
            log::debug!("{}: unlock: {e}", self.path);
        }
        if let Ok(ro) = &ro {
            if !lock_with_retry(ro) {
                log::debug!("{}: could not re-lock for the final re-read", self.path);
            }
        }
        drop(dev);
        match ro {
            Ok(ro) => {
                if wrote {
                    if let Err(e) = reread(ro.fd(), &self.path) {
                        first.get_or_insert(e);
                    }
                }
                // release the lock first: udev processes a locked disk's events only after it
                drop(ro);
                if wrote || !self.unmounted.is_empty() {
                    udev_settle();
                }
            }
            Err(e) => {
                first.get_or_insert(e);
            }
        }

        let unmounted = std::mem::take(&mut self.unmounted);
        if !wrote {
            let entries: Vec<MountEntry> = unmounted.into_iter().map(|u| u.entry).collect();
            let failed = remount_all(&*self.mounter, &entries);
            if !failed.is_empty() {
                first.get_or_insert(Error::Busy(format!(
                    "the write session on {} wrote nothing, but could NOT mount again: {}",
                    self.path,
                    failed.join(", ")
                )));
            }
        } else {
            let (keep, gone): (Vec<Unmounted>, Vec<Unmounted>) = unmounted
                .into_iter()
                .partition(|u| remount_after_write(&written, u.extent, |x| still_there(&u.entry.device, x)));
            for u in &gone {
                log::info!(
                    "{} not mounted again at {}: the session wrote into it or its partition changed",
                    u.entry.device,
                    u.entry.mount_point
                );
            }
            let entries: Vec<MountEntry> = keep.into_iter().map(|u| u.entry).collect();
            for f in remount_all(&*self.mounter, &entries) {
                log::warn!("after writing {}: could not mount again {f}", self.path);
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
        // no udev_settle here: udev waits for the disk lock this session holds
        reread(dev.dev.fd(), &self.path)
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
    use std::sync::Mutex;

    /// Records calls; unmounting a mount point listed in `busy` fails with EBUSY, re-mounting
    /// one listed in `stuck` fails with EIO.
    #[derive(Default)]
    struct MockMounter {
        busy: Vec<&'static str>,
        stuck: Vec<&'static str>,
        log: Mutex<Vec<String>>,
    }

    impl MockMounter {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Mounter for MockMounter {
        fn unmount(&self, mp: &str) -> io::Result<()> {
            self.log.lock().unwrap().push(format!("umount {mp}"));
            if self.busy.contains(&mp) {
                Err(io::Error::from_raw_os_error(libc::EBUSY))
            } else {
                Ok(())
            }
        }
        fn remount(&self, e: &MountEntry) -> io::Result<()> {
            self.log.lock().unwrap().push(format!("mount {} {} {}", e.device, e.mount_point, e.fstype));
            if self.stuck.contains(&e.mount_point.as_str()) {
                Err(io::Error::from_raw_os_error(libc::EIO))
            } else {
                Ok(())
            }
        }
    }

    fn entry(dev: &str, mp: &str, fstype: &str) -> MountEntry {
        MountEntry {
            device: dev.into(),
            mount_point: mp.into(),
            fstype: fstype.into(),
            options: "rw".into(),
            root: "/".into(),
            ..Default::default()
        }
    }

    fn ok_verify(_: BorrowedFd<'_>) -> Result<()> {
        Ok(())
    }

    #[test]
    fn all_unmounted_deepest_first() {
        let m = MockMounter::default();
        let mounts = [entry("/dev/loop0p1", "/mnt/a", "vfat"), entry("/dev/loop0p2", "/mnt/a/b", "ext4")];
        let done = unmount_all(&m, &mounts).unwrap();
        assert_eq!(done.len(), 2);
        assert_eq!(m.log(), ["umount /mnt/a/b", "umount /mnt/a"]);
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
            m.log(),
            ["umount /mnt/p1", "umount /mnt/p2", "mount /dev/loop0p1 /mnt/p1 vfat"],
            "nothing is left unmounted"
        );
    }

    #[test]
    fn first_busy_unmounts_nothing_and_failed_remounts_are_reported() {
        let m = MockMounter { busy: vec!["/mnt/p1"], ..Default::default() };
        let mounts = [entry("/dev/sdb1", "/mnt/p1", "vfat"), entry("/dev/sdb2", "/mnt/p2", "ext4")];
        assert!(matches!(unmount_all(&m, &mounts), Err(Error::Busy(_))));
        assert_eq!(m.log(), ["umount /mnt/p1"]);

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
        assert_eq!(m.log()[3..], ["mount /dev/sdb2 /mnt/p2 ext4", "mount /dev/sdb1 /mnt/p1 vfat"]);
    }

    #[test]
    fn remount_options_are_the_original_ones() {
        let flags = vfs_flags("ro,nosuid,nodev,noexec,relatime");
        assert_eq!(
            flags,
            libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_RELATIME
        );
        assert_eq!(vfs_flags("rw,noatime,sync,dirsync"), libc::MS_NOATIME | libc::MS_SYNCHRONOUS | libc::MS_DIRSYNC);
        assert_eq!(fs_data("rw,fmask=0022,dmask=0022,codepage=437,seclabel"), "fmask=0022,dmask=0022,codepage=437");
        assert_eq!(fs_data("rw"), "");
        let e = MountEntry {
            options: "rw,nosuid,nodev,relatime".into(),
            super_options: "rw,uid=1000,errors=remount-ro".into(),
            ..entry("/dev/sdb1", "/media/u/X", "vfat")
        };
        assert_eq!(command_options(&e), "rw,nosuid,nodev,relatime,uid=1000,errors=remount-ro");
    }

    /// A regular file standing in for the disk, with one "partition" node `<file>1`.
    fn file_disk(t: &tempfile::TempDir) -> (String, DiskInfo, std::path::PathBuf) {
        let img = t.path().join("disk.img");
        std::fs::write(&img, vec![0u8; 1 << 20]).unwrap();
        let path = img.to_string_lossy().into_owned();
        let d = DiskInfo {
            path: path.clone(),
            partitions: vec![pm_core::PartitionInfo {
                start: 4096,
                len: 8192,
                volume: Some(pm_core::VolumeId(format!("{path}1"))),
                ..Default::default()
            }],
            ..Default::default()
        };
        let sys = t.path().join("sys-disk");
        std::fs::create_dir_all(&sys).unwrap();
        (path, d, sys)
    }

    #[test]
    fn failure_to_open_after_unmounting_remounts() {
        // a first session holds the flock, so the second one unmounts, fails to lock, and
        // must mount everything again
        let t = tempfile::tempdir().unwrap();
        let (path, d, sys) = file_disk(&t);
        let mounts = [entry(&format!("{path}1"), "/mnt/x", "vfat"), entry("/dev/other1", "/mnt/y", "ext4")];

        let m = Arc::new(MockMounter::default());
        let first = begin_write(3, &d, &sys, &mounts, &[], m.clone(), &ok_verify).unwrap();
        assert_eq!(first.disk(), 3);
        assert_eq!(m.log(), ["umount /mnt/x"], "only the disk's own mounts");

        let m2 = Arc::new(MockMounter::default());
        match begin_write(3, &d, &sys, &mounts, &[], m2.clone(), &ok_verify) {
            Err(Error::Busy(msg)) => {
                assert!(msg.contains("locked by another program"), "{msg}");
                assert!(msg.contains("mounted again"), "{msg}");
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("second session on a locked disk"),
        }
        assert_eq!(m2.log(), [format!("umount /mnt/x"), format!("mount {path}1 /mnt/x vfat")]);
        // the first session wrote nothing: dropping it mounts its filesystem again
        drop(first);
        assert_eq!(m.log(), [format!("umount /mnt/x"), format!("mount {path}1 /mnt/x vfat")]);
        // swap / holders refuse before anything is unmounted
        let m3 = Arc::new(MockMounter::default());
        let refused = begin_write(3, &d, &sys, &mounts, &[format!("{path}1")], m3.clone(), &ok_verify);
        assert!(matches!(refused, Err(Error::Busy(msg)) if msg.contains("swap")));
        assert!(m3.log().is_empty());
    }

    #[test]
    fn a_different_disk_behind_the_name_is_refused_and_rolled_back() {
        // I1: the verify callback (disk sequence number / size) says the opened disk is not the
        // enumerated one
        let t = tempfile::tempdir().unwrap();
        let (path, d, sys) = file_disk(&t);
        let mounts = [entry(&format!("{path}1"), "/mnt/x", "vfat")];
        let m = Arc::new(MockMounter::default());
        let changed = |_: BorrowedFd<'_>| -> Result<()> {
            Err(Error::Invalid("disk 3 changed since the disk list was read: refresh and retry".into()))
        };
        match begin_write(3, &d, &sys, &mounts, &[], m.clone(), &changed) {
            Err(Error::Invalid(msg)) => assert!(msg.contains("refresh and retry"), "{msg}"),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("session on a changed disk"),
        }
        assert_eq!(m.log(), [format!("umount /mnt/x"), format!("mount {path}1 /mnt/x vfat")]);
        // and the lock was released with the descriptor: a correct disk opens fine
        assert!(begin_write(3, &d, &sys, &[], &[], m, &ok_verify).is_ok());
    }

    #[test]
    fn a_written_session_does_not_remount_a_vanished_partition() {
        let t = tempfile::tempdir().unwrap();
        let (path, d, sys) = file_disk(&t);
        let mounts = [entry(&format!("{path}1"), "/mnt/x", "vfat")];
        let m = Arc::new(MockMounter::default());
        let mut s = begin_write(3, &d, &sys, &mounts, &[], m.clone(), &ok_verify).unwrap();
        s.device().write_at(0, b"x").unwrap();
        assert!(s.finish().is_err(), "BLKRRPART on a regular file fails");
        // `<file>1` is no partition node: not mounted again after the write
        assert_eq!(m.log(), ["umount /mnt/x"]);
    }

    #[test]
    fn bind_mounts_and_subvolumes_refuse_before_unmounting() {
        let t = tempfile::tempdir().unwrap();
        let (path, d, sys) = file_disk(&t);
        let sub = MountEntry { root: "/@home".into(), ..entry(&format!("{path}1"), "/home", "btrfs") };
        let m = Arc::new(MockMounter::default());
        match begin_write(3, &d, &sys, &[entry(&format!("{path}1"), "/mnt/x", "btrfs"), sub], &[], m.clone(), &ok_verify) {
            Err(Error::Busy(msg)) => assert!(msg.contains("bind mount or subvolume"), "{msg}"),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("session over a subvolume mount"),
        }
        assert!(m.log().is_empty(), "nothing unmounted");
    }

    #[test]
    fn written_ranges_merge_and_decide_remounting() {
        let mut w = Written::default();
        assert!(w.is_empty());
        w.add(0, 512); // protective MBR
        w.add(512, 512 * 33); // GPT header + entries: merges with the MBR
        w.add(100 << 20, 0); // empty writes are ignored
        w.add((64 << 20) - 512 * 33, 512 * 33); // backup GPT
        assert_eq!(w.ranges(), [(0, 512 * 34), ((64 << 20) - 512 * 33, 64 << 20)]);
        let there = |_: (u64, u64)| true;
        let p1 = Some((1 << 20, 32 << 20));
        let p2 = Some((40 << 20, 8 << 20));
        // a table edit: both partitions untouched
        assert!(remount_after_write(&w, p1, there) && remount_after_write(&w, p2, there));
        // the first sector of p2 rewritten (wipe / restore / clone onto the same layout)
        w.add(40 << 20, 512);
        assert!(remount_after_write(&w, p1, there));
        assert!(!remount_after_write(&w, p2, there));
        // touching the bytes just before / after an extent does not count
        let mut edge = Written::default();
        edge.add((1 << 20) - 1, 1);
        edge.add(33 << 20, 10);
        assert!(remount_after_write(&edge, p1, there));
        edge.add((33 << 20) - 1, 1);
        assert!(!remount_after_write(&edge, p1, there));
        // a whole-disk filesystem, or a partition that moved, is never mounted again
        assert!(!remount_after_write(&Written::default(), None, there));
        assert!(!remount_after_write(&Written::default(), p1, |_| false));
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
