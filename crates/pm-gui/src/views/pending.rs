//! "Pending operations: N" and the list of their summaries.

use crate::app::{AppState, Cmd};
use crate::theme::DANGER;
use eframe::egui::{self, RichText};

pub fn show(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    ui.add_space(4.0);
    ui.label(RichText::new(format!("Pending operations: {}", s.queue.len())).strong());
    // the same Apply / Undo / Discard as the toolbar, next to what they act on
    if !s.queue.is_empty() {
        ui.horizontal_wrapped(|ui| super::toolbar::queue_buttons(ui, s, out));
    }
    ui.separator();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if s.queue.is_empty() {
            ui.weak("Nothing queued. Actions you pick are listed here and run when you press Apply.");
        }
        for (i, op) in s.queue.ops().iter().enumerate() {
            let text = RichText::new(format!("{}. {}", i + 1, op.summary()));
            let text = if op.is_destructive() { text.color(DANGER) } else { text };
            ui.add(egui::Label::new(text).wrap());
        }
    });
}
