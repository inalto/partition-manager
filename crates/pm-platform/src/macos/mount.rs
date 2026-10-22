//! Mount / unmount through `diskutil` (macOS has no drive letters: folders only).

use super::diskutil::{volume_identifier, DiskutilRunner};
use crate::traits::MountPoint;
use pm_core::{Error, Result, VolumeId};

/// The `diskNsM` identifier of a volume id, or `Error::NotFound`.
pub fn slice_of(id: &VolumeId) -> Result<String> {
    volume_identifier(&id.0).ok_or_else(|| Error::NotFound(format!("{} is not a macOS disk slice", id.0)))
}

/// `diskutil mount -mountPoint <dir> diskNsM` (the folder must exist).
pub fn mount(runner: &dyn DiskutilRunner, id: &VolumeId, at: &MountPoint) -> Result<()> {
    let slice = slice_of(id)?;
    match at {
        MountPoint::Letter(_) => Err(Error::Unsupported(
            "drive letters exist only on Windows: mount into a folder".into(),
        )),
        MountPoint::Folder(dir) => {
            let dir = dir.to_str().ok_or_else(|| Error::Invalid(format!("{} is not UTF-8", dir.display())))?;
            runner.run(&["mount", "-mountPoint", dir, &slice]).map(|_| ())
        }
    }
}

/// `diskutil unmount diskNsM` (all mount points of the slice).
pub fn unmount(runner: &dyn DiskutilRunner, id: &VolumeId, _at: &MountPoint) -> Result<()> {
    let slice = slice_of(id)?;
    runner.run(&["unmount", &slice]).map(|_| ())
}
