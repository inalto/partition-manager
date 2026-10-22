//! Exclusive write access to a disk: `diskutil unmountDisk diskN`, then `/dev/rdiskN` opened
//! read-write.
//!
//! The kernel re-reads the partition map when the raw node is closed after writing. Volumes
//! are NOT mounted again by [`WriteSession::finish`]: the user decides (Disk Arbitration may
//! auto-mount new volumes on its own).

use super::device::{open_raw, MacDevice};
use super::diskutil::DiskutilRunner;
use super::ioctl;
use crate::traits::{OpenMode, WriteSession};
use pm_core::{BlockDevice, Error, Result};
use pm_parttable::{write_table, Table};

pub struct MacWriteSession {
    disk: u32,
    device: Option<MacDevice>,
}

/// Unmount every volume of `diskN` (failure → `Error::Busy` with diskutil's message) and open
/// `/dev/rdiskN` read-write.
pub fn begin_write(runner: &dyn DiskutilRunner, disk: u32) -> Result<Box<dyn WriteSession>> {
    let ident = format!("disk{disk}");
    runner.run(&["unmountDisk", &ident]).map_err(|e| match e {
        Error::Platform { msg, .. } => Error::Busy(msg),
        other => other,
    })?;
    let device = open_raw(&format!("/dev/rdisk{disk}"), OpenMode::ReadWrite)?;
    Ok(Box::new(MacWriteSession { disk, device: Some(device) }))
}

impl MacWriteSession {
    fn dev(&mut self) -> &mut MacDevice {
        self.device.as_mut().expect("device open until finish")
    }

    /// `fsync` plus a drive cache flush (the latter best effort).
    fn sync(&mut self) -> Result<()> {
        let dev = self.dev();
        dev.flush()?;
        if let Err(e) = ioctl::synchronize_cache(dev.inner().fd()) {
            log::warn!("{}: DKIOCSYNCHRONIZECACHE failed: {e}", dev.inner().path());
        }
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
        }
    }
}
