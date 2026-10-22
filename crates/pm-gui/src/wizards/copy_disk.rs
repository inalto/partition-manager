//! Copy Disk wizard: Source disk → Target disk (every disk but the source and the running
//! system disk; everything on it is erased) → Copy options (sizing Fit / Proportional /
//! Exact, Smart / sector-by-sector, 1 MB alignment, the partition table of the copy, boot
//! fix) → Preview (the source bar and the planned target bar, and which source volumes will
//! be shrunk) → summary → [`Operation::CloneDisk`].
//!
//! The pickers, option groups and the preview are shared with the Migrate OS wizard.

use super::{disk_preview, row};
use crate::tools::WARN;
use crate::wizard::{check_ops, shared, StepResult, Wizard, WizardCtx};
use eframe::egui::{self, RichText};
use pm_core::{format_size, DiskInfo, TableKind, GIB};
use pm_ops::{BootFix, CloneMode, Model, Operation, Planner, Sizing};

const TIB: u64 = 1024 * GIB;

/// Disks that can be copied: initialized, with partitions.
pub fn source_disks(m: &Model) -> Vec<u32> {
    m.disks
        .iter()
        .filter(|d| matches!(d.table, TableKind::Gpt | TableKind::Mbr) && !d.partitions.is_empty())
        .map(|d| d.index)
        .collect()
}

/// Disks a copy of `src` may be written to: not the source, not the running system disk.
pub fn target_disks(m: &Model, src: u32, system: Option<u32>) -> Vec<u32> {
    m.disks.iter().map(|d| d.index).filter(|&i| i != src && Some(i) != system).collect()
}

/// UEFI repair when `d` has an EFI system partition, BIOS for an MBR disk, else `fallback`.
pub fn default_boot_fix(d: &DiskInfo, fallback: BootFix) -> BootFix {
    if d.partitions.iter().any(|p| p.ptype.is_some_and(|t| t.is_efi())) {
        BootFix::Uefi
    } else if d.table == TableKind::Mbr {
        BootFix::Bios
    } else {
        fallback
    }
}

pub fn sizing_name(s: Sizing) -> &'static str {
    match s {
        Sizing::Fit => "Fit partitions to the target",
        Sizing::Proportional => "Fit partitions to entire disk (proportional)",
        Sizing::Exact => "Copy partitions without resize (exact)",
    }
}

pub fn boot_fix_name(b: BootFix) -> &'static str {
    match b {
        BootFix::None => "None",
        BootFix::Uefi => "UEFI (bcdboot)",
        BootFix::Bios => "BIOS (boot sector)",
        BootFix::All => "UEFI and BIOS",
    }
}

/// The partition table kind a copy of `src` gets by default: GPT when the source is MBR
/// and either the target is larger than an MBR can address (2 TiB) or the source boots
/// through an EFI partition (a UEFI machine); otherwise the source's kind (`None`).
pub fn default_target_table(src: &DiskInfo, dst: &DiskInfo) -> Option<TableKind> {
    let efi = src.partitions.iter().any(|p| p.ptype.is_some_and(|t| t.is_efi()));
    (src.table == TableKind::Mbr && (dst.size > 2 * TIB || efi)).then_some(TableKind::Gpt)
}

/// The kind the target gets: `choice`, else the source's.
pub fn effective_table(src: &DiskInfo, choice: Option<TableKind>) -> TableKind {
    choice.unwrap_or(src.table)
}

pub fn table_kind_name(k: TableKind) -> &'static str {
    match k {
        TableKind::Gpt => "GPT",
        TableKind::Mbr => "MBR",
        _ => "unknown",
    }
}

/// `"Same as the source (GPT)"`, `"GPT"` or `"MBR"`.
pub fn table_choice_name(src: &DiskInfo, choice: Option<TableKind>) -> String {
    match choice {
        None => format!("Same as the source ({})", table_kind_name(src.table)),
        Some(k) => table_kind_name(k).to_string(),
    }
}

/// An MBR copy on a target larger than 2 TiB cannot use the space past 2 TiB.
pub fn mbr_size_note(kind: TableKind, dst: &DiskInfo) -> Option<String> {
    (kind == TableKind::Mbr && dst.size > 2 * TIB).then(|| {
        format!(
            "An MBR partition table cannot address more than 2 TB of the {} target; choose GPT \
             to use all of it.",
            format_size(dst.size)
        )
    })
}

/// The partition-table picker of a copy: the source's kind, GPT or MBR, with the size note
/// and what a conversion does.
pub fn table_ui(ui: &mut egui::Ui, m: &Model, src: u32, dst: Option<u32>, choice: &mut Option<TableKind>) {
    let Some(s) = m.disk(src) else { return };
    ui.horizontal(|ui| {
        ui.label("Partition table on the target:");
        egui::ComboBox::from_id_salt("target_table").selected_text(table_choice_name(s, *choice)).show_ui(ui, |ui| {
            for c in [None, Some(TableKind::Gpt), Some(TableKind::Mbr)] {
                ui.selectable_value(choice, c, table_choice_name(s, c));
            }
        });
    });
    let kind = effective_table(s, *choice);
    if kind != s.table {
        ui.indent("table_help", |ui| {
            ui.weak(match kind {
                TableKind::Gpt => "Converted while copying: partition types are mapped and logical partitions become ordinary GPT partitions.",
                _ => "Converted while copying: partition types are mapped, the Microsoft reserved partition is dropped and the Windows partition is made active; at most 4 partitions fit.",
            })
        });
    }
    if let Some(note) = dst.and_then(|d| m.disk(d)).and_then(|d| mbr_size_note(kind, d)) {
        ui.colored_label(WARN, note);
    }
}

pub fn mode_name(m: CloneMode) -> &'static str {
    match m {
        CloneMode::Smart => "Smart (used sectors only)",
        CloneMode::Sector => "Sector by sector",
    }
}

fn disk_name(m: &Model, i: u32) -> String {
    m.disk(i).map_or(format!("Disk {i}"), |d| d.display_name())
}

pub struct CopyDiskState {
    pub src: u32,
    pub dst: Option<u32>,
    pub sizing: Sizing,
    pub mode: CloneMode,
    pub boot_fix: BootFix,
    /// The copy's partition table kind; `None` keeps the source's.
    pub target_table: Option<TableKind>,
}

impl CopyDiskState {
    /// The selected disk (when it can be copied), else the first that can.
    pub fn initial(ctx: &WizardCtx) -> Option<CopyDiskState> {
        let sources = source_disks(ctx.model);
        let src = ctx.selection.disk().filter(|d| sources.contains(d)).or_else(|| sources.first().copied())?;
        let boot_fix = boot_fix_for(ctx.os_name, default_boot_fix(ctx.model.disk(src)?, BootFix::None));
        Some(CopyDiskState { src, dst: None, sizing: Sizing::Fit, mode: CloneMode::Smart, boot_fix, target_table: None })
    }

    pub fn op(&self) -> Option<Operation> {
        Some(Operation::CloneDisk {
            src_disk: self.src,
            dst_disk: self.dst?,
            sizing: self.sizing,
            mode: self.mode,
            boot_fix: self.boot_fix,
            target_table: self.target_table,
        })
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        self.op().map(|o| vec![o]).ok_or_else(|| "Choose a target disk.".to_string())
    }
}

/// Radio list of `disks`; returns the new choice.
pub fn disk_radio(ui: &mut egui::Ui, m: &Model, disks: &[u32], choice: Option<u32>) -> Option<u32> {
    let mut pick = choice;
    egui::Grid::new(ui.next_auto_id()).num_columns(3).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
        ui.strong("Disk");
        ui.strong("Model");
        ui.strong("Size");
        ui.end_row();
        for &i in disks {
            let Some(d) = m.disk(i) else { continue };
            ui.radio_value(&mut pick, Some(i), format!("Disk {i}"));
            ui.label(&d.model);
            ui.label(format_size(d.size));
            ui.end_row();
        }
    });
    pick
}

/// The target picker: every eligible disk, the chosen one's bar and the erase warning.
pub fn target_ui(ui: &mut egui::Ui, m: &Model, src: u32, system: Option<u32>, dst: &mut Option<u32>) -> StepResult {
    let targets = target_disks(m, src, system);
    if targets.is_empty() {
        return StepResult::Invalid("There is no other disk to copy to.".into());
    }
    ui.label("Select the disk to copy to:");
    *dst = disk_radio(ui, m, &targets, *dst).filter(|d| targets.contains(d));
    if let Some(d) = dst.and_then(|i| m.disk(i)) {
        ui.add_space(8.0);
        disk_preview(ui, d, None);
        ui.add_space(4.0);
        super::danger(ui, format!("All data on Disk {} will be erased.", d.index));
    }
    if system.is_some() {
        ui.weak("The running system disk is not offered as a target.");
    }
    StepResult::from_problem(dst.is_none().then(|| "Choose a target disk.".to_string()))
}

/// Sizing radios with one-line explanations.
pub fn sizing_ui(ui: &mut egui::Ui, sizing: &mut Sizing) {
    ui.label(RichText::new("Copy options").strong());
    for (s, help) in [
        (Sizing::Fit, "Shrink to the used data on a smaller disk; on a larger one the largest data partition grows."),
        (Sizing::Proportional, "Scale the data partitions by the size of the target disk (system partitions keep their size)."),
        (Sizing::Exact, "Same positions and sizes as the source; the target must be at least as large."),
    ] {
        ui.radio_value(sizing, s, sizing_name(s));
        ui.indent(("sizing_help", sizing_name(s)), |ui| ui.weak(help));
    }
    let mut align = true;
    ui.add_enabled(false, egui::Checkbox::new(&mut align, "Align partitions to 1 MB"))
        .on_disabled_hover_text("Partitions are always laid out on 1 MB boundaries");
}

/// Shown instead of the boot repair choice on Linux and macOS.
pub const BOOT_REPAIR_WINDOWS_ONLY: &str =
    "Boot repair is Windows-only; the clone will not be made bootable from this host.";

/// `wanted` where boot repair can run (`os`, `Platform::os_name`: Windows), else none.
pub fn boot_fix_for(os: &str, wanted: BootFix) -> BootFix {
    if os == "windows" {
        wanted
    } else {
        BootFix::None
    }
}

pub fn boot_fix_ui(ui: &mut egui::Ui, boot_fix: &mut BootFix, os: &str) {
    let available = os == "windows";
    if !available {
        *boot_fix = BootFix::None;
    }
    ui.horizontal(|ui| {
        ui.label("Boot repair on the target:");
        ui.add_enabled_ui(available, |ui| {
            egui::ComboBox::from_id_salt("boot_fix").selected_text(boot_fix_name(*boot_fix)).show_ui(ui, |ui| {
                for b in [BootFix::None, BootFix::Uefi, BootFix::Bios, BootFix::All] {
                    ui.selectable_value(boot_fix, b, boot_fix_name(b));
                }
            });
        });
    });
    if !available {
        ui.weak(BOOT_REPAIR_WINDOWS_ONLY);
    }
}

/// The source bar, the planned target bar and the shrink warnings of `op`; the planner's
/// refusal, if any, as the step result.
pub fn preview_ui(ui: &mut egui::Ui, m: &Model, op: &Operation, src: u32, dst: u32) -> StepResult {
    if let Some(d) = m.disk(src) {
        ui.label(RichText::new(format!("Source: {}", d.display_name())).strong());
        disk_preview(ui, d, None);
    }
    ui.add_space(8.0);
    let planned = Planner::simulate(m, op).map_err(crate::wizard::error_text);
    match &planned {
        Ok(after) => {
            if let Some(d) = after.disk(dst) {
                ui.label(RichText::new(format!("Target after the copy: Disk {dst} ({})", format_size(d.size))).strong());
                disk_preview(ui, d, None);
            }
        }
        Err(_) => {
            ui.label(format!("Target: {}", disk_name(m, dst)));
        }
    }
    let shrinks = op.shrinks_source(m);
    if !shrinks.is_empty() {
        ui.add_space(8.0);
        ui.colored_label(WARN, format!("{} The source will be shrunk first:", crate::views::icons::WARNING));
        for s in shrinks {
            ui.colored_label(WARN, format!("• {s}"));
        }
    }
    StepResult::from_problem(planned.err().or_else(|| check_ops(m, std::slice::from_ref(op)).err()))
}

fn source_ui(s: &mut CopyDiskState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    let sources = source_disks(ctx.model);
    ui.label("Select the disk to copy:");
    if let Some(src) = disk_radio(ui, ctx.model, &sources, Some(s.src)) {
        if src != s.src {
            s.src = src;
            if s.dst == Some(src) {
                s.dst = None;
            }
            if let Some(d) = ctx.model.disk(src) {
                s.boot_fix = boot_fix_for(ctx.os_name, default_boot_fix(d, BootFix::None));
            }
        }
    }
    if let Some(d) = ctx.model.disk(s.src) {
        ui.add_space(8.0);
        disk_preview(ui, d, None);
    }
    StepResult::Continue
}

fn target_step(s: &mut CopyDiskState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    let before = s.dst;
    let r = target_ui(ui, ctx.model, s.src, ctx.system_disk, &mut s.dst);
    if s.dst != before {
        if let (Some(src), Some(dst)) = (ctx.model.disk(s.src), s.dst.and_then(|d| ctx.model.disk(d))) {
            s.target_table = default_target_table(src, dst);
        }
    }
    r
}

fn options_ui(s: &mut CopyDiskState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    sizing_ui(ui, &mut s.sizing);
    ui.add_space(8.0);
    ui.label(RichText::new("Copy mode").strong());
    ui.radio_value(&mut s.mode, CloneMode::Smart, mode_name(CloneMode::Smart));
    ui.radio_value(&mut s.mode, CloneMode::Sector, mode_name(CloneMode::Sector));
    ui.add_space(8.0);
    table_ui(ui, ctx.model, s.src, s.dst, &mut s.target_table);
    ui.add_space(8.0);
    boot_fix_ui(ui, &mut s.boot_fix, ctx.os_name);
    StepResult::Continue
}

fn preview_step(s: &mut CopyDiskState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    match (s.op(), s.dst) {
        (Some(op), Some(dst)) => preview_ui(ui, ctx.model, &op, s.src, dst),
        _ => StepResult::Invalid("Choose a target disk.".into()),
    }
}

fn source_summary(s: &CopyDiskState) -> Vec<(String, String)> {
    vec![row("Source", format!("Disk {}", s.src))]
}

fn target_summary(s: &CopyDiskState) -> Vec<(String, String)> {
    vec![row("Target", s.dst.map_or("-".into(), |d| format!("Disk {d} (all data erased)")))]
}

fn options_summary(s: &CopyDiskState) -> Vec<(String, String)> {
    vec![
        row("Sizing", sizing_name(s.sizing)),
        row("Copy mode", mode_name(s.mode)),
        row("Alignment", "1 MB"),
        row("Partition table", s.target_table.map_or("same as the source", table_kind_name)),
        row("Boot repair", boot_fix_name(s.boot_fix)),
    ]
}

fn no_summary(_: &CopyDiskState) -> Vec<(String, String)> {
    Vec::new()
}

pub fn wizard(state: CopyDiskState) -> Wizard {
    Wizard::over_state(
        "Copy Disk",
        shared(state),
        vec![
            ("Source disk", source_ui, source_summary),
            ("Target disk", target_step, target_summary),
            ("Copy options", options_ui, options_summary),
            ("Preview", preview_step, no_summary),
        ],
        CopyDiskState::build,
    )
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    CopyDiskState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Selected;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};
    use pm_core::MIB;

    fn model() -> Model {
        let mut big = gpt_disk(2);
        big.partitions.clear();
        big.size = 2048 * MIB;
        big.model = "Big SSD".into();
        let mut m = model_of(vec![gpt_disk(1), big, mbr_disk(0)]);
        m.system_disk = Some(0);
        m
    }

    #[test]
    fn build_clone_disk_op_from_filled_state() {
        let m = model();
        let mut ctx = WizardCtx::new(&m, vec![], Selected::Disk(1));
        // boot repair runs on Windows only
        ctx.os_name = "linux";
        assert_eq!(CopyDiskState::initial(&ctx).unwrap().boot_fix, BootFix::None);
        ctx.os_name = "windows";
        let mut s = CopyDiskState::initial(&ctx).unwrap();
        assert_eq!((s.src, s.dst, s.boot_fix), (1, None, BootFix::Uefi), "EFI source: UEFI repair");
        assert!(s.build().unwrap_err().contains("target"));
        assert_eq!(target_disks(&m, 1, ctx.system_disk), [2], "not the source, not the system disk");
        s.dst = Some(2);
        s.sizing = Sizing::Proportional;
        s.mode = CloneMode::Sector;
        let ops = s.build().unwrap();
        assert_eq!(
            ops,
            vec![Operation::CloneDisk {
                src_disk: 1,
                dst_disk: 2,
                sizing: Sizing::Proportional,
                mode: CloneMode::Sector,
                boot_fix: BootFix::Uefi,
                target_table: None,
            }]
        );
        let after = check_ops(&m, &ops).unwrap();
        assert_eq!(after.disk(2).unwrap().partitions.len(), 3);
        assert_eq!(wizard(s).build_ops().unwrap(), ops);

        // the MBR disk copied as GPT: the option reaches the operation and the planner
        let ctx = WizardCtx::new(&m, vec![], Selected::Disk(0));
        let mut s = CopyDiskState::initial(&ctx).unwrap();
        s.dst = Some(2);
        s.target_table = Some(TableKind::Gpt);
        let ops = s.build().unwrap();
        assert!(matches!(ops[0], Operation::CloneDisk { src_disk: 0, target_table: Some(TableKind::Gpt), .. }));
        let after = check_ops(&m, &ops).unwrap();
        let d2 = after.disk(2).unwrap();
        assert_eq!(d2.table, TableKind::Gpt);
        assert_eq!(d2.partitions.len(), 3, "primary and the two logicals, no container");
        assert_eq!(options_summary(&s)[3], row("Partition table", "GPT"));
    }

    #[test]
    fn target_table_defaults_to_gpt_for_mbr_sources_that_need_it() {
        let m = model();
        let (mbr, gpt) = (m.disk(0).unwrap(), m.disk(1).unwrap());
        let mut big = gpt.clone();
        big.size = 3 * TIB;
        // small target, BIOS source: keep MBR
        assert_eq!(default_target_table(mbr, gpt), None);
        // past 2 TiB: GPT
        assert_eq!(default_target_table(mbr, &big), Some(TableKind::Gpt));
        // an MBR source with an EFI partition (UEFI machine): GPT
        let mut uefi = mbr.clone();
        uefi.partitions[0].ptype = Some(pm_core::PartitionType::Mbr(0xEF));
        assert_eq!(default_target_table(&uefi, gpt), Some(TableKind::Gpt));
        // a GPT source stays GPT
        assert_eq!(default_target_table(gpt, &big), None);

        assert_eq!(effective_table(mbr, None), TableKind::Mbr);
        assert_eq!(effective_table(mbr, Some(TableKind::Gpt)), TableKind::Gpt);
        assert_eq!(table_choice_name(mbr, None), "Same as the source (MBR)");
        assert!(mbr_size_note(TableKind::Mbr, &big).unwrap().contains("GPT"));
        assert!(mbr_size_note(TableKind::Gpt, &big).is_none());
        assert!(mbr_size_note(TableKind::Mbr, gpt).is_none());

        // the wizard applies the default when the target is chosen (target_step); the
        // initial state has no target and keeps the source's kind
        let ctx = WizardCtx::new(&m, vec![], Selected::Disk(0));
        let s = CopyDiskState::initial(&ctx).unwrap();
        assert_eq!((s.dst, s.target_table), (None, None));
    }

    #[test]
    fn copy_disk_defaults() {
        let m = model();
        assert_eq!(source_disks(&m), [1, 0], "the empty disk cannot be copied");
        assert_eq!(default_boot_fix(m.disk(0).unwrap(), BootFix::None), BootFix::Bios);
        assert_eq!(boot_fix_for("windows", BootFix::Bios), BootFix::Bios);
        assert_eq!(boot_fix_for("macos", BootFix::Bios), BootFix::None);
        let ctx = WizardCtx::new(&m, vec![], Selected::Disk(2));
        assert_eq!(CopyDiskState::initial(&ctx).unwrap().src, 1, "falls back to the first copyable disk");
    }

    /// Every Task 20 operation wizard renders each page headless.
    #[test]
    fn clone_and_image_wizards_render_every_page() {
        let m = model();
        let ctx = egui::Context::default();
        let wctx = WizardCtx::new(&m, vec![], Selected::Partition(1, 3));
        let mut ws = vec![
            super::super::copy_partition::new(&wctx).unwrap(),
            super::super::image_create::new(&wctx).unwrap(),
            super::super::image_restore::new(&wctx).unwrap(),
        ];
        let dctx = WizardCtx::new(&m, vec![], Selected::Disk(1));
        ws.push(new(&dctx).unwrap());
        ws.push(super::super::migrate_os::new(&dctx).unwrap());
        for a in super::super::CLONE_ACTIONS {
            assert!(super::super::clone_for_action(a, &dctx).is_some(), "{a:?}");
        }
        for w in &mut ws {
            for page in 0..=w.step_count() {
                w.goto(page);
                for dark in [false, true] {
                    let _ = ctx.run(egui::RawInput::default(), |ctx| {
                        ctx.set_visuals(if dark { egui::Visuals::dark() } else { egui::Visuals::light() });
                        w.show(ctx, &dctx);
                    });
                }
            }
        }
    }
}
