//! The engine's working copy of a [`MapFile`]'s blocks.
//!
//! `MapFile::mark` splices a `Vec`, which costs O(blocks) per call: fine for a handful of
//! bad areas, ruinous for the millions of blocks a fragmented used map produces. The engine
//! therefore marks on this ordered map (O(log blocks) per call) and keeps running totals, so
//! progress reports need no scan either. A [`MapFile`] is rebuilt from it only to save.

use super::mapfile::{BlockStatus, MapFile};
use std::collections::BTreeMap;

const STATUSES: usize = 5;

/// Slices a map is summarised in for progress reports: more than the pixels of a progress
/// bar, few enough to copy on every report.
pub(crate) const SLICES: u64 = 512;

/// Bytes of status `st` in one entry of [`BlockMap::slice_bytes`].
pub(crate) fn slice_status(slice: &[u64; STATUSES], st: BlockStatus) -> u64 {
    slice[idx(st)]
}

fn idx(st: BlockStatus) -> usize {
    match st {
        BlockStatus::NonTried => 0,
        BlockStatus::NonTrimmed => 1,
        BlockStatus::NonScraped => 2,
        BlockStatus::Bad => 3,
        BlockStatus::Finished => 4,
    }
}

/// Blocks keyed by position, with the same invariant as [`MapFile::blocks`]: non-empty,
/// contiguous from 0 to `len`, neighbours of different status.
pub(crate) struct BlockMap {
    /// `pos -> (size, status)`.
    blocks: BTreeMap<u64, (u64, BlockStatus)>,
    len: u64,
    /// Bytes per status.
    bytes: [u64; STATUSES],
    /// Blocks per status.
    count: [usize; STATUSES],
    /// Bytes per status in each of the equal slices of `[0, len)` (see [`BlockMap::bound`]),
    /// kept up to date by `insert` / `remove` so that a report never scans the blocks.
    slices: Vec<[u64; STATUSES]>,
}

/// How many slices a map of `len` bytes has: [`SLICES`], or one per byte when it is shorter.
fn slice_count(len: u64) -> usize {
    len.min(SLICES) as usize
}

impl BlockMap {
    /// From a map whose blocks satisfy the [`MapFile`] invariant.
    pub fn from_map(map: &MapFile) -> Self {
        let mut bytes = [0; STATUSES];
        let mut count = [0; STATUSES];
        for &(_, s, st) in &map.blocks {
            bytes[idx(st)] += s;
            count[idx(st)] += 1;
        }
        let mut bm = BlockMap {
            // ascending input: built in linear time
            blocks: map.blocks.iter().map(|&(p, s, st)| (p, (s, st))).collect(),
            len: map.len(),
            bytes,
            count,
            slices: vec![[0; STATUSES]; slice_count(map.len())],
        };
        for &(p, s, st) in &map.blocks {
            bm.slice_add(p, s, st, true);
        }
        bm
    }

    /// First byte of slice `i` (`i` may be the slice count: the end of the map).
    fn bound(&self, i: usize) -> u64 {
        (i as u128 * self.len as u128 / self.slices.len().max(1) as u128) as u64
    }

    /// The slice byte `pos` (inside the map) falls in.
    fn slice_of(&self, pos: u64) -> usize {
        let n = self.slices.len();
        let mut i = ((pos as u128 * n as u128 / self.len.max(1) as u128) as usize).min(n - 1);
        while self.bound(i) > pos {
            i -= 1;
        }
        while i + 1 < n && self.bound(i + 1) <= pos {
            i += 1;
        }
        i
    }

    /// Add (or take away) the bytes of block `[pos, pos + size)` to the slices it covers.
    fn slice_add(&mut self, pos: u64, size: u64, st: BlockStatus, add: bool) {
        if size == 0 || self.slices.is_empty() {
            return;
        }
        let end = pos + size;
        let mut i = self.slice_of(pos);
        loop {
            let (lo, hi) = (self.bound(i).max(pos), self.bound(i + 1).min(end));
            let cell = &mut self.slices[i][idx(st)];
            if add {
                *cell += hi - lo;
            } else {
                *cell -= hi - lo;
            }
            if hi >= end {
                break;
            }
            i += 1;
        }
    }

    /// Bytes per status in each slice, first to last, indexed like [`slice_status`].
    pub fn slice_bytes(&self) -> &[[u64; STATUSES]] {
        &self.slices
    }

    /// `(start, end)` of slice `i` in bytes.
    pub fn slice_range(&self, i: usize) -> (u64, u64) {
        (self.bound(i), self.bound(i + 1))
    }

    /// The blocks in [`MapFile::blocks`] form.
    pub fn to_blocks(&self) -> Vec<(u64, u64, BlockStatus)> {
        self.blocks.iter().map(|(&p, &(s, st))| (p, s, st)).collect()
    }

    fn insert(&mut self, pos: u64, size: u64, st: BlockStatus) {
        self.bytes[idx(st)] += size;
        self.count[idx(st)] += 1;
        self.blocks.insert(pos, (size, st));
        self.slice_add(pos, size, st, true);
    }

    fn remove(&mut self, pos: u64) -> (u64, BlockStatus) {
        let (size, st) = self.blocks.remove(&pos).expect("block map is contiguous");
        self.bytes[idx(st)] -= size;
        self.count[idx(st)] -= 1;
        self.slice_add(pos, size, st, false);
        (size, st)
    }

    /// Set `[pos, pos + len)` (clipped to the map) to `st`: [`MapFile::mark`], in
    /// O(log blocks) amortised.
    pub fn mark(&mut self, pos: u64, len: u64, st: BlockStatus) {
        if len == 0 || pos >= self.len {
            return;
        }
        let end = pos.saturating_add(len).min(self.len);
        let (&first, &(first_size, first_st)) =
            self.blocks.range(..=pos).next_back().expect("block map starts at 0");
        if first_st == st && first + first_size >= end {
            return;
        }
        // remove every block overlapping [pos, end), keeping what sticks out on either side
        // (as part of the new block where it already has the new status)
        let (mut new_start, mut new_end) = (pos, end);
        let mut k = first;
        loop {
            let (size, old) = self.remove(k);
            let e = k + size;
            if k < pos {
                if old == st {
                    new_start = k;
                } else {
                    self.insert(k, pos - k, old);
                }
            }
            if e > end {
                if old == st {
                    new_end = e;
                } else {
                    self.insert(end, e - end, old);
                }
            }
            if e >= end {
                break;
            }
            k = e;
        }
        // coalesce with untouched neighbours of the same status
        if new_start == pos {
            if let Some((&p, &(_, pst))) = self.blocks.range(..pos).next_back() {
                if pst == st {
                    self.remove(p);
                    new_start = p;
                }
            }
        }
        if new_end == end {
            if let Some(&(size, nst)) = self.blocks.get(&end) {
                if nst == st {
                    self.remove(end);
                    new_end = end + size;
                }
            }
        }
        self.insert(new_start, new_end - new_start, st);
    }

    /// Ascending `(pos, size)` of every area with status `st`: [`MapFile::ranges`].
    pub fn ranges(&self, st: BlockStatus) -> Vec<(u64, u64)> {
        self.blocks.iter().filter(|(_, b)| b.1 == st).map(|(&p, &(s, _))| (p, s)).collect()
    }

    pub fn has(&self, st: BlockStatus) -> bool {
        self.count[idx(st)] > 0
    }

    /// Bytes with status `st`.
    pub fn bytes(&self, st: BlockStatus) -> u64 {
        self.bytes[idx(st)]
    }

    /// Bytes that are not finished.
    pub fn unrescued_bytes(&self) -> u64 {
        self.len - self.bytes(BlockStatus::Finished)
    }

    /// Number of maximal areas that are not finished (bad, non-tried, non-trimmed or
    /// non-scraped, adjacent ones counted once): what ddrescue reports as "errors".
    pub fn unrescued_areas(&self) -> u64 {
        if self.blocks.is_empty() {
            return 0;
        }
        // finished blocks are never adjacent, so they separate the unrescued areas: one
        // more area than separators, less one for each end of the map that is finished
        let finished = |b: Option<(&u64, &(u64, BlockStatus))>| {
            b.is_some_and(|(_, b)| b.1 == BlockStatus::Finished) as u64
        };
        let separators = self.count[idx(BlockStatus::Finished)] as u64;
        separators + 1
            - finished(self.blocks.first_key_value())
            - finished(self.blocks.last_key_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BlockStatus::*;

    fn areas_by_scan(blocks: &[(u64, u64, BlockStatus)]) -> u64 {
        let mut areas = 0;
        let mut prev = false;
        for b in blocks {
            let unrescued = b.2 != Finished;
            areas += (unrescued && !prev) as u64;
            prev = unrescued;
        }
        areas
    }

    fn check(bm: &BlockMap, reference: &MapFile) {
        assert_eq!(bm.to_blocks(), reference.blocks);
        let st = reference.stats();
        assert_eq!(bm.bytes(Finished), st.rescued);
        assert_eq!(bm.bytes(NonTried), st.non_tried);
        assert_eq!(bm.bytes(NonTrimmed), st.non_trimmed);
        assert_eq!(bm.bytes(NonScraped), st.non_scraped);
        assert_eq!(bm.bytes(Bad), st.bad_bytes);
        assert_eq!(bm.unrescued_bytes(), reference.len() - st.rescued);
        assert_eq!(bm.unrescued_areas(), areas_by_scan(&reference.blocks));
        for s in [NonTried, NonTrimmed, NonScraped, Bad, Finished] {
            assert_eq!(bm.ranges(s), reference.ranges(s));
            assert_eq!(bm.has(s), reference.blocks.iter().any(|b| b.2 == s));
        }
        assert_eq!(bm.slice_bytes(), slices_by_scan(reference).as_slice());
    }

    /// The per-slice bytes, counted byte range by byte range from the plain blocks.
    fn slices_by_scan(m: &MapFile) -> Vec<[u64; STATUSES]> {
        let n = slice_count(m.len());
        let bound = |i: usize| (i as u128 * m.len() as u128 / n as u128) as u64;
        (0..n)
            .map(|i| {
                let (lo, hi) = (bound(i), bound(i + 1));
                let mut b = [0; STATUSES];
                for &(p, s, st) in &m.blocks {
                    b[idx(st)] += (p + s).min(hi).saturating_sub(p.max(lo));
                }
                b
            })
            .collect()
    }

    #[test]
    fn slices_follow_the_marks() {
        // 5120 bytes in 512 slices of 10
        let mut bm = BlockMap::from_map(&MapFile::new(5120));
        assert_eq!(bm.slice_bytes().len(), 512);
        assert_eq!(bm.slice_range(3), (30, 40));
        assert!(bm.slice_bytes().iter().all(|s| slice_status(s, NonTried) == 10));
        // a range over three slices: 5 bytes of the first, all of the second, 2 of the third
        bm.mark(15, 17, Bad);
        let bad: Vec<u64> = bm.slice_bytes()[..4].iter().map(|s| slice_status(s, Bad)).collect();
        assert_eq!(bad, [0, 5, 10, 2]);
        assert_eq!(slice_status(&bm.slice_bytes()[1], NonTried), 5);
        // rescued on a retry: the bad bytes are gone from the slices
        bm.mark(15, 17, Finished);
        assert!(bm.slice_bytes().iter().all(|s| slice_status(s, Bad) == 0));
        assert_eq!(slice_status(&bm.slice_bytes()[2], Finished), 10);
        // a map shorter than the slice count has one slice per byte; an uneven one loses none
        assert_eq!(BlockMap::from_map(&MapFile::new(7)).slice_bytes().len(), 7);
        let odd = BlockMap::from_map(&MapFile::new(1000));
        assert_eq!(odd.slice_bytes().iter().map(|s| slice_status(s, NonTried)).sum::<u64>(), 1000);
        assert_eq!(odd.slice_range(511).1, 1000);
    }

    /// Every mark gives exactly what `MapFile::mark` gives, totals included.
    #[test]
    fn mark_matches_mapfile_mark() {
        const LEN: u64 = 4096;
        let statuses = [NonTried, NonTrimmed, NonScraped, Bad, Finished];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            // xorshift64
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..50 {
            let mut reference = MapFile::new(LEN);
            let mut bm = BlockMap::from_map(&reference);
            check(&bm, &reference);
            for _ in 0..400 {
                let pos = next() % (LEN + 64);
                // mostly short ranges, sometimes long ones spanning many blocks
                let len = if next() % 8 == 0 { next() % (LEN + 64) } else { next() % 48 };
                let st = statuses[(next() % 5) as usize];
                reference.mark(pos, len, st);
                bm.mark(pos, len, st);
                check(&bm, &reference);
            }
        }
    }

    #[test]
    fn empty_map() {
        let mut bm = BlockMap::from_map(&MapFile::new(0));
        bm.mark(0, 10, Finished);
        assert!(bm.to_blocks().is_empty());
        assert_eq!(bm.unrescued_areas(), 0);
        assert_eq!(bm.unrescued_bytes(), 0);
        assert!(bm.slice_bytes().is_empty());
    }

    #[test]
    fn from_map_counts_existing_blocks() {
        let mut m = MapFile::new(1000);
        m.mark(100, 100, Finished);
        m.mark(500, 10, Bad);
        m.mark(510, 10, NonScraped);
        let bm = BlockMap::from_map(&m);
        check(&bm, &m);
        assert_eq!(bm.unrescued_areas(), 2);
    }
}
