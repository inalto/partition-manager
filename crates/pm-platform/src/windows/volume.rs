//! Volume enumeration (`FindFirstVolumeW` …) and volume path helpers.

use super::error::platform_error;
use super::ioctl;
use super::layout::{fs_from_name, parse_disk_extents};
use pm_core::{Result, VolumeId, VolumeInfo};

/// Volume path suitable for `CreateFileW` (no trailing backslash).
pub fn device_path(id: &VolumeId) -> String {
    id.0.trim_end_matches('\\').to_string()
}

/// Volume path with a trailing backslash (root form used by the volume-management APIs).
pub fn root_path(id: &VolumeId) -> String {
    let mut s = device_path(id);
    s.push('\\');
    s
}

/// `(disk, start, len)` of the volume's first extent, `disk = None` when it spans several
/// extents or the query fails (e.g. CD-ROM, network-backed volumes).
pub fn volume_extent(id: &VolumeId) -> (Option<u32>, u64, u64) {
    let extents = ioctl::open(&device_path(id), 0)
        .and_then(|h| ioctl::volume_extents(&h))
        .ok()
        .and_then(|b| parse_disk_extents(&b).ok())
        .unwrap_or_default();
    match extents.as_slice() {
        [] => (None, 0, 0),
        [(disk, start, len)] => (Some(*disk), *start, *len),
        [(_, start, len), ..] => (None, *start, *len),
    }
}

fn volume_info(id: VolumeId) -> VolumeInfo {
    let mount_points = ioctl::volume_path_names(&root_path(&id)).unwrap_or_else(|e| {
        log::debug!("volume {}: GetVolumePathNamesForVolumeNameW failed ({e})", id.0);
        Vec::new()
    });
    let (disk, start, len) = volume_extent(&id);
    // unformatted / RAW volumes fail here: no label, fs None
    let (label, fs) = match ioctl::volume_information(&root_path(&id)) {
        Ok((label, fs_name)) => (label, fs_from_name(&fs_name)),
        Err(_) => (String::new(), None),
    };
    let space_root = mount_points.first().cloned().unwrap_or_else(|| root_path(&id));
    let (total, free) = if fs.is_some() {
        ioctl::disk_free_space(&space_root).unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    VolumeInfo {
        id,
        disk,
        start,
        len,
        fs,
        label,
        mount_points,
        total,
        free,
    }
}

/// All volumes known to the volume manager.
pub fn enumerate_volumes() -> Result<Vec<VolumeInfo>> {
    let ids = ioctl::list_volumes().map_err(|c| platform_error(c, "enumerate volumes"))?;
    Ok(ids.into_iter().map(|s| volume_info(VolumeId(s))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_paths() {
        let id = VolumeId(r"\\?\Volume{1234}\".into());
        assert_eq!(device_path(&id), r"\\?\Volume{1234}");
        assert_eq!(root_path(&id), r"\\?\Volume{1234}\");
        let id = VolumeId(r"\\?\Volume{1234}".into());
        assert_eq!(root_path(&id), r"\\?\Volume{1234}\");
    }
}
