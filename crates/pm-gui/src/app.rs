//! Application state and the commands that change it.
//!
//! Views and dialogs read [`AppState`] and emit [`Cmd`]s; [`AppState::handle`] applies them
//! after the frame is drawn. The pending-operation queue is only ever mutated here, on the UI
//! thread; everything that can take time (enumeration, Apply) runs as a [`crate::jobs`] job.

use crate::actions::{Action, Effect};
use crate::jobs::{spawn_job, JobHandle, JobOutput};
use crate::selection::Selected;
use crate::theme::Theme;
use pm_core::VolumeId;
use pm_ops::{execute, ExecOpts, ExecReport, Model, OpQueue, Operation, WipeMethod};
use pm_platform::Platform;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a toast stays on screen.
const TOAST_TIME: Duration = Duration::from_secs(3);

/// Tool windows opened from the toolbar (placeholders until their own step lands).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    DataRecovery,
    PartitionRecovery,
    DiskBenchmark,
    SpaceAnalyzer,
    BackupImage,
}

impl Tool {
    pub const ALL: [Tool; 5] =
        [Tool::DataRecovery, Tool::PartitionRecovery, Tool::DiskBenchmark, Tool::SpaceAnalyzer, Tool::BackupImage];

    pub fn label(self) -> &'static str {
        match self {
            Tool::DataRecovery => "Data Recovery",
            Tool::PartitionRecovery => "Partition Recovery",
            Tool::DiskBenchmark => "Disk Benchmark",
            Tool::SpaceAnalyzer => "Space Analyzer",
            Tool::BackupImage => "Backup / Image",
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
    WipeMethod { disk: u32, method: WipeMethod },
    MountLetter { volume: VolumeId, letters: Vec<char>, choice: Option<char> },
    Unmount { volume: VolumeId, mount_points: Vec<String>, choice: usize },
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
    CloseTool,
    CloseProperties,
    SetTheme(Theme),
    SetDryRun(bool),
    DismissBanner,
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
    pub tool: Option<Tool>,
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
            tool: None,
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

    /// A modal dialog or Apply is in progress: the main window does not take input.
    pub fn modal(&self) -> bool {
        self.dialog.is_some() || matches!(self.job, Some(RunningJob { kind: JobKind::Apply { .. }, .. }))
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
        let dry_run = self.dry_run;
        let opts = ExecOpts { dry_run, allow_system_disk: allow_system };
        let handle = spawn_job("Applying changes", move |progress, cancel| {
            let report = execute(&*p, &ops, &opts, progress, cancel)?;
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
                    " Start Partition Manager as administrator, or open image files with --disk-file."
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
        let mut pushed = 0;
        for op in ops {
            if let Err(e) = self.queue.push(op) {
                for _ in 0..pushed {
                    self.queue.undo();
                }
                self.error(e.to_string());
                return;
            }
            pushed += 1;
        }
        self.selected = self.selected.revalidate(self.queue.preview());
    }

    fn dispatch(&mut self, action: Action, sel: Selected) {
        self.selected = sel;
        let Some(effect) = action.effect(&sel, self.queue.preview()) else {
            return;
        };
        match effect {
            Effect::Push(ops) => self.push_ops(ops),
            Effect::ChooseWipeMethod { disk } => {
                self.dialog = Some(Dialog::WipeMethod { disk, method: WipeMethod::Zero })
            }
            Effect::ChooseMountLetter { volume } => {
                // GetLogicalDrives: instant, fine on the UI thread
                let preview = self.queue.preview();
                let letters: Vec<char> =
                    self.platform.free_letters().into_iter().filter(|c| !preview.letter_in_use(*c)).collect();
                let choice = letters.first().copied();
                self.dialog = Some(Dialog::MountLetter { volume, letters, choice });
            }
            Effect::ChooseUnmount { volume, mount_points } => {
                self.dialog = Some(Dialog::Unmount { volume, mount_points, choice: 0 })
            }
            Effect::ShowProperties { disk } => self.properties = Some(disk),
            Effect::Explore { path } => explore(self, &path),
            Effect::Wizard(a) => {
                self.pending_action = Some((a, sel));
                self.toast(format!("{}: coming in the next step", a.label()));
            }
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
            Cmd::OpenTool(t) => self.tool = Some(t),
            Cmd::CloseTool => self.tool = None,
            Cmd::CloseProperties => self.properties = None,
            Cmd::SetTheme(t) => self.theme = t,
            Cmd::SetDryRun(d) => self.dry_run = d,
            Cmd::DismissBanner => self.banner = None,
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
/// validate (in order, up to the first that does not). Returns how many were dropped.
pub fn rebase(queue: &mut OpQueue, base: Model) -> usize {
    let ops = queue.ops().to_vec();
    queue.reset(base);
    let mut kept = 0;
    for op in &ops {
        if queue.push(op.clone()).is_err() {
            break;
        }
        kept += 1;
    }
    ops.len() - kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};
    use pm_core::{FsKind, MemDevice, VolumeInfo, MIB};
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
        assert_eq!(s.queue.len(), 1);
        assert_eq!(s.selected, Selected::Disk(1));
    }

    #[test]
    fn wizard_actions_are_remembered_with_a_toast() {
        let mut s = mock_state();
        s.handle(Cmd::Action(Action::FormatPartition, Selected::Partition(1, 3)));
        assert_eq!(s.pending_action, Some((Action::FormatPartition, Selected::Partition(1, 3))));
        assert!(s.toast.as_ref().unwrap().text.contains("Format"));
        assert!(s.queue.is_empty());
    }

    #[test]
    fn popups_for_wipe_mount_unmount_properties() {
        let mut s = mock_state();
        s.handle(Cmd::Action(Action::WipeDisk, Selected::Disk(1)));
        assert!(matches!(s.dialog, Some(Dialog::WipeMethod { disk: 1, .. })));
        s.handle(Cmd::Push(vec![Operation::WipeDisk { disk: 1, method: WipeMethod::Zero }]));
        assert!(s.dialog.is_none());
        assert_eq!(s.queue.len(), 1);
        s.handle(Cmd::Discard);
        s.handle(Cmd::Action(Action::Unmount, Selected::Partition(1, 3)));
        assert!(matches!(&s.dialog, Some(Dialog::Unmount { mount_points, .. }) if mount_points == &vec!["E:\\".to_string()]));
        s.handle(Cmd::CloseDialog);
        s.handle(Cmd::Action(Action::DiskProperties, Selected::Disk(0)));
        assert_eq!(s.properties, Some(0));
    }

    #[test]
    fn apply_dry_run_keeps_queue_and_reports() {
        let mut s = mock_state();
        s.dry_run = true;
        s.handle(Cmd::Action(Action::DeletePartition, Selected::Partition(0, 6)));
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
        s.handle(Cmd::Action(Action::DeletePartition, Selected::Partition(0, 6)));
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
        s.handle(Cmd::Action(Action::DeletePartition, Selected::Partition(0, 6)));
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
        s.handle(Cmd::Action(Action::DeletePartition, Selected::Partition(0, 6)));
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
            },
        ];
        assert_eq!(system_disk_ops(&base, &ops), [1, 2, 3]);
        base.system_disk = None;
        assert!(system_disk_ops(&base, &ops).is_empty(), "no C: anywhere");
    }
}
