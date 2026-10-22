//! Application state and the commands that change it.
//!
//! Views and dialogs read [`AppState`] and emit [`Cmd`]s; [`AppState::handle`] applies them
//! after the frame is drawn. The pending-operation queue is only ever mutated here, on the UI
//! thread; everything that can take time (enumeration, Apply) runs as a [`crate::jobs`] job.

use crate::actions::{Action, Effect};
use crate::jobs::{spawn_job, JobHandle, JobOutput};
use crate::selection::Selected;
use crate::theme::Theme;
use crate::tools::{Device, ToolEnv, ToolWindow};
use crate::wizard::{Wizard, WizardCtx};
use pm_ops::{execute_queued, ExecOpts, ExecReport, Model, OpQueue, Operation};
use pm_platform::Platform;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a toast stays on screen.
const TOAST_TIME: Duration = Duration::from_secs(3);

/// Tools opened from the toolbar: tool windows and wizards that are not sidebar actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    DataRecovery,
    PartitionRecovery,
    DiskBenchmark,
    SpaceAnalyzer,
    /// The "Backup / Image" menu ([`Tool::IMAGE_MENU`]).
    BackupImage,
    CreateImage,
    RestoreImage,
    RescueCopy,
    SurfaceTest,
    Smart,
}

impl Tool {
    /// The toolbar buttons (Backup / Image is a menu of [`Tool::IMAGE_MENU`]).
    pub const ALL: [Tool; 5] =
        [Tool::DataRecovery, Tool::PartitionRecovery, Tool::DiskBenchmark, Tool::SpaceAnalyzer, Tool::BackupImage];
    pub const IMAGE_MENU: [Tool; 3] = [Tool::CreateImage, Tool::RestoreImage, Tool::RescueCopy];
    /// The "Disk tools" menu.
    pub const MORE: [Tool; 2] = [Tool::SurfaceTest, Tool::Smart];

    pub fn label(self) -> &'static str {
        match self {
            Tool::DataRecovery => "Data Recovery",
            Tool::PartitionRecovery => "Partition Recovery",
            Tool::DiskBenchmark => "Disk Benchmark",
            Tool::SpaceAnalyzer => "Space Analyzer",
            Tool::BackupImage => "Backup / Image",
            Tool::CreateImage => "Create Image",
            Tool::RestoreImage => "Restore Image",
            Tool::RescueCopy => "Rescue Copy (ddrescue)",
            Tool::SurfaceTest => "Surface Test",
            Tool::Smart => "SMART",
        }
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
    OpenTool(Tool),
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
}

/// A wizard on screen and what it was opened on.
pub struct OpenWizard {
    pub wizard: Wizard,
    pub selection: Selected,
    /// Free drive letters of the platform, read once when the wizard opened.
    pub free_letters: Vec<char>,
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
    /// Open tool windows and job wizards (drawn in order; modal ones disable the main window).
    pub windows: Vec<Box<dyn ToolWindow>>,
    /// The open wizard (modal).
    pub wizard: Option<OpenWizard>,
    /// The next successful refresh drops the pending operations instead of rebasing them
    /// (set after a failed Apply, when some of them may already have been written).
    pub reset_on_refresh: bool,
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
            windows: Vec::new(),
            wizard: None,
            reset_on_refresh: false,
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

    /// A modal dialog, a wizard or Apply is in progress: the main window does not take input.
    pub fn modal(&self) -> bool {
        self.dialog.is_some()
            || self.wizard.is_some()
            || self.windows.iter().any(|w| w.modal())
            || matches!(self.job, Some(RunningJob { kind: JobKind::Apply { .. }, .. }))
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

    /// Collect the result of a finished job, if any. Call once per frame.
    pub fn poll_job(&mut self) {
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
        WizardCtx::new(self.queue.preview(), w.free_letters.clone(), w.selection)
    }

    /// Open the wizard of `action` on `sel`; actions without one (yet) are remembered in
    /// `pending_action` with a toast.
    fn open_wizard(&mut self, action: Action, sel: Selected) {
        // GetLogicalDrives: instant, fine on the UI thread
        let free_letters = self.platform.free_letters();
        let ctx = WizardCtx::new(self.queue.preview(), free_letters.clone(), sel);
        let wizard = crate::wizards::for_action(action, &ctx).or_else(|| crate::wizards::clone_for_action(action, &ctx));
        match wizard {
            Some(wizard) => self.wizard = Some(OpenWizard { wizard, selection: sel, free_letters }),
            None if action == Action::PartitionRecovery => self.open_tool(Tool::PartitionRecovery),
            None if action == Action::SurfaceTest => self.open_tool(Tool::SurfaceTest),
            None if crate::wizards::ACTIONS.contains(&action) || crate::wizards::CLONE_ACTIONS.contains(&action) => {
                self.toast(format!("{}: not available for this selection", action.label()))
            }
            None => {
                self.pending_action = Some((action, sel));
                self.toast(format!("{}: coming in the next step", action.label()));
            }
        }
    }

    /// Open tool `t` on the current selection: an operation wizard (images), a job wizard or a
    /// tool window (one of each kind; an open one stays as it is).
    pub fn open_tool(&mut self, t: Tool) {
        let sel = self.selected;
        let live = self.queue.base();
        let device = Device::from_selection(sel).filter(|d| d.range(live).is_some());
        let window: Box<dyn ToolWindow> = match t {
            Tool::CreateImage | Tool::RestoreImage => {
                let free_letters = Vec::new();
                let ctx = WizardCtx::new(self.queue.preview(), free_letters.clone(), sel);
                let w = if t == Tool::CreateImage {
                    crate::wizards::image_create::new(&ctx)
                } else {
                    crate::wizards::image_restore::new(&ctx)
                };
                match w {
                    Some(wizard) => self.wizard = Some(OpenWizard { wizard, selection: sel, free_letters }),
                    None => self.toast(format!("{}: there is no disk", t.label())),
                }
                return;
            }
            Tool::BackupImage => return,
            Tool::DataRecovery => Box::new(crate::wizards::data_recovery::DataRecoveryWizard::new(device)),
            Tool::PartitionRecovery => Box::new(crate::wizards::partition_recovery::PartitionRecoveryWizard::new(
                device.map(|d| d.disk()),
            )),
            Tool::RescueCopy => Box::new(crate::wizards::rescue::RescueWizard::new(device)),
            Tool::DiskBenchmark => Box::new(crate::tools::benchmark::BenchmarkWindow::new(
                sel.partition_in(live).and_then(|p| p.letter()).map(|c| format!("{c}:\\")),
            )),
            Tool::SpaceAnalyzer => Box::new(crate::tools::space_analyzer::SpaceAnalyzerWindow::new(
                sel.partition_in(live).and_then(|p| p.letter()).map(|c| format!("{c}:\\")),
            )),
            Tool::SurfaceTest => Box::new(crate::tools::surface_test::SurfaceTestWindow::new(device)),
            Tool::Smart => Box::new(crate::tools::smart::SmartWindow::new(device.map(|d| d.disk()))),
        };
        if !self.windows.iter().any(|w| w.kind() == window.kind()) {
            self.windows.push(window);
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
            Cmd::FinishWizard(ops) => match self.try_push_ops(ops) {
                Ok(()) => self.wizard = None,
                Err(e) => match &mut self.wizard {
                    Some(w) => w.wizard.set_error(e),
                    None => self.error(e),
                },
            },
            Cmd::CloseWizard => self.wizard = None,
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
    use crate::test_util::{gpt_disk, mbr_disk, model_of};
    use pm_core::{FsKind, MemDevice, VolumeId, VolumeInfo, MIB};
    use pm_ops::WipeMethod;
    use pm_platform::MockPlatform;

    fn wait_idle(s: &mut AppState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while s.busy() {
            assert!(Instant::now() < deadline, "job did not finish");
            std::thread::sleep(Duration::from_millis(5));
            s.poll_job();
        }
    }

    fn mock_state() -> AppState {
        let mock = MockPlatform::new();
        for d in [gpt_disk(1), mbr_disk(0)] {
            let mut dev = MemDevice::new(d.size);
            pm_parttable::write_table(&mut dev, &pm_ops::table_from_disk(&d)).unwrap();
            mock.add_disk(d.index, dev, d);
        }
        let vol = VolumeInfo {
            id: VolumeId("vol-e".into()),
            fs: Some(FsKind::Ntfs),
            label: "Data".into(),
            mount_points: vec!["E:\\".into()],
            total: 684 * MIB,
            free: 342 * MIB,
            ..Default::default()
        };
        mock.add_volume(vol, 1, 117 * MIB, 684 * MIB);
        // a system disk that is neither test disk, so the executor guard lets both through
        mock.set_system_disk(7);
        let mut s = AppState::new(Arc::new(mock), StartOptions::default());
        wait_idle(&mut s);
        s
    }

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
        assert!(s.modal(), "job wizards are modal");
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
