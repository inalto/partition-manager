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
        if p.total > 0 {
            let frac = (p.done as f64 / p.total as f64).clamp(0.0, 1.0) as f32;
            ui.add(egui::ProgressBar::new(frac).show_percentage());
        } else {
            // preparing (reading the disks, scanning used space, ...): no byte count yet
            activity_bar(ui);
        }
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
            if p.total > 0 {
                ui.label("Time left");
                ui.label(format_eta(p.eta_secs));
                ui.end_row();
            }
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

/// Fraction of the bar the moving segment of [`activity_bar`] covers.
const ACTIVITY_SEGMENT: f32 = 0.3;
/// Seconds the segment takes to cross the bar once.
const ACTIVITY_PERIOD: f64 = 1.4;

/// Left edge of the moving segment at `time` seconds, as a fraction of the bar width: it
/// enters from the left edge, leaves through the right one, and starts over.
pub fn activity_offset(time: f64) -> f32 {
    let t = (time.rem_euclid(ACTIVITY_PERIOD) / ACTIVITY_PERIOD) as f32;
    t * (1.0 + ACTIVITY_SEGMENT) - ACTIVITY_SEGMENT
}

/// An indeterminate progress bar: a segment sweeping from left to right for as long as it
/// is drawn (work is running, but how much is left is not known).
pub fn activity_bar(ui: &mut egui::Ui) {
    let size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let rounding = rect.height() / 2.0;
        ui.painter().rect_filled(rect, rounding, ui.visuals().extreme_bg_color);
        let x = rect.left() + activity_offset(ui.input(|i| i.time)) * rect.width();
        let segment = egui::Rect::from_min_max(
            egui::pos2(x.max(rect.left()), rect.top()),
            egui::pos2((x + ACTIVITY_SEGMENT * rect.width()).min(rect.right()), rect.bottom()),
        );
        if segment.width() > 0.0 {
            ui.painter().rect_filled(segment, rounding, ui.visuals().selection.bg_fill);
        }
    }
    ui.ctx().request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_segment_sweeps_across_and_wraps() {
        assert_eq!(activity_offset(0.0), -ACTIVITY_SEGMENT);
        let mid = activity_offset(ACTIVITY_PERIOD / 2.0);
        assert!(mid > 0.0 && mid < 1.0, "{mid}");
        assert!(activity_offset(ACTIVITY_PERIOD * 0.999) > 0.99);
        assert!((activity_offset(ACTIVITY_PERIOD * 3.25) - activity_offset(ACTIVITY_PERIOD * 0.25)).abs() < 1e-3);
    }
}
