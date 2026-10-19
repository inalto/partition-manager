//! Pure geometry of the MiniTool-style resize/move bar, shared by the Resize/Move wizard and
//! the size step of the Create wizard.
//!
//! The bar spans an [`Extent`]: the largest range the partition may occupy (from the end of
//! its left neighbour to the start of its right neighbour). The partition is a block inside
//! it with a left handle (start), a right handle (end) and a body (move). Every edit goes
//! through one of the `set_*` / [`Extent::drag`] functions, which snap edges to the grid
//! (1 MiB) and keep the block inside the extent and at least [`Extent::min_len`] long.

use pm_core::{align_down, align_up};

/// Which part of the bar is being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Start,
    End,
    Body,
}

/// The range a partition may occupy and the rules its edges follow. All values in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// Lowest start: end of the previous partition, or start of the usable area.
    pub lo: u64,
    /// Highest end: start of the next partition, or end of the usable area.
    pub hi: u64,
    /// Smallest length: the used data (shown hatched), at least one grid step.
    pub min_len: u64,
    /// Edges snap to multiples of this.
    pub grid: u64,
    /// An off-grid start that is still allowed (the partition's current start: an in-place
    /// resize may keep a legacy, unaligned start).
    pub keep_start: Option<u64>,
    /// An off-grid end that is still allowed (the partition's current end).
    pub keep_end: Option<u64>,
    /// Whether the end may sit exactly on `hi` even when `hi` is off the grid (the end of a
    /// GPT's usable area).
    pub end_at_hi: bool,
}

impl Extent {
    fn span(&self) -> u64 {
        self.hi.saturating_sub(self.lo)
    }

    /// X position (0..=width) of byte offset `b` on a bar `width` points wide.
    pub fn x_of(&self, b: u64, width: f32) -> f32 {
        let span = self.span();
        if span == 0 {
            return 0.0;
        }
        let rel = b.clamp(self.lo, self.hi) - self.lo;
        (rel as f64 / span as f64 * width.max(0.0) as f64) as f32
    }

    /// Byte offset at X position `x` of a bar `width` points wide (clamped to the extent).
    pub fn bytes_at(&self, x: f32, width: f32) -> u64 {
        if width <= 0.0 {
            return self.lo;
        }
        let f = (x / width).clamp(0.0, 1.0) as f64;
        self.lo + (f * self.span() as f64).round() as u64
    }

    /// Bytes represented by `dx` points (signed) on a bar `width` points wide.
    pub fn delta_bytes(&self, dx: f32, width: f32) -> i64 {
        if width <= 0.0 {
            return 0;
        }
        (dx as f64 / width as f64 * self.span() as f64).round() as i64
    }

    /// The candidate closest to `v` among the grid points around `v` and `extra`, restricted
    /// to `[lo, hi]`; `v` clamped to `[lo, hi]` when there is no candidate (a range narrower
    /// than the grid).
    fn snap_within(&self, v: u64, lo: u64, hi: u64, extra: &[Option<u64>]) -> u64 {
        if hi < lo {
            return lo;
        }
        let g = self.grid.max(1);
        let v = v.clamp(lo, hi);
        [Some(align_down(v, g)), Some(align_up(v, g))]
            .iter()
            .chain(extra.iter())
            .flatten()
            .copied()
            .filter(|c| (lo..=hi).contains(c))
            .min_by_key(|c| c.abs_diff(v))
            .unwrap_or(v)
    }

    /// Move the left edge to `new_start`, keeping the end.
    pub fn set_start(&self, start: u64, len: u64, new_start: u64) -> (u64, u64) {
        let end = start.saturating_add(len).min(self.hi);
        let max_start = end.saturating_sub(self.min_len);
        let s = self.snap_within(new_start, self.lo, max_start, &[self.keep_start]);
        (s, end.saturating_sub(s))
    }

    /// Move the right edge to `new_end`, keeping the start.
    pub fn set_end(&self, start: u64, _len: u64, new_end: u64) -> (u64, u64) {
        let min_end = start.saturating_add(self.min_len);
        let hi_ok = self.end_at_hi.then_some(self.hi);
        let e = self.snap_within(new_end, min_end, self.hi, &[self.keep_end, hi_ok]);
        (start, e.saturating_sub(start))
    }

    /// Move the whole block to `new_start`, keeping its length. A block that leaves its
    /// current (possibly unaligned) start gets a grid-multiple length, so both of its edges
    /// land on the grid.
    pub fn move_to(&self, _start: u64, len: u64, new_start: u64) -> (u64, u64) {
        let g = self.grid.max(1);
        let len = len.min(self.span());
        let grid_len = if len % g == 0 {
            len
        } else {
            let down = align_down(len, g);
            if down >= self.min_len {
                down
            } else {
                align_up(self.min_len, g).min(align_down(self.span(), g))
            }
        };
        let s = self.snap_within(new_start, self.lo, self.hi.saturating_sub(grid_len), &[self.keep_start]);
        // on the original start nothing moves: any length is fine there
        if Some(s) == self.keep_start && s.saturating_add(len) <= self.hi {
            (s, len)
        } else {
            (s, grid_len)
        }
    }

    /// Apply a drag of `delta` bytes on `handle` to the block `(start0, len0)` it started from.
    pub fn drag(&self, handle: Handle, start0: u64, len0: u64, delta: i64) -> (u64, u64) {
        let shift = |v: u64| v.saturating_add_signed(delta);
        match handle {
            Handle::Start => self.set_start(start0, len0, shift(start0)),
            Handle::End => self.set_end(start0, len0, shift(start0.saturating_add(len0))),
            Handle::Body => self.move_to(start0, len0, shift(start0)),
        }
    }

    /// "Unallocated space before": keep the size, move the block.
    pub fn set_before(&self, start: u64, len: u64, before: u64) -> (u64, u64) {
        self.move_to(start, len, self.lo.saturating_add(before))
    }

    /// "Partition size": keep the start, move the end.
    pub fn set_size(&self, start: u64, len: u64, size: u64) -> (u64, u64) {
        self.set_end(start, len, start.saturating_add(size))
    }

    /// "Unallocated space after": keep the start, move the end.
    pub fn set_after(&self, start: u64, len: u64, after: u64) -> (u64, u64) {
        self.set_end(start, len, self.hi.saturating_sub(after))
    }

    /// `(before, size, after)` of the block `(start, len)`.
    pub fn fields(&self, start: u64, len: u64) -> (u64, u64, u64) {
        let end = start.saturating_add(len);
        (start.saturating_sub(self.lo), len, self.hi.saturating_sub(end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::MIB;

    /// A partition at 100 MiB + 500 MiB with free neighbours from 50 MiB to 1000 MiB and
    /// 200 MiB of used data.
    fn ext() -> Extent {
        Extent {
            lo: 50 * MIB,
            hi: 1000 * MIB,
            min_len: 200 * MIB,
            grid: MIB,
            keep_start: Some(100 * MIB),
            keep_end: Some(600 * MIB),
            end_at_hi: true,
        }
    }

    const M: i64 = MIB as i64;

    #[test]
    fn resize_bar_math() {
        let e = ext();
        let w = 950.0; // one point per MiB
        // pixels <-> bytes round trip
        assert_eq!(e.x_of(e.lo, w), 0.0);
        assert_eq!(e.x_of(e.hi, w), w);
        assert_eq!(e.x_of(0, w), 0.0, "clamped below");
        assert_eq!(e.x_of(u64::MAX, w), w, "clamped above");
        for b in [50 * MIB, 100 * MIB + 12345, 600 * MIB, 999 * MIB] {
            let back = e.bytes_at(e.x_of(b, w), w);
            assert!(back.abs_diff(b) <= MIB / 1000, "{b} -> {back}");
        }
        for x in [0.0f32, 10.5, 475.0, 949.9] {
            assert!((e.x_of(e.bytes_at(x, w), w) - x).abs() < 1e-3, "{x}");
        }
        assert_eq!(e.bytes_at(-5.0, w), e.lo);
        assert_eq!(e.bytes_at(5000.0, w), e.hi);
        assert_eq!(e.delta_bytes(10.0, w), 10 * M);
        assert_eq!(e.delta_bytes(-10.0, w), -10 * M);

        // min = used: neither edge can be dragged into the used data
        assert_eq!(e.drag(Handle::End, 100 * MIB, 500 * MIB, -450 * M), (100 * MIB, 200 * MIB));
        assert_eq!(e.drag(Handle::Start, 100 * MIB, 500 * MIB, 450 * M), (400 * MIB, 200 * MIB));

        // clamping to the neighbours
        assert_eq!(e.drag(Handle::Start, 100 * MIB, 500 * MIB, -900 * M), (50 * MIB, 550 * MIB));
        assert_eq!(e.drag(Handle::End, 100 * MIB, 500 * MIB, 900 * M), (100 * MIB, 900 * MIB));
        assert_eq!(e.drag(Handle::Body, 100 * MIB, 500 * MIB, 900 * M), (500 * MIB, 500 * MIB));
        assert_eq!(e.drag(Handle::Body, 100 * MIB, 500 * MIB, -900 * M), (50 * MIB, 500 * MIB));

        // snapping to 1 MiB
        assert_eq!(e.drag(Handle::Start, 100 * MIB, 500 * MIB, -(10 * M + 300_000)), (90 * MIB, 510 * MIB));
        assert_eq!(e.drag(Handle::End, 100 * MIB, 500 * MIB, 3 * M + 700_000), (100 * MIB, 504 * MIB));
        assert_eq!(e.drag(Handle::Body, 100 * MIB, 500 * MIB, 7 * M + 100), (107 * MIB, 500 * MIB));

        // the fields stay in sync
        assert_eq!(e.fields(100 * MIB, 500 * MIB), (50 * MIB, 500 * MIB, 400 * MIB));
        assert_eq!(e.set_before(100 * MIB, 500 * MIB, 0), (50 * MIB, 500 * MIB));
        assert_eq!(e.set_size(100 * MIB, 500 * MIB, 10 * MIB), (100 * MIB, 200 * MIB), "min = used");
        assert_eq!(e.set_after(100 * MIB, 500 * MIB, 0), (100 * MIB, 900 * MIB));
        assert_eq!(e.set_after(100 * MIB, 500 * MIB, 5000 * MIB), (100 * MIB, 200 * MIB));
    }

    #[test]
    fn legacy_unaligned_partition_keeps_or_aligns_its_edges() {
        // starts at sector 63, odd length; free space up to an unaligned usable end
        let start = 63 * 512;
        let len = 300 * MIB + 4096;
        let e = Extent {
            lo: start,
            hi: 1000 * MIB + 512,
            min_len: 100 * MIB,
            grid: MIB,
            keep_start: Some(start),
            keep_end: Some(start + len),
            end_at_hi: true,
        };
        // growing in place keeps the legacy start; the end snaps to the grid or to `hi`
        assert_eq!(e.set_end(start, len, start + len + 5 * MIB), (start, 305 * MIB - start));
        assert_eq!(e.set_end(start, len, e.hi), (start, e.hi - start));
        // nudging the end back to where it was restores the exact original end
        assert_eq!(e.set_end(start, len, start + len + 10), (start, len));
        // moving lands both edges on the grid
        let (s, l) = e.move_to(start, len, 10 * MIB + 7);
        assert_eq!((s % MIB, l % MIB), (0, 0));
        assert_eq!((s, l), (10 * MIB, 300 * MIB));
        // moving back to the original start is an in-place resize again
        assert_eq!(e.move_to(10 * MIB, 300 * MIB, start), (start, 300 * MIB));
        assert_eq!(e.move_to(start, len, start + 100), (start, len));
    }

    #[test]
    fn create_extent_never_ends_off_grid() {
        // a GPT free tail: aligned start, unaligned usable end
        let e = Extent {
            lo: 801 * MIB,
            hi: 1024 * MIB - 33 * 512,
            min_len: MIB,
            grid: MIB,
            keep_start: None,
            keep_end: None,
            end_at_hi: false,
        };
        assert_eq!(e.set_size(801 * MIB, MIB, 5000 * MIB), (801 * MIB, 222 * MIB));
        assert_eq!(e.set_size(801 * MIB, 10 * MIB, 1000), (801 * MIB, MIB), "min 1 MiB");
        assert_eq!(e.move_to(801 * MIB, 222 * MIB, 900 * MIB), (801 * MIB, 222 * MIB), "no room to move");
        assert_eq!(e.set_start(801 * MIB, 222 * MIB, 900 * MIB + 300_000), (900 * MIB, 123 * MIB));
        // a range narrower than the grid: the block stays inside the extent
        let tiny = Extent { lo: 10, hi: 20, min_len: 1, grid: MIB, keep_start: None, keep_end: None, end_at_hi: false };
        let (s, l) = tiny.set_end(10, 5, 15);
        assert!(s >= 10 && s + l <= 20);
    }
}
