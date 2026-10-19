//! Desktop-shell helpers: opening a folder in the system file manager.

use pm_core::{Error, Result};
use std::path::Path;
use std::process::Command;

/// The file-manager command for this OS: `explorer.exe` on Windows, `open` on macOS,
/// `xdg-open` elsewhere.
pub fn file_manager_command() -> &'static str {
    if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    }
}

/// Open `path` in the system file manager. The file manager is started detached: this never
/// waits for it. `Error::NotFound` when `path` does not exist, `Error::Platform` when the
/// file manager cannot be started.
pub fn open_in_file_manager(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(Error::NotFound(format!("{} does not exist", path.display())));
    }
    let cmd = file_manager_command();
    Command::new(cmd).arg(path).spawn().map(drop).map_err(|e| Error::Platform {
        code: e.raw_os_error().map_or(0, |c| c as u32),
        msg: format!("cannot start {cmd} for {}: {e}", path.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_path_is_not_found_and_nothing_is_started() {
        let p = Path::new("this/path/does/not/exist/pm-shell-test");
        assert!(matches!(open_in_file_manager(p), Err(Error::NotFound(_))));
    }

    #[test]
    fn command_per_os() {
        let c = file_manager_command();
        if cfg!(windows) {
            assert_eq!(c, "explorer.exe");
        } else {
            assert!(c == "open" || c == "xdg-open");
        }
    }
}
