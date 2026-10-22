//! Migrate OS wizard (MiniTool screenshot 4): "How to migrate?" — option A replaces the
//! system disk (copies every partition), option B moves the OS and keeps the original
//! (copies only the partitions the system needs) → Target disk → Options (sizing, 1 MB
//! alignment, the partition table of the copy — GPT by default for an MBR source on a
//! UEFI machine or a target past 2 TiB —, keeping the source's ids, boot repair) →
//! Preview (bars, shrink warning, the firmware boot-order note) → summary →
//! [`Operation::MigrateOs`].

use super::copy_disk::{
    boot_fix_name, boot_fix_ui, default_target_table, effective_table, preview_ui, sizing_name, sizing_ui,
    table_kind_name, table_ui, target_ui,
};
use super::{disk_preview, row};
use crate::wizard::{shared, StepResult, Wizard, WizardCtx};
use eframe::egui::{self, RichText};
use pm_core::{DiskInfo, TableKind};
use pm_ops::{BootFix, Model, Operation, Sizing};

/// The disk the OS is migrated from: the running system disk, else the selected disk, else
/// the first disk holding a Windows partition.
pub fn source_disk(m: &Model, selected: Option<u32>) -> Option<u32> {
    let ok = |i: u32| m.disk(i).is_some_and(|d| matches!(d.table, TableKind::Gpt | TableKind::Mbr) && !d.partitions.is_empty());
    m.effective_system_disk()
        .filter(|&i| ok(i))
        .or_else(|| selected.filter(|&i| ok(i)))
        .or_else(|| m.disks.iter().find(|d| ok(d.index) && m.windows_partition_of(d.index).is_some()).map(|d| d.index))
        .or_else(|| m.disks.iter().find(|d| ok(d.index)).map(|d| d.index))
}

/// UEFI repair for an EFI source, both otherwise.
pub fn default_boot_fix(d: &DiskInfo) -> BootFix {
    if d.partitions.iter().any(|p| p.ptype.is_some_and(|t| t.is_efi())) {
        BootFix::Uefi
    } else {
        BootFix::All
    }
}

pub const KEEP_IDS_LABEL: &str = "Keep the disk and partition ids of the source (detach the source before the first boot)";
pub const KEEP_IDS_HELP: &str = "Windows maps drive letters to disk and partition ids (MountedDevices). With new ids \
the copy may come up with C: pointing at the old disk; with the source's ids it boots exactly like the \
source — but Windows takes a disk with a duplicate id offline, so the source must be detached before \
the copy starts. Only possible when the copy keeps the source's partition table kind.";

pub struct MigrateState {
    pub src: u32,
    /// Option B: only the partitions required by the system.
    pub only_system: bool,
    pub dst: Option<u32>,
    pub sizing: Sizing,
    pub boot_fix: BootFix,
    /// The copy's partition table kind; `None` keeps the source's.
    pub target_table: Option<TableKind>,
    /// Keep the source's disk and partition ids on the copy.
    pub keep_ids: bool,
}

impl MigrateState {
    pub fn initial(ctx: &WizardCtx) -> Option<MigrateState> {
        let src = source_disk(ctx.model, ctx.selection.disk())?;
        // boot repair (bcdboot / bootsect) runs on Windows only
        let boot_fix = super::copy_disk::boot_fix_for(ctx.os_name, default_boot_fix(ctx.model.disk(src)?));
        Some(MigrateState {
            src,
            only_system: false,
            dst: None,
            sizing: Sizing::Fit,
            boot_fix,
            target_table: None,
            keep_ids: false,
        })
    }

    /// True while the copy keeps the source's table kind (ids can only be kept then).
    pub fn can_keep_ids(&self, m: &Model) -> bool {
        m.disk(self.src).is_some_and(|d| effective_table(d, self.target_table) == d.table)
    }

    pub fn op(&self) -> Option<Operation> {
        Some(Operation::MigrateOs {
            src_disk: self.src,
            dst_disk: self.dst?,
            only_system_parts: self.only_system,
            sizing: self.sizing,
            boot_fix: self.boot_fix,
            target_table: self.target_table,
            keep_ids: self.keep_ids,
        })
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        self.op().map(|o| vec![o]).ok_or_else(|| "Choose a target disk.".to_string())
    }
}

fn how_ui(s: &mut MigrateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    if let Some(d) = ctx.model.disk(s.src) {
        ui.label(format!("Migrating the system on {}", d.display_name()));
        disk_preview(ui, d, None);
        ui.add_space(10.0);
    }
    ui.label(RichText::new("How to migrate?").strong());
    ui.radio_value(&mut s.only_system, false, "A: Replace the system disk with a new disk");
    ui.indent("opt_a", |ui| ui.weak("Copies all partitions of the system disk to the new disk."));
    ui.radio_value(&mut s.only_system, true, "B: Move my OS to another disk and keep the original");
    ui.indent("opt_b", |ui| ui.weak("Copies only the partitions required by the system (EFI, MSR, Windows, recovery)."));
    StepResult::Continue
}

fn target_step(s: &mut MigrateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    let before = s.dst;
    let r = target_ui(ui, ctx.model, s.src, ctx.system_disk, &mut s.dst);
    if s.dst != before {
        if let (Some(src), Some(dst)) = (ctx.model.disk(s.src), s.dst.and_then(|d| ctx.model.disk(d))) {
            s.target_table = default_target_table(src, dst);
        }
    }
    r
}

fn options_ui(s: &mut MigrateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    sizing_ui(ui, &mut s.sizing);
    ui.add_space(8.0);
    table_ui(ui, ctx.model, s.src, s.dst, &mut s.target_table);
    let can_keep = s.can_keep_ids(ctx.model);
    if !can_keep {
        s.keep_ids = false;
    }
    ui.add_enabled(can_keep, egui::Checkbox::new(&mut s.keep_ids, KEEP_IDS_LABEL))
        .on_hover_text(KEEP_IDS_HELP)
        .on_disabled_hover_text("A copy with another partition table kind gets new ids.");
    ui.add_space(8.0);
    boot_fix_ui(ui, &mut s.boot_fix, ctx.os_name);
    StepResult::Continue
}

fn preview_step(s: &mut MigrateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    let r = match (s.op(), s.dst) {
        (Some(op), Some(dst)) => preview_ui(ui, ctx.model, &op, s.src, dst),
        _ => return StepResult::Invalid("Choose a target disk.".into()),
    };
    ui.add_space(8.0);
    ui.label(RichText::new("After the migration, set the new disk as the first boot device in the firmware (BIOS/UEFI) settings.").strong());
    r
}

fn how_summary(s: &MigrateState) -> Vec<(String, String)> {
    vec![
        row("Source", format!("Disk {}", s.src)),
        row(
            "Migrate",
            if s.only_system { "B: only the partitions required by the system" } else { "A: all partitions" },
        ),
    ]
}

fn target_summary(s: &MigrateState) -> Vec<(String, String)> {
    vec![row("Target", s.dst.map_or("-".into(), |d| format!("Disk {d} (all data erased)")))]
}

fn options_summary(s: &MigrateState) -> Vec<(String, String)> {
    vec![
        row("Sizing", sizing_name(s.sizing)),
        row("Alignment", "1 MB"),
        row("Partition table", s.target_table.map_or("same as the source", table_kind_name)),
        row("Disk and partition ids", if s.keep_ids { "kept from the source (detach it before the first boot)" } else { "new" }),
        row("Boot repair", boot_fix_name(s.boot_fix)),
    ]
}

fn no_summary(_: &MigrateState) -> Vec<(String, String)> {
    Vec::new()
}

pub fn wizard(state: MigrateState) -> Wizard {
    Wizard::over_state(
        "Migrate OS to SSD/HD",
        shared(state),
        vec![
            ("How to migrate", how_ui, how_summary),
            ("Target disk", target_step, target_summary),
            ("Options", options_ui, options_summary),
            ("Preview", preview_step, no_summary),
        ],
        MigrateState::build,
    )
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    MigrateState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Selected;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};
    use crate::wizard::check_ops;
    use pm_core::MIB;

    #[test]
    fn build_migrate_op_from_filled_state() {
        let mut sys = gpt_disk(1);
        sys.partitions[2].mount_points = vec!["C:\\".into()];
        let mut new_ssd = gpt_disk(2);
        new_ssd.partitions.clear();
        new_ssd.size = 4096 * MIB;
        let m = model_of(vec![sys, new_ssd, mbr_disk(0)]);
        let mut ctx = WizardCtx::new(&m, vec![], Selected::None);
        // from Linux / macOS the clone is not made bootable
        ctx.os_name = "macos";
        assert_eq!(MigrateState::initial(&ctx).unwrap().boot_fix, BootFix::None);
        ctx.os_name = "windows";
        let mut s = MigrateState::initial(&ctx).unwrap();
        assert_eq!(s.src, 1, "the disk holding C:");
        assert_eq!(s.boot_fix, BootFix::Uefi);
        s.only_system = true;
        s.dst = Some(2);
        s.sizing = Sizing::Exact;
        let ops = s.build().unwrap();
        assert_eq!(
            ops,
            vec![Operation::MigrateOs {
                src_disk: 1,
                dst_disk: 2,
                only_system_parts: true,
                sizing: Sizing::Exact,
                boot_fix: BootFix::Uefi,
                target_table: None,
                keep_ids: false,
            }]
        );
        let after = check_ops(&m, &ops).unwrap();
        assert_eq!(after.disk(2).unwrap().partitions.len(), 3, "EFI, MSR and the Windows partition");
        assert_eq!(wizard(s).build_ops().unwrap(), ops);
        // option A on the MBR disk: BIOS + UEFI repair by default
        assert_eq!(default_boot_fix(m.disk(0).unwrap()), BootFix::All);
    }

    #[test]
    fn migrate_target_table_and_keep_ids_reach_the_operation() {
        let mut sys = gpt_disk(1);
        sys.partitions[2].mount_points = vec!["C:\\".into()];
        sys.disk_id = "11111111-2222-3333-4444-555555555555".into();
        let mut new_ssd = gpt_disk(2);
        new_ssd.partitions.clear();
        new_ssd.size = 4096 * MIB;
        let m = model_of(vec![sys, new_ssd]);
        let ctx = WizardCtx::new(&m, vec![], Selected::None);
        let mut s = MigrateState::initial(&ctx).unwrap();
        s.dst = Some(2);
        assert!(s.can_keep_ids(&m), "same kind: ids can be kept");
        s.keep_ids = true;
        let ops = s.build().unwrap();
        assert!(matches!(ops[0], Operation::MigrateOs { keep_ids: true, target_table: None, .. }));
        let after = check_ops(&m, &ops).unwrap();
        assert_eq!(after.disk(2).unwrap().disk_id, m.disk(1).unwrap().disk_id, "the planner keeps the disk id");
        assert_eq!(options_summary(&s)[3].1, "kept from the source (detach it before the first boot)");

        // another table kind: the ids cannot be kept, the planner converts
        s.target_table = Some(TableKind::Mbr);
        assert!(!s.can_keep_ids(&m));
        s.keep_ids = false;
        let ops = s.build().unwrap();
        let after = check_ops(&m, &ops).unwrap();
        let d2 = after.disk(2).unwrap();
        assert_eq!(d2.table, TableKind::Mbr);
        assert_eq!(d2.partitions.len(), 2, "EFI and Windows: the MSR is dropped on MBR");
        assert_eq!(options_summary(&s)[2].1, "MBR");
        // keep-ids with a conversion is refused by the planner
        s.keep_ids = true;
        assert!(check_ops(&m, &s.build().unwrap()).unwrap_err().contains("same partition table"));
    }
}
