//! Mount / unmount through `diskutil` (macOS has no drive letters: folders only). The target
//! must be a slice of a physical disk ([`super::guard::checked_slice`]).

use super::diskutil::DiskutilRunner;
use super::guard::checked_slice;
use crate::traits::MountPoint;
use pm_core::{Error, Result, VolumeId};

/// `diskutil mount -mountPoint <dir> diskNsM` (the folder must exist).
pub fn mount(runner: &dyn DiskutilRunner, id: &VolumeId, at: &MountPoint) -> Result<()> {
    let dir = match at {
        MountPoint::Letter(_) => {
            return Err(Error::Unsupported("drive letters exist only on Windows: mount into a folder".into()))
        }
        MountPoint::Folder(dir) => {
            dir.to_str().ok_or_else(|| Error::Invalid(format!("{} is not UTF-8", dir.display())))?.to_string()
        }
    };
    let slice = checked_slice(runner, id)?;
    runner.run(&["mount", "-mountPoint", &dir, &slice]).map(|_| ())
}

/// `diskutil unmount diskNsM` (all mount points of the slice).
pub fn unmount(runner: &dyn DiskutilRunner, id: &VolumeId, _at: &MountPoint) -> Result<()> {
    let slice = checked_slice(runner, id)?;
    runner.run(&["unmount", &slice]).map(|_| ())
}
