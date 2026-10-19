//! The NTFS boot sector (BIOS parameter block).

use crate::util::{le16, le64};
use pm_core::{Error, Result};

#[derive(Clone, Debug)]
pub struct NtfsBoot {
    pub bytes_per_sector: u32,
    pub cluster_size: u32,
    /// Sectors in the volume, excluding the backup boot sector that follows it.
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mftmirr_lcn: u64,
    pub record_size: u32,
    pub index_block_size: u32,
    pub serial: u64,
}

/// Decode a "clusters per record" byte: positive = clusters, negative = 2^-v bytes.
fn record_bytes(v: i8, cluster: u64) -> Option<u64> {
    match v {
        0 => None,
        v if v > 0 => cluster.checked_mul(v as u64),
        v => {
            let shift = -(v as i32);
            (shift < 32).then(|| 1u64 << shift)
        }
    }
}

fn valid_block(size: Option<u64>) -> Option<u32> {
    size.filter(|s| s.is_power_of_two() && (512..=65536).contains(s)).map(|s| s as u32)
}

impl NtfsBoot {
    pub fn parse(b: &[u8]) -> Result<NtfsBoot> {
        let bad = |m: &str| Error::Parse(format!("NTFS boot sector: {m}"));
        if b.len() < 512 || &b[3..11] != b"NTFS    " {
            return Err(Error::Parse("not an NTFS boot sector".into()));
        }
        let bps = le16(b, 0x0B) as u64;
        if !bps.is_power_of_two() || !(256..=4096).contains(&bps) {
            return Err(bad("invalid bytes per sector"));
        }
        // values >= 0x80 encode 2^(256 - v) sectors per cluster (clusters above 64 KiB)
        let spc_raw = b[0x0D];
        let spc = match spc_raw {
            0 => return Err(bad("zero sectors per cluster")),
            v if v < 0x80 => v as u64,
            v => {
                let shift = 256 - v as u32;
                if shift > 20 {
                    return Err(bad("invalid sectors per cluster"));
                }
                1u64 << shift
            }
        };
        let cluster = bps * spc;
        if !cluster.is_power_of_two() || cluster > 2 << 20 {
            return Err(bad("invalid cluster size"));
        }
        let total_sectors = le64(b, 0x28);
        let mft_lcn = le64(b, 0x30);
        let mftmirr_lcn = le64(b, 0x38);
        let record_size = valid_block(record_bytes(b[0x40] as i8, cluster)).ok_or_else(|| bad("invalid MFT record size"))?;
        // the per-directory $INDEX_ROOT value is authoritative; fall back to 4 KiB
        let index_block_size = valid_block(record_bytes(b[0x44] as i8, cluster)).unwrap_or(4096);
        let total_clusters = total_sectors.checked_mul(bps).ok_or_else(|| bad("volume too large"))? / cluster;
        if total_clusters == 0 || mft_lcn >= total_clusters || mftmirr_lcn >= total_clusters {
            return Err(bad("MFT location outside the volume"));
        }
        Ok(NtfsBoot {
            bytes_per_sector: bps as u32,
            cluster_size: cluster as u32,
            total_sectors,
            mft_lcn,
            mftmirr_lcn,
            record_size,
            index_block_size,
            serial: le64(b, 0x48),
        })
    }

    /// Volume size in bytes (the backup boot sector lies just past it).
    pub fn volume_bytes(&self) -> u64 {
        self.total_sectors * self.bytes_per_sector as u64
    }

    pub fn total_clusters(&self) -> u64 {
        self.volume_bytes() / self.cluster_size as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sector(spc: u8, per_record: u8) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        b[0x0D] = spc;
        b[0x28..0x30].copy_from_slice(&(1u64 << 30).to_le_bytes());
        b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
        b[0x40] = per_record;
        b[0x44] = 1;
        b
    }

    #[test]
    fn cluster_and_record_sizes() {
        let b = NtfsBoot::parse(&sector(8, 0xF6)).unwrap();
        assert_eq!(b.cluster_size, 4096);
        assert_eq!(b.record_size, 1024);
        assert_eq!(b.index_block_size, 4096);
        // 0xF8 = 2^8 sectors = 128 KiB clusters
        let b = NtfsBoot::parse(&sector(0xF8, 0xF6)).unwrap();
        assert_eq!(b.cluster_size, 128 << 10);
        // a record of 2 clusters of 512 bytes
        let b = NtfsBoot::parse(&sector(1, 2)).unwrap();
        assert_eq!(b.record_size, 1024);
        assert!(NtfsBoot::parse(&sector(0, 0xF6)).is_err());
        assert!(NtfsBoot::parse(&sector(8, 0)).is_err());
        assert!(NtfsBoot::parse(&sector(8, 0x80)).is_err());
        assert!(NtfsBoot::parse(&sector(3, 0xF6)).is_err());
    }
}
