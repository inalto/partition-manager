//! Create Partition wizard: Location (disk and unallocated range) → Size and position (the
//! resize bar over the free range, snapped to 1 MiB, at least 1 MiB) → Properties (type,
//! filesystem, label, drive letter, GPT name) → [`Operation::CreatePartition`].

use super::resize::{bar_editor, BarState};
use super::resize_bar::Extent;
use super::{disk_preview, label_problem, letter_combo, row, FAT32_MAX};
use crate::selection::Selected;
use crate::wizard::{check_ops, shared, StepResult, Wizard, WizardCtx};
use eframe::egui;
use pm_core::{align_down, align_up, format_size, gpt_types, DiskInfo, FsKind, PartitionType, TableKind, MIB};
use pm_ops::{Model, Operation};

/// Unallocated ranges of `d` that can hold a 1 MiB-aligned partition of at least 1 MiB.
pub fn usable_ranges(d: &DiskInfo) -> Vec<(u64, u64)> {
    d.free_ranges(MIB)
        .into_iter()
        .filter(|&(s, l)| align_down(s + l, MIB).saturating_sub(align_up(s, MIB)) >= MIB)
        .collect()
}

/// The bar extent of the free range `(start, len)`: aligned start, 1 MiB minimum, and an end
/// that never leaves the grid (the planner rounds a new partition's length down to 1 MiB).
pub fn range_extent((start, len): (u64, u64)) -> Extent {
    Extent {
        lo: align_up(start, MIB),
        hi: start + len,
        min_len: MIB,
        grid: MIB,
        keep_start: None,
        keep_end: None,
        end_at_hi: false,
    }
}

/// Disks a partition can be created on: initialized, with usable free space.
pub fn eligible_disks(m: &Model) -> Vec<u32> {
    m.disks
        .iter()
        .filter(|d| matches!(d.table, TableKind::Gpt | TableKind::Mbr) && !usable_ranges(d).is_empty())
        .map(|d| d.index)
        .collect()
}

/// Partition type presets for a `table` disk.
pub fn type_choices(table: TableKind) -> Vec<(PartitionType, &'static str)> {
    match table {
        TableKind::Gpt => vec![
            (PartitionType::Gpt(gpt_types::BASIC_DATA), "Basic data"),
            (PartitionType::Gpt(gpt_types::EFI_SYSTEM), "EFI system"),
            (PartitionType::Gpt(gpt_types::MSR), "Microsoft reserved"),
            (PartitionType::Gpt(gpt_types::WINDOWS_RECOVERY), "Recovery"),
            (PartitionType::Gpt(gpt_types::LINUX_FS), "Linux filesystem"),
        ],
        _ => vec![
            (PartitionType::Mbr(0x07), "NTFS / exFAT (0x07)"),
            (PartitionType::Mbr(0x0C), "FAT32 LBA (0x0C)"),
            (PartitionType::Mbr(0x83), "Linux (0x83)"),
        ],
    }
}

/// Filesystems offered for a new partition.
pub const FS_CHOICES: [Option<FsKind>; 4] = [None, Some(FsKind::Ntfs), Some(FsKind::Fat32), Some(FsKind::ExFat)];

fn fs_name(fs: Option<FsKind>) -> &'static str {
    fs.map_or("None (unformatted)", |f| f.display_name())
}

pub struct CreateState {
    pub disk: u32,
    pub table: TableKind,
    pub ranges: Vec<(u64, u64)>,
    pub range: usize,
    pub bar: BarState,
    pub ptype: PartitionType,
    pub fs: Option<FsKind>,
    pub label: String,
    pub letter: Option<char>,
    pub name: String,
    /// Letters offered (free on the system and in the preview).
    pub letters: Vec<char>,
}

impl CreateState {
    /// State for the selection: its disk (the first eligible disk otherwise) and, for a
    /// selected unallocated range, that range, entirely.
    pub fn initial(ctx: &WizardCtx) -> Option<CreateState> {
        let eligible = eligible_disks(ctx.model);
        let disk = ctx.selection.disk().filter(|d| eligible.contains(d))?;
        let d = ctx.model.disk(disk)?;
        let ranges = usable_ranges(d);
        let range = match ctx.selection {
            Selected::Free(_, s, l) => ranges.iter().position(|&(rs, rl)| s < rs + rl && rs < s + l).unwrap_or(0),
            _ => 0,
        };
        let mut st = CreateState {
            disk,
            table: d.table,
            ranges: Vec::new(),
            range: 0,
            bar: BarState::new(range_extent((MIB, MIB)), MIB, MIB, None),
            ptype: type_choices(d.table)[0].0,
            fs: Some(FsKind::Ntfs),
            label: String::new(),
            letter: ctx.platform_free_letters.first().copied(),
            name: String::new(),
            letters: ctx.platform_free_letters.clone(),
        };
        st.set_disk(d);
        st.set_range(range);
        Some(st)
    }

    /// Switch to disk `d` (its first range, its default type).
    pub fn set_disk(&mut self, d: &DiskInfo) {
        if self.disk != d.index || self.table != d.table || self.ranges.is_empty() {
            self.ptype = type_choices(d.table)[0].0;
        }
        self.disk = d.index;
        self.table = d.table;
        self.ranges = usable_ranges(d);
        self.set_range(0);
    }

    /// Select free range `i` and fill it entirely.
    pub fn set_range(&mut self, i: usize) {
        let Some(&r) = self.ranges.get(i) else { return };
        self.range = i;
        let e = range_extent(r);
        let len = align_down(e.hi, MIB) - e.lo;
        self.bar = BarState::new(e, e.lo, len, None);
    }

    /// Pick `fs`; on MBR the matching type preset follows it.
    pub fn set_fs(&mut self, fs: Option<FsKind>) {
        self.fs = fs;
        if self.table == TableKind::Mbr {
            match fs {
                Some(FsKind::Ntfs | FsKind::ExFat) => self.ptype = PartitionType::Mbr(0x07),
                Some(FsKind::Fat32) => self.ptype = PartitionType::Mbr(0x0C),
                _ => {}
            }
        }
    }

    /// Why the properties cannot be used, if they cannot.
    pub fn props_problem(&self) -> Option<String> {
        if self.fs == Some(FsKind::Fat32) && self.bar.len > FAT32_MAX {
            return Some("FAT32 is limited to 32 GB here; choose NTFS or exFAT.".into());
        }
        if let Some(c) = self.letter {
            if !self.letters.contains(&c) {
                return Some(format!("Drive letter {c}: is not free."));
            }
        }
        label_problem(self.fs, &self.label)
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        if let Some(p) = self.props_problem() {
            return Err(p);
        }
        Ok(vec![Operation::CreatePartition {
            disk: self.disk,
            start: self.bar.start,
            len: self.bar.len,
            ptype: self.ptype,
            fs: self.fs,
            label: self.label.trim().to_string(),
            letter: self.letter,
            name: if self.table == TableKind::Gpt { self.name.trim().to_string() } else { String::new() },
        }])
    }
}

fn location_ui(s: &mut CreateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    let disks = eligible_disks(ctx.model);
    let name = |i: u32| ctx.model.disk(i).map_or(format!("Disk {i}"), |d| d.display_name());
    ui.horizontal(|ui| {
        ui.label("Disk:");
        let mut pick = s.disk;
        egui::ComboBox::from_id_salt("create_disk").width(320.0).selected_text(name(s.disk)).show_ui(ui, |ui| {
            for i in &disks {
                ui.selectable_value(&mut pick, *i, name(*i));
            }
        });
        if pick != s.disk {
            if let Some(d) = ctx.model.disk(pick) {
                s.set_disk(d);
            }
        }
    });
    ui.add_space(6.0);
    ui.label("Unallocated space:");
    let mut pick = s.range;
    for (i, &(start, len)) in s.ranges.iter().enumerate() {
        ui.radio_value(&mut pick, i, format!("{} at {}", format_size(len), format_size(start)));
    }
    if pick != s.range {
        s.set_range(pick);
    }
    if let Some(d) = ctx.model.disk(s.disk) {
        ui.add_space(8.0);
        let (start, len) = s.ranges[s.range];
        disk_preview(ui, d, Some(Selected::Free(s.disk, start, len)));
    }
    if s.table == TableKind::Mbr {
        ui.add_space(4.0);
        ui.weak("MBR disks hold at most 4 primary partitions.");
    }
    StepResult::Continue
}

/// The model after the new partition, or why it cannot be created.
fn preview(s: &CreateState, model: &Model) -> Result<Model, String> {
    s.build().and_then(|ops| check_ops(model, &ops))
}

fn size_ui(s: &mut CreateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    ui.weak("Drag the handles or the partition, or type the sizes (MB unless a unit is given).");
    ui.add_space(6.0);
    bar_editor(ui, &mut s.bar);
    ui.add_space(8.0);
    let result = preview(s, ctx.model);
    if let Ok(m) = &result {
        if let Some(d) = m.disk(s.disk) {
            ui.label("Preview:");
            let new = d.partitions.iter().find(|p| p.start == s.bar.start).map(|p| p.number);
            disk_preview(ui, d, new.map(|n| Selected::Partition(s.disk, n)));
        }
    }
    StepResult::from_problem(result.err())
}

fn props_ui(s: &mut CreateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    egui::Grid::new("create_props").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Partition type:");
        let types = type_choices(s.table);
        let current = types.iter().find(|t| t.0 == s.ptype).map_or_else(|| s.ptype.name(), |t| t.1.to_string());
        egui::ComboBox::from_id_salt("create_type").width(220.0).selected_text(current).show_ui(ui, |ui| {
            for (t, name) in &types {
                ui.selectable_value(&mut s.ptype, *t, *name);
            }
        });
        ui.end_row();

        ui.label("File system:");
        let mut fs = s.fs;
        egui::ComboBox::from_id_salt("create_fs").width(220.0).selected_text(fs_name(fs)).show_ui(ui, |ui| {
            for f in FS_CHOICES {
                let too_big = f == Some(FsKind::Fat32) && s.bar.len > FAT32_MAX;
                ui.add_enabled_ui(!too_big, |ui| ui.selectable_value(&mut fs, f, fs_name(f)))
                    .response
                    .on_disabled_hover_text("FAT32 volumes are limited to 32 GB");
            }
        });
        if fs != s.fs {
            s.set_fs(fs);
        }
        ui.end_row();

        ui.label("Label:");
        ui.add_enabled(s.fs.is_some(), egui::TextEdit::singleline(&mut s.label).desired_width(220.0));
        ui.end_row();

        ui.label("Drive letter:");
        letter_combo(ui, "create_letter", &mut s.letter, &s.letters);
        ui.end_row();

        if s.table == TableKind::Gpt {
            ui.label("Partition name:");
            ui.add(egui::TextEdit::singleline(&mut s.name).desired_width(220.0).hint_text("optional"));
            ui.end_row();
        }
    });
    if s.bar.len > FAT32_MAX {
        ui.add_space(4.0);
        ui.weak("FAT32 is not offered: the partition is larger than 32 GB.");
    }
    StepResult::from_problem(preview(s, ctx.model).err())
}

fn location_summary(s: &CreateState) -> Vec<(String, String)> {
    vec![row("Disk", format!("Disk {}", s.disk))]
}

fn size_summary(s: &CreateState) -> Vec<(String, String)> {
    vec![row("Start", format_size(s.bar.start)), row("Size", format_size(s.bar.len))]
}

fn props_summary(s: &CreateState) -> Vec<(String, String)> {
    let mut rows = vec![
        row("Type", s.ptype.name()),
        row("File system", fs_name(s.fs)),
        row("Label", if s.label.is_empty() { "(none)".to_string() } else { s.label.clone() }),
        row("Drive letter", s.letter.map_or("None".to_string(), |c| format!("{c}:"))),
    ];
    if s.table == TableKind::Gpt && !s.name.trim().is_empty() {
        rows.push(row("Name", s.name.trim()));
    }
    rows
}

pub fn wizard(state: CreateState) -> Wizard {
    Wizard::over_state(
        "Create Partition",
        shared(state),
        vec![
            ("Location", location_ui, location_summary),
            ("Size and position", size_ui, size_summary),
            ("Properties", props_ui, props_summary),
        ],
        CreateState::build,
    )
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    CreateState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::disk_items;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};
    use pm_core::GIB;

    fn gpt_free_ctx(m: &Model) -> WizardCtx<'_> {
        let free = disk_items(m.disk(1).unwrap()).last().unwrap().sel;
        WizardCtx::new(m, vec!['G', 'H'], free)
    }

    #[test]
    fn create_size_snapping_and_clamping() {
        let m = model_of(vec![gpt_disk(1)]);
        let mut s = CreateState::initial(&gpt_free_ctx(&m)).unwrap();
        // the whole free tail, ending on the last full MiB before the GPT backup
        assert_eq!((s.bar.start, s.bar.len), (801 * MIB, 222 * MIB));
        s.bar.apply_fields(None, Some(100 * MIB + 300_000), None);
        assert_eq!(s.bar.len, 100 * MIB, "snapped to 1 MiB");
        s.bar.apply_fields(None, Some(10 * GIB), None);
        assert_eq!(s.bar.len, 222 * MIB, "clamped to the free range");
        s.bar.apply_fields(None, Some(1000), None);
        assert_eq!(s.bar.len, MIB, "at least 1 MiB");
        s.bar.apply_fields(Some(20 * MIB + 600_000), Some(50 * MIB), None);
        assert_eq!((s.bar.start, s.bar.len), (822 * MIB, 50 * MIB));
        s.bar.apply_fields(None, None, Some(0));
        assert_eq!(s.bar.start + s.bar.len, 1023 * MIB, "never past the aligned end");
    }

    #[test]
    fn build_create_op_from_filled_state() {
        let m = model_of(vec![gpt_disk(1)]);
        let mut s = CreateState::initial(&gpt_free_ctx(&m)).unwrap();
        s.bar.apply_fields(None, Some(100 * MIB), None);
        s.set_fs(Some(FsKind::ExFat));
        s.label = " Photos ".into();
        s.letter = Some('H');
        s.name = "photos".into();
        let ops = s.build().unwrap();
        assert_eq!(
            ops,
            vec![Operation::CreatePartition {
                disk: 1,
                start: 801 * MIB,
                len: 100 * MIB,
                ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
                fs: Some(FsKind::ExFat),
                label: "Photos".into(),
                letter: Some('H'),
                name: "photos".into(),
            }]
        );
        assert!(check_ops(&m, &ops).is_ok());
        assert_eq!(wizard(s).build_ops().unwrap(), ops);
    }

    #[test]
    fn create_validation() {
        let m = model_of(vec![gpt_disk(1)]);
        let mut s = CreateState::initial(&gpt_free_ctx(&m)).unwrap();
        s.set_fs(Some(FsKind::Fat32));
        s.label = "TWELVE_CHARS".into();
        assert!(s.build().unwrap_err().contains("11 characters"));
        s.label.clear();
        s.letter = Some('E');
        assert!(s.build().unwrap_err().contains("not free"));
        s.letter = None;
        s.bar.len = 40 * GIB; // as if the range were that large
        assert!(s.build().unwrap_err().contains("FAT32"));
    }

    #[test]
    fn mbr_types_follow_the_filesystem_and_free_range_is_preselected() {
        let m = model_of(vec![mbr_disk(0)]);
        // the free range after the extended container, 700 MiB .. 1 GiB
        let ctx = WizardCtx::new(&m, vec![], Selected::Free(0, 700 * MIB, 324 * MIB));
        let mut s = CreateState::initial(&ctx).unwrap();
        assert_eq!(s.ranges, [(201 * MIB, 99 * MIB), (700 * MIB, 324 * MIB)]);
        assert_eq!((s.bar.start, s.bar.len), (700 * MIB, 324 * MIB));
        assert_eq!(s.ptype, PartitionType::Mbr(0x07));
        s.set_fs(Some(FsKind::Fat32));
        assert_eq!(s.ptype, PartitionType::Mbr(0x0C));
        s.name = "ignored".into();
        let Operation::CreatePartition { name, letter, .. } = &s.build().unwrap()[0] else { panic!() };
        assert!(name.is_empty() && letter.is_none());
        // uninitialized disks are not eligible
        let mut raw = gpt_disk(2);
        raw.partitions.clear();
        raw.table = TableKind::None;
        let m2 = model_of(vec![raw]);
        assert!(CreateState::initial(&WizardCtx::new(&m2, vec![], Selected::Disk(2))).is_none());
    }
}
