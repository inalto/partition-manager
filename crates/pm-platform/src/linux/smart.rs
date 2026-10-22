//! S.M.A.R.T. through `smartctl -j -a /dev/<disk>` (smartmontools).

use crate::posix::{cmd, smartctl};
use crate::traits::SmartData;
use pm_core::{Error, Result};

/// SMART data of the disk node `path`; `Ok(None)` when smartctl is not installed or reports
/// nothing usable (loop devices, USB bridges without SAT pass-through, no permission).
pub fn smart(path: &str) -> Result<Option<SmartData>> {
    match cmd::output("smartctl", &["-j", "-a", path]) {
        Ok(out) => Ok(smartctl::parse(&String::from_utf8_lossy(&out.stdout))),
        Err(Error::Unsupported(msg)) => {
            log::info!("{msg}: no S.M.A.R.T. data");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
