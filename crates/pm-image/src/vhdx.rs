//! Microsoft VHDX images, opened **read-only**, per [MS-VHDX]. All fields are little-endian.
//!
//! Layout: a 64 KiB file identifier, two 4 KiB headers (64 KiB and 128 KiB), two region
//! tables (192 KiB and 256 KiB; the first is used), and regions placed by the region table
//! (the BAT and the metadata region). The BAT maps payload blocks to file offsets; after
//! every `chunk_ratio` payload entries comes one sector-bitmap entry, which only
//! differencing disks use and which is skipped here.
//!
//! Supported: fixed and dynamic VHDX. Differencing disks (`HasParent`, or a partially
//! present block) are refused with [`Error::Unsupported`]. The log is not replayed, so an
//! image that was not closed cleanly can show stale metadata. Writing is not implemented.

use pm_core::{BlockDevice, Error, FileDevice, Result};
use std::path::Path;

pub(crate) const SIGNATURE: &[u8; 8] = b"vhdxfile";

const HEADER1_OFFSET: u64 = 64 * 1024;
const HEADER2_OFFSET: u64 = 128 * 1024;
const HEADER_SIZE: usize = 4096;
const REGION_TABLE_OFFSET: u64 = 192 * 1024;
const REGION_TABLE_SIZE: usize = 64 * 1024;
const MIN_BLOCK_SIZE: u64 = 1 << 20;
const MAX_BLOCK_SIZE: u64 = 256 << 20;
/// The whole BAT is read into memory; refuse absurdly large ones (16M entries).
const MAX_BAT_BYTES: u64 = 128 << 20;

const GUID_BAT: [u8; 16] = guid(0x2DC27766, 0xF623, 0x4200, [0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08]);
const GUID_METADATA: [u8; 16] = guid(0x8B7CA206, 0x4790, 0x4B9A, [0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E]);
const GUID_FILE_PARAMETERS: [u8; 16] =
    guid(0xCAA16737, 0xFA36, 0x4D43, [0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B]);
const GUID_VIRTUAL_DISK_SIZE: [u8; 16] =
    guid(0x2FA54224, 0xCD1B, 0x4876, [0xB2, 0xB5, 0x73, 0xA3, 0xE7, 0xD3, 0xFA, 0x1A]);
const GUID_LOGICAL_SECTOR_SIZE: [u8; 16] =
    guid(0x8141BF1D, 0xA96F, 0x4709, [0xBA, 0x47, 0xF2, 0x33, 0xF8, 0xFA, 0xAB, 0x5F]);

const STATE_NOT_PRESENT: u64 = 0;
const STATE_UNDEFINED: u64 = 1;
const STATE_ZERO: u64 = 2;
const STATE_UNMAPPED: u64 = 3;
const STATE_FULLY_PRESENT: u64 = 6;
const STATE_PARTIALLY_PRESENT: u64 = 7;

/// A GUID in its on-disk (mixed-endian) byte order.
const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let a = d1.to_le_bytes();
    let b = d2.to_le_bytes();
    let c = d3.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7],
    ]
}

/// CRC-32C (Castagnoli, reflected polynomial 0x82F63B78), as used by VHDX checksums.
pub(crate) fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82F6_3B78 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Invalid(format!("VHDX: {}", msg.into()))
}

/// CRC-32C of `b` with the 4-byte checksum field at offset 4 treated as zero.
fn checksum_with_zeroed_field(b: &[u8]) -> u32 {
    let mut copy = b.to_vec();
    copy[4..8].fill(0);
    crc32c(&copy)
}

/// A header's sequence number if its signature and checksum are valid.
fn parse_header(b: &[u8]) -> Option<u64> {
    if b.len() != HEADER_SIZE || &b[0..4] != b"head" || le32(b, 4) != checksum_with_zeroed_field(b) {
        return None;
    }
    Some(le64(b, 8))
}

pub(crate) struct VhdxDevice {
    file: FileDevice,
    block_size: u64,
    virtual_size: u64,
    sector_size: u32,
    chunk_ratio: u64,
    /// Raw BAT entries (payload and sector-bitmap entries interleaved).
    bat: Vec<u64>,
}

/// Open a VHDX image read-only. `read_only = false` is refused.
pub(crate) fn open(path: &Path, read_only: bool) -> Result<Box<dyn BlockDevice>> {
    if !read_only {
        return Err(Error::Invalid("VHDX is read-only".into()));
    }
    Ok(Box::new(VhdxDevice::open(path)?))
}

impl VhdxDevice {
    fn open(path: &Path) -> Result<Self> {
        let mut file = FileDevice::open(path, true)?;
        let flen = file.len();
        let mut id = [0u8; 8];
        if flen < REGION_TABLE_OFFSET + REGION_TABLE_SIZE as u64 {
            return Err(bad("file too small"));
        }
        file.read_at(0, &mut id)?;
        if &id != SIGNATURE {
            return Err(bad("missing file identifier"));
        }

        // Current header: valid checksum, highest sequence number.
        let mut best: Option<(u64, [u8; HEADER_SIZE])> = None;
        for off in [HEADER1_OFFSET, HEADER2_OFFSET] {
            let mut h = [0u8; HEADER_SIZE];
            file.read_at(off, &mut h)?;
            if let Some(seq) = parse_header(&h) {
                if best.as_ref().map_or(true, |(s, _)| seq > *s) {
                    best = Some((seq, h));
                }
            }
        }
        let (_, header) = best.ok_or_else(|| bad("no valid header"))?;
        let version = u16::from_le_bytes([header[66], header[67]]);
        if version != 1 {
            return Err(Error::Unsupported(format!("VHDX version {version}")));
        }

        // Region table.
        let mut rt = vec![0u8; REGION_TABLE_SIZE];
        file.read_at(REGION_TABLE_OFFSET, &mut rt)?;
        if &rt[0..4] != b"regi" || le32(&rt, 4) != checksum_with_zeroed_field(&rt) {
            return Err(bad("invalid region table"));
        }
        let count = le32(&rt, 8) as usize;
        if count > (REGION_TABLE_SIZE - 16) / 32 {
            return Err(bad("region table entry count out of range"));
        }
        let (mut bat_region, mut meta_region) = (None, None);
        for i in 0..count {
            let e = &rt[16 + i * 32..16 + (i + 1) * 32];
            let region = (le64(e, 16), le32(e, 24) as u64);
            if e[0..16] == GUID_BAT {
                bat_region = Some(region);
            } else if e[0..16] == GUID_METADATA {
                meta_region = Some(region);
            } else if le32(e, 28) & 1 != 0 {
                return Err(Error::Unsupported("VHDX has an unknown required region".into()));
            }
        }
        let (bat_off, bat_len) = bat_region.ok_or_else(|| bad("no BAT region"))?;
        let (meta_off, meta_len) = meta_region.ok_or_else(|| bad("no metadata region"))?;
        if meta_len < 64 * 1024 || meta_off.checked_add(meta_len).map_or(true, |e| e > flen) {
            return Err(bad("metadata region out of range"));
        }
        if bat_off.checked_add(bat_len).map_or(true, |e| e > flen) {
            return Err(bad("BAT region out of range"));
        }

        // Metadata table.
        let mut mt = vec![0u8; 64 * 1024];
        file.read_at(meta_off, &mut mt)?;
        if &mt[0..8] != b"metadata" {
            return Err(bad("invalid metadata table"));
        }
        let n = u16::from_le_bytes([mt[10], mt[11]]) as usize;
        if n > (mt.len() - 32) / 32 {
            return Err(bad("metadata entry count out of range"));
        }
        let item = |file: &mut FileDevice, id: &[u8; 16], size: usize| -> Result<Vec<u8>> {
            for i in 0..n {
                let e = &mt[32 + i * 32..32 + (i + 1) * 32];
                if e[0..16] == *id {
                    let (o, l) = (le32(e, 16) as u64, le32(e, 20) as usize);
                    if l < size || o + l as u64 > meta_len {
                        return Err(bad("metadata item out of range"));
                    }
                    let mut v = vec![0u8; size];
                    file.read_at(meta_off + o, &mut v)?;
                    return Ok(v);
                }
            }
            Err(bad("required metadata item missing"))
        };
        let fp = item(&mut file, &GUID_FILE_PARAMETERS, 8)?;
        let block_size = le32(&fp, 0) as u64;
        let fp_flags = le32(&fp, 4);
        if fp_flags & 2 != 0 {
            return Err(Error::Unsupported("differencing VHDX (has a parent)".into()));
        }
        if !block_size.is_power_of_two() || !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&block_size) {
            return Err(bad(format!("invalid block size {block_size}")));
        }
        let virtual_size = le64(&item(&mut file, &GUID_VIRTUAL_DISK_SIZE, 8)?, 0);
        let sector_size = le32(&item(&mut file, &GUID_LOGICAL_SECTOR_SIZE, 4)?, 0);
        if sector_size != 512 && sector_size != 4096 {
            return Err(bad(format!("invalid logical sector size {sector_size}")));
        }
        if virtual_size == 0 || virtual_size % sector_size as u64 != 0 {
            return Err(bad("invalid virtual disk size"));
        }

        // BAT.
        let chunk_ratio = ((1u64 << 23) * sector_size as u64) / block_size;
        let payload_blocks = virtual_size.div_ceil(block_size);
        let entries = payload_blocks + (payload_blocks - 1) / chunk_ratio;
        let bytes = entries * 8;
        if bytes > bat_len {
            return Err(bad("BAT region too small for the virtual disk"));
        }
        if bytes > MAX_BAT_BYTES {
            return Err(Error::Unsupported("VHDX BAT too large".into()));
        }
        let mut raw = vec![0u8; bytes as usize];
        file.read_at(bat_off, &mut raw)?;
        let bat = raw.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();

        Ok(VhdxDevice { file, block_size, virtual_size, sector_size, chunk_ratio, bat })
    }
}

impl BlockDevice for VhdxDevice {
    fn len(&self) -> u64 {
        self.virtual_size
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&e| e <= self.virtual_size)
            .ok_or_else(|| Error::Invalid(format!("read past end of VHDX (offset {offset}, len {})", buf.len())))?;
        let mut pos = offset;
        while pos < end {
            let block = pos / self.block_size;
            let within = pos % self.block_size;
            let n = (self.block_size - within).min(end - pos) as usize;
            let dst = &mut buf[(pos - offset) as usize..(pos - offset) as usize + n];
            let entry = self.bat[(block + block / self.chunk_ratio) as usize];
            match entry & 7 {
                STATE_NOT_PRESENT | STATE_UNDEFINED | STATE_ZERO | STATE_UNMAPPED => dst.fill(0),
                STATE_FULLY_PRESENT => {
                    let base = entry & !0xF_FFFF;
                    self.file.read_at(base + within, dst)?;
                }
                STATE_PARTIALLY_PRESENT => {
                    return Err(Error::Unsupported("differencing VHDX (partially present block)".into()))
                }
                s => return Err(bad(format!("invalid BAT state {s}"))),
            }
            pos += n as u64;
        }
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> Result<()> {
        Err(Error::Invalid("VHDX is read-only".into()))
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const MIB: u64 = 1 << 20;

    #[test]
    fn crc32c_known_vector() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 13 + 5) % 251) as u8 + 1).collect()
    }

    fn put_checksum(b: &mut [u8]) {
        b[4..8].fill(0);
        let c = crc32c(b);
        b[4..8].copy_from_slice(&c.to_le_bytes());
    }

    /// Minimal 2 MiB dynamic VHDX with 1 MiB blocks: block 0 FULLY_PRESENT (pattern),
    /// block 1 ZERO. Layout: BAT @1 MiB, metadata @2 MiB, payload @3 MiB.
    fn build_fixture(second_header_newer_but_corrupt: bool) -> Vec<u8> {
        let mut f = vec![0u8; (4 * MIB) as usize];
        f[0..8].copy_from_slice(SIGNATURE);

        let mut h = vec![0u8; HEADER_SIZE];
        h[0..4].copy_from_slice(b"head");
        h[8..16].copy_from_slice(&7u64.to_le_bytes());
        h[66..68].copy_from_slice(&1u16.to_le_bytes());
        put_checksum(&mut h);
        f[HEADER1_OFFSET as usize..HEADER1_OFFSET as usize + HEADER_SIZE].copy_from_slice(&h);
        if second_header_newer_but_corrupt {
            let mut h2 = h.clone();
            h2[8..16].copy_from_slice(&9u64.to_le_bytes()); // checksum now wrong
            f[HEADER2_OFFSET as usize..HEADER2_OFFSET as usize + HEADER_SIZE].copy_from_slice(&h2);
        }

        let mut rt = vec![0u8; REGION_TABLE_SIZE];
        rt[0..4].copy_from_slice(b"regi");
        rt[8..12].copy_from_slice(&2u32.to_le_bytes());
        for (i, (g, off, len)) in [(GUID_BAT, MIB, MIB), (GUID_METADATA, 2 * MIB, MIB)].iter().enumerate() {
            let e = &mut rt[16 + i * 32..16 + (i + 1) * 32];
            e[0..16].copy_from_slice(g);
            e[16..24].copy_from_slice(&off.to_le_bytes());
            e[24..28].copy_from_slice(&(*len as u32).to_le_bytes());
            e[28..32].copy_from_slice(&1u32.to_le_bytes());
        }
        put_checksum(&mut rt);
        f[REGION_TABLE_OFFSET as usize..REGION_TABLE_OFFSET as usize + REGION_TABLE_SIZE].copy_from_slice(&rt);

        // Metadata: table at 0, items from 64 KiB.
        let meta = 2 * MIB as usize;
        f[meta..meta + 8].copy_from_slice(b"metadata");
        f[meta + 10..meta + 12].copy_from_slice(&3u16.to_le_bytes());
        let items: [([u8; 16], u32, Vec<u8>); 3] = [
            (GUID_FILE_PARAMETERS, 0x10000, {
                let mut v = (MIB as u32).to_le_bytes().to_vec();
                v.extend_from_slice(&0u32.to_le_bytes());
                v
            }),
            (GUID_VIRTUAL_DISK_SIZE, 0x10008, (2 * MIB).to_le_bytes().to_vec()),
            (GUID_LOGICAL_SECTOR_SIZE, 0x10010, 512u32.to_le_bytes().to_vec()),
        ];
        for (i, (g, off, data)) in items.iter().enumerate() {
            let e = meta + 32 + i * 32;
            f[e..e + 16].copy_from_slice(g);
            f[e + 16..e + 20].copy_from_slice(&off.to_le_bytes());
            f[e + 20..e + 24].copy_from_slice(&(data.len() as u32).to_le_bytes());
            let d = meta + *off as usize;
            f[d..d + data.len()].copy_from_slice(data);
        }

        // BAT: entry 0 FULLY_PRESENT at 3 MiB, entry 1 ZERO.
        let bat = MIB as usize;
        let e0 = (3 * MIB) | STATE_FULLY_PRESENT;
        f[bat..bat + 8].copy_from_slice(&e0.to_le_bytes());
        f[bat + 8..bat + 16].copy_from_slice(&STATE_ZERO.to_le_bytes());

        let p = pattern(MIB as usize);
        f[3 * MIB as usize..4 * MIB as usize].copy_from_slice(&p);
        f
    }

    fn write_fixture(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::File::create(&path).unwrap().write_all(bytes).unwrap();
        path
    }

    #[test]
    fn guid_on_disk_order() {
        assert_eq!(
            GUID_BAT,
            [0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08]
        );
    }

    #[test]
    fn vhdx_read_fixture_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir, "disk.vhdx", &build_fixture(false));
        assert_eq!(crate::identify(&path).unwrap(), crate::ImageFormat::Vhdx);

        let mut dev = crate::open(&path, true).unwrap();
        assert_eq!(dev.len(), 2 * MIB);
        assert_eq!(dev.sector_size(), 512);
        assert!(dev.is_read_only());
        let p = pattern(MIB as usize);
        let mut b = vec![0u8; MIB as usize];
        dev.read_at(0, &mut b).unwrap();
        assert_eq!(b, p);
        dev.read_at(MIB, &mut b).unwrap();
        assert!(b.iter().all(|&x| x == 0));
        // a read crossing the block boundary
        let mut c = vec![0u8; 200];
        dev.read_at(MIB - 100, &mut c).unwrap();
        assert_eq!(&c[..100], &p[p.len() - 100..]);
        assert!(c[100..].iter().all(|&x| x == 0));
        // bounds and read-only
        assert!(dev.read_at(2 * MIB - 10, &mut [0u8; 20]).is_err());
        assert!(matches!(dev.write_at(0, &[1]), Err(Error::Invalid(m)) if m.contains("read-only")));
        assert!(crate::open(&path, false).is_err());
    }

    #[test]
    fn vhdx_ignores_newer_header_with_bad_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir, "disk.vhdx", &build_fixture(true));
        let mut dev = crate::open(&path, true).unwrap();
        let mut b = [0u8; 16];
        dev.read_at(0, &mut b).unwrap();
        assert_eq!(&b[..], &pattern(16)[..]);
    }

    #[test]
    fn vhdx_rejects_corrupt_and_differencing() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = build_fixture(false);
        f[HEADER1_OFFSET as usize + 20] ^= 1; // the only header is now corrupt
        let path = write_fixture(&dir, "bad.vhdx", &f);
        assert!(crate::open(&path, true).is_err());

        let mut f = build_fixture(false);
        f[2 * MIB as usize + 0x10000 + 4] = 2; // HasParent
        let path = write_fixture(&dir, "diff.vhdx", &f);
        assert!(matches!(crate::open(&path, true), Err(Error::Unsupported(_))));

        let mut f = build_fixture(false);
        let bat = MIB as usize;
        f[bat + 8..bat + 16].copy_from_slice(&((3 * MIB) | STATE_PARTIALLY_PRESENT).to_le_bytes());
        let path = write_fixture(&dir, "partial.vhdx", &f);
        let mut dev = crate::open(&path, true).unwrap();
        assert!(matches!(dev.read_at(MIB, &mut [0u8; 1]), Err(Error::Unsupported(_))));
    }
}
