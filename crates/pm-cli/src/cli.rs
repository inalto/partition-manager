//! Command line definition (clap) and the CLI error type with its exit-code mapping.

use clap::{Args, Parser, Subcommand};
use std::fmt;
use std::path::PathBuf;

/// Exit code for success.
pub const EXIT_OK: i32 = 0;
/// Exit code for usage errors and invalid arguments.
pub const EXIT_USAGE: i32 = 1;
/// Exit code for platform / runtime errors.
pub const EXIT_RUNTIME: i32 = 2;

#[derive(Parser, Debug)]
#[command(name = "pm", version, about = "Partition Manager command line", propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List disks and their partitions
    List {
        /// Print the disks as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show the partition table of a disk, volume or image file
    Table {
        /// `disk:N`, a drive letter (`D:`), a `\\?\Volume{..}\` path, or an image file (`file:PATH` or PATH)
        device: String,
        /// Print the table as JSON
        #[arg(long)]
        json: bool,
    },
    /// Mount a volume at a drive letter or an empty folder
    Mount {
        /// Drive letter (`D:`) or `\\?\Volume{..}\` path
        volume: String,
        #[command(flatten)]
        at: MountTarget,
    },
    /// Remove a drive letter or folder mount point from a volume
    Unmount {
        /// Drive letter (`D:`) or `\\?\Volume{..}\` path
        volume: String,
        #[command(flatten)]
        at: MountTarget,
    },
    /// Print environment diagnostics (elevation, toolchain, disks seen)
    Doctor,
}

/// Exactly one of `--letter` / `--folder`.
#[derive(Args, Debug)]
#[command(group(clap::ArgGroup::new("target").required(true).multiple(false)))]
pub struct MountTarget {
    /// Drive letter, e.g. `E` or `E:`
    #[arg(long, group = "target", value_parser = parse_letter)]
    pub letter: Option<char>,
    /// Empty NTFS folder to use as the mount point
    #[arg(long, group = "target")]
    pub folder: Option<PathBuf>,
}

/// `E`, `e`, `E:` or `E:\` -> `'E'`.
pub fn parse_letter(s: &str) -> Result<char, String> {
    let t = s.strip_suffix('\\').unwrap_or(s);
    let t = t.strip_suffix(':').unwrap_or(t);
    let mut it = t.chars();
    match (it.next(), it.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Ok(c.to_ascii_uppercase()),
        _ => Err(format!("'{s}' is not a drive letter (expected A-Z)")),
    }
}

/// Failure of a command, mapped to the process exit code by [`CliError::exit_code`].
#[derive(Debug)]
pub enum CliError {
    /// Bad arguments (exit 1).
    Usage(String),
    /// Platform or runtime failure (exit 2).
    Runtime(pm_core::Error),
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => EXIT_USAGE,
            CliError::Runtime(_) => EXIT_RUNTIME,
        }
    }

    /// Whether this is an access-denied failure (elevation missing).
    fn is_access_denied(&self) -> bool {
        match self {
            CliError::Runtime(pm_core::Error::Platform { code, msg }) => {
                *code == 5 || msg.to_ascii_lowercase().contains("access denied")
            }
            CliError::Runtime(pm_core::Error::Io { source, .. }) => {
                source.kind() == std::io::ErrorKind::PermissionDenied
            }
            _ => false,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(m) => f.write_str(m),
            CliError::Runtime(e) => {
                write!(f, "{e}")?;
                if self.is_access_denied() {
                    write!(f, " (hint: run this command from an elevated prompt)")?;
                }
                Ok(())
            }
        }
    }
}

impl From<pm_core::Error> for CliError {
    fn from(e: pm_core::Error) -> Self {
        CliError::Runtime(e)
    }
}

/// Convert an argument-resolution error: `Invalid` / `NotFound` are the user's mistake (exit 1),
/// anything else is a runtime failure.
pub fn arg_error(e: pm_core::Error) -> CliError {
    match e {
        pm_core::Error::Invalid(m) | pm_core::Error::NotFound(m) => CliError::Usage(m),
        other => CliError::Runtime(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::Error;

    #[test]
    fn letters_parse() {
        assert_eq!(parse_letter("e"), Ok('E'));
        assert_eq!(parse_letter("E:"), Ok('E'));
        assert_eq!(parse_letter("z:\\"), Ok('Z'));
        assert!(parse_letter("1").is_err());
        assert!(parse_letter("EF").is_err());
        assert!(parse_letter("").is_err());
    }

    #[test]
    fn exit_codes_and_hint() {
        assert_eq!(CliError::Usage("x".into()).exit_code(), 1);
        let denied = CliError::Runtime(Error::Platform {
            code: 5,
            msg: "open x: Access denied".into(),
        });
        assert_eq!(denied.exit_code(), 2);
        assert!(denied.to_string().contains("elevated prompt"));
        let other = CliError::Runtime(Error::Platform {
            code: 2,
            msg: "Device not found".into(),
        });
        assert!(!other.to_string().contains("elevated"));
        assert!(matches!(arg_error(Error::Invalid("x".into())), CliError::Usage(_)));
        assert!(matches!(arg_error(Error::Cancelled), CliError::Runtime(_)));
    }
}
