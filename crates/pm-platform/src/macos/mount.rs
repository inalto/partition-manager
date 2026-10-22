//! Mount / unmount through `diskutil` (macOS has no drive letters: folders only). The target
//! must be a slice of a physical disk ([`super::guard::checked_slice`]).

use super::diskutil::DiskutilRunner;
use super::guard::{checked_slice, checked_slice_info};
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

/// `dir` without trailing slashes (`/` stays `/`), resolved when it exists.
fn normalised(dir: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    let trimmed = if trimmed.is_empty() && dir.starts_with('/') { "/" } else { trimmed };
    match std::fs::canonicalize(trimmed) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => trimmed.to_string(),
    }
}

/// `diskutil unmount diskNsM`, after checking that the slice is mounted at `at` (unmounting
/// is by slice, so a wrong folder would unmount the volume wherever it really is).
pub fn unmount(runner: &dyn DiskutilRunner, id: &VolumeId, at: &MountPoint) -> Result<()> {
    let (slice, info) = checked_slice_info(runner, id)?;
    let dir = match at {
        MountPoint::Letter(_) => {
            return Err(Error::Unsupported("drive letters exist only on Windows: unmount a folder".into()))
        }
        MountPoint::Folder(dir) => {
            dir.to_str().ok_or_else(|| Error::Invalid(format!("{} is not UTF-8", dir.display())))?.to_string()
        }
    };
    let Some(real) = info.mount_point.as_deref() else {
        return Err(Error::NotFound(format!("{slice} is not mounted")));
    };
    if normalised(&dir) != normalised(real) {
        return Err(Error::NotFound(format!("{slice} is not mounted at {dir} (it is mounted at {real})")));
    }
    runner.run(&["unmount", &slice]).map(|_| ())
}
