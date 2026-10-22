//! Which disk holds `/` (or any path), from `diskutil info -plist <path>`.
//!
//! An APFS volume lives on a synthesized container disk (`disk3s1s1` on `disk3`); the disk
//! that matters for partitioning is the one holding the container's physical store
//! (`APFSPhysicalStores`: `disk0s2`), so that wins over `ParentWholeDisk`.
//!
//! [`system_disk`] fails closed (`None`, so the planner refuses every disk it cannot rule
//! out) when `/` spans several disks (a Fusion drive: stores on two disks) or sits on a
//! virtual volume without APFS stores (CoreStorage).

use super::diskutil::{parse_identifier, parse_info, DeviceInfo, DiskutilRunner};
use std::path::Path;

/// `(disk, slice)` of the device behind `info`: the APFS physical store when there is one,
/// else the device itself. `None` when the stores span several disks (Fusion).
fn physical_location(info: &DeviceInfo) -> Option<(u32, Option<u32>)> {
    if info.apfs_physical_stores.is_empty() {
        return parse_identifier(&info.identifier).or_else(|| parse_identifier(&info.parent_whole_disk));
    }
    let stores: Vec<(u32, Option<u32>)> = info.apfs_physical_stores.iter().filter_map(|s| parse_identifier(s)).collect();
    let first = *stores.first()?;
    if stores.iter().any(|(d, _)| *d != first.0) {
        log::info!("{} is on APFS stores {:?} (several disks)", info.identifier, info.apfs_physical_stores);
        return None;
    }
    Some(first)
}

/// Index of the disk holding `/`; `None` when `diskutil` cannot tell or `/` is not on exactly
/// one physical disk (see the module documentation).
pub fn system_disk(runner: &dyn DiskutilRunner) -> Option<u32> {
    let bytes = runner.run(&["info", "-plist", "/"]).map_err(|e| log::warn!("diskutil info /: {e}")).ok()?;
    let info = parse_info(&bytes).map_err(|e| log::warn!("diskutil info /: {e}")).ok()?;
    if info.apfs_physical_stores.is_empty() && info.virtual_or_physical.eq_ignore_ascii_case("Virtual") {
        log::info!("/ is on the virtual volume {} (CoreStorage?): system disk unknown", info.identifier);
        return None;
    }
    physical_location(&info).map(|(disk, _)| disk)
}

/// Disk and slice holding `path`. `diskutil info` resolves mount points, so the path and then
/// each ancestor is tried until one answers (`/` always does). `path` should be absolute and
/// resolved (symlinks); the caller does that on a real system.
pub fn disk_of_path(runner: &dyn DiskutilRunner, path: &Path) -> Option<(u32, Option<u32>)> {
    // the first ancestor diskutil knows decides, even when it resolves to nothing (Fusion)
    let info = path.ancestors().filter(|a| !a.as_os_str().is_empty()).find_map(|a| {
        let bytes = runner.run(&["info", "-plist", a.to_str()?]).ok()?;
        parse_info(&bytes).ok()
    })?;
    physical_location(&info)
}
