//! FAT directory entry parsing: 8.3 entries, long file name assembly and DOS timestamps.

use crate::traits::Entry;
use crate::util::{le16, le32, trim_padded_latin1};

pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_LFN: u8 = 0x0F;

/// Result of scanning one directory's raw bytes.
#[derive(Default)]
pub struct DirScan {
    pub live: Vec<Entry>,
    /// Deleted (0xE5) entries; names are "?" + the rest of the 8.3 name, lower-cased.
    pub deleted: Vec<Entry>,
    /// Volume label entry, if present (only meaningful for the root directory).
    pub volume_label: Option<String>,
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// DOS date/time to unix seconds (the timestamp is taken as UTC). A zero or invalid date
/// yields `None`.
pub fn dos_to_unix(date: u16, time: u16) -> Option<i64> {
    let day = (date & 0x1F) as i64;
    let month = ((date >> 5) & 0x0F) as i64;
    let year = 1980 + (date >> 9) as i64;
    if date == 0 || !(1..=12).contains(&month) || day == 0 {
        return None;
    }
    let sec = ((time & 0x1F) * 2) as i64;
    let min = ((time >> 5) & 0x3F) as i64;
    let hour = (time >> 11) as i64;
    if sec > 59 || min > 59 || hour > 23 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// LFN checksum of an 11-byte short name.
fn short_name_checksum(name: &[u8]) -> u8 {
    let mut sum = 0u8;
    for &b in &name[..11] {
        sum = (sum >> 1).wrapping_add((sum & 1) << 7).wrapping_add(b);
    }
    sum
}

fn join_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// Accumulates the pieces of a long-file-name sequence.
#[derive(Default)]
struct Lfn {
    chars: Vec<u16>,
    /// Sequence number expected next (counts down to 1); 0 when no sequence is active.
    next: u8,
    checksum: u8,
}

impl Lfn {
    fn reset(&mut self) {
        self.chars.clear();
        self.next = 0;
    }

    fn feed(&mut self, e: &[u8]) {
        let seq = e[0];
        let idx = seq & 0x1F;
        if seq & 0x40 != 0 {
            if idx == 0 {
                self.reset();
                return;
            }
            self.chars = vec![0xFFFF; idx as usize * 13];
            self.checksum = e[13];
        } else if self.next == 0 || idx != self.next || e[13] != self.checksum {
            self.reset();
            return;
        }
        let base = (idx as usize - 1) * 13;
        const SLOTS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (k, &o) in SLOTS.iter().enumerate() {
            self.chars[base + k] = le16(e, o);
        }
        self.next = idx - 1;
    }

    /// The assembled name if the sequence is complete and matches the short entry.
    fn take(&mut self, short_name: &[u8]) -> Option<String> {
        let ok = self.next == 0 && !self.chars.is_empty() && self.checksum == short_name_checksum(short_name);
        let name = if ok {
            let end = self.chars.iter().position(|&c| c == 0 || c == 0xFFFF).unwrap_or(self.chars.len());
            Some(String::from_utf16_lossy(&self.chars[..end]))
        } else {
            None
        };
        self.reset();
        name.filter(|n| !n.is_empty())
    }
}

/// Scan the raw bytes of a directory.
///
/// `parent` is the absolute path of the directory; `fat32` says whether the high cluster
/// word is meaningful.
pub fn scan(bytes: &[u8], parent: &str, fat32: bool) -> DirScan {
    let mut out = DirScan::default();
    let mut lfn = Lfn::default();
    for e in bytes.chunks_exact(32) {
        let first = e[0];
        if first == 0x00 {
            break;
        }
        let attr = e[11];
        if first == 0xE5 {
            if attr & 0x3F == ATTR_LFN || attr & ATTR_VOLUME_ID != 0 {
                lfn.reset();
                continue;
            }
            lfn.reset();
            let base = trim_padded_latin1(&e[1..8]).to_lowercase();
            let ext = trim_padded_latin1(&e[8..11]).to_lowercase();
            let name = if ext.is_empty() { format!("?{base}") } else { format!("?{base}.{ext}") };
            out.deleted.push(entry_from(e, name.clone(), join_path(parent, &name), fat32, true));
            continue;
        }
        if attr & 0x3F == ATTR_LFN {
            lfn.feed(e);
            continue;
        }
        if attr & ATTR_VOLUME_ID != 0 {
            lfn.reset();
            if out.volume_label.is_none() {
                out.volume_label = Some(trim_padded_latin1(&e[0..11]));
            }
            continue;
        }
        let mut raw = [0u8; 11];
        raw.copy_from_slice(&e[0..11]);
        let long = lfn.take(&raw);
        if raw[0] == 0x05 {
            raw[0] = 0xE5;
        }
        if &raw[..] == b".          " || &raw[..] == b"..         " {
            continue;
        }
        let name = match long {
            Some(n) => n,
            None => {
                let mut base = trim_padded_latin1(&raw[0..8]);
                let mut ext = trim_padded_latin1(&raw[8..11]);
                if e[12] & 0x08 != 0 {
                    base = base.to_lowercase();
                }
                if e[12] & 0x10 != 0 {
                    ext = ext.to_lowercase();
                }
                if ext.is_empty() {
                    base
                } else {
                    format!("{base}.{ext}")
                }
            }
        };
        let path = join_path(parent, &name);
        out.live.push(entry_from(e, name, path, fat32, false));
    }
    out
}

fn entry_from(e: &[u8], name: String, path: String, fat32: bool, deleted: bool) -> Entry {
    let attr = e[11];
    let hi = if fat32 { le16(e, 20) as u64 } else { 0 };
    let cluster = (hi << 16) | le16(e, 26) as u64;
    let is_dir = attr & ATTR_DIRECTORY != 0;
    Entry {
        name,
        path,
        is_dir,
        size: if is_dir { 0 } else { le32(e, 28) as u64 },
        modified: dos_to_unix(le16(e, 24), le16(e, 22)),
        id: cluster,
        deleted,
        attrs: attr as u32,
    }
}
