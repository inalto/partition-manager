//! The eframe application: lays the panels out, shows the dialogs and applies the commands
//! they emit, once per frame.

use crate::app::{AppState, Cmd, JobKind, StartOptions};
use crate::args::Args;
use crate::dialogs::{self, progress, properties, update as update_dialogs};
use crate::settings::settings_dir;
use crate::theme::{self, Theme, DANGER};
use crate::update::UpdateState;
use crate::views::{disk_map, disk_table, icons, legend, sidebar, toolbar};
use crate::tools::WindowOutcome;
use crate::wizard::WizardOutcome;
use eframe::egui::{self, Color32, RichText};
use pm_platform::Platform;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct PartitionApp {
    state: AppState,
    applied_theme: Option<Theme>,
    /// What an earlier update left in the install folder has been removed (once, after the
    /// first frame: until this version has shown that it starts, the previous one is kept).
    cleaned_up: bool,
}

impl PartitionApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: AppState) -> Self {
        // rows and segments are click targets; selectable label text would swallow the clicks
        cc.egui_ctx.style_mut(|s| s.interaction.selectable_labels = false);
        PartitionApp::with_state(state)
    }

    fn with_state(state: AppState) -> Self {
        PartitionApp { state, applied_theme: None, cleaned_up: false }
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
        if s.busy() || s.update.busy() {
            // poll the job's (or the updater's) progress and result without blocking the UI thread
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // Closing the window mid-Apply would kill the worker between two writes.
        let applying = matches!(&s.job, Some(j) if matches!(j.kind, JobKind::Apply { .. }));
        if applying && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            s.toast("Apply is running: wait for it to finish or press Cancel first");
        }
        // the same for a tool that is writing (rescue, partition recovery, file restore)
        if s.windows.iter().any(|w| w.writing()) && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            s.toast("A recovery job is writing: wait for it to finish or cancel it first");
        }
        // and for the install, which swaps the executables
        if s.update.downloading.is_some() && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            s.toast("The update is being installed: wait for it to finish or cancel it first");
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
                // inside the enabled scope: Retry must not start a refresh under a modal (nor
                // the notice's buttons open a dialog)
                update_dialogs::notice(ui, &s.update, &mut cmds);
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
        let mut windows = std::mem::take(&mut s.windows);
        {
            let env = s.tool_env();
            windows.retain_mut(|w| match w.show(ctx, &env) {
                WindowOutcome::Open => true,
                WindowOutcome::Close => false,
                WindowOutcome::Refresh => {
                    cmds.push(Cmd::Refresh);
                    true
                }
            });
        }
        windows.append(&mut s.windows);
        s.windows = windows;
        if let Some(job) = &s.job {
            if matches!(job.kind, JobKind::Apply { .. }) {
                progress::show(ctx, job, &mut cmds);
            }
        }
        if let Some(task) = &s.update.downloading {
            update_dialogs::download(ctx, task, s.update.release().map(|r| r.version), &mut cmds);
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
        if s.quit {
            // the updated version has been started
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if !self.cleaned_up {
            self.cleaned_up = true;
            if let Some(dir) = self.state.update.install_dir() {
                pm_update::clean_up(dir);
            }
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

/// Whether to run the automatic update check at startup: not with `--no-update-check`, and
/// in a debug build only when `PM_UPDATE_CURRENT` sets the version to pretend to run.
pub fn auto_check_wanted(no_update_check: bool, debug_build: bool, current_overridden: bool) -> bool {
    !no_update_check && (!debug_build || current_overridden)
}

/// The update state of this run: the settings file in the configuration folder, the folder
/// of the executable (cleaned of what an earlier update left there after the first frame,
/// see [`PartitionApp`]), and the automatic check when it is wanted and due.
fn start_update(state: &mut AppState, args: &Args) {
    let install_dir = pm_update::install_dir().ok();
    let settings = settings_dir().map(|d| d.join("settings.json"));
    let fetch = Arc::new(pm_update::HttpFetch::new());
    state.update = UpdateState::new(fetch, settings, install_dir, pm_update::Version::current());
    let overridden = std::env::var_os("PM_UPDATE_CURRENT").is_some();
    if auto_check_wanted(args.no_update_check, cfg!(debug_assertions), overridden) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        state.start_auto_check(now);
    }
}

/// Open the main window and run until it is closed.
pub fn run(args: Args) -> Result<(), String> {
    let (platform, source) = platform_for(&args)?;
    let opts = StartOptions { dry_run: args.dry_run, source };
    let mut state = AppState::new(platform, opts);
    start_update(&mut state, &args);
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
        Box::new(move |cc| Ok(Box::new(PartitionApp::new(cc, state)))),
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
        let mut app = PartitionApp::with_state(state);
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
        // tool windows and job wizards draw over it too
        for t in [crate::app::Tool::SpaceAnalyzer, crate::app::Tool::Smart, crate::app::Tool::DataRecovery] {
            app.state.handle(Cmd::OpenTool(t));
        }
        frame(&mut app);
        assert_eq!(app.state.windows.len(), 3);
        app.state.windows.clear();
        app.state.handle(Cmd::Push(vec![pm_ops::Operation::DeletePartition { disk: 1, number: 3 }]));
        app.state.handle(Cmd::Apply);
        app.state.handle(Cmd::SetTheme(Theme::Dark));
        frame(&mut app);
        frame(&mut app);
        assert!(app.state.dialog.is_some());
    }

    #[test]
    fn automatic_check_only_where_wanted() {
        // release builds check unless told not to
        assert!(auto_check_wanted(false, false, false));
        assert!(!auto_check_wanted(true, false, false));
        // debug builds (and tests) only with PM_UPDATE_CURRENT
        assert!(!auto_check_wanted(false, true, false));
        assert!(auto_check_wanted(false, true, true));
        assert!(!auto_check_wanted(true, true, true));
    }

    /// A worker standing in for the install: reports `total` bytes and runs until cancelled.
    fn fake_install(total: u64) -> crate::tools::Task<pm_update::Version> {
        let task = crate::tools::Task::spawn("Updating Partition Manager", move |p, c| {
            p.report(&pm_core::ProgressReport {
                phase: "Downloading".into(),
                done: total / 2,
                total,
                ..Default::default()
            });
            while c.check().is_ok() {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(pm_core::Error::Cancelled)
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while task.progress().phase.is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        task
    }

    /// The update notice and every update window draw headlessly; the close of the window is
    /// refused while the install runs, and `quit` closes it.
    #[test]
    fn update_frames_render_headless() {
        use crate::update::UpdateState;
        use pm_update::testing::{release_fixture, release_json};
        let dir = tempfile::tempdir().unwrap();
        let (fetch, r) = release_fixture("0.9.0", true);
        let fetch = fetch.with(pm_update::LATEST_URL, release_json(&r));
        let u = UpdateState::new(
            Arc::new(fetch),
            Some(dir.path().join("settings.json")),
            None,
            pm_update::Version::parse("0.2.2").unwrap(),
        );
        let mut state = crate::test_util::mock_state().with_update(u);
        state.update.start_check(false);
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.update.busy() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
            state.poll_job();
        }
        assert!(state.update.notice.is_some());
        let mut app = PartitionApp::with_state(state);
        let ctx = egui::Context::default();
        let frame = |app: &mut PartitionApp, close: bool| {
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
                ..Default::default()
            };
            if close {
                input.viewports.entry(egui::ViewportId::ROOT).or_default().events.push(egui::ViewportEvent::Close);
            }
            ctx.run(input, |ctx| app.draw(ctx))
        };
        let window = |title: &str| ctx.memory(|m| m.area_rect(egui::Id::new(title))).is_some();
        // the notice, also when the release cannot be installed here (no install folder)
        assert!(!app.state.update.can_install);
        frame(&mut app, false);
        app.state.update.can_install = true;
        frame(&mut app, false);
        app.state.update.hidden = true;
        frame(&mut app, false);
        app.state.update.hidden = false;

        app.state.handle(Cmd::UpdateWhatsNew);
        frame(&mut app, false);
        frame(&mut app, false);
        assert!(window("What's new in 0.9.0"));
        app.state.handle(Cmd::CloseDialog);
        // (Update itself would report that there is no install folder)
        app.state.dialog = Some(crate::app::Dialog::UpdateConfirm { release: r });
        frame(&mut app, false);
        frame(&mut app, false);
        assert!(window("Update to 0.9.0"));
        app.state.dialog = Some(crate::app::Dialog::Updated { version: pm_update::Version::parse("0.9.0").unwrap() });
        frame(&mut app, false);
        frame(&mut app, false);
        assert!(window("Update installed"));
        app.state.dialog = None;

        // the download, before and after its size is known
        for total in [0, 4 << 20] {
            app.state.update.downloading = Some(fake_install(total));
            frame(&mut app, false);
            frame(&mut app, false);
            assert!(window("Updating to 0.9.0"));
            app.state.toast = None;
            let out = frame(&mut app, true);
            let cmds = &out.viewport_output[&egui::ViewportId::ROOT].commands;
            assert!(cmds.contains(&egui::ViewportCommand::CancelClose), "{cmds:?}");
            let text = app.state.toast.as_ref().map(|t| t.text.clone()).unwrap_or_default();
            assert_eq!(text, "The update is being installed: wait for it to finish or cancel it first");
            app.state.update.downloading = None;
        }

        // quit (after starting the new version) closes the window
        let out = frame(&mut app, false);
        assert!(!out.viewport_output[&egui::ViewportId::ROOT].commands.contains(&egui::ViewportCommand::Close));
        app.state.quit = true;
        let out = frame(&mut app, false);
        assert!(out.viewport_output[&egui::ViewportId::ROOT].commands.contains(&egui::ViewportCommand::Close));
    }

    /// What an earlier update left in the install folder is removed only once this version has
    /// drawn its first frame, and never without a known install folder.
    #[test]
    fn old_files_are_removed_after_the_first_frame() {
        let install = tempfile::tempdir().unwrap();
        let gui = pm_update::exe_name("partition-manager", cfg!(windows));
        std::fs::write(install.path().join(&gui), "new").unwrap();
        let old = install.path().join(format!("{gui}.old"));
        std::fs::write(&old, "old").unwrap();
        let ctx = egui::Context::default();
        let frame = |app: &mut PartitionApp| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| app.draw(ctx));
        };

        let u = UpdateState::new(
            Arc::new(pm_update::testing::MapFetch::new()),
            None,
            Some(install.path().to_path_buf()),
            pm_update::Version::parse("0.2.2").unwrap(),
        );
        let mut app = PartitionApp::with_state(crate::test_util::mock_state().with_update(u));
        assert!(old.exists(), "nothing is removed before the first frame");
        frame(&mut app);
        assert!(!old.exists(), "removed after the first frame");
        assert!(install.path().join(&gui).exists());

        // the install folder is unknown: the file stays
        std::fs::write(&old, "old").unwrap();
        let mut app = PartitionApp::with_state(crate::test_util::mock_state());
        frame(&mut app);
        frame(&mut app);
        assert!(old.exists());
    }

    /// Clicking Apply in the confirmation starts the job (the click is a real pointer click
    /// on the button, through a whole frame, not a hand-made command).
    #[test]
    fn confirmation_apply_button_starts_the_job() {
        let mock = MockPlatform::new();
        let d = gpt_disk(1);
        let mut dev = MemDevice::new(d.size);
        pm_parttable::write_table(&mut dev, &pm_ops::table_from_disk(&d)).unwrap();
        mock.add_disk(1, dev, d);
        let state = AppState::new(Arc::new(mock), StartOptions { dry_run: true, source: Some("test".into()) });
        let mut app = PartitionApp::with_state(state);
        let ctx = egui::Context::default();
        let frame = |app: &mut PartitionApp, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 820.0))),
                events,
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| app.draw(ctx));
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !app.state.loaded {
            assert!(Instant::now() < deadline);
            frame(&mut app, Vec::new());
            std::thread::sleep(Duration::from_millis(5));
        }
        app.state.handle(Cmd::Push(vec![pm_ops::Operation::DeletePartition { disk: 1, number: 3 }]));
        // the buttons sit in the dialog's last row, Cancel then Apply: click along that row
        // (a click on Cancel closes the dialog, which is then opened again)
        let mut started = false;
        for step in 0..60 {
            if app.state.dialog.is_none() {
                app.state.handle(Cmd::Apply);
            }
            frame(&mut app, Vec::new());
            frame(&mut app, Vec::new());
            let rect = ctx
                .memory(|m| m.area_rect(egui::Id::new("Apply (dry run)")))
                .expect("confirmation window");
            let pos = egui::pos2(rect.left() + 10.0 + 5.0 * step as f32, rect.bottom() - 18.0);
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(&mut app, vec![egui::Event::PointerMoved(pos)]);
            frame(&mut app, vec![button(true)]);
            frame(&mut app, vec![button(false)]);
            if app.state.job.is_some() {
                started = true;
                break;
            }
        }
        assert!(started, "clicking Apply in the confirmation must start the job");
        assert!(app.state.dialog.is_none(), "the confirmation closes when the job starts");
    }
}
