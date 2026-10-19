//! exFAT main boot sector.

use crate::util::{le16, le32, le64};
use pm_core::{Error, Result};

/// Validated geometry from the main boot sector. All offsets and sizes are in bytes.
#[derive(Clone, Debug)]
pub struct ExFatBoot {
    pub bytes_per_sector: u32,
    pub cluster_size: u32,
    pub volume_bytes: u64,
    /// Start of the FAT that is in use.
    pub fat_start: u64,
    /// Length of one FAT.
    pub fat_bytes: u64,
    pub heap_start: u64,
    pub cluster_count: u32,
    pub root_cluster: u32,
    pub serial: u32,
}

impl ExFatBoot {
    /// Parse and validate sector 0 against the device length `dev_len`. Every field that is
    /// later used as an offset or a size is checked here so the rest of the reader can rely
    /// on it.
    pub fn parse(s: &[u8], dev_len: u64) -> Result<ExFatBoot> {
        if s.len() < 512 {
            return Err(Error::Parse("exFAT boot sector truncated".into()));
        }
        if &s[3..11] != b"EXFAT   " {
            return Err(Error::Parse("not an exFAT boot sector".into()));
        }
        let bps_shift = s[108] as u32;
        let spc_shift = s[109] as u32;
        if !(9..=12).contains(&bps_shift) {
            return Err(Error::Parse(format!("exFAT bytes-per-sector shift {bps_shift} out of range")));
        }
        if bps_shift + spc_shift > 25 {
            return Err(Error::Parse(format!("exFAT sectors-per-cluster shift {spc_shift} out of range")));
        }
        let bps = 1u64 << bps_shift;
        let cluster = 1u64 << (bps_shift + spc_shift);
        let bad = |what: &str| Error::Parse(format!("exFAT {what}"));

        let volume_sectors = le64(s, 72);
        let volume_bytes = volume_sectors.checked_mul(bps).ok_or_else(|| bad("volume length overflows"))?;
        if volume_bytes == 0 {
            return Err(bad("volume length is zero"));
        }
        if volume_bytes > dev_len {
            return Err(Error::Parse(format!(
                "exFAT volume of {volume_bytes} bytes exceeds the {dev_len}-byte device"
            )));
        }
        let fat_offset = le32(s, 80) as u64 * bps;
        let fat_len = le32(s, 84) as u64 * bps;
        let heap_start = le32(s, 88) as u64 * bps;
        let cluster_count = le32(s, 92);
        let root_cluster = le32(s, 96);
        let num_fats = s[110] as u64;
        let flags = le16(s, 106);

        if !(1..=2).contains(&num_fats) {
            return Err(bad("number of FATs must be 1 or 2"));
        }
        if fat_offset == 0 || fat_len == 0 {
            return Err(bad("FAT region is empty"));
        }
        let fats_end = fat_len
            .checked_mul(num_fats)
            .and_then(|l| l.checked_add(fat_offset))
            .ok_or_else(|| bad("FAT region overflows"))?;
        if fats_end > heap_start {
            return Err(bad("FAT region overlaps the cluster heap"));
        }
        if cluster_count == 0 || cluster_count > 0xFFFF_FFF5 {
            return Err(bad("cluster count out of range"));
        }
        let heap_end = (cluster_count as u64)
            .checked_mul(cluster)
            .and_then(|l| l.checked_add(heap_start))
            .ok_or_else(|| bad("cluster heap overflows"))?;
        if heap_end > volume_bytes {
            return Err(bad("cluster heap extends beyond the volume"));
        }
        if root_cluster < 2 || root_cluster as u64 >= cluster_count as u64 + 2 {
            return Err(bad("root directory cluster out of range"));
        }
        // with two FATs (TexFAT) bit 0 of VolumeFlags names the active one
        let active = if num_fats == 2 && flags & 1 != 0 { 1 } else { 0 };
        Ok(ExFatBoot {
            bytes_per_sector: bps as u32,
            cluster_size: cluster as u32,
            volume_bytes,
            fat_start: fat_offset + active * fat_len,
            fat_bytes: fat_len,
            heap_start,
            cluster_count,
            root_cluster,
            serial: le32(s, 100),
        })
    }

    /// Byte offset of cluster `c` (2-based), when it lies in the heap.
    pub fn cluster_offset(&self, c: u32) -> Result<u64> {
        if c < 2 || c as u64 >= self.cluster_count as u64 + 2 {
            return Err(Error::Parse(format!("cluster {c} is outside the cluster heap")));
        }
        Ok(self.heap_start + (c as u64 - 2) * self.cluster_size as u64)
    }
}
