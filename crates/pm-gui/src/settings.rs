//! The small settings file of the update check: `settings.json` in [`settings_dir`]
//! (`%APPDATA%\PartitionManager` on Windows, `~/Library/Application Support/PartitionManager`
//! on macOS, `$XDG_CONFIG_HOME/partition-manager` or `~/.config/partition-manager` elsewhere).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::logging::LogOs;

/// File name of the settings inside [`settings_dir`].
pub const SETTINGS_FILE: &str = "settings.json";

/// Seconds between two automatic update checks.
const CHECK_INTERVAL: u64 = 86_400;

/// A newer release found by an earlier check.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Found {
    pub version: String,
    pub page_url: String,
}

/// What the application remembers between runs about updates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Whether the automatic check is on.
    pub check_updates: bool,
    /// Unix time (seconds) of the last check.
    pub last_check: u64,
    /// A version the user chose to skip.
    pub skipped: Option<String>,
    /// The newer release the last check found.
    pub found: Option<Found>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { check_updates: true, last_check: 0, skipped: None, found: None }
    }
}

/// The settings folder of this OS; `None` when the environment it derives from is missing.
pub fn settings_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    settings_dir_for(
        if cfg!(windows) {
            LogOs::Windows
        } else if cfg!(target_os = "macos") {
            LogOs::MacOs
        } else {
            LogOs::Other
        },
        var("APPDATA").as_deref(),
        var("XDG_CONFIG_HOME").as_deref(),
        var("HOME").as_deref(),
    )
}

/// Pure resolution of the settings folder from the relevant environment values.
pub fn settings_dir_for(
    os: LogOs,
    app_data: Option<&OsStr>,
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    match os {
        LogOs::Windows => Some(PathBuf::from(app_data?).join("PartitionManager")),
        LogOs::MacOs => Some(PathBuf::from(home?).join("Library").join("Application Support").join("PartitionManager")),
        LogOs::Other => match xdg_config_home {
            // the XDG spec only accepts absolute paths here
            Some(x) if Path::new(x).is_absolute() => Some(PathBuf::from(x).join("partition-manager")),
            _ => Some(PathBuf::from(home?).join(".config").join("partition-manager")),
        },
    }
}

impl Settings {
    /// Read the settings; anything missing, unreadable or malformed gives the defaults.
    pub fn load(path: &Path) -> Settings {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write the settings, creating the folder; the file is replaced atomically.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        // write beside the target and rename over it, so an interruption never leaves a half file
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let result = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Whether an automatic check should run now (`now` is Unix seconds).
    pub fn check_due(&self, now: u64) -> bool {
        self.check_updates && (now < self.last_check || now - self.last_check >= CHECK_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_and_unknown_fields() {
        assert!(Settings::default().check_updates);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(SETTINGS_FILE);
        let s = Settings {
            check_updates: false,
            last_check: 1_700_000_000,
            skipped: Some("0.3.0".into()),
            found: Some(Found { version: "0.4.0".into(), page_url: "https://github.com/x/y/releases/tag/v0.4.0".into() }),
        };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        // no temporary file is left behind
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        // saving again replaces the file
        Settings::default().save(&path).unwrap();
        assert_eq!(Settings::load(&path), Settings::default());

        // unknown fields are ignored, missing ones take the defaults
        std::fs::write(&path, r#"{"last_check": 5, "future": [1, 2]}"#).unwrap();
        assert_eq!(Settings::load(&path), Settings { last_check: 5, ..Default::default() });

        // missing, empty, truncated and wrong-shaped files give the defaults
        assert_eq!(Settings::load(&dir.path().join("missing.json")), Settings::default());
        for bad in ["{", "", "[]", r#"{"check_updates": "yes"}"#] {
            std::fs::write(&path, bad).unwrap();
            assert_eq!(Settings::load(&path), Settings::default(), "content {bad:?}");
        }
        // a folder where the file should be is unreadable: defaults too
        assert_eq!(Settings::load(dir.path()), Settings::default());
    }

    #[test]
    fn folders_per_os() {
        let none: Option<&OsStr> = None;
        let abs = std::env::temp_dir();
        let a = Some(abs.as_os_str());
        assert_eq!(settings_dir_for(LogOs::Windows, a, none, none), Some(abs.join("PartitionManager")));
        assert_eq!(settings_dir_for(LogOs::Windows, none, a, a), None);
        assert_eq!(
            settings_dir_for(LogOs::MacOs, none, none, a),
            Some(abs.join("Library").join("Application Support").join("PartitionManager"))
        );
        assert_eq!(settings_dir_for(LogOs::MacOs, none, none, none), None);
        assert_eq!(
            settings_dir_for(LogOs::Other, none, a, Some(OsStr::new("home"))),
            Some(abs.join("partition-manager"))
        );
        let under_home = Some(PathBuf::from("home").join(".config").join("partition-manager"));
        assert_eq!(settings_dir_for(LogOs::Other, none, none, Some(OsStr::new("home"))), under_home);
        // a relative XDG_CONFIG_HOME is ignored
        assert_eq!(
            settings_dir_for(LogOs::Other, none, Some(OsStr::new("rel")), Some(OsStr::new("home"))),
            under_home
        );
        assert_eq!(settings_dir_for(LogOs::Other, none, none, none), None);
    }

    #[test]
    fn check_is_due_after_a_day_never_when_off_and_when_the_clock_went_back() {
        let s = Settings { last_check: 1_000_000, ..Default::default() };
        assert!(!s.check_due(1_000_000 + 86_399));
        assert!(s.check_due(1_000_000 + 86_400));
        assert!(s.check_due(999_999));
        assert!(!Settings { check_updates: false, ..s }.check_due(u64::MAX));
    }
}
