//! Microsoft VHD (Virtual Hard Disk) images, fixed and dynamic, per the
//! "Virtual Hard Disk Image Format Specification" v1.0. All fields are big-endian.
//!
//! Fixed:   `[data (capacity bytes)][footer 512]`
//! Dynamic: `[footer copy 512][dynamic header 1024][BAT][bitmap 512 + block 2 MiB]...[footer 512]`

use crate::sparse::{BlockAllocator, BlockMap, ImageFile, SparseDevice};
use pm_core::{BlockDevice, Error, FileDevice, Result, SliceDevice};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const FOOTER_SIZE: u64 = 512;
const DYN_HEADER_OFFSET: u64 = 512;
const DYN_HEADER_SIZE: usize = 1024;
const BAT_OFFSET: u64 = 1536;
const BLOCK_SIZE: u64 = 0x20_0000;
const UNALLOCATED: u32 = 0xFFFF_FFFF;
pub(crate) const TYPE_FIXED: u32 = 2;
pub(crate) const TYPE_DYNAMIC: u32 = 3;
const TYPE_DIFFERENCING: u32 = 4;
/// Largest disk the VHD format supports (2040 GiB).
const MAX_CAPACITY: u64 = 2040 * 1024 * 1024 * 1024;
/// 2000-01-01T00:00:00Z as a Unix timestamp.
const VHD_EPOCH: u64 = 946_684_800;

fn put_u16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_be_bytes());
}
fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}
fn put_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_be_bytes());
}
fn get_u32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn get_u64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

/// Ones' complement of the byte sum of `b`, skipping the 4-byte checksum field at `field`.
pub(crate) fn checksum(b: &[u8], field: usize) -> u32 {
    let sum = b
        .iter()
        .enumerate()
        .filter(|(i, _)| *i < field || *i >= field + 4)
        .fold(0u32, |s, (_, &x)| s.wrapping_add(x as u32));
    !sum
}

/// CHS geometry from the VHD specification (appendix "CHS Calculation").
pub(crate) fn geometry(capacity: u64) -> (u16, u8, u8) {
    let mut total = capacity / 512;
    if total > 65535 * 16 * 255 {
        total = 65535 * 16 * 255;
    }
    let (spt, heads, cth);
    if total >= 65535 * 16 * 63 {
        spt = 255;
        heads = 16;
        cth = total / spt;
    } else {
        let mut s = 17;
        let mut c = total / s;
        let mut h = (c + 1023) / 1024;
        if h < 4 {
            h = 4;
        }
        if c >= h * 1024 || h > 16 {
            s = 31;
            h = 16;
            c = total / s;
        }
        if c >= h * 1024 {
            s = 63;
            h = 16;
            c = total / s;
        }
        spt = s;
        heads = h;
        cth = c;
    }
    ((cth / heads) as u16, heads as u8, spt as u8)
}

fn timestamp() -> u32 {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(VHD_EPOCH);
    unix.saturating_sub(VHD_EPOCH) as u32
}

fn build_footer(capacity: u64, disk_type: u32, data_offset: u64) -> [u8; 512] {
    let mut f = [0u8; 512];
    f[0..8].copy_from_slice(b"conectix");
    put_u32(&mut f, 8, 0x0000_0002); // features: reserved bit always set
    put_u32(&mut f, 12, 0x0001_0000); // file format version
    put_u64(&mut f, 16, data_offset);
    put_u32(&mut f, 24, timestamp());
    f[28..32].copy_from_slice(b"pman");
    put_u32(&mut f, 32, 0x0001_0000); // creator version
    f[36..40].copy_from_slice(b"Wi2k");
    put_u64(&mut f, 40, capacity); // original size
    put_u64(&mut f, 48, capacity); // current size
    let (c, h, s) = geometry(capacity);
    put_u16(&mut f, 56, c);
    f[58] = h;
    f[59] = s;
    put_u32(&mut f, 60, disk_type);
    f[68..84].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    f[84] = 0; // saved state
    let sum = checksum(&f, 64);
    put_u32(&mut f, 64, sum);
    f
}

fn build_dyn_header(max_entries: u32) -> [u8; DYN_HEADER_SIZE] {
    let mut h = [0u8; DYN_HEADER_SIZE];
    h[0..8].copy_from_slice(b"cxsparse");
    put_u64(&mut h, 8, u64::MAX); // data offset: unused
    put_u64(&mut h, 16, BAT_OFFSET);
    put_u32(&mut h, 24, 0x0001_0000);
    put_u32(&mut h, 28, max_entries);
    put_u32(&mut h, 32, BLOCK_SIZE as u32);
    // parent unique id / timestamp / name / locators stay zero (no parent)
    let sum = checksum(&h, 36);
    put_u32(&mut h, 36, sum);
    h
}

fn check_capacity(capacity: u64) -> Result<()> {
    if capacity > MAX_CAPACITY {
        return Err(Error::Unsupported(format!(
            "VHD images are limited to 2040 GiB (requested {capacity} bytes)"
        )));
    }
    Ok(())
}

/// Bytes of the sector bitmap preceding each data block, padded to a sector.
fn bitmap_size(block_size: u64) -> u64 {
    (block_size / 512).div_ceil(8).div_ceil(512) * 512
}

pub(crate) fn create_fixed(path: &Path, capacity: u64) -> Result<Box<dyn BlockDevice>> {
    check_capacity(capacity)?;
    let mut dev = FileDevice::create(path, capacity + FOOTER_SIZE)?;
    dev.write_at(capacity, &build_footer(capacity, TYPE_FIXED, u64::MAX))?;
    dev.flush()?;
    Ok(Box::new(SliceDevice::new(dev, 0, capacity)?))
}

pub(crate) fn create_dynamic(path: &Path, capacity: u64) -> Result<Box<dyn BlockDevice>> {
    check_capacity(capacity)?;
    let max_entries = capacity.div_ceil(BLOCK_SIZE);
    let bat_bytes = (max_entries * 4).div_ceil(512) * 512;
    let footer = build_footer(capacity, TYPE_DYNAMIC, DYN_HEADER_OFFSET);
    let mut file = ImageFile::create(path)?;
    file.write_at(0, &footer)?;
    file.write_at(DYN_HEADER_OFFSET, &build_dyn_header(max_entries as u32))?;
    file.write_at(BAT_OFFSET, &vec![0xFFu8; bat_bytes as usize])?;
    let footer_pos = BAT_OFFSET + bat_bytes;
    file.write_at(footer_pos, &footer)?;
    file.flush()?;
    Ok(Box::new(SparseDevice {
        capacity,
        map: BlockMap::new(BLOCK_SIZE, max_entries),
        alloc: VhdAlloc {
            file,
            footer,
            footer_pos,
            bat_offset: BAT_OFFSET,
            block_size: BLOCK_SIZE,
            bitmap_size: bitmap_size(BLOCK_SIZE),
        },
    }))
}

/// Read and validate the footer at the end of the file.
pub(crate) fn read_footer(file: &mut ImageFile) -> Result<[u8; 512]> {
    if file.len() < FOOTER_SIZE {
        return Err(Error::Parse("file too small for a VHD footer".into()));
    }
    let mut f = [0u8; 512];
    file.read_at(file.len() - FOOTER_SIZE, &mut f)?;
    if &f[0..8] != b"conectix" {
        return Err(Error::Parse("missing VHD footer cookie".into()));
    }
    if get_u32(&f, 64) != checksum(&f, 64) {
        return Err(Error::Parse("VHD footer checksum mismatch".into()));
    }
    Ok(f)
}

pub(crate) fn disk_type(footer: &[u8; 512]) -> u32 {
    get_u32(footer, 60)
}

pub(crate) fn open(path: &Path, read_only: bool) -> Result<Box<dyn BlockDevice>> {
    let mut file = ImageFile::open(path, read_only)?;
    let footer = read_footer(&mut file)?;
    let capacity = get_u64(&footer, 48);
    match disk_type(&footer) {
        TYPE_FIXED => {
            if capacity > file.len() - FOOTER_SIZE {
                return Err(Error::Parse("fixed VHD is shorter than its size".into()));
            }
            drop(file);
            let dev = FileDevice::open(path, read_only)?;
            Ok(Box::new(SliceDevice::new(dev, 0, capacity)?))
        }
        TYPE_DYNAMIC => open_dynamic(file, footer, capacity),
        TYPE_DIFFERENCING => Err(Error::Unsupported("differencing VHD".into())),
        t => Err(Error::Parse(format!("unknown VHD disk type {t}"))),
    }
}

fn open_dynamic(mut file: ImageFile, footer: [u8; 512], capacity: u64) -> Result<Box<dyn BlockDevice>> {
    let hdr_off = get_u64(&footer, 16);
    let mut h = [0u8; DYN_HEADER_SIZE];
    file.read_at(hdr_off, &mut h)?;
    if &h[0..8] != b"cxsparse" {
        return Err(Error::Parse("missing VHD dynamic header cookie".into()));
    }
    if get_u32(&h, 36) != checksum(&h, 36) {
        return Err(Error::Parse("VHD dynamic header checksum mismatch".into()));
    }
    let bat_offset = get_u64(&h, 16);
    let max_entries = get_u32(&h, 28) as u64;
    let block_size = get_u32(&h, 32) as u64;
    if block_size == 0 || block_size % 512 != 0 {
        return Err(Error::Parse(format!("invalid VHD block size {block_size}")));
    }
    if max_entries * block_size < capacity {
        return Err(Error::Parse("VHD BAT does not cover the disk size".into()));
    }
    let bitmap = bitmap_size(block_size);
    let mut bat = vec![0u8; (max_entries * 4) as usize];
    file.read_at(bat_offset, &mut bat)?;
    let mut map = BlockMap::new(block_size, max_entries);
    for (i, e) in bat.chunks_exact(4).enumerate() {
        let sector = u32::from_be_bytes(e.try_into().unwrap());
        if sector != UNALLOCATED {
            let data = sector as u64 * 512 + bitmap;
            if data + block_size > file.len() {
                return Err(Error::Parse(format!("VHD block {i} lies beyond end of file")));
            }
            map.set(i as u64, data);
        }
    }
    let footer_pos = file.len() - FOOTER_SIZE;
    Ok(Box::new(SparseDevice {
        capacity,
        map,
        alloc: VhdAlloc {
            file,
            footer,
            footer_pos,
            bat_offset,
            block_size,
            bitmap_size: bitmap,
        },
    }))
}

/// Dynamic VHD allocation: new blocks overwrite the footer at the end of the file and
/// the footer is re-appended behind them.
struct VhdAlloc {
    file: ImageFile,
    footer: [u8; 512],
    footer_pos: u64,
    bat_offset: u64,
    block_size: u64,
    bitmap_size: u64,
}

impl BlockAllocator for VhdAlloc {
    fn file(&mut self) -> &mut ImageFile {
        &mut self.file
    }

    fn is_read_only(&self) -> bool {
        self.file.is_read_only()
    }

    fn place_block(&mut self, _idx: u64) -> Result<u64> {
        let start = self.footer_pos;
        if start / 512 >= UNALLOCATED as u64 {
            return Err(Error::Unsupported("VHD file exceeds 2 TiB of sectors".into()));
        }
        // Every sector of the block is written (zero padded), so mark all present.
        self.file.write_at(start, &vec![0xFFu8; self.bitmap_size as usize])?;
        Ok(start + self.bitmap_size)
    }

    fn on_allocate(&mut self, idx: u64, file_off: u64) -> Result<()> {
        let block_start = file_off - self.bitmap_size;
        self.footer_pos = file_off + self.block_size;
        self.file.write_at(self.footer_pos, &self.footer)?;
        self.file.write_at(
            self.bat_offset + idx * 4,
            &((block_start / 512) as u32).to_be_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_matches_spec_examples() {
        // 40 MiB: small disk branch (17 sectors per track)
        assert_eq!(geometry(40 << 20), (963, 5, 17));
        // 4 GiB: 63 sectors per track branch
        assert_eq!(geometry(4 << 30), (8322, 16, 63));
        // beyond 65535*16*63 sectors: 255 sectors per track
        assert_eq!(geometry(127 << 30), (65278, 16, 255));
        // clamped
        assert_eq!(geometry(2000 << 30), (65535, 16, 255));
    }

    #[test]
    fn checksum_is_ones_complement_of_sum() {
        let mut b = [0u8; 16];
        b[0] = 1;
        b[1] = 2;
        b[8] = 0xAA; // inside the skipped field
        assert_eq!(checksum(&b, 8), !3u32);
    }
}
