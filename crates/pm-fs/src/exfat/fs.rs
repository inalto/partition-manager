//! The read-only exFAT [`FileSystem`].

use super::boot::ExFatBoot;
use super::dir::{self, FileSet};
use crate::bitmap::UsedMap;
use crate::traits::{Entry, FileSystem};
use crate::util::read_padded;
use pm_core::{BlockDevice, CancelToken, Error, FsKind, ProgressReport, ProgressSink, Result};
use std::collections::{HashMap, HashSet, VecDeque};

/// The spec limits a directory to 256 MiB.
const MAX_DIR_BYTES: u64 = 256 << 20;
/// Bytes of the allocation bitmap processed per read.
const BITMAP_CHUNK: usize = 1 << 20;

const NOFAT_BIT: u64 = 1 << 32;
const CLUSTERS_SHIFT: u32 = 33;
const MAX_CLUSTERS: u64 = (1 << 31) - 1;

/// Where the data of a stream lives; decoded from [`Entry::id`].
#[derive(Clone, Copy, Debug)]
struct Loc {
    first: u32,
    nofat: bool,
    /// Cluster count of a `NoFatChain` stream.
    clusters: u64,
}

impl Loc {
    fn from_id(id: u64) -> Loc {
        Loc { first: id as u32, nofat: id & NOFAT_BIT != 0, clusters: id >> CLUSTERS_SHIFT }
    }

    /// A `NoFatChain` run must lie wholly inside the cluster heap (`first >= 2`,
    /// `first + clusters <= cluster_count + 2`). Every consumer of such a run calls this.
    fn check_run(&self, cluster_count: u32) -> Result<()> {
        let end = (self.first as u64).checked_add(self.clusters);
        if self.first < 2 || end.is_none_or(|e| e > cluster_count as u64 + 2) {
            return Err(Error::Parse(format!(
                "contiguous stream of {} clusters at cluster {} leaves the cluster heap",
                self.clusters, self.first
            )));
        }
        Ok(())
    }

    fn to_id(self) -> u64 {
        self.first as u64 | if self.nofat { NOFAT_BIT | (self.clusters << CLUSTERS_SHIFT) } else { 0 }
    }

    fn of_set(s: &FileSet, cluster_size: u64) -> Loc {
        let clusters = s.data_len.div_ceil(cluster_size).min(MAX_CLUSTERS);
        Loc { first: s.first_cluster, nofat: s.nofat, clusters: if s.nofat { clusters } else { 0 } }
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" || dir.is_empty() {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// A mounted (read-only) exFAT volume.
pub struct ExFat {
    dev: Box<dyn BlockDevice>,
    boot: ExFatBoot,
    label: String,
    /// `(first cluster, index, cluster)`: where the last FAT-chain lookup ended, so sequential
    /// reads of a fragmented file do not rewalk the chain from its start.
    cursor: Option<(u32, u64, u32)>,
    /// ValidDataLength of every non-empty file handed out by `list` / `deleted_entries`, keyed
    /// by `(first cluster, deleted)` so live and deleted entries never collide (of two deleted
    /// entries sharing a first cluster the first one listed wins). `Entry` has no field for
    /// it, so entries built by hand read up to `size`.
    valid_len: HashMap<(u32, bool), u64>,
}

impl ExFat {
    pub fn open(mut dev: Box<dyn BlockDevice>) -> Result<ExFat> {
        let mut s0 = [0u8; 512];
        read_padded(&mut *dev, 0, &mut s0)?;
        let boot = ExFatBoot::parse(&s0, dev.len())?;
        let mut fs = ExFat { dev, boot, label: String::new(), cursor: None, valid_len: HashMap::new() };
        let root = Loc { first: fs.boot.root_cluster, nofat: false, clusters: 0 };
        if let Ok(bytes) = fs.read_dir_bytes(root) {
            if let Ok(p) = dir::parse(&bytes, false, false) {
                fs.label = p.label.unwrap_or_default();
            }
        }
        Ok(fs)
    }

    fn cs(&self) -> u64 {
        self.boot.cluster_size as u64
    }

    /// FAT entry of cluster `c`.
    fn fat_entry(&mut self, c: u32) -> Result<u32> {
        let off = c as u64 * 4;
        if off + 4 > self.boot.fat_bytes {
            return Err(Error::Parse(format!("cluster {c} lies beyond the FAT")));
        }
        let mut b = [0u8; 4];
        self.dev.read_at(self.boot.fat_start + off, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// The cluster after `c` in its FAT chain; `None` at the end of the chain.
    fn next_cluster(&mut self, c: u32) -> Result<Option<u32>> {
        let v = self.fat_entry(c)?;
        if v >= 0xFFFF_FFF8 {
            Ok(None)
        } else if v >= 2 && (v as u64) < self.boot.cluster_count as u64 + 2 {
            Ok(Some(v))
        } else {
            Err(Error::Parse(format!("cluster {c} chains to invalid FAT entry {v:#x}")))
        }
    }

    /// A deleted chained stream whose FAT entries were freed is read as contiguous (best
    /// effort); everything else is read as recorded.
    fn effective(&mut self, loc: Loc, e: &Entry) -> Loc {
        if e.deleted && !loc.nofat && loc.first >= 2 && self.fat_entry(loc.first).is_ok_and(|v| v == 0) {
            let clusters = e.size.div_ceil(self.cs()).min(MAX_CLUSTERS);
            return Loc { first: loc.first, nofat: true, clusters };
        }
        loc
    }

    /// Cluster number of the `idx`-th cluster of the stream.
    fn cluster_at(&mut self, loc: Loc, idx: u64) -> Result<u32> {
        if loc.nofat {
            loc.check_run(self.boot.cluster_count)?;
            if idx >= loc.clusters {
                return Err(Error::Parse("read beyond the clusters of a contiguous stream".into()));
            }
            let c = loc.first as u64 + idx;
            return Ok(c as u32);
        }
        if loc.first < 2 || loc.first as u64 >= self.boot.cluster_count as u64 + 2 {
            return Err(Error::Parse(format!("stream starts at invalid cluster {}", loc.first)));
        }
        let (mut c, mut i) = match self.cursor {
            Some((f, ci, cc)) if f == loc.first && ci <= idx => (cc, ci),
            _ => (loc.first, 0),
        };
        while i < idx {
            c = self.next_cluster(c)?.ok_or_else(|| Error::Parse("FAT chain shorter than the stream".into()))?;
            i += 1;
            if i > self.boot.cluster_count as u64 {
                return Err(Error::Parse("FAT chain loops".into()));
            }
        }
        self.cursor = Some((loc.first, i, c));
        Ok(c)
    }

    /// Every cluster of a stream, in order; more than `limit` clusters (or a loop) is an error.
    fn chain(&mut self, loc: Loc, limit: u64) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        if loc.nofat {
            loc.check_run(self.boot.cluster_count)?;
            if loc.clusters > limit {
                return Err(Error::Parse("contiguous stream longer than allowed".into()));
            }
            for k in 0..loc.clusters {
                out.push(self.cluster_at(loc, k)?);
            }
            return Ok(out);
        }
        let mut c = loc.first;
        self.boot.cluster_offset(c)?;
        loop {
            out.push(c);
            if out.len() as u64 > limit.min(self.boot.cluster_count as u64) {
                return Err(Error::Parse("cluster chain too long or loops".into()));
            }
            match self.next_cluster(c)? {
                Some(n) => c = n,
                None => return Ok(out),
            }
        }
    }

    fn read_dir_bytes(&mut self, loc: Loc) -> Result<Vec<u8>> {
        let cs = self.cs();
        let clusters = self.chain(loc, MAX_DIR_BYTES / cs)?;
        let mut out = vec![0u8; clusters.len() * cs as usize];
        for (k, &c) in clusters.iter().enumerate() {
            let at = self.boot.cluster_offset(c)?;
            self.dev.read_at(at, &mut out[k * cs as usize..(k + 1) * cs as usize])?;
        }
        Ok(out)
    }

    fn entry_of(&mut self, parent: &str, s: &FileSet, deleted: bool) -> Entry {
        let loc = Loc::of_set(s, self.cs());
        if !s.is_dir() && s.first_cluster >= 2 {
            let key = (s.first_cluster, deleted);
            if deleted {
                self.valid_len.entry(key).or_insert(s.valid_len);
            } else {
                self.valid_len.insert(key, s.valid_len);
            }
        }
        Entry {
            name: s.name.clone(),
            path: join(parent, &s.name),
            is_dir: s.is_dir(),
            size: if s.is_dir() { 0 } else { s.data_len },
            modified: s.modified,
            id: loc.to_id(),
            deleted,
            attrs: s.attrs as u32,
        }
    }

    /// Mark the clusters whose bit is set in one allocation bitmap.
    fn mark_bitmap(&mut self, map: &mut UsedMap, bm: dir::BitmapEntry) -> Result<()> {
        let count = self.boot.cluster_count as u64;
        let need = count.div_ceil(8);
        let cs = self.cs();
        if bm.data_len < need {
            return Err(Error::Parse(format!(
                "allocation bitmap of {} bytes is too short for {count} clusters",
                bm.data_len
            )));
        }
        if bm.first_cluster < 2 || bm.first_cluster as u64 >= count + 2 {
            return Err(Error::Parse("allocation bitmap starts outside the cluster heap".into()));
        }
        // like the other system files it is normally FAT-chained; a free first FAT entry
        // means a contiguous run
        let n_clusters = need.div_ceil(cs);
        let n_total = bm.data_len.div_ceil(cs);
        let loc = if self.fat_entry(bm.first_cluster)? == 0 {
            Loc { first: bm.first_cluster, nofat: true, clusters: n_clusters }
        } else {
            Loc { first: bm.first_cluster, nofat: false, clusters: 0 }
        };
        let clusters = self.chain(loc, n_total.max(n_clusters))?;
        if (clusters.len() as u64) < n_clusters {
            return Err(Error::Parse("allocation bitmap chain is shorter than its length".into()));
        }
        let heap = self.boot.heap_start;
        let flush = |map: &mut UsedMap, start: u64, end: u64| {
            map.set(heap + start * cs, (end - start) * cs);
        };
        let mut run: Option<u64> = None;
        let mut buf = vec![0u8; BITMAP_CHUNK.min(cs as usize).max(1)];
        let mut byte_idx = 0u64; // bitmap byte index of the next byte to process
        'outer: for &c in clusters.iter().take(n_clusters as usize) {
            let base = self.boot.cluster_offset(c)?;
            map.set(base, cs); // the bitmap's own clusters
            let mut off = 0u64;
            while off < cs {
                if byte_idx >= need {
                    break 'outer;
                }
                let n = (buf.len() as u64).min(cs - off).min(need - byte_idx) as usize;
                self.dev.read_at(base + off, &mut buf[..n])?;
                for (k, &b) in buf[..n].iter().enumerate() {
                    let first_bit = (byte_idx + k as u64) * 8;
                    if b == 0 {
                        if let Some(s) = run.take() {
                            flush(map, s, first_bit);
                        }
                        continue;
                    }
                    if b == 0xFF && run.is_some() && first_bit + 8 <= count {
                        continue;
                    }
                    for bit in 0..8u64 {
                        let cl = first_bit + bit;
                        if cl >= count {
                            break;
                        }
                        if (b >> bit) & 1 != 0 {
                            run.get_or_insert(cl);
                        } else if let Some(s) = run.take() {
                            flush(map, s, cl);
                        }
                    }
                }
                byte_idx += n as u64;
                off += n as u64;
            }
        }
        if let Some(s) = run.take() {
            flush(map, s, count);
        }
        Ok(())
    }
}

impl FileSystem for ExFat {
    fn kind(&self) -> FsKind {
        FsKind::ExFat
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn total(&self) -> u64 {
        self.boot.volume_bytes
    }

    fn cluster_size(&self) -> u32 {
        self.boot.cluster_size
    }

    fn root(&self) -> Entry {
        Entry {
            name: String::new(),
            path: "/".into(),
            is_dir: true,
            size: 0,
            modified: None,
            id: self.boot.root_cluster as u64,
            deleted: false,
            attrs: 0x10,
        }
    }

    fn list(&mut self, d: &Entry) -> Result<Vec<Entry>> {
        if !d.is_dir {
            return Err(Error::Invalid(format!("{} is not a directory", d.path)));
        }
        let bytes = self.read_dir_bytes(Loc::from_id(d.id))?;
        let parsed = dir::parse(&bytes, true, false)?;
        let mut out = Vec::with_capacity(parsed.sets.len());
        for s in &parsed.sets {
            out.push(self.entry_of(&d.path, s, false));
        }
        Ok(out)
    }

    fn read_file(&mut self, e: &Entry, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if e.is_dir {
            return Err(Error::Invalid(format!("{} is a directory", e.path)));
        }
        if offset >= e.size || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(e.size - offset) as usize;
        let loc = self.effective(Loc::from_id(e.id), e);
        if loc.nofat {
            loc.check_run(self.boot.cluster_count)?;
        }
        // bytes at or beyond ValidDataLength read as zeros
        let valid = self.valid_len.get(&(loc.first, e.deleted)).copied().unwrap_or(u64::MAX).min(e.size);
        let disk = valid.saturating_sub(offset).min(want as u64) as usize;
        let cs = self.cs();
        let mut done = 0usize;
        while done < disk {
            let pos = offset + done as u64;
            let idx = pos / cs;
            let within = pos % cs;
            let c = self.cluster_at(loc, idx)?;
            let mut n = ((disk - done) as u64).min(cs - within);
            if loc.nofat {
                // a contiguous stream: read across cluster boundaries in one go
                n = ((disk - done) as u64).min((loc.clusters - idx) * cs - within);
            }
            let at = self.boot.cluster_offset(c)? + within;
            if at.checked_add(n).is_none_or(|end| end > self.dev.len()) {
                return Err(Error::Parse(format!("data at byte {at} lies beyond the device")));
            }
            self.dev.read_at(at, &mut buf[done..done + n as usize])?;
            done += n as usize;
        }
        buf[disk..want].fill(0);
        Ok(want)
    }

    fn used_map(&mut self) -> Result<UsedMap> {
        let cs = self.boot.cluster_size;
        let heap = self.boot.heap_start;
        let mut map = UsedMap::with_origin(self.boot.volume_bytes, cs, heap % cs as u64);
        // boot regions, FAT(s) and any alignment padding before the heap
        map.set(0, heap);

        let root = Loc { first: self.boot.root_cluster, nofat: false, clusters: 0 };
        let root_clusters = self.chain(root, MAX_DIR_BYTES / cs as u64)?;
        for &c in &root_clusters {
            map.set(self.boot.cluster_offset(c)?, cs as u64);
        }
        let bytes = self.read_dir_bytes(root)?;
        let parsed = dir::parse(&bytes, false, false)?;
        if parsed.bitmaps.is_empty() {
            return Err(Error::Parse("no allocation bitmap entry in the root directory".into()));
        }
        // TexFAT volumes carry two bitmaps, only one of them current: take the union, which
        // can only over-approximate
        for bm in parsed.bitmaps {
            self.mark_bitmap(&mut map, bm)?;
        }
        Ok(map)
    }

    fn deleted_entries(&mut self, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let root = Loc { first: self.boot.root_cluster, nofat: false, clusters: 0 };
        let mut queue: VecDeque<(String, Loc)> = VecDeque::new();
        let mut seen: HashSet<u32> = HashSet::new();
        seen.insert(root.first);
        queue.push_back(("/".to_string(), root));
        let mut done = 0u64;
        let mut total = 1u64;
        while let Some((path, loc)) = queue.pop_front() {
            cancel.check()?;
            progress.report(&ProgressReport {
                phase: "Scanning directories".into(),
                done,
                total,
                ..Default::default()
            });
            done += 1;
            let bytes = match self.read_dir_bytes(loc) {
                Ok(b) => b,
                // a damaged directory must not end the scan
                Err(Error::Parse(_)) => continue,
                Err(e) => return Err(e),
            };
            let Ok(parsed) = dir::parse(&bytes, false, true) else { continue };
            for s in &parsed.sets {
                if s.in_use {
                    if s.is_dir() && seen.insert(s.first_cluster) {
                        queue.push_back((join(&path, &s.name), Loc::of_set(s, self.cs())));
                        total += 1;
                    }
                } else {
                    out.push(self.entry_of(&path, s, true));
                }
            }
        }
        progress.report(&ProgressReport { phase: "Scanning directories".into(), done, total, ..Default::default() });
        Ok(out)
    }
}
