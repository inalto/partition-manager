//! Installing a verified release over the running copy.
//!
//! This runs with administrator rights and replaces the application's own
//! executables. It only ever touches files directly inside the install
//! folder under their fixed names; nothing an archive says chooses a
//! destination. Both executables are swapped or neither: the old files are
//! renamed to `<name>.old` (a running `.exe` cannot be overwritten or deleted
//! on Windows, but it can be renamed) and put back when anything fails.

use crate::download::download;
use crate::fetch::Fetch;
use crate::release::Release;
use crate::unpack::{exe_name, unpack, DOCUMENTS, EXECUTABLES};
use crate::version::TARGET;
use pm_core::{CancelToken, Error, ProgressSink, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Name of the staging folder inside the install folder.
pub const STAGING: &str = ".pm-update";

/// The folder of the running executable, with symlinks resolved. On Windows
/// the `\\?\` prefix `canonicalize` adds is stripped so that messages show a
/// normal path.
pub fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| Error::Invalid(format!("cannot locate the running program: {e}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| Error::Invalid(format!("{} has no folder", exe.display())))?;
    Ok(strip_verbatim(dir))
}

/// `\\?\C:\...` becomes `C:\...`; every other path, verbatim or not (`\\?\UNC\...`,
/// `\\?\Volume{...}\...`), is returned unchanged: dropping its prefix would change its
/// meaning or make it relative.
fn strip_verbatim(path: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut components = path.components();
    if let Some(Component::Prefix(p)) = components.next() {
        if let Prefix::VerbatimDisk(drive) = p.kind() {
            let mut plain = PathBuf::from(format!("{}:", char::from(drive)));
            plain.push(components.as_path());
            return plain;
        }
    }
    path.to_path_buf()
}

/// Start of the name of the file `install_kind` creates to test the folder.
const PROBE: &str = ".pm-update-probe-";

/// Whether this installation can update itself.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallKind {
    Portable,
    /// Not possible, with a one-line reason.
    Unavailable(String),
}

/// `Unavailable` when this platform has no release archive or when nothing
/// can be created in `dir` (a system-wide, read-only or missing folder). The test
/// creates and removes a probe file; one left over by an earlier run with the same
/// process id is removed first.
pub fn install_kind(dir: &Path) -> InstallKind {
    if TARGET.is_none() {
        return InstallKind::Unavailable("self-update is not available on this platform".into());
    }
    let probe = dir.join(format!("{PROBE}{}", std::process::id()));
    if std::fs::symlink_metadata(&probe).is_ok_and(|m| !m.is_dir()) {
        let _ = std::fs::remove_file(&probe);
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            InstallKind::Portable
        }
        Err(e) => InstallKind::Unavailable(format!(
            "{} is not writable ({})",
            dir.display(),
            e.to_string().lines().next().unwrap_or("unknown error")
        )),
    }
}

fn executables() -> impl Iterator<Item = String> {
    EXECUTABLES.iter().map(|n| exe_name(n, cfg!(windows)))
}

fn old_name(name: &str) -> String {
    format!("{name}.old")
}

fn io_error(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::Invalid(format!("cannot {what} {}: {e}", path.display()))
}

/// Replaces the installed executables in `dir` with those in `staged`, then
/// the installed documents. Both executables are swapped or neither: on any
/// error the folder is back as it was. The old files stay as `<name>.old`
/// until `clean_up`.
pub fn swap(staged: &Path, dir: &Path) -> Result<()> {
    swap_with(staged, dir, &mut |from, to| std::fs::rename(from, to))
}

/// `swap` with the rename operation injectable, so that failures half-way
/// (in the swap or in the rollback) can be tested.
fn swap_with(
    staged: &Path,
    dir: &Path,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    // The running program is always replaced; `pm` only when it is installed.
    let gui = exe_name("partition-manager", cfg!(windows));
    if !dir.join(&gui).exists() {
        return Err(Error::Invalid(format!("{gui} is not in {}", dir.display())));
    }
    let installed: Vec<String> = executables().filter(|n| dir.join(n).exists()).collect();

    // Everything that can be checked without changing anything comes first.
    for name in &installed {
        let new = staged.join(name);
        let is_file = std::fs::symlink_metadata(&new).map(|m| m.is_file()).unwrap_or(false);
        if !is_file {
            return Err(Error::Invalid(format!("the update does not contain {name}")));
        }
    }
    for name in &installed {
        let old = dir.join(old_name(name));
        match std::fs::symlink_metadata(&old) {
            Ok(_) => std::fs::remove_file(&old)
                .map_err(|e| io_error("remove the previous version", &old, e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error("inspect", &old, e)),
        }
    }

    let mut done: Vec<&String> = Vec::new();
    for name in &installed {
        if let Err(failure) = swap_one(staged, dir, name, rename) {
            let mut stuck = failure.stuck;
            for name in done.into_iter().rev() {
                stuck.extend(undo_one(staged, dir, name, rename));
            }
            return Err(if stuck.is_empty() {
                failure.cause
            } else {
                rollback_error(&failure.cause, dir, &stuck)
            });
        }
        done.push(name);
    }

    // Best effort: a missing or failing document never fails the update.
    for name in DOCUMENTS {
        let target = dir.join(name);
        if target.is_file() {
            let _ = std::fs::copy(staged.join(name), &target);
        }
    }
    Ok(())
}

/// Start of the message of a failed rollback; see `rollback_failed`.
const ROLLBACK_FAILED: &str = "the update failed and could not be rolled back";

/// The error for a swap that failed with `cause` after which the files in
/// `stuck` could not be restored: it names each of them and where its
/// previous version survives.
fn rollback_error(cause: &Error, dir: &Path, stuck: &[String]) -> Error {
    let files: Vec<String> = stuck
        .iter()
        .map(|n| format!("{n} (previous version: {})", dir.join(old_name(n)).display()))
        .collect();
    Error::Invalid(format!("{ROLLBACK_FAILED} ({cause}); not restored: {}", files.join(", ")))
}

/// Whether `e` says that an update failed and the installed copy could not be
/// put back as it was. The previous versions are then still on disk as
/// `<name>.old` and must be restored by hand.
pub fn rollback_failed(e: &Error) -> bool {
    matches!(e, Error::Invalid(m) if m.starts_with(ROLLBACK_FAILED))
}

/// Why one executable could not be swapped, and which files could not be put
/// back while undoing that single step.
struct Failure {
    cause: Error,
    stuck: Vec<String>,
}

/// `<name>` becomes `<name>.old` and the staged file becomes `<name>`. When
/// the second step fails the first is undone.
fn swap_one(
    staged: &Path,
    dir: &Path,
    name: &str,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> std::result::Result<(), Failure> {
    let (target, old) = (dir.join(name), dir.join(old_name(name)));
    rename(&target, &old)
        .map_err(|e| Failure { cause: io_error("move aside", &target, e), stuck: Vec::new() })?;
    if let Err(e) = rename(&staged.join(name), &target) {
        let stuck = match rename(&old, &target) {
            Ok(()) => Vec::new(),
            Err(_) => vec![name.to_string()],
        };
        return Err(Failure { cause: io_error("replace", &target, e), stuck });
    }
    Ok(())
}

/// Reverts a completed `swap_one`: the new file goes back to the staging
/// folder (or is removed) and the old one is restored. Returns `name` when
/// the old one could not be restored.
fn undo_one(
    staged: &Path,
    dir: &Path,
    name: &str,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Option<String> {
    let (target, old) = (dir.join(name), dir.join(old_name(name)));
    if rename(&target, &staged.join(name)).is_err() {
        let _ = std::fs::remove_file(&target);
    }
    rename(&old, &target).err().map(|_| name.to_string())
}

/// Removes the previous versions (`<name>.old`) and the staging folder left
/// by an update, and probe files left by `install_kind`. Errors are ignored:
/// a file still in use is retried at the next start. An `.old` whose
/// executable is missing is the only copy left of it (a failed rollback): it
/// stays, and so does the staging folder.
pub fn clean_up(dir: &Path) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let is_probe = entry.file_name().to_str().is_some_and(|n| n.starts_with(PROBE));
        if is_probe && entry.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let mut orphan = false;
    for name in executables() {
        if dir.join(&name).exists() {
            let _ = std::fs::remove_file(dir.join(old_name(&name)));
        } else if dir.join(old_name(&name)).exists() {
            orphan = true;
        }
    }
    if !orphan {
        remove_staging(dir);
    }
}

fn remove_staging(dir: &Path) {
    let staging = dir.join(STAGING);
    match std::fs::symlink_metadata(&staging) {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir_all(&staging);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(&staging);
        }
        Err(_) => {}
    }
}

/// Downloads `release`, verifies and unpacks it in `dir/.pm-update` and swaps
/// it in. The staging folder is removed on success and on every failure; on
/// failure the installed files are untouched.
pub fn update(
    fetch: &dyn Fetch,
    release: &Release,
    dir: &Path,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<()> {
    update_with(fetch, release, dir, progress, cancel, &mut |from, to| std::fs::rename(from, to))
}

fn update_with(
    fetch: &dyn Fetch,
    release: &Release,
    dir: &Path,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let staging = dir.join(STAGING);
    remove_staging(dir);
    if std::fs::symlink_metadata(&staging).is_ok() {
        return Err(Error::Invalid(format!("cannot remove the old {}", staging.display())));
    }
    std::fs::create_dir(&staging).map_err(|e| io_error("create", &staging, e))?;
    let result = stage_and_swap(fetch, release, dir, &staging, progress, cancel, rename);
    // After a failed rollback the staging folder holds the only other copy of
    // the new files: it stays.
    if !matches!(&result, Err(e) if rollback_failed(e)) {
        remove_staging(dir);
    }
    result
}

fn stage_and_swap(
    fetch: &dyn Fetch,
    release: &Release,
    dir: &Path,
    staging: &Path,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let archive = download(fetch, release, staging, progress, cancel)?;
    let unpacked = staging.join("unpacked");
    std::fs::create_dir(&unpacked).map_err(|e| io_error("create", &unpacked, e))?;
    unpack(&archive, &unpacked)?;
    // The last point where giving up is free.
    cancel.check()?;
    swap_with(&unpacked, dir, rename)
}

/// Starts `dir/partition-manager[.exe]` with `args`, detached: the new
/// process outlives this one and is not waited for.
pub fn restart(dir: &Path, args: &[OsString]) -> Result<()> {
    let exe = dir.join(exe_name("partition-manager", cfg!(windows)));
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn().map(drop).map_err(|e| Error::Platform {
        code: e.raw_os_error().unwrap_or(0) as u32,
        msg: format!("cannot start {}: {e}", exe.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MapFetch;
    use pm_core::NullProgress;
    use std::fs;
    use std::io;

    const WINDOWS: bool = cfg!(windows);
    const EXE_GUI: &str = if WINDOWS { "partition-manager.exe" } else { "partition-manager" };
    const EXE_PM: &str = if WINDOWS { "pm.exe" } else { "pm" };

    fn read(dir: &tempfile::TempDir, name: &str) -> String {
        fs::read_to_string(dir.path().join(name)).unwrap()
    }

    fn put(dir: &Path, name: &str, text: &str) {
        fs::write(dir.join(name), text).unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// An install folder with the given executables (and README.md) in it.
    fn installed(with_pm: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), EXE_GUI, "old gui");
        if with_pm {
            put(dir.path(), EXE_PM, "old pm");
        }
        put(dir.path(), "README.md", "old readme");
        dir
    }

    /// A staging folder holding all five files with "new ..." contents.
    fn staged_all() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), EXE_GUI, "new gui");
        put(dir.path(), EXE_PM, "new pm");
        put(dir.path(), "README.md", "new readme");
        put(dir.path(), "LICENSE", "new license");
        put(dir.path(), "CHANGELOG.md", "new changelog");
        dir
    }

    #[test]
    fn swap_replaces_only_what_is_installed() {
        let dir = installed(false);
        let staged = staged_all();
        swap(staged.path(), dir.path()).unwrap();
        assert_eq!(read(&dir, EXE_GUI), "new gui");
        assert_eq!(read(&dir, "README.md"), "new readme");
        assert!(!dir.path().join(EXE_PM).exists() && !dir.path().join("LICENSE").exists());
        assert!(!dir.path().join("CHANGELOG.md").exists());
        assert_eq!(read(&dir, &format!("{EXE_GUI}.old")), "old gui");
    }

    #[test]
    fn swap_replaces_both_executables_when_both_are_installed() {
        let dir = installed(true);
        let staged = staged_all();
        swap(staged.path(), dir.path()).unwrap();
        assert_eq!(read(&dir, EXE_GUI), "new gui");
        assert_eq!(read(&dir, EXE_PM), "new pm");
        assert_eq!(read(&dir, &format!("{EXE_GUI}.old")), "old gui");
        assert_eq!(read(&dir, &format!("{EXE_PM}.old")), "old pm");
    }

    #[test]
    fn a_failing_second_executable_rolls_the_first_back() {
        let dir = installed(true);
        let staged = staged_all();
        fs::remove_file(staged.path().join(EXE_PM)).unwrap();
        assert!(swap(staged.path(), dir.path()).is_err());
        assert_eq!(read(&dir, EXE_GUI), "old gui");
        assert_eq!(read(&dir, EXE_PM), "old pm");
        assert_eq!(read(&dir, "README.md"), "old readme");
        assert_eq!(names(dir.path()), {
            let mut v = vec![EXE_GUI.to_string(), EXE_PM.to_string(), "README.md".to_string()];
            v.sort();
            v
        });
    }

    #[test]
    fn a_rename_failing_at_any_step_rolls_everything_back() {
        // The four renames of a full swap: gui -> .old, staged -> gui,
        // pm -> .old, staged -> pm. Whichever fails, nothing changes.
        for failing in 1..=4 {
            let dir = installed(true);
            let staged = staged_all();
            let mut calls = 0;
            let mut rename = |from: &Path, to: &Path| {
                calls += 1;
                if calls == failing {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
                } else {
                    fs::rename(from, to)
                }
            };
            let err = swap_with(staged.path(), dir.path(), &mut rename);
            assert!(err.is_err(), "step {failing}");
            assert_eq!(read(&dir, EXE_GUI), "old gui", "step {failing}");
            assert_eq!(read(&dir, EXE_PM), "old pm", "step {failing}");
            assert_eq!(read(&dir, "README.md"), "old readme", "step {failing}");
            assert_eq!(names(dir.path()).len(), 3, "step {failing}: {:?}", names(dir.path()));
        }
    }

    #[test]
    fn an_undeletable_stale_old_file_fails_before_anything_is_renamed() {
        let dir = installed(true);
        let staged = staged_all();
        // A non-empty folder named like the stale file: `remove_file` cannot
        // remove it on any platform.
        let stale = dir.path().join(format!("{EXE_PM}.old"));
        fs::create_dir(&stale).unwrap();
        put(&stale, "keep", "x");
        assert!(swap(staged.path(), dir.path()).is_err());
        assert_eq!(read(&dir, EXE_GUI), "old gui");
        assert_eq!(read(&dir, EXE_PM), "old pm");
        assert!(!dir.path().join(format!("{EXE_GUI}.old")).exists());
        assert!(stale.join("keep").exists());
    }

    #[cfg(windows)]
    #[test]
    fn swap_renames_an_executable_that_is_held_open() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = installed(true);
        let staged = staged_all();
        // Like a running image: readable, not writable, but renamable
        // (FILE_SHARE_READ | FILE_SHARE_DELETE).
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(1 | 4)
            .open(dir.path().join(EXE_GUI))
            .unwrap();
        swap(staged.path(), dir.path()).unwrap();
        drop(held);
        assert_eq!(read(&dir, EXE_GUI), "new gui");
        assert_eq!(read(&dir, &format!("{EXE_GUI}.old")), "old gui");
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_stale_old_file_fails_the_swap_cleanly() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = installed(true);
        let staged = staged_all();
        put(dir.path(), &format!("{EXE_GUI}.old"), "ancient");
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(dir.path().join(format!("{EXE_GUI}.old")))
            .unwrap();
        assert!(swap(staged.path(), dir.path()).is_err());
        drop(locked);
        assert_eq!(read(&dir, EXE_GUI), "old gui");
        assert_eq!(read(&dir, EXE_PM), "old pm");
        assert_eq!(read(&dir, &format!("{EXE_GUI}.old")), "ancient");
        assert!(!dir.path().join(format!("{EXE_PM}.old")).exists());
    }

    /// The fake release 0.3.0 for this platform.
    fn release_fixture(good_sum: bool) -> (MapFetch, Release) {
        crate::testing::release_fixture("0.3.0", good_sum)
    }

    #[test]
    fn update_end_to_end_with_fixture_release() {
        let dir = installed(true);
        let (fetch, release) = release_fixture(true);
        update(&fetch, &release, dir.path(), &NullProgress, &CancelToken::default()).unwrap();
        assert_eq!(read(&dir, EXE_GUI), format!("bin:{EXE_GUI}"));
        assert_eq!(read(&dir, EXE_PM), format!("bin:{EXE_PM}"));
        assert_eq!(read(&dir, "README.md"), "bin:README.md");
        assert!(!dir.path().join("LICENSE").exists());
        assert_eq!(read(&dir, &format!("{EXE_GUI}.old")), "old gui");
        assert!(!dir.path().join(STAGING).exists());
    }

    #[test]
    fn stale_old_files_and_staging_are_replaced() {
        let dir = installed(true);
        put(dir.path(), &format!("{EXE_GUI}.old"), "ancient");
        fs::create_dir(dir.path().join(STAGING)).unwrap();
        put(&dir.path().join(STAGING), "junk", "x");
        let (fetch, release) = release_fixture(true);
        update(&fetch, &release, dir.path(), &NullProgress, &CancelToken::default()).unwrap();
        assert_eq!(read(&dir, EXE_GUI), format!("bin:{EXE_GUI}"));
        assert_eq!(read(&dir, &format!("{EXE_GUI}.old")), "old gui");
        assert!(!dir.path().join(STAGING).exists());
    }

    #[test]
    fn failed_update_leaves_the_install_untouched_and_no_staging() {
        let dir = installed(true);
        let (fetch, release) = release_fixture(false);
        let err = update(&fetch, &release, dir.path(), &NullProgress, &CancelToken::default());
        assert!(err.is_err());
        assert_eq!(read(&dir, EXE_GUI), "old gui");
        assert_eq!(read(&dir, EXE_PM), "old pm");
        assert_eq!(names(dir.path()).len(), 3, "{:?}", names(dir.path()));
    }

    #[test]
    fn cancelled_update_leaves_the_install_untouched_and_no_staging() {
        let dir = installed(true);
        let (fetch, release) = release_fixture(true);
        let cancel = CancelToken::default();
        cancel.cancel();
        let err = update(&fetch, &release, dir.path(), &NullProgress, &cancel);
        assert!(matches!(err, Err(Error::Cancelled)), "{err:?}");
        assert_eq!(read(&dir, EXE_GUI), "old gui");
        assert_eq!(names(dir.path()).len(), 3, "{:?}", names(dir.path()));
    }

    #[test]
    fn clean_up_removes_old_files_and_staging() {
        let dir = installed(true);
        put(dir.path(), &format!("{EXE_GUI}.old"), "old");
        put(dir.path(), &format!("{EXE_PM}.old"), "old");
        put(dir.path(), "notes.old", "mine");
        fs::create_dir_all(dir.path().join(STAGING).join("unpacked")).unwrap();
        put(&dir.path().join(STAGING), "a.zip", "x");
        clean_up(dir.path());
        assert_eq!(names(dir.path()), {
            let mut v = vec![
                EXE_GUI.to_string(),
                EXE_PM.to_string(),
                "README.md".to_string(),
                "notes.old".to_string(),
            ];
            v.sort();
            v
        });
        // Nothing to clean, or no such folder: no panic.
        clean_up(dir.path());
        clean_up(&dir.path().join("missing"));
    }

    #[test]
    fn install_kind_reports_an_unwritable_folder() {
        let dir = tempfile::tempdir().unwrap();
        match install_kind(&dir.path().join("missing")) {
            InstallKind::Unavailable(reason) => assert!(!reason.is_empty() && !reason.contains('\n')),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn install_kind_of_a_writable_folder_is_portable_and_leaves_no_probe() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(install_kind(dir.path()), InstallKind::Portable);
        assert!(names(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn install_kind_reports_a_read_only_folder() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        // root can write anywhere; the test is only meaningful otherwise.
        let can_write = fs::File::create(dir.path().join("probe")).is_ok();
        let kind = install_kind(dir.path());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        if !can_write {
            assert!(matches!(kind, InstallKind::Unavailable(_)), "{kind:?}");
        }
    }

    #[test]
    fn install_kind_is_not_fooled_by_a_stale_probe_file() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), &format!(".pm-update-probe-{}", std::process::id()), "left by a crash");
        assert_eq!(install_kind(dir.path()), InstallKind::Portable);
        assert!(names(dir.path()).is_empty(), "{:?}", names(dir.path()));
    }

    #[test]
    fn clean_up_removes_stale_probe_files() {
        let dir = installed(true);
        put(dir.path(), ".pm-update-probe-123", "");
        put(dir.path(), ".pm-update-probe-98765", "");
        put(dir.path(), "pm-update-probe-1", "mine");
        fs::create_dir(dir.path().join(".pm-update-probe-dir")).unwrap();
        clean_up(dir.path());
        assert_eq!(names(dir.path()), {
            let mut v = vec![
                EXE_GUI.to_string(),
                EXE_PM.to_string(),
                "README.md".to_string(),
                "pm-update-probe-1".to_string(),
                ".pm-update-probe-dir".to_string(),
            ];
            v.sort();
            v
        });
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_only_strips_the_drive_letter_form() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\Program Files\pm")), PathBuf::from(r"C:\Program Files\pm"));
        assert_eq!(strip_verbatim(Path::new(r"\\?\D:\")), PathBuf::from(r"D:\"));
        for keep in [
            r"\\?\UNC\server\share\pm",
            r"\\?\Volume{0e9bd4a6-62a4-4b6c-9f4e-1a2b3c4d5e6f}\pm",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\pm",
            r"\\.\C:\pm",
            r"C:\pm",
            r"\\server\share\pm",
        ] {
            assert_eq!(strip_verbatim(Path::new(keep)), PathBuf::from(keep), "{keep}");
        }
    }

    #[test]
    fn install_dir_is_a_plain_existing_folder() {
        let dir = install_dir().unwrap();
        assert!(dir.is_dir());
        assert!(!dir.to_string_lossy().starts_with(r"\\?\"), "{}", dir.display());
    }

    /// Runs a full swap of two installed executables where the renames whose
    /// 1-based numbers are in `failing` fail.
    fn swap_failing(failing: &[usize]) -> (tempfile::TempDir, Result<()>) {
        let dir = installed(true);
        let staged = staged_all();
        let mut calls = 0;
        let mut rename = |from: &Path, to: &Path| {
            calls += 1;
            if failing.contains(&calls) {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
            } else {
                fs::rename(from, to)
            }
        };
        let r = swap_with(staged.path(), dir.path(), &mut rename);
        (dir, r)
    }

    #[test]
    fn a_failing_restore_is_reported_with_the_surviving_old_files() {
        // Renames: 1 gui->old, 2 staged->gui, 3 pm->old, 4 staged->pm; then
        // the rollback: 5 pm.old->pm, 6 gui->staged, 7 gui.old->gui.
        // (failing renames, executables that stay missing)
        let cases: [(&[usize], &[&str]); 4] = [
            (&[4, 5], &[EXE_PM]),
            (&[4, 7], &[EXE_GUI]),
            (&[4, 5, 7], &[EXE_GUI, EXE_PM]),
            (&[2, 3], &[EXE_GUI]),
        ];
        for (failing, missing) in cases {
            let (dir, r) = swap_failing(failing);
            let e = r.unwrap_err();
            let text = e.to_string();
            assert!(rollback_failed(&e), "{failing:?}: {text}");
            assert!(
                e.to_string().contains("the update failed and could not be rolled back"),
                "{failing:?}: {text}"
            );
            for name in missing {
                let old = dir.path().join(format!("{name}.old"));
                assert!(old.exists(), "{failing:?}: {name}");
                assert!(text.contains(&old.display().to_string()), "{failing:?}: {text}");
                assert!(!dir.path().join(name).exists(), "{failing:?}: {name}");
            }
        }
    }

    #[test]
    fn a_failing_move_back_of_the_new_file_still_restores_the_old_one() {
        // Rename 6 (new gui -> staged) fails: the new file is removed instead.
        let (dir, r) = swap_failing(&[4, 6]);
        let e = r.unwrap_err();
        assert!(!rollback_failed(&e), "{e}");
        assert_eq!(read(&dir, EXE_GUI), "old gui");
        assert_eq!(read(&dir, EXE_PM), "old pm");
        assert_eq!(names(dir.path()).len(), 3, "{:?}", names(dir.path()));
    }

    #[test]
    fn an_ordinary_failure_is_not_a_failed_rollback() {
        assert!(!rollback_failed(&Error::Invalid("cannot replace x".into())));
        assert!(!rollback_failed(&Error::Cancelled));
    }

    #[test]
    fn update_keeps_the_staging_folder_when_the_rollback_failed() {
        let dir = installed(true);
        let (fetch, release) = release_fixture(true);
        let mut calls = 0;
        let mut rename = |from: &Path, to: &Path| {
            calls += 1;
            if calls == 2 || calls == 3 {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
            } else {
                fs::rename(from, to)
            }
        };
        let e = update_with(&fetch, &release, dir.path(), &NullProgress, &CancelToken::default(), &mut rename)
            .unwrap_err();
        assert!(rollback_failed(&e), "{e}");
        assert!(dir.path().join(STAGING).is_dir());
        assert!(dir.path().join(format!("{EXE_GUI}.old")).exists());
    }

    #[test]
    fn clean_up_keeps_an_old_file_whose_executable_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), &format!("{EXE_GUI}.old"), "only copy");
        put(dir.path(), EXE_PM, "pm");
        put(dir.path(), &format!("{EXE_PM}.old"), "old pm");
        fs::create_dir(dir.path().join(STAGING)).unwrap();
        clean_up(dir.path());
        assert_eq!(read(&dir, &format!("{EXE_GUI}.old")), "only copy");
        assert!(!dir.path().join(format!("{EXE_PM}.old")).exists());
        assert!(dir.path().join(STAGING).is_dir());
    }

    #[test]
    fn swap_without_the_gui_executable_fails_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        put(dir.path(), EXE_PM, "old pm");
        let staged = staged_all();
        let e = swap(staged.path(), dir.path()).unwrap_err();
        assert!(e.to_string().contains(EXE_GUI), "{e}");
        assert!(!rollback_failed(&e));
        assert_eq!(names(dir.path()), vec![EXE_PM.to_string()]);
        assert_eq!(read(&dir, EXE_PM), "old pm");
    }

    #[test]
    fn restart_of_a_missing_executable_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = restart(dir.path(), &[]).unwrap_err();
        assert!(err.to_string().contains(EXE_GUI), "{err}");
    }
}
