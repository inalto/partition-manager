//! Volume-level operations through the platform: format, drive letters, mount points.

use super::{locate_volume, volume_of, Ctx};
use crate::model::mount_letter;
use pm_core::{Error, FsKind, Result, VolumeId};
use pm_platform::MountPoint;

/// Format the volume that appears for the new partition at `start` of `disk` (quick format,
/// default cluster size). Runs after the table session is finished and the layout re-read.
pub(crate) fn format_new(
    ctx: &mut Ctx,
    disk: u32,
    start: u64,
    fs: FsKind,
    label: &str,
) -> Result<()> {
    let vol = locate_volume(ctx.platform, disk, start, ctx.cancel)?;
    let sink = ctx.step(&format!("format as {}", fs.display_name()));
    ctx.platform.format(&vol, fs, label, None, true, &sink)
}

/// Mount the volume of the new partition at `start` of `disk` on drive letter `c`.
pub(crate) fn assign_letter(ctx: &mut Ctx, disk: u32, start: u64, c: char) -> Result<()> {
    let vol = locate_volume(ctx.platform, disk, start, ctx.cancel)?;
    let c = c.to_ascii_uppercase();
    ctx.step(&format!("assign drive letter {c}:"));
    ctx.platform.mount(&vol, &MountPoint::Letter(c))
}

pub(crate) fn format_partition(
    ctx: &mut Ctx,
    disk: u32,
    number: u32,
    fs: FsKind,
    label: &str,
    cluster: Option<u32>,
    quick: bool,
) -> Result<()> {
    let p = ctx.live_partition(disk, number)?;
    let vol = volume_of(ctx, disk, p)?;
    let sink = ctx.step(&format!("format as {}", fs.display_name()));
    ctx.platform.format(&vol, fs, label, cluster, quick, &sink)
}

/// Replace the drive letter(s) of a partition's volume with `letter` (or remove them).
pub(crate) fn set_letter(ctx: &mut Ctx, disk: u32, number: u32, letter: Option<char>) -> Result<()> {
    let p = ctx.live_partition(disk, number)?;
    let vol = p.volume.clone().ok_or_else(|| {
        Error::Invalid(format!(
            "partition {number} on Disk {disk} has no volume to assign a drive letter to"
        ))
    })?;
    for c in p.mount_points.iter().filter_map(|m| mount_letter(m)) {
        ctx.step(&format!("remove drive letter {c}:"));
        ctx.platform.unmount(&vol, &MountPoint::Letter(c))?;
    }
    if let Some(c) = letter {
        let c = c.to_ascii_uppercase();
        ctx.step(&format!("assign drive letter {c}:"));
        ctx.platform.mount(&vol, &MountPoint::Letter(c))?;
    }
    Ok(())
}

pub(crate) fn mount(ctx: &mut Ctx, vol: &VolumeId, at: &MountPoint) -> Result<()> {
    ctx.step(&format!("mount at {}", at.to_path_string()));
    ctx.platform.mount(vol, at)
}

pub(crate) fn unmount(ctx: &mut Ctx, vol: &VolumeId, at: &MountPoint) -> Result<()> {
    ctx.step(&format!("unmount from {}", at.to_path_string()));
    ctx.platform.unmount(vol, at)
}
