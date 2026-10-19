//! Disk, partition and volume domain model.

use crate::fs_kind::FsKind;
use crate::ptype::PartitionType;
use crate::units::{format_size, MIB};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Space reserved at the start of every disk (alignment / boot code), in bytes.
const FRONT_RESERVE: u64 = MIB;
/// GPT backup header (1 sector) + backup entry array (32 sectors at 128 entries x 128 bytes / 512).
const GPT_TAIL_SECTORS: u64 = 33;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableKind {
    Mbr,
    Gpt,
    None,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskBus {
    Nvme,
    Sata,
    Usb,
    Scsi,
    Sd,
    Virtual,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskFlags {
    pub removable: bool,
    pub system: bool,
    pub offline: bool,
    pub read_only: bool,
    pub trim: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartFlags {
    pub active: bool,
    pub hidden: bool,
    pub required: bool,
    pub no_letter: bool,
    pub read_only: bool,
    /// Contains the running/boot operating system.
    pub boot: bool,
}

/// Windows: `\\?\Volume{guid}\`; images: `"img:<path>#<index>"`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VolumeId(pub String);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionInfo {
    pub number: u32,
    pub start: u64,
    pub len: u64,
    pub ptype: Option<PartitionType>,
    pub guid: Option<Uuid>,
    pub name: String,
    pub fs: Option<FsKind>,
    pub label: String,
    pub mount_points: Vec<String>,
    pub used: Option<u64>,
    pub flags: PartFlags,
    pub volume: Option<VolumeId>,
    pub logical: bool,
}

impl PartitionInfo {
    /// Exclusive end offset in bytes.
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }

    /// First drive-letter mount point (`C:` or `C:\`), upper-cased.
    pub fn letter(&self) -> Option<char> {
        self.mount_points.iter().find_map(|m| {
            let b = m.as_bytes();
            let drive_root = b.len() >= 2
                && b[0].is_ascii_alphabetic()
                && b[1] == b':'
                && (b.len() == 2 || (b.len() == 3 && (b[2] == b'\\' || b[2] == b'/')));
            drive_root.then(|| (b[0] as char).to_ascii_uppercase())
        })
    }

    /// `"Active & System & Boot"`-style status, or `"None"`.
    pub fn status_text(&self) -> String {
        let mut parts = Vec::new();
        if self.flags.active {
            parts.push("Active");
        }
        if self.flags.required || self.ptype.map_or(false, |t| t.is_efi()) {
            parts.push("System");
        }
        if self.flags.boot {
            parts.push("Boot");
        }
        if parts.is_empty() {
            "None".to_string()
        } else {
            parts.join(" & ")
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskInfo {
    pub index: u32,
    pub path: String,
    pub model: String,
    pub serial: String,
    pub bus: DiskBus,
    pub size: u64,
    pub sector_size: u32,
    pub physical_sector_size: u32,
    pub table: TableKind,
    /// MBR signature (hex) or GPT disk GUID.
    pub disk_id: String,
    pub flags: DiskFlags,
    pub partitions: Vec<PartitionInfo>,
}

impl DiskInfo {
    /// Unallocated gaps `(start, len)` of at least `min_len` bytes.
    ///
    /// The usable area is `[1 MiB, size - tail)` where the tail is the 33-sector GPT backup
    /// structures for GPT disks and zero otherwise. Partitions may be given in any order and
    /// may overlap (e.g. an MBR extended container together with its logical partitions).
    pub fn free_ranges(&self, min_len: u64) -> Vec<(u64, u64)> {
        let tail = match self.table {
            TableKind::Gpt => GPT_TAIL_SECTORS * self.sector_size as u64,
            _ => 0,
        };
        let usable_end = self.size.saturating_sub(tail);
        if usable_end <= FRONT_RESERVE {
            return Vec::new();
        }
        let mut parts: Vec<(u64, u64)> = self
            .partitions
            .iter()
            .filter(|p| p.len > 0)
            .map(|p| (p.start, p.end()))
            .collect();
        parts.sort_unstable();

        let mut gaps = Vec::new();
        let mut push = |start: u64, end: u64| {
            if end > start && end - start >= min_len {
                gaps.push((start, end - start));
            }
        };
        let mut cursor = FRONT_RESERVE;
        for (start, end) in parts {
            if start >= usable_end {
                break;
            }
            if start > cursor {
                push(cursor, start);
            }
            cursor = cursor.max(end);
        }
        if cursor < usable_end {
            push(cursor, usable_end);
        }
        gaps
    }

    /// `"Disk 1 (Fanxiang S880E 2TB, GPT, 1.82 TB)"`.
    pub fn display_name(&self) -> String {
        let table = match self.table {
            TableKind::Gpt => "GPT",
            TableKind::Mbr => "MBR",
            TableKind::None | TableKind::Unknown => "Uninitialized",
        };
        format!(
            "Disk {} ({}, {}, {})",
            self.index,
            self.model,
            table,
            format_size(self.size)
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub id: VolumeId,
    pub disk: Option<u32>,
    pub start: u64,
    pub len: u64,
    pub fs: Option<FsKind>,
    pub label: String,
    pub mount_points: Vec<String>,
    pub total: u64,
    pub free: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{GIB, MIB};

    fn part(number: u32, start: u64, len: u64) -> PartitionInfo {
        PartitionInfo {
            number,
            start,
            len,
            ..Default::default()
        }
    }

    fn disk_100m(table: TableKind) -> DiskInfo {
        DiskInfo {
            index: 1,
            size: 100 * MIB,
            sector_size: 512,
            table,
            partitions: vec![part(1, MIB, 49 * MIB), part(2, 60 * MIB, 30 * MIB)],
            ..Default::default()
        }
    }

    #[test]
    fn free_ranges_finds_gaps() {
        let d = disk_100m(TableKind::Gpt);
        assert_eq!(
            d.free_ranges(MIB),
            vec![(50 * MIB, 10 * MIB), (90 * MIB, 10 * MIB - 33 * 512)]
        );
        // MBR has no tail reserve
        let d = disk_100m(TableKind::Mbr);
        assert_eq!(d.free_ranges(MIB), vec![(50 * MIB, 10 * MIB), (90 * MIB, 10 * MIB)]);
        // min_len filters small gaps
        assert_eq!(d.free_ranges(10 * MIB + 1), Vec::<(u64, u64)>::new());
    }

    #[test]
    fn free_ranges_edge_cases() {
        // empty disk: whole usable area after the 1 MiB front reserve
        let d = DiskInfo {
            size: 10 * MIB,
            sector_size: 512,
            table: TableKind::None,
            ..Default::default()
        };
        assert_eq!(d.free_ranges(1), vec![(MIB, 9 * MIB)]);
        // unsorted + overlapping (extended container with logicals) input
        let mut d = disk_100m(TableKind::Mbr);
        d.partitions = vec![part(3, 60 * MIB, 30 * MIB), part(1, MIB, 49 * MIB), part(5, 70 * MIB, 5 * MIB)];
        assert_eq!(d.free_ranges(MIB), vec![(50 * MIB, 10 * MIB), (90 * MIB, 10 * MIB)]);
        // disk smaller than the front reserve
        let tiny = DiskInfo {
            size: 512 * 1024,
            sector_size: 512,
            ..Default::default()
        };
        assert!(tiny.free_ranges(0).is_empty());
        // 4Kn sectors: GPT tail is 33 * 4096
        let d = DiskInfo {
            size: 10 * MIB,
            sector_size: 4096,
            table: TableKind::Gpt,
            ..Default::default()
        };
        assert_eq!(d.free_ranges(1), vec![(MIB, 9 * MIB - 33 * 4096)]);
    }

    #[test]
    fn display_name_format() {
        let d = DiskInfo {
            index: 1,
            model: "Fanxiang S880E 2TB".into(),
            size: 2_000_398_934_016,
            table: TableKind::Gpt,
            ..Default::default()
        };
        assert_eq!(d.display_name(), "Disk 1 (Fanxiang S880E 2TB, GPT, 1.82 TB)");
        let d = DiskInfo {
            index: 0,
            model: "X".into(),
            size: 4 * GIB,
            table: TableKind::Mbr,
            ..Default::default()
        };
        assert_eq!(d.display_name(), "Disk 0 (X, MBR, 4.00 GB)");
        let d = DiskInfo {
            table: TableKind::None,
            model: "Y".into(),
            size: 0,
            ..Default::default()
        };
        assert_eq!(d.display_name(), "Disk 0 (Y, Uninitialized, 0 B)");
    }

    #[test]
    fn partition_helpers() {
        let mut p = part(1, 100, 50);
        assert_eq!(p.end(), 150);
        assert_eq!(p.status_text(), "None");
        p.flags.active = true;
        assert_eq!(p.status_text(), "Active");
        p.flags.required = true;
        assert_eq!(p.status_text(), "Active & System");
        p.flags.boot = true;
        assert_eq!(p.status_text(), "Active & System & Boot");
        let mut q = part(2, 0, 1);
        q.flags.boot = true;
        assert_eq!(q.status_text(), "Boot");
        q.ptype = Some(PartitionType::Gpt(crate::ptype::gpt_types::EFI_SYSTEM));
        assert_eq!(q.status_text(), "System & Boot");

        assert_eq!(p.letter(), None);
        p.mount_points = vec!["c:\\".into()];
        assert_eq!(p.letter(), Some('C'));
        p.mount_points = vec!["D:".into()];
        assert_eq!(p.letter(), Some('D'));
        p.mount_points = vec!["/mnt/data".into(), "E:\\".into()];
        assert_eq!(p.letter(), Some('E'));
        p.mount_points = vec!["C:\\Mounts\\x".into()];
        assert_eq!(p.letter(), None);
    }

    #[test]
    fn serde_roundtrip_diskinfo() {
        let d = DiskInfo {
            index: 2,
            path: r"\\.\PhysicalDrive2".into(),
            model: "Test Disk".into(),
            serial: "SN123".into(),
            bus: DiskBus::Nvme,
            size: 512 * GIB,
            sector_size: 512,
            physical_sector_size: 4096,
            table: TableKind::Gpt,
            disk_id: "11111111-2222-3333-4444-555555555555".into(),
            flags: DiskFlags {
                removable: false,
                system: true,
                offline: false,
                read_only: false,
                trim: true,
            },
            partitions: vec![
                PartitionInfo {
                    number: 1,
                    start: MIB,
                    len: 100 * MIB,
                    ptype: Some(PartitionType::Gpt(crate::ptype::gpt_types::EFI_SYSTEM)),
                    guid: Some(uuid::Uuid::new_v4()),
                    name: "EFI".into(),
                    fs: Some(FsKind::Fat32),
                    label: "SYSTEM".into(),
                    mount_points: vec!["S:\\".into()],
                    used: Some(30 * MIB),
                    flags: PartFlags {
                        active: false,
                        hidden: true,
                        required: true,
                        no_letter: false,
                        read_only: false,
                        boot: false,
                    },
                    volume: Some(VolumeId("\\\\?\\Volume{abc}\\".into())),
                    logical: false,
                },
                PartitionInfo {
                    number: 5,
                    ptype: Some(PartitionType::Mbr(0x83)),
                    logical: true,
                    ..Default::default()
                },
            ],
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: DiskInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
        assert_eq!(serde_json::to_string(&back).unwrap(), json);

        let v = VolumeInfo {
            id: VolumeId("img:/tmp/a.img#1".into()),
            disk: Some(3),
            fs: Some(FsKind::Ext4),
            total: 10,
            free: 4,
            ..Default::default()
        };
        let back: VolumeInfo = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(back, v);
    }
}
