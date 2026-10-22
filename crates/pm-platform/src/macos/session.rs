//! Exclusive write access to a disk: the checks, `diskutil unmountDisk diskN` and the
//! remount-on-failure of [`super::write::prepare_write`], then `/dev/rdiskN` opened read-write.
//!
//! The kernel re-reads the partition map when the raw node is closed after writing. Volumes
//! are NOT mounted again by [`WriteSession::finish`]: the user decides (Disk Arbitration may
//! auto-mount new volumes on its own).

use super::device::{open_raw, MacDevice};
use super::diskutil::DiskutilRunner;
use super::ioctl;
use super::write::prepare_write;
use crate::posix;
use crate::traits::{OpenMode, WriteSession};
use pm_core::{BlockDevice, Result};
use pm_parttable::{write_table, Table};

pub struct MacWriteSession {
    disk: u32,
    device: Option<MacDevice>,
}

/// Take `diskN` for writing (root only, physical whole disks only, volumes unmounted and
/// mounted again on failure) and open `/dev/rdiskN` read-write.
pub fn begin_write(runner: &dyn DiskutilRunner, disk: u32) -> Result<Box<dyn WriteSession>> {
    let device = prepare_write(runner, disk, posix::is_root(), |path| open_raw(path, OpenMode::ReadWrite))?;
    Ok(Box::new(MacWriteSession { disk, device: Some(device) }))
}

/// Ask the drive behind `dev` to flush its write cache (best effort, logged).
fn synchronize_cache(dev: &MacDevice) {
    if let Err(e) = ioctl::synchronize_cache(dev.inner().fd()) {
        log::warn!("{}: DKIOCSYNCHRONIZECACHE failed: {e}", dev.inner().path());
    }
}

impl MacWriteSession {
    fn dev(&mut self) -> &mut MacDevice {
        self.device.as_mut().expect("device open until finish")
    }

    /// `fsync` plus a drive cache flush (the latter best effort).
    fn sync(&mut self) -> Result<()> {
        let dev = self.dev();
        dev.flush()?;
        synchronize_cache(dev);
        Ok(())
    }
}

impl WriteSession for MacWriteSession {
    fn disk(&self) -> u32 {
        self.disk
    }
    fn device(&mut self) -> &mut dyn BlockDevice {
        self.dev()
    }
    fn commit_table(&mut self, t: &Table) -> Result<()> {
        write_table(self.dev(), t)?;
        self.sync()
    }
    fn finish(mut self: Box<Self>) -> Result<()> {
        let r = self.sync();
        // closing the raw node makes the kernel re-read the partition map
        self.device = None;
        r
    }
}

impl Drop for MacWriteSession {
    fn drop(&mut self) {
        if let Some(mut dev) = self.device.take() {
            if let Err(e) = dev.flush() {
                log::warn!("disk {}: flush on drop failed: {e}", self.disk);
            }
            synchronize_cache(&dev);
        }
    }
}
