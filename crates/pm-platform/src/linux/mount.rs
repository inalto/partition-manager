//! Mounting and unmounting partitions in folders (`mount(2)` / `umount2`, with the `mount` /
//! `umount` commands as fallback, e.g. for FUSE `ntfs-3g`), and the validation of the device
//! nodes every volume operation (mount, unmount, format, resize, open) is given.

use super::ioctl;
use super::mounts::{mount_types, read_mounts};
use super::sysfs::{classify_node, SYSFS_SECTOR};
use crate::posix::{cmd, os_error, FdDevice};
use crate::traits::MountPoint;
use pm_core::{Error, FsKind, Result, VolumeId};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::time::{Duration, Instant};

const SYS_CLASS_BLOCK: &str = "/sys/class/block";
const SYS_BLOCK: &str = "/sys/block";

/// The folder of a mount point; drive letters do not exist on Linux.
pub fn folder(at: &MountPoint) -> Result<String> {
    match at {
        MountPoint::Folder(p) => {
            let c = std::fs::canonicalize(p).map_err(|e| os_error(&e, &format!("mount point {}", p.display())))?;
            if !c.is_dir() {
                return Err(Error::Invalid(format!("{} is not a directory", c.display())));
            }
            Ok(c.to_string_lossy().into_owned())
        }
        MountPoint::Letter(c) => Err(Error::Unsupported(format!(
            "drive letters ({c}:) do not exist on Linux: mount into a folder"
        ))),
    }
}

/// The filesystem on `dev` from its signature (`None` when unrecognised or unreadable).
pub fn detect_fs(dev: &str) -> Option<FsKind> {
    let mut d = FdDevice::open_with(Path::new(dev), true, 0, ioctl::geometry).ok()?;
    pm_fs::detect(&mut d, 0).map(|p| p.kind).filter(|k| *k != FsKind::Unknown)
}

fn raw_node(id: &VolumeId) -> Result<&str> {
    if id.0.starts_with("/dev/") && id.0.len() > 5 {
        Ok(&id.0)
    } else {
        Err(Error::NotFound(format!("{} is not a Linux device node", id.0)))
    }
}

/// The canonical device node of volume `id`, which must be a block device under `/dev` that is
/// a partition of a managed disk, or a managed disk without partitions (a filesystem on the
/// whole disk). A whole partitioned disk (formatting it would overwrite the partition table),
/// device-mapper / md / zram / optical nodes and paths escaping `/dev` are refused.
pub fn node(id: &VolumeId) -> Result<String> {
    node_in(id, Path::new(SYS_CLASS_BLOCK), Path::new(SYS_BLOCK))
}

/// [`node`] against the given `/sys/class/block` and `/sys/block` directories.
pub fn node_in(id: &VolumeId, class: &Path, block_root: &Path) -> Result<String> {
    let raw = raw_node(id)?;
    let canonical = std::fs::canonicalize(raw).map_err(|e| Error::NotFound(format!("{raw}: {e}")))?;
    if !canonical.starts_with("/dev/") {
        return Err(Error::Invalid(format!("{raw} resolves to {}, outside /dev", canonical.display())));
    }
    let meta = std::fs::metadata(&canonical).map_err(|e| os_error(&e, raw))?;
    if !meta.file_type().is_block_device() {
        return Err(Error::Invalid(format!("{raw} is not a block device")));
    }
    let name = canonical.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    classify_node(class, block_root, &name).map_err(Error::Invalid)?;
    Ok(canonical.to_string_lossy().into_owned())
}

/// `(start, len)` in bytes of the node `name` as the kernel sees it now (whole disk: start 0).
pub(crate) fn kernel_extent(name: &str) -> Option<(u64, u64)> {
    let dir = Path::new(SYS_CLASS_BLOCK).join(name);
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).ok()?.trim().parse::<u64>().ok();
    let size = read("size")? * SYSFS_SECTOR;
    let start = if dir.join("partition").exists() { read("start")? * SYSFS_SECTOR } else { 0 };
    Some((start, size))
}

/// [`node`], waiting up to 3 s for the node to appear (right after a partition table was
/// re-read the kernel and udev may still be (re)creating partition nodes) and, when `expected`
/// is given (the start, and the length when it cannot legitimately have changed, that the last
/// enumeration reported for this volume), checking after `udevadm settle` that the node still
/// starts (and ends) there, so a stale volume id never reaches a different partition.
///
/// Residual race: the check and the tool that then opens the node are separate steps; a
/// partition table re-read by another program in between is not detected. Write sessions of
/// this program hold the disk's lock while they change the table, so only an outside actor can
/// cause it.
pub fn ready_node(id: &VolumeId, expected: Option<(u64, Option<u64>)>) -> Result<String> {
    let raw = raw_node(id)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while !Path::new(raw).exists() {
        if Instant::now() >= deadline {
            return Err(Error::NotFound(format!("{raw} does not exist")));
        }
        super::session::udev_settle();
        std::thread::sleep(Duration::from_millis(100));
    }
    let node = node(id)?;
    if let Some(want) = expected {
        let name = node.trim_start_matches("/dev/");
        let fits = |now: Option<(u64, u64)>| {
            now.is_some_and(|(start, len)| start == want.0 && want.1.map_or(true, |l| l == len))
        };
        let mut now = kernel_extent(name);
        if !fits(now) {
            super::session::udev_settle();
            now = kernel_extent(name);
        }
        if !fits(now) {
            return Err(Error::Invalid(format!(
                "{node} is no longer the partition at byte {} (length {:?}; the kernel now reports                  {now:?}): the partition table changed; refresh the disk list and retry",
                want.0, want.1
            )));
        }
    }
    Ok(node)
}

/// Mount volume `id` at folder `at` (`nosuid,nodev`): `mount(2)` with each kernel type that
/// fits the detected filesystem, then the `mount` command with the same types (FUSE helpers),
/// finally `mount` with automatic type detection.
pub fn mount(id: &VolumeId, at: &MountPoint, expected: Option<(u64, Option<u64>)>) -> Result<()> {
    let dir = folder(at)?;
    let dev = ready_node(id, expected)?;
    let dev = dev.as_str();
    let types = detect_fs(dev).map(mount_types).unwrap_or(&[]);
    let mut last: Option<Error> = None;
    for t in types {
        match ioctl::mount(dev, &dir, t, libc::MS_NOSUID | libc::MS_NODEV, None) {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                return Err(Error::Busy(format!("{dev} at {dir}: {e}")));
            }
            Err(e) => {
                log::debug!("mount -t {t} {dev} {dir}: {e}");
                last = Some(os_error(&e, &format!("mount {dev} at {dir} ({t})")));
            }
        }
    }
    for t in types {
        match cmd::run("mount", &["-t", t, "-o", "nosuid,nodev", dev, &dir]) {
            Ok(_) => return Ok(()),
            Err(e) => {
                log::debug!("mount command -t {t}: {e}");
                if !matches!(e, Error::Unsupported(_)) {
                    last = Some(e);
                }
            }
        }
    }
    if types.is_empty() {
        return cmd::run("mount", &["-o", "nosuid,nodev", dev, dir.as_str()]).map(|_| ());
    }
    Err(last.unwrap_or_else(|| Error::Unsupported(format!("no way to mount {dev}"))))
}

/// Unmount `at`, which must currently be a mount point of `id`.
pub fn unmount(id: &VolumeId, at: &MountPoint) -> Result<()> {
    let dir = folder(at)?;
    let dev = node(id)?;
    if !read_mounts().iter().any(|e| e.device == dev && e.mount_point == dir) {
        return Err(Error::NotFound(format!("{dev} is not mounted at {dir}")));
    }
    match ioctl::umount2(&dir, 0) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => Err(Error::Busy(format!("{dir}: {e}"))),
        Err(e) => {
            log::debug!("umount2 {dir}: {e}; trying the umount command");
            cmd::run("umount", &[dir.as_str()]).map(|_| ()).map_err(|ce| match ce {
                Error::Unsupported(_) => os_error(&e, &format!("unmount {dir}")),
                other => other,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_are_unsupported_and_ids_are_nodes() {
        assert!(matches!(folder(&MountPoint::Letter('E')), Err(Error::Unsupported(_))));
        assert!(folder(&MountPoint::Folder("/nonexistent/pm-x".into())).is_err());
        let t = tempfile::tempdir().unwrap();
        let f = folder(&MountPoint::Folder(t.path().into())).unwrap();
        assert!(f.starts_with('/'));
        assert!(matches!(node(&VolumeId(r"\\?\Volume{x}\".into())), Err(Error::NotFound(_))));
        assert!(node(&VolumeId("/dev/".into())).is_err());
        assert!(matches!(node(&VolumeId("/dev/pm-no-such-node".into())), Err(Error::NotFound(_))));
    }

    #[test]
    fn nodes_outside_dev_or_not_block_devices_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let img = t.path().join("x.img");
        std::fs::write(&img, [0u8; 512]).unwrap();
        // `/dev/../<tmp>/x.img` escapes /dev once resolved
        let escape = format!("/dev/..{}", img.display());
        assert!(matches!(node(&VolumeId(escape)), Err(Error::Invalid(m)) if m.contains("outside /dev")));
        // a character device
        assert!(matches!(node(&VolumeId("/dev/null".into())), Err(Error::Invalid(m)) if m.contains("not a block device")));
        // a RAM disk exists on many systems and is never managed
        if Path::new("/dev/ram0").exists() {
            assert!(matches!(node(&VolumeId("/dev/ram0".into())), Err(Error::Invalid(_))));
        }
    }
}
