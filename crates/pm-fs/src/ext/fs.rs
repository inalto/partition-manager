//! The read-only ext2/3/4 [`FileSystem`].

use super::dir::{self, DirEntry};
use super::extent;
use super::inode::{self, Inode, Map, FL_ENCRYPT, FL_EXTENTS, FL_INLINE_DATA, S_IFLNK, S_IFMT, S_IFREG};
use super::super_block::{GroupDesc, SuperBlock, BG_BLOCK_UNINIT, BG_INODE_UNINIT, SB_OFFSET, SB_SIZE};
use crate::bitmap::UsedMap;
use crate::traits::{Entry, FileSystem};
use crate::util::{le16, le32, read_padded};
use pm_core::{BlockDevice, CancelToken, Error, FsKind, ProgressReport, ProgressSink, Result};
use std::collections::HashMap;
use std::sync::Arc;

const ROOT_INO: u32 = 2;
/// Directories larger than this are treated as corrupt.
const MAX_DIR_BYTES: u64 = 256 << 20;
/// Cached metadata blocks (indirect blocks and extent nodes).
const CACHE_BLOCKS: usize = 64;
/// Bytes of an inode table read per I/O while scanning for deleted inodes.
const SCAN_CHUNK: u64 = 1 << 20;

fn join(dir: &str, name: &str) -> String {
    if dir == "/" || dir.is_empty() {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// A mounted (read-only) ext2/3/4 filesystem.
pub struct Ext {
    dev: Box<dyn BlockDevice>,
    sb: SuperBlock,
    /// The raw group descriptor table.
    gdt: Vec<u8>,
    cache: HashMap<u64, Arc<[u8]>>,
}

impl Ext {
    pub fn open(mut dev: Box<dyn BlockDevice>) -> Result<Ext> {
        let mut raw = vec![0u8; SB_SIZE];
        read_padded(&mut *dev, SB_OFFSET, &mut raw)?;
        let sb = SuperBlock::parse(&raw, dev.len())?;
        // the table fits the filesystem (checked by the parser), so this allocation is bounded
        // by the device length
        let mut gdt = vec![0u8; sb.gdt_bytes() as usize];
        dev.read_at(sb.gdt_offset(), &mut gdt)?;
        Ok(Ext { dev, sb, gdt, cache: HashMap::new() })
    }

    fn bs(&self) -> u64 {
        self.sb.block_size as u64
    }

    fn group(&self, g: u32) -> Result<GroupDesc> {
        let ds = self.sb.desc_size as usize;
        let o = g as usize * ds;
        self.gdt
            .get(o..o + ds)
            .map(|d| self.sb.parse_group(d))
            .ok_or_else(|| Error::Parse(format!("group {g} has no descriptor")))
    }

    /// A read of metadata block `blk` through the small cache.
    fn read_block(&mut self, blk: u64) -> Result<Arc<[u8]>> {
        if let Some(b) = self.cache.get(&blk) {
            return Ok(b.clone());
        }
        if blk >= self.sb.blocks_count {
            return Err(Error::Parse(format!("block {blk} lies beyond the filesystem")));
        }
        let mut buf = vec![0u8; self.bs() as usize];
        self.dev.read_at(blk * self.bs(), &mut buf)?;
        let arc: Arc<[u8]> = Arc::from(buf);
        if self.cache.len() >= CACHE_BLOCKS {
            self.cache.clear();
        }
        self.cache.insert(blk, arc.clone());
        Ok(arc)
    }

    fn max_ino(&self) -> u64 {
        self.sb.groups as u64 * self.sb.inodes_per_group as u64
    }

    /// Byte range of inode `n`'s table slot.
    fn inode_offset(&self, n: u32) -> Result<u64> {
        if n == 0 || n as u64 > self.max_ino() {
            return Err(Error::Parse(format!("inode {n} is out of range")));
        }
        let ipg = self.sb.inodes_per_group as u64;
        let g = ((n as u64 - 1) / ipg) as u32;
        let idx = (n as u64 - 1) % ipg;
        let d = self.group(g)?;
        let table_end = d.inode_table.checked_add(self.sb.inode_table_blocks());
        if d.inode_table == 0 || table_end.is_none_or(|e| e > self.sb.blocks_count) {
            return Err(Error::Parse(format!("group {g} inode table lies outside the filesystem")));
        }
        Ok(d.inode_table * self.bs() + idx * self.sb.inode_size as u64)
    }

    fn read_inode(&mut self, n: u32) -> Result<Inode> {
        let off = self.inode_offset(n)?;
        let mut raw = vec![0u8; self.sb.inode_size as usize];
        self.dev.read_at(off, &mut raw)?;
        Inode::parse(&raw)
    }

    fn map_block(&mut self, ino: &Inode, lblock: u64) -> Result<Map> {
        let bs = self.bs();
        let blocks_count = self.sb.blocks_count;
        // split borrows: the closure needs the device and the cache, not the rest of `self`
        let mut read = |blk: u64| -> Result<Arc<[u8]>> { self.read_block(blk) };
        if ino.flags & FL_EXTENTS != 0 {
            extent::lookup(&ino.block, lblock, blocks_count, &mut read)
        } else {
            inode::lookup_indirect(&ino.block, lblock, bs, blocks_count, &mut read)
        }
    }

    /// Read file data from a mapped inode: holes and uninitialized extents read as zeros.
    fn read_data(&mut self, ino: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= ino.size || buf.is_empty() {
            return Ok(0);
        }
        let bs = self.bs();
        let want = (buf.len() as u64).min(ino.size - offset) as usize;
        let mut done = 0usize;
        while done < want {
            let pos = offset + done as u64;
            let within = pos % bs;
            let (phys, n) = match self.map_block(ino, pos / bs)? {
                Map::Data { phys, n } => (Some(phys), n),
                Map::Hole(n) | Map::Zero(n) => (None, n),
            };
            let avail = n.saturating_mul(bs).saturating_sub(within);
            if avail == 0 {
                return Err(Error::Parse("empty block mapping".into()));
            }
            let take = ((want - done) as u64).min(avail) as usize;
            match phys {
                Some(p) => {
                    let at = p * bs + within;
                    if at.checked_add(take as u64).is_none_or(|e| e > self.dev.len()) {
                        return Err(Error::Parse(format!("data at byte {at} lies beyond the device")));
                    }
                    self.dev.read_at(at, &mut buf[done..done + take])?;
                }
                None => buf[done..done + take].fill(0),
            }
            done += take;
        }
        Ok(want)
    }

    /// The whole content of a directory inode, a whole number of blocks.
    fn read_dir_bytes(&mut self, ino: &Inode) -> Result<Vec<u8>> {
        let bs = self.bs();
        if ino.size > MAX_DIR_BYTES || ino.size > self.dev.len() {
            return Err(Error::Parse(format!("directory of {} bytes is implausibly large", ino.size)));
        }
        let len = (ino.size / bs * bs) as usize;
        let mut buf = vec![0u8; len];
        let n = self.read_data(ino, 0, &mut buf)?;
        if n != len {
            return Err(Error::Parse("directory shorter than its size".into()));
        }
        Ok(buf)
    }

    fn dir_entries(&mut self, ino: &Inode) -> Result<Vec<DirEntry>> {
        if ino.flags & FL_INLINE_DATA != 0 {
            return Err(Error::Unsupported("ext4 inline-data directory".into()));
        }
        if ino.flags & FL_ENCRYPT != 0 {
            return Err(Error::Unsupported("ext4 encrypted directory".into()));
        }
        let bytes = self.read_dir_bytes(ino)?;
        let mut out = Vec::new();
        for block in bytes.chunks(self.bs() as usize) {
            dir::parse_block(block, &mut out)?;
        }
        Ok(out)
    }

    fn entry_of(&self, parent: &str, name: &str, ino_no: u32, ino: Option<&Inode>, file_type: u8) -> Entry {
        let (is_dir, size, modified, attrs) = match ino {
            Some(i) => (i.is_dir(), if i.is_dir() { 0 } else { i.size }, Some(i.mtime), i.mode as u32),
            // unreadable inode: fall back to what the directory entry says
            None => (file_type == 2, 0, None, 0),
        };
        Entry {
            name: name.to_string(),
            path: join(parent, name),
            is_dir,
            size,
            modified,
            id: ino_no as u64,
            deleted: false,
            attrs,
        }
    }

    /// Mark the blocks of one group's bitmap (`d`) in `map`.
    fn mark_group_bitmap(&mut self, map: &mut UsedMap, g: u32, d: &GroupDesc) -> Result<()> {
        let bs = self.bs();
        let first = self.sb.group_first_block(g);
        let count = (self.sb.blocks_per_group as u64).min(self.sb.blocks_count - first);
        let bm = self.read_block(d.block_bitmap)?;
        if (bm.len() as u64) * 8 < count {
            return Err(Error::Parse("block bitmap smaller than its group".into()));
        }
        let mut run: Option<u64> = None;
        let flush = |map: &mut UsedMap, s: u64, e: u64| map.set((first + s) * bs, (e - s) * bs);
        let nbytes = count.div_ceil(8) as usize;
        for (k, &b) in bm[..nbytes].iter().enumerate() {
            let first_bit = k as u64 * 8;
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
                let i = first_bit + bit;
                if i >= count {
                    break;
                }
                if (b >> bit) & 1 != 0 {
                    run.get_or_insert(i);
                } else if let Some(s) = run.take() {
                    flush(map, s, i);
                }
            }
        }
        if let Some(s) = run.take() {
            flush(map, s, count);
        }
        Ok(())
    }
}

impl FileSystem for Ext {
    fn kind(&self) -> FsKind {
        self.sb.kind
    }

    fn label(&self) -> String {
        self.sb.label.clone()
    }

    fn total(&self) -> u64 {
        self.sb.total_bytes()
    }

    fn cluster_size(&self) -> u32 {
        self.sb.block_size
    }

    fn root(&self) -> Entry {
        Entry {
            name: String::new(),
            path: "/".into(),
            is_dir: true,
            size: 0,
            modified: None,
            id: ROOT_INO as u64,
            deleted: false,
            attrs: (inode::S_IFDIR | 0o755) as u32,
        }
    }

    fn list(&mut self, d: &Entry) -> Result<Vec<Entry>> {
        let id = u32::try_from(d.id).map_err(|_| Error::Invalid(format!("{} is not a valid inode", d.path)))?;
        let dino = self.read_inode(id)?;
        if !dino.is_dir() {
            return Err(Error::Invalid(format!("{} is not a directory", d.path)));
        }
        let ents = self.dir_entries(&dino)?;
        let mut out = Vec::with_capacity(ents.len());
        for e in ents {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let child = self.read_inode(e.inode).ok();
            out.push(self.entry_of(&d.path, &e.name, e.inode, child.as_ref(), e.file_type));
        }
        Ok(out)
    }

    fn read_file(&mut self, e: &Entry, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let id = u32::try_from(e.id).map_err(|_| Error::Invalid(format!("{} is not a valid inode", e.path)))?;
        let ino = self.read_inode(id)?;
        if ino.is_dir() {
            return Err(Error::Invalid(format!("{} is a directory", e.path)));
        }
        if ino.flags & FL_INLINE_DATA != 0 {
            return Err(Error::Unsupported("ext4 inline data".into()));
        }
        if ino.flags & FL_ENCRYPT != 0 {
            return Err(Error::Unsupported("ext4 encrypted file".into()));
        }
        if ino.is_fast_symlink() {
            let size = ino.size as usize;
            if offset >= size as u64 || buf.is_empty() {
                return Ok(0);
            }
            let n = buf.len().min(size - offset as usize);
            buf[..n].copy_from_slice(&ino.block[offset as usize..offset as usize + n]);
            return Ok(n);
        }
        match ino.mode & S_IFMT {
            S_IFREG | S_IFLNK => self.read_data(&ino, offset, buf),
            _ => Err(Error::Unsupported(format!("{} is a special file", e.path))),
        }
    }

    fn used_map(&mut self) -> Result<UsedMap> {
        let bs = self.bs();
        let len = self.sb.total_bytes();
        let mut map = UsedMap::new(len, self.sb.block_size);
        if self.sb.needs_recovery() {
            // the journal has not been replayed, so the bitmaps may be stale: copy everything
            map.set(0, len);
            return Ok(map);
        }
        let gdt_blocks = self.sb.gdt_blocks();
        let sb_area = self.sb.first_data_block + 1 + gdt_blocks + self.sb.reserved_gdt_blocks as u64;
        // boot block, superblock, descriptor table and reserved descriptor blocks
        map.set(0, sb_area * bs);
        let itb = self.sb.inode_table_blocks();
        let use_flags = self.sb.has_group_flags();
        for g in 0..self.sb.groups {
            let d = self.group(g)?;
            let first = self.sb.group_first_block(g);
            let bad = |what: &str| Error::Parse(format!("group {g}: {what} lies outside the filesystem"));
            if d.block_bitmap >= self.sb.blocks_count {
                return Err(bad("block bitmap"));
            }
            if d.inode_bitmap >= self.sb.blocks_count {
                return Err(bad("inode bitmap"));
            }
            if d.inode_table.checked_add(itb).is_none_or(|e| e > self.sb.blocks_count) {
                return Err(bad("inode table"));
            }
            map.set(d.block_bitmap * bs, bs);
            map.set(d.inode_bitmap * bs, bs);
            map.set(d.inode_table * bs, itb * bs);
            if use_flags && d.flags & BG_BLOCK_UNINIT != 0 {
                // no valid bitmap: only metadata is allocated in the group
                if self.sb.may_have_backup(g) {
                    map.set(first * bs, (1 + gdt_blocks + self.sb.reserved_gdt_blocks as u64) * bs);
                }
                continue;
            }
            self.mark_group_bitmap(&mut map, g, &d)?;
        }
        Ok(map)
    }

    fn deleted_entries(&mut self, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<Vec<Entry>> {
        let bs = self.bs();
        let isz = self.sb.inode_size as u64;
        let ipg = self.sb.inodes_per_group as u64;
        let itb = self.sb.inode_table_blocks();
        let groups = self.sb.groups as u64;
        let use_flags = self.sb.has_group_flags();
        let per_chunk = (SCAN_CHUNK / isz).max(1);
        let mut out = Vec::new();
        for g in 0..self.sb.groups {
            cancel.check()?;
            progress.report(&ProgressReport {
                phase: "Scanning inode tables".into(),
                done: g as u64,
                total: groups,
                ..Default::default()
            });
            let d = self.group(g)?;
            if use_flags && d.flags & BG_INODE_UNINIT != 0 {
                continue;
            }
            // a damaged descriptor must not end the scan
            if d.inode_table == 0 || d.inode_table.checked_add(itb).is_none_or(|e| e > self.sb.blocks_count) {
                continue;
            }
            let mut idx = 0u64;
            while idx < ipg {
                let n = per_chunk.min(ipg - idx);
                let mut raw = vec![0u8; (n * isz) as usize];
                self.dev.read_at(d.inode_table * bs + idx * isz, &mut raw)?;
                for k in 0..n {
                    let r = &raw[(k * isz) as usize..((k + 1) * isz) as usize];
                    let ino_no = g as u64 * ipg + idx + k + 1;
                    if ino_no < self.sb.first_ino as u64 || ino_no > u32::MAX as u64 {
                        continue;
                    }
                    if le16(r, 0) == 0 || le32(r, 0x14) == 0 || le16(r, 0x1A) != 0 {
                        continue;
                    }
                    let Ok(ino) = Inode::parse(r) else { continue };
                    out.push(Entry {
                        name: format!("inode-{ino_no}"),
                        path: format!("/inode-{ino_no}"),
                        is_dir: ino.is_dir(),
                        size: if ino.is_dir() { 0 } else { ino.size },
                        modified: Some(ino.mtime),
                        id: ino_no,
                        deleted: true,
                        attrs: ino.mode as u32,
                    });
                }
                idx += n;
            }
        }
        progress.report(&ProgressReport {
            phase: "Scanning inode tables".into(),
            done: groups,
            total: groups,
            ..Default::default()
        });
        Ok(out)
    }
}
