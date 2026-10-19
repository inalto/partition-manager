//! FAT12/16/32 read-only filesystem.

pub mod bpb;
pub mod dir;
pub mod format;

use crate::bitmap::UsedMap;
use crate::traits::{Entry, FileSystem};
use bpb::Bpb;
use pm_core::{BlockDevice, CancelToken, Error, FsKind, ProgressReport, ProgressSink, Result};
use std::collections::HashSet;

/// Remembers where the last chain walk ended so sequential reads stay O(1) per cluster.
#[derive(Clone, Copy)]
struct Cursor {
    first: u32,
    idx: u64,
    cluster: u32,
}

/// A mounted (read-only) FAT volume.
pub struct FatFs {
    dev: Box<dyn BlockDevice>,
    /// Byte offset of the volume within `dev`.
    base: u64,
    bpb: Bpb,
    label: String,
    /// One cached FAT sector: (sector index within the FAT, contents).
    fat_cache: Option<(u64, Vec<u8>)>,
    cursor: Option<Cursor>,
}

impl FatFs {
    /// Open the FAT volume starting at byte `base` of `dev`.
    pub fn open(mut dev: Box<dyn BlockDevice>, base: u64) -> Result<FatFs> {
        let mut s0 = [0u8; 512];
        dev.read_at(base, &mut s0)?;
        let bpb = Bpb::parse(&s0).ok_or_else(|| Error::Parse("not a FAT filesystem".into()))?;
        let label = bpb.label.clone();
        let mut fs = FatFs { dev, base, bpb, label, fat_cache: None, cursor: None };
        // The root-directory label entry, when present, wins over the BPB label.
        if let Ok(bytes) = fs.read_dir_bytes(fs.root_id()) {
            if let Some(l) = dir::scan(&bytes, "/", fs.is_fat32()).volume_label {
                fs.label = l;
            }
        }
        Ok(fs)
    }

    fn is_fat32(&self) -> bool {
        self.bpb.kind == FsKind::Fat32
    }

    fn root_id(&self) -> u32 {
        if self.is_fat32() {
            self.bpb.root_cluster
        } else {
            0
        }
    }

    fn entry_size(&self) -> u64 {
        match self.bpb.kind {
            FsKind::Fat32 => 4,
            _ => 2,
        }
    }

    /// Byte offset within one FAT of the entry for `cluster`.
    fn entry_offset(&self, cluster: u64) -> u64 {
        match self.bpb.kind {
            FsKind::Fat12 => cluster * 3 / 2,
            FsKind::Fat16 => cluster * 2,
            _ => cluster * 4,
        }
    }

    /// Decode the entry for `cluster` from `buf`, where `buf[0]` is FAT byte `buf_start`.
    fn decode(&self, buf: &[u8], buf_start: u64, cluster: u64) -> u32 {
        let o = (self.entry_offset(cluster) - buf_start) as usize;
        match self.bpb.kind {
            FsKind::Fat12 => {
                let v = u16::from_le_bytes([buf[o], buf[o + 1]]) as u32;
                if cluster & 1 == 1 {
                    v >> 4
                } else {
                    v & 0x0FFF
                }
            }
            FsKind::Fat16 => u16::from_le_bytes([buf[o], buf[o + 1]]) as u32,
            _ => u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]) & 0x0FFF_FFFF,
        }
    }

    /// Bytes spanned by one entry read (FAT12 entries straddle byte boundaries).
    fn entry_span(&self) -> u64 {
        match self.bpb.kind {
            FsKind::Fat12 => 2,
            _ => self.entry_size(),
        }
    }

    /// Read `buf.len()` bytes of the first FAT at `off` through the one-sector cache.
    fn fat_read(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        let bps = self.bpb.bytes_per_sector as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            let sector = pos / bps;
            let cached = matches!(&self.fat_cache, Some((s, _)) if *s == sector);
            if !cached {
                let mut data = vec![0u8; bps as usize];
                self.dev.read_at(self.base + self.bpb.fat_start() + sector * bps, &mut data)?;
                self.fat_cache = Some((sector, data));
            }
            let data = &self.fat_cache.as_ref().expect("cache filled above").1;
            let in_sector = (pos % bps) as usize;
            let n = (bps as usize - in_sector).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&data[in_sector..in_sector + n]);
            done += n;
        }
        Ok(())
    }

    fn fat_entry(&mut self, cluster: u32) -> Result<u32> {
        let off = self.entry_offset(cluster as u64);
        let span = self.entry_span();
        if off + span > self.bpb.fat_bytes() {
            return Err(Error::Parse(format!("cluster {cluster} lies outside the FAT")));
        }
        let mut b = [0u8; 4];
        self.fat_read(off, &mut b[..span as usize])?;
        Ok(self.decode(&b, off, cluster as u64))
    }

    fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && (c as u64) < self.bpb.cluster_count as u64 + 2
    }

    /// The cluster following `c`; `None` at the end of the chain.
    fn next_cluster(&mut self, c: u32) -> Result<Option<u32>> {
        let v = self.fat_entry(c)?;
        let (eoc_min, bad) = match self.bpb.kind {
            FsKind::Fat12 => (0xFF8, 0xFF7),
            FsKind::Fat16 => (0xFFF8, 0xFFF7),
            _ => (0x0FFF_FFF8, 0x0FFF_FFF7),
        };
        if v >= eoc_min {
            Ok(None)
        } else if v == bad || !self.valid_cluster(v) {
            Err(Error::Parse(format!("FAT chain broken after cluster {c}")))
        } else {
            Ok(Some(v))
        }
    }

    /// All clusters of the chain starting at `first`.
    fn chain(&mut self, first: u32) -> Result<Vec<u32>> {
        if !self.valid_cluster(first) {
            return Err(Error::Parse(format!("invalid start cluster {first}")));
        }
        let limit = self.bpb.cluster_count as usize;
        let mut v = Vec::new();
        let mut c = first;
        loop {
            v.push(c);
            if v.len() > limit {
                return Err(Error::Parse("FAT chain loop".into()));
            }
            match self.next_cluster(c)? {
                Some(n) => c = n,
                None => return Ok(v),
            }
        }
    }

    /// The `target`-th (0-based) cluster of the chain starting at `first`.
    fn cluster_at(&mut self, first: u32, target: u64) -> Result<u32> {
        if !self.valid_cluster(first) {
            return Err(Error::Parse(format!("invalid start cluster {first}")));
        }
        let (mut idx, mut c) = match self.cursor {
            Some(cur) if cur.first == first && cur.idx <= target => (cur.idx, cur.cluster),
            _ => (0, first),
        };
        while idx < target {
            match self.next_cluster(c)? {
                Some(n) => c = n,
                None => return Err(Error::Parse("FAT chain shorter than the file".into())),
            }
            idx += 1;
            if idx >= self.bpb.cluster_count as u64 {
                return Err(Error::Parse("FAT chain loop".into()));
            }
        }
        self.cursor = Some(Cursor { first, idx, cluster: c });
        Ok(c)
    }

    /// Raw bytes of a directory: the fixed root region on FAT12/16, else its cluster chain.
    fn read_dir_bytes(&mut self, id: u32) -> Result<Vec<u8>> {
        let id = if id == 0 { self.root_id() } else { id };
        if id == 0 {
            let mut buf = vec![0u8; self.bpb.root_dir_bytes() as usize];
            self.dev.read_at(self.base + self.bpb.root_dir_start(), &mut buf)?;
            return Ok(buf);
        }
        let chain = self.chain(id)?;
        let cs = self.bpb.cluster_bytes() as usize;
        let mut buf = vec![0u8; chain.len() * cs];
        for (i, &c) in chain.iter().enumerate() {
            self.dev
                .read_at(self.base + self.bpb.cluster_offset(c), &mut buf[i * cs..(i + 1) * cs])?;
        }
        Ok(buf)
    }
}

impl FileSystem for FatFs {
    fn kind(&self) -> FsKind {
        self.bpb.kind
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn total(&self) -> u64 {
        self.bpb.total_bytes()
    }

    fn cluster_size(&self) -> u32 {
        self.bpb.cluster_bytes()
    }

    fn root(&self) -> Entry {
        Entry {
            name: String::new(),
            path: "/".into(),
            is_dir: true,
            size: 0,
            modified: None,
            id: self.root_id() as u64,
            deleted: false,
            attrs: dir::ATTR_DIRECTORY as u32,
        }
    }

    fn list(&mut self, d: &Entry) -> Result<Vec<Entry>> {
        if !d.is_dir {
            return Err(Error::Invalid(format!("{} is not a directory", d.path)));
        }
        let bytes = self.read_dir_bytes(d.id as u32)?;
        Ok(dir::scan(&bytes, &d.path, self.is_fat32()).live)
    }

    fn read_file(&mut self, e: &Entry, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if e.is_dir {
            return Err(Error::Invalid(format!("{} is a directory", e.path)));
        }
        if offset >= e.size || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(e.size - offset) as usize;
        let first = u32::try_from(e.id).map_err(|_| Error::Parse("bad start cluster".into()))?;
        let cs = self.bpb.cluster_bytes() as u64;
        let mut done = 0usize;
        while done < want {
            let pos = offset + done as u64;
            let c = self.cluster_at(first, pos / cs)?;
            let in_cluster = pos % cs;
            let n = ((cs - in_cluster) as usize).min(want - done);
            let at = self.base + self.bpb.cluster_offset(c) + in_cluster;
            self.dev.read_at(at, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(done)
    }

    fn used_map(&mut self) -> Result<UsedMap> {
        let cs = self.bpb.cluster_bytes();
        let mut map = UsedMap::new(self.bpb.total_bytes(), cs);
        // reserved sectors, every FAT and the fixed root directory
        map.set(0, self.bpb.data_start());

        let span = self.entry_span();
        let fat_bytes = self.bpb.fat_bytes();
        // clusters whose entries fit inside the FAT
        let by_fat = match self.bpb.kind {
            FsKind::Fat12 => fat_bytes * 2 / 3,
            _ => fat_bytes / span,
        };
        let mut end = (self.bpb.cluster_count as u64 + 2).min(by_fat);
        while end > 2 && self.entry_offset(end - 1) + span > fat_bytes {
            end -= 1;
        }
        const CHUNK: u64 = 1 << 18;
        let mut first = 2u64;
        while first < end {
            let last = (first + CHUNK).min(end);
            let start_byte = self.entry_offset(first);
            let end_byte = self.entry_offset(last - 1) + span;
            let mut buf = vec![0u8; (end_byte - start_byte) as usize];
            self.dev
                .read_at(self.base + self.bpb.fat_start() + start_byte, &mut buf)?;
            for c in first..last {
                if self.decode(&buf, start_byte, c) != 0 {
                    map.set(self.bpb.cluster_offset(c as u32), cs as u64);
                }
            }
            first = last;
        }
        Ok(map)
    }

    fn deleted_entries(&mut self, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<Vec<Entry>> {
        let fat32 = self.is_fat32();
        let mut out = Vec::new();
        let mut seen: HashSet<u32> = HashSet::new();
        let mut stack: Vec<(u32, String)> = vec![(self.root_id(), "/".into())];
        seen.insert(self.root_id());
        let mut visited = 0u64;
        while let Some((id, path)) = stack.pop() {
            cancel.check()?;
            // a damaged subdirectory must not abort the whole scan
            let bytes = match self.read_dir_bytes(id) {
                Ok(b) => b,
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(_) => continue,
            };
            let scan = dir::scan(&bytes, &path, fat32);
            out.extend(scan.deleted);
            for e in scan.live.into_iter().filter(|e| e.is_dir) {
                let sub = e.id as u32;
                if sub >= 2 && seen.insert(sub) {
                    stack.push((sub, e.path));
                }
            }
            visited += 1;
            progress.report(&ProgressReport {
                phase: "Scanning directories".into(),
                done: visited,
                total: visited + stack.len() as u64,
                detail: path,
                ..Default::default()
            });
        }
        Ok(out)
    }
}
