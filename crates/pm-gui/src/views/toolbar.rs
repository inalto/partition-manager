//! Top toolbar: Apply / Undo / Discard, tool windows, Refresh and Settings.

use super::icons;
use crate::app::{AppState, Cmd, Tool};
use crate::theme::{Theme, ACCENT_STRONG};
use eframe::egui::{self, Button, RichText};

/// The text of the Apply button: how many operations it runs, and whether it only simulates.
pub fn apply_label(pending: usize, dry_run: bool) -> String {
    let count = if pending == 0 { String::new() } else { format!(" {pending}") };
    let dry = if dry_run { " (dry run)" } else { "" };
    format!("{} Apply{count}{dry}", icons::APPLY)
}

/// Apply, Undo and Discard, in a row. Apply is the one button that writes to disks: it is
/// filled with the accent color while there is something to apply. Shown in the toolbar and
/// next to the pending operations.
pub fn queue_buttons(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    let queue_actions = !s.queue.is_empty() && !s.busy();
    let label = RichText::new(apply_label(s.queue.len(), s.dry_run)).strong();
    let apply = if queue_actions {
        Button::new(label.color(egui::Color32::WHITE)).fill(ACCENT_STRONG)
    } else {
        Button::new(label)
    };
    let hint = if s.dry_run { "Simulate the pending operations without writing" } else { "Apply the pending operations" };
    if ui.add_enabled(queue_actions, apply).on_hover_text(hint).clicked() {
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
}

pub fn show(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().button_padding = egui::vec2(8.0, 5.0);
        queue_buttons(ui, s, out);
        ui.separator();
        for t in Tool::ALL {
            if t == Tool::BackupImage {
                ui.menu_button(t.label(), |ui| tool_menu(ui, &Tool::IMAGE_MENU, out));
            } else if ui.button(t.label()).clicked() {
                out.push(Cmd::OpenTool(t));
            }
        }
        ui.menu_button("Disk tools", |ui| tool_menu(ui, &Tool::MORE, out));
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
            let mut check = s.update.settings.check_updates;
            if ui.checkbox(&mut check, "Check for updates at startup").changed() {
                out.push(Cmd::SetCheckUpdates(check));
            }
            if ui.button("Check for updates now").clicked() {
                out.push(Cmd::CheckUpdates);
                ui.close_menu();
            }
            ui.separator();
            if ui
                .button("Open log folder")
                .on_hover_text(r"%LOCALAPPDATA%\PartitionManager (partition-manager.log)")
                .clicked()
            {
                out.push(Cmd::OpenLogFolder);
                ui.close_menu();
            }
        });
    });
}

fn tool_menu(ui: &mut egui::Ui, tools: &[Tool], out: &mut Vec<Cmd>) {
    for t in tools {
        if ui.button(t.label()).clicked() {
            out.push(Cmd::OpenTool(*t));
            ui.close_menu();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_label_counts_the_queue_and_marks_a_dry_run() {
        assert_eq!(apply_label(0, false), format!("{} Apply", icons::APPLY));
        assert_eq!(apply_label(3, false), format!("{} Apply 3", icons::APPLY));
        assert_eq!(apply_label(0, true), format!("{} Apply (dry run)", icons::APPLY));
        assert_eq!(apply_label(12, true), format!("{} Apply 12 (dry run)", icons::APPLY));
    }
}
