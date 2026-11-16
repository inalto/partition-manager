//! Application state and the commands that change it.
//!
//! Views and dialogs read [`AppState`] and emit [`Cmd`]s; [`AppState::handle`] applies them
//! after the frame is drawn. The pending-operation queue is only ever mutated here, on the UI
//! thread; everything that can take time (enumeration, Apply) runs as a [`crate::jobs`] job.

use crate::actions::{Action, Effect};
use crate::jobs::{spawn_job, JobHandle, JobOutput};
use crate::selection::Selected;
use crate::theme::Theme;
use crate::tools::{Device, ToolEnv, ToolWindow, WindowKind};
use crate::update::UpdateState;
use crate::wizard::{Wizard, WizardCtx};
use pm_ops::{execute_queued, ExecOpts, ExecReport, Model, OpQueue, Operation};
use pm_platform::Platform;
use pm_update::{Release, Version};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a toast stays on screen.
const TOAST_TIME: Duration = Duration::from_secs(3);

/// The tabs of the main window: the disks, and one per tool or family of tools. The content
/// of a tab fills the main area; the side panel shows what belongs to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Disks,
    DataRecovery,
    PartitionRecovery,
    DiskBenchmark,
    SpaceAnalyzer,
    /// Create Image, Restore Image, Rescue Copy.
    Image,
    /// Surface Test, SMART.
    DiskTools,
}

impl Tab {
    /// In the order they are shown.
    pub const ALL: [Tab; 7] = [
        Tab::Disks,
        Tab::DataRecovery,
        Tab::PartitionRecovery,
        Tab::DiskBenchmark,
        Tab::SpaceAnalyzer,
        Tab::Image,
        Tab::DiskTools,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Disks => "Disks",
            Tab::Image => "Backup / Image",
            Tab::DiskTools => "Disk tools",
            other => other.tools()[0].label(),
        }
    }

    /// The tools of the tab (none for the disks). A tab with several lists them in the side
    /// panel.
    pub fn tools(self) -> &'static [Tool] {
        match self {
            Tab::Disks => &[],
            Tab::DataRecovery => &[Tool::DataRecovery],
            Tab::PartitionRecovery => &[Tool::PartitionRecovery],
            Tab::DiskBenchmark => &[Tool::DiskBenchmark],
            Tab::SpaceAnalyzer => &[Tool::SpaceAnalyzer],
            Tab::Image => &Tool::IMAGE_MENU,
            Tab::DiskTools => &Tool::MORE,
        }
    }

    /// The tab `t` lives in.
    pub fn of(t: Tool) -> Tab {
        Tab::ALL.into_iter().find(|tab| tab.tools().contains(&t)).unwrap_or(Tab::Disks)
    }
}

/// The tools of the tabs: tool windows and wizards that do not need a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    DataRecovery,
    PartitionRecovery,
    DiskBenchmark,
    SpaceAnalyzer,
    CreateImage,
    RestoreImage,
    RescueCopy,
    SurfaceTest,
    Smart,
}

impl Tool {
    /// The tools of the "Backup / Image" tab.
    pub const IMAGE_MENU: [Tool; 3] = [Tool::CreateImage, Tool::RestoreImage, Tool::RescueCopy];
    /// The tools of the "Disk tools" tab.
    pub const MORE: [Tool; 2] = [Tool::SurfaceTest, Tool::Smart];

    pub fn label(self) -> &'static str {
        match self {
            Tool::DataRecovery => "Data Recovery",
            Tool::PartitionRecovery => "Partition Recovery",
            Tool::DiskBenchmark => "Disk Benchmark",
            Tool::SpaceAnalyzer => "Space Analyzer",
            Tool::CreateImage => "Create Image",
            Tool::RestoreImage => "Restore Image",
            Tool::RescueCopy => "Rescue Copy (ddrescue)",
            Tool::SurfaceTest => "Surface Test",
            Tool::Smart => "SMART",
        }
    }

    /// One or two sentences for the side panel: what the tool does and what it touches.
    pub fn about(self) -> &'static str {
        match self {
            Tool::DataRecovery => "Finds files on a damaged, formatted or deleted file system and copies them to another disk. The source is only read.",
            Tool::PartitionRecovery => "Searches a disk for lost partitions and writes the ones you choose back to its partition table.",
            Tool::DiskBenchmark => "Measures read and write speed on a volume, with a test file it creates and removes.",
            Tool::SpaceAnalyzer => "Shows which folders and files take the space on a volume. Reads only.",
            Tool::CreateImage => "Copies a disk or a partition into an image file. It becomes a pending operation and runs on Apply.",
            Tool::RestoreImage => "Writes an image file back to a disk or a partition. It becomes a pending operation and runs on Apply.",
            Tool::RescueCopy => "Copies a failing disk, skipping unreadable areas and coming back to them, like ddrescue. Starts at once, not on Apply.",
            Tool::SurfaceTest => "Reads every sector of a disk or partition and maps the slow and the unreadable ones. Reads only.",
            Tool::Smart => "The health attributes a drive reports about itself.",
        }
    }

    /// The window of the tool; `None` for the two that are operation wizards (Create Image,
    /// Restore Image).
    pub fn kind(self) -> Option<WindowKind> {
        Some(match self {
            Tool::CreateImage | Tool::RestoreImage => return None,
            Tool::DataRecovery => WindowKind::DataRecovery,
            Tool::PartitionRecovery => WindowKind::PartitionRecovery,
            Tool::DiskBenchmark => WindowKind::Benchmark,
            Tool::SpaceAnalyzer => WindowKind::SpaceAnalyzer,
            Tool::RescueCopy => WindowKind::RescueCopy,
            Tool::SurfaceTest => WindowKind::SurfaceTest,
            Tool::Smart => WindowKind::Smart,
        })
    }
}

/// Modal dialogs (at most one at a time).
#[derive(Clone, Debug)]
pub enum Dialog {
    Error(String),
    /// Apply confirmation; `allow_system` relaxes the executor's system-disk guard for the
    /// non-destructive family (resize, letter, label, mount).
    /// The destructive / system-disk flags are recomputed from the live queue every frame.
    Confirm { allow_system: bool },
    /// Result of an Apply run.
    Report { report: ExecReport, dry_run: bool, note: Option<String> },
    /// The notes of a newer release.
    WhatsNew { release: Release },
    /// Ask before downloading and installing `release`.
    UpdateConfirm { release: Release },
    /// `version` was installed; offers the restart.
    Updated { version: Version },
    /// The About window.
    About,
}

/// Everything a view or dialog can ask for.
#[derive(Clone, Debug)]
pub enum Cmd {
    Select(Selected),
    /// Run `Action` on this selection (which also becomes the current selection).
    Action(Action, Selected),
    /// Queue operations, all or none.
    Push(Vec<Operation>),
    /// Open the Apply confirmation.
    Apply,
    /// Start Apply (from the confirmation).
    ConfirmApply { allow_system: bool },
    Undo,
    Discard,
    Refresh,
    CancelJob,
    CloseDialog,
    /// Show a tool in its tab.
    OpenTool(Tool),
    /// Show a tab.
    OpenTab(Tab),
    /// Open the About window.
    About,
    CloseProperties,
    SetTheme(Theme),
    SetDryRun(bool),
    /// Show the folder of the application log in the file manager.
    OpenLogFolder,
    DismissBanner,
    /// Queue the operations of the open wizard; it closes on success and shows the error on
    /// its summary page otherwise.
    FinishWizard(Vec<Operation>),
    CloseWizard,
    /// Check for a newer release now and report the outcome.
    CheckUpdates,
    /// Turn the automatic update check on or off (saved).
    SetCheckUpdates(bool),
    /// Show the notes of the newer release (checking first when only its version is known).
    UpdateWhatsNew,
    /// Open the update confirmation (checking first when only the version is known).
    UpdateAsk,
    /// Download and install the release of the confirmation.
    UpdateStart,
    UpdateCancel,
    /// Do not offer the version of the notice again (saved).
    UpdateSkip,
    /// Hide the notice for this session.
    UpdateHide,
    /// Start the installed new version and quit.
    UpdateRestart,
    /// Open a GitHub page in the browser.
    OpenUrl(String),
}

/// A wizard on screen and what it was opened on.
pub struct OpenWizard {
    pub wizard: Wizard,
    pub selection: Selected,
    /// Free drive letters of the platform, read once when the wizard opened.
    pub free_letters: Vec<char>,
    /// The tool it is, when it was opened from a tab: it then fills that tab instead of
    /// being a dialog. `None` for the wizard of an action on the selection.
    pub tool: Option<Tool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Refresh,
    Apply { dry_run: bool },
}

pub struct RunningJob {
    pub kind: JobKind,
    pub handle: JobHandle,
}

pub struct Toast {
    pub text: String,
    pub until: Instant,
}

/// Start-up options from the command line.
#[derive(Clone, Debug, Default)]
pub struct StartOptions {
    pub dry_run: bool,
    /// Shown in the status bar, e.g. "Disk files: a.img".
    pub source: Option<String>,
}

pub struct AppState {
    pub platform: Arc<dyn Platform>,
    pub queue: OpQueue,
    pub selected: Selected,
    pub theme: Theme,
    pub dry_run: bool,
    pub elevated: bool,
    pub source: Option<String>,
    /// The first enumeration has finished (successfully or not).
    pub loaded: bool,
    /// Non-blocking error banner (e.g. enumeration failed because the app is not elevated).
    pub banner: Option<String>,
    pub job: Option<RunningJob>,
    /// The last action that needs a wizard (picked up by the wizard step).
    pub pending_action: Option<(Action, Selected)>,
    pub toast: Option<Toast>,
    pub dialog: Option<Dialog>,
    /// Disk whose properties window is open.
    pub properties: Option<u32>,
    /// The tab on screen.
    pub tab: Tab,
    /// The tool last used in the "Backup / Image" tab, and in the "Disk tools" tab.
    pub image_tool: Tool,
    pub disk_tool: Tool,
    /// The tools that have been opened, one of each kind. Only the one of the active tab is
    /// drawn; the others keep their state (and their running jobs) until their tab is back.
    pub windows: Vec<Box<dyn ToolWindow>>,
    /// The open wizard: a dialog for an action on the selection, or the content of the
    /// "Backup / Image" tab (see [`OpenWizard::tool`]).
    pub wizard: Option<OpenWizard>,
    /// The next successful refresh drops the pending operations instead of rebasing them
    /// (set after a failed Apply, when some of them may already have been written).
    pub reset_on_refresh: bool,
    /// The update check, notice and install.
    pub update: UpdateState,
    /// The application should close (set after starting the updated version).
    pub quit: bool,
}

impl AppState {
    /// New state over `platform`; starts the first enumeration.
    pub fn new(platform: Arc<dyn Platform>, opts: StartOptions) -> Self {
        let elevated = platform.is_elevated();
        let mut s = AppState {
            platform,
            queue: OpQueue::new(Model::default()),
            selected: Selected::None,
            theme: Theme::Light,
            dry_run: opts.dry_run,
            elevated,
            source: opts.source,
            loaded: false,
            banner: None,
            job: None,
            pending_action: None,
            toast: None,
            dialog: None,
            properties: None,
            tab: Tab::Disks,
            image_tool: Tool::CreateImage,
            disk_tool: Tool::SurfaceTest,
            windows: Vec::new(),
            wizard: None,
            reset_on_refresh: false,
            update: UpdateState::disabled(),
            quit: false,
        };
        s.start_refresh();
        s
    }

    /// The model shown everywhere: the disks as they will be after the pending operations.
    pub fn preview(&self) -> &Model {
        self.queue.preview()
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// A dialog, the wizard of an action or Apply is in progress: nothing else takes input.
    /// A tool in its tab is not a dialog, whatever it is doing.
    pub fn modal(&self) -> bool {
        self.dialog.is_some()
            || self.wizard.as_ref().is_some_and(|w| w.tool.is_none())
            || matches!(self.job, Some(RunningJob { kind: JobKind::Apply { .. }, .. }))
            || self.update.downloading.is_some()
    }

    /// A tool is writing to a disk (a rescue, a partition recovery, a file restore): the
    /// disks tab is read-only until it finishes, so that nothing is queued or applied under it.
    pub fn disks_locked(&self) -> bool {
        self.windows.iter().any(|w| w.writing())
    }

    /// The tool of the tab on screen (`None` on the disks).
    pub fn active_tool(&self) -> Option<Tool> {
        match self.tab {
            Tab::Disks => None,
            Tab::Image => Some(self.image_tool),
            Tab::DiskTools => Some(self.disk_tool),
            other => other.tools().first().copied(),
        }
    }

    /// Whether tool `t` has been opened and not closed since.
    pub fn tool_open(&self, t: Tool) -> bool {
        match t.kind() {
            Some(kind) => self.windows.iter().any(|w| w.kind() == kind),
            None => self.wizard.as_ref().is_some_and(|w| w.tool == Some(t)),
        }
    }

    /// Whether a job of tool `t` is running (shown on its tab).
    pub fn tool_running(&self, t: Tool) -> bool {
        t.kind().is_some_and(|kind| self.windows.iter().any(|w| w.kind() == kind && w.running()))
    }

    /// Work is running that an update or a restart must not interrupt: a job (refresh,
    /// Apply), a worker of a window, or the install itself.
    pub fn anything_running(&self) -> bool {
        self.job.is_some() || self.windows.iter().any(|w| w.running()) || self.update.downloading.is_some()
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), until: Instant::now() + TOAST_TIME });
    }

    fn error(&mut self, text: impl Into<String>) {
        self.dialog = Some(Dialog::Error(text.into()));
    }

    pub fn start_refresh(&mut self) {
        if self.busy() {
            return;
        }
        let p = self.platform.clone();
        let handle = spawn_job("Reading disks", move |progress, _cancel| {
            progress.report(&pm_core::ProgressReport { phase: "Reading disks".into(), ..Default::default() });
            Model::from_platform(&*p).map(JobOutput::Model)
        });
        self.job = Some(RunningJob { kind: JobKind::Refresh, handle });
    }

    fn start_apply(&mut self, allow_system: bool) {
        if self.busy() || self.queue.is_empty() {
            return;
        }
        let p = self.platform.clone();
        let ops = self.queue.ops().to_vec();
        let base = self.queue.base().clone();
        let dry_run = self.dry_run;
        let opts = ExecOpts { dry_run, allow_system_disk: allow_system };
        let handle = spawn_job("Applying changes", move |progress, cancel| {
            // checked against the disks the operations were queued on
            let report = execute_queued(&*p, &base, &ops, &opts, progress, cancel)?;
            let refreshed = if dry_run {
                None
            } else {
                progress.report(&pm_core::ProgressReport { phase: "Reading disks".into(), ..Default::default() });
                Some(Model::from_platform(&*p).map_err(|e| e.to_string()))
            };
            Ok(JobOutput::Applied { report, refreshed })
        });
        self.job = Some(RunningJob { kind: JobKind::Apply { dry_run }, handle });
    }

    /// Collect the result of a finished job, and of the update workers, if any. Call once per
    /// frame.
    pub fn poll_job(&mut self) {
        self.poll_update();
        let Some(job) = self.job.as_mut() else { return };
        let Some(result) = job.handle.poll() else { return };
        let kind = job.kind;
        self.job = None;
        match (kind, result) {
            (JobKind::Refresh, Ok(JobOutput::Model(m))) => {
                self.loaded = true;
                self.banner = None;
                let dropped = if std::mem::take(&mut self.reset_on_refresh) {
                    let n = self.queue.len();
                    self.queue.reset(m);
                    n
                } else {
                    rebase(&mut self.queue, m)
                };
                if dropped > 0 {
                    self.toast(format!("{dropped} pending operation(s) no longer apply and were removed"));
                }
                self.selected = self.selected.revalidate(self.queue.preview());
            }
            (JobKind::Refresh, Err(e)) => {
                self.loaded = true;
                let hint = if self.elevated {
                    ""
                } else {
                    crate::format_util::elevation_advice(self.platform.os_name())
                };
                self.banner = Some(format!("Could not read the disks: {e}.{hint}"));
            }
            (JobKind::Apply { dry_run }, Ok(JobOutput::Applied { report, refreshed })) => {
                let mut note = None;
                match refreshed {
                    None => {}
                    Some(Ok(m)) => self.queue.reset(m),
                    Some(Err(e)) => {
                        self.queue.reset(Model::default());
                        let msg = format!("Could not read the disks after Apply: {e}. Press Refresh.");
                        self.banner = Some(msg.clone());
                        note = Some(msg);
                    }
                }
                self.selected = self.selected.revalidate(self.queue.preview());
                self.dialog = Some(Dialog::Report { report, dry_run, note });
            }
            (JobKind::Apply { dry_run }, Err(e)) => {
                self.error(format!("Apply failed: {e}"));
                if !dry_run {
                    // Some operations may have written before the failure (or the worker
                    // panicked): re-read the disks and drop the queue rather than rebase it,
                    // so nothing already applied is queued again.
                    self.reset_on_refresh = true;
                    self.start_refresh();
                }
            }
            (_, Ok(other)) => log::warn!("unexpected job output {other:?}"),
        }
    }

    /// Queue `ops`, all or none: on the first rejection the ones already pushed are undone
    /// and the planner's message is shown.
    pub fn push_ops(&mut self, ops: Vec<Operation>) {
        if let Err(e) = self.try_push_ops(ops) {
            self.error(e);
        }
    }

    /// [`AppState::push_ops`] returning the planner's message instead of showing it.
    pub fn try_push_ops(&mut self, ops: Vec<Operation>) -> Result<(), String> {
        let mut pushed = 0;
        for op in ops {
            if let Err(e) = self.queue.push(op) {
                for _ in 0..pushed {
                    self.queue.undo();
                }
                return Err(e.to_string());
            }
            pushed += 1;
        }
        self.selected = self.selected.revalidate(self.queue.preview());
        Ok(())
    }

    /// The context the open wizard `w` is drawn with.
    pub fn wizard_ctx(&self, w: &OpenWizard) -> WizardCtx<'_> {
        WizardCtx::new(self.queue.preview(), w.free_letters.clone(), w.selection).with_platform(self.platform.clone())
    }

    /// Open the wizard of `action` on `sel`; actions without one (yet) are remembered in
    /// `pending_action` with a toast.
    fn open_wizard(&mut self, action: Action, sel: Selected) {
        // GetLogicalDrives: instant, fine on the UI thread
        let free_letters = self.platform.free_letters();
        let ctx = WizardCtx::new(self.queue.preview(), free_letters.clone(), sel).with_platform(self.platform.clone());
        let wizard = crate::wizards::for_action(action, &ctx).or_else(|| crate::wizards::clone_for_action(action, &ctx));
        match wizard {
            Some(wizard) => self.wizard = Some(OpenWizard { wizard, selection: sel, free_letters, tool: None }),
            None if action == Action::PartitionRecovery => self.open_tool_on_selection(Tool::PartitionRecovery),
            None if action == Action::SurfaceTest => self.open_tool_on_selection(Tool::SurfaceTest),
            None if crate::wizards::ACTIONS.contains(&action) || crate::wizards::CLONE_ACTIONS.contains(&action) => {
                self.toast(format!("{}: not available for this selection", action.label()))
            }
            None => {
                self.pending_action = Some((action, sel));
                self.toast(format!("{}: coming in the next step", action.label()));
            }
        }
    }

    /// Show the tab `tab`, opening its tool if it is not open yet.
    pub fn open_tab(&mut self, tab: Tab) {
        self.tab = tab;
        if let Some(t) = self.active_tool() {
            self.ensure_tool(t);
        }
    }

    /// Show tool `t` in its tab. A tool that is already open stays as it is, with what was
    /// entered in it and whatever it is running.
    pub fn open_tool(&mut self, t: Tool) {
        match Tab::of(t) {
            Tab::Image => self.image_tool = t,
            Tab::DiskTools => self.disk_tool = t,
            _ => {}
        }
        self.open_tab(Tab::of(t));
    }

    /// [`AppState::open_tool`] for an action on the selection: the tool starts over on that
    /// selection, unless a job of it is running.
    fn open_tool_on_selection(&mut self, t: Tool) {
        if let Some(kind) = t.kind() {
            self.windows.retain(|w| w.kind() != kind || w.running() || w.writing());
        }
        self.open_tool(t);
    }

    /// Create tool `t` on the current selection, if it is not open: an operation wizard
    /// (images), a job wizard or a tool window, one of each kind.
    fn ensure_tool(&mut self, t: Tool) {
        if self.tool_open(t) {
            return;
        }
        let sel = self.selected;
        let live = self.queue.base();
        let device = Device::from_selection(sel).filter(|d| d.range(live).is_some());
        let window: Box<dyn ToolWindow> = match t {
            Tool::CreateImage | Tool::RestoreImage => {
                let free_letters = Vec::new();
                let ctx = WizardCtx::new(self.queue.preview(), free_letters.clone(), sel).with_platform(self.platform.clone());
                let w = if t == Tool::CreateImage {
                    crate::wizards::image_create::new(&ctx)
                } else {
                    crate::wizards::image_restore::new(&ctx)
                };
                match w {
                    Some(wizard) => self.wizard = Some(OpenWizard { wizard, selection: sel, free_letters, tool: Some(t) }),
                    None => {
                        self.tab = Tab::Disks;
                        self.toast(format!("{}: there is no disk", t.label()));
                    }
                }
                return;
            }
            Tool::DataRecovery => Box::new(crate::wizards::data_recovery::DataRecoveryWizard::new(device)),
            Tool::PartitionRecovery => Box::new(crate::wizards::partition_recovery::PartitionRecoveryWizard::new(
                device.map(|d| d.disk()),
            )),
            Tool::RescueCopy => Box::new(crate::wizards::rescue::RescueWizard::new(device)),
            Tool::DiskBenchmark => Box::new(crate::tools::benchmark::BenchmarkWindow::new(
                sel.partition_in(live).and_then(|p| p.mount_points.first().cloned()),
            )),
            Tool::SpaceAnalyzer => Box::new(crate::tools::space_analyzer::SpaceAnalyzerWindow::new(
                sel.partition_in(live).and_then(|p| p.mount_points.first().cloned()),
            )),
            Tool::SurfaceTest => Box::new(crate::tools::surface_test::SurfaceTestWindow::new(device)),
            Tool::Smart => Box::new(crate::tools::smart::SmartWindow::new(device.map(|d| d.disk()))),
        };
        if !self.windows.iter().any(|w| w.kind() == window.kind()) {
            self.windows.push(window);
        }
    }

    /// Close the wizard; one that filled a tab gives the screen back to the disks.
    fn close_wizard(&mut self) {
        if let Some(w) = self.wizard.take() {
            if w.tool.is_some() && self.tab == Tab::Image {
                self.tab = Tab::Disks;
            }
        }
    }

    /// The environment tool windows are drawn with.
    pub fn tool_env(&self) -> ToolEnv<'_> {
        ToolEnv { platform: &self.platform, live: self.queue.base(), pending: self.queue.ops() }
    }

    fn dispatch(&mut self, action: Action, sel: Selected) {
        self.selected = sel;
        let Some(effect) = action.effect(&sel, self.queue.preview()) else {
            return;
        };
        match effect {
            Effect::Push(ops) => self.push_ops(ops),
            // the wipe / mount / unmount choices are made in their wizards
            Effect::ChooseWipeMethod { .. }
            | Effect::ChooseMountLetter { .. }
            | Effect::ChooseUnmount { .. }
            | Effect::Wizard(_) => self.open_wizard(action, sel),
            Effect::ShowProperties { disk } => self.properties = Some(disk),
            Effect::Explore { path } => explore(self, &path),
        }
    }

    pub fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Select(s) => self.selected = s,
            Cmd::Action(a, s) => {
                if !self.busy() {
                    self.dispatch(a, s)
                }
            }
            Cmd::Push(ops) => {
                self.dialog = None;
                self.push_ops(ops);
            }
            Cmd::Apply => {
                if !self.queue.is_empty() && !self.busy() {
                    self.dialog = Some(Dialog::Confirm { allow_system: false });
                }
            }
            Cmd::ConfirmApply { allow_system } => {
                // only from the confirmation the user is looking at
                if matches!(self.dialog, Some(Dialog::Confirm { .. })) {
                    self.dialog = None;
                    self.start_apply(allow_system);
                }
            }
            Cmd::Undo => {
                if !self.busy() {
                    self.queue.undo();
                    self.selected = self.selected.revalidate(self.queue.preview());
                }
            }
            Cmd::Discard => {
                if !self.busy() {
                    self.queue.discard();
                    self.selected = self.selected.revalidate(self.queue.preview());
                }
            }
            // a refresh rebases the queue: never under an open dialog (e.g. the confirmation)
            Cmd::Refresh => {
                if self.dialog.is_none() {
                    self.start_refresh();
                }
            }
            Cmd::CancelJob => {
                if let Some(j) = &self.job {
                    j.handle.cancel();
                }
            }
            Cmd::CloseDialog => self.dialog = None,
            Cmd::OpenTool(t) => self.open_tool(t),
            Cmd::OpenTab(tab) => self.open_tab(tab),
            Cmd::About => {
                if self.dialog.is_none() {
                    self.dialog = Some(Dialog::About);
                }
            }
            Cmd::CloseProperties => self.properties = None,
            Cmd::SetTheme(t) => self.theme = t,
            Cmd::SetDryRun(d) => self.dry_run = d,
            Cmd::OpenLogFolder => match crate::logging::log_dir() {
                Some(dir) => {
                    let _ = std::fs::create_dir_all(&dir);
                    explore(self, &dir.display().to_string());
                }
                None => self.toast(format!(
                    "The log folder is unknown ({} is not set)",
                    if cfg!(windows) { "LOCALAPPDATA" } else { "HOME" }
                )),
            },
            Cmd::DismissBanner => self.banner = None,
            Cmd::FinishWizard(ops) => match self.try_push_ops(ops.clone()) {
                Ok(()) => {
                    self.close_wizard();
                    self.run_finished_images(&ops);
                }
                Err(e) => match &mut self.wizard {
                    Some(w) => w.wizard.set_error(e),
                    None => self.error(e),
                },
            },
            Cmd::CloseWizard => self.close_wizard(),
            Cmd::CheckUpdates
            | Cmd::SetCheckUpdates(_)
            | Cmd::UpdateWhatsNew
            | Cmd::UpdateAsk
            | Cmd::UpdateStart
            | Cmd::UpdateCancel
            | Cmd::UpdateSkip
            | Cmd::UpdateHide
            | Cmd::UpdateRestart
            | Cmd::OpenUrl(_) => self.handle_update(cmd),
        }
    }

    /// Image operations do not change what the disk views show, so queueing them silently
    /// looks like nothing happened: when a wizard queued only image operations (`ops`), run
    /// them. Creating an image with nothing else pending starts at once (the wizard's summary
    /// was the confirmation, and the disks are only read); a restore, or other pending
    /// operations that would run first, go through the Apply confirmation.
    fn run_finished_images(&mut self, ops: &[Operation]) {
        let image = |o: &Operation| {
            matches!(o, Operation::ImageDisk { .. } | Operation::ImagePartition { .. } | Operation::RestoreImage { .. })
        };
        if ops.is_empty() || !ops.iter().all(image) {
            return;
        }
        if self.busy() {
            self.toast("Queued: press Apply once the disks have been read");
        } else if self.queue.len() == ops.len() && !ops.iter().any(Operation::is_destructive) {
            self.start_apply(false);
        } else {
            self.dialog = Some(Dialog::Confirm { allow_system: false });
        }
    }

    /// Indices of the queued operations that touch the (effective) running system disk.
    pub fn system_disk_ops(&self) -> Vec<usize> {
        system_disk_ops(self.queue.base(), self.queue.ops())
    }
}

/// Open the system file browser on `path` (detached; never waits for it).
fn explore(state: &mut AppState, path: &str) {
    if let Err(e) = pm_platform::shell::open_in_file_manager(std::path::Path::new(path)) {
        state.toast(format!("Cannot open {path}: {e}"));
    }
}

/// Indices of `ops` that write to or read from the running system disk of `base`, including
/// mount changes of its volumes.
pub fn system_disk_ops(base: &Model, ops: &[Operation]) -> Vec<usize> {
    let Some(sys) = base.effective_system_disk() else {
        return Vec::new();
    };
    ops.iter()
        .enumerate()
        .filter(|(_, op)| match op {
            Operation::Mount { volume, .. } | Operation::Unmount { volume, .. } => {
                base.volumes.iter().any(|v| v.id == *volume && v.disk == Some(sys))
                    || base.disk(sys).map_or(false, |d| {
                        d.partitions.iter().any(|p| p.volume.as_ref() == Some(volume))
                    })
            }
            _ => op.touches_disk(sys),
        })
        .map(|(i, _)| i)
        .collect()
}

/// Per operation of `ops` (applied in order from `base`): whether it is destructive in the
/// model it will actually run on (e.g. a pure grow is not, a shrink or move is).
pub fn destructive_flags(base: &Model, ops: &[Operation]) -> Vec<bool> {
    let mut m = base.clone();
    ops.iter()
        .map(|op| {
            let d = op.is_destructive_in(&m);
            if let Ok(next) = pm_ops::Planner::simulate(&m, op) {
                m = next;
            }
            d
        })
        .collect()
}

/// Move `queue` onto a freshly enumerated `base`, keeping the pending operations that still
/// validate and still reference the same disks and partitions ([`pm_ops::layout_matches`],
/// the executor's check), in order, up to the first that does not. Returns how many were
/// dropped.
pub fn rebase(queue: &mut OpQueue, base: Model) -> usize {
    let ops = queue.ops().to_vec();
    // the models the operations were queued against, one per operation
    let mut old = queue.base().clone();
    queue.reset(base);
    let mut kept = 0;
    for op in &ops {
        let new = queue.preview().clone();
        if pm_ops::layout_matches(&old, &new, op).is_err() || queue.push(op.clone()).is_err() {
            break;
        }
        let Ok(mut next) = pm_ops::Planner::simulate(&old, op) else { break };
        // identifiers the two simulations invented independently are not a change
        pm_ops::adopt_generated_ids(&old, &mut next, queue.preview(), &op.disks());
        old = next;
        kept += 1;
    }
    ops.len() - kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, mock_state, model_of, wait_idle};
    use pm_core::{VolumeId, MIB};
    use pm_ops::WipeMethod;

    #[test]
    fn refresh_loads_disks_on_start() {
        let s = mock_state();
        assert!(s.loaded);
        assert!(s.banner.is_none());
        let mut idx: Vec<u32> = s.preview().disks.iter().map(|d| d.index).collect();
        idx.sort();
        assert_eq!(idx, [0, 1]);
    }

    #[test]
    fn enumeration_error_shows_banner_and_empty_list() {
        let mut s = AppState::new(Arc::new(pm_platform::NullPlatform), StartOptions::default());
        wait_idle(&mut s);
        assert!(s.loaded);
        assert!(s.banner.as_deref().unwrap_or("").contains("Could not read the disks"));
        assert!(s.preview().disks.is_empty());
    }

    #[test]
    fn push_ops_is_all_or_nothing() {
        let mut s = mock_state();
        s.push_ops(vec![
            Operation::DeletePartition { disk: 1, number: 3 },
            Operation::DeletePartition { disk: 1, number: 99 },
        ]);
        assert!(s.queue.is_empty(), "first op rolled back");
        assert!(matches!(s.dialog, Some(Dialog::Error(_))));
        s.handle(Cmd::CloseDialog);
        s.handle(Cmd::Action(Action::DeleteAllPartitions, Selected::Disk(0)));
        assert_eq!(s.queue.len(), 4);
        assert!(s.dialog.is_none());
        // undo / discard
        s.handle(Cmd::Undo);
        assert_eq!(s.queue.len(), 3);
        s.handle(Cmd::Discard);
        assert!(s.queue.is_empty());
    }

    #[test]
    fn selection_follows_deleted_partition_to_its_disk() {
        let mut s = mock_state();
        s.handle(Cmd::Action(Action::DeletePartition, Selected::Partition(1, 3)));
        assert!(s.queue.is_empty(), "Delete opens its confirmation wizard first");
        let ops = s.wizard.as_ref().unwrap().wizard.build_ops().unwrap();
        assert_eq!(ops, vec![Operation::DeletePartition { disk: 1, number: 3 }]);
        s.handle(Cmd::FinishWizard(ops));
        assert_eq!(s.queue.len(), 1);
        assert_eq!(s.selected, Selected::Disk(1));
    }

    #[test]
    fn actions_without_a_wizard_yet_are_remembered_with_a_toast() {
        let mut s = mock_state();
        s.handle(Cmd::Action(Action::ChangeType, Selected::Partition(1, 3)));
        assert_eq!(s.pending_action, Some((Action::ChangeType, Selected::Partition(1, 3))));
        assert!(s.toast.as_ref().unwrap().text.contains("Change Type"));
        assert!(s.wizard.is_none());
        assert!(s.queue.is_empty());
    }

    #[test]
    fn wizard_actions_open_their_wizard() {
        let mut s = mock_state();
        for (a, sel) in [
            (Action::FormatPartition, Selected::Partition(1, 3)),
            (Action::WipeDisk, Selected::Disk(1)),
            (Action::Unmount, Selected::Partition(1, 3)),
            (Action::ConvertMbrToGpt, Selected::Disk(0)),
            (Action::DeletePartition, Selected::Partition(1, 3)),
        ] {
            s.handle(Cmd::Action(a, sel));
            let w = s.wizard.as_ref().unwrap_or_else(|| panic!("{a:?}: no wizard"));
            assert_eq!(w.selection, sel);
            assert!(s.modal());
            assert!(s.dialog.is_none() && s.pending_action.is_none());
            s.handle(Cmd::CloseWizard);
            assert!(!s.modal());
        }
    }

    #[test]
    fn clone_wizards_and_tool_windows_open() {
        let mut s = mock_state();
        for (a, sel) in [
            (Action::CopyDisk, Selected::Disk(1)),
            (Action::CopyPartition, Selected::Partition(1, 3)),
            (Action::MigrateOs, Selected::Disk(1)),
        ] {
            s.handle(Cmd::Action(a, sel));
            assert!(s.wizard.is_some(), "{a:?}: no wizard");
            s.handle(Cmd::CloseWizard);
        }
        s.handle(Cmd::Action(Action::PartitionRecovery, Selected::Disk(1)));
        assert_eq!(s.windows.len(), 1);
        assert!(!s.modal(), "a tool in its tab is not a dialog");
        assert_eq!(s.tab, Tab::PartitionRecovery);
        s.windows.clear();
        s.handle(Cmd::Action(Action::SurfaceTest, Selected::Partition(1, 3)));
        for t in [Tool::DiskBenchmark, Tool::SpaceAnalyzer, Tool::Smart, Tool::Smart] {
            s.handle(Cmd::OpenTool(t));
        }
        assert_eq!(s.windows.len(), 4, "one window of each kind");
        assert!(!s.modal(), "tool windows are not modal");
        for t in [Tool::DataRecovery, Tool::RescueCopy, Tool::PartitionRecovery] {
            s.handle(Cmd::OpenTool(t));
        }
        assert_eq!(s.windows.len(), 7);
        s.windows.clear();
        s.handle(Cmd::OpenTool(Tool::CreateImage));
        assert!(s.wizard.is_some());
        s.handle(Cmd::CloseWizard);
        s.handle(Cmd::OpenTool(Tool::RestoreImage));
        assert!(s.wizard.is_some());
    }

    #[test]
    fn tabs_open_their_tool_and_keep_it() {
        let mut s = mock_state();
        assert_eq!((s.tab, s.active_tool()), (Tab::Disks, None));
        // every tool has exactly one tab, and every tab but the disks has tools
        for t in Tool::IMAGE_MENU.into_iter().chain(Tool::MORE) {
            assert!(matches!(Tab::of(t), Tab::Image | Tab::DiskTools), "{t:?}");
        }
        for tab in Tab::ALL {
            assert_eq!(tab.tools().is_empty(), tab == Tab::Disks);
            assert!(tab.tools().iter().all(|t| Tab::of(*t) == tab && !t.about().is_empty()));
        }
        // a single-tool tab opens its tool
        s.handle(Cmd::OpenTab(Tab::DiskBenchmark));
        assert_eq!(s.active_tool(), Some(Tool::DiskBenchmark));
        assert!(s.tool_open(Tool::DiskBenchmark) && !s.modal());
        // a family tab opens the tool used last in it (the first one at the start)
        s.handle(Cmd::OpenTab(Tab::DiskTools));
        assert_eq!(s.active_tool(), Some(Tool::SurfaceTest));
        s.handle(Cmd::OpenTool(Tool::Smart));
        assert_eq!((s.tab, s.active_tool()), (Tab::DiskTools, Some(Tool::Smart)));
        s.handle(Cmd::OpenTab(Tab::Disks));
        s.handle(Cmd::OpenTab(Tab::DiskTools));
        assert_eq!(s.active_tool(), Some(Tool::Smart), "remembered");
        // leaving a tab closes nothing
        assert_eq!(s.windows.len(), 3);
        // Create Image fills the image tab; it is not a dialog, and closing it goes back
        s.handle(Cmd::OpenTab(Tab::Image));
        assert_eq!(s.active_tool(), Some(Tool::CreateImage));
        assert!(s.tool_open(Tool::CreateImage) && !s.modal());
        s.handle(Cmd::OpenTab(Tab::Disks));
        assert!(s.tool_open(Tool::CreateImage), "kept while another tab is shown");
        s.handle(Cmd::OpenTab(Tab::Image));
        s.handle(Cmd::CloseWizard);
        assert_eq!(s.tab, Tab::Disks);
        // the wizard of an action is still a dialog
        s.handle(Cmd::Action(Action::CopyDisk, Selected::Disk(1)));
        assert!(s.modal());
        s.handle(Cmd::CloseWizard);
        // an action on a selection starts its tool over on that selection
        s.handle(Cmd::Action(Action::SurfaceTest, Selected::Partition(1, 3)));
        assert_eq!((s.tab, s.active_tool()), (Tab::DiskTools, Some(Tool::SurfaceTest)));
        assert_eq!(s.windows.len(), 3);
        assert!(!s.disks_locked());
    }

    #[test]
    fn finishing_a_wizard_queues_or_shows_the_error_on_it() {
        let mut s = mock_state();
        s.handle(Cmd::Action(Action::FormatPartition, Selected::Partition(1, 3)));
        // rejected by the queue: the wizard stays open on its summary page with the message
        s.handle(Cmd::FinishWizard(vec![Operation::DeletePartition { disk: 1, number: 99 }]));
        let w = &s.wizard.as_ref().unwrap().wizard;
        assert!(w.on_summary());
        assert!(w.error().unwrap().contains("no partition 99"), "{:?}", w.error());
        assert!(s.queue.is_empty() && s.dialog.is_none());
        // accepted: queued and closed
        let ops = s.wizard.as_ref().unwrap().wizard.build_ops().unwrap();
        s.handle(Cmd::FinishWizard(ops));
        assert!(s.wizard.is_none());
        assert_eq!(s.queue.len(), 1);
        assert!(matches!(s.queue.ops()[0], Operation::FormatPartition { disk: 1, number: 3, .. }));
    }

    #[test]
    fn finishing_create_image_starts_it_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.qcow2");
        let image = || Operation::ImagePartition {
            disk: 1,
            number: 3,
            path: path.clone(),
            format: pm_image::ImageFormat::Qcow2,
            mode: pm_ops::CloneMode::Sector,
            rescue: None,
        };
        let mut s = mock_state();
        s.handle(Cmd::OpenTool(Tool::CreateImage));
        assert_eq!(s.wizard.as_ref().unwrap().wizard.finish_label(), "Start");
        s.handle(Cmd::FinishWizard(vec![image()]));
        assert!(s.wizard.is_none() && s.dialog.is_none());
        assert!(matches!(s.job, Some(RunningJob { kind: JobKind::Apply { dry_run: false }, .. })), "Apply runs");
        wait_idle(&mut s);
        match &s.dialog {
            Some(Dialog::Report { report, .. }) => assert_eq!((report.completed, &report.failed), (1, &None), "{report:?}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(path.exists());
        assert!(s.queue.is_empty());
        s.handle(Cmd::CloseDialog);

        // other operations are pending (they would run first): the Apply confirmation opens
        s.handle(Cmd::Push(vec![Operation::DeletePartition { disk: 0, number: 6 }]));
        s.handle(Cmd::FinishWizard(vec![image()]));
        assert!(s.job.is_none());
        assert!(matches!(s.dialog, Some(Dialog::Confirm { .. })));
        assert_eq!(s.queue.len(), 2);
        s.handle(Cmd::CloseDialog);
        s.handle(Cmd::Discard);

        // a restore overwrites its target: never without the confirmation
        s.handle(Cmd::FinishWizard(vec![Operation::RestoreImage { path: path.clone(), dst_disk: 1, dst_start: None }]));
        assert!(s.job.is_none());
        assert!(matches!(s.dialog, Some(Dialog::Confirm { .. })), "{:?}", s.dialog);
        s.handle(Cmd::CloseDialog);
        s.handle(Cmd::Discard);

        // other wizards keep queueing
        s.handle(Cmd::FinishWizard(vec![Operation::DeletePartition { disk: 0, number: 6 }]));
        assert!(s.job.is_none() && s.dialog.is_none());
        assert_eq!(s.queue.len(), 1);
    }

    #[test]
    fn push_command_and_properties_window() {
        let mut s = mock_state();
        s.handle(Cmd::Push(vec![Operation::WipeDisk { disk: 1, method: WipeMethod::Zero }]));
        assert!(s.dialog.is_none());
        assert_eq!(s.queue.len(), 1);
        s.handle(Cmd::Discard);
        s.handle(Cmd::Action(Action::DiskProperties, Selected::Disk(0)));
        assert_eq!(s.properties, Some(0));
    }

    #[test]
    fn apply_dry_run_keeps_queue_and_reports() {
        let mut s = mock_state();
        s.dry_run = true;
        s.handle(Cmd::Push(vec![Operation::DeletePartition { disk: 0, number: 6 }]));
        s.handle(Cmd::Apply);
        assert!(matches!(s.dialog, Some(Dialog::Confirm { .. })));
        s.handle(Cmd::ConfirmApply { allow_system: false });
        assert!(s.modal());
        wait_idle(&mut s);
        match &s.dialog {
            Some(Dialog::Report { report, dry_run: true, .. }) => {
                assert_eq!(report.completed, 1, "{report:?}");
                assert!(report.failed.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(s.queue.len(), 1, "dry run keeps the queue");
    }

    #[test]
    fn destructive_flags_follow_the_model() {
        let base = model_of(vec![gpt_disk(1)]);
        let ops = vec![
            Operation::SetLabel { disk: 1, number: 3, label: "x".into() },
            // grow into the free tail: not destructive
            Operation::ResizeMove { disk: 1, number: 3, new_start: 117 * MIB, new_len: 700 * MIB },
            // shrink: destructive
            Operation::ResizeMove { disk: 1, number: 3, new_start: 117 * MIB, new_len: 600 * MIB },
            Operation::DeletePartition { disk: 1, number: 3 },
        ];
        assert_eq!(destructive_flags(&base, &ops), [false, false, true, true]);
    }

    #[test]
    fn confirm_apply_without_the_dialog_does_nothing() {
        let mut s = mock_state();
        s.handle(Cmd::Push(vec![Operation::DeletePartition { disk: 0, number: 6 }]));
        s.handle(Cmd::ConfirmApply { allow_system: true });
        assert!(s.job.is_none(), "no Apply without the confirmation");
        assert_eq!(s.queue.len(), 1);
        // with another dialog open it is refused as well
        s.dialog = Some(Dialog::Error("x".into()));
        s.handle(Cmd::ConfirmApply { allow_system: false });
        assert!(s.job.is_none());
        assert!(matches!(s.dialog, Some(Dialog::Error(_))));
        assert_eq!(s.queue.len(), 1);
    }

    #[test]
    fn refresh_is_refused_while_a_dialog_is_open() {
        let mut s = mock_state();
        s.handle(Cmd::Push(vec![Operation::DeletePartition { disk: 0, number: 6 }]));
        s.handle(Cmd::Apply);
        assert!(matches!(s.dialog, Some(Dialog::Confirm { .. })));
        s.handle(Cmd::Refresh);
        assert!(s.job.is_none(), "no refresh (and no rebase) under the confirmation");
        s.handle(Cmd::CloseDialog);
        s.handle(Cmd::Refresh);
        assert!(s.busy());
        wait_idle(&mut s);
        assert_eq!(s.queue.len(), 1, "rebased, still valid");
    }

    #[test]
    fn failed_apply_refreshes_and_drops_the_queue() {
        let mut s = mock_state();
        s.handle(Cmd::Push(vec![Operation::DeletePartition { disk: 0, number: 6 }]));
        assert_eq!(s.queue.len(), 1);
        // a worker that dies mid-Apply ("job stopped unexpectedly")
        let handle = spawn_job("Applying changes", |_p, _c| panic!("worker died"));
        s.job = Some(RunningJob { kind: JobKind::Apply { dry_run: false }, handle });
        wait_idle(&mut s); // the Apply error, then the refresh it starts
        assert!(matches!(&s.dialog, Some(Dialog::Error(m)) if m.contains("Apply failed")));
        assert!(s.queue.is_empty(), "queue reset on the re-read disks, not rebased");
        assert!(!s.reset_on_refresh);
        assert_eq!(s.preview().disks.len(), 2);
    }

    #[test]
    fn rebase_drops_ops_that_no_longer_apply() {
        let mut q = OpQueue::new(model_of(vec![gpt_disk(1)]));
        q.push(Operation::DeletePartition { disk: 1, number: 3 }).unwrap();
        // same layout: kept
        assert_eq!(rebase(&mut q, model_of(vec![gpt_disk(1)])), 0);
        assert_eq!(q.len(), 1);
        // partition 3 vanished: dropped
        let mut d = gpt_disk(1);
        d.partitions.retain(|p| p.number != 3);
        assert_eq!(rebase(&mut q, model_of(vec![d])), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn rebase_drops_ops_whose_partition_was_replaced() {
        let mut q = OpQueue::new(model_of(vec![gpt_disk(1)]));
        q.push(Operation::SetLetter { disk: 1, number: 3, letter: Some('F') }).unwrap();
        q.push(Operation::DeletePartition { disk: 1, number: 3 }).unwrap();
        let n = q.len();
        assert_eq!(n, 2);
        // partition 3 is still number 3, but another partition (other GUID) at another place
        let mut d = gpt_disk(1);
        d.partitions[2].start += MIB;
        d.partitions[2].guid = Some(uuid::Uuid::from_u128(0xBEEF));
        assert_eq!(rebase(&mut q, model_of(vec![d])), n);
        assert!(q.is_empty());
        // a new partition created in the queue is followed through the rebase
        let mut q = OpQueue::new(model_of(vec![gpt_disk(1)]));
        q.push(Operation::CreatePartition {
            disk: 1,
            start: 801 * MIB,
            len: 100 * MIB,
            ptype: pm_core::PartitionType::Gpt(pm_core::gpt_types::BASIC_DATA),
            fs: None,
            label: String::new(),
            letter: None,
            name: String::new(),
        })
        .unwrap();
        q.push(Operation::DeletePartition { disk: 1, number: 4 }).unwrap();
        assert_eq!(rebase(&mut q, model_of(vec![gpt_disk(1)])), 0);
        assert_eq!(q.len(), 2);
        // a different disk under the same index
        let mut other = gpt_disk(1);
        other.serial = "OTHER".into();
        assert_eq!(rebase(&mut q, model_of(vec![other])), 2);
    }

    #[test]
    fn system_disk_ops_flags_ops_on_the_system_disk() {
        let mut base = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        base.system_disk = Some(1);
        let ops = vec![
            Operation::DeletePartition { disk: 0, number: 6 },
            Operation::SetLabel { disk: 1, number: 3, label: "x".into() },
            Operation::Unmount { volume: VolumeId("vol-e".into()), at: pm_platform::MountPoint::Letter('E') },
            Operation::CloneDisk {
                src_disk: 0,
                dst_disk: 1,
                sizing: pm_ops::Sizing::Fit,
                mode: pm_ops::CloneMode::Smart,
                boot_fix: pm_ops::BootFix::None,
                target_table: None,
            },
        ];
        assert_eq!(system_disk_ops(&base, &ops), [1, 2, 3]);
        base.system_disk = None;
        assert!(system_disk_ops(&base, &ops).is_empty(), "no C: anywhere");
    }
}
