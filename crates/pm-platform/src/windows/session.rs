//! Exclusive write sessions: lock + dismount the disk's volumes, take the disk offline.

use super::device::WinDevice;
use super::disk::disk_path;
use super::error::platform_error;
use super::ioctl::{self, Handle, GENERIC_READ, GENERIC_WRITE};
use super::layout::parse_disk_extents;
use super::volume::{device_path, root_path};
use crate::traits::{OpenMode, WriteSession};
use pm_core::{BlockDevice, Error, Result, VolumeId};
use pm_parttable::{write_table, Table};
use std::time::Duration;
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_DATA, ERROR_INVALID_FUNCTION, ERROR_NOT_READY, ERROR_NOT_SUPPORTED,
};
use windows_sys::Win32::System::Ioctl::DISK_ATTRIBUTE_OFFLINE;

const LOCK_ATTEMPTS: u32 = 5;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(200);

/// A locked + dismounted volume.
struct LockedVolume {
    handle: Handle,
    name: String,
}

/// Write session on one disk; see [`WriteSession`]. Dropping it without `finish` still
/// restores the disk (online if this session took it offline, unlocked), ignoring errors.
pub struct WindowsWriteSession {
    disk: u32,
    device: WinDevice,
    locked: Vec<LockedVolume>,
    /// This session took the disk offline and must bring it back online.
    offlined: bool,
    finished: bool,
}

/// What `begin_write` does with one volume of the system.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VolumeAction {
    /// Has at least one extent on the target disk: lock + dismount.
    Lock,
    /// Lives entirely on other disks, or is not disk-backed at all.
    Skip,
    /// Its extents cannot be determined: refuse the session (`Error::Busy`).
    Refuse(String),
}

/// Decide what to do with volume `name` given its extent query result.
///
/// A volume with ANY extent on `disk` is locked (spanned volumes included). Failure codes
/// meaning "this volume is not backed by a disk volume manager" (`ERROR_INVALID_FUNCTION` /
/// `ERROR_NOT_SUPPORTED`: CD/DVD and similar class drivers) or "no media"
/// (`ERROR_NOT_READY`: nothing mounted that a raw write could corrupt) are skipped; any other
/// failure refuses the session rather than writing under a possibly mounted filesystem.
pub(crate) fn volume_action(
    extents: std::result::Result<Vec<(u32, u64, u64)>, u32>,
    disk: u32,
    name: &str,
) -> VolumeAction {
    match extents {
        Ok(e) if e.iter().any(|&(d, _, _)| d == disk) => VolumeAction::Lock,
        Ok(_) => VolumeAction::Skip,
        Err(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_NOT_READY) => VolumeAction::Skip,
        Err(code) => VolumeAction::Refuse(format!(
            "cannot determine extents of {name} ({})",
            super::friendly_error(code)
        )),
    }
}

/// Whether the session should take the disk offline (and later bring it back online):
/// never for the system disk, only when the disk is known to be online now, so a disk that was
/// already offline (user, SAN policy, Hyper-V pass-through) is left offline afterwards.
pub(crate) fn should_take_offline(is_system_disk: bool, attributes: Option<u64>) -> bool {
    match attributes {
        Some(a) => !is_system_disk && a & DISK_ATTRIBUTE_OFFLINE == 0,
        None => false,
    }
}

fn describe(id: &VolumeId, mount_points: &[String]) -> String {
    if mount_points.is_empty() {
        id.0.clone()
    } else {
        mount_points.join(", ")
    }
}

fn release(locked: &mut Vec<LockedVolume>) {
    while let Some(v) = locked.pop() {
        if let Err(c) = ioctl::unlock_volume(&v.handle) {
            // expected when the volume was torn down by taking the disk offline
            log::debug!("unlock {}: error {c}", v.name);
        }
        drop(v.handle);
    }
}

fn lock_with_retry(h: &Handle) -> std::result::Result<(), u32> {
    let mut last = 0;
    for attempt in 0..LOCK_ATTEMPTS {
        match ioctl::lock_volume(h) {
            Ok(()) => return Ok(()),
            Err(c) => last = c,
        }
        if attempt + 1 < LOCK_ATTEMPTS {
            std::thread::sleep(LOCK_RETRY_DELAY);
        }
    }
    Err(last)
}

/// Extents of a volume, queried directly (query-only handle).
fn query_extents(id: &VolumeId) -> std::result::Result<Vec<(u32, u64, u64)>, u32> {
    let h = ioctl::open(&device_path(id), 0)?;
    let buf = ioctl::volume_extents(&h)?;
    parse_disk_extents(&buf).map_err(|_| ERROR_INVALID_DATA)
}

/// Lock + dismount one volume, pushing it onto `locked`.
fn lock_volume(id: &VolumeId, name: String, locked: &mut Vec<LockedVolume>) -> Result<()> {
    let handle = ioctl::open(&device_path(id), GENERIC_READ | GENERIC_WRITE)
        .map_err(|c| platform_error(c, &format!("open volume {name}")))?;
    if let Err(c) = lock_with_retry(&handle) {
        log::warn!("lock {name} failed: error {c}");
        return Err(Error::Busy(name));
    }
    if let Err(c) = ioctl::dismount_volume(&handle) {
        let _ = ioctl::unlock_volume(&handle);
        return Err(platform_error(c, &format!("dismount {name}")));
    }
    locked.push(LockedVolume { handle, name });
    Ok(())
}

/// Lock and dismount every volume with an extent on `disk`, open the disk read-write and take
/// it offline if it is online and not the system disk.
pub fn begin_write(disk: u32, system_disk: Option<u32>) -> Result<Box<dyn WriteSession>> {
    let ids = ioctl::list_volumes().map_err(|c| platform_error(c, "enumerate volumes"))?;
    let mut locked: Vec<LockedVolume> = Vec::new();
    for id in ids.into_iter().map(VolumeId) {
        let mount_points = ioctl::volume_path_names(&root_path(&id)).unwrap_or_default();
        let name = describe(&id, &mount_points);
        let step = match volume_action(query_extents(&id), disk, &name) {
            VolumeAction::Skip => Ok(()),
            VolumeAction::Refuse(msg) => Err(Error::Busy(msg)),
            VolumeAction::Lock => lock_volume(&id, name, &mut locked),
        };
        if let Err(e) = step {
            release(&mut locked);
            return Err(e);
        }
    }

    let device = match WinDevice::open(&disk_path(disk), OpenMode::ReadWrite) {
        Ok(d) => d,
        Err(e) => {
            release(&mut locked);
            return Err(e);
        }
    };
    let attributes = ioctl::disk_attributes(device.handle())
        .map_err(|c| log::warn!("disk {disk}: cannot read attributes ({c}); leaving it online"))
        .ok();
    let mut offlined = false;
    if should_take_offline(system_disk == Some(disk), attributes) {
        match ioctl::set_disk_offline(device.handle(), true, false) {
            Ok(()) => offlined = true,
            // the volumes are locked and dismounted, which already gives exclusive access
            Err(c) => log::warn!("disk {disk}: could not take offline ({})", super::friendly_error(c)),
        }
    }
    Ok(Box::new(WindowsWriteSession {
        disk,
        device,
        locked,
        offlined,
        finished: false,
    }))
}

impl WindowsWriteSession {
    /// Online, re-read properties, unlock and close volumes (reverse order of `begin_write`).
    /// Every step runs even if an earlier one fails; the first error is returned.
    fn restore(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let mut first: Option<Error> = None;
        let _ = self.device.flush();
        if self.offlined {
            if let Err(c) = ioctl::set_disk_offline(self.device.handle(), false, false) {
                first.get_or_insert(platform_error(c, &format!("bring disk {} online", self.disk)));
            }
            self.offlined = false;
        }
        if let Err(c) = ioctl::update_properties(self.device.handle()) {
            first.get_or_insert(platform_error(c, &format!("rescan disk {}", self.disk)));
        }
        release(&mut self.locked);
        first.map_or(Ok(()), Err)
    }
}

impl WriteSession for WindowsWriteSession {
    fn disk(&self) -> u32 {
        self.disk
    }

    fn device(&mut self) -> &mut dyn BlockDevice {
        &mut self.device
    }

    fn commit_table(&mut self, t: &Table) -> Result<()> {
        write_table(&mut self.device, t)?;
        self.device.flush()?;
        ioctl::update_properties(self.device.handle())
            .map_err(|c| platform_error(c, &format!("rescan disk {}", self.disk)))
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.restore()
    }
}

impl Drop for WindowsWriteSession {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            log::warn!("disk {}: restoring after write session failed: {e}", self.disk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};

    const MIB: u64 = 1 << 20;

    #[test]
    fn volumes_with_any_extent_on_the_disk_are_locked() {
        assert_eq!(volume_action(Ok(vec![(2, MIB, MIB)]), 2, "E:"), VolumeAction::Lock);
        // spanned volume: second extent on the target disk
        assert_eq!(
            volume_action(Ok(vec![(0, MIB, MIB), (2, MIB, MIB)]), 2, "S:"),
            VolumeAction::Lock
        );
        assert_eq!(volume_action(Ok(vec![(0, MIB, MIB), (1, 0, MIB)]), 2, "C:"), VolumeAction::Skip);
        assert_eq!(volume_action(Ok(vec![]), 2, "X:"), VolumeAction::Skip);
    }

    #[test]
    fn unknown_extents_refuse_the_session() {
        for code in [ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_DATA] {
            match volume_action(Err(code), 2, "F:") {
                VolumeAction::Refuse(msg) => {
                    assert!(msg.starts_with("cannot determine extents of F:"), "{msg}")
                }
                other => panic!("{code}: {other:?}"),
            }
        }
        // not disk-backed (CD/DVD) or no media
        for code in [ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, ERROR_NOT_READY] {
            assert_eq!(volume_action(Err(code), 2, "R:"), VolumeAction::Skip);
        }
    }

    #[test]
    fn offline_only_when_online_and_not_system() {
        assert!(should_take_offline(false, Some(0)));
        // already offline: leave it alone, so finish/drop will not bring it online
        assert!(!should_take_offline(false, Some(DISK_ATTRIBUTE_OFFLINE)));
        assert!(!should_take_offline(false, Some(DISK_ATTRIBUTE_OFFLINE | 2)));
        assert!(should_take_offline(false, Some(2))); // read-only but online
        assert!(!should_take_offline(true, Some(0)));
        // unknown state: do not touch it
        assert!(!should_take_offline(false, None));
    }
}
