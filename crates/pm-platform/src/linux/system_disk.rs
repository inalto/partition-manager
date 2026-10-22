//! Which disk holds the running system (`/`).

use super::mounts::MountEntry;
use pm_core::DiskInfo;
use std::path::Path;

/// Appended to the log line when the system disk stays unknown.
const UNKNOWN_HINT: &str = "root filesystem on a device spanning several disks, or on an unsupported stack";

/// The kernel name of `/dev/mapper/<name>`: the `<class>/dm-N` whose `dm/name` is `<name>`
/// (`/dev/mapper` entries are symlinks to `/dev/dm-N`; the mounts may show either form).
fn mapper_to_dm(class: &Path, mapper: &str) -> Option<String> {
    if let Ok(p) = std::fs::canonicalize(format!("/dev/mapper/{mapper}")) {
        if let Some(n) = p.file_name() {
            let n = n.to_string_lossy().into_owned();
            if class.join(&n).exists() {
                return Some(n);
            }
        }
    }
    std::fs::read_dir(class).ok()?.filter_map(|e| e.ok()).find_map(|e| {
        let name = std::fs::read_to_string(e.path().join("dm/name")).ok()?;
        (name.trim() == mapper).then(|| e.file_name().to_string_lossy().into_owned())
    })
}

/// Index of the disk holding `/`, from the mounts (devices already canonical) and the
/// enumerated disks, seeing through device-mapper / LVM / LUKS and md RAID stacks
/// (`/sys/class/block/<dev>/slaves`, followed recursively).
pub fn system_disk_from(mounts: &[MountEntry], disks: &[DiskInfo]) -> Option<u32> {
    system_disk_with(mounts, disks, Path::new("/sys/class/block"))
}

/// [`system_disk_from`] against the given `/sys/class/block` directory.
///
/// `/` on a plain disk or partition gives that disk. `/` on a dm / md device gives the disk all
/// of the stack's leaf devices are on (the usual LVM-on-one-disk install). It fails closed
/// (`None`, so the planner refuses to touch any disk it cannot rule out) when the leaves span
/// several disks (an LV over two disks, an md mirror) or are unknown, and for `overlay`,
/// `rootfs`, ZFS datasets and anything else that is not a block device node.
pub fn system_disk_with(mounts: &[MountEntry], disks: &[DiskInfo], class: &Path) -> Option<u32> {
    let root = mounts.iter().rev().find(|e| e.mount_point == "/")?;
    let dev = root.device.as_str();
    let unknown = || {
        log::warn!("/ is on {dev} ({}): system disk unknown ({UNKNOWN_HINT})", root.fstype);
        None
    };
    let Some(rest) = dev.strip_prefix("/dev/").filter(|r| !r.is_empty()) else { return unknown() };
    let node = match rest.strip_prefix("mapper/") {
        Some(mapper) => match mapper_to_dm(class, mapper) {
            Some(dm) => format!("/dev/{dm}"),
            None => return unknown(),
        },
        None => dev.to_string(),
    };
    match super::locate_stacked(disks, class, &node) {
        Some((index, _)) => Some(index),
        None => unknown(),
    }
}
