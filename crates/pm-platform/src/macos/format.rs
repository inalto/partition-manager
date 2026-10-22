//! Formatting through `diskutil eraseVolume <personality> <name> diskNsM`.
//!
//! `diskutil` always does a quick format and picks the cluster size itself; it also sets the
//! partition type to match the new filesystem. NTFS needs third-party tools on macOS.
//!
//! Caution (unverified on a Mac): `eraseVolume` on an `Apple_APFS` physical store slice
//! deletes the whole APFS container, every volume in it included.

use super::diskutil::DiskutilRunner;
use super::guard::checked_slice;
use pm_core::{Error, FsKind, Result, VolumeId};

/// `diskutil` personality name for `fs`.
pub fn personality(fs: FsKind) -> Result<&'static str> {
    match fs {
        FsKind::Fat32 => Ok("FAT32"),
        FsKind::Fat16 => Ok("FAT16"),
        FsKind::Fat12 => Ok("FAT12"),
        FsKind::ExFat => Ok("ExFAT"),
        FsKind::HfsPlus => Ok("JHFS+"),
        FsKind::Apfs => Ok("APFS"),
        FsKind::Ntfs => Err(Error::Unsupported("NTFS formatting needs third-party tools on macOS".into())),
        other => Err(Error::Unsupported(format!("formatting {} is not supported on macOS", other.display_name()))),
    }
}

/// Erase `id` with a new `fs` named `label` (`UNTITLED` / `Untitled` when empty). `cluster`
/// and `quick` are ignored: `diskutil` chooses the cluster size and never does a full format.
pub fn format(
    runner: &dyn DiskutilRunner,
    id: &VolumeId,
    fs: FsKind,
    label: &str,
    cluster: Option<u32>,
    quick: bool,
) -> Result<()> {
    let personality = personality(fs)?;
    let slice = checked_slice(runner, id)?;
    if cluster.is_some() {
        log::info!("{slice}: diskutil picks the cluster size itself; the requested one is ignored");
    }
    if !quick {
        log::info!("{slice}: diskutil eraseVolume only does quick formats; full format ignored");
    }
    let name = match (label.trim(), fs) {
        ("", FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32) => "UNTITLED",
        ("", _) => "Untitled",
        (l, _) => l,
    };
    runner.run(&["eraseVolume", personality, name, &slice]).map(|_| ())
}
