//! Running system tools (`mkfs.*`, `smartctl`, `resize2fs`, `diskutil`, ...) without a shell.
//!
//! Tools are looked up on `PATH` and then in the system directories (`/sbin`, `/usr/sbin`, ...)
//! that `PATH` often lacks for a normal user. A tool that is not installed gives
//! `Error::Unsupported("<tool> is not installed")`. Arguments are passed as an argument vector,
//! never through a shell; stdin is `/dev/null` so a tool can never wait for an answer, and
//! `LC_ALL=C` keeps messages in English for logs and error texts.

use pm_core::{Error, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// System directories: searched after `PATH` normally, and first when running as root.
const SYSTEM_DIRS: [&str; 6] = ["/sbin", "/usr/sbin", "/bin", "/usr/bin", "/usr/local/sbin", "/usr/local/bin"];

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Owned by root and writable by nobody else (no group / world write bit).
fn root_controlled(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0)
}

/// Whether root may run `bin` found in the `PATH` directory `dir`: both must be controlled by
/// root alone, otherwise another user could plant a `mkfs.vfat` that root then runs.
pub fn trusted_for_root(dir: &Path, bin: &Path) -> bool {
    root_controlled(dir) && root_controlled(bin)
}

/// [`find_tool`] over explicit `PATH` directories. As root the system directories come first
/// and a `PATH` match is accepted only when [`trusted_for_root`] (a `PATH` kept through
/// `sudo -E`, `su -m` or a distro without `secure_path` cannot redirect root to a user's
/// binary); otherwise `PATH` comes first.
pub fn find_tool_in(tool: &str, path_dirs: &[PathBuf], as_root: bool) -> Option<PathBuf> {
    if tool.contains('/') {
        let p = PathBuf::from(tool);
        return (is_executable(&p) && (!as_root || p.parent().is_some_and(|d| trusted_for_root(d, &p)))).then_some(p);
    }
    let system = SYSTEM_DIRS.iter().map(PathBuf::from).map(|d| d.join(tool)).find(|p| is_executable(p));
    let from_path = || {
        path_dirs
            .iter()
            .filter(|d| d.is_absolute())
            .map(|d| (d, d.join(tool)))
            .find(|(d, p)| is_executable(p) && (!as_root || trusted_for_root(d, p)))
            .map(|(_, p)| p)
    };
    if as_root {
        system.or_else(from_path)
    } else {
        from_path().or(system)
    }
}

/// Full path of `tool`: a name containing `/` is taken as is, otherwise the first executable
/// match on `PATH`, then in `/sbin`, `/usr/sbin` and the other system directories — as root,
/// the system directories first and only root-controlled `PATH` entries (see
/// [`find_tool_in`]).
pub fn find_tool(tool: &str) -> Option<PathBuf> {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    find_tool_in(tool, &path_dirs, super::is_root())
}

/// Whether `tool` is installed (see [`find_tool`]).
pub fn has_tool(tool: &str) -> bool {
    find_tool(tool).is_some()
}

/// Run `tool` with `args` and return its output whatever the exit status.
/// `Error::Unsupported` when the tool is not installed, `Error::Platform` when it cannot be
/// started.
pub fn output<S: AsRef<OsStr>>(tool: &str, args: &[S]) -> Result<Output> {
    let exe = find_tool(tool).ok_or_else(|| Error::Unsupported(format!("{tool} is not installed")))?;
    log::debug!(
        "running {} {}",
        exe.display(),
        args.iter().map(|a| a.as_ref().to_string_lossy()).collect::<Vec<_>>().join(" ")
    );
    Command::new(&exe)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| super::os_error(&e, &format!("cannot run {}", exe.display())))
}

/// The text a failed tool left: stderr, or stdout when stderr is empty.
pub fn failure_text(out: &Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr);
    let text = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { err };
    text.trim().to_string()
}

/// Run `tool` like [`output`]; a non-zero exit becomes `Error::Platform { code: exit status,
/// msg: "<tool> failed: <stderr>" }` (code `u32::MAX` when killed by a signal).
pub fn run<S: AsRef<OsStr>>(tool: &str, args: &[S]) -> Result<Output> {
    let out = output(tool, args)?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(Error::Platform {
            code: out.status.code().map_or(u32::MAX, |c| c as u32),
            msg: format!("{tool} failed: {}", failure_text(&out)),
        })
    }
}

/// [`run`], returning stdout followed by stderr as text.
pub fn run_text<S: AsRef<OsStr>>(tool: &str, args: &[S]) -> Result<String> {
    let out = run(tool, args)?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tools_are_unsupported() {
        assert!(find_tool("pm-no-such-tool-xyz").is_none());
        match run("pm-no-such-tool-xyz", &["a"]) {
            Err(Error::Unsupported(m)) => assert_eq!(m, "pm-no-such-tool-xyz is not installed"),
            other => panic!("{other:?}"),
        }
        assert!(find_tool("/nonexistent/sh").is_none());
    }

    #[test]
    fn root_prefers_system_dirs_and_rejects_writable_path_entries() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().to_path_buf();
        for name in ["sh", "pm-only-here"] {
            let p = dir.join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // a group/world-writable directory is never trusted, whoever owns it
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let path = vec![dir.clone()];
        // as a normal user PATH wins
        assert_eq!(find_tool_in("sh", &path, false), Some(dir.join("sh")));
        assert_eq!(find_tool_in("pm-only-here", &path, false), Some(dir.join("pm-only-here")));
        // as root the system copy wins and the untrusted PATH entry is ignored
        let sys_sh = find_tool_in("sh", &path, true).unwrap();
        assert!(SYSTEM_DIRS.iter().any(|d| sys_sh.starts_with(d)), "{}", sys_sh.display());
        assert_eq!(find_tool_in("pm-only-here", &path, true), None);
        assert_eq!(find_tool_in(&dir.join("pm-only-here").to_string_lossy(), &[], true), None);
        assert!(!trusted_for_root(&dir, &dir.join("sh")));
        // root-owned, non-writable system locations are trusted
        let bin = Path::new("/bin");
        if let Some(sh) = ["/bin/sh", "/usr/bin/sh"].iter().map(Path::new).find(|p| p.exists()) {
            assert!(trusted_for_root(sh.parent().unwrap_or(bin), sh));
        }
    }

    #[test]
    fn runs_without_a_shell_and_reports_exit_codes() {
        assert!(has_tool("sh"));
        // the argument is passed verbatim: no globbing, no word splitting
        let out = run("echo", &["a *  b"]).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "a *  b\n");
        match run("sh", &["-c", "echo oops >&2; exit 3"]) {
            Err(Error::Platform { code, msg }) => {
                assert_eq!(code, 3);
                assert_eq!(msg, "sh failed: oops");
            }
            other => panic!("{other:?}"),
        }
        // output() keeps the status for tools with meaningful non-zero exits (smartctl)
        let out = output("sh", &["-c", "echo data; exit 4"]).unwrap();
        assert_eq!(out.status.code(), Some(4));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "data\n");
        // stdin is /dev/null: a prompt reads EOF instead of hanging
        let out = run("sh", &["-c", "read x; echo \"[$x]\""]);
        assert!(out.is_err() || String::from_utf8_lossy(&out.unwrap().stdout) == "[]\n");
        assert_eq!(run_text("sh", &["-c", "echo out; echo err >&2"]).unwrap(), "out\nerr\n");
    }
}
