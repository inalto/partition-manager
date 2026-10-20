//! Modal dialogs, the progress dialog and the properties window (tool windows: `crate::tools`).
//!
//! Like the views, dialogs read the state and push [`Cmd`]s; the only thing they mutate is
//! their own [`Dialog`] (the user's choice in a radio list, a checkbox).

pub mod confirm;
pub mod progress;
pub mod properties;

use crate::app::{AppState, Cmd, Dialog};
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

/// Height budget for the body of a multi-page dialog (step list + page) so that the
/// title bar, the page heading, the footer buttons and a margin always fit on screen.
///
/// Vertical separators and columns in a horizontal layout take the whole *available*
/// height; inside an unbounded window that is the rest of the screen, which makes the
/// window taller than the viewport. Every wizard frame therefore wraps its body in a scope
/// whose max height is this value.
pub(crate) fn wizard_body_height(ctx: &egui::Context, preferred: f32) -> f32 {
    const CHROME: f32 = 150.0; // title bar + footer row + separators + window margin
    const MIN: f32 = 200.0;
    (ctx.screen_rect().height() - CHROME).clamp(MIN, preferred.max(MIN))
}

/// The scrollable part of a wizard page: the body height minus the heading and the
/// validation line under the scroll area.
pub(crate) fn wizard_scroll_height(body: f32) -> f32 {
    (body - 60.0).max(120.0)
}

/// One line per rescue-mode image (one with a mapfile) or image with unreadable areas, from
/// [`pm_ops::ExecReport::rescue_stats`] (`N` counts every image of the run):
/// `Image N: rescued X, errsize Y, errors E[, mapfile P]` (errors: the unrescued areas, as
/// in the log), and whether it is a warning (unreadable areas, left as zeros in the image).
pub fn rescue_lines(stats: &[pm_image::RescueStats]) -> Vec<(String, bool)> {
    use pm_core::format_size;
    stats
        .iter()
        .enumerate()
        .filter(|(_, st)| st.mapfile.is_some() || st.bad_areas > 0)
        .map(|(i, st)| {
            let mut line = format!(
                "Image {}: rescued {}, errsize {}, errors {}",
                i + 1,
                format_size(st.rescued),
                format_size(st.bad_bytes),
                st.bad_areas
            );
            if let Some(m) = &st.mapfile {
                line.push_str(&format!(", mapfile {}", m.display()));
            }
            let warn = st.bad_areas > 0;
            if warn {
                line.push_str(
                    " (unreadable areas are zeros in the image; run the operation again with more retries to retry \
                     the unreadable areas in place)",
                );
            }
            (line, warn)
        })
        .collect()
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
                for (line, warn) in rescue_lines(&report.rescue_stats) {
                    if warn {
                        ui.colored_label(crate::tools::WARN, format!("{} {line}", icons::WARNING));
                    } else {
                        ui.label(line);
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use pm_image::RescueStats;
    use std::path::PathBuf;

    #[test]
    fn report_rescue_lines_show_stats_mapfile_and_warn_on_errors() {
        let clean = RescueStats { rescued: 4 << 20, ..Default::default() };
        let damaged = RescueStats {
            rescued: (4 << 20) - 1024,
            bad_bytes: 1024,
            bad_areas: 2,
            read_errors: 7,
            mapfile: Some(PathBuf::from(r"D:\img\disk.vhd.map")),
            ..Default::default()
        };
        let rescued = RescueStats { mapfile: Some(PathBuf::from(r"D:\img\p.qcow2.map")), ..clean.clone() };
        let lines = rescue_lines(&[clean, damaged, rescued]);
        // an image without a mapfile and without errors (plain mode) gets no line
        assert_eq!(lines.len(), 2);
        let (text, warn) = &lines[0];
        assert!(warn, "errors > 0 is shown as a warning");
        assert!(
            text.starts_with(r"Image 2: rescued 4.00 MB, errsize 1.00 KB, errors 2, mapfile D:\img\disk.vhd.map"),
            "{text}"
        );
        assert!(text.contains("zeros in the image") && text.contains("in place"), "{text}");
        assert_eq!(
            lines[1],
            (r"Image 3: rescued 4.00 MB, errsize 0 B, errors 0, mapfile D:\img\p.qcow2.map".to_string(), false)
        );
        assert!(rescue_lines(&[]).is_empty());
    }
}
