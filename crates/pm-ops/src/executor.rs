//! Applies a list of pending operations to the disks, one at a time, through [`Platform`].
//!
//! For every operation, in order:
//! 1. the live state is re-read ([`Model::from_platform`]); every disk and partition the
//!    operation references must still be the one it was queued against
//!    ([`crate::layout_matches`] against the queue's base model with the earlier operations
//!    simulated), and the operation is re-validated with [`Planner::simulate`] on it — the
//!    disks may have changed since the queue was built;
//! 2. the running system disk is refused as a destination (re-checked here, not trusted from
//!    the queue);
//! 3. the operation's steps run. Nothing is written outside a [`pm_platform::WriteSession`],
//!    and every session is finished, on success and on every error path;
//! 4. the OS re-reads the layout of the disks that were written, and the executor waits
//!    (bounded) for their volumes to come back before the next operation re-reads the disks.
//!
//! The first failing operation stops the run.

use crate::identity::{adopt_generated_ids, layout_matches};
use crate::model::Model;
use crate::operation::{Operation, Sizing};
use crate::sizing::LayoutSpec;
use crate::planner::Planner;
use crate::steps::{self, message, Ctx};
use pm_core::{CancelToken, Error, ProgressReport, ProgressSink, Result};
use pm_image::RescueStats;
use pm_platform::Platform;
use serde::{Deserialize, Serialize};

/// Progress of one step of one operation. [`execute`] delivers it to the caller's
/// [`ProgressSink`] as a [`ProgressReport`] whose `phase` is
/// `[i/n] <operation summary>: <step>` and whose `detail` carries the engine's own phase and
/// detail (e.g. `Copying`).
#[derive(Clone, Debug)]
pub struct StepEvent {
    /// Index of the operation in the list given to [`execute`].
    pub op_index: usize,
    pub step: String,
    pub progress: ProgressReport,
}

/// Options for [`execute`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ExecOpts {
    /// Re-validate and log `would: <summary>` lines, write nothing.
    pub dry_run: bool,
    /// Let the non-destructive family (resize, drive letter, label, mount/unmount and creating
    /// a partition in free space) touch the running system disk. Never relaxes initialize,
    /// wipe, clone, restore or any other destructive operation.
    pub allow_system_disk: bool,
}

/// What [`execute`] did.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExecReport {
    /// Operations completed (dry run: operations that validated).
    pub completed: usize,
    /// The operation that failed (index into the list) and why; nothing after it ran.
    pub failed: Option<(usize, String)>,
    /// One human-readable line per step, plus notes such as `boot fix failed: ...`.
    pub log: Vec<String>,
    /// Read statistics of every image created.
    pub rescue_stats: Vec<RescueStats>,
}

/// The executor's static refusals, checked on `model` (the state the operation will run
/// against) without touching any device. [`execute`] runs it before every operation (dry run
/// included) and [`crate::OpQueue::push`] at queue time, so an operation the executor would
/// always refuse is never accepted into the queue. The planner's own validation
/// ([`Planner::simulate`]) is separate; missing disks/partitions are left to it.
///
/// Refused:
/// * `SetLabel` (no platform support yet);
/// * a length change of a partition with a recognised filesystem other than NTFS, except
///   growing ReFS (ReFS cannot be shrunk online);
/// * an alignment that would shrink a partition with a recognised filesystem other than NTFS;
/// * a clone / migration that puts a source partition into a smaller target unless the source
///   is an NTFS volume that can be shrunk online. (Conservative: a filesystem smaller
///   than its partition would fit, but the model does not know filesystem sizes.)
/// * any write to a disk Windows reports as read-only or offline;
/// * a rescue-mode image whose mapfile folder does not exist, whose mapfile is the image
///   itself, or whose cluster size is zero.
pub fn precheck(model: &Model, op: &Operation) -> Result<()> {
    if let Some(d) = written_disk(model, op).and_then(|d| model.disk(d)) {
        let i = d.index;
        if d.flags.read_only {
            return Err(Error::Invalid(format!(
                "Disk {i} is read-only; clear its read-only attribute first (Disk Management, or \
                 Set-Disk {i} -IsReadOnly $false)"
            )));
        }
        if d.flags.offline {
            return Err(Error::Invalid(format!(
                "Disk {i} is offline; bring it online first (Disk Management, or Set-Disk {i} \
                 -IsOffline $false)"
            )));
        }
    }
    match op {
        Operation::SetLabel { .. } => Err(set_label_unsupported()),
        Operation::ResizeMove { disk, number, new_len, .. } => {
            if let Some(p) = model.partition(*disk, *number) {
                steps::resize::check_resize(p, *disk, *number, *new_len, false)?;
            }
            Ok(())
        }
        Operation::AlignPartitions { disk } => {
            if let Some(d) = model.disk(*disk) {
                steps::table_ops::align_changes(d, false)?;
            }
            Ok(())
        }
        Operation::ImageDisk { path, rescue: Some(r), .. }
        | Operation::ImagePartition { path, rescue: Some(r), .. } => {
            steps::image::check_rescue_settings(path, r)
        }
        _ => {
            for (src_disk, p, _) in steps::clone::smaller_targets(model, op) {
                if !steps::clone::shrinkable(p) {
                    return Err(steps::clone::must_shrink_first(src_disk, p));
                }
            }
            Ok(())
        }
    }
}

/// `Platform` has no API to change a volume label yet.
fn set_label_unsupported() -> Error {
    Error::Unsupported("set label requires platform support (planned)".into())
}

impl Operation {
    /// Source volumes this clone / migration will shrink (NTFS sources whose planned
    /// target on `model` is smaller), one human-readable line each, for the confirmation
    /// dialog. Empty for every other operation.
    pub fn shrinks_source(&self, model: &Model) -> Vec<String> {
        steps::clone::smaller_targets(model, self)
            .into_iter()
            .filter(|(_, p, _)| steps::clone::shrinkable(p))
            .map(|(d, p, len)| {
                let fs = p.fs.map_or("unknown filesystem", |f| f.display_name());
                let at = p.mount_points.first().map(|m| format!(", {m}")).unwrap_or_default();
                format!(
                    "partition {} ({fs}{at}) on Disk {d}: the source volume will be shrunk from \
                     {} to {}",
                    p.number,
                    pm_core::format_size(p.len),
                    pm_core::format_size(len)
                )
            })
            .collect()
    }
}

/// Operations that `allow_system_disk` may let through on the running system disk.
fn relaxed(op: &Operation) -> bool {
    matches!(
        op,
        Operation::ResizeMove { .. }
            | Operation::SetLabel { .. }
            | Operation::SetLetter { .. }
            | Operation::Mount { .. }
            | Operation::Unmount { .. }
            | Operation::CreatePartition { .. }
    )
}

/// The disk `op` changes: its target disk, or the disk of the volume it mounts/unmounts.
fn written_disk(live: &Model, op: &Operation) -> Option<u32> {
    match op {
        Operation::Mount { volume, .. } | Operation::Unmount { volume, .. } => {
            live.volumes.iter().find(|v| v.id == *volume).and_then(|v| v.disk)
        }
        _ => op.target_disk(),
    }
}

/// Refuse to change the running system disk (or any disk when it cannot be determined),
/// except for the relaxed family when `allow` is set.
fn guard_system_disk(live: &Model, op: &Operation, allow: bool) -> Result<()> {
    let Some(d) = written_disk(live, op) else {
        return Ok(());
    };
    let sys = live.effective_system_disk();
    if sys.is_some_and(|s| s != d) || (allow && relaxed(op)) {
        return Ok(());
    }
    let hint = if relaxed(op) { " (system disk changes are not allowed for this run)" } else { "" };
    Err(Error::Invalid(match sys {
        Some(_) => format!("Disk {d} is the running system disk; refusing to write to it{hint}"),
        None => format!(
            "Disk {d}: cannot determine the running system disk; refusing to write to it{hint}"
        ),
    }))
}

/// Apply `ops` in order (see the module documentation), planned against the disks as they are
/// now: [`execute_queued`] with a freshly read base model.
pub fn execute(
    platform: &dyn Platform,
    ops: &[Operation],
    opts: &ExecOpts,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<ExecReport> {
    let base = Model::from_platform(platform)?;
    execute_queued(platform, &base, ops, opts, progress, cancel)
}

/// The model the next operation is expected to find, carried from one operation to the next.
struct Expected {
    /// The model `model` was simulated from (for [`adopt_generated_ids`]).
    before: Option<Model>,
    model: Model,
    /// Disks the previous operation touched.
    disks: Vec<u32>,
}

impl Expected {
    fn new(base: &Model) -> Self {
        Expected { before: None, model: base.clone(), disks: Vec::new() }
    }

    /// The expected model for the operation about to run on `live`: the identifiers the
    /// previous simulation invented are replaced by those really written.
    fn sync(&mut self, live: &Model) -> &Model {
        if let Some(before) = self.before.take() {
            adopt_generated_ids(&before, &mut self.model, live, &self.disks);
        }
        &self.model
    }

    /// Advance past `op`: fold [`Planner::simulate`] over the expected model; should it not
    /// validate there (it did on the live model), continue from `fallback` (the live model's
    /// simulation, as `fallback_before` produced it).
    fn advance(&mut self, op: &Operation, fallback_before: &Model, fallback: &Model) {
        match Planner::simulate(&self.model, op) {
            Ok(next) => {
                let before = std::mem::replace(&mut self.model, next);
                self.before = Some(before);
            }
            Err(_) => {
                self.model = fallback.clone();
                self.before = Some(fallback_before.clone());
            }
        }
        self.disks = op.disks();
    }
}

/// Apply `ops`, queued against `base` (the model the user saw, e.g. [`crate::OpQueue::base`]),
/// in order (see the module documentation). Before every operation the disks are re-read and
/// everything the operation references must still match the expected model — `base` with the
/// operations before it simulated ([`layout_matches`]); otherwise the run stops with "disk
/// layout changed since the operations were queued" and nothing of that operation is written.
/// Failures of individual operations are reported in [`ExecReport::failed`], not as `Err`;
/// the `Result` is kept for failures of the executor itself.
pub fn execute_queued(
    platform: &dyn Platform,
    base: &Model,
    ops: &[Operation],
    opts: &ExecOpts,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<ExecReport> {
    let mut report = ExecReport::default();
    if opts.dry_run {
        dry_run(platform, base, ops, opts, &mut report);
        return Ok(report);
    }
    let n = ops.len();
    let mut expected = Expected::new(base);
    for (i, op) in ops.iter().enumerate() {
        let r = cancel
            .check()
            .and_then(|_| Model::from_platform(platform))
            .and_then(|live| {
                layout_matches(expected.sync(&live), &live, op)?;
                guard_system_disk(&live, op, opts.allow_system_disk)?;
                precheck(&live, op)?;
                let after = Planner::simulate(&live, op)?;
                let mut ctx = Ctx::new(
                    platform,
                    progress,
                    cancel,
                    (i, n),
                    op,
                    &mut report.log,
                    &mut report.rescue_stats,
                    (&live, &after),
                );
                run_op(&mut ctx, op)?;
                Ok((written_disk(&live, op), live, after))
            });
        let (written, live, after) = match r {
            Ok(v) => v,
            Err(e) => {
                let msg = message(e);
                report.log.push(format!("[{}/{n}] {}: failed: {msg}", i + 1, op.summary()));
                report.failed = Some((i, msg));
                break;
            }
        };
        report.completed += 1;
        expected.advance(op, &live, &after);
        if i + 1 < n {
            if let Some(d) = written {
                settle_volumes(platform, d, &after, cancel, &mut report.log);
            }
        }
    }
    Ok(report)
}

/// Before the next operation re-reads the disks: give the OS time to bring back the volumes of
/// `disk` (every partition of `after` on it that has a volume). Not reaching them is only
/// logged; the next operation re-validates against whatever is there.
fn settle_volumes(
    platform: &dyn Platform,
    disk: u32,
    after: &Model,
    cancel: &CancelToken,
    log: &mut Vec<String>,
) {
    let expected = after
        .disk(disk)
        .map_or(0, |d| d.partitions.iter().filter(|p| p.volume.is_some()).count());
    if expected == 0 {
        return;
    }
    match steps::wait_for_volumes(platform, disk, expected, cancel) {
        Ok(true) => {}
        Ok(false) => log.push(format!(
            "note: not every volume of Disk {disk} came back after the write; continuing"
        )),
        Err(e) => log.push(format!("note: waiting for the volumes of Disk {disk}: {}", message(e))),
    }
}

/// Validate the whole list against the live state (each operation on the model the previous
/// ones produce, and against `base` as [`execute_queued`] does) and log what would be done.
fn dry_run(platform: &dyn Platform, base: &Model, ops: &[Operation], opts: &ExecOpts, report: &mut ExecReport) {
    let n = ops.len();
    let mut model = match Model::from_platform(platform) {
        Ok(m) => m,
        Err(e) => {
            report.failed = Some((0, message(e)));
            return;
        }
    };
    let mut expected = Expected::new(base);
    for (i, op) in ops.iter().enumerate() {
        let r = layout_matches(expected.sync(&model), &model, op)
            .and_then(|_| guard_system_disk(&model, op, opts.allow_system_disk))
            .and_then(|_| precheck(&model, op))
            .and_then(|_| Planner::simulate(&model, op));
        match r {
            Ok(m) => {
                expected.advance(op, &model, &m);
                model = m;
                report.log.push(format!("[{}/{n}] would: {}", i + 1, op.summary()));
                report.completed += 1;
            }
            Err(e) => {
                let msg = message(e);
                report.log.push(format!("[{}/{n}] would fail: {}: {msg}", i + 1, op.summary()));
                report.failed = Some((i, msg));
                return;
            }
        }
    }
}

fn run_op(ctx: &mut Ctx, op: &Operation) -> Result<()> {
    use steps::*;
    use Operation::*;
    match op {
        InitializeDisk { disk, kind } => table_ops::initialize(ctx, *disk, *kind),
        CreatePartition { disk, fs, label, letter, .. } => {
            table_ops::create(ctx, *disk, *fs, label, *letter)
        }
        DeletePartition { disk, number } => table_ops::delete(ctx, *disk, *number),
        FormatPartition { disk, number, fs, label, cluster, quick } => {
            format::format_partition(ctx, *disk, *number, *fs, label, *cluster, *quick)
        }
        ResizeMove { disk, number, new_start, new_len } => {
            resize::resize_move(ctx, *disk, *number, *new_start, *new_len)
        }
        SetActive { disk, number } => table_ops::set_active(ctx, *disk, *number),
        // refused by `precheck` before any step runs; kept for exhaustiveness
        SetLabel { .. } => Err(set_label_unsupported()),
        SetLetter { disk, number, letter } => format::set_letter(ctx, *disk, *number, *letter),
        SetType { disk, number, ptype } => table_ops::set_type(ctx, *disk, *number, *ptype),
        Mount { volume, at } => format::mount(ctx, volume, at),
        Unmount { volume, at } => format::unmount(ctx, volume, at),
        WipeDisk { disk, method } => wipe::wipe_disk(ctx, *disk, *method),
        WipePartition { disk, number, method } => {
            wipe::wipe_partition(ctx, *disk, *number, *method)
        }
        ConvertTable { disk, to } => table_ops::convert(ctx, *disk, *to),
        AlignPartitions { disk } => table_ops::align(ctx, *disk),
        RebuildMbr { disk } => table_ops::rebuild_mbr(ctx, *disk),
        ClonePartition { src_disk, src_number, dst_disk, mode, .. } => {
            clone::clone_partition(ctx, (*src_disk, *src_number), *dst_disk, *mode)
        }
        CloneDisk { src_disk, dst_disk, mode, boot_fix, .. } => {
            let spec = op.layout_spec().unwrap_or(LayoutSpec::new(Sizing::Fit, false));
            migrate::clone_disk(ctx, *src_disk, *dst_disk, spec, *mode, *boot_fix)
        }
        MigrateOs { src_disk, dst_disk, boot_fix, .. } => {
            let spec = op.layout_spec().unwrap_or(LayoutSpec::new(Sizing::Fit, true));
            migrate::clone_disk(
                ctx,
                *src_disk,
                *dst_disk,
                spec,
                crate::operation::CloneMode::Smart,
                *boot_fix,
            )
        }
        ImageDisk { disk, path, format, mode, rescue } => {
            image::image(ctx, *disk, None, path, *format, *mode, rescue.as_ref())
        }
        ImagePartition { disk, number, path, format, mode, rescue } => {
            image::image(ctx, *disk, Some(*number), path, *format, *mode, rescue.as_ref())
        }
        RestoreImage { path, dst_disk, dst_start } => {
            image::restore(ctx, path, *dst_disk, *dst_start)
        }
    }
}
