//! Right-click menu on disk-map segments and table rows: the sidebar actions that apply to
//! the clicked item, grouped like the sidebar.

use crate::actions::GROUPS;
use crate::app::{AppState, Cmd};
use crate::selection::Selected;
use eframe::egui;

pub fn show(ui: &mut egui::Ui, s: &AppState, sel: Selected, out: &mut Vec<Cmd>) {
    let mut any = false;
    for g in &GROUPS {
        let enabled: Vec<_> = g.actions.iter().filter(|a| a.enabled(&sel, s.preview())).collect();
        if enabled.is_empty() {
            continue;
        }
        if any {
            ui.separator();
        }
        any = true;
        for a in enabled {
            if ui.add_enabled(!s.busy(), egui::Button::new(a.label())).clicked() {
                out.push(Cmd::Action(*a, sel));
                ui.close_menu();
            }
        }
    }
    if !any {
        ui.weak("No actions");
    }
}
