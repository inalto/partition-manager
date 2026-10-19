//! Surface Test window: a disk or partition and a block size; Start / Stop; a grid of squares
//! (one per block, bucketed to at most [`MAX_CELLS`] cells on large devices) that turns green
//! as the read test progresses and, at the end, red / orange for bad / slow blocks from the
//! [`SurfaceReport`].

use super::{device_combo, error_text, open_range, progress_panel, tool_frame, Device, Task, ToolEnv, ToolWindow, WindowKind, WindowOutcome};
use eframe::egui::{self, pos2, vec2, Color32, Rect, Sense};
use pm_analyze::{surface_test, SurfaceReport, DEFAULT_BLOCK};
use pm_core::format_size;
use pm_platform::OpenMode;
use std::time::Duration;

/// Block sizes offered.
pub const BLOCKS: [u64; 5] = [64 << 10, 256 << 10, 1 << 20, 4 << 20, 16 << 20];
/// Most squares drawn.
pub const MAX_CELLS: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    Untested,
    Ok,
    Slow,
    Bad,
}

/// Blocks per cell when `blocks` blocks are shown in at most `max_cells` cells.
pub fn blocks_per_cell(blocks: u64, max_cells: usize) -> u64 {
    blocks.div_ceil(max_cells.max(1) as u64).max(1)
}

/// The cells for `blocks` blocks: during the run, the cells whose blocks are all below
/// `done_blocks` are `Ok`; with the final `report`, every cell takes the worst state of its
/// blocks (bad > slow > ok).
pub fn cells(blocks: u64, max_cells: usize, done_blocks: u64, report: Option<&SurfaceReport>) -> Vec<Cell> {
    let per = blocks_per_cell(blocks, max_cells);
    let n = blocks.div_ceil(per) as usize;
    let mut out: Vec<Cell> = (0..n)
        .map(|i| {
            let end = ((i as u64 + 1) * per).min(blocks);
            if report.is_some() || end <= done_blocks {
                Cell::Ok
            } else {
                Cell::Untested
            }
        })
        .collect();
    if let Some(r) = report {
        for &b in &r.slow {
            if let Some(c) = out.get_mut((b as u64 / per) as usize) {
                if *c != Cell::Bad {
                    *c = Cell::Slow;
                }
            }
        }
        for &b in &r.bad {
            if let Some(c) = out.get_mut((b as u64 / per) as usize) {
                *c = Cell::Bad;
            }
        }
    }
    out
}

pub struct SurfaceTestWindow {
    pub device: Option<Device>,
    pub block: u64,
    task: Option<Task<SurfaceReport>>,
    /// Length and block size of the current / last run.
    run: Option<(u64, u64)>,
    report: Option<SurfaceReport>,
    status: Option<String>,
}

impl SurfaceTestWindow {
    pub fn new(device: Option<Device>) -> SurfaceTestWindow {
        SurfaceTestWindow { device, block: DEFAULT_BLOCK, task: None, run: None, report: None, status: None }
    }

    fn start(&mut self, env: &ToolEnv) {
        let Some(dev) = self.device else { return };
        let Some((start, len)) = dev.range(env.live) else { return };
        let range = matches!(dev, Device::Partition(..)).then_some((start, len));
        let (p, disk, block) = (env.platform.clone(), dev.disk(), self.block);
        self.run = Some((len, block));
        self.report = None;
        self.status = None;
        self.task = Some(Task::spawn("Surface test", move |progress, cancel| {
            let mut d = open_range(&*p, disk, range, OpenMode::ReadOnly)?;
            surface_test(&mut *d, block, progress, cancel)
        }));
    }
}

impl ToolWindow for SurfaceTestWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::SurfaceTest
    }

    fn show(&mut self, ctx: &egui::Context, env: &ToolEnv) -> WindowOutcome {
        if let Some(t) = self.task.as_mut() {
            ctx.request_repaint_after(Duration::from_millis(150));
            if let Some(r) = t.poll() {
                match r {
                    Ok(rep) => {
                        self.status = Some(format!(
                            "Finished in {}: {} bad block(s), {} slow block(s).",
                            super::short_duration(Duration::from_secs_f64(rep.elapsed_secs.max(0.0))),
                            rep.bad.len(),
                            rep.slow.len()
                        ));
                        self.report = Some(rep);
                    }
                    Err(e) => self.status = Some(error_text(&e)),
                }
                self.task = None;
            }
        }
        if tool_frame(ctx, "Surface Test", [720.0, 600.0], |ui| self.ui(ui, env)) {
            WindowOutcome::Open
        } else {
            WindowOutcome::Close
        }
    }
}

impl SurfaceTestWindow {
    fn ui(&mut self, ui: &mut egui::Ui, env: &ToolEnv) {
        let running = self.task.is_some();
        ui.add_enabled_ui(!running, |ui| {
            ui.horizontal(|ui| {
                ui.label("Device:");
                device_combo(ui, "surface_dev", &mut self.device, &Device::all(env.live), env.live);
            });
            ui.horizontal(|ui| {
                ui.label("Block size:");
                egui::ComboBox::from_id_salt("surface_block").selected_text(format_size(self.block)).show_ui(ui, |ui| {
                    for b in BLOCKS {
                        ui.selectable_value(&mut self.block, b, format_size(b));
                    }
                });
            });
        });
        ui.horizontal(|ui| {
            if running {
                if ui.button("Stop").clicked() {
                    if let Some(t) = &self.task {
                        t.cancel();
                    }
                }
            } else if ui.add_enabled(self.device.is_some(), egui::Button::new("Start")).clicked() {
                self.start(env);
            }
            ui.weak("Reads every block; nothing is written.");
        });
        ui.separator();
        let done_bytes = match &self.task {
            Some(t) => {
                let p = t.progress();
                if progress_panel(ui, &p, t.elapsed(), t.is_cancelled(), true) {
                    t.cancel();
                }
                p.done
            }
            None => 0,
        };
        if let Some(s) = &self.status {
            ui.label(s);
        }
        let Some((len, block)) = self.run else { return };
        let blocks = len.div_ceil(block.max(1));
        let report = self.report.as_ref();
        let cs = cells(blocks, MAX_CELLS, done_bytes / block.max(1), report);
        let count = |c: Cell| cs.iter().filter(|x| **x == c).count();
        ui.horizontal(|ui| {
            legend(ui, Color32::from_rgb(0x4C, 0xAF, 0x50), format!("OK ({})", count(Cell::Ok)));
            legend(ui, Color32::from_rgb(0xE0, 0x80, 0x10), format!("Slow ({})", count(Cell::Slow)));
            legend(ui, crate::theme::DANGER, format!("Bad ({})", count(Cell::Bad)));
            ui.label(format!(
                "{} blocks of {}, {} per square",
                blocks,
                format_size(block),
                blocks_per_cell(blocks, MAX_CELLS)
            ));
        });
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| grid(ui, &cs));
    }
}

fn color(c: Cell, ui: &egui::Ui) -> Color32 {
    match c {
        Cell::Untested => ui.visuals().widgets.inactive.bg_fill,
        Cell::Ok => Color32::from_rgb(0x4C, 0xAF, 0x50),
        Cell::Slow => Color32::from_rgb(0xE0, 0x80, 0x10),
        Cell::Bad => crate::theme::DANGER,
    }
}

fn legend(ui: &mut egui::Ui, c: Color32, text: String) {
    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().rect_filled(r, 1.0, c);
    ui.label(text);
}

fn grid(ui: &mut egui::Ui, cs: &[Cell]) {
    let side = if cs.len() <= 2_000 { 10.0 } else { 5.0 };
    let width = ui.available_width().max(100.0);
    let cols = ((width / (side + 1.0)).floor() as usize).max(1);
    let rows = cs.len().div_ceil(cols);
    let (rect, _) = ui.allocate_exact_size(vec2(width, rows as f32 * (side + 1.0)), Sense::hover());
    let painter = ui.painter_at(rect);
    for (i, c) in cs.iter().enumerate() {
        let (r, k) = (i / cols, i % cols);
        let min = pos2(rect.min.x + k as f32 * (side + 1.0), rect.min.y + r as f32 * (side + 1.0));
        painter.rect_filled(Rect::from_min_size(min, vec2(side, side)), 1.0, color(*c, ui));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_buckets_and_colours_blocks() {
        assert_eq!(blocks_per_cell(100, MAX_CELLS), 1);
        assert_eq!(blocks_per_cell(25_000, MAX_CELLS), 3);
        // during the run: fully tested cells only
        let c = cells(10, 4, 5, None);
        assert_eq!(c.len(), 4, "3 blocks per cell, last cell holds one");
        assert_eq!(c, [Cell::Ok, Cell::Untested, Cell::Untested, Cell::Untested]);
        let c = cells(10, 4, 10, None);
        assert!(c.iter().all(|x| *x == Cell::Ok));
        // at the end: bad beats slow beats ok
        let r = SurfaceReport { block: 1, blocks: 10, bad: vec![4], slow: vec![3, 5, 9], ms_per_block: vec![], elapsed_secs: 1.0 };
        assert_eq!(cells(10, 4, 0, Some(&r)), [Cell::Ok, Cell::Bad, Cell::Ok, Cell::Slow]);
        assert!(cells(30_000_000, MAX_CELLS, 0, None).len() <= MAX_CELLS);
    }
}
