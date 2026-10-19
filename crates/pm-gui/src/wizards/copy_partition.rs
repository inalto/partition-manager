//! Copy Partition wizard: Source partition → Target space (an unallocated range on any
//! initialized disk that can hold the used data) → Size (a slider within the range, at least
//! the data to copy) → Copy mode → summary → [`Operation::ClonePartition`].

use super::create::usable_ranges;
use super::{disk_preview, partition_desc, row};
use crate::items::is_container;
use crate::selection::Selected;
use crate::wizard::{check_ops, shared, StepResult, Wizard, WizardCtx};
use eframe::egui;
use pm_core::{align_down, align_up, format_size, TableKind, MIB};
use pm_ops::{CloneMode, Model, Operation};

/// Bytes a copy of partition `(disk, number)` needs in `mode`.
pub fn needed(m: &Model, (disk, number): (u32, u32), mode: CloneMode) -> u64 {
    m.partition(disk, number).map_or(0, |p| match mode {
        CloneMode::Smart => p.used.unwrap_or(p.len),
        CloneMode::Sector => p.len,
    })
}

/// Partitions that can be copied (extended containers cannot).
pub fn source_partitions(m: &Model) -> Vec<(u32, u32)> {
    m.disks
        .iter()
        .flat_map(|d| d.partitions.iter().filter(|p| !is_container(d, p) && p.len > 0).map(|p| (d.index, p.number)))
        .collect()
}

/// `(disk, aligned start, usable length)` of every unallocated range of an initialized disk
/// that holds at least `need` bytes (rounded up to 1 MiB).
pub fn target_ranges(m: &Model, need: u64) -> Vec<(u32, u64, u64)> {
    let need = align_up(need.max(1), MIB);
    m.disks
        .iter()
        .filter(|d| matches!(d.table, TableKind::Gpt | TableKind::Mbr))
        .flat_map(|d| {
            usable_ranges(d).into_iter().map(move |(s, l)| {
                let start = align_up(s, MIB);
                (d.index, start, align_down(s + l, MIB).saturating_sub(start))
            })
        })
        .filter(|r| r.2 >= need)
        .collect()
}

pub struct CopyPartitionState {
    pub src: (u32, u32),
    pub range: Option<(u32, u64, u64)>,
    pub len: u64,
    pub mode: CloneMode,
}

impl CopyPartitionState {
    pub fn initial(ctx: &WizardCtx) -> Option<CopyPartitionState> {
        let sources = source_partitions(ctx.model);
        let src = match ctx.selection {
            Selected::Partition(d, n) if sources.contains(&(d, n)) => (d, n),
            _ => *sources.first()?,
        };
        Some(CopyPartitionState { src, range: None, len: 0, mode: CloneMode::Smart })
    }

    /// Smallest size the slider allows: the data to copy, rounded up to 1 MiB.
    pub fn min_len(&self, m: &Model) -> u64 {
        align_up(needed(m, self.src, self.mode).max(MIB), MIB)
    }

    /// Choose `range` and size the copy like the source (clamped to the range).
    pub fn set_range(&mut self, m: &Model, range: (u32, u64, u64)) {
        self.range = Some(range);
        let src_len = m.partition(self.src.0, self.src.1).map_or(0, |p| align_up(p.len, MIB));
        self.len = src_len.clamp(self.min_len(m).min(range.2), range.2);
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        let (dst_disk, dst_start, _) = self.range.ok_or("Choose where to copy the partition.")?;
        Ok(vec![Operation::ClonePartition {
            src_disk: self.src.0,
            src_number: self.src.1,
            dst_disk,
            dst_start,
            dst_len: self.len,
            mode: self.mode,
        }])
    }
}

fn source_ui(s: &mut CopyPartitionState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    let m = ctx.model;
    let desc = |(d, n): (u32, u32)| m.partition(d, n).map_or(format!("Partition {n} on Disk {d}"), |p| partition_desc(d, p));
    ui.label("Select the partition to copy:");
    let mut pick = s.src;
    egui::ComboBox::from_id_salt("copy_part_src").width(460.0).selected_text(desc(s.src)).show_ui(ui, |ui| {
        for p in source_partitions(m) {
            ui.selectable_value(&mut pick, p, desc(p));
        }
    });
    if pick != s.src {
        s.src = pick;
        s.range = None;
    }
    if let Some(d) = m.disk(s.src.0) {
        ui.add_space(8.0);
        disk_preview(ui, d, Some(Selected::Partition(s.src.0, s.src.1)));
    }
    let used = m.partition(s.src.0, s.src.1).and_then(|p| p.used);
    ui.label(format!("Used data: {}", used.map_or("unknown (the whole partition is copied)".into(), format_size)));
    StepResult::Continue
}

fn target_ui(s: &mut CopyPartitionState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    let m = ctx.model;
    let ranges = target_ranges(m, needed(m, s.src, CloneMode::Smart));
    if ranges.is_empty() {
        return StepResult::Invalid("No unallocated space is large enough for the data of this partition.".into());
    }
    ui.label("Select unallocated space for the copy:");
    let mut pick = s.range;
    for r in &ranges {
        let name = m.disk(r.0).map_or(format!("Disk {}", r.0), |d| d.display_name());
        ui.radio_value(&mut pick, Some(*r), format!("{name}: {} at {}", format_size(r.2), format_size(r.1)));
    }
    if pick != s.range {
        if let Some(r) = pick {
            s.set_range(m, r);
        }
    }
    if let Some((d, start, _)) = s.range {
        if let Some(disk) = m.disk(d) {
            ui.add_space(8.0);
            let free = super::create::usable_ranges(disk).into_iter().find(|&(fs, fl)| fs <= start && start < fs + fl);
            disk_preview(ui, disk, free.map(|(fs, fl)| Selected::Free(d, fs, fl)));
        }
    }
    StepResult::from_problem(s.range.is_none().then(|| "Choose where to copy the partition.".into()))
}

fn size_ui(s: &mut CopyPartitionState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    let m = ctx.model;
    let Some((d, _, max)) = s.range else {
        return StepResult::Invalid("Choose where to copy the partition.".into());
    };
    let min = s.min_len(m).min(max);
    let mut mb = s.len / MIB;
    ui.label("Size of the copy:");
    ui.add(egui::Slider::new(&mut mb, min / MIB..=max / MIB).suffix(" MB"));
    s.len = (mb * MIB).clamp(min, max);
    ui.weak(format!("At least {} (the data to copy), at most {} (the free space).", format_size(min), format_size(max)));
    preview(s, ui, m, d)
}

fn preview(s: &CopyPartitionState, ui: &mut egui::Ui, m: &Model, d: u32) -> StepResult {
    let result = s.build().and_then(|ops| check_ops(m, &ops));
    if let Ok(after) = &result {
        if let Some(disk) = after.disk(d) {
            ui.add_space(8.0);
            ui.label("Preview:");
            let new = s.range.and_then(|(_, start, _)| disk.partitions.iter().find(|p| p.start == start)).map(|p| p.number);
            disk_preview(ui, disk, new.map(|n| Selected::Partition(d, n)));
        }
    }
    StepResult::from_problem(result.err())
}

fn mode_ui(s: &mut CopyPartitionState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    ui.radio_value(&mut s.mode, CloneMode::Smart, super::copy_disk::mode_name(CloneMode::Smart));
    ui.indent("smart_help", |ui| ui.weak("Copies only the blocks the filesystem uses."));
    ui.radio_value(&mut s.mode, CloneMode::Sector, super::copy_disk::mode_name(CloneMode::Sector));
    ui.indent("sector_help", |ui| ui.weak("Copies every sector, including free space; the copy must be at least as large as the source."));
    if let Some((_, _, max)) = s.range {
        let min = s.min_len(ctx.model);
        if min > max {
            return StepResult::Invalid(format!("A sector copy needs {}; the chosen space has {}.", format_size(min), format_size(max)));
        }
        s.len = s.len.max(min);
    }
    StepResult::from_problem(s.build().and_then(|ops| check_ops(ctx.model, &ops)).err())
}

fn source_summary(s: &CopyPartitionState) -> Vec<(String, String)> {
    vec![row("Source", format!("Partition {} on Disk {}", s.src.1, s.src.0))]
}

fn target_summary(s: &CopyPartitionState) -> Vec<(String, String)> {
    vec![row("Target", s.range.map_or("-".into(), |(d, st, _)| format!("Disk {d} at {}", format_size(st))))]
}

fn size_summary(s: &CopyPartitionState) -> Vec<(String, String)> {
    vec![row("Size", format_size(s.len))]
}

fn mode_summary(s: &CopyPartitionState) -> Vec<(String, String)> {
    vec![row("Copy mode", super::copy_disk::mode_name(s.mode))]
}

pub fn wizard(state: CopyPartitionState) -> Wizard {
    Wizard::over_state(
        "Copy Partition",
        shared(state),
        vec![
            ("Source partition", source_ui, source_summary),
            ("Target space", target_ui, target_summary),
            ("Size", size_ui, size_summary),
            ("Copy mode", mode_ui, mode_summary),
        ],
        CopyPartitionState::build,
    )
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    CopyPartitionState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};

    #[test]
    fn build_clone_partition_op_from_filled_state() {
        let mut big = gpt_disk(2);
        big.partitions.clear();
        let m = model_of(vec![gpt_disk(1), big, mbr_disk(0)]);
        let ctx = WizardCtx::new(&m, vec![], Selected::Partition(1, 3));
        let mut s = CopyPartitionState::initial(&ctx).unwrap();
        assert_eq!(s.src, (1, 3));
        assert!(s.build().is_err(), "no target yet");
        // 342 MiB used: the 222 MiB tail of Disk 1 and the MBR gap of 99 MiB are too small
        let ranges = target_ranges(&m, needed(&m, s.src, CloneMode::Smart));
        assert!(ranges.iter().all(|r| r.0 != 1), "{ranges:?}");
        let r = *ranges.iter().find(|r| r.0 == 2).unwrap();
        assert_eq!(r.1, MIB);
        s.set_range(&m, r);
        assert_eq!(s.len, 684 * MIB, "sized like the source");
        assert_eq!(s.min_len(&m), 342 * MIB);
        s.len = 400 * MIB;
        let ops = s.build().unwrap();
        assert_eq!(
            ops,
            vec![Operation::ClonePartition {
                src_disk: 1,
                src_number: 3,
                dst_disk: 2,
                dst_start: MIB,
                dst_len: 400 * MIB,
                mode: CloneMode::Smart
            }]
        );
        let after = check_ops(&m, &ops).unwrap();
        assert_eq!(after.disk(2).unwrap().partitions.len(), 1);
        // a sector copy needs the whole partition
        s.mode = CloneMode::Sector;
        assert_eq!(s.min_len(&m), 684 * MIB);
        assert!(check_ops(&m, &s.build().unwrap()).is_err());
        assert_eq!(wizard(s).build_ops().unwrap()[0], Operation::ClonePartition {
            src_disk: 1,
            src_number: 3,
            dst_disk: 2,
            dst_start: MIB,
            dst_len: 400 * MIB,
            mode: CloneMode::Sector
        });
        // the extended container is not a source
        assert!(!source_partitions(&m).contains(&(0, 2)));
    }
}
