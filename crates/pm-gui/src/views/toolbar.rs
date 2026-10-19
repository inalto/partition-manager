//! Top toolbar: Apply / Undo / Discard, tool windows, Refresh and Settings.

use super::icons;
use crate::app::{AppState, Cmd, Tool};
use crate::theme::Theme;
use eframe::egui::{self, Button, RichText};

pub fn show(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().button_padding = egui::vec2(8.0, 5.0);
        let queue_actions = !s.queue.is_empty() && !s.busy();
        let apply = if s.dry_run {
            format!("{} Apply (dry run)", icons::APPLY)
        } else {
            format!("{} Apply", icons::APPLY)
        };
        if ui
            .add_enabled(queue_actions, Button::new(RichText::new(apply).strong()))
            .on_hover_text("Apply the pending operations")
            .clicked()
        {
            out.push(Cmd::Apply);
        }
        if ui
            .add_enabled(queue_actions, Button::new(format!("{} Undo", icons::UNDO)))
            .on_hover_text("Remove the last pending operation")
            .clicked()
        {
            out.push(Cmd::Undo);
        }
        if ui
            .add_enabled(queue_actions, Button::new(format!("{} Discard", icons::DISCARD)))
            .on_hover_text("Remove every pending operation")
            .clicked()
        {
            out.push(Cmd::Discard);
        }
        ui.separator();
        for t in Tool::ALL {
            if ui.button(t.label()).clicked() {
                out.push(Cmd::OpenTool(t));
            }
        }
        ui.separator();
        if ui
            .add_enabled(!s.busy(), Button::new(format!("{} Refresh", icons::REFRESH)))
            .on_hover_text("Read the disks again (pending operations that still apply are kept)")
            .clicked()
        {
            out.push(Cmd::Refresh);
        }
        ui.menu_button(format!("{} Settings", icons::SETTINGS), |ui| {
            let mut dark = s.theme == Theme::Dark;
            if ui.checkbox(&mut dark, "Dark theme").changed() {
                out.push(Cmd::SetTheme(if dark { Theme::Dark } else { Theme::Light }));
            }
            let mut dry = s.dry_run;
            if ui
                .checkbox(&mut dry, "Dry run (Apply only simulates)")
                .on_hover_text("Validate the pending operations against the disks without writing")
                .changed()
            {
                out.push(Cmd::SetDryRun(dry));
            }
        });
    });
}
