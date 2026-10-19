//! Change Label / Change Letter wizard (one wizard, two modes): a new volume label
//! (≤ 32 characters, ≤ 11 on FAT/exFAT) → [`Operation::SetLabel`], or a drive letter from the
//! free letters or "None" → [`Operation::SetLetter`].
//!
//! The executor does not support `SetLabel` yet (no platform API): the step checks its
//! operation like the queue will and shows the refusal right away, with Next disabled. The
//! wizard needs no change once the executor supports it.

use super::{label_problem, letter_combo, max_label_len, partition_desc, row};
use crate::items::is_container;
use crate::selection::Selected;
use crate::wizard::{check_ops, shared, StepResult, Wizard, WizardCtx};
use eframe::egui;
use pm_core::FsKind;
use pm_ops::{Model, Operation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Label,
    Letter,
}

pub struct LabelLetterState {
    pub disk: u32,
    pub number: u32,
    pub desc: String,
    pub fs: Option<FsKind>,
    pub mode: Mode,
    pub label: String,
    pub current_letter: Option<char>,
    pub letter: Option<char>,
    /// Letters offered: the current one (if any) and the free ones.
    pub letters: Vec<char>,
}

impl LabelLetterState {
    pub fn initial(ctx: &WizardCtx, mode: Mode) -> Option<LabelLetterState> {
        let Selected::Partition(disk, number) = ctx.selection else { return None };
        let d = ctx.model.disk(disk)?;
        let p = ctx.model.partition(disk, number).filter(|p| !is_container(d, p))?;
        let current = p.letter();
        let mut letters = ctx.platform_free_letters.clone();
        letters.extend(current);
        letters.sort_unstable();
        letters.dedup();
        Some(LabelLetterState {
            disk,
            number,
            desc: partition_desc(disk, p),
            fs: p.fs,
            // a partition without a filesystem has no label to change
            mode: if p.fs.is_none() { Mode::Letter } else { mode },
            label: p.label.clone(),
            current_letter: current,
            letter: current,
            letters,
        })
    }

    pub fn problem(&self) -> Option<String> {
        match self.mode {
            Mode::Label if self.fs.is_none() => Some("The partition has no filesystem to label.".into()),
            Mode::Label => label_problem(self.fs, self.label.trim()),
            Mode::Letter if self.letter == self.current_letter => Some(String::new()),
            Mode::Letter => self
                .letter
                .filter(|c| !self.letters.contains(c))
                .map(|c| format!("Drive letter {c}: is not free.")),
        }
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        if let Some(p) = self.problem() {
            return Err(if p.is_empty() { "Choose a different drive letter.".into() } else { p });
        }
        let (disk, number) = (self.disk, self.number);
        Ok(vec![match self.mode {
            Mode::Label => Operation::SetLabel { disk, number, label: self.label.trim().to_string() },
            Mode::Letter => Operation::SetLetter { disk, number, letter: self.letter },
        }])
    }

    /// What keeps the step from going on: the input problems, then whatever the queue would
    /// refuse (e.g. `SetLabel`, which the executor does not support yet).
    pub fn step_problem(&self, model: &Model) -> Option<String> {
        if let Some(p) = self.problem() {
            return Some(p);
        }
        self.build().and_then(|ops| check_ops(model, &ops)).err()
    }
}

fn step_ui(s: &mut LabelLetterState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    ui.label(&s.desc);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_enabled_ui(s.fs.is_some(), |ui| ui.radio_value(&mut s.mode, Mode::Label, "Change label"));
        ui.radio_value(&mut s.mode, Mode::Letter, "Change drive letter");
    });
    ui.add_space(6.0);
    match s.mode {
        Mode::Label => {
            ui.horizontal(|ui| {
                ui.label("New label:");
                ui.add(egui::TextEdit::singleline(&mut s.label).desired_width(220.0));
            });
            ui.weak(format!("At most {} characters on this filesystem.", max_label_len(s.fs)));
        }
        Mode::Letter => {
            ui.horizontal(|ui| {
                ui.label("New drive letter:");
                letter_combo(ui, "label_letter_letter", &mut s.letter, &s.letters);
            });
            let now = s.current_letter.map_or("none".to_string(), |c| format!("{c}:"));
            ui.weak(format!("Current drive letter: {now}."));
        }
    }
    StepResult::from_problem(s.step_problem(ctx.model))
}

fn summary(s: &LabelLetterState) -> Vec<(String, String)> {
    let mut rows = vec![row("Partition", s.desc.clone())];
    rows.push(match s.mode {
        Mode::Label => row("New label", format!("\"{}\"", s.label.trim())),
        Mode::Letter => row("New drive letter", s.letter.map_or("None".to_string(), |c| format!("{c}:"))),
    });
    rows
}

pub fn wizard(state: LabelLetterState) -> Wizard {
    let title = match state.mode {
        Mode::Label => "Change Label",
        Mode::Letter => "Change Drive Letter",
    };
    Wizard::over_state(title, shared(state), vec![("Label / letter", step_ui, summary)], LabelLetterState::build)
}

pub fn new(ctx: &WizardCtx, mode: Mode) -> Option<Wizard> {
    LabelLetterState::initial(ctx, mode).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, model_of};

    #[test]
    fn build_label_and_letter_ops() {
        let m = model_of(vec![gpt_disk(1)]);
        let ctx = WizardCtx::new(&m, vec!['G'], Selected::Partition(1, 3));
        let mut s = LabelLetterState::initial(&ctx, Mode::Label).unwrap();
        assert_eq!(s.letters, ['E', 'G'], "own letter stays selectable");
        s.label = " Archive ".into();
        assert_eq!(s.build().unwrap(), vec![Operation::SetLabel { disk: 1, number: 3, label: "Archive".into() }]);
        // the executor refuses SetLabel for now: shown on the first step already, not a crash
        let e = check_ops(&m, &s.build().unwrap()).unwrap_err();
        assert!(e.contains("set label"), "{e}");
        assert_eq!(s.step_problem(&m), Some(e));
        s.label = "x".repeat(33);
        assert!(s.build().unwrap_err().contains("32"));

        let mut s = LabelLetterState::initial(&ctx, Mode::Letter).unwrap();
        assert!(s.build().is_err(), "unchanged letter");
        s.letter = Some('G');
        let ops = s.build().unwrap();
        assert_eq!(ops, vec![Operation::SetLetter { disk: 1, number: 3, letter: Some('G') }]);
        assert!(check_ops(&m, &ops).is_ok());
        assert_eq!(s.step_problem(&m), None, "letter mode is supported");
        s.letter = None;
        assert_eq!(wizard(s).build_ops().unwrap(), vec![Operation::SetLetter { disk: 1, number: 3, letter: None }]);
    }

    #[test]
    fn fat_labels_and_unformatted_partitions() {
        let m = model_of(vec![gpt_disk(1)]);
        let mut s = LabelLetterState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 1)), Mode::Label).unwrap();
        s.label = "TWELVE_CHARS".into();
        assert!(s.build().unwrap_err().contains("11"));
        // MSR: no filesystem, so letter mode only
        let s = LabelLetterState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 2)), Mode::Label).unwrap();
        assert_eq!(s.mode, Mode::Letter);
    }
}
