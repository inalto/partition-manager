//! Status bar: color legend of the disk map, the disk source and the elevation status.

use crate::app::{AppState, JobKind};
use crate::items::ItemKind;
use crate::theme::{map_colors, DANGER};
use eframe::egui::{self, RichText};

pub const ENTRIES: [(ItemKind, &str); 5] = [
    (ItemKind::Primary, "GPT/Primary"),
    (ItemKind::Logical, "Logical"),
    (ItemKind::Unallocated, "Unallocated"),
    (ItemKind::System, "EFI/System"),
    (ItemKind::Recovery, "Recovery"),
];

pub fn show(ui: &mut egui::Ui, s: &AppState) {
    let colors = map_colors(s.theme);
    ui.horizontal(|ui| {
        for (kind, label) in ENTRIES {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 2.0, colors.of(kind));
            ui.label(label);
            ui.add_space(8.0);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let text = crate::format_util::elevation_status(s.platform.os_name(), s.elevated);
            if s.elevated {
                ui.label(text);
            } else {
                ui.label(RichText::new(text).color(DANGER));
            }
            if let Some(job) = &s.job {
                if job.kind == JobKind::Refresh {
                    ui.separator();
                    ui.label(&job.handle.name);
                    ui.spinner();
                }
            }
            // last: takes (and truncates to) whatever width is left
            if let Some(src) = &s.source {
                ui.separator();
                ui.add(egui::Label::new(src).truncate()).on_hover_text(src);
            }
        });
    });
}
