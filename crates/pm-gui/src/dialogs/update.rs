//! What the user sees of the update: the notice bar, What's new, the confirmation, the
//! download window and the "updated" message. Like the other dialogs they only read the
//! state and push [`Cmd`]s.

use super::modal_window;
use super::progress::activity_bar;
use crate::app::{AppState, Cmd};
use crate::tools::{progress_panel, Task};
use crate::update::{Notice, UpdateState};
use eframe::egui::{self, RichText};
use pm_update::{release_page, Release, Version};
use std::path::Path;

/// The text of the notice bar.
pub fn notice_text(n: &Notice, current: Version) -> String {
    format!("Partition Manager {} is available (you have {current}).", n.version)
}

/// The lines of the update confirmation: where the release comes from and goes, and (when
/// there is any) the work that is lost on the restart.
pub fn confirm_lines(release: &Release, dir: &Path, pending: usize, open_windows: usize) -> Vec<String> {
    let mut lines = vec![format!(
        "Partition Manager {} will be downloaded from GitHub and installed in {}.",
        release.version,
        dir.display()
    )];
    let ops = match pending {
        0 => None,
        1 => Some("The 1 pending operation".to_string()),
        n => Some(format!("The {n} pending operations")),
    };
    let dropped = match (ops, open_windows > 0) {
        (None, false) => None,
        (None, true) => Some("The open windows are dropped".to_string()),
        (Some(ops), true) => Some(format!("{ops} and the open windows are dropped")),
        (Some(ops), false) if pending == 1 => Some(format!("{ops} is dropped")),
        (Some(ops), false) => Some(format!("{ops} are dropped")),
    };
    if let Some(d) = dropped {
        lines.push(format!("{d} when the application restarts."));
    }
    lines
}

/// The notice bar of a newer release (in the central panel, above the error banner). Not
/// drawn without a notice or when it is hidden; its buttons wait while a check for it runs.
pub fn notice(ui: &mut egui::Ui, u: &UpdateState, out: &mut Vec<Cmd>) {
    let Some(n) = u.notice.as_ref().filter(|_| !u.hidden) else { return };
    egui::Frame::none().fill(ui.visuals().faint_bg_color).rounding(4.0).inner_margin(6.0).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(notice_text(n, u.current));
            ui.add_enabled_ui(!u.checking(), |ui| {
                if ui.small_button("What's new").clicked() {
                    out.push(Cmd::UpdateWhatsNew);
                }
                if u.can_install {
                    if ui.small_button("Update").clicked() {
                        out.push(Cmd::UpdateAsk);
                    }
                } else if ui.small_button("Open the download page").clicked() {
                    out.push(Cmd::OpenUrl(release_page(&n.page_url)));
                }
                if ui.small_button("Skip this version").clicked() {
                    out.push(Cmd::UpdateSkip);
                }
                if ui.small_button("x").on_hover_text("Hide until the next start").clicked() {
                    out.push(Cmd::UpdateHide);
                }
            });
        });
    });
    ui.add_space(4.0);
}

/// The notes of `release`; returns `false` when it should close.
pub fn whats_new(ctx: &egui::Context, release: &Release, out: &mut Vec<Cmd>) -> bool {
    let mut keep = true;
    modal_window(format!("What's new in {}", release.version)).show(ctx, |ui| {
        ui.set_max_width(560.0);
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            ui.add(egui::Label::new(&release.notes).wrap());
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Open the release page").clicked() {
                out.push(Cmd::OpenUrl(release_page(&release.page_url)));
            }
            if ui.button("Close").clicked() {
                keep = false;
            }
        });
    });
    keep
}

/// The confirmation before downloading `release`; returns `false` when it should close.
pub fn confirm(ctx: &egui::Context, s: &AppState, release: &Release, out: &mut Vec<Cmd>) -> bool {
    let mut keep = true;
    let dir = s.update.install_dir().unwrap_or(Path::new("the folder of the application"));
    let open_windows = s.windows.len() + usize::from(s.wizard.is_some());
    let lines = confirm_lines(release, dir, s.queue.ops().len(), open_windows);
    modal_window(format!("Update to {}", release.version)).show(ctx, |ui| {
        ui.set_max_width(480.0);
        for line in &lines {
            ui.add(egui::Label::new(line.as_str()).wrap());
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Update").strong()).clicked() {
                // the dialog stays until the command is handled: the install only starts
                // from an open confirmation, and the handler closes it
                out.push(Cmd::UpdateStart);
            }
            if ui.button("Cancel").clicked() {
                keep = false;
            }
        });
    });
    keep
}

/// `version` is installed; returns `false` when it should close.
pub fn updated(ctx: &egui::Context, version: Version, out: &mut Vec<Cmd>) -> bool {
    let mut keep = true;
    modal_window("Update installed").show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.label(format!("Updated to {version}. Restart Partition Manager to use it."));
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button(RichText::new("Restart now").strong()).clicked() {
                // stays open: the restart may have to wait, or fail with a message
                out.push(Cmd::UpdateRestart);
            }
            if ui.button("Later").clicked() {
                keep = false;
            }
        });
    });
    keep
}

/// The running download and install of `version` (modal).
pub fn download(ctx: &egui::Context, task: &Task<Version>, version: Option<Version>, out: &mut Vec<Cmd>) {
    let title = version.map_or_else(|| "Updating Partition Manager".to_string(), |v| format!("Updating to {v}"));
    let p = task.progress();
    modal_window(title).show(ctx, |ui| {
        ui.set_min_width(420.0);
        let cancel = if p.total == 0 {
            // before the size of the download is known (the checksums, the first request)
            ui.label(RichText::new(&p.phase).strong());
            activity_bar(ui);
            if task.is_cancelled() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Cancelling after the current step…");
                });
                false
            } else {
                ui.button("Cancel").clicked()
            }
        } else {
            progress_panel(ui, &p, task.elapsed(), task.is_cancelled(), true)
        };
        if cancel {
            out.push(Cmd::UpdateCancel);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn notice(s: &str) -> Notice {
        Notice { version: v(s), page_url: format!("https://github.com/inalto/partition-manager/releases/tag/v{s}") }
    }

    fn rel(s: &str) -> Release {
        Release {
            version: v(s),
            notes: String::new(),
            page_url: format!("https://github.com/inalto/partition-manager/releases/tag/v{s}"),
            archive: None,
            installer: None,
            sums_url: None,
        }
    }

    #[test]
    fn notice_and_confirmation_copy() {
        assert_eq!(notice_text(&notice("0.2.3"), v("0.2.2")), "Partition Manager 0.2.3 is available (you have 0.2.2).");
        let l = confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 0, 0);
        assert_eq!(l.len(), 1);
        assert!(l[0].contains("/opt/pm"));
        assert_eq!(l[0], "Partition Manager 0.2.3 will be downloaded from GitHub and installed in /opt/pm.");
        assert!(confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 1, 0)[1].starts_with("The 1 pending operation is dropped"));
        assert!(confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 3, 2)[1]
            .starts_with("The 3 pending operations and the open windows are dropped"));
        assert!(confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 0, 1)[1].starts_with("The open windows are dropped"));
        assert_eq!(
            confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 2, 0)[1],
            "The 2 pending operations are dropped when the application restarts."
        );
        assert_eq!(
            confirm_lines(&rel("0.2.3"), Path::new("/opt/pm"), 1, 1)[1],
            "The 1 pending operation and the open windows are dropped when the application restarts."
        );
    }
}
