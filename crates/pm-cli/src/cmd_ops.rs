//! One subcommand per simple operation. Each builds an [`Operation`] and then behaves exactly
//! like `pm apply` for a list of one: show it, validate it, and write only with `--yes`
//! (`--dry-run` validates only; `--emit-json` prints the operation JSON instead of running it,
//! to compose an operations file for `pm apply`).

use crate::cli::{parse_format, parse_letter, size_arg, CliError, CopyMode, CopyModeArgs};
use crate::cmd_apply::{run_ops, Env, RunFlags};
use crate::cmd_image::pick_format;
use clap::{ArgGroup, Args, Subcommand, ValueEnum};
use pm_core::{gpt_types, FsKind, PartitionType, TableKind};
use pm_image::ImageFormat;
use pm_ops::{BootFix, CloneMode, Operation, Sizing, WipeMethod};
use pm_platform::Platform;
use std::io::Write;
use std::path::PathBuf;

/// `--emit-json` plus the flags every operation shares.
#[derive(Args, Debug, Clone, Copy)]
pub struct OpRun {
    #[command(flatten)]
    pub run: RunFlags,
    /// Print the operation as JSON instead of running it (a file of such objects, in a JSON
    /// array, is what `pm apply` reads)
    #[arg(long)]
    pub emit_json: bool,
}

/// `N` or `disk:N`.
pub fn disk_arg(s: &str) -> Result<u32, String> {
    let t = s.get(..5).filter(|p| p.eq_ignore_ascii_case("disk:")).map_or(s, |_| &s[5..]);
    t.parse::<u32>()
        .map_err(|_| format!("'{s}' is not a disk: expected its index, e.g. 1 or disk:1"))
}

/// A drive letter, or "no letter".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LetterArg(pub Option<char>);

/// `E`, `E:` or `none` (no drive letter).
fn letter_or_none(s: &str) -> Result<LetterArg, String> {
    if s.eq_ignore_ascii_case("none") || s == "-" {
        return Ok(LetterArg(None));
    }
    parse_letter(s).map(|c| LetterArg(Some(c)))
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum FsArg {
    Ntfs,
    Fat32,
    Fat16,
    #[value(name = "exfat")]
    ExFat,
    #[value(name = "refs")]
    ReFs,
}

impl FsArg {
    fn kind(self) -> FsKind {
        match self {
            FsArg::Ntfs => FsKind::Ntfs,
            FsArg::Fat32 => FsKind::Fat32,
            FsArg::Fat16 => FsKind::Fat16,
            FsArg::ExFat => FsKind::ExFat,
            FsArg::ReFs => FsKind::ReFs,
        }
    }
}

/// The partition types a user can ask for by name; the table kind decides the actual value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PartKind {
    Basic,
    Efi,
    Msr,
    Recovery,
    Linux,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TableArg {
    Gpt,
    Mbr,
}

impl TableArg {
    pub fn kind(self) -> TableKind {
        match self {
            TableArg::Gpt => TableKind::Gpt,
            TableArg::Mbr => TableKind::Mbr,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum WipeArg {
    Zero,
    Random,
    Dod3,
}

impl WipeArg {
    fn method(self) -> WipeMethod {
        match self {
            WipeArg::Zero => WipeMethod::Zero,
            WipeArg::Random => WipeMethod::Random,
            WipeArg::Dod3 => WipeMethod::Dod3,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum SizingArg {
    Exact,
    Fit,
    Proportional,
}

impl SizingArg {
    fn sizing(self) -> Sizing {
        match self {
            SizingArg::Exact => Sizing::Exact,
            SizingArg::Fit => Sizing::Fit,
            SizingArg::Proportional => Sizing::Proportional,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum BootFixArg {
    None,
    Uefi,
    Bios,
    All,
}

impl BootFixArg {
    fn fix(self) -> BootFix {
        match self {
            BootFixArg::None => BootFix::None,
            BootFixArg::Uefi => BootFix::Uefi,
            BootFixArg::Bios => BootFix::Bios,
            BootFixArg::All => BootFix::All,
        }
    }
}

fn clone_mode(m: CopyModeArgs) -> CloneMode {
    match m.mode() {
        CopyMode::Sector => CloneMode::Sector,
        CopyMode::Auto | CopyMode::Smart => CloneMode::Smart,
    }
}

/// Partition number as `pm table` prints it (1-based).
fn part_arg(s: &str) -> Result<u32, String> {
    match s.parse::<u32>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!("'{s}' is not a partition number (they start at 1)")),
    }
}

/// The operation subcommands.
#[derive(Subcommand, Debug)]
pub enum OpCommand {
    /// Write an empty partition table to a disk that has no partitions
    #[command(group(ArgGroup::new("table").required(true).multiple(false)))]
    InitDisk {
        /// Disk index (`1` or `disk:1`)
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(long, group = "table")]
        gpt: bool,
        #[arg(long, group = "table")]
        mbr: bool,
        #[command(flatten)]
        run: OpRun,
    },
    /// Create a partition in free space (and format it when --fs is given)
    Create {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        /// Start offset, e.g. `1M` (aligned to 1 MiB)
        #[arg(long, value_parser = size_arg)]
        start: u64,
        /// Length, e.g. `100G`
        #[arg(long, value_parser = size_arg)]
        len: u64,
        #[arg(long, value_enum)]
        fs: Option<FsArg>,
        #[arg(long, default_value = "")]
        label: String,
        /// Drive letter to assign, e.g. `E`
        #[arg(long, value_parser = parse_letter)]
        letter: Option<char>,
        /// GPT partition name
        #[arg(long, default_value = "")]
        name: String,
        /// Partition type (default: basic data)
        #[arg(long = "type", value_enum)]
        ptype: Option<PartKind>,
        #[command(flatten)]
        run: OpRun,
    },
    /// Delete a partition
    Delete {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        /// Partition number as `pm table` prints it
        #[arg(value_parser = part_arg)]
        part: u32,
        #[command(flatten)]
        run: OpRun,
    },
    /// Format a partition
    Format {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(value_parser = part_arg)]
        part: u32,
        #[arg(long, value_enum)]
        fs: FsArg,
        #[arg(long, default_value = "")]
        label: String,
        /// Cluster size in bytes (default: the filesystem's choice)
        #[arg(long, value_parser = size_arg)]
        cluster: Option<u64>,
        /// Quick format (skip the full surface write)
        #[arg(long)]
        quick: bool,
        #[command(flatten)]
        run: OpRun,
    },
    /// Resize and/or move a partition
    Resize {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(value_parser = part_arg)]
        part: u32,
        /// New start offset
        #[arg(long, value_parser = size_arg)]
        start: u64,
        /// New length
        #[arg(long, value_parser = size_arg)]
        len: u64,
        #[command(flatten)]
        run: OpRun,
    },
    /// Mark an MBR partition active (bootable)
    SetActive {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(value_parser = part_arg)]
        part: u32,
        #[command(flatten)]
        run: OpRun,
    },
    /// Change the volume label of a partition
    SetLabel {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(value_parser = part_arg)]
        part: u32,
        label: String,
        #[command(flatten)]
        run: OpRun,
    },
    /// Assign a drive letter to a partition (`none` removes it)
    SetLetter {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(value_parser = part_arg)]
        part: u32,
        /// `E`, `E:` or `none`
        #[arg(value_parser = letter_or_none)]
        letter: LetterArg,
        #[command(flatten)]
        run: OpRun,
    },
    /// Change the type of a partition
    SetType {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(value_parser = part_arg)]
        part: u32,
        #[arg(long = "type", value_enum)]
        ptype: PartKind,
        #[command(flatten)]
        run: OpRun,
    },
    /// Overwrite a whole disk (destroys everything on it)
    WipeDisk {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(long, value_enum, default_value = "zero")]
        method: WipeArg,
        #[command(flatten)]
        run: OpRun,
    },
    /// Overwrite one partition (destroys its data)
    WipePartition {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(value_parser = part_arg)]
        part: u32,
        #[arg(long, value_enum, default_value = "zero")]
        method: WipeArg,
        #[command(flatten)]
        run: OpRun,
    },
    /// Convert a disk's partition table between MBR and GPT (`pm convert` converts image files)
    ConvertTable {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(long, value_enum)]
        to: TableArg,
        #[command(flatten)]
        run: OpRun,
    },
    /// Align every partition to 1 MiB
    Align {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[command(flatten)]
        run: OpRun,
    },
    /// Rewrite the MBR (or protective MBR and GPT copies) from the partition table
    RebuildMbr {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[command(flatten)]
        run: OpRun,
    },
    /// Copy a whole disk onto another, laying out the partitions on the target (overwrites it)
    CloneDisk {
        /// Source disk index
        #[arg(value_parser = disk_arg)]
        src: u32,
        /// Target disk index
        #[arg(value_parser = disk_arg)]
        dst: u32,
        #[arg(long, value_enum, default_value = "fit")]
        sizing: SizingArg,
        #[command(flatten)]
        mode: CopyModeArgs,
        #[arg(long, value_enum, default_value = "none")]
        boot_fix: BootFixArg,
        /// Partition table of the copy (default: the same kind as the source); converting
        /// maps the partition types, an MBR copy holds at most 4 partitions within 2 TiB
        #[arg(long, value_enum)]
        table: Option<TableArg>,
        #[command(flatten)]
        run: OpRun,
    },
    /// Copy the Windows system partitions (or all partitions) onto another disk
    MigrateOs {
        #[arg(value_parser = disk_arg)]
        src: u32,
        #[arg(value_parser = disk_arg)]
        dst: u32,
        /// Copy every partition, not only the system ones
        #[arg(long)]
        all_partitions: bool,
        #[arg(long, value_enum, default_value = "fit")]
        sizing: SizingArg,
        #[arg(long, value_enum, default_value = "none")]
        boot_fix: BootFixArg,
        /// Partition table of the copy (default: the same kind as the source); converting
        /// maps the partition types, an MBR copy holds at most 4 partitions within 2 TiB
        #[arg(long, value_enum)]
        table: Option<TableArg>,
        /// Keep the source's disk and partition ids on the copy, so Windows keeps its drive
        /// letters (MountedDevices); detach the source before the copy boots, Windows takes
        /// a disk with a duplicate id offline. Needs the same table kind as the source
        #[arg(long)]
        keep_ids: bool,
        #[command(flatten)]
        run: OpRun,
    },
    /// Save a whole disk to an image file
    ImageDisk {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        /// Image file to create
        path: PathBuf,
        /// raw, vhd, vhd-fixed, vmdk, vmdk-flat or qcow2 (default: from the file extension)
        #[arg(long, value_parser = parse_format)]
        format: Option<ImageFormat>,
        #[command(flatten)]
        mode: CopyModeArgs,
        #[command(flatten)]
        run: OpRun,
    },
    /// Save one partition to an image file
    ImagePartition {
        #[arg(value_parser = disk_arg)]
        disk: u32,
        #[arg(value_parser = part_arg)]
        part: u32,
        path: PathBuf,
        #[arg(long, value_parser = parse_format)]
        format: Option<ImageFormat>,
        #[command(flatten)]
        mode: CopyModeArgs,
        #[command(flatten)]
        run: OpRun,
    },
    /// Write an image file onto a disk (overwrites)
    RestoreImage {
        /// Image file
        path: PathBuf,
        #[arg(value_parser = disk_arg)]
        disk: u32,
        /// Write at this offset of the disk instead of over the whole disk (a partition image)
        #[arg(long, value_parser = size_arg)]
        start: Option<u64>,
        #[command(flatten)]
        run: OpRun,
    },
}

/// The partition type for `kind` on a disk with table `table` (and filesystem `fs`, which
/// picks the MBR type of a basic partition).
pub fn ptype_for(kind: Option<PartKind>, table: TableKind, fs: Option<FsKind>) -> Result<PartitionType, CliError> {
    let kind = kind.unwrap_or(PartKind::Basic);
    if table == TableKind::Mbr {
        let id = match kind {
            PartKind::Basic => match fs {
                Some(FsKind::Fat32) => 0x0C,
                Some(FsKind::Fat16) => 0x0E,
                Some(FsKind::Fat12) => 0x01,
                _ => 0x07,
            },
            PartKind::Efi => 0xEF,
            PartKind::Recovery => 0x27,
            PartKind::Linux => 0x83,
            PartKind::Msr => {
                return Err(CliError::Usage("the Microsoft reserved type exists on GPT disks only".into()))
            }
        };
        return Ok(PartitionType::Mbr(id));
    }
    Ok(PartitionType::Gpt(match kind {
        PartKind::Basic => gpt_types::BASIC_DATA,
        PartKind::Efi => gpt_types::EFI_SYSTEM,
        PartKind::Msr => gpt_types::MSR,
        PartKind::Recovery => gpt_types::WINDOWS_RECOVERY,
        PartKind::Linux => gpt_types::LINUX_FS,
    }))
}

/// The table kind of disk `disk`; a missing disk is an argument error.
fn table_kind_of(platform: &dyn Platform, disk: u32) -> Result<TableKind, CliError> {
    platform
        .enumerate_disks()?
        .into_iter()
        .find(|d| d.index == disk)
        .map(|d| d.table)
        .ok_or_else(|| CliError::Usage(format!("no disk with index {disk}")))
}

impl OpCommand {
    /// The operation this command stands for, with its run flags.
    pub fn build(self, platform: &dyn Platform) -> Result<(Operation, OpRun), CliError> {
        use OpCommand::*;
        Ok(match self {
            InitDisk { disk, gpt, mbr: _, run } => (
                Operation::InitializeDisk { disk, kind: if gpt { TableKind::Gpt } else { TableKind::Mbr } },
                run,
            ),
            Create { disk, start, len, fs, label, letter, name, ptype, run } => {
                let fs = fs.map(FsArg::kind);
                let ptype = ptype_for(ptype, table_kind_of(platform, disk)?, fs)?;
                (Operation::CreatePartition { disk, start, len, ptype, fs, label, letter, name }, run)
            }
            Delete { disk, part, run } => (Operation::DeletePartition { disk, number: part }, run),
            Format { disk, part, fs, label, cluster, quick, run } => {
                let cluster = cluster
                    .map(|c| u32::try_from(c).map_err(|_| CliError::Usage(format!("cluster size {c} is too large"))))
                    .transpose()?;
                (
                    Operation::FormatPartition { disk, number: part, fs: fs.kind(), label, cluster, quick },
                    run,
                )
            }
            Resize { disk, part, start, len, run } => {
                (Operation::ResizeMove { disk, number: part, new_start: start, new_len: len }, run)
            }
            SetActive { disk, part, run } => (Operation::SetActive { disk, number: part }, run),
            SetLabel { disk, part, label, run } => (Operation::SetLabel { disk, number: part, label }, run),
            SetLetter { disk, part, letter, run } => (Operation::SetLetter { disk, number: part, letter: letter.0 }, run),
            SetType { disk, part, ptype, run } => {
                let ptype = ptype_for(Some(ptype), table_kind_of(platform, disk)?, None)?;
                (Operation::SetType { disk, number: part, ptype }, run)
            }
            WipeDisk { disk, method, run } => (Operation::WipeDisk { disk, method: method.method() }, run),
            WipePartition { disk, part, method, run } => {
                (Operation::WipePartition { disk, number: part, method: method.method() }, run)
            }
            ConvertTable { disk, to, run } => (Operation::ConvertTable { disk, to: to.kind() }, run),
            Align { disk, run } => (Operation::AlignPartitions { disk }, run),
            RebuildMbr { disk, run } => (Operation::RebuildMbr { disk }, run),
            CloneDisk { src, dst, sizing, mode, boot_fix, table, run } => (
                Operation::CloneDisk {
                    src_disk: src,
                    dst_disk: dst,
                    sizing: sizing.sizing(),
                    mode: clone_mode(mode),
                    boot_fix: boot_fix.fix(),
                    target_table: table.map(TableArg::kind),
                },
                run,
            ),
            MigrateOs { src, dst, all_partitions, sizing, boot_fix, table, keep_ids, run } => (
                Operation::MigrateOs {
                    src_disk: src,
                    dst_disk: dst,
                    only_system_parts: !all_partitions,
                    sizing: sizing.sizing(),
                    boot_fix: boot_fix.fix(),
                    target_table: table.map(TableArg::kind),
                    keep_ids,
                },
                run,
            ),
            ImageDisk { disk, path, format, mode, run } => {
                let format = pick_format(format, &path)?;
                (Operation::ImageDisk { disk, path, format, mode: clone_mode(mode), rescue: None }, run)
            }
            ImagePartition { disk, part, path, format, mode, run } => {
                let format = pick_format(format, &path)?;
                (Operation::ImagePartition { disk, number: part, path, format, mode: clone_mode(mode), rescue: None }, run)
            }
            RestoreImage { path, disk, start, run } => {
                (Operation::RestoreImage { path, dst_disk: disk, dst_start: start }, run)
            }
        })
    }
}

pub fn run(env: &Env, cmd: OpCommand, out: &mut dyn Write) -> Result<(), CliError> {
    let (op, flags) = cmd.build(env.platform)?;
    if flags.emit_json {
        let mut text = serde_json::to_string_pretty(&op)
            .map_err(|e| CliError::Usage(format!("cannot serialize the operation: {e}")))?;
        text.push('\n');
        return out
            .write_all(text.as_bytes())
            .map_err(|e| CliError::Runtime(pm_core::Error::io(0, 0, e)));
    }
    run_ops(env, vec![op], &flags.run, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_arguments() {
        assert_eq!(disk_arg("3"), Ok(3));
        assert_eq!(disk_arg("disk:3"), Ok(3));
        assert_eq!(disk_arg("DISK:0"), Ok(0));
        assert!(disk_arg("x").is_err());
        assert!(disk_arg("disk:").is_err());
        assert!(disk_arg("-1").is_err());
    }

    #[test]
    fn letters_and_partition_numbers() {
        assert_eq!(letter_or_none("e:"), Ok(LetterArg(Some('E'))));
        assert_eq!(letter_or_none("None"), Ok(LetterArg(None)));
        assert!(letter_or_none("12").is_err());
        assert_eq!(part_arg("2"), Ok(2));
        assert!(part_arg("0").is_err());
        assert!(part_arg("a").is_err());
    }

    #[test]
    fn partition_types_follow_the_table() {
        assert_eq!(
            ptype_for(None, TableKind::Gpt, None).unwrap(),
            PartitionType::Gpt(gpt_types::BASIC_DATA)
        );
        assert_eq!(
            ptype_for(Some(PartKind::Efi), TableKind::Gpt, None).unwrap(),
            PartitionType::Gpt(gpt_types::EFI_SYSTEM)
        );
        assert_eq!(
            ptype_for(None, TableKind::Mbr, Some(FsKind::Fat32)).unwrap(),
            PartitionType::Mbr(0x0C)
        );
        assert_eq!(ptype_for(None, TableKind::Mbr, Some(FsKind::Ntfs)).unwrap(), PartitionType::Mbr(0x07));
        assert_eq!(ptype_for(Some(PartKind::Linux), TableKind::Mbr, None).unwrap(), PartitionType::Mbr(0x83));
        assert!(ptype_for(Some(PartKind::Msr), TableKind::Mbr, None).is_err());
        // an uninitialised disk gets the GPT values (the planner rejects the operation anyway)
        assert!(matches!(ptype_for(None, TableKind::None, None), Ok(PartitionType::Gpt(_))));
    }
}
