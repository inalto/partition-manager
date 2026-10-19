//! Exclusive write sessions: lock + dismount the disk's volumes, take the disk offline.

use super::device::WinDevice;
use super::disk::disk_path;
use super::error::platform_error;
use super::ioctl::{self, Handle, GENERIC_READ, GENERIC_WRITE};
use super::volume::{device_path, enumerate_volumes};
use crate::traits::{OpenMode, WriteSession};
use pm_core::{BlockDevice, Error, Result, VolumeInfo};
use pm_parttable::{write_table, Table};
use std::time::Duration;

const LOCK_ATTEMPTS: u32 = 5;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(200);

/// A locked + dismounted volume.
struct LockedVolume {
    handle: Handle,
    name: String,
}

/// Write session on one disk; see [`WriteSession`]. Dropping it without `finish` still
/// restores the disk (online, unlocked), ignoring errors.
pub struct WindowsWriteSession {
    disk: u32,
    device: WinDevice,
    locked: Vec<LockedVolume>,
    offlined: bool,
    finished: bool,
}

fn describe(v: &VolumeInfo) -> String {
    if v.mount_points.is_empty() {
        v.id.0.clone()
    } else {
        v.mount_points.join(", ")
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

/// Lock and dismount every volume on `disk`, take it offline unless it is the system disk,
/// and open it read-write.
pub fn begin_write(disk: u32, system_disk: Option<u32>) -> Result<Box<dyn WriteSession>> {
    let volumes: Vec<VolumeInfo> = enumerate_volumes()?
        .into_iter()
        .filter(|v| v.disk == Some(disk))
        .collect();
    let mut locked: Vec<LockedVolume> = Vec::new();
    for v in &volumes {
        let path = device_path(&v.id);
        let handle = match ioctl::open(&path, GENERIC_READ | GENERIC_WRITE) {
            Ok(h) => h,
            Err(c) => {
                release(&mut locked);
                return Err(platform_error(c, &format!("open volume {}", describe(v))));
            }
        };
        if let Err(c) = lock_with_retry(&handle) {
            release(&mut locked);
            log::warn!("lock {} failed: error {c}", describe(v));
            return Err(Error::Busy(describe(v)));
        }
        if let Err(c) = ioctl::dismount_volume(&handle) {
            let _ = ioctl::unlock_volume(&handle);
            release(&mut locked);
            return Err(platform_error(c, &format!("dismount {}", describe(v))));
        }
        locked.push(LockedVolume {
            handle,
            name: describe(v),
        });
    }

    let device = match WinDevice::open(&disk_path(disk), OpenMode::ReadWrite) {
        Ok(d) => d,
        Err(e) => {
            release(&mut locked);
            return Err(e);
        }
    };
    let mut offlined = false;
    if system_disk != Some(disk) {
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
