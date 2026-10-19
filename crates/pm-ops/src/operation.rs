//! The pending operations a user queues before pressing "Apply".

use crate::model::Model;
use pm_core::{format_size, FsKind, PartitionType, TableKind, VolumeId};
use pm_image::ImageFormat;
use pm_platform::MountPoint;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How data is copied when cloning or imaging.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloneMode {
    /// Only blocks the filesystem uses.
    Smart,
    /// Every sector.
    Sector,
}

/// How a disk clone / OS migration sizes the target partitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sizing {
    /// Same starts and lengths as the source.
    Exact,
    /// Shrink toward used data on smaller targets, give the leftover to the largest data
    /// partition on bigger ones.
    Fit,
    /// Scale data partitions by the target/source ratio; system partitions keep their size.
    Proportional,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WipeMethod {
    /// One pass of zeros.
    Zero,
    /// One pass of random data.
    Random,
    /// DoD 5220.22-M, three passes.
    Dod3,
}

/// Boot repair applied to the destination of a clone / migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BootFix {
    None,
    Uefi,
    Bios,
    All,
}

/// Serde mirror of [`MountPoint`] (which lives in `pm-platform` without serde derives).
#[derive(Serialize, Deserialize)]
#[serde(remote = "MountPoint")]
enum MountPointDef {
    Letter(char),
    Folder(PathBuf),
}

/// One pending operation. All offsets and lengths are in bytes; `disk` is a disk index and
/// `number` a partition number as shown in [`pm_core::PartitionInfo::number`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Operation {
    InitializeDisk {
        disk: u32,
        kind: TableKind,
    },
    CreatePartition {
        disk: u32,
        start: u64,
        len: u64,
        ptype: PartitionType,
        fs: Option<FsKind>,
        label: String,
        letter: Option<char>,
        name: String,
    },
    DeletePartition {
        disk: u32,
        number: u32,
    },
    FormatPartition {
        disk: u32,
        number: u32,
        fs: FsKind,
        label: String,
        cluster: Option<u32>,
        quick: bool,
    },
    ResizeMove {
        disk: u32,
        number: u32,
        new_start: u64,
        new_len: u64,
    },
    SetActive {
        disk: u32,
        number: u32,
    },
    SetLabel {
        disk: u32,
        number: u32,
        label: String,
    },
    SetLetter {
        disk: u32,
        number: u32,
        letter: Option<char>,
    },
    SetType {
        disk: u32,
        number: u32,
        ptype: PartitionType,
    },
    Mount {
        volume: VolumeId,
        #[serde(with = "MountPointDef")]
        at: MountPoint,
    },
    Unmount {
        volume: VolumeId,
        #[serde(with = "MountPointDef")]
        at: MountPoint,
    },
    WipeDisk {
        disk: u32,
        method: WipeMethod,
    },
    WipePartition {
        disk: u32,
        number: u32,
        method: WipeMethod,
    },
    ConvertTable {
        disk: u32,
        to: TableKind,
    },
    AlignPartitions {
        disk: u32,
    },
    RebuildMbr {
        disk: u32,
    },
    ClonePartition {
        src_disk: u32,
        src_number: u32,
        dst_disk: u32,
        dst_start: u64,
        dst_len: u64,
        mode: CloneMode,
    },
    CloneDisk {
        src_disk: u32,
        dst_disk: u32,
        sizing: Sizing,
        mode: CloneMode,
        boot_fix: BootFix,
    },
    MigrateOs {
        src_disk: u32,
        dst_disk: u32,
        only_system_parts: bool,
        sizing: Sizing,
        boot_fix: BootFix,
    },
    ImageDisk {
        disk: u32,
        path: PathBuf,
        format: ImageFormat,
        mode: CloneMode,
    },
    ImagePartition {
        disk: u32,
        number: u32,
        path: PathBuf,
        format: ImageFormat,
        mode: CloneMode,
    },
    RestoreImage {
        path: PathBuf,
        dst_disk: u32,
        /// `None` restores a whole-disk image over the entire disk.
        dst_start: Option<u64>,
    },
}

pub(crate) fn table_name(k: TableKind) -> &'static str {
    match k {
        TableKind::Gpt => "GPT",
        TableKind::Mbr => "MBR",
        TableKind::None => "no partition table",
        TableKind::Unknown => "unknown",
    }
}

fn wipe_name(m: WipeMethod) -> &'static str {
    match m {
        WipeMethod::Zero => "zeros",
        WipeMethod::Random => "random data",
        WipeMethod::Dod3 => "DoD 3 passes",
    }
}

fn mode_name(m: CloneMode) -> &'static str {
    match m {
        CloneMode::Smart => "smart",
        CloneMode::Sector => "sector by sector",
    }
}

fn sizing_name(s: Sizing) -> &'static str {
    match s {
        Sizing::Exact => "exact",
        Sizing::Fit => "fit",
        Sizing::Proportional => "proportional",
    }
}

/// `NTFS "Data"`, `NTFS`, or `unformatted`.
fn fs_and_label(fs: Option<FsKind>, label: &str) -> String {
    let fs = fs.map_or("unformatted", |f| f.display_name());
    if label.is_empty() {
        fs.to_string()
    } else {
        format!("{fs} \"{label}\"")
    }
}

impl Operation {
    /// One-line description shown in the pending-operations list and the Apply
    /// confirmation, e.g. `"Delete partition 2 on Disk 3"`.
    pub fn summary(&self) -> String {
        use Operation::*;
        match self {
            InitializeDisk { disk, kind } => {
                format!("Initialize Disk {disk} as {}", table_name(*kind))
            }
            CreatePartition { disk, start, len, fs, label, letter, .. } => {
                let mut s = format!(
                    "Create partition on Disk {disk} at {}, {}, {}",
                    format_size(*start),
                    format_size(*len),
                    fs_and_label(*fs, label)
                );
                if let Some(c) = letter {
                    s.push_str(&format!(" ({}:)", c.to_ascii_uppercase()));
                }
                s
            }
            DeletePartition { disk, number } => format!("Delete partition {number} on Disk {disk}"),
            FormatPartition { disk, number, fs, label, .. } => format!(
                "Format partition {number} on Disk {disk} as {}",
                fs_and_label(Some(*fs), label)
            ),
            ResizeMove { disk, number, new_start, new_len } => format!(
                "Resize/move partition {number} on Disk {disk} to {} – {}",
                format_size(*new_start),
                format_size(new_start.saturating_add(*new_len))
            ),
            SetActive { disk, number } => format!("Set partition {number} on Disk {disk} active"),
            SetLabel { disk, number, label } => {
                format!("Set label of partition {number} on Disk {disk} to \"{label}\"")
            }
            SetLetter { disk, number, letter: Some(c) } => format!(
                "Set drive letter of partition {number} on Disk {disk} to {}:",
                c.to_ascii_uppercase()
            ),
            SetLetter { disk, number, letter: None } => {
                format!("Remove drive letter of partition {number} on Disk {disk}")
            }
            SetType { disk, number, ptype } => {
                format!("Set type of partition {number} on Disk {disk} to {}", ptype.name())
            }
            Mount { volume, at } => format!("Mount {} at {}", volume.0, at.to_path_string()),
            Unmount { volume, at } => format!("Unmount {} from {}", volume.0, at.to_path_string()),
            WipeDisk { disk, method } => format!("Wipe Disk {disk} ({})", wipe_name(*method)),
            WipePartition { disk, number, method } => {
                format!("Wipe partition {number} on Disk {disk} ({})", wipe_name(*method))
            }
            ConvertTable { disk, to } => format!("Convert Disk {disk} to {}", table_name(*to)),
            AlignPartitions { disk } => format!("Align partitions on Disk {disk}"),
            RebuildMbr { disk } => format!("Rebuild MBR on Disk {disk}"),
            ClonePartition { src_disk, src_number, dst_disk, dst_start, dst_len, mode } => format!(
                "Clone partition {src_number} on Disk {src_disk} → Disk {dst_disk} at {}, {} ({})",
                format_size(*dst_start),
                format_size(*dst_len),
                mode_name(*mode)
            ),
            CloneDisk { src_disk, dst_disk, sizing, mode, .. } => format!(
                "Clone Disk {src_disk} → Disk {dst_disk} ({}, {})",
                sizing_name(*sizing),
                mode_name(*mode)
            ),
            MigrateOs { src_disk, dst_disk, only_system_parts, sizing, .. } => format!(
                "Migrate OS from Disk {src_disk} → Disk {dst_disk} ({}, {})",
                if *only_system_parts { "system partitions only" } else { "all partitions" },
                sizing_name(*sizing)
            ),
            ImageDisk { disk, path, format, mode } => format!(
                "Image Disk {disk} → {} ({}, {})",
                path.display(),
                format.display(),
                mode_name(*mode)
            ),
            ImagePartition { disk, number, path, format, mode } => format!(
                "Image partition {number} on Disk {disk} → {} ({}, {})",
                path.display(),
                format.display(),
                mode_name(*mode)
            ),
            RestoreImage { path, dst_disk, dst_start: None } => {
                format!("Restore {} → Disk {dst_disk}", path.display())
            }
            RestoreImage { path, dst_disk, dst_start: Some(s) } => format!(
                "Restore {} → Disk {dst_disk} at {}",
                path.display(),
                format_size(*s)
            ),
        }
    }

    /// True if the operation deletes or overwrites data. `ResizeMove` is conservatively
    /// destructive here because shrinking or moving cannot be told apart without the model;
    /// see [`Operation::is_destructive_in`].
    pub fn is_destructive(&self) -> bool {
        use Operation::*;
        match self {
            InitializeDisk { .. }
            | DeletePartition { .. }
            | FormatPartition { .. }
            | ResizeMove { .. }
            | WipeDisk { .. }
            | WipePartition { .. }
            | ConvertTable { .. }
            | ClonePartition { .. }
            | CloneDisk { .. }
            | MigrateOs { .. }
            | RestoreImage { .. } => true,
            CreatePartition { .. }
            | SetActive { .. }
            | SetLabel { .. }
            | SetLetter { .. }
            | SetType { .. }
            | Mount { .. }
            | Unmount { .. }
            | AlignPartitions { .. }
            | RebuildMbr { .. }
            | ImageDisk { .. }
            | ImagePartition { .. } => false,
        }
    }

    /// Like [`Operation::is_destructive`], but a `ResizeMove` that keeps its start and does
    /// not shrink the partition in `model` (an in-place extend) is not destructive.
    pub fn is_destructive_in(&self, model: &Model) -> bool {
        if let Operation::ResizeMove { disk, number, new_start, new_len } = self {
            if let Some(p) = model.partition(*disk, *number) {
                return *new_start != p.start || *new_len < p.len;
            }
        }
        self.is_destructive()
    }

    /// True if applying the operation reads or writes disk `d`. Mount/Unmount address a
    /// volume, not a disk, and return false.
    pub fn touches_disk(&self, d: u32) -> bool {
        use Operation::*;
        match self {
            InitializeDisk { disk, .. }
            | CreatePartition { disk, .. }
            | DeletePartition { disk, .. }
            | FormatPartition { disk, .. }
            | ResizeMove { disk, .. }
            | SetActive { disk, .. }
            | SetLabel { disk, .. }
            | SetLetter { disk, .. }
            | SetType { disk, .. }
            | WipeDisk { disk, .. }
            | WipePartition { disk, .. }
            | ConvertTable { disk, .. }
            | AlignPartitions { disk }
            | RebuildMbr { disk }
            | ImageDisk { disk, .. }
            | ImagePartition { disk, .. } => *disk == d,
            ClonePartition { src_disk, dst_disk, .. }
            | CloneDisk { src_disk, dst_disk, .. }
            | MigrateOs { src_disk, dst_disk, .. } => *src_disk == d || *dst_disk == d,
            RestoreImage { dst_disk, .. } => *dst_disk == d,
            Mount { .. } | Unmount { .. } => false,
        }
    }

    /// The disk the operation writes to (the destination for clones/restores), if any.
    pub fn target_disk(&self) -> Option<u32> {
        use Operation::*;
        match self {
            InitializeDisk { disk, .. }
            | CreatePartition { disk, .. }
            | DeletePartition { disk, .. }
            | FormatPartition { disk, .. }
            | ResizeMove { disk, .. }
            | SetActive { disk, .. }
            | SetLabel { disk, .. }
            | SetLetter { disk, .. }
            | SetType { disk, .. }
            | WipeDisk { disk, .. }
            | WipePartition { disk, .. }
            | ConvertTable { disk, .. }
            | AlignPartitions { disk }
            | RebuildMbr { disk } => Some(*disk),
            ClonePartition { dst_disk, .. }
            | CloneDisk { dst_disk, .. }
            | MigrateOs { dst_disk, .. }
            | RestoreImage { dst_disk, .. } => Some(*dst_disk),
            ImageDisk { .. } | ImagePartition { .. } | Mount { .. } | Unmount { .. } => None,
        }
    }
}
