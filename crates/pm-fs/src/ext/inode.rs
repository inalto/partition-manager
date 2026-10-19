//! ext inodes and the classic (indirect block) block map.

use crate::util::{le16, le32};
use pm_core::{Error, Result};
use std::sync::Arc;

pub const S_IFMT: u16 = 0xF000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFREG: u16 = 0x8000;
pub const S_IFLNK: u16 = 0xA000;

pub const FL_ENCRYPT: u32 = 0x800;
pub const FL_EXTENTS: u32 = 0x80000;
pub const FL_INLINE_DATA: u32 = 0x1000_0000;

/// Where a logical block of a file lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Map {
    /// `n` blocks starting at physical block `phys`.
    Data { phys: u64, n: u64 },
    /// `n` unallocated blocks (read as zeros).
    Hole(u64),
    /// `n` allocated but never written blocks (read as zeros).
    Zero(u64),
}

#[derive(Clone, Debug)]
pub struct Inode {
    pub mode: u16,
    /// File size; for regular files the 64-bit `i_size_high:i_size`.
    pub size: u64,
    pub flags: u32,
    pub links: u16,
    pub dtime: u32,
    /// Modification time, unix seconds.
    pub mtime: i64,
    pub block: [u8; 60],
}

impl Inode {
    /// Parse the raw inode (at least 128 bytes).
    pub fn parse(raw: &[u8]) -> Result<Inode> {
        if raw.len() < 128 {
            return Err(Error::Parse("ext inode truncated".into()));
        }
        let mode = le16(raw, 0);
        let lo = le32(raw, 4) as u64;
        let size = if mode & S_IFMT == S_IFREG { lo | (le32(raw, 0x6C) as u64) << 32 } else { lo };
        let mut mtime = le32(raw, 0x10) as i32 as i64;
        // i_extra_isize >= 12 covers i_mtime_extra; its low two bits extend the epoch
        if raw.len() >= 0x8C && le16(raw, 0x80) >= 12 {
            mtime += ((le32(raw, 0x88) & 3) as i64) << 32;
        }
        let mut block = [0u8; 60];
        block.copy_from_slice(&raw[0x28..0x28 + 60]);
        Ok(Inode {
            mode,
            size,
            flags: le32(raw, 0x20),
            links: le16(raw, 0x1A),
            dtime: le32(raw, 0x14),
            mtime,
            block,
        })
    }

    pub fn kind(&self) -> u16 {
        self.mode & S_IFMT
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == S_IFDIR
    }

    /// A symlink whose target is stored in `i_block` (always the case below 60 bytes).
    pub fn is_fast_symlink(&self) -> bool {
        self.kind() == S_IFLNK && self.size < 60 && self.flags & (FL_EXTENTS | FL_ENCRYPT) == 0
    }
}

/// Reads one filesystem block of metadata.
pub type ReadBlock<'a> = dyn FnMut(u64) -> Result<Arc<[u8]>> + 'a;

/// Look `lblock` up in a classic block map: 12 direct pointers, then single, double and triple
/// indirect blocks. `bs` is the block size, `blocks_count` bounds every pointer.
pub fn lookup_indirect(
    iblock: &[u8; 60],
    lblock: u64,
    bs: u64,
    blocks_count: u64,
    read: &mut ReadBlock<'_>,
) -> Result<Map> {
    let p = bs / 4;
    let check = |ptr: u32| -> Result<u64> {
        if ptr as u64 >= blocks_count {
            return Err(Error::Parse(format!("block pointer {ptr} lies beyond the filesystem")));
        }
        Ok(ptr as u64)
    };
    if lblock < 12 {
        let ptr = le32(iblock, 4 * lblock as usize);
        return if ptr == 0 { Ok(Map::Hole(1)) } else { Ok(Map::Data { phys: check(ptr)?, n: 1 }) };
    }
    let mut l = lblock - 12;
    // (level, root pointer slot): a level-k pointer block covers p^k data blocks
    let (mut level, mut ptr) = if l < p {
        (1u32, le32(iblock, 48))
    } else {
        l -= p;
        if l < p * p {
            (2, le32(iblock, 52))
        } else {
            l -= p * p;
            if l >= p * p * p {
                return Err(Error::Parse(format!("logical block {lblock} exceeds the indirect map")));
            }
            (3, le32(iblock, 56))
        }
    };
    loop {
        let cover = p.pow(level);
        if ptr == 0 {
            return Ok(Map::Hole(cover - l));
        }
        let blk = read(check(ptr)?)?;
        let sub = cover / p;
        let slot = (l / sub) as usize;
        l %= sub;
        if blk.len() < 4 * (slot + 1) {
            return Err(Error::Parse("indirect block truncated".into()));
        }
        let next = le32(&blk, 4 * slot);
        if level == 1 {
            return if next == 0 { Ok(Map::Hole(1)) } else { Ok(Map::Data { phys: check(next)?, n: 1 }) };
        }
        ptr = next;
        level -= 1;
    }
}
