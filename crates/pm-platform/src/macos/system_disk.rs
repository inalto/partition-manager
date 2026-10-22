//! Which disk holds `/` (or any path), from `diskutil info -plist <path>`.
//!
//! An APFS volume lives on a synthesized container disk (`disk3s1s1` on `disk3`); the disk
//! that matters for partitioning is the one holding the container's physical store
//! (`APFSPhysicalStores`: `disk0s2`), so that wins over `ParentWholeDisk`.

use super::diskutil::{parse_identifier, parse_info, DeviceInfo, DiskutilRunner};
use std::path::Path;

/// `(disk, slice)` of the device behind `info`: the APFS physical store when there is one,
/// else the device itself.
fn physical_location(info: &DeviceInfo) -> Option<(u32, Option<u32>)> {
    match info.apfs_physical_stores.first() {
        Some(store) => parse_identifier(store),
        None => parse_identifier(&info.identifier).or_else(|| parse_identifier(&info.parent_whole_disk)),
    }
}

/// Index of the disk holding `/`; `None` when `diskutil` cannot tell (the planner then
/// refuses every disk it cannot rule out).
pub fn system_disk(runner: &dyn DiskutilRunner) -> Option<u32> {
    let bytes = runner.run(&["info", "-plist", "/"]).map_err(|e| log::warn!("diskutil info /: {e}")).ok()?;
    let info = parse_info(&bytes).map_err(|e| log::warn!("diskutil info /: {e}")).ok()?;
    physical_location(&info).map(|(disk, _)| disk)
}

/// Disk and slice holding `path`. `diskutil info` resolves mount points, so the path and then
/// each ancestor is tried until one answers (`/` always does). `path` should be absolute and
/// resolved (symlinks); the caller does that on a real system.
pub fn disk_of_path(runner: &dyn DiskutilRunner, path: &Path) -> Option<(u32, Option<u32>)> {
    path.ancestors().filter(|a| !a.as_os_str().is_empty()).find_map(|a| {
        let s = a.to_str()?;
        let bytes = runner.run(&["info", "-plist", s]).ok()?;
        let info = parse_info(&bytes).ok()?;
        physical_location(&info)
    })
}
