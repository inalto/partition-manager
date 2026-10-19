//! Resize and move a partition.
//!
//! * A length change needs the filesystem to follow: NTFS and ReFS go through the platform
//!   (shrink: filesystem first, then the table; grow: table first, then the filesystem).
//!   A partition without a recognised filesystem changes only in the table. Any other
//!   filesystem is refused (`Error::Unsupported`).
//! * A move copies the data to the new start inside the write session (overlap-safe), then
//!   writes the table; any filesystem can be moved as long as its length is kept.

use super::{entry_mut, move_data, volume_of, with_session, Ctx};
use pm_core::{Error, FsKind, Result};

pub(crate) fn resize_move(
    ctx: &mut Ctx,
    disk: u32,
    number: u32,
    new_start: u64,
    new_len: u64,
) -> Result<()> {
    let p = ctx.live_partition(disk, number)?;
    let (old_start, old_len) = (p.start, p.len);
    let moving = new_start != old_start;
    let resizing = new_len != old_len;
    let via_platform = matches!(p.fs, Some(FsKind::Ntfs | FsKind::ReFs));
    if resizing {
        if let Some(fs) = p.fs.filter(|_| !via_platform) {
            return Err(Error::Unsupported(format!(
                "resize of {} is not supported (partition {number} on Disk {disk})",
                fs.display_name()
            )));
        }
    }
    let vol = if resizing && via_platform { Some(volume_of(ctx, disk, p)?) } else { None };

    if let (Some(vol), true) = (&vol, new_len < old_len) {
        ctx.step("shrink the filesystem");
        ctx.platform.shrink_volume(vol, new_len)?;
    }
    let platform = ctx.platform;
    with_session(platform, disk, |s| {
        let mut t = s.read_table()?;
        let e = entry_mut(&mut t, disk, number)?;
        if (e.start, e.len) != (old_start, old_len) {
            return Err(Error::Invalid(format!(
                "partition {number} on Disk {disk} changed on the device"
            )));
        }
        if moving {
            let sink = ctx.step("move the data");
            move_data(s.device(), old_start, new_start, new_len.min(old_len), &sink, ctx.cancel)?;
        }
        ctx.step("write partition table");
        let e = entry_mut(&mut t, disk, number)?;
        e.start = new_start;
        e.len = new_len;
        s.commit_table(&t)
    })?;
    ctx.rescan(disk)?;
    if let (Some(vol), true) = (&vol, new_len > old_len) {
        ctx.step("extend the filesystem");
        ctx.platform.extend_volume(vol, new_len)?;
    }
    Ok(())
}
