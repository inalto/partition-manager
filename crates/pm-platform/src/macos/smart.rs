//! S.M.A.R.T. through `smartctl -j -a /dev/diskN` (smartmontools, e.g. `brew install
//! smartmontools`; macOS has no built-in equivalent).

use crate::posix::{cmd, smartctl};
use crate::traits::SmartData;
use pm_core::{Error, Result};

/// SMART data of `diskN`; `Ok(None)` when smartctl is not installed or reports nothing usable
/// (disk images, USB bridges without SAT pass-through).
pub fn smart(disk: u32) -> Result<Option<SmartData>> {
    let node = format!("/dev/disk{disk}");
    match cmd::output("smartctl", &["-j", "-a", node.as_str()]) {
        Ok(out) => Ok(smartctl::parse(&String::from_utf8_lossy(&out.stdout))),
        Err(Error::Unsupported(msg)) => {
            log::info!("{msg} (brew install smartmontools): no S.M.A.R.T. data");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
