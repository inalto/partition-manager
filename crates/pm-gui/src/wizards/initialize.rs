//! Initialize Disk wizard: GPT (recommended) or MBR (≤ 2 TB) for a disk without a partition
//! table → [`Operation::InitializeDisk`].

use super::convert::{fits_mbr, table_name};
use super::row;
use crate::wizard::{check_ops, shared, StepResult, Wizard, WizardCtx};
use eframe::egui::{self, RichText};
use pm_core::TableKind;
use pm_ops::Operation;

pub struct InitializeState {
    pub disk: u32,
    pub desc: String,
    pub kind: TableKind,
    /// The disk is small enough for MBR.
    pub mbr_ok: bool,
}

impl InitializeState {
    pub fn initial(ctx: &WizardCtx) -> Option<InitializeState> {
        let disk = ctx.selection.disk()?;
        let d = ctx.model.disk(disk)?;
        if !matches!(d.table, TableKind::None | TableKind::Unknown) {
            return None;
        }
        Some(InitializeState { disk, desc: d.display_name(), kind: TableKind::Gpt, mbr_ok: fits_mbr(d) })
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        if self.kind == TableKind::Mbr && !self.mbr_ok {
            return Err("The disk is larger than 2 TB: MBR cannot address it. Use GPT.".into());
        }
        Ok(vec![Operation::InitializeDisk { disk: self.disk, kind: self.kind }])
    }
}

fn step_ui(s: &mut InitializeState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    ui.label(&s.desc);
    ui.add_space(6.0);
    ui.radio_value(&mut s.kind, TableKind::Gpt, RichText::new("GPT (recommended)").strong());
    ui.weak("    Any disk size, up to 128 partitions; required for UEFI boot.");
    ui.add_enabled_ui(s.mbr_ok, |ui| ui.radio_value(&mut s.kind, TableKind::Mbr, "MBR"))
        .response
        .on_disabled_hover_text("MBR cannot address disks larger than 2 TB");
    ui.weak("    Up to 2 TB and 4 primary partitions; for old BIOS-only computers.");
    let result = s.build().and_then(|ops| check_ops(ctx.model, &ops));
    StepResult::from_problem(result.err())
}

fn summary(s: &InitializeState) -> Vec<(String, String)> {
    vec![row("Disk", s.desc.clone()), row("Partition table", table_name(s.kind))]
}

pub fn wizard(state: InitializeState) -> Wizard {
    Wizard::over_state("Initialize Disk", shared(state), vec![("Partition table", step_ui, summary)], InitializeState::build)
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    InitializeState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Selected;
    use crate::test_util::{gpt_disk, model_of};
    use pm_core::GIB;

    fn blank(size: u64) -> pm_core::DiskInfo {
        let mut d = gpt_disk(2);
        d.partitions.clear();
        d.table = TableKind::None;
        d.size = size;
        d
    }

    #[test]
    fn build_initialize_ops() {
        let mut m = model_of(vec![blank(GIB), gpt_disk(1)]);
        m.system_disk = Some(1);
        let mut s = InitializeState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(2))).unwrap();
        assert_eq!(s.kind, TableKind::Gpt, "GPT is the default");
        assert_eq!(s.build().unwrap(), vec![Operation::InitializeDisk { disk: 2, kind: TableKind::Gpt }]);
        s.kind = TableKind::Mbr;
        let ops = s.build().unwrap();
        assert!(check_ops(&m, &ops).is_ok());
        assert_eq!(wizard(s).build_ops().unwrap(), vec![Operation::InitializeDisk { disk: 2, kind: TableKind::Mbr }]);
        assert!(InitializeState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(1))).is_none(), "already GPT");
        // too large for MBR
        let m = model_of(vec![blank(3 * 1024 * GIB)]);
        let mut s = InitializeState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(2))).unwrap();
        s.kind = TableKind::Mbr;
        assert!(s.build().unwrap_err().contains("2 TB"));
    }
}
