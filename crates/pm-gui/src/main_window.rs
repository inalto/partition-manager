//! The eframe application: lays the panels out, shows the dialogs and applies the commands
//! they emit, once per frame.

use crate::app::{AppState, Cmd, JobKind, StartOptions};
use crate::args::Args;
use crate::dialogs::{self, progress, properties};
use crate::theme::{self, Theme, DANGER};
use crate::views::{disk_map, disk_table, icons, legend, sidebar, toolbar};
use crate::wizard::WizardOutcome;
use eframe::egui::{self, Color32, RichText};
use pm_platform::Platform;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct PartitionApp {
    state: AppState,
    applied_theme: Option<Theme>,
}

impl PartitionApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: AppState) -> Self {
        // rows and segments are click targets; selectable label text would swallow the clicks
        cc.egui_ctx.style_mut(|s| s.interaction.selectable_labels = false);
        PartitionApp { state, applied_theme: None }
    }
}

impl eframe::App for PartitionApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}

impl PartitionApp {
    /// One frame: poll the job, draw everything, apply the commands.
    fn draw(&mut self, ctx: &egui::Context) {
        let s = &mut self.state;
        s.poll_job();
        if self.applied_theme != Some(s.theme) {
            theme::apply(ctx, s.theme);
            self.applied_theme = Some(s.theme);
        }
        if s.busy() {
            // poll the job's progress and result without blocking the UI thread
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // Closing the window mid-Apply would kill the worker between two writes.
        let applying = matches!(&s.job, Some(j) if matches!(j.kind, JobKind::Apply { .. }));
        if applying && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            s.toast("Apply is running: wait for it to finish or press Cancel first");
        }

        let mut cmds: Vec<Cmd> = Vec::new();
        let enabled = !s.modal();
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(3.0);
            ui.add_enabled_ui(enabled, |ui| toolbar::show(ui, s, &mut cmds));
            ui.add_space(1.0);
        });
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| legend::show(ui, s));
        egui::SidePanel::left("sidebar").resizable(true).default_width(230.0).show(ctx, |ui| {
            ui.add_enabled_ui(enabled, |ui| sidebar::show(ui, s, &mut cmds));
        });
        egui::TopBottomPanel::bottom("disk_map")
            .resizable(true)
            .default_height(280.0)
            .min_height(80.0)
            .show(ctx, |ui| {
                ui.add_enabled_ui(enabled, |ui| disk_map::show(ui, s, &mut cmds));
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(enabled, |ui| {
                // inside the enabled scope: Retry must not start a refresh under a modal
                if let Some(b) = &s.banner {
                    banner(ui, b, &mut cmds);
                }
                disk_table::show(ui, s, &mut cmds)
            });
        });

        if s.modal() {
            let dim = ctx.layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("modal_dim")));
            dim.rect_filled(ctx.screen_rect(), 0.0, Color32::from_black_alpha(50));
        }
        if let Some(i) = s.properties {
            match s.preview().disk(i) {
                Some(d) => properties::show(ctx, d, &mut cmds),
                None => cmds.push(Cmd::CloseProperties),
            }
        }
        if let Some(t) = s.tool {
            dialogs::tool_window(ctx, t, &mut cmds);
        }
        if let Some(job) = &s.job {
            if matches!(job.kind, JobKind::Apply { .. }) {
                progress::show(ctx, job, &mut cmds);
            }
        }
        if let Some(mut w) = s.wizard.take() {
            let wctx = s.wizard_ctx(&w);
            match w.wizard.show(ctx, &wctx) {
                WizardOutcome::Open => {}
                WizardOutcome::Cancelled => cmds.push(Cmd::CloseWizard),
                WizardOutcome::Finished(ops) => cmds.push(Cmd::FinishWizard(ops)),
            }
            s.wizard = Some(w);
        }
        if let Some(mut d) = s.dialog.take() {
            if dialogs::show(ctx, s, &mut d, &mut cmds) {
                s.dialog = Some(d);
            }
        }
        if let Some(t) = &s.toast {
            let now = Instant::now();
            if now >= t.until {
                s.toast = None;
            } else {
                toast(ctx, &t.text);
                ctx.request_repaint_after(t.until - now);
            }
        }

        for c in cmds {
            s.handle(c);
        }
    }
}

fn banner(ui: &mut egui::Ui, text: &str, out: &mut Vec<Cmd>) {
    let fill = if ui.visuals().dark_mode {
        Color32::from_rgb(0x4A, 0x22, 0x22)
    } else {
        Color32::from_rgb(0xFB, 0xE3, 0xE3)
    };
    egui::Frame::none().fill(fill).rounding(4.0).inner_margin(6.0).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{} {text}", icons::WARNING)).color(DANGER));
            if ui.small_button("Retry").clicked() {
                out.push(Cmd::Refresh);
            }
            if ui.small_button("Dismiss").clicked() {
                out.push(Cmd::DismissBanner);
            }
        });
    });
    ui.add_space(4.0);
}

fn toast(ctx: &egui::Context, text: &str) {
    egui::Area::new(egui::Id::new("toast"))
        .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -48.0])
        .order(egui::Order::Tooltip)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(text);
            });
        });
}

/// The platform for `args`: image files given with `--disk-file`, else the real disks.
/// Returns the platform and the status-bar description of the disk source.
pub fn platform_for(args: &Args) -> Result<(Arc<dyn Platform>, Option<String>), String> {
    if args.disk_files.is_empty() {
        return Ok((pm_platform::default_platform(), None));
    }
    let names: Vec<String> = args
        .disk_files
        .iter()
        .map(|p| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()))
        .collect();
    let p = pm_platform::FilePlatform::new(args.disk_files.clone()).map_err(|e| e.to_string())?;
    Ok((Arc::new(p), Some(format!("Disk files: {}", names.join(", ")))))
}

/// Open the main window and run until it is closed.
pub fn run(args: Args) -> Result<(), String> {
    let (platform, source) = platform_for(&args)?;
    let opts = StartOptions { dry_run: args.dry_run, source };
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Partition Manager")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Partition Manager",
        native,
        Box::new(move |cc| Ok(Box::new(PartitionApp::new(cc, AppState::new(platform, opts))))),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::gpt_disk;
    use pm_core::MemDevice;
    use pm_platform::MockPlatform;

    #[test]
    fn bundled_fonts_have_the_icons() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        for icon in icons::ALL {
            let ok = ctx.fonts(|f| f.has_glyphs(&egui::FontId::proportional(14.0), icon));
            assert!(ok, "missing glyph {icon:?}");
        }
    }

    /// Drive whole frames headlessly: every panel, the confirmation and the progress dialog
    /// render without panicking, and clicks are turned into commands.
    #[test]
    fn frames_render_headless() {
        let mock = MockPlatform::new();
        let d = gpt_disk(1);
        let mut dev = MemDevice::new(d.size);
        pm_parttable::write_table(&mut dev, &pm_ops::table_from_disk(&d)).unwrap();
        mock.add_disk(1, dev, d);
        mock.set_system_disk(7);
        let state = AppState::new(Arc::new(mock), StartOptions { dry_run: true, source: Some("test".into()) });
        let mut app = PartitionApp { state, applied_theme: None };
        let ctx = egui::Context::default();
        let frame = |app: &mut PartitionApp| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| app.draw(ctx));
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !app.state.loaded {
            assert!(Instant::now() < deadline);
            frame(&mut app);
            std::thread::sleep(Duration::from_millis(5));
        }
        // a wizard draws over the main window
        app.state.handle(Cmd::Action(crate::actions::Action::ResizeMove, crate::selection::Selected::Partition(1, 3)));
        frame(&mut app);
        assert!(app.state.wizard.is_some());
        app.state.handle(Cmd::CloseWizard);
        app.state.handle(Cmd::Action(crate::actions::Action::DeletePartition, crate::selection::Selected::Partition(1, 3)));
        app.state.handle(Cmd::Apply);
        app.state.handle(Cmd::SetTheme(Theme::Dark));
        frame(&mut app);
        frame(&mut app);
        assert!(app.state.dialog.is_some());
    }
}
