//! The update check and install, as seen by the application.
//!
//! [`UpdateState`] (in [`AppState::update`]) owns the settings file, the notice of a newer
//! release and the two workers of the updater: the check and the download/install. Both run
//! as [`Task`]s beside the job slot; [`AppState::poll_job`] collects them. The update
//! commands ([`Cmd::CheckUpdates`] … [`Cmd::OpenUrl`]) are applied by
//! [`AppState::handle_update`]. Nothing here starts a check on its own.

use crate::app::{AppState, Cmd, Dialog};
use crate::settings::{Found, Settings};
use crate::tools::{error_text, Task};
use pm_core::Error;
use pm_update::{Check, Fetch, InstallKind, Release, Version};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

/// The toast shown when an update or a restart must wait.
const WAIT_TEXT: &str = "Wait for the running work to finish first";

/// A newer release to point out (from a check of this session or remembered from an earlier one).
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub version: Version,
    pub page_url: String,
}

/// What to open once the running check has found the release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    WhatsNew,
    Ask,
}

/// A running check.
struct Checking {
    task: Task<Check>,
    /// Asked for by the user: its outcome is reported, a skipped version is shown anyway.
    manual: bool,
    then: Option<Then>,
}

/// Everything the application knows and does about updates.
pub struct UpdateState {
    pub settings: Settings,
    /// The newer release to point out; `None` when there is none or it was skipped.
    pub notice: Option<Notice>,
    /// The notice was hidden for this session.
    pub hidden: bool,
    /// The running version.
    pub current: Version,
    /// The release of the notice can be installed here (see [`UpdateState::install_problem`]).
    pub can_install: bool,
    /// The running download and install; yields the installed version.
    pub downloading: Option<Task<Version>>,
    fetch: Arc<dyn Fetch>,
    settings_path: Option<PathBuf>,
    install_dir: Option<PathBuf>,
    /// Why nothing can be installed into `install_dir`, whatever the release.
    folder_problem: Option<String>,
    /// The newer release found by a check of this session.
    release: Option<Release>,
    checking: Option<Checking>,
}

/// The [`Fetch`] of [`UpdateState::disabled`]: every request fails, nothing is contacted.
struct NoFetch;

impl Fetch for NoFetch {
    fn get(&self, _url: &str) -> pm_core::Result<Box<dyn Read + Send>> {
        Err(Error::Unsupported("update checks are not available in this session".into()))
    }
}

/// Unix time in seconds.
fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl UpdateState {
    /// The update state of a run of version `current`: the settings are read from
    /// `settings_path` (defaults without one), a remembered newer release becomes the notice
    /// when it is still newer than `current` and not skipped (and is forgotten otherwise),
    /// and `install_dir` is probed once for whether it can be written. Nothing is fetched.
    pub fn new(
        fetch: Arc<dyn Fetch>,
        settings_path: Option<PathBuf>,
        install_dir: Option<PathBuf>,
        current: Version,
    ) -> UpdateState {
        let settings = settings_path.as_deref().map(Settings::load).unwrap_or_default();
        let folder_problem = match &install_dir {
            None => Some("the folder of the application is unknown".to_string()),
            Some(dir) => match pm_update::install_kind(dir) {
                InstallKind::Portable => None,
                InstallKind::Unavailable(why) => Some(why),
            },
        };
        let mut u = UpdateState {
            settings,
            notice: None,
            hidden: false,
            current,
            can_install: false,
            downloading: None,
            fetch,
            settings_path,
            install_dir,
            folder_problem,
            release: None,
            checking: None,
        };
        if let Some(found) = u.settings.found.clone() {
            match Version::parse(&found.version) {
                Some(version) if version > current && !u.is_skipped(version) => {
                    u.notice = Some(Notice { version, page_url: found.page_url })
                }
                _ => {
                    u.settings.found = None;
                    u.save();
                }
            }
        }
        u.refresh_can_install();
        u
    }

    /// No settings file, no install folder and a [`Fetch`] that always fails: never touches
    /// the network, the configuration folder or the install folder.
    pub fn disabled() -> UpdateState {
        UpdateState::new(Arc::new(NoFetch), None, None, Version::current())
    }

    /// A check or an install is running.
    pub fn busy(&self) -> bool {
        self.checking.is_some() || self.downloading.is_some()
    }

    /// A check is running.
    pub fn checking(&self) -> bool {
        self.checking.is_some()
    }

    /// The newer release found by a check of this session.
    pub fn release(&self) -> Option<&Release> {
        self.release.as_ref()
    }

    /// Why the release of the notice cannot be installed here, if it cannot.
    pub fn install_problem(&self) -> Option<String> {
        if let Some(p) = &self.folder_problem {
            return Some(p.clone());
        }
        let r = self.release.as_ref()?;
        if r.archive.is_none() {
            Some(format!("release {} has no download for this system", r.version))
        } else if r.sums_url.is_none() {
            Some(format!("release {} has no checksums (SHA256SUMS.txt)", r.version))
        } else {
            None
        }
    }

    /// Start a check, unless one is running (a manual request then makes it manual). A manual
    /// check reports its outcome and shows a skipped version too; an automatic one only ever
    /// sets the notice.
    pub fn start_check(&mut self, manual: bool) {
        self.start_check_then(manual, None);
    }

    fn start_check_then(&mut self, manual: bool, then: Option<Then>) {
        if let Some(c) = &mut self.checking {
            c.manual |= manual;
            c.then = then.or(c.then);
            return;
        }
        let fetch = self.fetch.clone();
        let current = self.current;
        let task = Task::spawn("Checking for updates", move |_p, _c| pm_update::check(&*fetch, current));
        self.checking = Some(Checking { task, manual, then });
    }

    fn is_skipped(&self, v: Version) -> bool {
        self.settings.skipped.as_deref().and_then(Version::parse) == Some(v)
    }

    fn refresh_can_install(&mut self) {
        self.can_install = self.install_problem().is_none();
    }

    /// Write the settings file; a failure is logged and otherwise ignored.
    fn save(&self) {
        if let Some(path) = &self.settings_path {
            if let Err(e) = self.settings.save(path) {
                log::warn!("cannot save the settings to {}: {e}", path.display());
            }
        }
    }

    /// Forget the newer release (it is installed, or there is none).
    fn clear_found(&mut self) {
        self.notice = None;
        self.release = None;
        self.settings.found = None;
        self.refresh_can_install();
    }

    /// The release page of the notice, if any.
    fn page_url(&self) -> Option<String> {
        self.release.as_ref().map(|r| r.page_url.clone()).or_else(|| self.notice.as_ref().map(|n| n.page_url.clone()))
    }
}

impl AppState {
    /// Use `u` as the update state.
    pub fn with_update(mut self, u: UpdateState) -> AppState {
        self.update = u;
        self
    }

    /// The automatic check: starts one when the settings say it is due at `now` (Unix seconds).
    pub fn start_auto_check(&mut self, now: u64) {
        if self.update.settings.check_due(now) {
            self.update.start_check(false);
        }
    }

    /// Collect the finished check and install, if any (from [`AppState::poll_job`]).
    pub(crate) fn poll_update(&mut self) {
        if let Some(r) = self.update.checking.as_mut().and_then(|c| c.task.poll()) {
            let c = self.update.checking.take().expect("polled above");
            self.check_finished(r, c.manual, c.then);
        }
        if let Some(r) = self.update.downloading.as_mut().and_then(|t| t.poll()) {
            self.update.downloading = None;
            self.install_finished(r);
        }
    }

    fn check_finished(&mut self, r: pm_core::Result<Check>, manual: bool, then: Option<Then>) {
        let u = &mut self.update;
        match r {
            Ok(Check::Newer(release)) => {
                u.settings.last_check = unix_now();
                u.settings.found = Some(Found { version: release.version.to_string(), page_url: release.page_url.clone() });
                if manual || !u.is_skipped(release.version) {
                    u.notice = Some(Notice { version: release.version, page_url: release.page_url.clone() });
                    if manual {
                        u.hidden = false;
                    }
                } else {
                    u.notice = None;
                }
                u.release = Some(release);
                u.refresh_can_install();
                u.save();
                match then {
                    Some(Then::WhatsNew) => self.handle_update(Cmd::UpdateWhatsNew),
                    Some(Then::Ask) => self.handle_update(Cmd::UpdateAsk),
                    None => {}
                }
            }
            Ok(Check::UpToDate) => {
                u.settings.last_check = unix_now();
                u.clear_found();
                u.save();
                if manual {
                    let text = format!("You have the latest version ({})", u.current);
                    self.toast(text);
                }
            }
            Err(e) => {
                log::warn!("update check failed: {e}");
                if manual {
                    self.dialog = Some(Dialog::Error(format!("Cannot check for updates: {}", error_text(&e))));
                }
            }
        }
    }

    /// The install ended with `r`. Only `Error::Cancelled` means a cancel stopped it in time:
    /// a cancel that arrived during the swap (which cannot stop) leaves its real outcome,
    /// installed or failed (possibly without rollback), and that is what is shown.
    fn install_finished(&mut self, r: pm_core::Result<Version>) {
        match r {
            Ok(version) => {
                self.update.clear_found();
                self.update.save();
                self.dialog = Some(Dialog::Updated { version });
            }
            Err(Error::Cancelled) => log::info!("update cancelled"),
            Err(e) => {
                log::warn!("update failed: {e}");
                // a failed rollback names the files to restore by hand: shown in full
                let mut text = format!("The update failed: {}", error_text(&e));
                if let Some(page) = self.update.page_url() {
                    text.push_str(&format!("\n\nThe release can be downloaded from {page}"));
                }
                self.dialog = Some(Dialog::Error(text));
            }
        }
    }

    /// Apply one of the update commands.
    pub(crate) fn handle_update(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::CheckUpdates => self.update.start_check(true),
            Cmd::SetCheckUpdates(on) => {
                self.update.settings.check_updates = on;
                self.update.save();
            }
            Cmd::UpdateWhatsNew => match self.update.release.clone() {
                Some(release) => self.dialog = Some(Dialog::WhatsNew { release }),
                None if self.update.notice.is_some() => self.update.start_check_then(true, Some(Then::WhatsNew)),
                None => {}
            },
            Cmd::UpdateAsk => {
                if self.anything_running() {
                    self.toast(WAIT_TEXT);
                    return;
                }
                match self.update.release.clone() {
                    Some(release) => self.dialog = Some(Dialog::UpdateConfirm { release }),
                    None if self.update.notice.is_some() => self.update.start_check_then(true, Some(Then::Ask)),
                    None => {}
                }
            }
            Cmd::UpdateStart => self.start_install(),
            Cmd::UpdateCancel => {
                if let Some(t) = &self.update.downloading {
                    t.cancel();
                }
            }
            Cmd::UpdateSkip => {
                let version = self.update.notice.as_ref().map(|n| n.version).or(self.update.release.as_ref().map(|r| r.version));
                if let Some(v) = version {
                    self.update.settings.skipped = Some(v.to_string());
                    self.update.notice = None;
                    self.update.save();
                }
            }
            Cmd::UpdateHide => self.update.hidden = true,
            Cmd::UpdateRestart => {
                if self.anything_running() {
                    self.toast(WAIT_TEXT);
                    return;
                }
                let Some(dir) = self.update.install_dir.clone() else {
                    self.dialog = Some(Dialog::Error("Cannot restart: the folder of the application is unknown".into()));
                    return;
                };
                let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
                match pm_update::restart(&dir, &args) {
                    Ok(()) => self.quit = true,
                    Err(e) => self.dialog = Some(Dialog::Error(format!("Cannot restart: {}", error_text(&e)))),
                }
            }
            Cmd::OpenUrl(url) => {
                if !pm_update::url_allowed(&url) {
                    log::warn!("not opening {url:?}: not a GitHub address");
                } else if let Err(e) = pm_platform::shell::open_url(&url) {
                    log::warn!("cannot open {url}: {e}");
                    self.toast(format!("Cannot open the browser; the address is {url}"));
                }
            }
            other => log::warn!("not an update command: {other:?}"),
        }
    }

    /// [`Cmd::UpdateStart`]: only from the confirmation the user is looking at, with nothing
    /// else running, and only for a release that can be installed here.
    fn start_install(&mut self) {
        let Some(Dialog::UpdateConfirm { release }) = &self.dialog else { return };
        let release = release.clone();
        if self.anything_running() {
            self.toast(WAIT_TEXT);
            return;
        }
        self.update.release = Some(release.clone());
        self.update.refresh_can_install();
        let dir = match (self.update.install_problem(), self.update.install_dir.clone()) {
            (None, Some(dir)) => dir,
            (problem, _) => {
                let why = problem.unwrap_or_else(|| "the folder of the application is unknown".into());
                self.dialog = Some(Dialog::Error(format!(
                    "Cannot update here: {why}.\n\nThe release can be downloaded from {}",
                    release.page_url
                )));
                return;
            }
        };
        self.dialog = None;
        let fetch = self.update.fetch.clone();
        self.update.downloading = Some(Task::spawn("Updating Partition Manager", move |progress, cancel| {
            pm_update::update(&*fetch, &release, &dir, progress, cancel).map(|()| release.version)
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppState, Cmd, Dialog};
    use crate::settings::{Found, Settings};
    use crate::test_util::{mock_state, wait_idle};
    use crate::tools::{ToolEnv, ToolWindow, WindowKind, WindowOutcome};
    use pm_update::testing::{release_fixture, release_json, MapFetch};
    use pm_update::{Fetch, Release, Version, LATEST_URL};
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    const GUI: &str = if cfg!(windows) { "partition-manager.exe" } else { "partition-manager" };
    const PAGE: &str = "https://github.com/inalto/partition-manager/releases/tag/v0.9.0";

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn never_fetch() -> Arc<dyn Fetch> {
        Arc::new(MapFetch::new())
    }

    /// `fetch` also answering the release check with `r`.
    fn serving(fetch: MapFetch, r: &Release) -> MapFetch {
        fetch.with(LATEST_URL, release_json(r))
    }

    /// A fetcher for a complete release `version` of this platform.
    fn fake_release(version: &str) -> MapFetch {
        let (f, r) = release_fixture(version, true);
        serving(f, &r)
    }

    /// The application with update state over `fetch`, its settings file in a fresh folder
    /// (returned with the path, and kept alive by the `TempDir`), and `install` as the folder.
    fn state_installed(fetch: Arc<dyn Fetch>, current: &str, install: Option<PathBuf>) -> (AppState, PathBuf, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let u = UpdateState::new(fetch, Some(path.clone()), install, v(current));
        (mock_state().with_update(u), path, dir)
    }

    fn state_with(fetch: MapFetch, current: &str) -> (AppState, PathBuf, TempDir) {
        state_installed(Arc::new(fetch), current, None)
    }

    /// Poll until the check and the install have finished.
    fn wait_update(s: &mut AppState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while s.update.busy() {
            assert!(Instant::now() < deadline, "update work did not finish");
            std::thread::sleep(Duration::from_millis(5));
            s.poll_job();
        }
    }

    /// An install folder holding an old GUI executable.
    fn old_install() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(GUI), "old gui").unwrap();
        dir
    }

    fn read(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap()
    }

    fn toast(s: &AppState) -> String {
        s.toast.as_ref().map(|t| t.text.clone()).unwrap_or_default()
    }

    #[test]
    fn newer_release_shows_the_notice_and_is_remembered() {
        let (mut s, path, _dir) = state_with(fake_release("0.9.0"), "0.2.2");
        assert!(s.update.notice.is_none());
        s.update.start_check(false);
        wait_update(&mut s);
        assert_eq!(s.update.notice.as_ref().unwrap().version.to_string(), "0.9.0");
        assert_eq!(s.update.notice.as_ref().unwrap().page_url, PAGE);
        assert!(s.dialog.is_none() && s.toast.is_none());
        let saved = Settings::load(&path);
        assert_eq!(saved.found.unwrap(), Found { version: "0.9.0".into(), page_url: PAGE.into() });
        assert!(saved.last_check > 0);
        // a second start that does not check still shows it
        let again = UpdateState::new(never_fetch(), Some(path.clone()), None, v("0.2.2"));
        let n = again.notice.as_ref().unwrap();
        assert_eq!((n.version, n.page_url.as_str()), (v("0.9.0"), PAGE));
        // once this version runs, the remembered one is dropped
        let current = UpdateState::new(never_fetch(), Some(path.clone()), None, v("0.9.0"));
        assert!(current.notice.is_none() && current.settings.found.is_none());
    }

    #[test]
    fn equal_skipped_and_failed_checks_show_nothing() {
        // the same version: no notice, and a remembered newer one is forgotten
        let (mut s, path, _dir) = state_with(fake_release("0.9.0"), "0.9.0");
        s.update.settings.found = Some(Found { version: "0.9.5".into(), page_url: PAGE.into() });
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(s.update.notice.is_none() && s.dialog.is_none() && s.toast.is_none());
        let saved = Settings::load(&path);
        assert!(saved.found.is_none());
        assert!(saved.last_check > 0);

        // a skipped version: no notice
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        Settings { skipped: Some("0.9.0".into()), ..Settings::default() }.save(&path).unwrap();
        let u = UpdateState::new(Arc::new(fake_release("0.9.0")), Some(path.clone()), None, v("0.2.2"));
        let mut s = mock_state().with_update(u);
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(s.update.notice.is_none() && s.dialog.is_none() && s.toast.is_none());
        assert!(UpdateState::new(never_fetch(), Some(path), None, v("0.2.2")).notice.is_none());

        // a failed check: nothing shown, nothing remembered
        let (mut s, path, _dir) = state_with(MapFetch::new(), "0.2.2");
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(s.update.notice.is_none() && s.dialog.is_none() && s.toast.is_none());
        assert_eq!(Settings::load(&path).last_check, 0);
    }

    #[test]
    fn manual_check_reports_latest_or_the_error_and_ignores_skip() {
        let (mut s, _path, _dir) = state_with(fake_release("0.2.2"), "0.2.2");
        s.handle(Cmd::CheckUpdates);
        wait_update(&mut s);
        assert!(toast(&s).contains("You have the latest version (0.2.2)"), "{}", toast(&s));
        assert!(s.dialog.is_none() && s.update.notice.is_none());

        let (mut s, _path, _dir) = state_with(MapFetch::new(), "0.2.2");
        s.handle(Cmd::CheckUpdates);
        wait_update(&mut s);
        assert!(matches!(&s.dialog, Some(Dialog::Error(m)) if m.contains(LATEST_URL)), "{:?}", s.dialog);
        assert!(s.update.notice.is_none());

        let (mut s, _path, _dir) = state_with(fake_release("0.9.0"), "0.2.2");
        s.update.settings.skipped = Some("0.9.0".into());
        s.update.hidden = true;
        s.handle(Cmd::CheckUpdates);
        wait_update(&mut s);
        assert_eq!(s.update.notice.as_ref().unwrap().version, v("0.9.0"));
        assert!(!s.update.hidden, "a manual check shows the notice again");
        assert!(s.dialog.is_none());
    }

    #[test]
    fn a_second_check_while_one_runs_is_not_started() {
        let fetch = Arc::new(fake_release("0.9.0"));
        let (mut s, _path, _dir) = state_installed(fetch.clone(), "0.2.2", None);
        s.update.start_check(false);
        s.handle(Cmd::CheckUpdates);
        wait_update(&mut s);
        assert_eq!(fetch.requested(), [LATEST_URL]);
        assert!(s.update.notice.is_some());
    }

    #[test]
    fn remembered_notice_checks_before_opening_its_dialogs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let found = Found { version: "0.9.0".into(), page_url: PAGE.into() };
        Settings { found: Some(found), ..Settings::default() }.save(&path).unwrap();
        let fetch = Arc::new(fake_release("0.9.0"));
        let u = UpdateState::new(fetch.clone(), Some(path.clone()), None, v("0.2.2"));
        let mut s = mock_state().with_update(u);
        assert_eq!(s.update.notice.as_ref().unwrap().version, v("0.9.0"));
        assert!(fetch.requested().is_empty(), "nothing is fetched at start");

        // What's new: one check (a second click does not start another), then the dialog
        s.handle(Cmd::UpdateWhatsNew);
        assert!(s.dialog.is_none() && s.update.busy());
        s.handle(Cmd::UpdateWhatsNew);
        wait_update(&mut s);
        assert_eq!(fetch.requested(), [LATEST_URL]);
        match &s.dialog {
            Some(Dialog::WhatsNew { release }) => {
                assert_eq!(release.version, v("0.9.0"));
                assert_eq!(release.notes, "What is new in 0.9.0");
            }
            other => panic!("unexpected {other:?}"),
        }
        // the release is known now: no further check
        s.handle(Cmd::CloseDialog);
        s.handle(Cmd::UpdateAsk);
        assert!(matches!(&s.dialog, Some(Dialog::UpdateConfirm { release }) if release.version == v("0.9.0")));
        assert_eq!(fetch.requested().len(), 1);

        // Update on a remembered notice checks first as well
        let u = UpdateState::new(fetch.clone(), Some(path), None, v("0.2.2"));
        let mut s = mock_state().with_update(u);
        s.handle(Cmd::UpdateAsk);
        assert!(s.dialog.is_none());
        wait_update(&mut s);
        assert!(matches!(&s.dialog, Some(Dialog::UpdateConfirm { release }) if release.version == v("0.9.0")));
        assert_eq!(fetch.requested().len(), 2);
    }

    #[test]
    fn skip_hide_and_the_checkbox_are_saved() {
        let (mut s, path, _dir) = state_with(fake_release("0.9.0"), "0.2.2");
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(s.update.notice.is_some());
        s.handle(Cmd::UpdateHide);
        assert!(s.update.hidden);
        assert!(s.update.notice.is_some(), "hidden for this session only");
        s.handle(Cmd::UpdateSkip);
        assert!(s.update.notice.is_none());
        assert_eq!(Settings::load(&path).skipped.as_deref(), Some("0.9.0"));
        assert!(UpdateState::new(never_fetch(), Some(path.clone()), None, v("0.2.2")).notice.is_none());

        s.handle(Cmd::SetCheckUpdates(false));
        assert!(!Settings::load(&path).check_updates);
        s.start_auto_check(u64::MAX / 2);
        assert!(!s.update.busy(), "the automatic check is off");
        s.handle(Cmd::SetCheckUpdates(true));
        assert!(Settings::load(&path).check_updates);
        let last = s.update.settings.last_check;
        s.start_auto_check(last + 60);
        assert!(!s.update.busy(), "checked less than a day ago");
        s.start_auto_check(last + 86_400);
        assert!(s.update.busy(), "due again");
        wait_update(&mut s);
    }

    /// A window with a worker that never finishes.
    struct Running;

    impl ToolWindow for Running {
        fn kind(&self) -> WindowKind {
            WindowKind::Benchmark
        }
        fn running(&self) -> bool {
            true
        }
        fn show(&mut self, _ctx: &eframe::egui::Context, _env: &ToolEnv) -> WindowOutcome {
            WindowOutcome::Open
        }
    }

    #[test]
    fn update_and_restart_wait_for_running_work() {
        let install = tempfile::tempdir().unwrap(); // empty: a restart would fail visibly
        let fetch: Arc<dyn Fetch> = Arc::new(fake_release("0.9.0"));
        let (mut s, _path, _dir) = state_installed(fetch, "0.2.2", Some(install.path().to_path_buf()));
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(!s.anything_running());

        // the job slot is busy (a refresh)
        s.start_refresh();
        assert!(s.anything_running());
        s.handle(Cmd::UpdateAsk);
        assert!(s.dialog.is_none());
        assert!(toast(&s).contains("Wait for the running work to finish first"), "{}", toast(&s));
        s.toast = None;
        s.handle(Cmd::UpdateRestart);
        assert!(!s.quit && s.dialog.is_none());
        assert!(toast(&s).contains("Wait for the running work to finish first"));
        wait_idle(&mut s);

        // a window's worker is running
        s.toast = None;
        s.windows.push(Box::new(Running));
        assert!(s.anything_running());
        s.handle(Cmd::UpdateAsk);
        assert!(s.dialog.is_none());
        assert!(toast(&s).contains("Wait for the running work to finish first"));
        s.handle(Cmd::UpdateRestart);
        assert!(!s.quit && s.dialog.is_none());

        // nothing runs any more: both go ahead
        s.windows.clear();
        assert!(!s.anything_running());
        s.handle(Cmd::UpdateAsk);
        assert!(matches!(s.dialog, Some(Dialog::UpdateConfirm { .. })));
    }

    #[test]
    fn restart_without_the_executable_reports_and_stays() {
        let install = tempfile::tempdir().unwrap();
        let (mut s, _path, _dir) = state_installed(never_fetch(), "0.2.2", Some(install.path().to_path_buf()));
        s.handle(Cmd::UpdateRestart);
        assert!(!s.quit);
        match &s.dialog {
            Some(Dialog::Error(m)) => assert!(m.contains(GUI), "{m}"),
            other => panic!("unexpected {other:?}"),
        }
        // without an install folder there is nothing to start either
        let (mut s, _path, _dir) = state_with(MapFetch::new(), "0.2.2");
        s.handle(Cmd::UpdateRestart);
        assert!(!s.quit);
        assert!(matches!(s.dialog, Some(Dialog::Error(_))));
    }

    #[test]
    fn update_installs_and_offers_restart() {
        let install = old_install();
        let fetch: Arc<dyn Fetch> = Arc::new(fake_release("0.9.0"));
        let (mut s, path, _dir) = state_installed(fetch, "0.2.2", Some(install.path().to_path_buf()));
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(s.update.can_install);
        s.handle(Cmd::UpdateAsk);
        assert!(matches!(&s.dialog, Some(Dialog::UpdateConfirm { release }) if release.version == v("0.9.0")));
        s.handle(Cmd::UpdateStart);
        assert!(s.dialog.is_none());
        assert!(s.update.downloading.is_some());
        assert!(s.modal() && s.anything_running());
        wait_update(&mut s);
        assert!(!s.modal() || s.dialog.is_some());
        match &s.dialog {
            Some(Dialog::Updated { version }) => assert_eq!(*version, v("0.9.0")),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(read(install.path(), GUI), format!("bin:{GUI}"));
        assert_eq!(read(install.path(), &format!("{GUI}.old")), "old gui");
        assert!(s.update.notice.is_none());
        assert!(Settings::load(&path).found.is_none());
        assert!(!s.anything_running());
    }

    #[test]
    fn update_start_needs_the_confirmation() {
        let install = old_install();
        let fetch: Arc<dyn Fetch> = Arc::new(fake_release("0.9.0"));
        let (mut s, _path, _dir) = state_installed(fetch, "0.2.2", Some(install.path().to_path_buf()));
        s.update.start_check(false);
        wait_update(&mut s);
        s.handle(Cmd::UpdateStart);
        assert!(s.update.downloading.is_none());
        assert_eq!(read(install.path(), GUI), "old gui");
    }

    #[test]
    fn failed_install_reports_and_changes_nothing() {
        let install = old_install();
        let (f, r) = release_fixture("0.9.0", false);
        let fetch: Arc<dyn Fetch> = Arc::new(serving(f, &r));
        let (mut s, path, _dir) = state_installed(fetch, "0.2.2", Some(install.path().to_path_buf()));
        s.update.start_check(false);
        wait_update(&mut s);
        s.handle(Cmd::UpdateAsk);
        s.handle(Cmd::UpdateStart);
        wait_update(&mut s);
        match &s.dialog {
            Some(Dialog::Error(m)) => {
                assert!(m.contains(PAGE), "{m}");
                assert!(m.to_lowercase().contains("checksum"), "{m}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(read(install.path(), GUI), "old gui");
        assert!(!install.path().join(format!("{GUI}.old")).exists());
        assert!(s.update.notice.is_some(), "still offered");
        assert!(Settings::load(&path).found.is_some());
    }

    /// Serves `inner` a few bytes at a time, slowly enough for a cancel to land mid-download.
    struct Slow(MapFetch);

    struct Trickle(Box<dyn Read + Send>);

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(2));
            let n = buf.len().min(8);
            self.0.read(&mut buf[..n])
        }
    }

    impl Fetch for Slow {
        fn get(&self, url: &str) -> pm_core::Result<Box<dyn Read + Send>> {
            Ok(Box::new(Trickle(self.0.get(url)?)))
        }
    }

    #[test]
    fn cancelled_install_shows_nothing_and_changes_nothing() {
        let install = old_install();
        let fetch: Arc<dyn Fetch> = Arc::new(Slow(fake_release("0.9.0")));
        let (mut s, _path, _dir) = state_installed(fetch, "0.2.2", Some(install.path().to_path_buf()));
        s.update.start_check(false);
        wait_update(&mut s);
        s.handle(Cmd::UpdateAsk);
        s.handle(Cmd::UpdateStart);
        assert!(s.update.downloading.is_some());
        s.handle(Cmd::UpdateCancel);
        wait_update(&mut s);
        assert!(s.dialog.is_none() && s.toast.is_none(), "{:?}", s.dialog);
        assert_eq!(read(install.path(), GUI), "old gui");
        assert!(s.update.notice.is_some());
    }

    /// An install worker that waits until it is cancelled and then returns `r`, like an
    /// install whose cancel arrived during the swap (which cannot stop any more).
    fn cancelled_during_swap(r: pm_core::Result<Version>) -> Task<Version> {
        Task::spawn("fake install", move |_p, c| {
            while c.check().is_ok() {
                std::thread::sleep(Duration::from_millis(1));
            }
            r
        })
    }

    /// The application after a check found 0.9.0, with `task` as the running install.
    fn installing(task: Task<Version>) -> (AppState, PathBuf, TempDir) {
        let (mut s, path, dir) = state_with(fake_release("0.9.0"), "0.2.2");
        s.update.start_check(false);
        wait_update(&mut s);
        s.update.downloading = Some(task);
        s.handle(Cmd::UpdateCancel);
        wait_update(&mut s);
        (s, path, dir)
    }

    #[test]
    fn a_swap_that_fails_after_cancel_is_still_reported_in_full() {
        let msg = "the update failed and could not be rolled back (rename failed); not restored: \
                   partition-manager.exe (previous version: C:\\pm\\partition-manager.exe.old)";
        let e = pm_core::Error::Invalid(msg.into());
        assert!(pm_update::rollback_failed(&e));
        let (s, _path, _dir) = installing(cancelled_during_swap(Err(e)));
        match &s.dialog {
            Some(Dialog::Error(m)) => {
                assert!(m.contains(msg), "{m}");
                assert!(m.contains(PAGE), "{m}");
            }
            other => panic!("unexpected {other:?}"),
        }
        // an ordinary failure during the swap as well
        let e = pm_core::Error::Invalid("cannot rename pm.exe".into());
        let (s, _path, _dir) = installing(cancelled_during_swap(Err(e)));
        assert!(matches!(&s.dialog, Some(Dialog::Error(m)) if m.contains("cannot rename pm.exe") && m.contains(PAGE)));
    }

    #[test]
    fn a_swap_that_succeeds_after_cancel_is_reported_as_updated() {
        let (s, path, _dir) = installing(cancelled_during_swap(Ok(v("0.9.0"))));
        assert!(matches!(&s.dialog, Some(Dialog::Updated { version }) if *version == v("0.9.0")), "{:?}", s.dialog);
        assert!(s.update.notice.is_none());
        assert!(Settings::load(&path).found.is_none());
        // a cancel that stopped the install in time shows nothing
        let (s, _path, _dir) = installing(cancelled_during_swap(Err(pm_core::Error::Cancelled)));
        assert!(s.dialog.is_none() && s.toast.is_none());
    }

    #[test]
    fn no_archive_or_unwritable_folder_cannot_install() {
        // the release has no archive for this platform
        let install = old_install();
        let (f, mut r) = release_fixture("0.9.0", true);
        r.archive = None;
        let fetch = Arc::new(serving(f, &r));
        let (mut s, _path, _dir) = state_installed(fetch.clone(), "0.2.2", Some(install.path().to_path_buf()));
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(s.update.notice.is_some());
        assert!(!s.update.can_install);
        // asking anyway: the reason and the page, nothing downloaded
        s.handle(Cmd::UpdateAsk);
        s.handle(Cmd::UpdateStart);
        assert!(s.update.downloading.is_none());
        assert!(matches!(&s.dialog, Some(Dialog::Error(m)) if m.contains(PAGE)), "{:?}", s.dialog);
        assert_eq!(fetch.requested(), [LATEST_URL]);

        // the release has no checksums
        let (f, mut r) = release_fixture("0.9.0", true);
        r.sums_url = None;
        let fetch: Arc<dyn Fetch> = Arc::new(serving(f, &r));
        let (mut s, _path, _dir) = state_installed(fetch, "0.2.2", Some(install.path().to_path_buf()));
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(s.update.notice.is_some());
        assert!(!s.update.can_install);

        // no install folder
        let (mut s, _path, _dir) = state_with(fake_release("0.9.0"), "0.2.2");
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(s.update.notice.is_some());
        assert!(!s.update.can_install);

        // the folder is not writable (here: it is not a folder at all)
        let not_a_dir = install.path().join(GUI);
        let fetch: Arc<dyn Fetch> = Arc::new(fake_release("0.9.0"));
        let (mut s, _path, _dir) = state_installed(fetch, "0.2.2", Some(not_a_dir));
        s.update.start_check(false);
        wait_update(&mut s);
        assert!(s.update.notice.is_some());
        assert!(!s.update.can_install);

        // the disabled state never can
        assert!(!UpdateState::disabled().can_install);
    }

    #[test]
    fn disabled_state_touches_nothing() {
        let mut s = mock_state();
        assert!(s.update.notice.is_none() && !s.update.busy() && !s.quit);
        s.handle(Cmd::CheckUpdates);
        wait_update(&mut s);
        assert!(matches!(s.dialog, Some(Dialog::Error(_))), "a manual check reports that it cannot check");
        s.handle(Cmd::SetCheckUpdates(false));
        assert!(!s.update.settings.check_updates);
    }

    #[test]
    fn open_url_refuses_non_https() {
        assert!(pm_platform::shell::open_url("file:///x").is_err());
        // URLs off the allow list are not even handed to the launcher
        let mut s = mock_state();
        s.handle(Cmd::OpenUrl("file:///x".into()));
        s.handle(Cmd::OpenUrl("https://example.com/".into()));
        assert!(s.toast.is_none() && s.dialog.is_none());
    }
}
