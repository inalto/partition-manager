//! Command line definition (clap) and the CLI error type with its exit-code mapping.

use crate::cmd_apply::ApplyArgs;
use crate::cmd_ops::OpCommand;
use clap::{Args, Parser, Subcommand};
use pm_image::ImageFormat;
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
    /// Use this raw image file as a disk instead of the physical disks (repeatable: the first
    /// is `disk:0`, the next `disk:1`, ...). Every command that takes a disk, and `list`,
    /// then works on the files; no administrator rights are needed. Mounting, formatting and
    /// everything else that needs a volume layer is not available on image files.
    #[arg(long = "disk-file", global = true, value_name = "PATH")]
    pub disk_files: Vec<PathBuf>,
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
    /// Copy one device onto another (disk, partition, volume or image file)
    ///
    /// Refuses to write onto the source, onto a partition or volume overlapping it, onto the
    /// running system disk, or onto a VMDK descriptor's extent file. Limitation: other ways
    /// of reaching the same storage (hard links, junctions, a file stored on the disk being
    /// copied, a differencing image's parent) are not detected.
    Clone {
        /// Source device: `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)
        src: String,
        /// Destination device; must be at least as large as the source
        dst: String,
        #[command(flatten)]
        mode: CopyModeArgs,
        /// Write the copy starting at this byte offset of the destination (e.g. `1M`)
        #[arg(long, default_value = "0", value_parser = size_arg)]
        dst_offset: u64,
    },
    /// Save a device to an image file (raw, VHD, VMDK or qcow2)
    ///
    /// The image is written to FILE.partial and renamed when complete; a failed run leaves an
    /// existing FILE untouched. Refuses to overwrite the source or a VMDK extent it is read
    /// from. Limitation: other ways of reaching the same storage (hard links, junctions, a
    /// differencing image's parent) are not detected.
    Image {
        /// Source device: `disk:N[:P]`, a drive letter, or an image file
        src: String,
        /// Image file to create (an existing file is overwritten)
        file: PathBuf,
        /// Image format; by default chosen from the file extension
        #[arg(long, value_parser = parse_format)]
        format: Option<ImageFormat>,
        #[command(flatten)]
        mode: CopyModeArgs,
    },
    /// Write an image (or any device) onto a device
    ///
    /// Refuses to write onto the source, onto a partition or volume overlapping it, onto the
    /// running system disk, or onto a VMDK descriptor's extent file. Limitation: other ways
    /// of reaching the same storage (hard links, junctions, a file stored on the disk being
    /// copied, a differencing image's parent) are not detected.
    Restore {
        /// Image file (`file:PATH[:P]` or PATH) or other source device
        file: String,
        /// Destination device; must be at least as large as the image
        dst: String,
        /// Write starting at this byte offset of the destination (e.g. `1M`)
        #[arg(long, default_value = "0", value_parser = size_arg)]
        dst_offset: u64,
    },
    /// Convert an image file to another format
    ///
    /// The output is written to OUTPUT.partial and renamed when complete. Refuses to overwrite
    /// the input or its VMDK extent file.
    Convert {
        /// Existing image file (any supported format)
        input: PathBuf,
        /// Image file to create (an existing file is overwritten)
        output: PathBuf,
        /// Target format; by default chosen from the output extension
        #[arg(long, value_parser = parse_format)]
        format: Option<ImageFormat>,
    },
    /// Copy a failing device with ddrescue's algorithm, keeping a resumable mapfile
    ///
    /// Short options follow GNU ddrescue: -b sector size, -c sectors per read, -K skip size,
    /// -r retry passes, -R reverse, -n no scrape, -N no trim.
    Rescue(RescueArgs),
    /// Identify the filesystem on a device
    Probe {
        /// `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)
        device: String,
        /// Look for the filesystem at this byte offset of the device
        #[arg(long, default_value = "0", value_parser = size_arg)]
        offset: u64,
    },
    /// List a directory of the filesystem on a device
    Ls {
        /// `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)
        device: String,
        /// Directory or file, `/`-separated (default: the root)
        path: Option<String>,
    },
    /// Write a file of the filesystem on a device to standard output
    Cat {
        /// `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)
        device: String,
        /// File path, `/`-separated
        path: String,
    },
    /// Copy a file or directory tree out of the filesystem on a device
    Extract {
        /// `disk:N[:P]`, a drive letter, or an image file (`file:PATH[:P]`)
        device: String,
        /// File or directory path, `/`-separated (`/` extracts everything)
        path: String,
        /// Existing or new directory to extract into
        dest: PathBuf,
    },
    /// Apply the operations listed in a JSON file (see `--emit-json` of the operation commands)
    ///
    /// The operations are validated against the current disks first. Nothing is written
    /// unless --yes is given; --dry-run validates and shows what would be done.
    Apply(ApplyArgs),
    #[command(flatten)]
    Op(OpCommand),
}

/// `--smart` / `--sector` (neither: smart when a filesystem is recognised, else sector).
#[derive(Args, Debug, Clone, Copy)]
pub struct CopyModeArgs {
    /// Copy only the blocks the filesystem marks used
    #[arg(long, conflicts_with = "sector")]
    pub smart: bool,
    /// Copy every sector
    #[arg(long)]
    pub sector: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyMode {
    Auto,
    Smart,
    Sector,
}

impl CopyModeArgs {
    pub fn mode(&self) -> CopyMode {
        match (self.smart, self.sector) {
            (true, _) => CopyMode::Smart,
            (_, true) => CopyMode::Sector,
            _ => CopyMode::Auto,
        }
    }
}

/// Arguments of `pm rescue`; the short flags are GNU ddrescue's.
#[derive(Args, Debug)]
pub struct RescueArgs {
    /// Source device: `disk:N[:P]`, a drive letter, or an image file
    pub src: String,
    /// Destination device; must be at least as large as the source
    pub dst: String,
    /// Mapfile (ddrescue format): resumes from it when it exists, kept up to date while copying
    pub mapfile: PathBuf,
    /// Sector size in bytes: the unit of trimming, scraping and retrying (default: the source's)
    #[arg(short = 'b', long, value_parser = size_arg)]
    pub sector_size: Option<u64>,
    /// Sectors per read in the copy phase (default: 128 KiB worth)
    #[arg(short = 'c', long, value_parser = clap::value_parser!(u32).range(1..))]
    pub cluster_size: Option<u32>,
    /// Initial skip after a read error (doubles on further errors)
    #[arg(short = 'K', long, value_parser = size_arg)]
    pub skip_size: Option<u64>,
    /// Retry passes over bad sectors
    #[arg(short = 'r', long, default_value_t = 0)]
    pub retries: u32,
    /// Copy from the end of the source towards the start
    #[arg(short = 'R', long)]
    pub reverse: bool,
    /// Skip the scraping phase
    #[arg(short = 'n', long)]
    pub no_scrape: bool,
    /// Skip the trimming phase
    #[arg(short = 'N', long)]
    pub no_trim: bool,
    /// Write starting at this byte offset of the destination
    #[arg(long, default_value = "0", value_parser = size_arg)]
    pub dst_offset: u64,
    /// Skip areas the filesystem marks unused (they are zero-filled in the destination)
    #[arg(long)]
    pub used_map: bool,
    /// Redraw the status block in place with ANSI escapes even when the terminal is not detected
    #[arg(long)]
    pub ansi: bool,
}

/// A byte count such as `4096`, `64K`, `1.5G`.
pub fn size_arg(s: &str) -> Result<u64, String> {
    pm_core::parse_size(s).map_err(|e| e.to_string())
}

/// `raw`, `vhd` (dynamic), `vhd-fixed`, `vmdk` (sparse), `vmdk-flat` or `qcow2`.
pub fn parse_format(s: &str) -> Result<ImageFormat, String> {
    match s.to_ascii_lowercase().as_str() {
        "raw" | "img" => Ok(ImageFormat::Raw),
        "vhd" | "vhd-dynamic" => Ok(ImageFormat::VhdDynamic),
        "vhd-fixed" => Ok(ImageFormat::VhdFixed),
        "vmdk" | "vmdk-sparse" => Ok(ImageFormat::VmdkSparse),
        "vmdk-flat" => Ok(ImageFormat::VmdkFlat),
        "qcow2" => Ok(ImageFormat::Qcow2),
        _ => Err(format!(
            "unknown image format '{s}' (expected raw, vhd, vhd-fixed, vmdk, vmdk-flat or qcow2)"
        )),
    }
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
    /// An operation ran and failed; the text is the whole message (exit 2).
    Failed(String),
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => EXIT_USAGE,
            CliError::Runtime(_) | CliError::Failed(_) => EXIT_RUNTIME,
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
            CliError::Usage(m) | CliError::Failed(m) => f.write_str(m),
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
    fn formats_parse() {
        assert_eq!(parse_format("raw"), Ok(ImageFormat::Raw));
        assert_eq!(parse_format("VHD"), Ok(ImageFormat::VhdDynamic));
        assert_eq!(parse_format("vhd-fixed"), Ok(ImageFormat::VhdFixed));
        assert_eq!(parse_format("vmdk"), Ok(ImageFormat::VmdkSparse));
        assert_eq!(parse_format("vmdk-flat"), Ok(ImageFormat::VmdkFlat));
        assert_eq!(parse_format("qcow2"), Ok(ImageFormat::Qcow2));
        assert!(parse_format("iso").is_err());
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
