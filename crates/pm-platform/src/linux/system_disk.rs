//! Which disk holds the running system (`/`).

use super::mounts::MountEntry;
use pm_core::DiskInfo;

/// Index of the disk holding `/`, from the mounts (devices already canonical) and the
/// enumerated disks. Fails closed (`None`, so the planner refuses to touch any disk it cannot
/// rule out) when `/` is not a plain disk or partition node: device-mapper / LVM / LUKS
/// (`/dev/mapper/*`, `/dev/dm-*`), md RAID, `overlay`, `rootfs`, ZFS datasets, `/dev/root`.
pub fn system_disk_from(mounts: &[MountEntry], disks: &[DiskInfo]) -> Option<u32> {
    let root = mounts.iter().rev().find(|e| e.mount_point == "/")?;
    let dev = root.device.as_str();
    let not_a_disk = ["/dev/mapper/", "/dev/dm-", "/dev/md"];
    if !dev.starts_with("/dev/") || not_a_disk.iter().any(|p| dev.starts_with(p)) {
        log::info!("/ is on {dev} ({}): system disk unknown", root.fstype);
        return None;
    }
    disks
        .iter()
        .find(|d| d.path == dev || d.partitions.iter().any(|p| p.volume.as_ref().is_some_and(|v| v.0 == dev)))
        .map(|d| d.index)
}
