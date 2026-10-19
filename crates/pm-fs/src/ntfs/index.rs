//! Directory indexes: `$INDEX_ROOT` / `$INDEX_ALLOCATION` B-tree nodes and their in-order walk.

use super::mft::{apply_fixups, sub};
use crate::util::{le16, le32, le64};
use pm_core::{Error, Result};
use std::collections::HashSet;

/// Deepest B-tree accepted before the index is declared corrupt.
const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug)]
pub struct NodeEntry {
    pub file_ref: u64,
    /// The `$FILE_NAME` key; `None` for the terminating entry of a node.
    pub key: Option<Vec<u8>>,
    /// VCN of the sub-node holding the keys sorting before this entry.
    pub child: Option<u64>,
}

/// Entries of the node whose INDEX_HEADER starts at `hdr` in `buf`.
fn parse_node(buf: &[u8], hdr: usize) -> Result<Vec<NodeEntry>> {
    let bad = |m: &str| Error::Parse(format!("index node: {m}"));
    let h = sub(buf, hdr, 16)?;
    let (start, end) = match (hdr.checked_add(le32(h, 0) as usize), hdr.checked_add(le32(h, 4) as usize)) {
        (Some(s), Some(e)) if e <= buf.len() && s <= e => (s, e),
        _ => return Err(bad("entries outside the node")),
    };
    let mut out = Vec::new();
    let mut off = start;
    loop {
        if end - off < 0x10 {
            return Err(bad("missing terminating entry"));
        }
        let file_ref = le64(buf, off);
        let len = le16(buf, off + 8) as usize;
        let key_len = le16(buf, off + 10) as usize;
        let flags = le16(buf, off + 12);
        if len < 0x10 || len > end - off {
            return Err(bad("bad entry length"));
        }
        let e = &buf[off..off + len];
        let child = if flags & 1 != 0 {
            if len < 0x18 {
                return Err(bad("sub-node entry too short"));
            }
            Some(le64(e, len - 8))
        } else {
            None
        };
        let last = flags & 2 != 0;
        let key = if last {
            None
        } else {
            let room = if child.is_some() { len - 8 } else { len };
            Some(sub(&e[..room], 0x10, key_len)?.to_vec())
        };
        out.push(NodeEntry { file_ref, key, child });
        if last {
            return Ok(out);
        }
        off += len;
    }
}

/// Parse an `$INDEX_ROOT` value: (index block size, root node entries).
pub fn parse_root(v: &[u8]) -> Result<(u32, Vec<NodeEntry>)> {
    if v.len() < 0x20 {
        return Err(Error::Parse("$INDEX_ROOT truncated".into()));
    }
    Ok((le32(v, 8), parse_node(v, 0x10)?))
}

/// Undo the fixups of an INDX block read from VCN `vcn` and parse its node.
pub fn parse_block(buf: &mut [u8], vcn: u64) -> Result<Vec<NodeEntry>> {
    apply_fixups(buf, b"INDX")?;
    if le64(buf, 0x10) != vcn {
        return Err(Error::Parse(format!("INDX block at VCN {vcn} claims VCN {}", le64(buf, 0x10))));
    }
    parse_node(buf, 0x18)
}

/// In-order walk of the B-tree rooted at `root`; `read_block` loads the node at a VCN.
/// Returns `(file reference, $FILE_NAME key)` in collation order.
pub fn walk(
    root: Vec<NodeEntry>,
    read_block: &mut dyn FnMut(u64) -> Result<Vec<NodeEntry>>,
) -> Result<Vec<(u64, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    walk_node(root, read_block, &mut visited, 0, &mut out)?;
    Ok(out)
}

fn walk_node(
    entries: Vec<NodeEntry>,
    read_block: &mut dyn FnMut(u64) -> Result<Vec<NodeEntry>>,
    visited: &mut HashSet<u64>,
    depth: usize,
    out: &mut Vec<(u64, Vec<u8>)>,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::Parse("index B-tree too deep".into()));
    }
    for e in entries {
        if let Some(vcn) = e.child {
            if !visited.insert(vcn) {
                return Err(Error::Parse(format!("index B-tree revisits VCN {vcn}")));
            }
            let sub = read_block(vcn)?;
            walk_node(sub, read_block, visited, depth + 1, out)?;
        }
        if let Some(k) = e.key {
            out.push((e.file_ref, k));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(file_ref: u64, key: Option<&[u8]>, child: Option<u64>) -> Vec<u8> {
        let klen = key.map_or(0, |k| k.len());
        let mut len = (0x10 + klen + 7) & !7;
        if child.is_some() {
            len += 8;
        }
        let mut e = vec![0u8; len];
        e[..8].copy_from_slice(&file_ref.to_le_bytes());
        e[8..10].copy_from_slice(&(len as u16).to_le_bytes());
        e[10..12].copy_from_slice(&(klen as u16).to_le_bytes());
        let flags = child.is_some() as u16 | if key.is_none() { 2 } else { 0 };
        e[12..14].copy_from_slice(&flags.to_le_bytes());
        if let Some(k) = key {
            e[0x10..0x10 + k.len()].copy_from_slice(k);
        }
        if let Some(c) = child {
            e[len - 8..].copy_from_slice(&c.to_le_bytes());
        }
        e
    }

    fn root(entries: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; 0x20];
        v[8..12].copy_from_slice(&4096u32.to_le_bytes());
        v[0x10..0x14].copy_from_slice(&0x10u32.to_le_bytes());
        v[0x14..0x18].copy_from_slice(&((0x10 + entries.len()) as u32).to_le_bytes());
        v.extend_from_slice(entries);
        v
    }

    #[test]
    fn in_order_walk_and_loops() {
        // root: [b (child 0), END (child 1)]; node 0: [a, END]; node 1: [c, END]
        let mut r = entry(2, Some(b"b"), Some(0));
        r.extend(entry(0, None, Some(1)));
        let (bs, entries) = parse_root(&root(&r)).unwrap();
        assert_eq!(bs, 4096);
        let mut read = |vcn: u64| -> Result<Vec<NodeEntry>> {
            let mut n = entry(vcn * 2 + 1, Some(if vcn == 0 { b"a" } else { b"c" }), None);
            n.extend(entry(0, None, None));
            parse_node(&root(&n), 0x10)
        };
        let keys: Vec<Vec<u8>> = walk(entries, &mut read).unwrap().into_iter().map(|(_, k)| k).collect();
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);

        // a node pointing at itself
        let mut r = entry(0, None, Some(7));
        let (_, entries) = parse_root(&root(&r)).unwrap();
        let mut self_loop = |_vcn: u64| -> Result<Vec<NodeEntry>> { parse_node(&root(&entry(0, None, Some(7))), 0x10) };
        assert!(matches!(walk(entries, &mut self_loop), Err(Error::Parse(_))));

        // missing terminator / bad lengths
        r = entry(3, Some(b"x"), None);
        assert!(parse_root(&root(&r)).is_err());
        let mut v = root(&entry(0, None, None));
        v[0x28] = 0xF0; // entry length 0xF0 runs past the node
        assert!(parse_root(&v).is_err());
        assert!(parse_root(&[0u8; 0x10]).is_err());
    }
}
