//! Drive letters / folder mount points, disk online state and rescans.

use super::disk::disk_path;
use super::error::platform_error;
use super::ioctl::{self, GENERIC_READ, GENERIC_WRITE};
use super::volume::root_path;
use crate::traits::MountPoint;
use pm_core::{Error, Result, VolumeId};

pub fn mount(id: &VolumeId, at: &MountPoint) -> Result<()> {
    let mp = at.to_path_string();
    ioctl::set_mount_point(&mp, &root_path(id))
        .map_err(|c| platform_error(c, &format!("mount {} at {mp}", id.0)))
}

/// Remove mount point `at`, which must currently belong to volume `id`.
pub fn unmount(id: &VolumeId, at: &MountPoint) -> Result<()> {
    let mp = at.to_path_string();
    let current = ioctl::volume_path_names(&root_path(id))
        .map_err(|c| platform_error(c, &format!("query mount points of {}", id.0)))?;
    if !current.iter().any(|m| m.eq_ignore_ascii_case(&mp)) {
        return Err(Error::NotFound(format!("{} is not mounted at {mp}", id.0)));
    }
    ioctl::delete_mount_point(&mp).map_err(|c| platform_error(c, &format!("unmount {mp}")))
}

/// Letters `C`..=`Z` not in the `GetLogicalDrives` mask (A and B are never offered).
pub fn letters_free_in_mask(mask: u32) -> Vec<char> {
    (b'C'..=b'Z')
        .filter(|&c| mask & (1 << (c - b'A')) == 0)
        .map(char::from)
        .collect()
}

pub fn free_letters() -> Vec<char> {
    letters_free_in_mask(ioctl::logical_drives())
}

/// Persistently take a disk offline / bring it online, then re-read its properties.
pub fn set_offline(disk: u32, offline: bool) -> Result<()> {
    let path = disk_path(disk);
    let h = ioctl::open(&path, GENERIC_READ | GENERIC_WRITE)
        .map_err(|c| platform_error(c, &format!("open {path}")))?;
    ioctl::set_disk_offline(&h, offline, true).map_err(|c| {
        platform_error(c, &format!("set disk {disk} {}", if offline { "offline" } else { "online" }))
    })?;
    if let Err(c) = ioctl::update_properties(&h) {
        log::debug!("disk {disk}: update properties failed ({c})");
    }
    Ok(())
}

/// `IOCTL_DISK_UPDATE_PROPERTIES`: make Windows re-read the disk's partition table.
pub fn rescan(disk: u32) -> Result<()> {
    let path = disk_path(disk);
    let h = ioctl::open(&path, GENERIC_READ).map_err(|c| platform_error(c, &format!("open {path}")))?;
    ioctl::update_properties(&h).map_err(|c| platform_error(c, &format!("rescan disk {disk}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_letters_from_mask() {
        // A, B, C, D, Z in use
        let mask = 0b1111 | (1 << 25);
        let free = letters_free_in_mask(mask);
        assert_eq!(free.first(), Some(&'E'));
        assert_eq!(free.last(), Some(&'Y'));
        assert_eq!(free.len(), 26 - 5);
        // A and B never offered even when free
        assert_eq!(letters_free_in_mask(0).first(), Some(&'C'));
        assert_eq!(letters_free_in_mask(u32::MAX), Vec::<char>::new());
    }
}
