//! Tool windows (Benchmark, Space Analyzer, Surface Test, SMART) and the shared plumbing of
//! every window that runs a job: [`Task`] (a typed [`crate::jobs`] job), [`ToolWindow`] and
//! [`ToolEnv`] (how the main window drives them), the [`Device`] helpers and
//! [`wizard_window`], the step frame of the job-running wizards (Rescue Copy, Partition
//! Recovery, Data Recovery).
//!
//! Every disk read or write of a window happens in its [`Task`] on a worker thread; dropping a
//! `Task` (closing its window) cancels it.

pub mod benchmark;
pub mod smart;
pub mod space_analyzer;
pub mod surface_test;
pub mod treemap;

use crate::format_util::{format_eta, format_rate};
use crate::jobs::{spawn_job, JobHandle, JobOutput};
use crate::selection::Selected;
use crate::theme::DANGER;
use crate::views::icons;
use eframe::egui::{self, Align, Align2, Layout, RichText};
use pm_core::{format_size, BlockDevice, CancelToken, Error, ProgressReport, ProgressSink, SliceDevice};
use pm_ops::{Model, Operation};
use pm_platform::{OpenMode, Platform};
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

/// Which window a [`ToolWindow`] is (at most one of each kind is open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowKind {
    Benchmark,
    SpaceAnalyzer,
    SurfaceTest,
    Smart,
    RescueCopy,
    PartitionRecovery,
    DataRecovery,
}

/// What a window can see of the application.
pub struct ToolEnv<'a> {
    pub platform: &'a Arc<dyn Platform>,
    /// The disks as they are now (the queue's base model): tools act on the real disks, not
    /// on the preview of the pending operations.
    pub live: &'a Model,
    /// The pending operations.
    pub pending: &'a [Operation],
}

impl ToolEnv<'_> {
    /// Why nothing may be written to `disk` right now, if it may not: it is the running system
    /// disk (or that cannot be determined: fail closed, like the executor), or pending
    /// operations are planned on it.
    pub fn write_problem(&self, disk: u32) -> Option<String> {
        write_problem(self.live, self.pending, disk)
    }
}

/// See [`ToolEnv::write_problem`].
pub fn write_problem(live: &Model, pending: &[Operation], disk: u32) -> Option<String> {
    match live.effective_system_disk() {
        Some(s) if s == disk => return Some(format!("Disk {disk} is the running system disk; refusing to write to it.")),
        None => {
            return Some(format!(
                "Disk {disk}: cannot determine the running system disk; refusing to write to it."
            ))
        }
        Some(_) => {}
    }
    pending.iter().any(|op| op.touches_disk(disk)).then(|| {
        format!("Pending operations are planned on Disk {disk}; apply or discard them first.")
    })
}

/// What a window wants after a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowOutcome {
    Open,
    Close,
    /// Stay open and re-read the disks (something was written).
    Refresh,
}

/// A window drawn over the main window once per frame.
pub trait ToolWindow {
    fn kind(&self) -> WindowKind;
    /// Modal windows (the job wizards) disable the main window while open.
    fn modal(&self) -> bool {
        false
    }
    /// A job that writes is running: the window cannot be closed and neither can the app.
    fn writing(&self) -> bool {
        false
    }
    fn show(&mut self, ctx: &egui::Context, env: &ToolEnv) -> WindowOutcome;
}

/// A [`crate::jobs`] job with a typed result. Dropping it cancels the job.
pub struct Task<T> {
    handle: JobHandle,
    _out: PhantomData<fn() -> T>,
}

impl<T: Send + 'static> Task<T> {
    pub fn spawn<F>(name: &str, f: F) -> Task<T>
    where
        F: FnOnce(&dyn ProgressSink, &CancelToken) -> pm_core::Result<T> + Send + 'static,
    {
        let handle = spawn_job(name, move |p, c| f(p, c).map(|v| JobOutput::Any(Box::new(v))));
        Task { handle, _out: PhantomData }
    }

    /// The result once the job has finished (once), without blocking.
    pub fn poll(&mut self) -> Option<pm_core::Result<T>> {
        Some(match self.handle.poll()? {
            Ok(JobOutput::Any(b)) => {
                b.downcast::<T>().map(|v| *v).map_err(|_| Error::Invalid("job returned an unexpected result".into()))
            }
            Ok(other) => Err(Error::Invalid(format!("job returned an unexpected result {other:?}"))),
            Err(e) => Err(e),
        })
    }
}

impl<T> Task<T> {
    pub fn progress(&self) -> ProgressReport {
        self.handle.snapshot()
    }
    pub fn cancel(&self) {
        self.handle.cancel();
    }
    pub fn is_cancelled(&self) -> bool {
        self.handle.is_cancelled()
    }
    pub fn elapsed(&self) -> Duration {
        self.handle.started.elapsed()
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.handle.cancel();
    }
}

/// The text of a job error for the user.
pub fn error_text(e: &Error) -> String {
    match e {
        Error::Cancelled => "Cancelled.".into(),
        Error::Invalid(m) => m.clone(),
        other => other.to_string(),
    }
}

/// Phase, progress bar, bytes / rate / elapsed of a running task and a Cancel button; returns
/// `true` when Cancel was clicked. `bytes`: `done` / `total` are bytes (else plain counts).
pub fn progress_panel(ui: &mut egui::Ui, p: &ProgressReport, elapsed: Duration, cancelled: bool, bytes: bool) -> bool {
    ui.label(RichText::new(&p.phase).strong());
    if !p.detail.is_empty() {
        ui.label(&p.detail);
    }
    let frac = if p.total > 0 { (p.done as f64 / p.total as f64).clamp(0.0, 1.0) as f32 } else { 0.0 };
    ui.add(egui::ProgressBar::new(frac).show_percentage().animate(p.total == 0));
    let amount = |v: u64| if bytes { format_size(v) } else { v.to_string() };
    let mut line = if p.total > 0 { format!("{} of {}", amount(p.done), amount(p.total)) } else { amount(p.done) };
    let rate = format_rate(p.rate_bps);
    if bytes && !rate.is_empty() {
        line.push_str(&format!("   {rate}"));
    }
    line.push_str(&format!("   elapsed {}", format_eta(Some(elapsed.as_secs()))));
    if p.eta_secs.is_some() {
        line.push_str(&format!("   left {}", format_eta(p.eta_secs)));
    }
    ui.label(line);
    if cancelled {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Cancelling after the current step…");
        });
        false
    } else {
        ui.button("Cancel").clicked()
    }
}

/// A disk or a partition of the live model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    Disk(u32),
    Partition(u32, u32),
}

impl Device {
    pub fn disk(self) -> u32 {
        match self {
            Device::Disk(d) | Device::Partition(d, _) => d,
        }
    }

    /// The device of a selection (unallocated space: its disk).
    pub fn from_selection(sel: Selected) -> Option<Device> {
        match sel {
            Selected::None => None,
            Selected::Disk(d) | Selected::Free(d, _, _) => Some(Device::Disk(d)),
            Selected::Partition(d, n) => Some(Device::Partition(d, n)),
        }
    }

    /// `(start, len)` on its disk.
    pub fn range(self, m: &Model) -> Option<(u64, u64)> {
        match self {
            Device::Disk(d) => m.disk(d).map(|d| (0, d.size)),
            Device::Partition(d, n) => m.partition(d, n).map(|p| (p.start, p.len)),
        }
    }

    pub fn len(self, m: &Model) -> Option<u64> {
        self.range(m).map(|r| r.1)
    }

    pub fn describe(self, m: &Model) -> String {
        match self {
            Device::Disk(d) => m.disk(d).map_or(format!("Disk {d}"), |d| d.display_name()),
            Device::Partition(d, n) => match m.partition(d, n) {
                Some(p) => crate::wizards::partition_desc(d, p),
                None => format!("Partition {n} on Disk {d}"),
            },
        }
    }

    /// Every disk of `m`, each followed by its partitions (extended containers excluded).
    pub fn all(m: &Model) -> Vec<Device> {
        let mut out = Vec::new();
        for d in &m.disks {
            out.push(Device::Disk(d.index));
            for p in &d.partitions {
                if !crate::items::is_container(d, p) && p.len > 0 {
                    out.push(Device::Partition(d.index, p.number));
                }
            }
        }
        out
    }

    /// Drive letters stored on the device.
    pub fn letters(self, m: &Model) -> Vec<char> {
        let Some(d) = m.disk(self.disk()) else { return Vec::new() };
        d.partitions
            .iter()
            .filter(|p| match self {
                Device::Disk(_) => true,
                Device::Partition(_, n) => p.number == n,
            })
            .filter_map(|p| p.letter())
            .collect()
    }
}

/// The drive letter of the absolute path `p`, if it has one (`E:\x`, `\\?\E:\x`).
///
/// Limitation of every letter-based check built on this: a path reaching a volume through a
/// folder mount point, a junction, a UNC share or a `\\?\Volume{…}` name has no (or another)
/// drive letter and is not recognised as being on that volume.
pub fn path_letter(p: &std::path::Path) -> Option<char> {
    use std::path::{Component, Prefix};
    match p.components().next()? {
        Component::Prefix(x) => match x.kind() {
            Prefix::Disk(b) | Prefix::VerbatimDisk(b) => Some((b as char).to_ascii_uppercase()),
            _ => None,
        },
        _ => None,
    }
}

/// Why the file `path` (described as `what`) may not be used because it lies on one of
/// `letters`, the drives of `device` (a device being read or written), if it may not.
/// Only drive-letter paths are recognised (see [`path_letter`]): folder mount points,
/// junctions, UNC and `\\?\Volume{…}` paths are not covered. Relative paths have no letter;
/// callers that must not miss them require absolute paths.
pub fn on_letters_problem(path: &str, letters: &[char], what: &str, device: &str) -> Option<String> {
    let c = path_letter(std::path::Path::new(path.trim()))?;
    letters
        .contains(&c)
        .then(|| format!("The {what} is on drive {c}:, which is on {device}; choose a location on another disk."))
}

/// Open `disk` of `platform` (on a worker thread), whole or the `range` slice of it.
pub fn open_range(
    platform: &dyn Platform,
    disk: u32,
    range: Option<(u64, u64)>,
    mode: OpenMode,
) -> pm_core::Result<Box<dyn BlockDevice>> {
    let dev = platform.open_disk(disk, mode)?;
    match range {
        None => Ok(dev),
        Some((start, len)) => Ok(Box::new(SliceDevice::new(dev, start, len)?)),
    }
}

/// Combo box over `devices`; returns `true` when the choice changed.
pub fn device_combo(ui: &mut egui::Ui, id: &str, choice: &mut Option<Device>, devices: &[Device], m: &Model) -> bool {
    let before = *choice;
    let text = choice.map_or("Choose a device…".to_string(), |d| d.describe(m));
    egui::ComboBox::from_id_salt(id).width(420.0).selected_text(text).show_ui(ui, |ui| {
        for d in devices {
            let label = match d {
                Device::Disk(_) => d.describe(m),
                Device::Partition(..) => format!("    {}", d.describe(m)),
            };
            ui.selectable_value(choice, Some(*d), label);
        }
    });
    *choice != before
}

/// A red warning line.
pub fn danger(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.colored_label(DANGER, format!("{} {}", icons::WARNING, text.into()));
}

/// Orange (warning) text.
pub const WARN: egui::Color32 = egui::Color32::from_rgb(0xE0, 0x80, 0x10);

/// The buttons under a [`wizard_window`] page.
#[derive(Clone, Debug, Default)]
pub struct Footer {
    /// Back is offered.
    pub back: bool,
    /// The forward button: label and whether it is enabled (`None`: no forward button).
    pub next: Option<(String, bool)>,
    /// The close button's label (`None`: closing is not allowed now, e.g. while writing).
    pub close: Option<String>,
    /// Shown in red above the buttons.
    pub problem: Option<String>,
}

impl Footer {
    /// Back (unless on the first page), "Next >" enabled when `problem` is `None`, Cancel.
    pub fn step(first: bool, problem: Option<String>) -> Footer {
        Footer {
            back: !first,
            next: Some(("Next >".into(), problem.is_none())),
            close: Some("Cancel".into()),
            problem,
        }
    }
}

/// Which footer button was clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Stay,
    Back,
    Next,
    Close,
}

/// The frame of a job wizard: a window titled `title` with the numbered `pages` on the left
/// (the current one in bold), the page drawn by `body` on the right and its [`Footer`]
/// buttons below.
pub fn wizard_window(
    ctx: &egui::Context,
    title: &str,
    pages: &[&str],
    idx: usize,
    body: impl FnOnce(&mut egui::Ui) -> Footer,
) -> Nav {
    let mut nav = Nav::Stay;
    let screen = ctx.screen_rect();
    let center = screen.center();
    // Keep the window inside the viewport: the body is height-bounded (a vertical separator
    // in a horizontal layout would otherwise stretch to the bottom of the screen).
    let body_h = crate::dialogs::wizard_body_height(ctx, 560.0);
    let scroll_h = crate::dialogs::wizard_scroll_height(body_h);
    let max_w = (screen.width() - 40.0).max(400.0);
    let max_h = (screen.height() - 40.0).max(260.0);
    egui::Window::new(title)
        .id(egui::Id::new(("job_wizard", title)))
        .collapsible(false)
        .resizable(true)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(center)
        .default_size([880.0_f32.min(max_w), 560.0_f32.min(max_h)])
        .max_size([max_w, max_h])
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            let mut footer = Footer::default();
            ui.scope(|ui| {
                ui.set_max_height(body_h);
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        for (i, t) in pages.iter().enumerate() {
                            let text = format!("{}. {t}", i + 1);
                            ui.label(match i.cmp(&idx) {
                                std::cmp::Ordering::Equal => RichText::new(text).strong(),
                                std::cmp::Ordering::Less => RichText::new(text),
                                std::cmp::Ordering::Greater => RichText::new(text).weak(),
                            });
                            ui.add_space(2.0);
                        }
                    });
                    ui.separator();
                    ui.vertical(|ui| {
                        ui.set_min_width(640.0_f32.min(max_w - 200.0));
                        ui.set_min_height(body_h.min(380.0));
                        ui.heading(pages.get(idx).copied().unwrap_or(""));
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical().max_height(scroll_h).auto_shrink([false, true]).show(ui, |ui| {
                            footer = body(ui);
                        });
                        if let Some(p) = footer.problem.as_ref().filter(|p| !p.is_empty()) {
                            ui.add_space(4.0);
                            danger(ui, p.as_str());
                        }
                    });
                });
            });
            ui.separator();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                match &footer.close {
                    Some(label) => {
                        if ui.button(label.as_str()).clicked() {
                            nav = Nav::Close;
                        }
                    }
                    None => {
                        ui.add_enabled(false, egui::Button::new("Close"));
                    }
                }
                if let Some((label, enabled)) = &footer.next {
                    if ui.add_enabled(*enabled, egui::Button::new(label.as_str())).clicked() {
                        nav = Nav::Next;
                    }
                }
                if footer.back && ui.button("< Back").clicked() {
                    nav = Nav::Back;
                }
            });
        });
    nav
}

/// A non-modal tool window; returns `false` when the user closed it.
pub fn tool_frame(ctx: &egui::Context, title: &str, size: [f32; 2], add: impl FnOnce(&mut egui::Ui)) -> bool {
    let mut open = true;
    egui::Window::new(title)
        .id(egui::Id::new(("tool_window", title)))
        .open(&mut open)
        .collapsible(true)
        .resizable(true)
        .default_size(size)
        .show(ctx, add);
    open
}

/// `"1.5 s"`, `"2:05"`: a short duration for status lines.
pub fn short_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.1} s")
    } else {
        format_eta(Some(d.as_secs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};

    #[test]
    fn task_returns_typed_results_and_cancels_on_drop() {
        let mut t: Task<Vec<u32>> = Task::spawn("typed", |p, _c| {
            p.report(&ProgressReport { phase: "x".into(), done: 1, total: 2, ..Default::default() });
            Ok(vec![1, 2, 3])
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let r = loop {
            if let Some(r) = t.poll() {
                break r;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(r.unwrap(), [1, 2, 3]);
        let (tx, rx) = std::sync::mpsc::channel();
        let t2: Task<()> = Task::spawn("endless", move |_p, c| loop {
            if c.is_cancelled() {
                let _ = tx.send(());
                return Err(Error::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(1));
        });
        drop(t2);
        rx.recv_timeout(Duration::from_secs(10)).expect("dropping the task cancels it");
        assert_eq!(error_text(&Error::Cancelled), "Cancelled.");
    }

    #[test]
    fn devices_and_write_guard() {
        let mut m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        let all = Device::all(&m);
        assert_eq!(all[0], Device::Disk(1));
        assert!(all.contains(&Device::Partition(0, 5)));
        assert!(!all.contains(&Device::Partition(0, 2)), "extended container excluded");
        assert_eq!(Device::Partition(1, 3).range(&m), Some((117 * pm_core::MIB, 684 * pm_core::MIB)));
        assert_eq!(Device::Disk(1).letters(&m), ['E']);
        assert_eq!(Device::from_selection(Selected::Free(1, 0, 1)), Some(Device::Disk(1)));
        // unknown system disk: fail closed
        assert!(write_problem(&m, &[], 0).unwrap().contains("cannot determine"));
        m.system_disk = Some(1);
        assert!(write_problem(&m, &[], 1).unwrap().contains("system disk"));
        assert!(write_problem(&m, &[], 0).is_none());
        let pending = [Operation::DeletePartition { disk: 0, number: 6 }];
        assert!(write_problem(&m, &pending, 0).unwrap().contains("Pending operations"));
    }

}

/// Test helpers: an environment over a mock platform, and headless frames of a window.
#[cfg(test)]
pub mod test_env {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};

    pub fn model() -> Model {
        let mut m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        m.system_disk = Some(7);
        m
    }

    pub fn platform() -> Arc<dyn Platform> {
        Arc::new(pm_platform::NullPlatform)
    }

    /// Draw `w` for one frame in both themes; returns the last outcome.
    pub fn frame(w: &mut dyn ToolWindow, env: &ToolEnv) -> WindowOutcome {
        let ctx = egui::Context::default();
        let mut out = WindowOutcome::Open;
        for dark in [false, true] {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                ctx.set_visuals(if dark { egui::Visuals::dark() } else { egui::Visuals::light() });
                out = w.show(ctx, env);
            });
        }
        out
    }

    #[test]
    fn tool_windows_render() {
        let (m, p) = (model(), platform());
        let env = ToolEnv { platform: &p, live: &m, pending: &[] };
        let mut ws: Vec<Box<dyn ToolWindow>> = vec![
            Box::new(benchmark::BenchmarkWindow::new(Some("E:\\".into()))),
            Box::new(space_analyzer::SpaceAnalyzerWindow::new(None)),
            Box::new(surface_test::SurfaceTestWindow::new(Some(Device::Partition(1, 3)))),
            Box::new(smart::SmartWindow::new(Some(1))),
        ];
        for w in &mut ws {
            assert_eq!(frame(&mut **w, &env), WindowOutcome::Open);
            assert!(!w.modal() && !w.writing());
        }
    }
}
