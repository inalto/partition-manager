//! The ext4 extent tree.

use super::inode::{Map, ReadBlock};
use crate::util::{le16, le32};
use pm_core::{Error, Result};
use std::sync::Arc;

const MAGIC: u16 = 0xF30A;
/// The kernel's depth limit; deeper trees are corrupt.
const MAX_DEPTH: u16 = 5;

struct Header {
    entries: usize,
    depth: u16,
}

fn header(node: &[u8]) -> Result<Header> {
    if node.len() < 12 || le16(node, 0) != MAGIC {
        return Err(Error::Parse("bad extent tree node magic".into()));
    }
    let entries = le16(node, 2) as usize;
    let max = le16(node, 4) as usize;
    let depth = le16(node, 6);
    if entries > max || 12 + 12 * max > node.len() {
        return Err(Error::Parse("extent tree node entry counts out of range".into()));
    }
    if depth > MAX_DEPTH {
        return Err(Error::Parse(format!("extent tree depth {depth} out of range")));
    }
    Ok(Header { entries, depth })
}

/// Map logical block `lblock` through the extent tree rooted in `iblock` (the 60-byte
/// `i_block`). Interior and leaf nodes are fetched with `read`; their physical addresses must
/// be below `blocks_count`. Node depths must descend by exactly one, so a self-referencing or
/// looping index cannot recurse.
pub fn lookup(iblock: &[u8; 60], lblock: u64, blocks_count: u64, read: &mut ReadBlock<'_>) -> Result<Map> {
    let mut node: Arc<[u8]> = Arc::from(&iblock[..]);
    let mut expect: Option<u16> = None;
    // first logical block of the next sibling at any level above: caps the length of a hole
    let mut bound = u64::MAX;
    loop {
        let h = header(&node)?;
        if expect.is_some_and(|d| d != h.depth) {
            return Err(Error::Parse("extent tree node depth mismatch".into()));
        }
        let ent = |k: usize| &node[12 + 12 * k..12 + 12 * (k + 1)];
        // greatest start <= lblock, and the smallest start > lblock
        let mut best: Option<usize> = None;
        let mut next_start = bound;
        for k in 0..h.entries {
            let start = le32(ent(k), 0) as u64;
            if start <= lblock {
                if best.is_none_or(|b| start >= le32(ent(b), 0) as u64) {
                    best = Some(k);
                }
            } else {
                next_start = next_start.min(start);
            }
        }
        if h.depth == 0 {
            let Some(b) = best else { return Ok(Map::Hole(next_start - lblock)) };
            let e = ent(b);
            let start = le32(e, 0) as u64;
            let raw_len = le16(e, 4) as u64;
            let (len, uninit) = if raw_len > 32768 { (raw_len - 32768, true) } else { (raw_len, false) };
            if len == 0 || lblock >= start + len {
                return Ok(Map::Hole(next_start - lblock));
            }
            let phys = le32(e, 8) as u64 | (le16(e, 6) as u64) << 32;
            let n = start + len - lblock;
            if phys.saturating_add(len) > blocks_count {
                return Err(Error::Parse(format!("extent at block {phys} lies beyond the filesystem")));
            }
            return Ok(if uninit { Map::Zero(n) } else { Map::Data { phys: phys + (lblock - start), n } });
        }
        let Some(b) = best else { return Ok(Map::Hole(next_start - lblock)) };
        let e = ent(b);
        let child = le32(e, 4) as u64 | (le16(e, 8) as u64) << 32;
        if child >= blocks_count {
            return Err(Error::Parse(format!("extent index points at block {child} beyond the filesystem")));
        }
        bound = next_start;
        expect = Some(h.depth - 1);
        node = read(child)?;
    }
}
