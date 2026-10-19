//! The in-memory model of disks and volumes that operations are simulated against.

use pm_core::{gpt_types, DiskInfo, PartitionInfo, PartitionType, Result, TableKind, VolumeInfo};
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

    /// True if `disk` is the disk Windows is running from.
    pub fn is_system_disk(&self, disk: u32) -> bool {
        self.system_disk == Some(disk)
    }

    /// True if `p` (on `disk`) holds the running Windows installation: it lives on the
    /// system disk and is flagged required/boot or mounted at `C:\`.
    pub fn is_windows_partition(&self, disk: u32, p: &PartitionInfo) -> bool {
        self.is_system_disk(disk)
            && (p.flags.required || p.flags.boot || p.letter() == Some('C'))
            && !p.ptype.map_or(false, |t| t.is_efi() || t.is_msr() || t.is_recovery())
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

/// Rebuild a [`Table`] from a [`DiskInfo`]: kind, sector size, disk id (GPT GUID / MBR
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
