//! Disk Benchmark window (MiniTool screenshot 5): drive, transfer sizes from / to, total
//! length, queue depth, threads, sequential / random, cooldown; Start / Stop; a read / write
//! bar chart per transfer size with value labels, and a table.
//!
//! The job benchmarks one transfer size at a time (`pm_analyze::benchmark` on an unbuffered
//! scratch file in the chosen folder) so the chart fills in as each size finishes.

use super::{error_text, progress_panel, tool_frame, Task, ToolEnv, ToolWindow, WindowKind, WindowOutcome};
use eframe::egui::{self, pos2, vec2, Align2, Color32, FontId, Rect, Sense, Stroke};
use pm_analyze::{benchmark, BenchMode, BenchOpts, BenchPoint};
use pm_core::{format_size, CancelToken, Error, GIB, MIB};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Transfer sizes offered (4 KB … 1 MB).
pub const SIZES: [u32; 9] = [4 << 10, 8 << 10, 16 << 10, 32 << 10, 64 << 10, 128 << 10, 256 << 10, 512 << 10, 1 << 20];
/// Total lengths offered.
pub const LENGTHS: [u64; 4] = [100 * MIB, 200 * MIB, 500 * MIB, GIB];

pub const READ_COLOR: Color32 = Color32::from_rgb(0x3C, 0x78, 0xC8);
pub const WRITE_COLOR: Color32 = Color32::from_rgb(0xE0, 0x80, 0x10);

/// The transfer sizes from index `from` to index `to` of [`SIZES`] (either order).
pub fn sizes_between(from: usize, to: usize) -> Vec<u32> {
    let (a, b) = (from.min(to), from.max(to).min(SIZES.len() - 1));
    SIZES[a.min(b)..=b].to_vec()
}

/// Drive roots (`"E:\"`) of the live model, with a description.
pub fn drive_roots(env: &ToolEnv) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for d in &env.live.disks {
        for p in &d.partitions {
            if let Some(c) = p.letter() {
                let fs = p.fs.map_or("", |f| f.display_name());
                out.push((format!("{c}:\\"), format!("{c}: {} {fs} ({})", p.label, format_size(p.len))));
            }
        }
    }
    for v in &env.live.volumes {
        for m in &v.mount_points {
            let b = m.as_bytes();
            if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
                let root = format!("{}:\\", (b[0] as char).to_ascii_uppercase());
                if !out.iter().any(|o| o.0 == root) {
                    out.push((root.clone(), format!("{} {}", &root[..2], v.label)));
                }
            }
        }
    }
    out.sort();
    out
}

pub struct BenchmarkWindow {
    pub path: String,
    pub from: usize,
    pub to: usize,
    pub length: usize,
    pub queue_depth: u32,
    pub threads: u32,
    pub mode: BenchMode,
    pub cooldown: u32,
    task: Option<Task<()>>,
    /// Sizes of the current / last run.
    planned: Vec<u32>,
    points: Arc<Mutex<Vec<BenchPoint>>>,
    status: Option<String>,
}

impl BenchmarkWindow {
    pub fn new(path: Option<String>) -> BenchmarkWindow {
        let d = BenchOpts::default();
        BenchmarkWindow {
            path: path.unwrap_or_default(),
            from: 0,
            to: 3,
            length: 1,
            queue_depth: d.queue_depth,
            threads: d.threads,
            mode: d.mode,
            cooldown: d.cooldown_secs,
            task: None,
            planned: Vec::new(),
            points: Arc::new(Mutex::new(Vec::new())),
            status: None,
        }
    }

    /// The options of one run (all sizes; the job runs them one at a time).
    pub fn opts(&self) -> BenchOpts {
        BenchOpts {
            transfer_sizes: sizes_between(self.from, self.to),
            total_len: LENGTHS[self.length.min(LENGTHS.len() - 1)],
            queue_depth: self.queue_depth.max(1),
            threads: self.threads.max(1),
            mode: self.mode,
            cooldown_secs: self.cooldown,
        }
    }

    fn start(&mut self) {
        let opts = self.opts();
        let dir = PathBuf::from(self.path.trim());
        self.planned = opts.transfer_sizes.clone();
        self.points = Arc::new(Mutex::new(Vec::new()));
        self.status = None;
        let points = self.points.clone();
        self.task = Some(Task::spawn("Disk benchmark", move |progress, cancel| {
            if !dir.is_dir() {
                return Err(Error::Invalid(format!("{} is not a folder or drive", dir.display())));
            }
            for (i, &size) in opts.transfer_sizes.iter().enumerate() {
                if i > 0 {
                    cool_down(opts.cooldown_secs, cancel)?;
                }
                let one = BenchOpts { transfer_sizes: vec![size], cooldown_secs: 0, ..opts.clone() };
                let p = run_one(&dir, &one, progress, cancel)?;
                points.lock().unwrap_or_else(|e| e.into_inner()).extend(p);
            }
            Ok(())
        }));
    }
}

#[cfg(windows)]
fn run_one(dir: &std::path::Path, o: &BenchOpts, p: &dyn pm_core::ProgressSink, c: &CancelToken) -> pm_core::Result<Vec<BenchPoint>> {
    benchmark(&pm_platform::FileBenchTarget::new(dir.to_path_buf()), o, p, c)
}

#[cfg(not(windows))]
fn run_one(_dir: &std::path::Path, _o: &BenchOpts, _p: &dyn pm_core::ProgressSink, _c: &CancelToken) -> pm_core::Result<Vec<BenchPoint>> {
    let _ = benchmark;
    Err(Error::Unsupported("the disk benchmark needs Windows".into()))
}

/// Sleep `secs` seconds, checking for cancellation every 100 ms.
fn cool_down(secs: u32, cancel: &CancelToken) -> pm_core::Result<()> {
    for _ in 0..secs * 10 {
        cancel.check()?;
        std::thread::sleep(Duration::from_millis(100));
    }
    cancel.check()
}

fn size_text(s: u32) -> String {
    if s >= 1 << 20 {
        format!("{} MB", s >> 20)
    } else {
        format!("{} KB", s >> 10)
    }
}

impl ToolWindow for BenchmarkWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Benchmark
    }

    fn show(&mut self, ctx: &egui::Context, env: &ToolEnv) -> WindowOutcome {
        if let Some(t) = self.task.as_mut() {
            ctx.request_repaint_after(Duration::from_millis(150));
            if let Some(r) = t.poll() {
                self.status = Some(match r {
                    Ok(()) => "Benchmark finished.".into(),
                    Err(e) => error_text(&e),
                });
                self.task = None;
            }
        }
        let open = tool_frame(ctx, "Disk Benchmark", [760.0, 620.0], |ui| self.ui(ui, env));
        if open {
            WindowOutcome::Open
        } else {
            WindowOutcome::Close
        }
    }
}

impl BenchmarkWindow {
    fn ui(&mut self, ui: &mut egui::Ui, env: &ToolEnv) {
        let running = self.task.is_some();
        ui.add_enabled_ui(!running, |ui| {
            egui::Grid::new("bench_controls").num_columns(4).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Drive:");
                let roots = drive_roots(env);
                egui::ComboBox::from_id_salt("bench_drive")
                    .width(200.0)
                    .selected_text(if self.path.is_empty() { "Choose…".to_string() } else { self.path.clone() })
                    .show_ui(ui, |ui| {
                        for (root, desc) in &roots {
                            ui.selectable_value(&mut self.path, root.clone(), desc);
                        }
                    });
                ui.label("or folder:");
                ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(180.0).hint_text("D:\\"));
                ui.end_row();

                ui.label("Transfer size from:");
                size_combo(ui, "bench_from", &mut self.from);
                ui.label("to:");
                size_combo(ui, "bench_to", &mut self.to);
                ui.end_row();

                ui.label("Total length:");
                egui::ComboBox::from_id_salt("bench_len").selected_text(format_size(LENGTHS[self.length])).show_ui(
                    ui,
                    |ui| {
                        for (i, l) in LENGTHS.iter().enumerate() {
                            ui.selectable_value(&mut self.length, i, format_size(*l));
                        }
                    },
                );
                ui.label("Queue number:");
                ui.add(egui::DragValue::new(&mut self.queue_depth).range(1..=32));
                ui.end_row();

                ui.label("Thread number:");
                ui.add(egui::DragValue::new(&mut self.threads).range(1..=16));
                ui.label("Test mode:");
                egui::ComboBox::from_id_salt("bench_mode")
                    .selected_text(match self.mode {
                        BenchMode::Sequential => "Sequential",
                        BenchMode::Random => "Random",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.mode, BenchMode::Sequential, "Sequential");
                        ui.selectable_value(&mut self.mode, BenchMode::Random, "Random");
                    });
                ui.end_row();

                ui.label("Cooldown (s):");
                ui.add(egui::DragValue::new(&mut self.cooldown).range(0..=60));
                ui.end_row();
            });
        });
        ui.horizontal(|ui| {
            if running {
                if ui.button("Stop").clicked() {
                    if let Some(t) = &self.task {
                        t.cancel();
                    }
                }
            } else if ui.add_enabled(!self.path.trim().is_empty(), egui::Button::new("Start")).clicked() {
                self.start();
            }
            ui.weak("A scratch file of the total length is written to the drive and deleted afterwards.");
        });
        ui.separator();
        let points = self.points.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(t) = &self.task {
            let p = t.progress();
            ui.label(format!("Transfer size {} of {}", (points.len() + 1).min(self.planned.len()), self.planned.len()));
            if progress_panel(ui, &p, t.elapsed(), t.is_cancelled(), true) {
                t.cancel();
            }
        }
        if let Some(s) = &self.status {
            ui.label(s);
        }
        if !self.planned.is_empty() {
            bar_chart(ui, &self.planned, &points);
            ui.add_space(6.0);
            egui::Grid::new("bench_table").num_columns(5).striped(true).spacing([18.0, 3.0]).show(ui, |ui| {
                for h in ["Transfer size", "Read MB/s", "Write MB/s", "Read IOPS", "Write IOPS"] {
                    ui.strong(h);
                }
                ui.end_row();
                for p in &points {
                    ui.label(size_text(p.size));
                    ui.label(format!("{:.1}", p.read_mbps));
                    ui.label(format!("{:.1}", p.write_mbps));
                    ui.label(format!("{:.0}", p.read_iops));
                    ui.label(format!("{:.0}", p.write_iops));
                    ui.end_row();
                }
            });
        }
    }
}

fn size_combo(ui: &mut egui::Ui, id: &str, idx: &mut usize) {
    egui::ComboBox::from_id_salt(id).selected_text(size_text(SIZES[*idx])).show_ui(ui, |ui| {
        for (i, s) in SIZES.iter().enumerate() {
            ui.selectable_value(idx, i, size_text(*s));
        }
    });
}

/// Read (blue) and write (orange) MB/s per planned transfer size, with value labels.
fn bar_chart(ui: &mut egui::Ui, planned: &[u32], points: &[BenchPoint]) {
    let width = ui.available_width().max(300.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 240.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let text = ui.visuals().text_color();
    let grid = ui.visuals().widgets.noninteractive.bg_stroke.color;
    let plot = Rect::from_min_max(pos2(rect.min.x + 44.0, rect.min.y + 18.0), pos2(rect.max.x - 6.0, rect.max.y - 22.0));
    let max = points.iter().map(|p| p.read_mbps.max(p.write_mbps)).fold(0.0, f64::max).max(1.0);
    for i in 0..=4 {
        let v = max * i as f64 / 4.0;
        let y = plot.max.y - plot.height() * i as f32 / 4.0;
        painter.line_segment([pos2(plot.min.x, y), pos2(plot.max.x, y)], Stroke::new(1.0_f32, grid));
        painter.text(pos2(plot.min.x - 4.0, y), Align2::RIGHT_CENTER, format!("{v:.0}"), FontId::proportional(10.0), text);
    }
    painter.text(pos2(rect.min.x, rect.min.y), Align2::LEFT_TOP, "MB/s", FontId::proportional(10.0), text);
    for (k, (name, color)) in [("Read", READ_COLOR), ("Write", WRITE_COLOR)].into_iter().enumerate() {
        let x = plot.max.x - 120.0 + 60.0 * k as f32;
        painter.rect_filled(Rect::from_min_size(pos2(x, rect.min.y + 2.0), vec2(9.0, 9.0)), 1.0, color);
        painter.text(pos2(x + 12.0, rect.min.y), Align2::LEFT_TOP, name, FontId::proportional(11.0), text);
    }
    let n = planned.len().max(1);
    let group = plot.width() / n as f32;
    let bar = (group * 0.35).min(40.0);
    for (i, size) in planned.iter().enumerate() {
        let cx = plot.min.x + group * (i as f32 + 0.5);
        painter.text(pos2(cx, plot.max.y + 4.0), Align2::CENTER_TOP, size_text(*size), FontId::proportional(10.0), text);
        let Some(p) = points.iter().find(|p| p.size == *size) else { continue };
        for (k, (v, color)) in [(p.read_mbps, READ_COLOR), (p.write_mbps, WRITE_COLOR)].into_iter().enumerate() {
            let h = (v / max) as f32 * plot.height();
            let x0 = cx - bar + bar * k as f32;
            let r = Rect::from_min_max(pos2(x0, plot.max.y - h), pos2(x0 + bar - 2.0, plot.max.y));
            painter.rect_filled(r, 1.0, color);
            painter.text(pos2(r.center().x, r.min.y - 2.0), Align2::CENTER_BOTTOM, format!("{v:.0}"), FontId::proportional(9.0), text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_size_ranges_and_options() {
        assert_eq!(sizes_between(0, 3), [4096, 8192, 16384, 32768]);
        assert_eq!(sizes_between(3, 0), [4096, 8192, 16384, 32768], "either order");
        assert_eq!(sizes_between(8, 8), [1 << 20]);
        let mut w = BenchmarkWindow::new(Some("E:\\".into()));
        w.length = 3;
        w.mode = BenchMode::Random;
        w.threads = 0;
        let o = w.opts();
        assert_eq!(o.total_len, GIB);
        assert_eq!(o.threads, 1, "never zero");
        assert_eq!(o.mode, BenchMode::Random);
        assert_eq!(size_text(4096), "4 KB");
        assert_eq!(size_text(1 << 20), "1 MB");
    }
}
