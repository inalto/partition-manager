//! The application log: `env_logger` writing every record to stderr (debug builds have a
//! console; release builds usually do not) and appending it to
//! `%LOCALAPPDATA%\PartitionManager\partition-manager.log`. The level comes from `RUST_LOG`
//! and defaults to `info`.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

/// File name of the log inside [`log_dir`].
pub const LOG_FILE: &str = "partition-manager.log";

/// `%LOCALAPPDATA%\PartitionManager` (`None` when `LOCALAPPDATA` is not set).
pub fn log_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(base).join("PartitionManager"))
}

/// The log file: [`log_dir`]`\partition-manager.log`.
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
    fn log_file_is_under_local_app_data() {
        if let Some(base) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            assert_eq!(
                log_file().unwrap(),
                PathBuf::from(base).join("PartitionManager").join("partition-manager.log")
            );
        }
    }
}
