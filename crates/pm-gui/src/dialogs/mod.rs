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
use pm_ops::{Operation, WipeMethod};
use pm_platform::MountPoint;

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
        Dialog::Confirm { allow_system, destructive, system } => {
            confirm::show(ctx, s, allow_system, destructive, system, out)
        }
        Dialog::WipeMethod { disk, method } => wipe_method(ctx, *disk, method, out),
        Dialog::MountLetter { volume, letters, choice } => {
            let mut keep = true;
            modal_window("Mount").show(ctx, |ui| {
                ui.label(format!("Mount {} at drive letter:", volume.0));
                if letters.is_empty() {
                    ui.colored_label(DANGER, "No free drive letter.");
                }
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for c in letters.iter() {
                            ui.radio_value(choice, Some(*c), format!("{c}:"));
                        }
                    });
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                    if ui.add_enabled(choice.is_some(), egui::Button::new("OK")).clicked() {
                        if let Some(c) = *choice {
                            out.push(Cmd::Push(vec![Operation::Mount {
                                volume: volume.clone(),
                                at: MountPoint::Letter(c),
                            }]));
                        }
                        keep = false;
                    }
                });
            });
            keep
        }
        Dialog::Unmount { volume, mount_points, choice } => {
            let mut keep = true;
            modal_window("Unmount").show(ctx, |ui| {
                ui.label(format!("Remove this mount point of {}:", volume.0));
                for (i, m) in mount_points.iter().enumerate() {
                    ui.radio_value(choice, i, m);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                    if ui.button("OK").clicked() {
                        if let Some(m) = mount_points.get(*choice) {
                            out.push(Cmd::Push(vec![Operation::Unmount {
                                volume: volume.clone(),
                                at: mount_point_of(m),
                            }]));
                        }
                        keep = false;
                    }
                });
            });
            keep
        }
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

/// `"E:\"` → `Letter('E')`, anything else → a folder mount point.
fn mount_point_of(m: &str) -> MountPoint {
    let b = m.as_bytes();
    let is_root = b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || (b.len() == 3 && (b[2] == b'\\' || b[2] == b'/')));
    if is_root {
        MountPoint::Letter((b[0] as char).to_ascii_uppercase())
    } else {
        MountPoint::Folder(m.into())
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

fn wipe_method(ctx: &egui::Context, disk: u32, method: &mut WipeMethod, out: &mut Vec<Cmd>) -> bool {
    let mut keep = true;
    modal_window(format!("Wipe Disk {disk}")).show(ctx, |ui| {
        ui.label("Every byte of the disk is overwritten, including its partition table.");
        ui.radio_value(method, WipeMethod::Zero, "Fill with zeros (fast)");
        ui.radio_value(method, WipeMethod::Random, "Fill with random data");
        ui.radio_value(method, WipeMethod::Dod3, "DoD 5220.22-M (3 passes, slow)");
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                keep = false;
            }
            if ui.button("OK").clicked() {
                out.push(Cmd::Push(vec![Operation::WipeDisk { disk, method: *method }]));
                keep = false;
            }
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_point_strings() {
        assert_eq!(mount_point_of("e:\\"), MountPoint::Letter('E'));
        assert_eq!(mount_point_of("F:"), MountPoint::Letter('F'));
        assert_eq!(mount_point_of("C:\\mnt\\x\\"), MountPoint::Folder("C:\\mnt\\x\\".into()));
    }
}
