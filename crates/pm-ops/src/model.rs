//! The in-memory model of disks and volumes that operations are simulated against.

use pm_core::{
    gpt_types, DiskInfo, FsKind, PartitionInfo, PartitionType, Result, TableKind, VolumeInfo,
};
use pm_parttable::{Entry, Table};
use pm_core::SliceDevice;
use pm_platform::{OpenMode, Platform};
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
    /// `(disk, partition start)` of partitions whose filesystem holds `\Windows\System32`,
    /// found by [`Model::from_platform`] on disks where no partition is mounted at `C:\`
    /// (the second rule of [`windows_partition`]).
    #[serde(default)]
    pub windows_found: Vec<(u32, u64)>,
}

impl Model {
    /// Enumerate disks, volumes and the system disk from `p`, and look for Windows
    /// installations on disks without a `C:\` partition ([`platform_windows_probe`]).
    pub fn from_platform(p: &dyn Platform) -> Result<Model> {
        let mut m = Model {
            disks: p.enumerate_disks()?,
            volumes: p.enumerate_volumes()?,
            system_disk: p.system_disk(),
            windows_found: Vec::new(),
        };
        m.probe_windows(&platform_windows_probe(p));
        Ok(m)
    }

    /// Fill [`Model::windows_found`] with `probe`: on every disk where no candidate partition
    /// is mounted at `C:\`, the first candidate (in table order) for which `probe` says its
    /// filesystem contains `\Windows\System32`. Only NTFS partitions and partitions of
    /// unknown filesystem are probed.
    pub fn probe_windows(&mut self, probe: &dyn Fn(u32, &PartitionInfo) -> bool) {
        self.windows_found.clear();
        for d in &self.disks {
            let cands = candidates(d);
            if cands.iter().any(|p| p.letter() == Some('C')) {
                continue;
            }
            if let Some(p) = cands
                .iter()
                .filter(|p| matches!(p.fs, None | Some(FsKind::Ntfs)))
                .find(|p| probe(d.index, p))
            {
                self.windows_found.push((d.index, p.start));
            }
        }
    }

    /// The probe answers recorded by [`Model::probe_windows`], for [`windows_partition`].
    pub fn windows_probe(&self) -> impl Fn(u32, &PartitionInfo) -> bool + '_ {
        move |disk, p| self.windows_found.contains(&(disk, p.start))
    }

    /// The partition of disk `disk` holding Windows ([`windows_partition`] with this model's
    /// probe answers).
    pub fn windows_partition_of(&self, disk: u32) -> Option<u32> {
        let probe = self.windows_probe();
        windows_partition(self.disk(disk)?, Some(&probe))
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
        self.is_system_disk(disk) && self.windows_partition_of(disk) == Some(p.number)
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

/// Partitions of `d` that may hold Windows: not EFI, MSR, recovery or an MBR container.
fn candidates(d: &DiskInfo) -> Vec<&PartitionInfo> {
    d.partitions
        .iter()
        .filter(|p| !is_fixed_system(p) && !is_container(d.table, p))
        .collect()
}

/// The partition of `d` holding Windows, by the first rule that matches:
/// 1. mounted at `C:\`;
/// 2. its filesystem contains `\Windows\System32`, as `probe(disk, partition)` answers
///    (with a platform: [`platform_windows_probe`]; a [`Model`] records the answers in
///    [`Model::windows_found`] and hands them out through [`Model::windows_probe`]);
///    `None` skips this rule;
/// 3. the largest NTFS data partition.
///
/// The partition flags (`boot`, `required`) are not used: OEM utility partitions carry them
/// too. EFI, MSR, recovery partitions and MBR extended containers never qualify. This one
/// function is used both by the planner's guards and by the clone sizing.
pub fn windows_partition(
    d: &DiskInfo,
    probe: Option<&dyn Fn(u32, &PartitionInfo) -> bool>,
) -> Option<u32> {
    let cands = candidates(d);
    if let Some(p) = cands.iter().find(|p| p.letter() == Some('C')) {
        return Some(p.number);
    }
    if let Some(probe) = probe {
        if let Some(p) = cands.iter().find(|p| probe(d.index, p)) {
            return Some(p.number);
        }
    }
    cands
        .iter()
        .filter(|p| p.fs == Some(FsKind::Ntfs) && p.ptype.map_or(true, |t| t.is_data()))
        .max_by_key(|p| p.len)
        .map(|p| p.number)
}

/// Largest directory listing looked at while probing for Windows (a damaged or huge root is
/// not worth the wait).
const PROBE_MAX_ENTRIES: usize = 4096;

/// A probe for [`windows_partition`] / [`Model::probe_windows`] that opens the partition
/// read-only through `p` and looks for a `Windows` directory holding a `System32` directory
/// in the root of its filesystem. Any error (unreadable disk, unknown filesystem) is "no".
pub fn platform_windows_probe(p: &dyn Platform) -> impl Fn(u32, &PartitionInfo) -> bool + '_ {
    move |disk, part| has_windows_dir(p, disk, part).unwrap_or(false)
}

fn has_windows_dir(p: &dyn Platform, disk: u32, part: &PartitionInfo) -> Result<bool> {
    let dev = p.open_disk(disk, OpenMode::ReadOnly)?;
    let slice = SliceDevice::new(dev, part.start, part.len)?;
    let mut fs = pm_fs::open(Box::new(slice))?;
    let find = |list: Vec<pm_fs::Entry>, name: &str| {
        if list.len() > PROBE_MAX_ENTRIES {
            return None;
        }
        list.into_iter().find(|e| e.is_dir && !e.deleted && e.name.eq_ignore_ascii_case(name))
    };
    let root = fs.root();
    let Some(windows) = find(fs.list(&root)?, "Windows") else { return Ok(false) };
    Ok(find(fs.list(&windows)?, "System32").is_some())
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
