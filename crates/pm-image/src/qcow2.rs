//! QEMU qcow2 version 3 images (big-endian), per `docs/interop/qcow2.txt`.
//!
//! Created layout (64 KiB clusters): cluster 0 header, cluster 1.. L1 table, then the
//! refcount table (1 cluster), the first refcount block, and from there L2 tables,
//! refcount blocks and data clusters appended as they are needed. Every cluster in use
//! has refcount 1; no compression, encryption, snapshots, backing file or extensions.

use crate::sparse::{BlockAllocator, BlockMap, ImageFile, SparseDevice};
use pm_core::{BlockDevice, Error, Result};
use std::path::Path;

pub(crate) const MAGIC: &[u8; 4] = b"QFI\xfb";
const CLUSTER_BITS: u32 = 16;
const HEADER_LENGTH: u32 = 112;
const REFCOUNT_ORDER: u32 = 4;
const COPIED: u64 = 1 << 63;
const COMPRESSED: u64 = 1 << 62;
const ZERO_FLAG: u64 = 1;
const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
/// qemu's limits for tables read from a header: 32 MiB of L1, 8 MiB of refcount table.
const MAX_L1_BYTES: u64 = 32 << 20;
const MAX_REFTABLE_BYTES: u64 = 8 << 20;
const AUTOCLEAR_OFFSET: u64 = 88;
/// One refcount-table cluster addresses 8192 refblocks x 32768 clusters x 64 KiB.
const MAX_CAPACITY: u64 = 16 << 40;

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
fn be_u64s(b: &[u8]) -> Vec<u64> {
    b.chunks_exact(8)
        .map(|c| u64::from_be_bytes(c.try_into().unwrap()))
        .collect()
}

pub(crate) fn create(path: &Path, capacity: u64) -> Result<Box<dyn BlockDevice>> {
    if capacity > MAX_CAPACITY {
        return Err(Error::Unsupported(format!(
            "qcow2 images are limited to 16 TiB here (requested {capacity} bytes)"
        )));
    }
    let cs = 1u64 << CLUSTER_BITS;
    let l2_entries = cs / 8;
    let l1_size = capacity.div_ceil(cs * l2_entries);
    let l1_clusters = (l1_size * 8).div_ceil(cs).max(1);
    let l1_offset = cs;
    let rt_offset = l1_offset + l1_clusters * cs;
    let rb_offset = rt_offset + cs;
    let next_free = rb_offset + cs;

    let mut h = vec![0u8; cs as usize];
    h[0..4].copy_from_slice(MAGIC);
    put_u32(&mut h, 4, 3); // version
    put_u64(&mut h, 8, 0); // backing_file_offset
    put_u32(&mut h, 16, 0); // backing_file_size
    put_u32(&mut h, 20, CLUSTER_BITS);
    put_u64(&mut h, 24, capacity);
    put_u32(&mut h, 32, 0); // crypt_method
    put_u32(&mut h, 36, l1_size as u32);
    put_u64(&mut h, 40, l1_offset);
    put_u64(&mut h, 48, rt_offset);
    put_u32(&mut h, 56, 1); // refcount_table_clusters
    put_u32(&mut h, 60, 0); // nb_snapshots
    put_u64(&mut h, 64, 0); // snapshots_offset
    put_u64(&mut h, 72, 0); // incompatible_features
    put_u64(&mut h, 80, 0); // compatible_features
    put_u64(&mut h, 88, 0); // autoclear_features
    put_u32(&mut h, 96, REFCOUNT_ORDER);
    put_u32(&mut h, 100, HEADER_LENGTH);
    // 104: compression_type 0 (zlib) + padding; 112: header extension end marker (zero)

    let mut file = ImageFile::create(path)?;
    file.write_at(0, &h)?;
    let zero = vec![0u8; cs as usize];
    for i in 0..l1_clusters {
        file.write_at(l1_offset + i * cs, &zero)?;
    }
    let mut rt = vec![0u8; cs as usize];
    put_u64(&mut rt, 0, rb_offset);
    file.write_at(rt_offset, &rt)?;
    let mut rb = vec![0u8; cs as usize];
    for c in 0..next_free / cs {
        rb[c as usize * 2..c as usize * 2 + 2].copy_from_slice(&1u16.to_be_bytes());
    }
    file.write_at(rb_offset, &rb)?;
    file.flush()?;

    let mut rt_entries = vec![0u64; (cs / 8) as usize];
    rt_entries[0] = rb_offset;
    Ok(Box::new(SparseDevice {
        capacity,
        map: BlockMap::new(cs, capacity.div_ceil(cs)),
        alloc: Qcow2Alloc {
            file,
            cs,
            l1: vec![0; l1_size as usize],
            l1_offset,
            rt: rt_entries,
            rt_offset,
            next_free,
        },
    }))
}

pub(crate) fn open(path: &Path, read_only: bool) -> Result<Box<dyn BlockDevice>> {
    let mut file = ImageFile::open(path, read_only)?;
    if file.len() < 72 {
        return Err(Error::Parse("file too small for a qcow2 header".into()));
    }
    let mut h = vec![0u8; file.len().min(104) as usize];
    file.read_at(0, &mut h)?;
    if &h[0..4] != MAGIC {
        return Err(Error::Parse("missing qcow2 magic".into()));
    }
    let version = get_u32(&h, 4);
    if version != 2 && version != 3 {
        return Err(Error::Unsupported(format!("qcow2 version {version}")));
    }
    if version == 3 && h.len() < 104 {
        return Err(Error::Parse("truncated qcow2 v3 header".into()));
    }
    if get_u64(&h, 8) != 0 {
        return Err(Error::Unsupported("qcow2 with a backing file".into()));
    }
    let cluster_bits = get_u32(&h, 20);
    if !(9..=21).contains(&cluster_bits) {
        return Err(Error::Parse(format!("invalid qcow2 cluster_bits {cluster_bits}")));
    }
    let capacity = get_u64(&h, 24);
    if get_u32(&h, 32) != 0 {
        return Err(Error::Unsupported("encrypted qcow2".into()));
    }
    let l1_size = get_u32(&h, 36) as u64;
    let l1_offset = get_u64(&h, 40);
    let rt_offset = get_u64(&h, 48);
    let rt_clusters = get_u32(&h, 56) as u64;
    let nb_snapshots = get_u32(&h, 60);
    let (incompatible, refcount_order) = if version == 3 {
        (get_u64(&h, 72), get_u32(&h, 96))
    } else {
        (0, REFCOUNT_ORDER)
    };
    if incompatible != 0 {
        return Err(Error::Unsupported(format!(
            "qcow2 incompatible features {incompatible:#x}"
        )));
    }
    if !read_only && (refcount_order != REFCOUNT_ORDER || nb_snapshots != 0) {
        return Err(Error::Unsupported(
            "writing qcow2 images with snapshots or non-16-bit refcounts".into(),
        ));
    }
    let cs = 1u64 << cluster_bits;
    let l2_entries = cs / 8;
    let n_clusters = capacity.div_ceil(cs);
    if l1_size > MAX_L1_BYTES / 8 {
        return Err(Error::Parse(format!("qcow2 L1 table too large ({l1_size} entries)")));
    }
    if rt_clusters == 0 || rt_clusters > MAX_REFTABLE_BYTES / cs {
        return Err(Error::Parse(format!(
            "invalid qcow2 refcount table size ({rt_clusters} clusters)"
        )));
    }
    // l1_size <= 2^22 and l2_entries <= 2^18: no overflow
    if l1_size * l2_entries < n_clusters {
        return Err(Error::Parse("qcow2 L1 table does not cover the disk size".into()));
    }
    let file_len = file.len();
    let inside = |off: u64, len: u64| off.checked_add(len).is_some_and(|end| end <= file_len);
    if l1_offset % cs != 0 || !inside(l1_offset, l1_size * 8) {
        return Err(Error::Parse("qcow2 L1 table lies outside the file".into()));
    }
    if rt_offset % cs != 0 || !inside(rt_offset, rt_clusters * cs) {
        return Err(Error::Parse("qcow2 refcount table lies outside the file".into()));
    }

    let mut b = vec![0u8; (l1_size * 8) as usize];
    file.read_at(l1_offset, &mut b)?;
    let l1 = be_u64s(&b);
    let mut map = BlockMap::new(cs, n_clusters);
    let mut l2 = vec![0u8; cs as usize];
    for (i, &e) in l1.iter().enumerate() {
        let l2_off = e & OFFSET_MASK;
        if l2_off == 0 {
            continue;
        }
        if !inside(l2_off, cs) {
            return Err(Error::Parse(format!("qcow2 L2 table {i} lies beyond end of file")));
        }
        file.read_at(l2_off, &mut l2)?;
        for (j, e2) in be_u64s(&l2).into_iter().enumerate() {
            let idx = i as u64 * l2_entries + j as u64;
            if e2 == 0 || idx >= n_clusters {
                continue;
            }
            if e2 & COMPRESSED != 0 {
                return Err(Error::Unsupported("compressed qcow2 clusters".into()));
            }
            let off = e2 & OFFSET_MASK;
            // "all zeroes" clusters (with or without preallocation) read as zeros
            if off == 0 || (version == 3 && e2 & ZERO_FLAG != 0) {
                continue;
            }
            // off < 2^56 and cs <= 2^21: no overflow
            if off + cs > file.len().div_ceil(cs) * cs {
                return Err(Error::Parse(format!("qcow2 cluster {idx} lies beyond end of file")));
            }
            map.set(idx, off);
        }
    }
    let mut b = vec![0u8; (rt_clusters * cs) as usize];
    file.read_at(rt_offset, &mut b)?;
    let rt = be_u64s(&b);
    if let Some(bad) = rt.iter().find(|&&rb| rb != 0 && (rb % cs != 0 || !inside(rb, cs))) {
        return Err(Error::Parse(format!("invalid qcow2 refcount block offset {bad:#x}")));
    }
    if !read_only && version == 3 && get_u64(&h, AUTOCLEAR_OFFSET as usize) != 0 {
        // We do not understand any autoclear feature, so a writer must clear them all.
        file.write_at(AUTOCLEAR_OFFSET, &0u64.to_be_bytes())?;
        file.flush()?;
    }
    let next_free = file.len().div_ceil(cs) * cs;
    Ok(Box::new(SparseDevice {
        capacity,
        map,
        alloc: Qcow2Alloc {
            file,
            cs,
            l1,
            l1_offset,
            rt,
            rt_offset,
            next_free,
        },
    }))
}

/// Cluster allocator: clusters are handed out sequentially at the end of the file and
/// get refcount 1 before anything references them.
struct Qcow2Alloc {
    file: ImageFile,
    cs: u64,
    l1: Vec<u64>,
    l1_offset: u64,
    rt: Vec<u64>,
    rt_offset: u64,
    next_free: u64,
}

impl Qcow2Alloc {
    fn reserve(&mut self) -> u64 {
        let off = self.next_free;
        self.next_free += self.cs;
        off
    }

    /// Set the 16-bit refcount of the cluster at `host_off` to 1, allocating the
    /// refcount block that covers it if necessary.
    fn set_refcount(&mut self, host_off: u64) -> Result<()> {
        let cluster = host_off / self.cs;
        let per_block = self.cs / 2;
        let rti = (cluster / per_block) as usize;
        if rti >= self.rt.len() {
            return Err(Error::Unsupported("qcow2 refcount table is full".into()));
        }
        if self.rt[rti] == 0 {
            let rb = self.reserve();
            self.file.write_at(rb, &vec![0u8; self.cs as usize])?;
            self.rt[rti] = rb;
            self.file
                .write_at(self.rt_offset + rti as u64 * 8, &rb.to_be_bytes())?;
            self.set_refcount(rb)?;
        }
        let entry = self.rt[rti] + (cluster % per_block) * 2;
        self.file.write_at(entry, &1u16.to_be_bytes())
    }

    /// File offset of the L2 table for L1 index `l1i`, allocating it if needed.
    fn l2_table(&mut self, l1i: usize) -> Result<u64> {
        let existing = self.l1[l1i] & OFFSET_MASK;
        if existing != 0 {
            return Ok(existing);
        }
        let l2 = self.reserve();
        self.file.write_at(l2, &vec![0u8; self.cs as usize])?;
        self.set_refcount(l2)?;
        self.l1[l1i] = COPIED | l2;
        self.file
            .write_at(self.l1_offset + l1i as u64 * 8, &self.l1[l1i].to_be_bytes())?;
        Ok(l2)
    }
}

impl BlockAllocator for Qcow2Alloc {
    fn file(&mut self) -> &mut ImageFile {
        &mut self.file
    }

    fn is_read_only(&self) -> bool {
        self.file.is_read_only()
    }

    fn place_block(&mut self, _idx: u64) -> Result<u64> {
        Ok(self.reserve())
    }

    fn on_allocate(&mut self, idx: u64, file_off: u64) -> Result<()> {
        let l2_entries = self.cs / 8;
        self.set_refcount(file_off)?;
        let l2 = self.l2_table((idx / l2_entries) as usize)?;
        self.file
            .write_at(l2 + (idx % l2_entries) * 8, &(COPIED | file_off).to_be_bytes())
    }
}
