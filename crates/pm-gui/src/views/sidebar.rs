//! Left sidebar: the action groups (enabled by the selection) and the pending operations.

use super::pending;
use crate::actions::GROUPS;
use crate::app::{AppState, Cmd};
use eframe::egui::{self, Button, CollapsingHeader, RichText};

pub fn show(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    egui::TopBottomPanel::bottom("pending_operations")
        .resizable(true)
        .default_height(180.0)
        .show_inside(ui, |ui| pending::show(ui, s, out));
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for g in &GROUPS {
            CollapsingHeader::new(RichText::new(g.title).strong())
                .default_open(true)
                .show(ui, |ui| {
                    for a in g.actions {
                        let enabled = !s.busy() && a.enabled(&s.selected, s.preview());
                        let b = Button::new(a.label()).frame(false);
                        if ui.add_enabled(enabled, b).clicked() {
                            out.push(Cmd::Action(*a, s.selected));
                        }
                    }
                });
        }
    });
}
