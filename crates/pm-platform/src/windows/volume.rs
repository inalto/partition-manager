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

/// `path` in the comparable form of a volume mount point: backslashes, upper case, trailing
/// backslash, without the `\\?\` prefix.
fn mount_key(path: &str) -> String {
    let p = path.strip_prefix(r"\\?\").unwrap_or(path);
    let mut s = p.replace('/', "\\").to_uppercase();
    if !s.ends_with('\\') {
        s.push('\\');
    }
    s
}

/// The volume whose mount point (`C:\`, `C:\mnt\x\`) is the longest prefix of the absolute
/// path `path` (whole components only, case-insensitive).
pub fn volume_of_path<'a>(path: &str, volumes: &'a [VolumeInfo]) -> Option<&'a VolumeInfo> {
    let key = mount_key(path);
    volumes
        .iter()
        .flat_map(|v| v.mount_points.iter().map(move |m| (mount_key(m), v)))
        .filter(|(m, _)| key.starts_with(m.as_str()))
        .max_by_key(|(m, _)| m.len())
        .map(|(_, v)| v)
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

    #[test]
    fn path_to_volume_by_longest_mount_point() {
        let vol = |id: &str, mps: &[&str]| VolumeInfo {
            id: VolumeId(id.into()),
            mount_points: mps.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let vols = [vol("c", &[r"C:\"]), vol("m", &[r"C:\mnt\data\"]), vol("e", &[r"E:\", r"C:\mnt\e\"])];
        let of = |p: &str| volume_of_path(p, &vols).map(|v| v.id.0.as_str());
        assert_eq!(of(r"C:\Users\x\img.bin"), Some("c"));
        assert_eq!(of(r"\\?\C:\mnt\data\sub\f"), Some("m"));
        assert_eq!(of(r"c:/MNT/DATA"), Some("m"), "case and separators do not matter");
        assert_eq!(of(r"C:\mnt\database\f"), Some("c"), "whole components only");
        assert_eq!(of(r"e:\x"), Some("e"));
        assert_eq!(of(r"C:\mnt\e\x"), Some("e"));
        assert_eq!(of(r"F:\x"), None);
        assert_eq!(of(r"\\?\UNC\server\share\x"), None);
    }
}
