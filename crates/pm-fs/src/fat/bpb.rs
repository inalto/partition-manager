//! BIOS Parameter Block parsing for FAT12/16/32.

use crate::util::{le16, le32, trim_padded_latin1};
use pm_core::FsKind;

#[derive(Clone, Debug)]
pub struct Bpb {
    pub kind: FsKind,
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32,
    pub num_fats: u32,
    /// Root directory entries (FAT12/16 only; 0 on FAT32).
    pub root_entries: u32,
    pub total_sectors: u64,
    /// Sectors per FAT.
    pub fat_sectors: u32,
    /// First cluster of the root directory (FAT32 only; 0 otherwise).
    pub root_cluster: u32,
    pub volume_id: u32,
    /// Label from the BPB ("NO NAME" and blank labels give an empty string).
    pub label: String,
    /// Sectors occupied by the fixed root directory (FAT12/16).
    pub root_dir_sectors: u32,
    /// Number of data clusters.
    pub cluster_count: u32,
}

impl Bpb {
    /// Parse and sanity-check the boot sector. Returns `None` when `sector0` does not look
    /// like a FAT boot sector.
    pub fn parse(sector0: &[u8]) -> Option<Bpb> {
        if sector0.len() < 512 || sector0[510] != 0x55 || sector0[511] != 0xAA {
            return None;
        }
        let bps = le16(sector0, 11) as u32;
        if !matches!(bps, 512 | 1024 | 2048 | 4096) {
            return None;
        }
        let spc = sector0[13] as u32;
        if !(1..=128).contains(&spc) || !spc.is_power_of_two() {
            return None;
        }
        let reserved = le16(sector0, 14) as u32;
        if reserved < 1 {
            return None;
        }
        let num_fats = sector0[16] as u32;
        if !(1..=2).contains(&num_fats) {
            return None;
        }
        let root_entries = le16(sector0, 17) as u32;
        let total16 = le16(sector0, 19) as u64;
        let total_sectors = if total16 != 0 { total16 } else { le32(sector0, 32) as u64 };
        if total_sectors == 0 {
            return None;
        }
        let fat16 = le16(sector0, 22) as u32;
        let fat_sectors = if fat16 != 0 { fat16 } else { le32(sector0, 36) };
        if fat_sectors == 0 {
            return None;
        }
        let root_dir_sectors = (root_entries * 32).div_ceil(bps);
        let overhead = reserved as u64 + num_fats as u64 * fat_sectors as u64 + root_dir_sectors as u64;
        if overhead >= total_sectors {
            return None;
        }
        let clusters = (total_sectors - overhead) / spc as u64;
        if clusters == 0 {
            return None;
        }
        let cluster_count = clusters.min(u32::MAX as u64) as u32;
        let kind = if clusters < 4085 {
            FsKind::Fat12
        } else if clusters < 65525 {
            FsKind::Fat16
        } else {
            FsKind::Fat32
        };
        let is32 = kind == FsKind::Fat32;
        let root_cluster = if is32 { le32(sector0, 44) } else { 0 };
        if is32 && root_cluster < 2 {
            return None;
        }
        if !is32 && root_entries == 0 {
            return None;
        }
        let (sig_off, id_off, label_off) = if is32 { (66, 67, 71) } else { (38, 39, 43) };
        let (volume_id, label) = if sector0[sig_off] == 0x29 || sector0[sig_off] == 0x28 {
            let raw = trim_padded_latin1(&sector0[label_off..label_off + 11]);
            let label = if raw == "NO NAME" { String::new() } else { raw };
            (le32(sector0, id_off), label)
        } else {
            (0, String::new())
        };
        Some(Bpb {
            kind,
            bytes_per_sector: bps,
            sectors_per_cluster: spc,
            reserved_sectors: reserved,
            num_fats,
            root_entries,
            total_sectors,
            fat_sectors,
            root_cluster,
            volume_id,
            label,
            root_dir_sectors,
            cluster_count,
        })
    }

    pub fn cluster_bytes(&self) -> u32 {
        self.bytes_per_sector * self.sectors_per_cluster
    }

    /// Filesystem size in bytes.
    pub fn total_bytes(&self) -> u64 {
        self.total_sectors * self.bytes_per_sector as u64
    }

    /// Byte offset of the first FAT.
    pub fn fat_start(&self) -> u64 {
        self.reserved_sectors as u64 * self.bytes_per_sector as u64
    }

    /// Size in bytes of one FAT.
    pub fn fat_bytes(&self) -> u64 {
        self.fat_sectors as u64 * self.bytes_per_sector as u64
    }

    /// Byte offset of the fixed root directory (FAT12/16).
    pub fn root_dir_start(&self) -> u64 {
        self.fat_start() + self.num_fats as u64 * self.fat_bytes()
    }

    /// Size in bytes of the fixed root directory (FAT12/16).
    pub fn root_dir_bytes(&self) -> u64 {
        self.root_dir_sectors as u64 * self.bytes_per_sector as u64
    }

    /// Byte offset of cluster 2.
    pub fn data_start(&self) -> u64 {
        self.root_dir_start() + self.root_dir_bytes()
    }

    /// Byte offset of data cluster `n` (n >= 2).
    pub fn cluster_offset(&self, n: u32) -> u64 {
        self.data_start() + (n as u64 - 2) * self.cluster_bytes() as u64
    }
}
