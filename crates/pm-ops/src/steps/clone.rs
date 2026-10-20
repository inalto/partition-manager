//! Partition copies shared by partition clone, disk clone and OS migration.

use super::{added_partition, entry_of, fs_facts, locate_volume, with_session, Ctx, Session, StepSink};
use crate::model::Model;
use crate::operation::{CloneMode, Operation};
use crate::planner::Planner;
use crate::sizing::{plan_disk_clone, LayoutSpec};
use pm_core::{format_size, Error, FsKind, PartitionInfo, Result, SliceDevice, VolumeId};
use pm_fs::UsedMap;
use pm_platform::OpenMode;

/// True if the filesystem of `p` can be shrunk online through the platform: a mounted NTFS
/// volume (Windows' FSCTL_SHRINK_VOLUME does not support ReFS).
pub(crate) fn shrinkable(p: &PartitionInfo) -> bool {
    p.fs == Some(FsKind::Ntfs) && p.volume.is_some()
}

fn fs_name(p: &PartitionInfo) -> &'static str {
    p.fs.map_or("unknown filesystem", |f| f.display_name())
}

/// The refusal for a source that is larger than its target and cannot be shrunk online.
pub(crate) fn must_shrink_first(src_disk: u32, p: &PartitionInfo) -> Error {
    Error::Unsupported(format!(
        "partition {} ({}) on Disk {src_disk} must be shrunk first; only NTFS volumes can be \
         shrunk online",
        p.number,
        fs_name(p)
    ))
}

/// `(source disk, source partition, planned target length)` for every partition that `op`
/// (a partition clone, disk clone or OS migration) copies into a target **smaller than the
/// source partition**, computed on `model` with the planner's own numbers. Empty for other
/// operations or when the planner rejects the operation.
pub(crate) fn smaller_targets<'m>(
    model: &'m Model,
    op: &Operation,
) -> Vec<(u32, &'m PartitionInfo, u64)> {
    let pick = |src_disk: u32, pairs: Vec<(u32, u64)>| -> Vec<(u32, &'m PartitionInfo, u64)> {
        pairs
            .into_iter()
            .filter_map(|(n, len)| model.partition(src_disk, n).map(|p| (src_disk, p, len)))
            .filter(|(_, p, len)| *len < p.len)
            .collect()
    };
    match op {
        Operation::ClonePartition { src_disk, src_number, dst_disk, .. } => {
            let (Some(before), Ok(after)) = (model.disk(*dst_disk), Planner::simulate(model, op))
            else {
                return Vec::new();
            };
            let len = after.disk(*dst_disk).and_then(|d| added_partition(before, d)).map(|p| p.len);
            pick(*src_disk, len.map(|l| vec![(*src_number, l)]).unwrap_or_default())
        }
        Operation::CloneDisk { src_disk, dst_disk, mode, .. } => op
            .layout_spec()
            .and_then(|spec| disk_pairs(model, (*src_disk, *dst_disk), spec, *mode))
            .map_or_else(Vec::new, |v| pick(*src_disk, v)),
        Operation::MigrateOs { src_disk, dst_disk, .. } => op
            .layout_spec()
            .and_then(|spec| disk_pairs(model, (*src_disk, *dst_disk), spec, CloneMode::Smart))
            .map_or_else(Vec::new, |v| pick(*src_disk, v)),
        _ => Vec::new(),
    }
}

/// `(source number, target length)` of the disk clone layout the planner computes.
fn disk_pairs(
    model: &Model,
    (src_disk, dst_disk): (u32, u32),
    spec: LayoutSpec,
    mode: CloneMode,
) -> Option<Vec<(u32, u64)>> {
    let (src, dst) = (model.disk(src_disk)?, model.disk(dst_disk)?);
    let used_of = |p: &PartitionInfo| match mode {
        CloneMode::Smart => p.used.unwrap_or(p.len),
        CloneMode::Sector => p.len,
    };
    let probe = model.windows_probe();
    let layout =
        plan_disk_clone(src, dst.size, dst.sector_size, spec, &used_of, Some(&probe)).ok()?;
    Some(
        layout
            .map
            .iter()
            .filter_map(|&(sn, dn)| {
                layout.table.entries.iter().find(|e| e.number == dn).map(|e| (sn, e.len))
            })
            .collect(),
    )
}

/// One source partition to copy, fully decided before anything is written to the target.
pub(crate) struct CopyPlan<'a> {
    pub src_disk: u32,
    pub part: &'a PartitionInfo,
    /// Bytes to copy from the start of the source partition.
    pub len: u64,
    /// Smart mode: what the filesystem uses (`None`: copy everything).
    pub used: Option<UsedMap>,
}

/// Pre-flight of every copy of a clone, run **before** the target's write session so that a
/// refusal writes nothing:
///
/// 1. decide (read-only): a source larger than its target must have a filesystem that already
///    fits the target, or be an NTFS volume that can be shrunk online; anything else is
///    refused ([`must_shrink_first`]);
/// 2. shrink those source volumes through the platform (allowed on the system disk: that is
///    what migrating to a smaller SSD needs), logging `shrinking source volume …`;
/// 3. read the used maps (after the shrinks, which move clusters). A used map that cannot be
///    read falls back to a full copy — a clone never fails because of the used map.
pub(crate) fn prepare_copies<'a>(
    ctx: &mut Ctx,
    items: &[(u32, &'a PartitionInfo, u64)],
    mode: CloneMode,
) -> Result<Vec<CopyPlan<'a>>> {
    struct Decision<'a> {
        src_disk: u32,
        part: &'a PartitionInfo,
        target_len: u64,
        shrink: Option<VolumeId>,
    }
    let mut decisions = Vec::new();
    for &(src_disk, part, target_len) in items {
        let mut shrink = None;
        if target_len < part.len {
            let fs_len = fs_facts(ctx.platform, src_disk, part.start, part.len)
                .map_or(part.len, |f| f.footprint());
            if fs_len > target_len {
                if !shrinkable(part) {
                    return Err(must_shrink_first(src_disk, part));
                }
                shrink = part.volume.clone();
            }
        }
        decisions.push(Decision { src_disk, part, target_len, shrink });
    }
    let shrinking: Vec<String> = decisions
        .iter()
        .filter(|d| d.shrink.is_some())
        .map(|d| {
            format!(
                "partition {} ({}) on Disk {} from {} to {}",
                d.part.number,
                fs_name(d.part),
                d.src_disk,
                format_size(d.part.len),
                format_size(d.target_len)
            )
        })
        .collect();
    if !shrinking.is_empty() {
        ctx.note(&format!("source volume(s) will be shrunk: {}", shrinking.join("; ")));
    }
    for d in &decisions {
        if let Some(vol) = &d.shrink {
            ctx.step(&format!(
                "shrinking source volume {}: from {} to {}",
                vol.0,
                format_size(d.part.len),
                format_size(d.target_len)
            ));
            ctx.platform.shrink_volume(vol, d.target_len)?;
        }
    }
    let mut plans = Vec::new();
    for d in decisions {
        let len = d.part.len.min(d.target_len);
        let facts = (mode == CloneMode::Smart || d.shrink.is_some())
            .then(|| fs_facts(ctx.platform, d.src_disk, d.part.start, d.part.len))
            .flatten();
        if let Some(f) = &facts {
            if f.footprint() > len {
                return Err(Error::Invalid(format!(
                    "partition {} on Disk {}: its filesystem ({}) is still larger than its {} \
                     target",
                    d.part.number,
                    d.src_disk,
                    format_size(f.footprint()),
                    format_size(len)
                )));
            }
        }
        let used = match mode {
            CloneMode::Smart => facts.map(|f| f.used),
            CloneMode::Sector => None,
        };
        plans.push(CopyPlan { src_disk: d.src_disk, part: d.part, len, used });
    }
    Ok(plans)
}

/// Copy a prepared source partition to `dst_start` of the session's disk.
pub(crate) fn copy_partition(
    ctx: &Ctx,
    s: &mut Session,
    plan: &CopyPlan,
    dst_start: u64,
    sink: &StepSink,
) -> Result<u64> {
    let disk = ctx.platform.open_disk(plan.src_disk, OpenMode::ReadOnly)?;
    let mut from = SliceDevice::new(disk, plan.part.start, plan.len)?;
    pm_image::copy_simple(
        &mut from,
        s.device(),
        0,
        dst_start,
        plan.len,
        plan.used.as_ref(),
        sink,
        ctx.cancel,
    )
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
    let plans = prepare_copies(ctx, &[(src_disk, src, dst.len)], mode)?;
    let platform = ctx.platform;
    with_session(platform, dst_disk, |s| {
        let mut t = s.read_table()?;
        if t.entries.iter().any(|e| e.start < dst.end() && dst.start < e.end()) {
            return Err(Error::Invalid(format!(
                "the target range on Disk {dst_disk} is no longer free"
            )));
        }
        let sink = ctx.step(&format!("copy partition {src_number}"));
        copy_partition(ctx, s, &plans[0], dst.start, &sink)?;
        ctx.step("write partition table");
        let e = entry_of(t.kind, dst);
        t.entries.push(e);
        s.commit_table(&t)
    })?;
    ctx.rescan(dst_disk)?;
    grow_after_clone(ctx, dst_disk, src, (dst.start, dst.len));
    Ok(())
}
