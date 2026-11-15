//! The engine's working copy of a [`MapFile`]'s blocks.
//!
//! `MapFile::mark` splices a `Vec`, which costs O(blocks) per call: fine for a handful of
//! bad areas, ruinous for the millions of blocks a fragmented used map produces. The engine
//! therefore marks on this ordered map (O(log blocks) per call) and keeps running totals, so
//! progress reports need no scan either. A [`MapFile`] is rebuilt from it only to save.

use super::mapfile::{BlockStatus, MapFile};
use std::collections::BTreeMap;

const STATUSES: usize = 5;

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
        BlockMap {
            // ascending input: built in linear time
            blocks: map.blocks.iter().map(|&(p, s, st)| (p, (s, st))).collect(),
            len: map.len(),
            bytes,
            count,
        }
    }

    /// The blocks in [`MapFile::blocks`] form.
    pub fn to_blocks(&self) -> Vec<(u64, u64, BlockStatus)> {
        self.blocks.iter().map(|(&p, &(s, st))| (p, s, st)).collect()
    }

    fn insert(&mut self, pos: u64, size: u64, st: BlockStatus) {
        self.bytes[idx(st)] += size;
        self.count[idx(st)] += 1;
        self.blocks.insert(pos, (size, st));
    }

    fn remove(&mut self, pos: u64) -> (u64, BlockStatus) {
        let (size, st) = self.blocks.remove(&pos).expect("block map is contiguous");
        self.bytes[idx(st)] -= size;
        self.count[idx(st)] -= 1;
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
