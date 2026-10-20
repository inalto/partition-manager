//! A generic multi-step wizard: a modal window with a numbered step list on the left (the
//! current step in bold, like MiniTool), the current step's page on the right and Back / Next
//! / Finish / Cancel buttons. The last page is generated: a summary of every step's
//! [`WizardStep::summary`] rows and of the operations the wizard will queue.
//!
//! # Writing a wizard
//!
//! A wizard is a list of [`WizardStep`]s plus an `on_finish` function that turns the filled-in
//! steps into [`Operation`]s. Steps usually share one state struct through a [`Shared`]
//! (`Rc<RefCell<_>>`): each step draws and validates part of it, and `on_finish` (which may
//! ignore its `steps` argument) builds the operations from the same state. Keeping the
//! state a plain struct with a pure `build` function makes the wizard testable without egui.
//!
//! * [`WizardStep::ui`] draws the page and returns [`StepResult::Invalid`] (with a message
//!   shown under the page) while the step is incomplete; Next stays disabled until it returns
//!   [`StepResult::Continue`].
//! * On the summary page the operations are built and checked against the preview model
//!   exactly like [`pm_ops::OpQueue::push`] would ([`check_ops`]); a rejection is shown inline
//!   and Finish stays disabled.
//! * [`Wizard::show`] returns [`WizardOutcome::Finished`] with the operations; the caller
//!   queues them. Should queueing still fail, it calls [`Wizard::set_error`] and keeps the
//!   wizard open, so the message appears on the summary page.

use crate::selection::Selected;
use crate::theme::DANGER;
use crate::views::icons;
use eframe::egui::{self, Align, Layout, RichText};
use pm_ops::{Model, Operation, Planner};
use std::cell::RefCell;
use std::rc::Rc;

/// State shared between the steps of one wizard and its `on_finish`.
pub type Shared<T> = Rc<RefCell<T>>;

pub fn shared<T>(value: T) -> Shared<T> {
    Rc::new(RefCell::new(value))
}

/// Whether the user may leave a step with Next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepResult {
    Continue,
    /// Not yet; the message says what is missing (empty: say nothing).
    Invalid(String),
}

impl StepResult {
    /// `Continue` when `problem` is `None`.
    pub fn from_problem(problem: Option<String>) -> StepResult {
        problem.map_or(StepResult::Continue, StepResult::Invalid)
    }
}

/// What a wizard can see of the application.
pub struct WizardCtx<'a> {
    /// The preview model (the disks after the pending operations).
    pub model: &'a Model,
    /// Drive letters free on the system and in the preview model, sorted.
    pub platform_free_letters: Vec<char>,
    /// The selection the wizard was opened on.
    pub selection: Selected,
    /// The running system disk, if known.
    pub system_disk: Option<u32>,
}

impl<'a> WizardCtx<'a> {
    /// Context over `model`; `platform_letters` (from `Platform::free_letters`) are narrowed
    /// to the letters the pending operations do not use.
    pub fn new(model: &'a Model, platform_letters: Vec<char>, selection: Selected) -> Self {
        let mut letters: Vec<char> = platform_letters
            .into_iter()
            .map(|c| c.to_ascii_uppercase())
            .filter(|c| c.is_ascii_uppercase() && !model.letter_in_use(*c))
            .collect();
        letters.sort_unstable();
        letters.dedup();
        WizardCtx { model, platform_free_letters: letters, selection, system_disk: model.effective_system_disk() }
    }
}

/// One page of a wizard.
pub trait WizardStep {
    /// Shown in the step list and as the page heading.
    fn title(&self) -> &str;
    /// Draw the page; report whether the user may go on.
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult;
    /// Key / value rows for the summary page.
    fn summary(&self) -> Vec<(String, String)>;
    /// The user leaves this step with Next (`forward`) or Back, e.g. to forget a typed
    /// confirmation when going back. Does nothing by default.
    fn on_leave(&mut self, _forward: bool) {}
}

/// Builds the operations from the filled-in steps. Called for the summary page (every frame
/// it is shown) and on Finish, hence `Fn` rather than `FnOnce`.
pub type FinishFn = Box<dyn Fn(&[Box<dyn WizardStep>]) -> Result<Vec<Operation>, String>>;

/// Draws one step of a wizard over shared state `S`.
pub type StepUi<S> = fn(&mut S, &mut egui::Ui, &WizardCtx) -> StepResult;
/// Summary rows of one step of a wizard over shared state `S`.
pub type StepSummary<S> = fn(&S) -> Vec<(String, String)>;

/// A step made of two plain functions over the wizard's shared state (see
/// [`Wizard::over_state`]).
pub struct FnStep<S> {
    pub title: String,
    pub state: Shared<S>,
    pub ui: StepUi<S>,
    pub summary: StepSummary<S>,
    /// Called when the user leaves the step (see [`WizardStep::on_leave`]).
    pub on_leave: Option<fn(&mut S, bool)>,
}

impl<S> FnStep<S> {
    pub fn new(title: impl Into<String>, state: Shared<S>, ui: StepUi<S>, summary: StepSummary<S>) -> FnStep<S> {
        FnStep { title: title.into(), state, ui, summary, on_leave: None }
    }

    /// Run `f(state, forward)` when the user leaves the step.
    pub fn with_on_leave(mut self, f: fn(&mut S, bool)) -> FnStep<S> {
        self.on_leave = Some(f);
        self
    }
}

impl<S> WizardStep for FnStep<S> {
    fn title(&self) -> &str {
        &self.title
    }
    fn ui(&mut self, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
        (self.ui)(&mut self.state.borrow_mut(), ui, ctx)
    }
    fn summary(&self) -> Vec<(String, String)> {
        (self.summary)(&self.state.borrow())
    }
    fn on_leave(&mut self, forward: bool) {
        if let Some(f) = self.on_leave {
            f(&mut self.state.borrow_mut(), forward);
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum WizardOutcome {
    Open,
    Cancelled,
    Finished(Vec<Operation>),
}

/// The message of a planner error without the `invalid:` prefix of its `Display`.
pub fn error_text(e: pm_core::Error) -> String {
    match e {
        pm_core::Error::Invalid(m) => m,
        other => other.to_string(),
    }
}

/// Validate `ops` on `model` in order, exactly like pushing them onto an
/// [`pm_ops::OpQueue`] whose preview is `model`; returns the resulting model.
pub fn check_ops(model: &Model, ops: &[Operation]) -> Result<Model, String> {
    if ops.is_empty() {
        return Err("Nothing to do.".into());
    }
    let mut m = model.clone();
    for op in ops {
        pm_ops::precheck(&m, op).map_err(error_text)?;
        m = Planner::simulate(&m, op).map_err(error_text)?;
    }
    Ok(m)
}

pub struct Wizard {
    pub title: String,
    steps: Vec<Box<dyn WizardStep>>,
    /// Current page; `steps.len()` is the summary page.
    idx: usize,
    /// Set by the caller when queueing the finished operations failed.
    error: Option<String>,
    on_finish: FinishFn,
    /// The current step's result as of the last frame drawn.
    valid: StepResult,
}

impl Wizard {
    pub fn new(title: impl Into<String>, steps: Vec<Box<dyn WizardStep>>, on_finish: FinishFn) -> Wizard {
        Wizard {
            title: title.into(),
            steps,
            idx: 0,
            error: None,
            on_finish,
            valid: StepResult::Invalid(String::new()),
        }
    }

    /// The common shape of a wizard: one state struct `S`, steps drawn by plain functions
    /// over it, and a pure `build` turning the filled-in state into operations.
    pub fn over_state<S: 'static>(
        title: impl Into<String>,
        state: Shared<S>,
        steps: Vec<(&str, StepUi<S>, StepSummary<S>)>,
        build: fn(&S) -> Result<Vec<Operation>, String>,
    ) -> Wizard {
        let boxed: Vec<Box<dyn WizardStep>> = steps
            .into_iter()
            .map(|(t, ui, summary)| {
                Box::new(FnStep::new(t, state.clone(), ui, summary)) as Box<dyn WizardStep>
            })
            .collect();
        Wizard::new(title, boxed, Box::new(move |_| build(&state.borrow())))
    }

    /// Index of the current page (the summary page is `step_count()`).
    pub fn idx(&self) -> usize {
        self.idx
    }

    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    pub fn on_summary(&self) -> bool {
        self.idx >= self.steps.len()
    }

    /// Titles of every page, the summary included.
    pub fn page_titles(&self) -> Vec<String> {
        self.steps.iter().map(|s| s.title().to_string()).chain(std::iter::once("Summary".into())).collect()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Show `e` on the summary page (queueing the finished operations failed).
    pub fn set_error(&mut self, e: impl Into<String>) {
        self.idx = self.steps.len();
        self.error = Some(e.into());
    }

    /// Jump to page `i` without validation (tests and programmatic use).
    pub fn goto(&mut self, i: usize) {
        self.idx = i.min(self.steps.len());
        self.error = None;
        self.valid = StepResult::Invalid(String::new());
    }

    pub fn back(&mut self) {
        if self.idx > 0 {
            if let Some(step) = self.steps.get_mut(self.idx) {
                step.on_leave(false);
            }
            self.goto(self.idx - 1);
        }
    }

    /// Next page, if the current step reported `Continue` on the last frame.
    pub fn next(&mut self) {
        if !self.on_summary() && self.valid == StepResult::Continue {
            self.steps[self.idx].on_leave(true);
            self.goto(self.idx + 1);
        }
    }

    /// The operations the filled-in steps describe.
    pub fn build_ops(&self) -> Result<Vec<Operation>, String> {
        (self.on_finish)(&self.steps)
    }

    /// Every step's summary rows, in order.
    pub fn summary_rows(&self) -> Vec<(String, String)> {
        self.steps.iter().flat_map(|s| s.summary()).collect()
    }

    /// The operations, validated on `model` (what the summary page shows).
    pub fn checked_ops(&self, model: &Model) -> Result<Vec<Operation>, String> {
        let ops = self.build_ops()?;
        check_ops(model, &ops)?;
        Ok(ops)
    }

    /// Draw the wizard window for one frame.
    pub fn show(&mut self, ctx: &egui::Context, wctx: &WizardCtx) -> WizardOutcome {
        let mut outcome = WizardOutcome::Open;
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            return WizardOutcome::Cancelled;
        }
        let titles = self.page_titles();
        // Bound the body to the viewport: columns and vertical separators would otherwise
        // stretch to the bottom of the screen and push the footer off it.
        let body_h = crate::dialogs::wizard_body_height(ctx, 520.0);
        let scroll_h = crate::dialogs::wizard_scroll_height(body_h);
        let max_w = (ctx.screen_rect().width() - 40.0).max(400.0);
        crate::dialogs::modal_window(self.title.clone()).min_width(660.0_f32.min(max_w)).max_width(max_w).show(ctx, |ui| {
            ui.scope(|ui| {
                ui.set_max_height(body_h);
                ui.horizontal_top(|ui| {
                    // step list
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        for (i, t) in titles.iter().enumerate() {
                            let text = format!("{}. {t}", i + 1);
                            let rt = if i == self.idx {
                                RichText::new(text).strong()
                            } else if i < self.idx {
                                RichText::new(text)
                            } else {
                                RichText::new(text).weak()
                            };
                            ui.label(rt);
                            ui.add_space(2.0);
                        }
                    });
                    ui.separator();
                    // page
                    ui.vertical(|ui| {
                        ui.set_width(500.0_f32.min(max_w - 200.0));
                        ui.set_min_height(body_h.min(320.0));
                        ui.heading(&titles[self.idx]);
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical().max_height(scroll_h).auto_shrink([false, true]).show(ui, |ui| {
                            if self.on_summary() {
                                self.valid = self.summary_page(ui, wctx.model);
                            } else {
                                self.valid = self.steps[self.idx].ui(ui, wctx);
                            }
                        });
                        if let StepResult::Invalid(msg) = &self.valid {
                            if !msg.is_empty() {
                                ui.add_space(4.0);
                                ui.colored_label(DANGER, format!("{} {msg}", icons::WARNING));
                            }
                        }
                    });
                });
            });
            ui.separator();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    outcome = WizardOutcome::Cancelled;
                }
                let ok = self.valid == StepResult::Continue;
                if self.on_summary() {
                    if ui.add_enabled(ok && self.error.is_none(), egui::Button::new("Finish")).clicked() {
                        if let Ok(ops) = self.build_ops() {
                            outcome = WizardOutcome::Finished(ops);
                        }
                    }
                } else if ui.add_enabled(ok, egui::Button::new("Next >")).clicked() {
                    self.next();
                }
                if ui.add_enabled(self.idx > 0, egui::Button::new("< Back")).clicked() {
                    self.back();
                }
            });
        });
        outcome
    }

    fn summary_page(&mut self, ui: &mut egui::Ui, model: &Model) -> StepResult {
        egui::Grid::new("wizard_summary").num_columns(2).spacing([16.0, 4.0]).striped(true).show(ui, |ui| {
            for (k, v) in self.summary_rows() {
                ui.label(RichText::new(k).strong());
                ui.label(v);
                ui.end_row();
            }
        });
        ui.add_space(8.0);
        let result = self.checked_ops(model);
        match &result {
            Ok(ops) => {
                ui.label(RichText::new("Pending operations to add:").strong());
                for op in ops {
                    ui.label(format!("• {}", op.summary()));
                }
                ui.add_space(4.0);
                ui.weak("Nothing is written until you press Apply.");
            }
            Err(_) => {
                ui.label("The operation cannot be queued:");
            }
        }
        if let Some(e) = &self.error {
            return StepResult::Invalid(e.clone());
        }
        StepResult::from_problem(result.err())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, model_of};

    struct Field {
        title: &'static str,
        value: Shared<String>,
    }

    impl WizardStep for Field {
        fn title(&self) -> &str {
            self.title
        }
        fn ui(&mut self, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
            ui.text_edit_singleline(&mut *self.value.borrow_mut());
            StepResult::from_problem(self.value.borrow().is_empty().then(|| "Type something".into()))
        }
        fn summary(&self) -> Vec<(String, String)> {
            vec![(self.title.into(), self.value.borrow().clone())]
        }
    }

    fn two_steps() -> (Wizard, Shared<String>) {
        let v = shared(String::new());
        let steps: Vec<Box<dyn WizardStep>> = vec![
            Box::new(Field { title: "One", value: v.clone() }),
            Box::new(Field { title: "Two", value: v.clone() }),
        ];
        let v2 = v.clone();
        let w = Wizard::new(
            "Test",
            steps,
            Box::new(move |_| Ok(vec![Operation::SetLetter { disk: 1, number: 3, letter: v2.borrow().chars().next() }])),
        );
        (w, v)
    }

    fn frame(ctx: &egui::Context, w: &mut Wizard, m: &Model) -> WizardOutcome {
        let mut out = WizardOutcome::Open;
        let wctx = WizardCtx::new(m, vec!['F', 'g', 'E'], Selected::Partition(1, 3));
        let _ = ctx.run(egui::RawInput::default(), |ctx| out = w.show(ctx, &wctx));
        out
    }

    /// A step whose page is much taller than any screen: the window must still fit.
    struct Tall;
    impl WizardStep for Tall {
        fn title(&self) -> &str {
            "Tall"
        }
        fn ui(&mut self, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
            for i in 0..200 {
                ui.label(format!("row {i}"));
            }
            ui.separator();
            StepResult::Continue
        }
        fn summary(&self) -> Vec<(String, String)> {
            vec![]
        }
    }

    #[test]
    fn wizard_window_fits_a_short_viewport() {
        let m = model_of(vec![gpt_disk(1)]);
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 600.0));
        let input = || egui::RawInput { screen_rect: Some(screen), ..Default::default() };
        let mut w = Wizard::new("Tall wizard", vec![Box::new(Tall)], Box::new(|_| Ok(vec![])));
        let wctx = WizardCtx::new(&m, vec!['E'], Selected::Disk(1));
        // a few frames so the window settles on its final size
        for _ in 0..3 {
            let _ = ctx.run(input(), |ctx| {
                w.show(ctx, &wctx);
            });
        }
        let rect = ctx
            .memory(|mem| mem.area_rect(egui::Id::new("Tall wizard")))
            .expect("the wizard window was laid out");
        assert!(rect.min.y >= 0.0 && rect.max.y <= screen.max.y, "window {rect:?} leaves the {screen:?} viewport vertically");
        assert!(rect.min.x >= 0.0 && rect.max.x <= screen.max.x, "window {rect:?} leaves the viewport horizontally");
        assert!(rect.height() < screen.height(), "window {rect:?} must leave room around it");
    }

    #[test]
    fn next_waits_for_a_valid_step_and_summary_checks_ops() {
        let m = model_of(vec![gpt_disk(1)]);
        let ctx = egui::Context::default();
        let (mut w, v) = two_steps();
        assert_eq!(w.page_titles(), ["One", "Two", "Summary"]);
        assert_eq!(frame(&ctx, &mut w, &m), WizardOutcome::Open);
        w.next();
        assert_eq!(w.idx(), 0, "empty value: Next is refused");
        *v.borrow_mut() = "G".into();
        frame(&ctx, &mut w, &m);
        w.next();
        assert_eq!(w.idx(), 1);
        frame(&ctx, &mut w, &m);
        w.next();
        assert!(w.on_summary());
        frame(&ctx, &mut w, &m);
        assert_eq!(w.summary_rows(), [("One".to_string(), "G".to_string()), ("Two".to_string(), "G".to_string())]);
        assert!(w.checked_ops(&m).is_ok());
        // an operation the planner refuses is reported, not queued
        *v.borrow_mut() = "1".into();
        let e = w.checked_ops(&m).unwrap_err();
        assert!(e.contains("not a valid drive letter"), "{e}");
        w.back();
        assert_eq!(w.idx(), 1);
        w.set_error("queue said no");
        assert!(w.on_summary());
        assert_eq!(w.error(), Some("queue said no"));
    }

    #[test]
    fn ctx_narrows_letters_and_finds_system_disk() {
        let mut m = model_of(vec![gpt_disk(1)]);
        m.system_disk = Some(1);
        let c = WizardCtx::new(&m, vec!['g', 'E', 'F', 'F', '1'], Selected::None);
        assert_eq!(c.platform_free_letters, ['F', 'G'], "E: is used by partition 3");
        assert_eq!(c.system_disk, Some(1));
    }

    #[test]
    fn check_ops_is_in_order_and_rejects_empty() {
        let m = model_of(vec![gpt_disk(1)]);
        assert!(check_ops(&m, &[]).is_err());
        let ops = [
            Operation::DeletePartition { disk: 1, number: 3 },
            Operation::DeletePartition { disk: 1, number: 3 },
        ];
        let e = check_ops(&m, &ops).unwrap_err();
        assert!(e.contains("no partition 3"), "{e}");
        let after = check_ops(&m, &ops[..1]).unwrap();
        assert!(after.partition(1, 3).is_none());
    }
}
