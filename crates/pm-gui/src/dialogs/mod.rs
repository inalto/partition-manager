//! Modal dialogs, the progress dialog, the properties window and tool placeholders.
//!
//! Like the views, dialogs read the state and push [`Cmd`]s; the only thing they mutate is
//! their own [`Dialog`] (the user's choice in a radio list, a checkbox).

pub mod confirm;
pub mod progress;
pub mod properties;

use crate::app::{AppState, Cmd, Dialog, Tool};
use crate::theme::DANGER;
use crate::views::icons;
use eframe::egui::{self, Align2, RichText};

/// A centered, fixed dialog window.
pub(crate) fn modal_window(title: impl Into<egui::WidgetText>) -> egui::Window<'static> {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .order(egui::Order::Foreground)
}

/// Draw `dlg`; returns `false` when it should close.
pub fn show(ctx: &egui::Context, s: &AppState, dlg: &mut Dialog, out: &mut Vec<Cmd>) -> bool {
    match dlg {
        Dialog::Error(msg) => message(ctx, "Cannot do that", msg, out),
        Dialog::Confirm { allow_system } => confirm::show(ctx, s, allow_system, out),
        Dialog::Report { report, dry_run, note } => {
            let mut keep = true;
            let title = match (report.failed.is_some(), *dry_run) {
                (false, false) => "Apply finished",
                (true, false) => "Apply stopped",
                (false, true) => "Dry run finished",
                (true, true) => "Dry run: an operation would fail",
            };
            modal_window(title).resizable(true).show(ctx, |ui| {
                ui.label(format!("{} operation(s) completed.", report.completed));
                if let Some((i, msg)) = &report.failed {
                    ui.colored_label(DANGER, format!("{} Operation {} failed: {msg}", icons::WARNING, i + 1));
                    ui.label("Nothing after it was run.");
                }
                if let Some(n) = note {
                    ui.colored_label(DANGER, n);
                }
                ui.separator();
                egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                    for line in &report.log {
                        ui.monospace(line);
                    }
                });
                ui.separator();
                if ui.button("Close").clicked() {
                    keep = false;
                }
            });
            keep
        }
    }
}

fn message(ctx: &egui::Context, title: &str, msg: &str, _out: &mut Vec<Cmd>) -> bool {
    let mut keep = true;
    modal_window(title).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.label(RichText::new(format!("{} {msg}", icons::WARNING)).color(DANGER));
        ui.separator();
        if ui.button("OK").clicked() {
            keep = false;
        }
    });
    keep
}

/// Placeholder window for a toolbar tool (filled in by a later step).
pub fn tool_window(ctx: &egui::Context, tool: Tool, out: &mut Vec<Cmd>) {
    let mut open = true;
    egui::Window::new(tool.label())
        .open(&mut open)
        .collapsible(false)
        .default_width(360.0)
        .show(ctx, |ui| {
            ui.label(format!("{} opens here in a later step.", tool.label()));
        });
    if !open {
        out.push(Cmd::CloseTool);
    }
}
