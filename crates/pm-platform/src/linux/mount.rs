//! Mounting and unmounting partitions in folders (`mount(2)` / `umount2`, with the `mount` /
//! `umount` commands as fallback, e.g. for FUSE `ntfs-3g`).

use super::ioctl;
use super::mounts::{mount_types, read_mounts};
use crate::posix::{cmd, os_error, FdDevice};
use crate::traits::MountPoint;
use pm_core::{Error, FsKind, Result, VolumeId};
use std::path::Path;

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

/// The device node of a volume id (`/dev/...`).
pub fn node(id: &VolumeId) -> Result<&str> {
    if id.0.starts_with("/dev/") && id.0.len() > 5 {
        Ok(&id.0)
    } else {
        Err(Error::NotFound(format!("{} is not a Linux device node", id.0)))
    }
}

/// Mount volume `id` at folder `at`: `mount(2)` with each kernel type that fits the detected
/// filesystem, then the `mount` command with the same types (FUSE helpers), finally `mount`
/// with automatic type detection.
pub fn mount(id: &VolumeId, at: &MountPoint) -> Result<()> {
    let dev = node(id)?;
    let dir = folder(at)?;
    let types = detect_fs(dev).map(mount_types).unwrap_or(&[]);
    let mut last: Option<Error> = None;
    for t in types {
        match ioctl::mount(dev, &dir, t, 0, None) {
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
        match cmd::run("mount", &["-t", t, dev, &dir]) {
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
        return cmd::run("mount", &[dev, dir.as_str()]).map(|_| ());
    }
    Err(last.unwrap_or_else(|| Error::Unsupported(format!("no way to mount {dev}"))))
}

/// Unmount `at`, which must currently be a mount point of `id`.
pub fn unmount(id: &VolumeId, at: &MountPoint) -> Result<()> {
    let dev = node(id)?;
    let dir = folder(at)?;
    let canonical_dev = super::mounts::canonical_device(dev);
    if !read_mounts().iter().any(|e| e.device == canonical_dev && e.mount_point == dir) {
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
        assert_eq!(node(&VolumeId("/dev/sdb1".into())).unwrap(), "/dev/sdb1");
        assert!(matches!(node(&VolumeId(r"\\?\Volume{x}\".into())), Err(Error::NotFound(_))));
        assert!(node(&VolumeId("/dev/".into())).is_err());
    }
}
