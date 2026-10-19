//! Apply confirmation: every queued operation (destructive ones in red), a red banner when
//! the running system disk is involved, Cancel / Apply.

use super::modal_window;
use crate::app::{destructive_flags, AppState, Cmd};
use crate::theme::DANGER;
use crate::views::icons;
use eframe::egui::{self, Color32, RichText};

pub fn show(
    ctx: &egui::Context,
    s: &AppState,
    allow_system: &mut bool,
    out: &mut Vec<Cmd>,
) -> bool {
    let mut keep = true;
    let ops = s.queue.ops();
    // recomputed every frame from the live queue: in-memory simulation of a short list
    let destructive = destructive_flags(s.queue.base(), ops);
    let sys = s.system_disk_ops();
    let system: Vec<bool> = (0..ops.len()).map(|i| sys.contains(&i)).collect();
    let title = if s.dry_run { "Apply (dry run)" } else { "Apply pending operations" };
    modal_window(title).show(ctx, |ui| {
        ui.set_max_width(560.0);
        let sys_disk = s.queue.base().effective_system_disk();
        if system.iter().any(|x| *x) {
            egui::Frame::none().fill(DANGER).rounding(4.0).inner_margin(8.0).show(ui, |ui| {
                let disk = sys_disk.map_or(String::from("the system disk"), |d| format!("Disk {d}"));
                ui.label(
                    RichText::new(format!(
                        "{} These operations change {disk}, the disk Windows is running from. \
                         A failure can leave the computer unable to start.",
                        icons::WARNING
                    ))
                    .color(Color32::WHITE)
                    .strong(),
                );
            });
            ui.checkbox(
                allow_system,
                "Allow resize, drive letter, label and mount changes on the system disk",
            )
            .on_hover_text("Destructive operations on the system disk are always refused");
            ui.add_space(4.0);
        }
        if s.dry_run {
            ui.label("Dry run: nothing is written; each operation is validated against the disks.");
        }
        ui.label(format!("{} operation(s) will run in this order:", ops.len()));
        egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
            for (i, op) in ops.iter().enumerate() {
                let mut text = format!("{}. {}", i + 1, op.summary());
                if system.get(i).copied().unwrap_or(false) {
                    text.push_str("  [system disk]");
                }
                let rt = RichText::new(text);
                let rt = if destructive.get(i).copied().unwrap_or(true) { rt.color(DANGER) } else { rt };
                ui.add(egui::Label::new(rt).wrap());
            }
        });
        if destructive.iter().any(|d| *d) && !s.dry_run {
            ui.add_space(4.0);
            ui.colored_label(DANGER, "Operations in red overwrite or remove data. Make sure you have a backup.");
        }
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Cancel").clicked() {
                keep = false;
            }
            let label = if s.dry_run { "Simulate" } else { "Apply" };
            if ui.button(RichText::new(label).strong()).clicked() {
                out.push(Cmd::ConfirmApply { allow_system: *allow_system });
                keep = false;
            }
        });
    });
    keep
}
