//! ext directory blocks (linear format; htree index blocks look like empty entries).

use crate::util::{le16, le32};
use pm_core::{Error, Result};

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub inode: u32,
    pub name: String,
    /// `file_type` byte (0 when the filesystem has no FILETYPE feature).
    pub file_type: u8,
}

/// `rec_len` as stored on disk, decoded for the block size.
fn rec_len(raw: u16, bs: usize) -> usize {
    if bs < 65536 {
        return raw as usize;
    }
    if raw == 0 || raw == 65535 {
        bs
    } else {
        ((raw & 0xFFFC) as usize) | (((raw & 3) as usize) << 16)
    }
}

/// Append the live entries of one directory block to `out`. Entries with inode 0 (unused
/// slots, htree fake entries, checksum tails) are skipped. Malformed record lengths are a
/// parse error, which also guarantees progress: every record advances by at least 8 bytes.
pub fn parse_block(block: &[u8], out: &mut Vec<DirEntry>) -> Result<()> {
    let bs = block.len();
    let mut pos = 0usize;
    while pos < bs {
        if pos + 8 > bs {
            return Err(Error::Parse("directory entry header crosses the block end".into()));
        }
        let inode = le32(block, pos);
        let rl = rec_len(le16(block, pos + 4), bs);
        let name_len = block[pos + 6] as usize;
        if rl < 8 || rl % 4 != 0 || pos + rl > bs || 8 + name_len > rl {
            return Err(Error::Parse(format!("corrupt directory entry (rec_len {rl}, name_len {name_len}) at offset {pos}")));
        }
        if inode != 0 {
            let name = String::from_utf8_lossy(&block[pos + 8..pos + 8 + name_len]).into_owned();
            out.push(DirEntry { inode, name, file_type: block[pos + 7] });
        }
        pos += rl;
    }
    Ok(())
}
