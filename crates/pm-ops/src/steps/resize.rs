//! Resize and move a partition.
//!
//! * A length change needs the filesystem to follow through the platform (shrink: filesystem
//!   first, then the table; grow: table first, then the filesystem). NTFS can shrink and grow;
//!   ReFS can only grow (Windows cannot shrink ReFS online). A partition without a recognised
//!   filesystem may grow in the table only; it may not be shrunk (its filesystem may simply be
//!   unknown to the disk list), and should a shrink get this far the executor reads the
//!   partition head inside the write session and refuses when it finds a filesystem signature.
//!   Anything else is refused (`Error::Unsupported`).
//! * A move copies the data to the new start inside the write session (overlap-safe), then
//!   writes the table; any filesystem can be moved as long as its length is kept.

use super::{ensure_no_filesystem, entry_mut, locate_volume, move_data, volume_of, with_session, Ctx};
use pm_core::{Error, FsKind, PartitionInfo, Result};

/// True for an MBR extended-partition container (a table structure without data of its own).
pub(crate) fn is_container_part(p: &PartitionInfo) -> bool {
    !p.logical && p.ptype.is_some_and(|t| t.is_extended_mbr())
}

/// The refusal of a shrink of a partition whose filesystem the model does not know.
pub(crate) fn unknown_fs_shrink(disk: u32, number: u32) -> Error {
    Error::Invalid(format!(
        "Cannot shrink partition {number} on Disk {disk}: filesystem unknown; refresh or check \
         the volume"
    ))
}

/// Whether changing the length of `p` (partition `number` of `disk`) to `new_len` is possible:
/// `Ok(true)` when the filesystem must follow through the platform (NTFS either way, ReFS
/// growing only), `Ok(false)` for a table-only change (same length, growing a partition
/// without a recognised filesystem, or an extended container), `Unsupported` otherwise. A
/// shrink of a partition whose filesystem is unknown is refused, unless `verified_later`: the
/// executor's step, which checks the device itself inside the write session. Shared by the
/// executor and [`crate::precheck`].
pub(crate) fn check_resize(
    p: &PartitionInfo,
    disk: u32,
    number: u32,
    new_len: u64,
    verified_later: bool,
) -> Result<bool> {
    if new_len == p.len {
        return Ok(false);
    }
    match p.fs {
        None if new_len < p.len && !is_container_part(p) && !verified_later => {
            Err(unknown_fs_shrink(disk, number))
        }
        None => Ok(false),
        Some(FsKind::Ntfs) => Ok(true),
        Some(FsKind::ReFs) if new_len > p.len => Ok(true),
        Some(FsKind::ReFs) => Err(Error::Unsupported(format!(
            "shrinking ReFS is not supported (partition {number} on Disk {disk}); only NTFS \
             volumes can be shrunk online"
        ))),
        Some(fs) => Err(Error::Unsupported(format!(
            "resize of {} is not supported (partition {number} on Disk {disk})",
            fs.display_name()
        ))),
    }
}

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
    let via_platform = check_resize(p, disk, number, new_len, true)?;
    // a shrink that only the table follows: verified against the device inside the session
    let table_only_shrink = !via_platform && new_len < old_len && !is_container_part(p);
    if via_platform && new_len < old_len {
        let vol = volume_of(ctx, disk, p)?;
        ctx.step("shrink the filesystem");
        ctx.platform.shrink_volume(&vol, new_len)?;
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
        if table_only_shrink {
            ensure_no_filesystem(s.device(), old_start, disk, number)?;
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
    if via_platform && new_len > old_len {
        // look the volume up at its new position: after a move its identity may change
        // (MBR volume ids follow the partition offset)
        let vol = locate_volume(ctx.platform, disk, new_start, ctx.cancel)?;
        ctx.step("extend the filesystem");
        ctx.platform.extend_volume(&vol, new_len)?;
    }
    Ok(())
}
