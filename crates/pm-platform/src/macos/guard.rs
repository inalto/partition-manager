//! Checks run before anything that changes a disk or a slice: the target must be a slice of a
//! physical whole disk (never a whole disk taken as a volume, never a synthesized APFS
//! container or one of its volumes).

use super::diskutil::{parse_identifier, parse_info, volume_identifier, DeviceInfo, DiskutilRunner};
use pm_core::{Error, Result, VolumeId};

/// `diskutil info -plist <ident>` of a physical whole disk; `Error::Invalid` for a slice, a
/// synthesized APFS container disk or anything else that is not a physical whole disk.
pub fn physical_whole_disk(runner: &dyn DiskutilRunner, ident: &str) -> Result<DeviceInfo> {
    let info = parse_info(&runner.run(&["info", "-plist", ident])?)?;
    if !info.whole_disk || !matches!(parse_identifier(&info.identifier), Some((_, None))) {
        return Err(Error::Invalid(format!("{ident} is not a whole disk")));
    }
    if info.is_apfs_synthesized() {
        return Err(Error::Invalid(format!(
            "{ident} is a synthesized APFS container: partition its physical store disk instead"
        )));
    }
    Ok(info)
}

/// The `diskNsM` identifier of `id` after checking that it is a slice of a physical whole
/// disk. Whole disks (`/dev/disk2`) and non-disk ids are `Error::NotFound`; volumes of a
/// synthesized APFS container (`disk1s1`) are `Error::Invalid`.
pub fn checked_slice(runner: &dyn DiskutilRunner, id: &VolumeId) -> Result<String> {
    checked_slice_info(runner, id).map(|(slice, _)| slice)
}

/// [`checked_slice`], also returning the slice's `diskutil info`.
pub fn checked_slice_info(runner: &dyn DiskutilRunner, id: &VolumeId) -> Result<(String, DeviceInfo)> {
    let slice = volume_identifier(&id.0)
        .ok_or_else(|| Error::NotFound(format!("{} is not a slice of a macOS disk (diskNsM)", id.0)))?;
    let Some((disk, Some(_))) = parse_identifier(&slice) else {
        return Err(Error::NotFound(format!("{} is not a slice of a macOS disk", id.0)));
    };
    let info = parse_info(&runner.run(&["info", "-plist", &slice])?)?;
    if info.whole_disk || info.is_apfs_synthesized() {
        return Err(Error::Invalid(format!("{slice} is not a partition of a physical disk")));
    }
    let parent = if info.parent_whole_disk.is_empty() { format!("disk{disk}") } else { info.parent_whole_disk.clone() };
    if parse_identifier(&parent) != Some((disk, None)) {
        return Err(Error::Invalid(format!("{slice} belongs to {parent}, not disk{disk}")));
    }
    physical_whole_disk(runner, &parent).map_err(|e| match e {
        Error::Invalid(m) => Error::Invalid(format!("{slice}: {m}")),
        other => other,
    })?;
    Ok((slice, info))
}
