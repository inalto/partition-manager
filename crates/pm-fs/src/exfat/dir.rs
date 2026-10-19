//! exFAT directory entry sets.

use crate::util::{le16, le32, le64};
use pm_core::{Error, Result};

const ENTRY: usize = 32;

/// One file or directory entry set.
#[derive(Clone, Debug)]
pub struct FileSet {
    /// The primary entry was in use (type 0x85) rather than deleted (0x05).
    pub in_use: bool,
    pub attrs: u16,
    pub modified: Option<i64>,
    pub first_cluster: u32,
    /// Stream flag `NoFatChain`: the clusters are contiguous.
    pub nofat: bool,
    pub data_len: u64,
    /// ValidDataLength: bytes at or beyond it read as zeros (never above `data_len`).
    pub valid_len: u64,
    pub name: String,
}

impl FileSet {
    pub fn is_dir(&self) -> bool {
        self.attrs & 0x10 != 0
    }
}

/// An allocation bitmap entry (0x81).
#[derive(Clone, Copy, Debug)]
pub struct BitmapEntry {
    pub first_cluster: u32,
    pub data_len: u64,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub sets: Vec<FileSet>,
    pub bitmaps: Vec<BitmapEntry>,
    pub label: Option<String>,
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An exFAT timestamp (packed DOS-style date and time, plus a 10 ms increment and a UTC offset
/// in 15-minute units whose bit 7 says it is valid) as unix seconds. Without a valid offset the
/// local time is taken as UTC. `None` for an unset or impossible date.
pub fn exfat_time(ts: u32, inc_10ms: u8, utc: u8) -> Option<i64> {
    let year = 1980 + (ts >> 25) as i64;
    let month = ((ts >> 21) & 0xF) as i64;
    let day = ((ts >> 16) & 0x1F) as i64;
    let hour = ((ts >> 11) & 0x1F) as i64;
    let min = ((ts >> 5) & 0x3F) as i64;
    let sec = ((ts & 0x1F) * 2) as i64;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    let mut t = days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec + (inc_10ms as i64) / 100;
    if utc & 0x80 != 0 {
        let off = (((utc & 0x7F) << 1) as i8 >> 1) as i64; // sign-extend 7 bits
        t -= off * 900;
    }
    Some(t)
}

fn utf16(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied()).map(|r| r.unwrap_or('\u{FFFD}')).collect()
}

/// Parse a directory's bytes. `strict` makes a damaged in-use entry set an error; otherwise
/// it is skipped. `deleted` also returns the sets whose primary entry has bit 7 cleared (type
/// 0x05) with a plausible stream and name; those have `in_use == false`.
///
/// The set checksum is not verified: a mismatching set is still returned, which is what a
/// recovery tool wants.
pub fn parse(bytes: &[u8], strict: bool, deleted: bool) -> Result<Parsed> {
    let n = bytes.len() / ENTRY;
    let mut out = Parsed::default();
    let rec = |i: usize| &bytes[i * ENTRY..(i + 1) * ENTRY];
    let mut i = 0;
    while i < n {
        let e = rec(i);
        match e[0] {
            0x00 => break,
            0x81 => out.bitmaps.push(BitmapEntry { first_cluster: le32(e, 20), data_len: le64(e, 24) }),
            0x83 => {
                let chars = (e[1] as usize).min(11);
                let units: Vec<u16> = (0..chars).map(|k| le16(e, 2 + 2 * k)).collect();
                out.label = Some(utf16(&units));
            }
            0x85 => {
                let secondary = e[1] as usize;
                match in_use_set(bytes, i, secondary) {
                    Ok(s) => out.sets.push(s),
                    Err(err) if strict => return Err(err),
                    Err(_) => {}
                }
                i += 1 + secondary;
                continue;
            }
            0x05 if deleted => {
                if let Some(s) = deleted_set(bytes, i) {
                    out.sets.push(s);
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(out)
}

/// Fields shared by in-use and deleted sets, from the primary entry and the stream extension.
fn build(bytes: &[u8], i: usize, stream: usize, names: &[usize], in_use: bool) -> Option<FileSet> {
    let p = &bytes[i * ENTRY..(i + 1) * ENTRY];
    let s = &bytes[stream * ENTRY..(stream + 1) * ENTRY];
    let name_len = s[3] as usize;
    let mut units: Vec<u16> = Vec::with_capacity(name_len);
    for &k in names {
        let ne = &bytes[k * ENTRY..(k + 1) * ENTRY];
        for j in 0..15 {
            units.push(le16(ne, 2 + 2 * j));
        }
    }
    if name_len == 0 || units.len() < name_len {
        return None;
    }
    units.truncate(name_len);
    let modified = exfat_time(le32(p, 12), p[21], p[24]);
    Some(FileSet {
        in_use,
        attrs: le16(p, 4),
        modified,
        first_cluster: le32(s, 20),
        nofat: s[1] & 0x02 != 0,
        data_len: le64(s, 24),
        valid_len: le64(s, 8).min(le64(s, 24)),
        name: utf16(&units),
    })
}

fn in_use_set(bytes: &[u8], i: usize, secondary: usize) -> Result<FileSet> {
    let n = bytes.len() / ENTRY;
    let bad = |m: &str| Error::Parse(format!("exFAT entry set at slot {i}: {m}"));
    if secondary < 2 || i + 1 + secondary > n {
        return Err(bad("secondary entries missing or past the end of the directory"));
    }
    let stream = i + 1;
    if bytes[stream * ENTRY] != 0xC0 {
        return Err(bad("no stream extension entry"));
    }
    let names: Vec<usize> = (i + 2..=i + secondary).filter(|&k| bytes[k * ENTRY] == 0xC1).collect();
    build(bytes, i, stream, &names, true).ok_or_else(|| bad("name entries do not cover the name length"))
}

/// A deleted set: stream and names follow the primary with their in-use bits cleared. The
/// look-ahead stops at the first in-use entry, which belongs to a later set.
fn deleted_set(bytes: &[u8], i: usize) -> Option<FileSet> {
    let n = bytes.len() / ENTRY;
    let secondary = bytes[i * ENTRY + 1] as usize;
    let last = (i + secondary).min(n - 1);
    let stream = i + 1;
    if stream > last || bytes[stream * ENTRY] != 0x40 {
        return None;
    }
    let mut names = Vec::new();
    for k in i + 2..=last {
        match bytes[k * ENTRY] {
            0x41 => names.push(k),
            _ => break,
        }
    }
    build(bytes, i, stream, &names, false)
}
