//! Filesystem identification from on-disk signatures.

use crate::fat::bpb::Bpb;
use crate::util::{be32, be64, cstr_utf8, le16, le32, le64, read_padded, trim_padded_latin1, uuid_string, volume_id_string};
use pm_core::{BlockDevice, FsKind};
use serde::{Deserialize, Serialize};

/// Bytes read from the start of a candidate filesystem. Covers every signature checked
/// (the furthest is the btrfs superblock at 64 KiB).
const WINDOW: usize = 68 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FsProbe {
    pub kind: FsKind,
    pub label: String,
    /// Filesystem size in bytes; 0 when the header does not say.
    pub total: u64,
    /// Used bytes, when cheaply known (never set by [`detect`]).
    pub used: Option<u64>,
    /// Cluster / block size in bytes.
    pub cluster: Option<u32>,
    pub uuid: String,
}

impl FsProbe {
    fn new(kind: FsKind) -> FsProbe {
        FsProbe { kind, label: String::new(), total: 0, used: None, cluster: None, uuid: String::new() }
    }
}

fn at(w: &[u8], off: usize, sig: &[u8]) -> bool {
    w.get(off..off + sig.len()) == Some(sig)
}

/// Identify the filesystem starting at byte `offset` of `dev`. Devices shorter than the
/// probe window are zero-extended, so a short read never fails the probe.
pub fn detect(dev: &mut dyn BlockDevice, offset: u64) -> Option<FsProbe> {
    let mut w = vec![0u8; WINDOW];
    read_padded(dev, offset, &mut w).ok()?;
    let w = &w[..];

    if at(w, 3, b"EXFAT   ") {
        return Some(exfat(w));
    }
    if at(w, 3, b"NTFS    ") {
        return Some(ntfs(w));
    }
    if at(w, 3, b"ReFS") || at(w, 0x10, b"FSRS") {
        return Some(refs(w));
    }
    if let Some(bpb) = Bpb::parse(w) {
        return Some(fat(&bpb));
    }
    if le16(w, 0x400 + 0x38) == 0xEF53 {
        return Some(ext(w));
    }
    if at(w, 0x10040, b"_BHRfS_M") {
        return Some(btrfs(w));
    }
    if at(w, 0, b"XFSB") {
        return Some(xfs(w));
    }
    if at(w, 0x400, b"H+") || at(w, 0x400, b"HX") {
        return Some(hfs_plus(w));
    }
    if at(w, 0x20, b"NXSB") {
        return Some(apfs(w));
    }
    if at(w, 0, b"LUKS\xba\xbe") {
        return Some(FsProbe::new(FsKind::Luks));
    }
    if at(w, 4096 - 10, b"SWAPSPACE2") || at(w, 4096 - 10, b"SWAP-SPACE") {
        return Some(FsProbe::new(FsKind::LinuxSwap));
    }
    if at(w, 0x8001, b"CD001") {
        return Some(iso9660(w));
    }
    if at(w, 0x8001, b"BEA01") || (0..4).any(|k| at(w, 0x8001 + 0x800 * k, b"NSR02") || at(w, 0x8001 + 0x800 * k, b"NSR03")) {
        return Some(FsProbe::new(FsKind::Udf));
    }
    None
}

/// Adapter for `pm_parttable::search_lost`: `(kind, label, total bytes)`.
pub fn probe_for_search(dev: &mut dyn BlockDevice, off: u64) -> Option<(FsKind, String, u64)> {
    detect(dev, off).map(|p| (p.kind, p.label, p.total))
}

fn ntfs(w: &[u8]) -> FsProbe {
    let mut p = FsProbe::new(FsKind::Ntfs);
    let bps = le16(w, 11) as u64;
    let spc_raw = w[13];
    // values >= 0x80 encode the cluster size as a power of two: 2^(256 - value) sectors
    let spc: u64 = if spc_raw < 0x80 { spc_raw as u64 } else { 1u64 << (256 - spc_raw as u32).min(20) };
    p.total = le64(w, 40).saturating_mul(bps);
    let cluster = bps * spc;
    if cluster > 0 && cluster <= u32::MAX as u64 {
        p.cluster = Some(cluster as u32);
    }
    p.uuid = format!("{:016X}", le64(w, 72));
    p
}

fn exfat(w: &[u8]) -> FsProbe {
    let mut p = FsProbe::new(FsKind::ExFat);
    let bps_shift = w[108] as u32;
    let spc_shift = w[109] as u32;
    if (9..=12).contains(&bps_shift) && bps_shift + spc_shift <= 25 {
        p.total = le64(w, 72).saturating_mul(1u64 << bps_shift);
        p.cluster = Some(1u32 << (bps_shift + spc_shift));
    }
    p
}

fn refs(w: &[u8]) -> FsProbe {
    let mut p = FsProbe::new(FsKind::ReFs);
    let bps = le32(w, 0x20) as u64;
    let spc = le32(w, 0x24) as u64;
    p.total = le64(w, 0x18).saturating_mul(bps);
    if bps * spc > 0 && bps * spc <= u32::MAX as u64 {
        p.cluster = Some((bps * spc) as u32);
    }
    p
}

fn fat(bpb: &Bpb) -> FsProbe {
    let mut p = FsProbe::new(bpb.kind);
    p.label = bpb.label.clone();
    p.total = bpb.total_bytes();
    p.cluster = Some(bpb.cluster_bytes());
    p.uuid = volume_id_string(bpb.volume_id);
    p
}

fn ext(w: &[u8]) -> FsProbe {
    const SB: usize = 0x400;
    let compat = le32(w, SB + 0x5C);
    let incompat = le32(w, SB + 0x60);
    let ro_compat = le32(w, SB + 0x64);
    let kind = if incompat & (0x40 | 0x80) != 0 || ro_compat & (0x8 | 0x40) != 0 {
        FsKind::Ext4
    } else if compat & 0x4 != 0 {
        FsKind::Ext3
    } else {
        FsKind::Ext2
    };
    let mut p = FsProbe::new(kind);
    let log = le32(w, SB + 0x18);
    if log <= 6 {
        let block = 1024u64 << log;
        let mut blocks = le32(w, SB + 4) as u64;
        if incompat & 0x80 != 0 {
            blocks |= (le32(w, SB + 0x150) as u64) << 32;
        }
        p.total = blocks.saturating_mul(block);
        p.cluster = Some(block as u32);
    }
    p.label = cstr_utf8(&w[SB + 0x78..SB + 0x88]);
    p.uuid = uuid_string(&w[SB + 0x68..SB + 0x78]);
    p
}

fn btrfs(w: &[u8]) -> FsProbe {
    const SB: usize = 0x10000;
    let mut p = FsProbe::new(FsKind::Btrfs);
    p.total = le64(w, SB + 0x70);
    p.cluster = Some(le32(w, SB + 0x90)).filter(|&c| c > 0);
    p.label = cstr_utf8(&w[SB + 0x12B..SB + 0x12B + 256]);
    p.uuid = uuid_string(&w[SB + 0x20..SB + 0x30]);
    p
}

fn xfs(w: &[u8]) -> FsProbe {
    let mut p = FsProbe::new(FsKind::Xfs);
    let block = be32(w, 4) as u64;
    p.total = be64(w, 8).saturating_mul(block);
    p.cluster = Some(block as u32).filter(|&c| c > 0);
    p.uuid = uuid_string(&w[32..48]);
    p.label = cstr_utf8(&w[108..120]);
    p
}

fn hfs_plus(w: &[u8]) -> FsProbe {
    let mut p = FsProbe::new(FsKind::HfsPlus);
    let block = be32(w, 0x400 + 40) as u64;
    p.total = (be32(w, 0x400 + 44) as u64).saturating_mul(block);
    p.cluster = Some(block as u32).filter(|&c| c > 0);
    p
}

fn apfs(w: &[u8]) -> FsProbe {
    let mut p = FsProbe::new(FsKind::Apfs);
    let block = le32(w, 0x24) as u64;
    p.total = le64(w, 0x28).saturating_mul(block);
    p.cluster = Some(block as u32).filter(|&c| c > 0);
    p.uuid = uuid_string(&w[0x48..0x58]);
    p
}

fn iso9660(w: &[u8]) -> FsProbe {
    const PVD: usize = 0x8000;
    let mut p = FsProbe::new(FsKind::Iso9660);
    let block = le16(w, PVD + 128) as u64;
    p.total = (le32(w, PVD + 80) as u64).saturating_mul(block);
    p.cluster = Some(block as u32).filter(|&c| c > 0);
    p.label = trim_padded_latin1(&w[PVD + 40..PVD + 72]);
    p
}
