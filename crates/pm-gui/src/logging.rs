//! The application log: `env_logger` writing every record to stderr (debug builds have a
//! console; release builds usually do not) and appending it to `partition-manager.log` in
//! [`log_dir`]: `%LOCALAPPDATA%\PartitionManager` on Windows, `~/Library/Logs/PartitionManager` on
//! macOS and `$XDG_STATE_HOME/partition-manager` (default `~/.local/state/partition-manager`)
//! elsewhere. The level comes from `RUST_LOG` and defaults to `info`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

/// File name of the log inside [`log_dir`].
pub const LOG_FILE: &str = "partition-manager.log";

/// The log folder of this OS (see the module docs); `None` when the environment variable it
/// derives from is not set.
pub fn log_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    log_dir_for(
        if cfg!(windows) {
            LogOs::Windows
        } else if cfg!(target_os = "macos") {
            LogOs::MacOs
        } else {
            LogOs::Other
        },
        var("LOCALAPPDATA").as_deref(),
        var("XDG_STATE_HOME").as_deref(),
        var("HOME").as_deref(),
    )
}

/// The flavour of log location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogOs {
    Windows,
    MacOs,
    Other,
}

/// Pure resolution of the log folder from the relevant environment values.
pub fn log_dir_for(
    os: LogOs,
    local_app_data: Option<&std::ffi::OsStr>,
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    match os {
        LogOs::Windows => Some(PathBuf::from(local_app_data?).join("PartitionManager")),
        LogOs::MacOs => Some(PathBuf::from(home?).join("Library").join("Logs").join("PartitionManager")),
        LogOs::Other => match xdg_state_home {
            // the XDG spec only accepts absolute paths here
            Some(x) if std::path::Path::new(x).is_absolute() => Some(PathBuf::from(x).join("partition-manager")),
            _ => Some(PathBuf::from(home?).join(".local").join("state").join("partition-manager")),
        },
    }
}

/// The log file: `partition-manager.log` inside [`log_dir`].
pub fn log_file() -> Option<PathBuf> {
    log_dir().map(|d| d.join(LOG_FILE))
}

/// Copies everything written to it to stderr and, when there is one, to a file. Errors of
/// either side are ignored (a GUI-subsystem process may have no stderr), so logging never
/// fails the application.
pub struct Tee {
    file: Option<File>,
}

impl Tee {
    pub fn new(file: Option<File>) -> Self {
        Tee { file }
    }
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        if let Some(f) = &mut self.file {
            let _ = f.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(f) = &mut self.file {
            let _ = f.flush();
        }
        Ok(())
    }
}

/// Open (create, append) the log file, creating its folder.
fn open_log() -> Option<File> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    OpenOptions::new().create(true).append(true).open(dir.join(LOG_FILE)).ok()
}

/// Install the logger (once; later calls do nothing).
pub fn init() {
    let tee = Tee::new(open_log());
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(tee)))
        .try_init();
    log::info!(
        "Partition Manager {} started; log file: {}",
        env!("CARGO_PKG_VERSION"),
        log_file().map_or_else(|| "(none)".into(), |p| p.display().to_string())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tee_appends_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        std::fs::write(&path, b"old\n").unwrap();
        let file = OpenOptions::new().append(true).open(&path).unwrap();
        let mut tee = Tee::new(Some(file));
        tee.write_all(b"line one\n").unwrap();
        tee.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\nline one\n");
        // without a file it still accepts everything
        Tee::new(None).write_all(b"ignored\n").unwrap();
    }

    #[test]
    fn log_file_is_under_the_os_log_dir() {
        if let Some(dir) = log_dir() {
            assert_eq!(log_file().unwrap(), dir.join("partition-manager.log"));
        }
    }

    #[test]
    fn log_dir_per_os() {
        use std::ffi::OsStr;
        let none: Option<&OsStr> = None;
        // absolute on every host OS
        let abs = std::env::temp_dir();
        let a = Some(abs.as_os_str());
        assert_eq!(
            log_dir_for(LogOs::Windows, a, none, none),
            Some(abs.join("PartitionManager"))
        );
        assert_eq!(log_dir_for(LogOs::Windows, none, a, a), None);
        assert_eq!(
            log_dir_for(LogOs::MacOs, none, none, a),
            Some(abs.join("Library").join("Logs").join("PartitionManager"))
        );
        assert_eq!(log_dir_for(LogOs::MacOs, none, none, none), None);
        assert_eq!(
            log_dir_for(LogOs::Other, none, a, Some(OsStr::new("home"))),
            Some(abs.join("partition-manager"))
        );
        let under_home = Some(PathBuf::from("home").join(".local").join("state").join("partition-manager"));
        assert_eq!(log_dir_for(LogOs::Other, none, none, Some(OsStr::new("home"))), under_home);
        // a relative XDG_STATE_HOME is ignored
        assert_eq!(
            log_dir_for(LogOs::Other, none, Some(OsStr::new("rel")), Some(OsStr::new("home"))),
            under_home
        );
        assert_eq!(log_dir_for(LogOs::Other, none, none, none), None);
    }
}
