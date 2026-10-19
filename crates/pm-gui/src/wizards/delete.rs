//! Delete Partition wizard: one confirmation page with the partition's details and a red
//! warning → [`Operation::DeletePartition`].
//!
//! The sidebar's Delete action queues the deletion directly (it is a pending operation that
//! Undo reverts, as in MiniTool); this wizard is the confirming variant for callers that want
//! one (`wizards::for_action(Action::DeletePartition, ..)`).

use super::{danger, row};
use crate::format_util::{fs_text, partition_name, type_text};
use crate::items::is_container;
use crate::selection::Selected;
use crate::wizard::{shared, StepResult, Wizard, WizardCtx};
use eframe::egui;
use pm_core::format_size;
use pm_ops::Operation;

pub struct DeleteState {
    pub disk: u32,
    pub number: u32,
    /// Key / value details of the partition, shown on the page and in the summary.
    pub details: Vec<(String, String)>,
}

impl DeleteState {
    pub fn initial(ctx: &WizardCtx) -> Option<DeleteState> {
        let Selected::Partition(disk, number) = ctx.selection else { return None };
        let d = ctx.model.disk(disk)?;
        let p = ctx.model.partition(disk, number).filter(|p| !is_container(d, p))?;
        let details = vec![
            row("Partition", format!("{} (number {number}) on Disk {disk}", partition_name(p))),
            row("Size", format_size(p.len)),
            row("Used", p.used.map_or("-".to_string(), format_size)),
            row("File system", fs_text(p)),
            row("Type", type_text(p)),
        ];
        Some(DeleteState { disk, number, details })
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        Ok(vec![Operation::DeletePartition { disk: self.disk, number: self.number }])
    }
}

fn confirm_ui(s: &mut DeleteState, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
    egui::Grid::new("delete_details").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
        for (k, v) in &s.details {
            ui.label(k);
            ui.label(v);
            ui.end_row();
        }
    });
    ui.add_space(8.0);
    danger(ui, "Deleting the partition destroys all data on it once you press Apply.");
    StepResult::Continue
}

fn summary(s: &DeleteState) -> Vec<(String, String)> {
    s.details.clone()
}

pub fn wizard(state: DeleteState) -> Wizard {
    Wizard::over_state("Delete Partition", shared(state), vec![("Confirm", confirm_ui, summary)], DeleteState::build)
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    DeleteState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};

    #[test]
    fn build_delete_op() {
        let m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        let s = DeleteState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 3))).unwrap();
        assert_eq!(s.details[0].1, "E:Data (number 3) on Disk 1");
        assert_eq!(s.build().unwrap(), vec![Operation::DeletePartition { disk: 1, number: 3 }]);
        assert_eq!(wizard(s).build_ops().unwrap(), vec![Operation::DeletePartition { disk: 1, number: 3 }]);
        // the extended container is not offered
        assert!(DeleteState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(0, 2))).is_none());
    }
}
