//! Wipe wizard: target (the disk, or the selected partition) and method (zeros, random data,
//! DoD 5220.22-M 3 passes) with a duration estimate, then a typed confirmation ("WIPE") →
//! [`Operation::WipeDisk`] / [`Operation::WipePartition`].

use super::{danger, partition_desc, row};
use crate::items::is_container;
use crate::selection::Selected;
use crate::wizard::{shared, FnStep, Shared, StepResult, Wizard, WizardCtx, WizardStep};
use eframe::egui;
use pm_core::{format_size, MIB};
use pm_ops::{Operation, WipeMethod};

/// What the user must type to confirm.
pub const CONFIRM_WORD: &str = "WIPE";

/// Write speed assumed for the duration estimate (a typical SATA SSD / fast HDD).
const ASSUMED_RATE: u64 = 150 * MIB;

pub fn passes(m: WipeMethod) -> u64 {
    match m {
        WipeMethod::Zero | WipeMethod::Random => 1,
        WipeMethod::Dod3 => 3,
    }
}

pub fn method_name(m: WipeMethod) -> &'static str {
    match m {
        WipeMethod::Zero => "Fill with zeros",
        WipeMethod::Random => "Fill with random data",
        WipeMethod::Dod3 => "DoD 5220.22-M (3 passes)",
    }
}

/// Rough duration of wiping `bytes` with `m`, e.g. `"about 12 min"`.
pub fn duration_hint(bytes: u64, m: WipeMethod) -> String {
    let secs = bytes.saturating_mul(passes(m)) / ASSUMED_RATE;
    match secs {
        0..=59 => "under a minute".to_string(),
        60..=3599 => format!("about {} min", secs.div_ceil(60)),
        _ => format!("about {} h {:02} min", secs / 3600, secs / 60 % 60),
    }
}

pub struct WipeState {
    pub disk: u32,
    /// The selected partition, when there is one: `(number, description, size)`.
    pub partition: Option<(u32, String, u64)>,
    /// Wipe the partition rather than the whole disk.
    pub target_partition: bool,
    pub disk_desc: String,
    pub disk_size: u64,
    pub system_disk: bool,
    pub method: WipeMethod,
    pub confirm: String,
}

impl WipeState {
    pub fn initial(ctx: &WizardCtx, partition: bool) -> Option<WipeState> {
        let disk = ctx.selection.disk()?;
        let d = ctx.model.disk(disk)?;
        let part = match ctx.selection {
            Selected::Partition(_, n) => {
                ctx.model.partition(disk, n).filter(|p| !is_container(d, p)).map(|p| (n, partition_desc(disk, p), p.len))
            }
            _ => None,
        };
        if partition && part.is_none() {
            return None;
        }
        Some(WipeState {
            disk,
            partition: part,
            target_partition: partition,
            disk_desc: d.display_name(),
            disk_size: d.size,
            system_disk: ctx.system_disk == Some(disk),
            method: WipeMethod::Zero,
            confirm: String::new(),
        })
    }

    pub fn target_size(&self) -> u64 {
        match (&self.partition, self.target_partition) {
            (Some((_, _, len)), true) => *len,
            _ => self.disk_size,
        }
    }

    pub fn target_desc(&self) -> String {
        match (&self.partition, self.target_partition) {
            (Some((_, desc, _)), true) => desc.clone(),
            _ => self.disk_desc.clone(),
        }
    }

    pub fn confirmed(&self) -> bool {
        self.confirm.trim() == CONFIRM_WORD
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        if !self.confirmed() {
            return Err(format!("Type {CONFIRM_WORD} to confirm."));
        }
        let (disk, method) = (self.disk, self.method);
        Ok(vec![match (&self.partition, self.target_partition) {
            (Some((number, _, _)), true) => Operation::WipePartition { disk, number: *number, method },
            _ => Operation::WipeDisk { disk, method },
        }])
    }
}

fn target_ui(s: &mut WipeState, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
    ui.label("Wipe:");
    let disk_text = format!("The whole disk: {} (including its partition table)", s.disk_desc);
    ui.radio_value(&mut s.target_partition, false, disk_text);
    if let Some((_, desc, _)) = &s.partition {
        ui.radio_value(&mut s.target_partition, true, format!("Only {desc}"));
    }
    ui.add_space(8.0);
    ui.label("Method:");
    let size = s.target_size();
    for m in [WipeMethod::Zero, WipeMethod::Random, WipeMethod::Dod3] {
        ui.radio_value(&mut s.method, m, format!("{} ({})", method_name(m), duration_hint(size, m)));
    }
    ui.weak("Durations assume about 150 MB/s.");
    if s.system_disk && !s.target_partition {
        ui.add_space(6.0);
        danger(ui, "This is the running system disk; it cannot be wiped.");
    }
    StepResult::Continue
}

fn confirm_ui(s: &mut WipeState, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
    danger(ui, format!("Every byte of {} will be overwritten. This cannot be undone.", s.target_desc()));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(format!("Type {CONFIRM_WORD} to confirm:"));
        ui.add(egui::TextEdit::singleline(&mut s.confirm).desired_width(120.0));
    });
    StepResult::from_problem((!s.confirmed()).then(String::new))
}

fn target_summary(s: &WipeState) -> Vec<(String, String)> {
    vec![
        row("Target", s.target_desc()),
        row("Size", format_size(s.target_size())),
        row("Method", method_name(s.method)),
        row("Estimated time", duration_hint(s.target_size(), s.method)),
    ]
}

fn confirm_summary(_s: &WipeState) -> Vec<(String, String)> {
    Vec::new()
}

/// Going Back from the confirmation forgets what was typed: a changed target or method must
/// be confirmed again.
fn forget_confirmation(s: &mut WipeState, forward: bool) {
    if !forward {
        s.confirm.clear();
    }
}

fn wizard_over(st: Shared<WipeState>) -> Wizard {
    let title = if st.borrow().target_partition { "Wipe Partition" } else { "Wipe Disk" };
    let steps: Vec<Box<dyn WizardStep>> = vec![
        Box::new(FnStep::new("Target and method", st.clone(), target_ui, target_summary)),
        Box::new(FnStep::new("Confirm", st.clone(), confirm_ui, confirm_summary).with_on_leave(forget_confirmation)),
    ];
    Wizard::new(title, steps, Box::new(move |_| st.borrow().build()))
}

pub fn wizard(state: WipeState) -> Wizard {
    wizard_over(shared(state))
}

/// `partition`: opened from "Wipe Partition" (needs a selected partition).
pub fn new(ctx: &WizardCtx, partition: bool) -> Option<Wizard> {
    WipeState::initial(ctx, partition).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, model_of};
    use crate::wizard::check_ops;
    use pm_core::GIB;

    #[test]
    fn build_wipe_ops_needs_typed_confirmation() {
        let mut m = model_of(vec![gpt_disk(1)]);
        m.system_disk = Some(7);
        let mut s = WipeState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 3)), true).unwrap();
        s.method = WipeMethod::Dod3;
        assert!(s.build().unwrap_err().contains("WIPE"));
        s.confirm = "wipe".into();
        assert!(s.build().is_err(), "case-sensitive");
        s.confirm = " WIPE ".into();
        let ops = s.build().unwrap();
        assert_eq!(ops, vec![Operation::WipePartition { disk: 1, number: 3, method: WipeMethod::Dod3 }]);
        assert!(check_ops(&m, &ops).is_ok());
        s.target_partition = false;
        assert_eq!(s.build().unwrap(), vec![Operation::WipeDisk { disk: 1, method: WipeMethod::Dod3 }]);
        // disk selection: disk only
        let mut s = WipeState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(1)), false).unwrap();
        assert!(s.partition.is_none());
        s.confirm = "WIPE".into();
        assert_eq!(wizard(s).build_ops().unwrap(), vec![Operation::WipeDisk { disk: 1, method: WipeMethod::Zero }]);
        assert!(WipeState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(1)), true).is_none());
    }

    #[test]
    fn going_back_from_the_confirmation_clears_it() {
        let mut m = model_of(vec![gpt_disk(1)]);
        m.system_disk = Some(7);
        let st = shared(WipeState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 3)), true).unwrap());
        let mut w = wizard_over(st.clone());
        w.goto(1);
        st.borrow_mut().confirm = "WIPE".into();
        // back from the summary page to the confirmation keeps it (still on screen)
        w.goto(2);
        w.back();
        assert_eq!((w.idx(), st.borrow().confirm.as_str()), (1, "WIPE"));
        assert!(w.build_ops().is_ok());
        // back from the confirmation to the target forgets it
        w.back();
        assert_eq!((w.idx(), st.borrow().confirm.as_str()), (0, ""));
        assert!(w.build_ops().unwrap_err().contains("WIPE"));
    }

    #[test]
    fn duration_hints() {
        assert_eq!(duration_hint(100 * MIB, WipeMethod::Zero), "under a minute");
        assert_eq!(duration_hint(150 * MIB * 600, WipeMethod::Random), "about 10 min");
        assert_eq!(duration_hint(1024 * GIB, WipeMethod::Dod3), "about 5 h 49 min");
    }
}
