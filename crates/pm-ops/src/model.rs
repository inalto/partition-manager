//! The in-memory model of disks and volumes that operations are simulated against.

use pm_core::{
    gpt_types, DiskInfo, FsKind, PartitionInfo, PartitionType, Result, TableKind, VolumeInfo,
};
use pm_parttable::{Entry, Table};
use pm_platform::Platform;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Sectors a GPT needs at the start of the disk: LBA 0, the header and a 128-entry array.
const GPT_HEAD_SECTORS: u64 = 34;

/// Snapshot of every disk and volume, as enumerated by the platform or as it will look
/// after a list of pending operations ([`crate::Planner::simulate_all`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Model {
    pub disks: Vec<DiskInfo>,
    pub volumes: Vec<VolumeInfo>,
    /// Index of the disk Windows is running from.
    pub system_disk: Option<u32>,
}

impl Model {
    /// Enumerate disks, volumes and the system disk from `p`.
    pub fn from_platform(p: &dyn Platform) -> Result<Model> {
        Ok(Model {
            disks: p.enumerate_disks()?,
            volumes: p.enumerate_volumes()?,
            system_disk: p.system_disk(),
        })
    }

    /// The disk whose `index` is `i` (not the position in `disks`).
    pub fn disk(&self, i: u32) -> Option<&DiskInfo> {
        self.disks.iter().find(|d| d.index == i)
    }

    pub fn disk_mut(&mut self, i: u32) -> Option<&mut DiskInfo> {
        self.disks.iter_mut().find(|d| d.index == i)
    }

    /// Partition `number` of disk `disk`.
    pub fn partition(&self, disk: u32, number: u32) -> Option<&PartitionInfo> {
        self.disk(disk)?.partitions.iter().find(|p| p.number == number)
    }

    pub fn partition_mut(&mut self, disk: u32, number: u32) -> Option<&mut PartitionInfo> {
        self.disk_mut(disk)?.partitions.iter_mut().find(|p| p.number == number)
    }

    /// The disk Windows is running from: `system_disk` when the platform reported one,
    /// otherwise the disk holding a partition mounted at `C:\`, otherwise the disk of a
    /// volume mounted at `C:\`. `None` when none of these is known.
    pub fn effective_system_disk(&self) -> Option<u32> {
        self.system_disk
            .or_else(|| {
                self.disks
                    .iter()
                    .find(|d| d.partitions.iter().any(|p| p.letter() == Some('C')))
                    .map(|d| d.index)
            })
            .or_else(|| {
                self.volumes
                    .iter()
                    .find(|v| v.mount_points.iter().any(|m| mount_letter(m) == Some('C')))
                    .and_then(|v| v.disk)
            })
    }

    /// True if `disk` is the disk Windows is running from ([`Model::effective_system_disk`]).
    pub fn is_system_disk(&self, disk: u32) -> bool {
        self.effective_system_disk() == Some(disk)
    }

    /// True if `p` (on `disk`) holds the running Windows installation: `disk` is the system
    /// disk and `p` is its [`windows_partition`].
    pub fn is_windows_partition(&self, disk: u32, p: &PartitionInfo) -> bool {
        self.is_system_disk(disk)
            && self.disk(disk).and_then(windows_partition) == Some(p.number)
    }

    /// True if drive letter `letter` (any case) is used by a partition or a volume.
    pub fn letter_in_use(&self, letter: char) -> bool {
        let letter = letter.to_ascii_uppercase();
        self.disks
            .iter()
            .flat_map(|d| d.partitions.iter())
            .any(|p| p.letter() == Some(letter))
            || self
                .volumes
                .iter()
                .flat_map(|v| v.mount_points.iter())
                .any(|m| mount_letter(m) == Some(letter))
    }
}

/// MBR extended-partition container (not a partition with data of its own).
pub(crate) fn is_container(kind: TableKind, p: &PartitionInfo) -> bool {
    kind == TableKind::Mbr && !p.logical && p.ptype.map_or(false, |t| t.is_extended_mbr())
}

/// EFI system, Microsoft reserved or Windows recovery partition.
pub(crate) fn is_fixed_system(p: &PartitionInfo) -> bool {
    p.ptype.map_or(false, |t| t.is_efi() || t.is_msr() || t.is_recovery())
}

/// The partition of `d` holding Windows, by the first rule that matches:
/// 1. mounted at `C:\`;
/// 2. `flags.boot` (contains the running/boot OS);
/// 3. `flags.required` (checked last: OEM utility partitions carry this bit too);
/// 4. the largest NTFS data partition.
///
/// EFI, MSR, recovery partitions and MBR extended containers never qualify. This one
/// function is used both by the planner's guards and by the clone sizing.
pub fn windows_partition(d: &DiskInfo) -> Option<u32> {
    let candidates: Vec<&PartitionInfo> = d
        .partitions
        .iter()
        .filter(|p| !is_fixed_system(p) && !is_container(d.table, p))
        .collect();
    let rules: [&dyn Fn(&PartitionInfo) -> bool; 3] = [
        &|p| p.letter() == Some('C'),
        &|p| p.flags.boot,
        &|p| p.flags.required,
    ];
    for rule in rules {
        if let Some(p) = candidates.iter().find(|p| rule(p)) {
            return Some(p.number);
        }
    }
    candidates
        .iter()
        .filter(|p| p.fs == Some(FsKind::Ntfs) && p.ptype.map_or(true, |t| t.is_data()))
        .max_by_key(|p| p.len)
        .map(|p| p.number)
}

/// The drive letter of a mount point string such as `E:` or `E:\` (upper-cased).
pub(crate) fn mount_letter(m: &str) -> Option<char> {
    let b = m.as_bytes();
    let drive_root = b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || (b.len() == 3 && (b[2] == b'\\' || b[2] == b'/')));
    drive_root.then(|| (b[0] as char).to_ascii_uppercase())
}

/// Default partition type for a table kind (used when a partition's type is unknown).
pub(crate) fn default_ptype(kind: TableKind) -> PartitionType {
    match kind {
        TableKind::Mbr => PartitionType::Mbr(0x07),
        _ => PartitionType::Gpt(gpt_types::BASIC_DATA),
    }
}

/// Table attribute bits for a partition: GPT bit 0 required, 60 read-only, 62 hidden,
/// 63 no drive letter; MBR bit 0 active.
pub(crate) fn attrs_of(kind: TableKind, p: &PartitionInfo) -> u64 {
    let f = p.flags;
    match kind {
        TableKind::Gpt => {
            (f.required as u64)
                | (f.read_only as u64) << 60
                | (f.hidden as u64) << 62
                | (f.no_letter as u64) << 63
        }
        TableKind::Mbr => f.active as u64,
        _ => 0,
    }
}

/// Rebuild a [`Table`] from a [`DiskInfo`] **for modelling only** (validation, conversion
/// and alignment in the planner). It is lossy and must never be written to a disk: the MBR
/// boot code is zeroed, GPT attribute bits other than required / read-only / hidden /
/// no-letter are dropped, and the usable bounds are reconstructed rather than read.
///
/// Kind, sector size, disk id (GPT GUID / MBR
/// signature when parseable, random otherwise), and one entry per partition with its type
/// (basic data / 0x07 when unknown), GUID, name and attribute bits.
///
/// GPT usable bounds are the defaults of [`Table::empty`], lowered to the earliest
/// partition start (but never below the 34 sectors a GPT needs) for legacy layouts.
pub fn table_from_disk(d: &DiskInfo) -> Table {
    let ss = if d.sector_size == 0 { 512 } else { d.sector_size };
    let mut t = Table::empty(d.table, d.size, ss);
    match d.table {
        TableKind::Gpt => {
            if let Ok(g) = Uuid::parse_str(d.disk_id.trim()) {
                t.disk_guid = Some(g);
            }
            let min_first = GPT_HEAD_SECTORS.saturating_mul(ss as u64);
            if let Some(first) = d.partitions.iter().map(|p| p.start).min() {
                if first < t.gpt_first_usable {
                    t.gpt_first_usable = first.max(min_first);
                }
            }
        }
        TableKind::Mbr => {
            if let Ok(sig) = u32::from_str_radix(d.disk_id.trim(), 16) {
                if sig != 0 {
                    t.mbr_signature = sig;
                }
            }
        }
        TableKind::None | TableKind::Unknown => {}
    }
    t.entries = d
        .partitions
        .iter()
        .map(|p| Entry {
            number: p.number,
            start: p.start,
            len: p.len,
            ptype: p.ptype.unwrap_or_else(|| default_ptype(d.table)),
            guid: if d.table == TableKind::Gpt { p.guid } else { None },
            name: p.name.clone(),
            attrs: attrs_of(d.table, p),
            logical: p.logical,
        })
        .collect();
    t
}

/// Carry the volume-level fields (fs, label, mount points, used, volume id, boot flag) of
/// `old` partitions over to `new` ones, matching by `key`.
pub(crate) fn carry_volume_fields<K: PartialEq>(
    old: &[PartitionInfo],
    new: &mut [PartitionInfo],
    key: impl Fn(&PartitionInfo) -> K,
) {
    for n in new.iter_mut() {
        let k = key(n);
        if let Some(o) = old.iter().find(|o| key(o) == k) {
            n.fs = o.fs;
            n.label = o.label.clone();
            n.mount_points = o.mount_points.clone();
            n.used = o.used;
            n.volume = o.volume.clone();
            n.flags.boot = o.flags.boot;
        }
    }
}
