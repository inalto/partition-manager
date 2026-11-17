//! Progress of a running job: phase, bytes done / total, rate, ETA, a progress bar and Cancel.

use super::modal_window;
use crate::app::{Cmd, RunningJob};
use crate::format_util::{format_eta, format_rate};
use crate::theme::{mix, ACCENT, DANGER, WARN};
use eframe::egui::{self, Color32, RichText};
use pm_core::{format_size, SliceState};

pub fn show(ctx: &egui::Context, job: &RunningJob, out: &mut Vec<Cmd>) {
    let p = job.handle.snapshot();
    modal_window(job.handle.name.as_str()).show(ctx, |ui| {
        ui.set_min_width(420.0);
        ui.label(RichText::new(&p.phase).strong());
        if !p.detail.is_empty() {
            ui.label(&p.detail);
        }
        if let Some(slices) = p.slices.as_deref().filter(|s| !s.is_empty()) {
            // a copy that reads a device: the bar is a map of the source, so damaged areas
            // show where they are
            slice_bar(ui, slices);
            if p.total > 0 {
                ui.label(percent(p.done, p.total));
            }
            if p.can_finish {
                ui.label(FINISH_HINT);
            }
        } else if p.total > 0 {
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
            // no estimate for the damaged areas: a sector takes a moment or the drive's
            // whole timeout, and the rate counts only what was read
            if p.total > 0 && !p.can_finish {
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
        } else if job.handle.is_finishing() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Finishing after the current read…");
            });
        } else {
            ui.horizontal(|ui| {
                if p.can_finish && ui.button("Finish now").on_hover_text(FINISH_TIP).clicked() {
                    out.push(Cmd::FinishJob);
                }
                let cancel = ui.button("Cancel");
                let cancel = if p.can_finish { cancel.on_hover_text(CANCEL_TIP) } else { cancel };
                if cancel.clicked() {
                    out.push(Cmd::CancelJob);
                }
            });
        }
    });
}

/// Shown while a rescue goes through the damaged areas, where it can sit for hours or days.
const FINISH_HINT: &str = "Everything was read once. The damaged areas are now read again one sector at a time, which can take hours or days. Finish now keeps the image with what was read.";
const FINISH_TIP: &str = "Stop reading the damaged areas and complete the image. What was not read is zeros in the image; run the same operation again to retry it.";
const CANCEL_TIP: &str = "Stop without completing the image: the .partial file and the mapfile are kept, and the same operation resumes from them.";

/// `done` of `total` as a percentage with one decimal, rounded down: "100.0%" only when all
/// of it is done.
pub fn percent(done: u64, total: u64) -> String {
    if total == 0 {
        return String::new();
    }
    let tenths = (done.min(total) as u128 * 1000 / total as u128) as u64;
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// Consecutive slices in the same state, as `(state, start, end)` in fractions of the bar.
pub fn slice_runs(slices: &[SliceState]) -> Vec<(SliceState, f32, f32)> {
    let n = slices.len() as f32;
    let mut out: Vec<(SliceState, f32, f32)> = Vec::new();
    for (i, s) in slices.iter().enumerate() {
        let end = (i + 1) as f32 / n;
        match out.last_mut() {
            Some(last) if last.0 == *s => last.2 = end,
            _ => out.push((*s, i as f32 / n, end)),
        }
    }
    out
}

/// What a state is called in the legend and the tooltip.
pub fn slice_name(s: SliceState) -> &'static str {
    match s {
        SliceState::Skipped => "Skipped (unused space)",
        SliceState::Done => "Copied",
        SliceState::Pending => "Not read yet",
        SliceState::Retrying => "Read error, retrying",
        SliceState::Bad => "Damaged",
    }
}

/// The states the legend under the bar explains: none while the copy is clean (the bar
/// then reads like any progress bar), else every state on the bar, worst first.
pub fn slice_legend(slices: &[SliceState]) -> Vec<SliceState> {
    let has = |s: SliceState| slices.contains(&s);
    if !has(SliceState::Bad) && !has(SliceState::Retrying) {
        return Vec::new();
    }
    [SliceState::Bad, SliceState::Retrying, SliceState::Done, SliceState::Skipped, SliceState::Pending]
        .into_iter()
        .filter(|s| has(*s))
        .collect()
}

fn slice_color(ui: &egui::Ui, s: SliceState) -> Color32 {
    let bg = ui.visuals().extreme_bg_color;
    match s {
        SliceState::Pending => bg,
        SliceState::Skipped => mix(bg, ACCENT, 0.35),
        SliceState::Done => ACCENT,
        SliceState::Retrying => WARN,
        SliceState::Bad => DANGER,
    }
}

/// Narrowest a damaged or retrying run is drawn, in points: one slice of a wide bar is
/// under a pixel, and these are the ones that must not be missed.
const MIN_TROUBLE_WIDTH: f32 = 3.0;

/// The progress bar of a copy as a map of its source: one colored stretch per run of
/// slices, damaged and retrying ones on top and never thinner than [`MIN_TROUBLE_WIDTH`].
/// Hovering names the state under the pointer; a legend appears once something failed.
pub fn slice_bar(ui: &mut egui::Ui, slices: &[SliceState]) {
    let size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let runs = slice_runs(slices);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
        let trouble = |s: SliceState| matches!(s, SliceState::Bad | SliceState::Retrying);
        for pass in [false, true] {
            for &(s, a, b) in runs.iter().filter(|r| trouble(r.0) == pass) {
                let (mut x0, mut x1) = (rect.left() + a * rect.width(), rect.left() + b * rect.width());
                if pass && x1 - x0 < MIN_TROUBLE_WIDTH {
                    let mid = ((x0 + x1) / 2.0).clamp(rect.left() + MIN_TROUBLE_WIDTH / 2.0, rect.right() - MIN_TROUBLE_WIDTH / 2.0);
                    (x0, x1) = (mid - MIN_TROUBLE_WIDTH / 2.0, mid + MIN_TROUBLE_WIDTH / 2.0);
                }
                let r = egui::Rect::from_min_max(egui::pos2(x0, rect.top()), egui::pos2(x1, rect.bottom()));
                painter.rect_filled(r, 0.0, slice_color(ui, s));
            }
        }
        painter.rect_stroke(rect, 2.0, ui.visuals().widgets.noninteractive.bg_stroke);
    }
    if let Some(pos) = resp.hover_pos() {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 0.999_999);
        let state = slices[(frac * slices.len() as f32) as usize];
        resp.on_hover_text(format!("{} (around {:.0}% of the source)", slice_name(state), frac * 100.0));
    }
    let legend = slice_legend(slices);
    if !legend.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for s in legend {
                let (sw, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                ui.painter().rect_filled(sw, 2.0, slice_color(ui, s));
                ui.painter().rect_stroke(sw, 2.0, ui.visuals().widgets.noninteractive.bg_stroke);
                let n = slices.iter().filter(|x| **x == s).count();
                let text = match s {
                    SliceState::Bad | SliceState::Retrying => format!("{} ({n} of {} slices)", slice_name(s), slices.len()),
                    _ => slice_name(s).to_string(),
                };
                ui.label(text);
                ui.add_space(6.0);
            }
        });
    }
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
    fn slices_group_into_runs_and_the_legend_waits_for_trouble() {
        use SliceState::*;
        assert!(slice_runs(&[]).is_empty());
        assert_eq!(
            slice_runs(&[Done, Done, Bad, Pending]),
            [(Done, 0.0, 0.5), (Bad, 0.5, 0.75), (Pending, 0.75, 1.0)]
        );
        // a clean copy needs no legend, also with skipped space
        assert!(slice_legend(&[Done, Skipped, Pending]).is_empty());
        // once something failed: what is on the bar, worst first
        assert_eq!(slice_legend(&[Done, Retrying, Pending]), [Retrying, Done, Pending]);
        assert_eq!(slice_legend(&[Skipped, Bad, Done, Bad]), [Bad, Done, Skipped]);
        assert_eq!(slice_name(Bad), "Damaged");
    }

    #[test]
    fn percent_reaches_100_only_when_all_is_done() {
        assert_eq!(percent(0, 1000), "0.0%");
        assert_eq!(percent(505, 1000), "50.5%");
        // 133.75 GB of 133.78 GB, with damaged areas left: not "100%"
        assert_eq!(percent(133_750, 133_780), "99.9%");
        assert_eq!(percent(999_999, 1_000_000), "99.9%");
        assert_eq!(percent(7, 7), "100.0%");
        assert_eq!(percent(9, 7), "100.0%");
        assert_eq!(percent(1, 0), "");
    }

    #[test]
    fn activity_segment_sweeps_across_and_wraps() {
        assert_eq!(activity_offset(0.0), -ACTIVITY_SEGMENT);
        let mid = activity_offset(ACTIVITY_PERIOD / 2.0);
        assert!(mid > 0.0 && mid < 1.0, "{mid}");
        assert!(activity_offset(ACTIVITY_PERIOD * 0.999) > 0.99);
        assert!((activity_offset(ACTIVITY_PERIOD * 3.25) - activity_offset(ACTIVITY_PERIOD * 0.25)).abs() < 1e-3);
    }
}
