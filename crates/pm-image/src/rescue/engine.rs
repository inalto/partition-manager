//! The rescue engine: GNU ddrescue's copy / trim / scrape / retry algorithm over
//! [`BlockDevice`]s, driven by a [`MapFile`].
//!
//! The source is only ever read (`read_at`); every byte written to the destination comes
//! from a successful source read, except zero-filled unused areas (`zero_unused`).

use super::mapfile::{BlockStatus, MapFile};
use pm_core::{format_size, BlockDevice, CancelToken, Error, ProgressSink, ProgressTracker, Result};
use pm_fs::UsedMap;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Options for [`rescue`].
#[derive(Clone, Debug)]
pub struct RescueOpts {
    /// Read size of the copy phase (rounded down to a sector multiple). Default 128 KiB.
    pub block_size: usize,
    /// Sector size: the unit of trimming, scraping and retrying. Default 512.
    pub sector: u32,
    /// Initial skip after a read error in the copy phase; doubles after every further
    /// error up to `max(1% of the source, 64 KiB)` and resets after a good read.
    /// Default 64 KiB.
    pub skip_size: u64,
    /// Number of retry passes over bad sectors. Default 0.
    pub max_retries: u32,
    /// Copy from the end of the source towards the start.
    pub reverse: bool,
    pub no_trim: bool,
    pub no_scrape: bool,
    /// Only read the areas this map marks used (offsets relative to the source start;
    /// bytes past `used_map.len` count as used). Unused areas are marked finished.
    pub used_map: Option<UsedMap>,
    /// Write zeros to the destination for unused areas instead of leaving them untouched.
    pub zero_unused: bool,
    /// Plain clone mode: the first read error aborts with that error.
    pub abort_on_error: bool,
    /// Mapfile to resume from (if it exists) and to keep up to date.
    pub mapfile: Option<PathBuf>,
    /// Minimum read rate (bytes/s) over the last 5 s in the copy phase; below it the engine
    /// skips ahead as after a read error (the data just read is kept).
    pub min_read_rate: Option<u64>,
}

impl Default for RescueOpts {
    fn default() -> Self {
        RescueOpts {
            block_size: 128 << 10,
            sector: 512,
            skip_size: 64 << 10,
            max_retries: 0,
            reverse: false,
            no_trim: false,
            no_scrape: false,
            used_map: None,
            zero_unused: false,
            abort_on_error: false,
            mapfile: None,
            min_read_rate: None,
        }
    }
}

/// Outcome of a [`rescue`] run, computed from the final mapfile.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RescueStats {
    /// Bytes rescued (finished), excluding unused areas skipped through the used map;
    /// includes bytes rescued by earlier runs of a resumed mapfile.
    pub rescued: u64,
    /// Bytes not rescued: bad sectors plus anything left non-tried, non-trimmed or
    /// non-scraped (e.g. with `no_trim` / `no_scrape`).
    pub bad_bytes: u64,
    /// Number of maximal unrescued areas.
    pub bad_areas: u64,
    /// Failed source reads during this run.
    pub read_errors: u64,
    pub elapsed_secs: f64,
    /// Retry passes run.
    pub passes: u32,
}

const SAVE_INTERVAL: Duration = Duration::from_secs(30);
const RATE_WINDOW: Duration = Duration::from_secs(5);
const DETAIL_INTERVAL: Duration = Duration::from_millis(250);
const SIMPLE_CHUNK: u64 = 4 << 20;
const ZERO_CHUNK: u64 = 1 << 20;

/// Outcome of one source read.
enum Read {
    Good,
    Bad,
}

/// Ascending, coalesced ranges of `[0, len)` the used map marks used, widened to whole
/// sectors; bytes past `used.len` are used.
fn used_ranges(used: &UsedMap, len: u64, sector: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut push = |s: u64, e: u64| {
        let s = s / sector * sector;
        let e = e.div_ceil(sector).saturating_mul(sector).min(len);
        if s >= e {
            return;
        }
        match out.last_mut() {
            Some(last) if last.0 + last.1 >= s => {
                let end = (last.0 + last.1).max(e);
                last.1 = end - last.0;
            }
            _ => out.push((s, e - s)),
        }
    };
    for (s, l) in used.ranges() {
        if s >= len {
            break;
        }
        push(s, s.saturating_add(l).min(len));
    }
    if used.len < len {
        push(used.len, len);
    }
    out
}

/// Complement of ascending, non-overlapping `ranges` within `[0, len)`.
fn complement(ranges: &[(u64, u64)], len: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut pos = 0;
    for &(s, l) in ranges {
        if s > pos {
            out.push((pos, s - pos));
        }
        pos = pos.max(s + l);
    }
    if pos < len {
        out.push((pos, len - pos));
    }
    out
}

struct Engine<'a> {
    src: &'a mut dyn BlockDevice,
    dst: &'a mut dyn BlockDevice,
    dst_offset: u64,
    opts: &'a RescueOpts,
    progress: &'a dyn ProgressSink,
    cancel: &'a CancelToken,
    map: MapFile,
    comments: Vec<String>,
    len: u64,
    sector: u64,
    block: u64,
    /// Bytes marked finished without reading (unused areas).
    unused: u64,
    tracker: Option<ProgressTracker<'a>>,
    buf: Vec<u8>,
    start: Instant,
    last_save: Instant,
    last_detail: Instant,
    read_errors: u64,
    rescued_this_run: u64,
    passes: u32,
    /// `(time, bytes read)` samples of the copy phase, for `min_read_rate`.
    rate_window: VecDeque<(Instant, u64)>,
    copy_read_bytes: u64,
}

impl<'a> Engine<'a> {
    fn save(&mut self) -> Result<()> {
        self.last_save = Instant::now();
        if let Some(p) = &self.opts.mapfile {
            // data the mapfile calls finished must be durable before the mapfile says so
            self.dst.flush()?;
            self.map.save_with_comments(p, &self.comments)?;
        }
        Ok(())
    }

    fn maybe_save(&mut self) -> Result<()> {
        if self.last_save.elapsed() >= SAVE_INTERVAL {
            self.save()?;
        }
        Ok(())
    }

    fn total(&self) -> u64 {
        self.len - self.unused
    }

    fn done(&self) -> u64 {
        self.map.stats().rescued.saturating_sub(self.unused)
    }

    fn start_phase(&mut self, name: &str, status: char, pass: u32) -> Result<()> {
        if let Some(t) = self.tracker.as_mut() {
            t.finish();
        }
        self.map.current_status = status;
        self.map.pass = pass;
        let mut t = ProgressTracker::new(self.progress, name, self.total());
        t.set_detail(&self.detail());
        t.add(self.done());
        self.tracker = Some(t);
        self.last_detail = Instant::now();
        self.save()
    }

    fn detail(&self) -> String {
        let st = self.map.stats();
        let secs = self.start.elapsed().as_secs_f64();
        let rate = if secs > 0.0 {
            (self.rescued_this_run as f64 / secs) as u64
        } else {
            0
        };
        format!(
            "errsize {}, errors {}, rate {}/s",
            format_size(st.non_trimmed + st.non_scraped + st.bad_bytes),
            self.read_errors,
            format_size(rate)
        )
    }

    fn progressed(&mut self, n: u64, force_detail: bool) {
        let refresh = force_detail || self.last_detail.elapsed() >= DETAIL_INTERVAL;
        let detail = if refresh {
            self.last_detail = Instant::now();
            Some(self.detail())
        } else {
            None
        };
        if let Some(t) = self.tracker.as_mut() {
            if let Some(d) = detail {
                t.set_detail(&d);
            }
            t.add(n);
        }
    }

    /// Read `[pos, pos + n)`; on success write it to the destination and mark it finished,
    /// on a read error mark it `on_err`. Cancellation, destination errors, non-I/O source
    /// errors and (with `abort_on_error`) read errors are returned as `Err`.
    fn try_read(&mut self, pos: u64, n: u64, on_err: BlockStatus) -> Result<Read> {
        self.cancel.check()?;
        self.maybe_save()?;
        self.map.current_pos = pos;
        let n = n as usize;
        match self.src.read_at(pos, &mut self.buf[..n]) {
            Ok(()) => {
                self.dst.write_at(self.dst_offset + pos, &self.buf[..n])?;
                self.map.mark(pos, n as u64, BlockStatus::Finished);
                self.rescued_this_run += n as u64;
                self.progressed(n as u64, false);
                Ok(Read::Good)
            }
            Err(e @ Error::Io { .. }) => {
                self.read_errors += 1;
                self.map.mark(pos, n as u64, on_err);
                self.progressed(0, true);
                if self.opts.abort_on_error {
                    return Err(e);
                }
                Ok(Read::Bad)
            }
            Err(e) => Err(e),
        }
    }

    /// Mark unused areas finished (writing zeros for them first with `zero_unused`, so a
    /// saved mapfile never calls an area finished before it is zeroed).
    fn apply_used_map(&mut self, used: &UsedMap) -> Result<()> {
        let unused = complement(&used_ranges(used, self.len, self.sector), self.len);
        self.unused = unused.iter().map(|r| r.1).sum();
        let mut map = self.map.clone();
        let changed = map.mark_many(&unused, BlockStatus::Finished);
        if self.opts.zero_unused {
            let zeros = vec![0u8; ZERO_CHUNK as usize];
            for (s, l) in changed {
                let mut off = 0;
                while off < l {
                    self.cancel.check()?;
                    let n = (l - off).min(ZERO_CHUNK);
                    self.dst.write_at(self.dst_offset + s + off, &zeros[..n as usize])?;
                    off += n;
                }
            }
        }
        self.map = map;
        Ok(())
    }

    /// Record a good copy-phase read; true if the rate over the last 5 s is below
    /// `min_read_rate`.
    fn too_slow(&mut self, n: u64) -> bool {
        let Some(min) = self.opts.min_read_rate else {
            return false;
        };
        let now = Instant::now();
        self.copy_read_bytes += n;
        self.rate_window.push_back((now, self.copy_read_bytes));
        // keep exactly one sample at least RATE_WINDOW old at the front
        while self.rate_window.len() > 1
            && now.duration_since(self.rate_window[1].0) >= RATE_WINDOW
        {
            self.rate_window.pop_front();
        }
        let (t0, b0) = self.rate_window[0];
        let span = now.duration_since(t0);
        if span < RATE_WINDOW {
            return false;
        }
        let rate = (self.copy_read_bytes - b0) as f64 / span.as_secs_f64();
        if rate < min as f64 {
            // judge the next area on fresh samples
            self.rate_window.clear();
            self.rate_window.push_back((now, self.copy_read_bytes));
            true
        } else {
            false
        }
    }

    /// One sweep over the non-tried areas. With `skipping`, a read error (or a too slow
    /// read) jumps ahead by the skip size, leaving the jumped-over bytes non-tried.
    fn copy_pass(&mut self, backward: bool, skipping: bool) -> Result<()> {
        let init_skip = self.opts.skip_size.div_ceil(self.sector) * self.sector;
        let max_skip = ((self.len / 100).max(64 << 10) / self.sector * self.sector).max(init_skip);
        let bs = self.block;
        let mut ranges = self.map.ranges(BlockStatus::NonTried);
        if backward {
            ranges.reverse();
        }
        let mut skip = init_skip;
        for (start, size) in ranges {
            let end = start + size;
            if !backward {
                let mut pos = start;
                while pos < end {
                    let chunk_end = end.min((pos / bs + 1) * bs);
                    let r = self.try_read(pos, chunk_end - pos, BlockStatus::NonTrimmed)?;
                    let slow = matches!(r, Read::Good) && self.too_slow(chunk_end - pos);
                    pos = chunk_end;
                    if matches!(r, Read::Bad) || slow {
                        if skipping {
                            pos = pos.saturating_add(skip).min(end);
                            skip = (skip * 2).min(max_skip);
                        }
                    } else {
                        skip = init_skip;
                    }
                }
            } else {
                let mut pos = end;
                while pos > start {
                    let chunk_start = start.max((pos - 1) / bs * bs);
                    let r = self.try_read(chunk_start, pos - chunk_start, BlockStatus::NonTrimmed)?;
                    let slow = matches!(r, Read::Good) && self.too_slow(pos - chunk_start);
                    pos = chunk_start;
                    if matches!(r, Read::Bad) || slow {
                        if skipping {
                            pos = pos.saturating_sub(skip).max(start);
                            skip = (skip * 2).min(max_skip);
                        }
                    } else {
                        skip = init_skip;
                    }
                }
            }
        }
        Ok(())
    }

    /// Sector `k` of the area starting at `start` and ending at `end`: `(pos, len)`.
    fn sector_at(&self, start: u64, end: u64, k: u64) -> (u64, u64) {
        let p = start + k * self.sector;
        (p, self.sector.min(end - p))
    }

    fn sectors(&self, start: u64, end: u64) -> u64 {
        (end - start).div_ceil(self.sector)
    }

    /// Trim each non-trimmed area from both edges, sector by sector, up to the first
    /// error at each edge (which becomes bad); the middle becomes non-scraped.
    fn trim(&mut self) -> Result<()> {
        let mut ranges = self.map.ranges(BlockStatus::NonTrimmed);
        if self.opts.reverse {
            ranges.reverse();
        }
        for (start, size) in ranges {
            let end = start + size;
            let count = self.sectors(start, end);
            let mut front_bad = None;
            for k in 0..count {
                let (p, n) = self.sector_at(start, end, k);
                if let Read::Bad = self.try_read(p, n, BlockStatus::Bad)? {
                    front_bad = Some(k);
                    break;
                }
            }
            let Some(fb) = front_bad else { continue };
            let mut back_bad = None;
            for k in (fb + 1..count).rev() {
                let (p, n) = self.sector_at(start, end, k);
                if let Read::Bad = self.try_read(p, n, BlockStatus::Bad)? {
                    back_bad = Some(k);
                    break;
                }
            }
            if let Some(bb) = back_bad {
                if bb > fb + 1 {
                    let mid_start = start + (fb + 1) * self.sector;
                    let mid_end = start + bb * self.sector;
                    self.map.mark(mid_start, mid_end - mid_start, BlockStatus::NonScraped);
                }
            }
        }
        Ok(())
    }

    /// Read every sector of the areas with status `st` in the given direction.
    fn sweep_sectors(&mut self, st: BlockStatus, backward: bool) -> Result<()> {
        let mut ranges = self.map.ranges(st);
        if backward {
            ranges.reverse();
        }
        for (start, size) in ranges {
            let end = start + size;
            let count = self.sectors(start, end);
            let ks: Box<dyn Iterator<Item = u64>> = if backward {
                Box::new((0..count).rev())
            } else {
                Box::new(0..count)
            };
            for k in ks {
                let (p, n) = self.sector_at(start, end, k);
                self.try_read(p, n, BlockStatus::Bad)?;
            }
        }
        Ok(())
    }

    fn has(&self, st: BlockStatus) -> bool {
        self.map.blocks.iter().any(|b| b.2 == st)
    }

    fn run(&mut self) -> Result<()> {
        let opts = self.opts;
        if let Some(used) = &opts.used_map {
            self.apply_used_map(used)?;
        }
        let rev = self.opts.reverse;
        if self.has(BlockStatus::NonTried) {
            self.start_phase("Copying", '?', 1)?;
            self.copy_pass(rev, true)?;
            if self.has(BlockStatus::NonTried) {
                self.map.pass = 2;
                self.copy_pass(!rev, true)?;
            }
            if self.has(BlockStatus::NonTried) {
                self.map.pass = 3;
                self.copy_pass(rev, false)?;
            }
        }
        if !self.opts.no_trim && self.has(BlockStatus::NonTrimmed) {
            self.start_phase("Trimming", '*', 1)?;
            self.trim()?;
        }
        if !self.opts.no_scrape && self.has(BlockStatus::NonScraped) {
            self.start_phase("Scraping", '/', 1)?;
            self.sweep_sectors(BlockStatus::NonScraped, rev)?;
        }
        for pass in 1..=self.opts.max_retries {
            if !self.has(BlockStatus::Bad) {
                break;
            }
            self.start_phase(&format!("Retrying (pass {pass})"), '-', pass)?;
            // the first retry pass runs against the copy direction, then alternate
            let backward = if pass % 2 == 1 { !rev } else { rev };
            self.sweep_sectors(BlockStatus::Bad, backward)?;
            self.passes = pass;
        }
        Ok(())
    }

    fn stats(&self) -> RescueStats {
        let mut bad_bytes = 0;
        let mut bad_areas = 0;
        let mut prev_unrescued = false;
        for &(_, s, st) in &self.map.blocks {
            let unrescued = st != BlockStatus::Finished;
            if unrescued {
                bad_bytes += s;
                if !prev_unrescued {
                    bad_areas += 1;
                }
            }
            prev_unrescued = unrescued;
        }
        RescueStats {
            rescued: self.done(),
            bad_bytes,
            bad_areas,
            read_errors: self.read_errors,
            elapsed_secs: self.start.elapsed().as_secs_f64(),
            passes: self.passes,
        }
    }
}

/// Rescue-copy all of `src` to `dst` at `dst_offset` with GNU ddrescue's algorithm:
///
/// 1. **Copy**: read the non-tried areas in `block_size` chunks (backwards with
///    `reverse`); a failed chunk becomes non-trimmed and the engine skips ahead (pass 1
///    in the copy direction, pass 2 the other way, both skipping; pass 3 without skipping
///    so every byte gets tried).
/// 2. **Trim**: each non-trimmed area is read sector by sector from its front up to the
///    first error, then from its back up to the first error; those sectors become bad
///    and the middle non-scraped.
/// 3. **Scrape**: every sector of every non-scraped area; failures become bad.
/// 4. **Retry**: `max_retries` passes over the bad sectors, alternating direction.
///
/// With `opts.mapfile` the run resumes from an existing mapfile (finished areas are never
/// read again; its size must equal `src.len()`), and the mapfile is saved every 30 s, at
/// every phase change, on error, on cancel and at the end. Cancellation returns
/// `Err(Cancelled)` after saving. The source is only read, never written.
pub fn rescue(
    src: &mut dyn BlockDevice,
    dst: &mut dyn BlockDevice,
    dst_offset: u64,
    opts: &RescueOpts,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<RescueStats> {
    let start = Instant::now();
    let len = src.len();
    if opts.sector == 0 {
        return Err(Error::Invalid("rescue sector size must be non-zero".into()));
    }
    let sector = opts.sector as u64;
    if dst_offset.checked_add(len).map_or(true, |e| e > dst.len()) {
        return Err(Error::Invalid(format!(
            "rescue destination too small: need {len} bytes at offset {dst_offset}, device has {}",
            dst.len()
        )));
    }
    let block = (opts.block_size as u64 / sector * sector).max(sector);
    let map = match &opts.mapfile {
        Some(p) if p.exists() => {
            let text = std::fs::read_to_string(p).map_err(|e| Error::io(0, 0, e))?;
            if text.trim().is_empty() {
                MapFile::new(len)
            } else {
                let m = MapFile::parse(&text)?;
                if m.len() != len {
                    return Err(Error::Invalid(format!(
                        "mapfile {} covers {} bytes but the source has {len}",
                        p.display(),
                        m.len()
                    )));
                }
                m
            }
        }
        _ => MapFile::new(len),
    };
    let mut comments = Vec::new();
    if opts.used_map.is_some() {
        comments.push(format!(
            "Note: used-map applied; unused areas are marked finished without being read ({})",
            if opts.zero_unused {
                "zero-filled in the destination"
            } else {
                "destination left untouched"
            }
        ));
    }
    let now = Instant::now();
    let mut eng = Engine {
        src,
        dst,
        dst_offset,
        opts,
        progress,
        cancel,
        map,
        comments,
        len,
        sector,
        block,
        unused: 0,
        tracker: None,
        buf: vec![0u8; block as usize],
        start,
        last_save: now,
        last_detail: now,
        read_errors: 0,
        rescued_this_run: 0,
        passes: 0,
        rate_window: VecDeque::new(),
        copy_read_bytes: 0,
    };
    if let Err(e) = eng.run() {
        if let Some(t) = eng.tracker.as_mut() {
            t.finish();
        }
        // best effort: the original error matters more than a failure to save
        let _ = eng.save();
        return Err(e);
    }
    eng.map.current_status = '+';
    let d = final_detail(&eng.map, eng.read_errors);
    // nothing left to do (e.g. a finished mapfile): still report the outcome once
    let (total, done) = (eng.total(), eng.done());
    let t = eng.tracker.get_or_insert_with(|| {
        let mut t = ProgressTracker::new(progress, "Copying", total);
        t.add(done);
        t
    });
    t.set_detail(&d);
    t.finish();
    eng.dst.flush()?;
    eng.save()?;
    Ok(eng.stats())
}

fn final_detail(map: &MapFile, errors: u64) -> String {
    let st = map.stats();
    format!(
        "errsize {}, errors {errors}, finished",
        format_size(st.non_trimmed + st.non_scraped + st.bad_bytes)
    )
}

/// Fast plain copy of `len` bytes from `src` at `src_off` to `dst` at `dst_off` in 4 MiB
/// chunks, aborting on the first error (clone mode). With `used`, whose offsets are
/// relative to `src_off`, unused areas are neither read nor written (callers that need
/// them zeroed use [`rescue`] with `zero_unused`). Returns the bytes copied.
#[allow(clippy::too_many_arguments)]
pub fn copy_simple(
    src: &mut dyn BlockDevice,
    dst: &mut dyn BlockDevice,
    src_off: u64,
    dst_off: u64,
    len: u64,
    used: Option<&UsedMap>,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<u64> {
    let fits = |off: u64, dev: u64| off.checked_add(len).is_some_and(|e| e <= dev);
    if !fits(src_off, src.len()) || !fits(dst_off, dst.len()) {
        return Err(Error::Invalid(format!(
            "copy range out of bounds: src {src_off}+{len} (of {}), dst {dst_off}+{len} (of {})",
            src.len(),
            dst.len()
        )));
    }
    let sector = (src.sector_size() as u64).max(1);
    let ranges = match used {
        Some(u) => used_ranges(u, len, sector),
        None if len > 0 => vec![(0, len)],
        None => Vec::new(),
    };
    let total: u64 = ranges.iter().map(|r| r.1).sum();
    let mut tracker = ProgressTracker::new(progress, "Copying", total);
    let mut buf = vec![0u8; SIMPLE_CHUNK.min(total) as usize];
    let mut copied = 0u64;
    cancel.check()?;
    for (s, l) in ranges {
        let mut off = 0;
        while off < l {
            cancel.check()?;
            let n = (l - off).min(SIMPLE_CHUNK) as usize;
            src.read_at(src_off + s + off, &mut buf[..n])?;
            dst.write_at(dst_off + s + off, &buf[..n])?;
            off += n as u64;
            copied += n as u64;
            tracker.add(n as u64);
        }
    }
    dst.flush()?;
    tracker.finish();
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_ranges_widen_to_sectors_and_cover_tail() {
        let mut u = UsedMap::new(10_000, 1000);
        u.set(0, 1000);
        u.set(5000, 1000);
        // units [0,1000) and [5000,6000) -> sectors [0,1024) and [4608,6144); tail past
        // the map's 10_000 bytes is used
        assert_eq!(
            used_ranges(&u, 12_000, 512),
            vec![(0, 1024), (4608, 6144 - 4608), (9728, 12_000 - 9728)]
        );
        assert_eq!(complement(&[(0, 1024), (4608, 1536)], 8192), vec![(1024, 3584), (6144, 2048)]);
        assert_eq!(complement(&[], 10), vec![(0, 10)]);
    }
}
