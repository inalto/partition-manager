//! "Pending operations: N" and the list of their summaries.

use crate::app::{AppState, Cmd};
use crate::theme::DANGER;
use eframe::egui::{self, RichText};

pub fn show(ui: &mut egui::Ui, s: &AppState, _out: &mut Vec<Cmd>) {
    ui.add_space(4.0);
    ui.label(RichText::new(format!("Pending operations: {}", s.queue.len())).strong());
    ui.separator();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if s.queue.is_empty() {
            ui.weak("Nothing queued. Pick an action on the left or right-click a disk or partition.");
        }
        for (i, op) in s.queue.ops().iter().enumerate() {
            let text = RichText::new(format!("{}. {}", i + 1, op.summary()));
            let text = if op.is_destructive() { text.color(DANGER) } else { text };
            ui.add(egui::Label::new(text).wrap());
        }
    });
}
