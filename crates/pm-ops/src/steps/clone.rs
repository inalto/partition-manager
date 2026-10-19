//! Partition copies shared by partition clone, disk clone and OS migration.

use super::{entry_of, fs_facts, locate_volume, with_session, Ctx, Session, StepSink};
use crate::operation::CloneMode;
use pm_core::{format_size, Error, FsKind, PartitionInfo, Result, SliceDevice};
use pm_platform::OpenMode;

/// Copy source partition `src` of disk `src_disk` to `[dst_start, dst_start + dst_len)` of the
/// session's disk.
///
/// `Smart` copies only what the filesystem uses; when its used map cannot be read the whole
/// partition is copied (a clone never fails because of the used map). A target smaller than
/// the source is only accepted when the source filesystem (its size and used data) fits it;
/// shrinking a filesystem during a copy is not supported.
pub(crate) fn copy_partition(
    ctx: &Ctx,
    s: &mut Session,
    (src_disk, src): (u32, &PartitionInfo),
    (dst_start, dst_len): (u64, u64),
    mode: CloneMode,
    sink: &StepSink,
) -> Result<u64> {
    let shrinking = dst_len < src.len;
    let facts = (mode == CloneMode::Smart || shrinking)
        .then(|| fs_facts(ctx.platform, src_disk, src.start, src.len))
        .flatten();
    if shrinking {
        let fits = facts.as_ref().is_some_and(|f| {
            f.total <= dst_len && f.used.ranges().all(|(o, l)| o + l <= dst_len)
        });
        if !fits {
            return Err(Error::Unsupported(format!(
                "partition {} of Disk {src_disk} ({}) does not fit its {} target without \
                 shrinking its filesystem, which is not supported; shrink the source partition \
                 first or use a larger target",
                src.number,
                format_size(src.len),
                format_size(dst_len)
            )));
        }
    }
    let len = src.len.min(dst_len);
    let used = match mode {
        CloneMode::Smart => facts.map(|f| f.used),
        CloneMode::Sector => None,
    };
    let disk = ctx.platform.open_disk(src_disk, OpenMode::ReadOnly)?;
    let mut from = SliceDevice::new(disk, src.start, len)?;
    pm_image::copy_simple(&mut from, s.device(), 0, dst_start, len, used.as_ref(), sink, ctx.cancel)
}

/// After a clone: grow an NTFS/ReFS filesystem to fill a target larger than its source. Any
/// failure is only logged — the data is already cloned.
pub(crate) fn grow_after_clone(
    ctx: &mut Ctx,
    dst_disk: u32,
    src: &PartitionInfo,
    (dst_start, dst_len): (u64, u64),
) {
    if dst_len <= src.len {
        return;
    }
    if !matches!(src.fs, Some(FsKind::Ntfs | FsKind::ReFs)) {
        if src.fs.is_some() {
            ctx.note(&format!(
                "the filesystem of partition {} keeps its original size ({})",
                src.number,
                format_size(src.len)
            ));
        }
        return;
    }
    ctx.step(&format!("extend the filesystem of the copy of partition {}", src.number));
    let r = locate_volume(ctx.platform, dst_disk, dst_start, ctx.cancel)
        .and_then(|v| ctx.platform.extend_volume(&v, dst_len));
    if let Err(e) = r {
        ctx.note(&format!("could not extend the filesystem: {e}"));
    }
}

pub(crate) fn clone_partition(
    ctx: &mut Ctx,
    (src_disk, src_number): (u32, u32),
    dst_disk: u32,
    mode: CloneMode,
) -> Result<()> {
    let src = ctx.live_partition(src_disk, src_number)?;
    let dst = ctx.new_partition(dst_disk)?;
    let platform = ctx.platform;
    with_session(platform, dst_disk, |s| {
        let mut t = s.read_table()?;
        if t.entries.iter().any(|e| e.start < dst.end() && dst.start < e.end()) {
            return Err(Error::Invalid(format!(
                "the target range on Disk {dst_disk} is no longer free"
            )));
        }
        let sink = ctx.step(&format!("copy partition {src_number}"));
        copy_partition(ctx, s, (src_disk, src), (dst.start, dst.len), mode, &sink)?;
        ctx.step("write partition table");
        let e = entry_of(t.kind, dst);
        t.entries.push(e);
        s.commit_table(&t)
    })?;
    ctx.rescan(dst_disk)?;
    grow_after_clone(ctx, dst_disk, src, (dst.start, dst.len));
    Ok(())
}
