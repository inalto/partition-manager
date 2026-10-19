//! GNU ddrescue-compatible mapfile: the rescue state of every byte of the source.
//!
//! Format (GNU ddrescue 1.2x):
//!
//! ```text
//! # Mapfile. Created by pm (Partition Manager)
//! # current_pos  current_status  current_pass
//! 0x00000000     ?               1
//! #      pos        size  status
//! 0x00000000  0x00100000  +
//! 0x00100000  0x00000200  -
//! ```
//!
//! Lines starting with `#` are comments. The first data line is the status line
//! (`current_pos current_status [current_pass]`), every further one a block
//! (`pos size status`). Numbers are read like C `strtoull(.., 0)` (`0x` hex, leading-`0`
//! octal, else decimal) and written as `0x` + lowercase hex padded to 8 digits.

use pm_core::{Error, Result};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Rescue state of a block of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockStatus {
    /// `?`: not read yet.
    NonTried,
    /// `*`: a large read failed somewhere inside; edges not trimmed yet.
    NonTrimmed,
    /// `/`: trimmed; the middle still needs a sector-by-sector scrape.
    NonScraped,
    /// `-`: a bad sector (or run of them).
    Bad,
    /// `+`: rescued (copied to the destination).
    Finished,
}

impl BlockStatus {
    pub fn as_char(self) -> char {
        match self {
            BlockStatus::NonTried => '?',
            BlockStatus::NonTrimmed => '*',
            BlockStatus::NonScraped => '/',
            BlockStatus::Bad => '-',
            BlockStatus::Finished => '+',
        }
    }

    pub fn from_char(c: char) -> Option<Self> {
        Some(match c {
            '?' => BlockStatus::NonTried,
            '*' => BlockStatus::NonTrimmed,
            '/' => BlockStatus::NonScraped,
            '-' => BlockStatus::Bad,
            '+' => BlockStatus::Finished,
            _ => return None,
        })
    }
}

/// Byte totals per status of a [`MapFile`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MapStats {
    pub rescued: u64,
    pub non_tried: u64,
    pub non_trimmed: u64,
    pub non_scraped: u64,
    pub bad_bytes: u64,
    /// Number of (maximal, coalesced) bad-sector areas.
    pub bad_areas: u64,
}

/// A ddrescue mapfile. `blocks` are `(pos, size, status)`, kept non-empty, contiguous
/// from 0 to [`MapFile::len`] and coalesced by every method; after mutating the field
/// directly, repair with [`MapFile::merge_coalesce`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapFile {
    pub current_pos: u64,
    /// Phase of the run that wrote the file: `?` copying, `*` trimming, `/` scraping,
    /// `-` retrying, `+` finished.
    pub current_status: char,
    pub pass: u32,
    pub blocks: Vec<(u64, u64, BlockStatus)>,
}

/// First header line of every mapfile we write.
pub(crate) const HEADER: &str = "# Mapfile. Created by pm (Partition Manager)";

fn parse_num(tok: &str, line_no: usize) -> Result<u64> {
    let bad = || Error::Parse(format!("mapfile line {line_no}: invalid number {tok:?}"));
    let hex = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X"));
    let (digits, radix) = if let Some(h) = hex {
        (h, 16)
    } else if tok.len() > 1 && tok.starts_with('0') {
        (&tok[1..], 8)
    } else {
        (tok, 10)
    };
    if digits.is_empty() || digits.starts_with(['+', '-']) {
        return Err(bad());
    }
    u64::from_str_radix(digits, radix).map_err(|_| bad())
}

fn single_char(tok: &str) -> Option<char> {
    let mut it = tok.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

impl MapFile {
    /// A map of `len` bytes, all non-tried, at the start of the copy phase.
    pub fn new(len: u64) -> Self {
        MapFile {
            current_pos: 0,
            current_status: '?',
            pass: 1,
            blocks: if len == 0 {
                Vec::new()
            } else {
                vec![(0, len, BlockStatus::NonTried)]
            },
        }
    }

    /// End of the last block: the size of the rescue domain.
    pub fn len(&self) -> u64 {
        self.blocks.last().map_or(0, |&(p, s, _)| p + s)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Parse GNU ddrescue (1.2x, and the older 2-field status line) mapfile text.
    ///
    /// Accepts any comment lines, CRLF, surrounding whitespace and a missing
    /// `current_pass` (defaults to 1). `F`/`G` (filling/generating) statuses are read as
    /// `+`. Gaps between blocks (and before the first) become non-tried; overlapping or
    /// unsorted blocks are a parse error. Zero-size blocks are dropped and neighbours with
    /// the same status are merged.
    pub fn parse(s: &str) -> Result<Self> {
        let mut status: Option<(u64, char, u32)> = None;
        let mut blocks: Vec<(u64, u64, BlockStatus)> = Vec::new();
        let mut end = 0u64;
        for (idx, raw) in s.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let toks: Vec<&str> = line.split_whitespace().collect();
            if status.is_none() {
                if !(2..=3).contains(&toks.len()) {
                    return Err(Error::Parse(format!(
                        "mapfile line {line_no}: expected \
                         'current_pos current_status [current_pass]'"
                    )));
                }
                let pos = parse_num(toks[0], line_no)?;
                let c = match single_char(toks[1]) {
                    Some(c @ ('?' | '*' | '/' | '-' | '+')) => c,
                    Some('F' | 'G') => '+',
                    _ => {
                        return Err(Error::Parse(format!(
                            "mapfile line {line_no}: invalid current_status {:?}",
                            toks[1]
                        )))
                    }
                };
                let pass = match toks.get(2) {
                    Some(t) => u32::try_from(parse_num(t, line_no)?).map_err(|_| {
                        Error::Parse(format!("mapfile line {line_no}: current_pass too large"))
                    })?,
                    None => 1,
                };
                status = Some((pos, c, pass));
                continue;
            }
            if toks.len() != 3 {
                return Err(Error::Parse(format!(
                    "mapfile line {line_no}: expected 'pos size status'"
                )));
            }
            let pos = parse_num(toks[0], line_no)?;
            let size = parse_num(toks[1], line_no)?;
            let st = single_char(toks[2]).and_then(BlockStatus::from_char).ok_or_else(|| {
                Error::Parse(format!(
                    "mapfile line {line_no}: invalid block status {:?}",
                    toks[2]
                ))
            })?;
            let block_end = pos.checked_add(size).ok_or_else(|| {
                Error::Parse(format!("mapfile line {line_no}: block end overflows"))
            })?;
            if pos < end {
                return Err(Error::Parse(format!(
                    "mapfile line {line_no}: block at {pos:#x} overlaps or is out of order"
                )));
            }
            if size == 0 {
                continue;
            }
            if pos > end {
                blocks.push((end, pos - end, BlockStatus::NonTried));
            }
            blocks.push((pos, size, st));
            end = block_end;
        }
        let (current_pos, current_status, pass) =
            status.ok_or_else(|| Error::Parse("mapfile has no status line".into()))?;
        let mut m = MapFile {
            current_pos,
            current_status,
            pass,
            blocks,
        };
        m.merge_coalesce();
        Ok(m)
    }

    /// Read and parse a mapfile.
    pub fn load(p: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(p).map_err(|e| Error::io(0, 0, e))?;
        Self::parse(&s)
    }

    /// Write the mapfile atomically and durably: `<path>.tmp` is written and fsynced, then
    /// renamed over `path`, so a crash leaves either the old or the new complete file.
    pub fn save(&self, p: &Path) -> Result<()> {
        self.save_with_comments(p, &[])
    }

    /// Like [`MapFile::save`], with extra `# ...` comment lines after the header.
    pub fn save_with_comments(&self, p: &Path, comments: &[String]) -> Result<()> {
        let mut tmp = p.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let io = |e| Error::io(0, 0, e);
        let mut f = std::fs::File::create(&tmp).map_err(io)?;
        f.write_all(self.render(comments).as_bytes()).map_err(io)?;
        f.sync_all().map_err(io)?;
        drop(f);
        std::fs::rename(&tmp, p).map_err(io)
    }

    /// The mapfile text with extra comment lines after the header.
    pub fn render(&self, comments: &[String]) -> String {
        let mut s = String::with_capacity(160 + self.blocks.len() * 28);
        s.push_str(HEADER);
        s.push('\n');
        for c in comments {
            for l in c.lines() {
                s.push_str("# ");
                s.push_str(l);
                s.push('\n');
            }
        }
        s.push_str("# current_pos  current_status  current_pass\n");
        s.push_str(&format!(
            "0x{:08x}     {}               {}\n",
            self.current_pos, self.current_status, self.pass
        ));
        s.push_str("#      pos        size  status\n");
        for &(p, sz, st) in &self.blocks {
            s.push_str(&format!("0x{p:08x}  0x{sz:08x}  {}\n", st.as_char()));
        }
        s
    }

    /// Set `[pos, pos + len)` (clipped to the map) to `st`, keeping the blocks contiguous
    /// and coalesced.
    pub fn mark(&mut self, pos: u64, len: u64, st: BlockStatus) {
        let total = self.len();
        if len == 0 || pos >= total {
            return;
        }
        let end = pos.saturating_add(len).min(total);
        let i = self.blocks.partition_point(|b| b.0 + b.1 <= pos);
        let j = self.blocks.partition_point(|b| b.0 < end) - 1;
        let (fs, _, fst) = self.blocks[i];
        let (ls, lsz, lst) = self.blocks[j];
        let mut repl = Vec::with_capacity(3);
        if fs < pos {
            repl.push((fs, pos - fs, fst));
        }
        repl.push((pos, end - pos, st));
        if ls + lsz > end {
            repl.push((end, ls + lsz - end, lst));
        }
        let n = repl.len();
        self.blocks.splice(i..=j, repl);
        self.coalesce_window(i.saturating_sub(1), i + n);
    }

    /// Set every range of `ranges` (ascending, non-overlapping) to `st` in one linear pass.
    /// Returns the pieces whose status actually changed, ascending and coalesced.
    pub(crate) fn mark_many(&mut self, ranges: &[(u64, u64)], st: BlockStatus) -> Vec<(u64, u64)> {
        fn push(v: &mut Vec<(u64, u64, BlockStatus)>, p: u64, s: u64, st: BlockStatus) {
            if s == 0 {
                return;
            }
            match v.last_mut() {
                Some(last) if last.2 == st && last.0 + last.1 == p => last.1 += s,
                _ => v.push((p, s, st)),
            }
        }
        let mut out: Vec<(u64, u64, BlockStatus)> =
            Vec::with_capacity(self.blocks.len() + 2 * ranges.len());
        let mut changed: Vec<(u64, u64)> = Vec::new();
        let mut r = 0usize;
        for &(bp, bs, bst) in &self.blocks {
            let be = bp + bs;
            let mut cur = bp;
            while cur < be {
                while r < ranges.len() && ranges[r].0.saturating_add(ranges[r].1) <= cur {
                    r += 1;
                }
                match ranges.get(r) {
                    Some(&(rs, rl)) if rs < be => {
                        if rs > cur {
                            push(&mut out, cur, rs - cur, bst);
                            cur = rs;
                        }
                        let pe = rs.saturating_add(rl).min(be);
                        push(&mut out, cur, pe - cur, st);
                        if bst != st {
                            match changed.last_mut() {
                                Some(c) if c.0 + c.1 == cur => c.1 += pe - cur,
                                _ => changed.push((cur, pe - cur)),
                            }
                        }
                        cur = pe;
                    }
                    _ => {
                        push(&mut out, cur, be - cur, bst);
                        cur = be;
                    }
                }
            }
        }
        self.blocks = out;
        changed
    }

    /// Merge neighbours with equal status among `blocks[lo..=hi]` (clamped to the vector).
    fn coalesce_window(&mut self, lo: usize, hi: usize) {
        let mut k = lo;
        let mut hi = hi;
        while k < hi && k + 1 < self.blocks.len() {
            let (a, b) = (self.blocks[k], self.blocks[k + 1]);
            if a.2 == b.2 && a.0 + a.1 == b.0 {
                self.blocks[k].1 += b.1;
                self.blocks.remove(k + 1);
                hi -= 1;
            } else {
                k += 1;
            }
        }
    }

    /// Ascending `(pos, size)` of every maximal area with status `st`.
    pub fn ranges(&self, st: BlockStatus) -> Vec<(u64, u64)> {
        let mut v: Vec<(u64, u64)> = Vec::new();
        for &(p, s, bst) in &self.blocks {
            if bst != st || s == 0 {
                continue;
            }
            match v.last_mut() {
                Some(last) if last.0 + last.1 == p => last.1 += s,
                _ => v.push((p, s)),
            }
        }
        v
    }

    /// Byte totals per status and the number of bad areas.
    pub fn stats(&self) -> MapStats {
        let mut m = MapStats::default();
        let mut prev_bad_end: Option<u64> = None;
        for &(p, s, st) in &self.blocks {
            match st {
                BlockStatus::NonTried => m.non_tried += s,
                BlockStatus::NonTrimmed => m.non_trimmed += s,
                BlockStatus::NonScraped => m.non_scraped += s,
                BlockStatus::Finished => m.rescued += s,
                BlockStatus::Bad => {
                    m.bad_bytes += s;
                    if s > 0 && prev_bad_end != Some(p) {
                        m.bad_areas += 1;
                    }
                    prev_bad_end = Some(p + s);
                }
            }
        }
        m
    }

    /// Drop empty blocks and merge adjacent blocks with the same status.
    pub fn merge_coalesce(&mut self) {
        let mut out: Vec<(u64, u64, BlockStatus)> = Vec::with_capacity(self.blocks.len());
        for &(p, s, st) in &self.blocks {
            if s == 0 {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.2 == st && last.0 + last.1 == p => last.1 += s,
                _ => out.push((p, s, st)),
            }
        }
        self.blocks = out;
    }
}

/// GNU ddrescue mapfile text (accepted by GNU ddrescue 1.2x); provides `to_string()`.
impl fmt::Display for MapFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(&[]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BlockStatus::*;

    #[test]
    fn mark_many_splits_and_reports_changes() {
        let mut m = MapFile::new(10_000);
        m.mark(1000, 1000, Finished);
        m.mark(5000, 500, Bad);
        let changed = m.mark_many(&[(0, 1500), (4000, 1200), (9000, 5000)], Finished);
        assert_eq!(changed, vec![(0, 1000), (4000, 1200), (9000, 1000)]);
        assert_eq!(
            m.blocks,
            vec![
                (0, 2000, Finished),
                (2000, 2000, NonTried),
                (4000, 1200, Finished),
                (5200, 300, Bad),
                (5500, 3500, NonTried),
                (9000, 1000, Finished)
            ]
        );
        assert!(m.mark_many(&[], Bad).is_empty());
        assert_eq!(m.blocks.len(), 6);
    }
}
