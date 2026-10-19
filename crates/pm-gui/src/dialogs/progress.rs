//! Progress of a running job: phase, bytes done / total, rate, ETA, a progress bar and Cancel.

use super::modal_window;
use crate::app::{Cmd, RunningJob};
use crate::format_util::{format_eta, format_rate};
use eframe::egui::{self, RichText};
use pm_core::format_size;

pub fn show(ctx: &egui::Context, job: &RunningJob, out: &mut Vec<Cmd>) {
    let p = job.handle.snapshot();
    modal_window(job.handle.name.as_str()).show(ctx, |ui| {
        ui.set_min_width(420.0);
        ui.label(RichText::new(&p.phase).strong());
        if !p.detail.is_empty() {
            ui.label(&p.detail);
        }
        let frac = if p.total > 0 { (p.done as f64 / p.total as f64).clamp(0.0, 1.0) as f32 } else { 0.0 };
        ui.add(egui::ProgressBar::new(frac).show_percentage().animate(p.total == 0));
        egui::Grid::new("progress_grid").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
            if p.total > 0 {
                ui.label("Done");
                ui.label(format!("{} of {}", format_size(p.done), format_size(p.total)));
                ui.end_row();
            }
            let rate = format_rate(p.rate_bps);
            if !rate.is_empty() {
                ui.label("Speed");
                ui.label(rate);
                ui.end_row();
            }
            ui.label("Time left");
            ui.label(format_eta(p.eta_secs));
            ui.end_row();
            ui.label("Elapsed");
            ui.label(format_eta(Some(job.handle.started.elapsed().as_secs())));
            ui.end_row();
        });
        ui.separator();
        if job.handle.is_cancelled() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Cancelling after the current step…");
            });
        } else if ui.button("Cancel").clicked() {
            out.push(Cmd::CancelJob);
        }
    });
}
