//! Top bar: the tabs (the disks and the tools), then Apply / Undo / Discard, Refresh and
//! Settings.

use super::icons;
use crate::app::{AppState, Cmd, Tab};
use crate::theme::{Theme, ACCENT, ACCENT_STRONG};
use eframe::egui::{self, Button, RichText, Sense};

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
    let queue_actions = !s.queue.is_empty() && !s.busy() && !s.disks_locked();
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
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for tab in Tab::ALL {
                let running = tab.tools().iter().any(|t| s.tool_running(*t));
                if tab_button(ui, tab.label(), s.tab == tab, running) {
                    out.push(Cmd::OpenTab(tab));
                }
            }
        });
        ui.separator();
        queue_buttons(ui, s, out);
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
            if ui.button("About Partition Manager").clicked() {
                out.push(Cmd::About);
                ui.close_menu();
            }
        });
    });
}

/// One tab of the top bar: its name, an accent line under the one on screen, and a dot while
/// a job of its tool is running (the job goes on when another tab is shown). True when clicked.
fn tab_button(ui: &mut egui::Ui, label: &str, selected: bool, running: bool) -> bool {
    const PAD: egui::Vec2 = egui::vec2(10.0, 6.0);
    const DOT: f32 = 3.0;
    let font = egui::TextStyle::Button.resolve(ui.style());
    let visuals = ui.visuals();
    let color = if !ui.is_enabled() {
        visuals.weak_text_color()
    } else if selected {
        visuals.strong_text_color()
    } else {
        visuals.text_color()
    };
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
    let dot_room = if running { DOT * 2.0 + 6.0 } else { 0.0 };
    let size = galley.size() + PAD * 2.0 + egui::vec2(dot_room, 0.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, ui.is_enabled(), selected, label));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if resp.hovered() && !selected {
            painter.rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        if resp.has_focus() {
            painter.rect_stroke(rect.shrink(1.0), 3.0, ui.visuals().selection.stroke);
        }
        painter.galley(rect.min + PAD, galley, color);
        if running {
            painter.circle_filled(egui::pos2(rect.right() - PAD.x - DOT, rect.center().y), DOT, ACCENT);
        }
        if selected {
            let line = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 4.0, rect.bottom() - 2.5),
                egui::pos2(rect.right() - 4.0, rect.bottom()),
            );
            painter.rect_filled(line, 1.0, ACCENT);
        }
    }
    let resp = if running { resp.on_hover_text("A job of this tool is running") } else { resp };
    resp.clicked()
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
