//! VMware VMDK images: `monolithicFlat` (text descriptor + raw `-flat` extent) and
//! `monolithicSparse` (hosted sparse extent with embedded descriptor), per the
//! "Virtual Disk Format 5.0" specification. Sparse extent fields are little-endian.
//!
//! Sparse layout (sectors): `[header 1][descriptor 20][rGD][rGTs][GD][GTs] pad to 1 MiB
//! = overHead`, then 64 KiB grains appended as they are written.

use crate::sparse::{BlockAllocator, BlockMap, ImageFile, SparseDevice};
use pm_core::{BlockDevice, Error, FileDevice, Result, SliceDevice};
use std::path::{Path, PathBuf};

pub(crate) const SPARSE_MAGIC: &[u8; 4] = b"KDMV";
pub(crate) const DESCRIPTOR_SIGNATURE: &str = "# Disk DescriptorFile";
const VERSION: u32 = 1;
/// Bit 0: valid new-line detection test; bit 1: redundant grain table in use.
const FLAGS: u32 = 3;
const FLAG_REDUNDANT_GT: u32 = 1 << 1;
const FLAG_ZEROED_GTE: u32 = 1 << 2;
const FLAG_COMPRESSED: u32 = 1 << 16;
const GRAIN_SECTORS: u64 = 128;
const DESCRIPTOR_OFFSET: u64 = 1;
const DESCRIPTOR_SECTORS: u64 = 20;
const GTES_PER_GT: u64 = 512;
const GD_AT_END: u64 = u64::MAX;
/// Largest grain accepted on open (qemu's bound: 0x200000 sectors = 1 GiB).
const MAX_GRAIN_SECTORS: u64 = 0x20_0000;
/// Text descriptors are tiny; refuse to slurp anything bigger than this.
const MAX_DESCRIPTOR_FILE: u64 = 1 << 20;
const ZERO_CHUNK: usize = 1 << 20;

fn put_u32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
fn get_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn get_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn random_cid() -> u32 {
    let b = *uuid::Uuid::new_v4().as_bytes();
    match u32::from_le_bytes([b[0], b[1], b[2], b[3]]) {
        u32::MAX => 0xFFFF_FFFE, // ffffffff means "no parent"
        c => c,
    }
}

/// Text descriptor shared by both variants.
fn descriptor(create_type: &str, extent_line: &str, sectors: u64) -> String {
    let cylinders = sectors.div_ceil(16 * 63);
    format!(
        "# Disk DescriptorFile\n\
         version=1\n\
         encoding=\"UTF-8\"\n\
         CID={cid:08x}\n\
         parentCID=ffffffff\n\
         createType=\"{create_type}\"\n\
         \n\
         # Extent description\n\
         {extent_line}\n\
         \n\
         # The Disk Data Base\n\
         #DDB\n\
         \n\
         ddb.virtualHWVersion = \"4\"\n\
         ddb.geometry.cylinders = \"{cylinders}\"\n\
         ddb.geometry.heads = \"16\"\n\
         ddb.geometry.sectors = \"63\"\n\
         ddb.adapterType = \"lsilogic\"\n",
        cid = random_cid(),
    )
}

fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| Error::Invalid(format!("bad image path {}", path.display())))
}

// ------------------------------------------------------------------ flat

pub(crate) fn create_flat(path: &Path, capacity: u64) -> Result<Box<dyn BlockDevice>> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Invalid(format!("bad image path {}", path.display())))?;
    let flat_name = format!("{stem}-flat.vmdk");
    let flat_path = path.with_file_name(&flat_name);
    let dev = FileDevice::create(&flat_path, capacity)?;
    let sectors = capacity / 512;
    let text = descriptor(
        "monolithicFlat",
        &format!("RW {sectors} FLAT \"{flat_name}\" 0"),
        sectors,
    );
    std::fs::write(path, text).map_err(|e| Error::io(0, 0, e))?;
    Ok(Box::new(dev))
}

struct Extent {
    access: String,
    sectors: u64,
    kind: String,
    file: String,
    offset: u64,
}

fn parse_extent(line: &str) -> Result<Extent> {
    let bad = || Error::Parse(format!("malformed VMDK extent line: {line}"));
    let q1 = line.find('"').ok_or_else(bad)?;
    let q2 = line.rfind('"').filter(|&q| q > q1).ok_or_else(bad)?;
    let head: Vec<&str> = line[..q1].split_whitespace().collect();
    if head.len() != 3 {
        return Err(bad());
    }
    let offset = match line[q2 + 1..].trim() {
        "" => 0,
        s => s.parse().map_err(|_| bad())?,
    };
    Ok(Extent {
        access: head[0].to_string(),
        sectors: head[1].parse().map_err(|_| bad())?,
        kind: head[2].to_string(),
        file: line[q1 + 1..q2].to_string(),
        offset,
    })
}

/// Open a text descriptor (`createType="monolithicFlat"`) and its single FLAT extent,
/// resolved relative to the descriptor's directory.
pub(crate) fn open_flat(path: &Path, read_only: bool) -> Result<Box<dyn BlockDevice>> {
    let size = std::fs::metadata(path).map_err(|e| Error::io(0, 0, e))?.len();
    if size > MAX_DESCRIPTOR_FILE {
        return Err(Error::Parse(format!("VMDK descriptor file too large ({size} bytes)")));
    }
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(0, 0, e))?;
    if !text.starts_with(DESCRIPTOR_SIGNATURE) {
        return Err(Error::Parse("not a VMDK descriptor".into()));
    }
    let extents: Vec<Extent> = text
        .lines()
        .map(str::trim)
        .filter(|l| ["RW ", "RDONLY ", "NOACCESS "].iter().any(|p| l.starts_with(p)))
        .map(parse_extent)
        .collect::<Result<_>>()?;
    let [ext] = extents.as_slice() else {
        return Err(Error::Unsupported(format!(
            "VMDK descriptors with {} extents",
            extents.len()
        )));
    };
    if ext.kind != "FLAT" {
        return Err(Error::Unsupported(format!("VMDK extent type {}", ext.kind)));
    }
    if ext.access == "NOACCESS" {
        return Err(Error::Unsupported("VMDK extent marked NOACCESS".into()));
    }
    let extent_path = {
        let p = PathBuf::from(&ext.file);
        if p.is_absolute() {
            p
        } else {
            path.parent().unwrap_or(Path::new(".")).join(p)
        }
    };
    let ro = read_only || ext.access == "RDONLY";
    let dev = FileDevice::open(&extent_path, ro)?;
    let range = ext.offset.checked_mul(512).zip(ext.sectors.checked_mul(512));
    let Some((off, len)) = range.filter(|(o, l)| o.checked_add(*l).is_some()) else {
        return Err(Error::Parse("VMDK extent offset/size overflow".into()));
    };
    if off + len > dev.len() {
        return Err(Error::Parse(format!(
            "flat extent {} is shorter than the descriptor says",
            extent_path.display()
        )));
    }
    Ok(Box::new(SliceDevice::new(dev, off, len)?))
}

// ------------------------------------------------------------------ sparse

struct SparseLayout {
    num_gts: u64,
    rgd: u64,
    rgt_start: u64,
    gd: u64,
    gt_start: u64,
    overhead: u64,
}

impl SparseLayout {
    fn new(sectors: u64) -> Self {
        let num_gts = sectors.div_ceil(GRAIN_SECTORS * GTES_PER_GT);
        let gd_sectors = (num_gts * 4).div_ceil(512);
        let gt_sectors = GTES_PER_GT * 4 / 512;
        let rgd = DESCRIPTOR_OFFSET + DESCRIPTOR_SECTORS;
        let rgt_start = rgd + gd_sectors;
        let gd = rgt_start + num_gts * gt_sectors;
        let gt_start = gd + gd_sectors;
        let end = gt_start + num_gts * gt_sectors;
        SparseLayout {
            num_gts,
            rgd,
            rgt_start,
            gd,
            gt_start,
            overhead: end.div_ceil(2048) * 2048,
        }
    }
}

fn write_zeros(file: &mut ImageFile, mut off: u64, end: u64) -> Result<()> {
    let zeros = vec![0u8; ZERO_CHUNK];
    while off < end {
        let n = (end - off).min(ZERO_CHUNK as u64) as usize;
        file.write_at(off, &zeros[..n])?;
        off += n as u64;
    }
    Ok(())
}

fn directory_bytes(num_gts: u64, first_gt: u64) -> Vec<u8> {
    let gt_sectors = GTES_PER_GT * 4 / 512;
    let mut b = vec![0u8; (num_gts * 4) as usize];
    for i in 0..num_gts {
        put_u32(&mut b, i as usize * 4, (first_gt + i * gt_sectors) as u32);
    }
    b
}

pub(crate) fn create_sparse(path: &Path, capacity: u64) -> Result<Box<dyn BlockDevice>> {
    let sectors = capacity / 512;
    let l = SparseLayout::new(sectors);
    if l.overhead + sectors.div_ceil(GRAIN_SECTORS) * GRAIN_SECTORS > u32::MAX as u64 {
        return Err(Error::Unsupported(
            "sparse VMDK extents are limited to 2 TiB".into(),
        ));
    }
    let mut h = [0u8; 512];
    h[0..4].copy_from_slice(SPARSE_MAGIC);
    put_u32(&mut h, 4, VERSION);
    put_u32(&mut h, 8, FLAGS);
    put_u64(&mut h, 12, sectors);
    put_u64(&mut h, 20, GRAIN_SECTORS);
    put_u64(&mut h, 28, DESCRIPTOR_OFFSET);
    put_u64(&mut h, 36, DESCRIPTOR_SECTORS);
    put_u32(&mut h, 44, GTES_PER_GT as u32);
    put_u64(&mut h, 48, l.rgd);
    put_u64(&mut h, 56, l.gd);
    put_u64(&mut h, 64, l.overhead);
    h[72] = 0; // uncleanShutdown
    h[73] = b'\n';
    h[74] = b' ';
    h[75] = b'\r';
    h[76] = b'\n';
    // compressAlgorithm (77..79) = 0, pad zero

    let text = descriptor(
        "monolithicSparse",
        &format!("RW {sectors} SPARSE \"{}\"", file_name(path)?),
        sectors,
    );
    let mut desc = text.into_bytes();
    if desc.len() > (DESCRIPTOR_SECTORS * 512) as usize {
        return Err(Error::Invalid("VMDK descriptor too long".into()));
    }
    desc.resize((DESCRIPTOR_SECTORS * 512) as usize, 0);

    let mut file = ImageFile::create(path)?;
    write_zeros(&mut file, 0, l.overhead * 512)?;
    file.write_at(0, &h)?;
    file.write_at(DESCRIPTOR_OFFSET * 512, &desc)?;
    file.write_at(l.rgd * 512, &directory_bytes(l.num_gts, l.rgt_start))?;
    file.write_at(l.gd * 512, &directory_bytes(l.num_gts, l.gt_start))?;
    file.flush()?;
    let gt_sectors = GTES_PER_GT * 4 / 512;
    Ok(Box::new(SparseDevice {
        capacity,
        map: BlockMap::new(GRAIN_SECTORS * 512, sectors.div_ceil(GRAIN_SECTORS)),
        alloc: VmdkAlloc {
            file,
            gd: (0..l.num_gts).map(|i| l.gt_start + i * gt_sectors).collect(),
            rgd: Some((0..l.num_gts).map(|i| l.rgt_start + i * gt_sectors).collect()),
            gtes_per_gt: GTES_PER_GT,
        },
    }))
}

/// Read `n` grain-directory entries at `sector`, refusing anything outside the file
/// before allocating.
fn read_directory(file: &mut ImageFile, sector: u64, n: u64) -> Result<Vec<u64>> {
    let off = sector.checked_mul(512);
    let len = n.checked_mul(4);
    match off.zip(len).and_then(|(o, l)| o.checked_add(l)) {
        Some(end) if end <= file.len() => {}
        _ => return Err(Error::Parse("VMDK grain directory lies beyond end of file".into())),
    }
    let mut b = vec![0u8; (n * 4) as usize];
    file.read_at(sector * 512, &mut b)?;
    Ok(b.chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()) as u64)
        .collect())
}

pub(crate) fn open_sparse(path: &Path, read_only: bool) -> Result<Box<dyn BlockDevice>> {
    let mut file = ImageFile::open(path, read_only)?;
    let mut h = [0u8; 512];
    file.read_at(0, &mut h)?;
    if &h[0..4] != SPARSE_MAGIC {
        return Err(Error::Parse("missing VMDK sparse magic".into()));
    }
    let version = get_u32(&h, 4);
    if !(1..=3).contains(&version) {
        return Err(Error::Unsupported(format!("VMDK sparse version {version}")));
    }
    let flags = get_u32(&h, 8);
    let compress = u16::from_le_bytes([h[77], h[78]]);
    if compress != 0 || flags & FLAG_COMPRESSED != 0 {
        return Err(Error::Unsupported("compressed (streamOptimized) VMDK".into()));
    }
    let sectors = get_u64(&h, 12);
    let grain = get_u64(&h, 20);
    let gtes = get_u32(&h, 44) as u64;
    let rgd_off = get_u64(&h, 48);
    let gd_off = get_u64(&h, 56);
    if !grain.is_power_of_two() || grain > MAX_GRAIN_SECTORS {
        return Err(Error::Parse(format!("invalid VMDK grain size {grain}")));
    }
    if gtes == 0 || gtes > GTES_PER_GT {
        return Err(Error::Parse(format!("invalid VMDK numGTEsPerGT {gtes}")));
    }
    if gd_off == GD_AT_END || gd_off == 0 {
        return Err(Error::Parse("invalid VMDK grain directory offset".into()));
    }
    let capacity = sectors
        .checked_mul(512)
        .ok_or_else(|| Error::Parse(format!("VMDK capacity {sectors} sectors overflows")))?;
    // grain and gtes are non-zero (checked above)
    let n_grains = sectors.div_ceil(grain);
    let num_gts = n_grains.div_ceil(gtes);
    let gd = read_directory(&mut file, gd_off, num_gts)?;
    let rgd = if flags & FLAG_REDUNDANT_GT != 0 && rgd_off != 0 {
        Some(read_directory(&mut file, rgd_off, num_gts)?)
    } else {
        None
    };
    let mut map = BlockMap::new(grain * 512, n_grains);
    let mut gt = vec![0u8; (gtes * 4) as usize];
    for (g, &gt_sector) in gd.iter().enumerate() {
        if gt_sector == 0 {
            continue; // whole table unallocated
        }
        file.read_at(gt_sector * 512, &mut gt)?;
        for (e, c) in gt.chunks_exact(4).enumerate() {
            let idx = g as u64 * gtes + e as u64;
            let v = u32::from_le_bytes(c.try_into().unwrap()) as u64;
            if idx >= n_grains || v == 0 || (v == 1 && flags & FLAG_ZEROED_GTE != 0) {
                continue;
            }
            // v < 2^32 and grain <= 2^21: no overflow
            if (v + grain) * 512 > file.len() {
                return Err(Error::Parse(format!("VMDK grain {idx} lies beyond end of file")));
            }
            map.set(idx, v * 512);
        }
    }
    Ok(Box::new(SparseDevice {
        capacity,
        map,
        alloc: VmdkAlloc {
            file,
            gd,
            rgd,
            gtes_per_gt: gtes,
        },
    }))
}

/// Grains are appended at the end of the file; their sector offset is recorded in the
/// primary and (if present) redundant grain table.
struct VmdkAlloc {
    file: ImageFile,
    gd: Vec<u64>,
    rgd: Option<Vec<u64>>,
    gtes_per_gt: u64,
}

impl BlockAllocator for VmdkAlloc {
    fn file(&mut self) -> &mut ImageFile {
        &mut self.file
    }

    fn is_read_only(&self) -> bool {
        self.file.is_read_only()
    }

    fn place_block(&mut self, idx: u64) -> Result<u64> {
        if self.gd[(idx / self.gtes_per_gt) as usize] == 0 {
            return Err(Error::Unsupported(
                "writing to a VMDK grain table that is not preallocated".into(),
            ));
        }
        let off = self.file.len().div_ceil(512) * 512;
        if off / 512 > u32::MAX as u64 {
            return Err(Error::Unsupported("sparse VMDK extent exceeds 2 TiB".into()));
        }
        Ok(off)
    }

    fn on_allocate(&mut self, idx: u64, file_off: u64) -> Result<()> {
        let (g, e) = ((idx / self.gtes_per_gt) as usize, idx % self.gtes_per_gt);
        let value = ((file_off / 512) as u32).to_le_bytes();
        if let Some(rgd) = &self.rgd {
            if rgd[g] != 0 {
                self.file.write_at(rgd[g] * 512 + e * 4, &value)?;
            }
        }
        self.file.write_at(self.gd[g] * 512 + e * 4, &value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_mib_aligned_and_ordered() {
        let l = SparseLayout::new(37 * 2048 + 5);
        assert_eq!(l.num_gts, 2);
        assert_eq!(l.rgd, 21);
        assert_eq!(l.rgt_start, 22);
        assert_eq!(l.gd, 30);
        assert_eq!(l.gt_start, 31);
        assert_eq!(l.overhead, 2048);
    }

    #[test]
    fn extent_line_parsing() {
        let e = parse_extent("RW 2048 FLAT \"my disk-flat.vmdk\" 0").unwrap();
        assert_eq!((e.access.as_str(), e.sectors, e.kind.as_str()), ("RW", 2048, "FLAT"));
        assert_eq!(e.file, "my disk-flat.vmdk");
        assert_eq!(e.offset, 0);
        let e = parse_extent("RDONLY 10 SPARSE \"x.vmdk\"").unwrap();
        assert_eq!(e.offset, 0);
        assert!(parse_extent("RW FLAT x").is_err());
    }
}
