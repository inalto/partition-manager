//! The About window: the logo, which release this is and how it is installed, and where to
//! read more.

use super::modal_window;
use crate::app::{AppState, Cmd};
use eframe::egui::{self, RichText};
use pm_update::{Version, RELEASES_PAGE, TARGET, UNINSTALLER};

/// The logo as raw RGBA, `LOGO_SIDE` pixels square (`scripts/make-icon.py` writes it from
/// `assets/logo.png`). Also the window icon.
pub const LOGO_RGBA: &[u8] = include_bytes!("../../assets/icon-256.rgba");
pub const LOGO_SIDE: u32 = 256;

/// The project page, and the page with the notes of release `v`.
pub fn project_page() -> String {
    RELEASES_PAGE.trim_end_matches("/releases/latest").to_string()
}

pub fn notes_page(v: Version) -> String {
    format!("{}/releases/tag/v{v}", project_page())
}

/// `"just now"`, `"5 minutes ago"`, `"3 hours ago"`, `"2 days ago"`.
pub fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    let unit = |n: u64, name: &str| format!("{n} {name}{} ago", if n == 1 { "" } else { "s" });
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => unit(secs / 60, "minute"),
        3600..=86_399 => unit(secs / 3600, "hour"),
        _ => unit(secs / 86_400, "day"),
    }
}

/// The rows of the window: what is running, where from, and what is known about newer
/// releases. `now` is the Unix time in seconds.
pub fn rows(s: &AppState, current: Version, now: u64) -> Vec<(&'static str, String)> {
    let mut out = vec![("Version", current.to_string()), ("Platform", TARGET.unwrap_or("not a release platform").to_string())];
    if let Some(dir) = s.update.install_dir() {
        let how = if dir.join(UNINSTALLER).is_file() { "Windows installer" } else { "Unpacked archive (portable)" };
        out.push(("Installed from", how.to_string()));
        out.push(("Folder", dir.display().to_string()));
    }
    let settings = &s.update.settings;
    let newer = s.update.release().map(|r| r.version).filter(|v| *v > current);
    let updates = match (newer, settings.last_check) {
        (Some(v), _) => format!("{v} is available"),
        (None, 0) if !settings.check_updates => "Not checked (the check at startup is off)".to_string(),
        (None, 0) => "Not checked yet".to_string(),
        (None, t) => format!("Up to date, checked {}", ago(now, t)),
    };
    out.push(("Updates", updates));
    out.push(("License", "MIT".to_string()));
    out
}

fn logo(ctx: &egui::Context) -> egui::TextureHandle {
    let id = egui::Id::new("pm_about_logo");
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return t;
    }
    let side = LOGO_SIDE as usize;
    let image = egui::ColorImage::from_rgba_unmultiplied([side, side], LOGO_RGBA);
    let t = ctx.load_texture("pm_logo", image, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    t
}

/// Draw the window; returns `false` when it should close.
pub fn show(ctx: &egui::Context, s: &AppState, out: &mut Vec<Cmd>) -> bool {
    let mut keep = true;
    let current = Version::current();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    modal_window("About Partition Manager").show(ctx, |ui| {
        ui.set_max_width(520.0);
        ui.horizontal(|ui| {
            ui.image((logo(ctx).id(), egui::vec2(96.0, 96.0)));
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(14.0);
                ui.heading("Partition Manager");
                ui.add_space(4.0);
                ui.label("Partitions, images, recovery and disk checks, with nothing written until Apply.");
            });
        });
        ui.add_space(8.0);
        egui::Grid::new("about_rows").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            for (name, value) in rows(s, current, now) {
                ui.label(RichText::new(name).strong());
                ui.add(egui::Label::new(value).wrap());
                ui.end_row();
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Release notes").on_hover_text(notes_page(current)).clicked() {
                out.push(Cmd::OpenUrl(notes_page(current)));
            }
            if ui.button("Project page").on_hover_text(project_page()).clicked() {
                out.push(Cmd::OpenUrl(project_page()));
            }
            if ui.button("Check for updates").clicked() {
                // the answer is a dialog of its own: this one makes room for it
                out.push(Cmd::CheckUpdates);
                keep = false;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    keep = false;
                }
            });
        });
    });
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_is_a_square_of_rgba_pixels_with_a_transparent_corner() {
        assert_eq!(LOGO_RGBA.len(), (LOGO_SIDE * LOGO_SIDE * 4) as usize);
        assert_eq!(LOGO_RGBA[3], 0, "the background was cut away");
        let centre = ((LOGO_SIDE / 2 * LOGO_SIDE + LOGO_SIDE / 2) * 4 + 3) as usize;
        assert_eq!(LOGO_RGBA[centre], 255);
    }

    #[test]
    fn pages_are_github_pages_that_may_be_opened() {
        assert_eq!(project_page(), "https://github.com/inalto/partition-manager");
        let v = Version::parse("0.4.0").unwrap();
        assert_eq!(notes_page(v), "https://github.com/inalto/partition-manager/releases/tag/v0.4.0");
        assert!(pm_update::url_allowed(&project_page()) && pm_update::url_allowed(&notes_page(v)));
    }

    #[test]
    fn ago_picks_the_unit() {
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(100, 200), "just now", "a clock that went back");
        assert_eq!(ago(160, 100), "1 minute ago");
        assert_eq!(ago(100 + 3 * 3600 + 5, 100), "3 hours ago");
        assert_eq!(ago(100 + 2 * 86_400, 100), "2 days ago");
    }

    #[test]
    fn rows_say_what_runs_and_what_is_known_about_updates() {
        let mut s = crate::test_util::mock_state();
        let v = Version::parse("0.3.0").unwrap();
        let get = |s: &AppState, name: &str| rows(s, v, 10_000).into_iter().find(|r| r.0 == name).map(|r| r.1);
        assert_eq!(get(&s, "Version").as_deref(), Some("0.3.0"));
        assert_eq!(get(&s, "License").as_deref(), Some("MIT"));
        assert_eq!(get(&s, "Updates").as_deref(), Some("Not checked yet"));
        s.update.settings.last_check = 10_000 - 7200;
        assert_eq!(get(&s, "Updates").as_deref(), Some("Up to date, checked 2 hours ago"));
        s.update.settings = crate::settings::Settings { check_updates: false, ..Default::default() };
        assert_eq!(get(&s, "Updates").as_deref(), Some("Not checked (the check at startup is off)"));
    }
}
