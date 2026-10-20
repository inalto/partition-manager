//! Rescue engine: ddrescue-style phases against an error-injecting source.

use pm_core::{BlockDevice, CancelToken, Error, FnProgress, MemDevice, NullProgress, ProgressReport};
use pm_fs::UsedMap;
use pm_image::rescue::{copy_simple, rescue, BlockStatus, FaultyDevice, MapFile, RescueOpts};
use std::collections::HashMap;
use std::sync::Mutex;

const MIB: u64 = 1 << 20;
const LEN: u64 = 4 * MIB;
const BAD: u64 = MIB + 512;

fn pattern() -> Vec<u8> {
    (0..LEN as usize).map(|i| (i / 7) as u8 ^ (i as u8)).collect()
}

fn faulty(bad: &[u64], transient: &[(u64, u32)]) -> FaultyDevice {
    FaultyDevice::new(
        MemDevice::from_vec(pattern()),
        bad.to_vec(),
        transient.iter().copied().collect::<HashMap<_, _>>(),
    )
}

/// Blocks are non-empty, contiguous, cover [0, len) and are coalesced.
fn assert_map_invariant(m: &MapFile, len: u64) {
    let mut pos = 0;
    for (i, &(p, s, st)) in m.blocks.iter().enumerate() {
        assert_eq!(p, pos, "not contiguous: {:?}", m.blocks);
        assert!(s > 0);
        if i > 0 {
            assert_ne!(m.blocks[i - 1].2, st, "not coalesced: {:?}", m.blocks);
        }
        pos += s;
    }
    assert_eq!(pos, len);
}

/// dst equals src everywhere except the sectors listed (which must be zero in dst).
fn assert_copied_except(dst: &[u8], holes: &[(u64, u64)]) {
    let src = pattern();
    let mut pos = 0usize;
    for &(h, l) in holes {
        assert!(dst[pos..h as usize] == src[pos..h as usize], "mismatch before hole {h:#x}");
        assert!(dst[h as usize..(h + l) as usize].iter().all(|&b| b == 0), "hole {h:#x} not zero");
        pos = (h + l) as usize;
    }
    assert!(dst[pos..] == src[pos..], "mismatch after last hole");
}

#[test]
fn rescue_clean_source_copies_all() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("clean.map");
    let mut src = faulty(&[], &[]);
    let mut dst = MemDevice::new(LEN + 4096);
    let reports = Mutex::new(Vec::<ProgressReport>::new());
    let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
    let opts = RescueOpts { mapfile: Some(mf.clone()), ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 4096, &opts, &sink, &CancelToken::new()).unwrap();
    assert_eq!(st.rescued, LEN);
    assert_eq!(st.bad_bytes, 0);
    assert_eq!(st.bad_areas, 0);
    assert_eq!(st.read_errors, 0);
    assert_eq!(st.passes, 0);
    assert!(dst.as_slice()[..4096].iter().all(|&b| b == 0));
    assert!(dst.as_slice()[4096..] == pattern()[..]);
    assert_eq!(src.write_count(), 0, "source must never be written");
    // 128 KiB reads, each once
    assert_eq!(src.read_count(), LEN / (128 << 10));
    let m = MapFile::load(&mf).unwrap();
    assert_eq!(m.blocks, vec![(0, LEN, BlockStatus::Finished)]);
    assert_eq!(m.current_status, '+');
    let r = reports.lock().unwrap();
    let last = r.last().unwrap();
    assert_eq!(last.phase, "Copying");
    assert_eq!((last.done, last.total), (LEN, LEN));
}

#[test]
fn rescue_bad_sector_isolated_to_one_sector() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("bad.map");
    let mut src = faulty(&[BAD], &[]);
    let mut dst = MemDevice::new(LEN);
    let opts = RescueOpts { mapfile: Some(mf.clone()), ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_copied_except(dst.as_slice(), &[(BAD, 512)]);
    assert_eq!(st.bad_bytes, 512);
    assert_eq!(st.bad_areas, 1);
    assert_eq!(st.rescued, LEN - 512);
    assert!(st.read_errors >= 2, "copy + trim errors, got {}", st.read_errors);
    assert_eq!(src.write_count(), 0);
    let m = MapFile::load(&mf).unwrap();
    assert_map_invariant(&m, LEN);
    assert_eq!(m.ranges(BlockStatus::Bad), vec![(BAD, 0x200)]);
    assert_eq!(
        m.blocks,
        vec![
            (0, BAD, BlockStatus::Finished),
            (BAD, 512, BlockStatus::Bad),
            (BAD + 512, LEN - BAD - 512, BlockStatus::Finished)
        ]
    );
    // only the failing 128 KiB block is trimmed sector by sector: no read touches the bad
    // sector more than twice (one copy read, one trim read)
    let touching = src
        .read_log()
        .iter()
        .filter(|&&(o, l)| o <= BAD && BAD < o + l as u64)
        .count();
    assert_eq!(touching, 2);
}

#[test]
fn rescue_scrape_marks_middle_sectors() {
    // three bad sectors in one block with good sectors between them: trim stops at the
    // outer two, scrape recovers the good middle sectors and isolates the inner bad one
    let bads = [MIB + 1024, MIB + 4096, MIB + 8192];
    let mut src = faulty(&bads, &[]);
    let mut dst = MemDevice::new(LEN);
    let st = rescue(&mut src, &mut dst, 0, &RescueOpts::default(), &NullProgress, &CancelToken::new())
        .unwrap();
    assert_eq!(st.bad_bytes, 3 * 512);
    assert_eq!(st.bad_areas, 3);
    assert_copied_except(dst.as_slice(), &[(bads[0], 512), (bads[1], 512), (bads[2], 512)]);
    // no_scrape leaves the middle non-scraped (and not copied)
    let mut src = faulty(&bads, &[]);
    let mut dst = MemDevice::new(LEN);
    let opts = RescueOpts { no_scrape: true, ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(st.bad_bytes, bads[2] + 512 - bads[0]);
    assert_eq!(st.bad_areas, 1);
}

#[test]
fn rescue_no_trim_leaves_non_trimmed() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("nt.map");
    let mut src = faulty(&[BAD], &[]);
    let mut dst = MemDevice::new(LEN);
    let opts = RescueOpts { no_trim: true, mapfile: Some(mf.clone()), ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(st.bad_bytes, 128 << 10);
    let m = MapFile::load(&mf).unwrap();
    assert_eq!(m.ranges(BlockStatus::NonTrimmed), vec![(MIB, 128 << 10)]);
}

#[test]
fn rescue_permanent_bad_sector_terminates() {
    let mut src = faulty(&[BAD, 3 * MIB], &[]);
    let mut dst = MemDevice::new(LEN);
    let opts = RescueOpts { max_retries: 3, ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(st.passes, 3);
    assert_eq!(st.bad_bytes, 1024);
    assert_eq!(st.bad_areas, 2);
    assert_copied_except(dst.as_slice(), &[(BAD, 512), (3 * MIB, 512)]);
    // each retry pass reads each bad sector exactly once, alternating direction
    let log = src.read_log();
    let retry: Vec<u64> = log[log.len() - 6..].iter().map(|&(o, _)| o).collect();
    assert_eq!(retry, vec![3 * MIB, BAD, BAD, 3 * MIB, 3 * MIB, BAD]);
}

#[test]
fn rescue_transient_error_recovered_on_retry() {
    let mut src = faulty(&[], &[(BAD, 2)]);
    let mut dst = MemDevice::new(LEN);
    let opts = RescueOpts { max_retries: 2, ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(st.bad_bytes, 0);
    assert_eq!(st.bad_areas, 0);
    assert_eq!(st.rescued, LEN);
    assert!(st.passes >= 1);
    assert!(dst.as_slice() == &pattern()[..]);
}

#[test]
fn rescue_resume_from_mapfile_skips_finished() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("resume.map");
    let opts = RescueOpts { mapfile: Some(mf.clone()), ..RescueOpts::default() };
    let mut src = faulty(&[BAD], &[]);
    let mut dst = MemDevice::new(LEN);
    let first = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert!(src.read_count() > 0);
    let before = dst.as_slice().to_vec();

    let mut src2 = faulty(&[BAD], &[]);
    let second = rescue(&mut src2, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(src2.read_count(), 0, "nothing left to read");
    assert!(dst.as_slice() == &before[..]);
    assert_eq!(second.bad_bytes, first.bad_bytes);
    assert_eq!(second.rescued, first.rescued);

    // a partially rescued map: only the non-finished part is read
    let mut m = MapFile::new(LEN);
    m.mark(0, 2 * MIB, BlockStatus::Finished);
    m.save(&mf).unwrap();
    let mut src3 = faulty(&[], &[]);
    let mut dst3 = MemDevice::new(LEN);
    rescue(&mut src3, &mut dst3, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert!(src3.read_log().iter().all(|&(o, _)| o >= 2 * MIB));
    assert!(dst3.as_slice()[..(2 * MIB) as usize].iter().all(|&b| b == 0));
    assert!(dst3.as_slice()[(2 * MIB) as usize..] == pattern()[(2 * MIB) as usize..]);

    // a mapfile for a device of another size is rejected
    MapFile::new(LEN / 2).save(&mf).unwrap();
    let r = rescue(&mut faulty(&[], &[]), &mut dst3, 0, &opts, &NullProgress, &CancelToken::new());
    assert!(matches!(r, Err(Error::Invalid(_))));
}

#[test]
fn rescue_reverse_direction() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("rev.map");
    let mut src = faulty(&[BAD], &[]);
    let mut dst = MemDevice::new(LEN);
    let opts = RescueOpts { reverse: true, mapfile: Some(mf.clone()), ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    let log = src.read_log();
    assert_eq!(log[0], (LEN - (128 << 10), 128 << 10), "first read is the last block");
    assert_eq!(log[1], (LEN - (256 << 10), 128 << 10));
    assert_eq!(st.bad_bytes, 512);
    assert_eq!(st.bad_areas, 1);
    assert_copied_except(dst.as_slice(), &[(BAD, 512)]);
    let m = MapFile::load(&mf).unwrap();
    assert_map_invariant(&m, LEN);
    assert_eq!(m.ranges(BlockStatus::Bad), vec![(BAD, 512)]);
}

#[test]
fn rescue_used_map_skips_unused_and_zero_fills() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("used.map");
    let mut used = UsedMap::new(LEN, 4096);
    used.set(0, MIB);
    used.set(2 * MIB, 64 << 10);
    let used_bytes = MIB + (64 << 10);
    // a bad sector in an unused area is never touched
    let mut src = faulty(&[3 * MIB], &[]);
    let mut dst = MemDevice::from_vec(vec![0xEE; LEN as usize]);
    let opts = RescueOpts {
        used_map: Some(used.clone()),
        zero_unused: true,
        mapfile: Some(mf.clone()),
        ..RescueOpts::default()
    };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(st.rescued, used_bytes);
    assert_eq!(st.bad_bytes, 0);
    assert_eq!(st.read_errors, 0);
    for &(o, l) in &src.read_log() {
        let e = o + l as u64;
        assert!(e <= MIB || (o >= 2 * MIB && e <= 2 * MIB + (64 << 10)), "read unused {o:#x}+{l}");
    }
    let s = pattern();
    let d = dst.as_slice();
    assert!(d[..MIB as usize] == s[..MIB as usize]);
    assert!(d[MIB as usize..2 * MIB as usize].iter().all(|&b| b == 0));
    assert!(d[2 * MIB as usize..(2 * MIB + (64 << 10)) as usize] == s[2 * MIB as usize..(2 * MIB + (64 << 10)) as usize]);
    assert!(d[(2 * MIB + (64 << 10)) as usize..].iter().all(|&b| b == 0));
    let m = MapFile::load(&mf).unwrap();
    assert_eq!(m.blocks, vec![(0, LEN, BlockStatus::Finished)]);
    let text = std::fs::read_to_string(&mf).unwrap();
    assert!(text.lines().any(|l| l.starts_with('#') && l.contains("used-map")), "{text}");

    // without zero_unused the stale destination data in unused areas is left alone
    let mut src = faulty(&[], &[]);
    let mut dst = MemDevice::from_vec(vec![0xEE; LEN as usize]);
    let opts = RescueOpts { used_map: Some(used), ..RescueOpts::default() };
    rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert!(dst.as_slice()[MIB as usize..2 * MIB as usize].iter().all(|&b| b == 0xEE));
    assert!(dst.as_slice()[..MIB as usize] == s[..MIB as usize]);
}

#[test]
fn rescue_abort_on_error_saves_mapfile() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("abort.map");
    let mut src = faulty(&[BAD], &[]);
    let mut dst = MemDevice::new(LEN);
    let opts = RescueOpts { abort_on_error: true, mapfile: Some(mf.clone()), ..RescueOpts::default() };
    let r = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new());
    assert!(matches!(r, Err(Error::Io { offset, .. }) if offset == MIB), "{r:?}");
    let m = MapFile::load(&mf).unwrap();
    assert_map_invariant(&m, LEN);
    assert_eq!(m.ranges(BlockStatus::Finished), vec![(0, MIB)]);
    assert_eq!(m.ranges(BlockStatus::NonTrimmed), vec![(MIB, 128 << 10)]);
}

/// Source wrapper that cancels a token on its `n`-th read (the read itself succeeds).
struct CancelAfter {
    inner: FaultyDevice,
    n: u64,
    token: CancelToken,
}

impl BlockDevice for CancelAfter {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> pm_core::Result<()> {
        if self.inner.read_count() + 1 == self.n {
            self.token.cancel();
        }
        self.inner.read_at(offset, buf)
    }
    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> pm_core::Result<()> {
        panic!("source written");
    }
    fn flush(&mut self) -> pm_core::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        true
    }
}

#[test]
fn rescue_cancel_saves_consistent_mapfile() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("cancel.map");
    let opts = RescueOpts { mapfile: Some(mf.clone()), ..RescueOpts::default() };
    let mut dst = MemDevice::new(LEN);

    // cancelled before the first read
    let pre = CancelToken::new();
    pre.cancel();
    let r = rescue(&mut faulty(&[], &[]), &mut dst, 0, &opts, &NullProgress, &pre);
    assert!(matches!(r, Err(Error::Cancelled)));
    let m = MapFile::load(&mf).unwrap();
    assert_map_invariant(&m, LEN);
    assert_eq!(m.stats().rescued, 0);

    // cancelled mid-copy: the 5th read completes and is recorded, then the engine stops
    let token = CancelToken::new();
    let mut src = CancelAfter { inner: faulty(&[BAD], &[]), n: 5, token: token.clone() };
    let r = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &token);
    assert!(matches!(r, Err(Error::Cancelled)));
    assert_eq!(src.inner.read_count(), 5);
    let m = MapFile::load(&mf).unwrap();
    assert_map_invariant(&m, LEN);
    assert_eq!(m.ranges(BlockStatus::Finished), vec![(0, 5 * (128 << 10))]);

    // resuming without cancel completes and reads only what is left
    let mut src = faulty(&[BAD], &[]);
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert!(src.read_log().iter().all(|&(o, _)| o >= 5 * (128 << 10)));
    assert_eq!(st.rescued, LEN - 512);
    assert_copied_except(dst.as_slice(), &[(BAD, 512)]);
}

#[test]
fn rescue_rejects_small_destination() {
    let mut src = faulty(&[], &[]);
    let mut dst = MemDevice::new(LEN);
    let r = rescue(&mut src, &mut dst, 512, &RescueOpts::default(), &NullProgress, &CancelToken::new());
    assert!(matches!(r, Err(Error::Invalid(_))));
    assert_eq!(src.read_count(), 0);
}

#[test]
fn copy_simple_aborts_on_error() {
    let mut src = faulty(&[3 * MIB], &[]);
    let mut dst = MemDevice::new(LEN);
    // [0, 2 MiB) is clean: copied fully
    let n = copy_simple(&mut src, &mut dst, 0, 0, 2 * MIB, None, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(n, 2 * MIB);
    assert!(dst.as_slice()[..(2 * MIB) as usize] == pattern()[..(2 * MIB) as usize]);
    // the whole device hits the bad sector: Io error
    let r = copy_simple(&mut src, &mut dst, 0, 0, LEN, None, &NullProgress, &CancelToken::new());
    assert!(matches!(r, Err(Error::Io { .. })), "{r:?}");
    assert_eq!(src.write_count(), 0);
    // with a used map that excludes the bad sector it succeeds; offsets are relative to src_off
    let mut used = UsedMap::new(2 * MIB, 4096);
    used.set(0, 512 << 10);
    let mut dst = MemDevice::new(LEN);
    let n = copy_simple(&mut src, &mut dst, 2 * MIB, 4096, 2 * MIB, Some(&used), &NullProgress, &CancelToken::new())
        .unwrap();
    assert_eq!(n, 512 << 10);
    let s = pattern();
    assert!(dst.as_slice()[4096..4096 + (512 << 10)] == s[(2 * MIB) as usize..(2 * MIB) as usize + (512 << 10)]);
    assert!(dst.as_slice()[4096 + (512 << 10)..].iter().all(|&b| b == 0));
    // cancelled before starting
    let c = CancelToken::new();
    c.cancel();
    let r = copy_simple(&mut src, &mut dst, 0, 0, MIB, None, &NullProgress, &c);
    assert!(matches!(r, Err(Error::Cancelled)));
}

#[test]
fn rescue_odd_length_tail_sector() {
    // source length not a multiple of the sector size, bad partial last sector
    let mut data = pattern();
    data.extend((0..100u8).map(|b| b ^ 0x5A));
    let len = data.len() as u64;
    let mut src = FaultyDevice::new(MemDevice::from_vec(data.clone()), vec![LEN], HashMap::new());
    let mut dst = MemDevice::new(len);
    let opts = RescueOpts { max_retries: 1, ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(st.bad_bytes, 100);
    assert_eq!(st.rescued, LEN);
    assert!(dst.as_slice()[..LEN as usize] == data[..LEN as usize]);
    assert!(src.read_log().iter().all(|&(o, l)| o + l as u64 <= len));
}

#[test]
fn rescue_zero_unused_reports_progress() {
    let mut used = UsedMap::new(LEN, 4096);
    used.set(0, MIB);
    let unused = LEN - MIB;
    let reports = Mutex::new(Vec::<ProgressReport>::new());
    let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
    let mut src = faulty(&[], &[]);
    let mut dst = MemDevice::from_vec(vec![0xEE; LEN as usize]);
    let opts = RescueOpts { used_map: Some(used), zero_unused: true, ..RescueOpts::default() };
    rescue(&mut src, &mut dst, 0, &opts, &sink, &CancelToken::new()).unwrap();
    assert!(dst.as_slice()[MIB as usize..].iter().all(|&b| b == 0));
    let r = reports.lock().unwrap();
    let zeroing: Vec<_> = r.iter().filter(|p| p.phase == "Zeroing unused").collect();
    let last = zeroing.last().expect("a Zeroing unused report");
    assert_eq!((last.done, last.total), (unused, unused));
    // zeroing precedes copying, which covers only the used bytes
    let copy = r.iter().rposition(|p| p.phase == "Copying").unwrap();
    assert!(r.iter().position(|p| p.phase == "Zeroing unused").unwrap() < copy);
    assert_eq!((r[copy].done, r[copy].total), (MIB, MIB));
}

#[test]
fn rescue_corrupt_mapfile_is_reported_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("nul.map");
    // what a power loss can leave behind: a file of NULs
    std::fs::write(&mf, vec![0u8; 300]).unwrap();
    let mut src = faulty(&[], &[]);
    let mut dst = MemDevice::new(LEN);
    let opts = RescueOpts { mapfile: Some(mf.clone()), ..RescueOpts::default() };
    let r = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new());
    match r {
        Err(Error::Parse(msg)) => {
            assert!(msg.contains("nul.map"), "{msg}");
            assert!(msg.contains("delete or move the mapfile to start over"), "{msg}");
        }
        other => panic!("expected Parse, got {other:?}"),
    }
    assert_eq!(src.read_count(), 0);
    assert_eq!(std::fs::read(&mf).unwrap(), vec![0u8; 300], "corrupt mapfile left as is");
    // invalid UTF-8 is reported the same way
    std::fs::write(&mf, [0xFFu8, 0xFE, 0x00]).unwrap();
    let r = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new());
    assert!(matches!(r, Err(Error::Parse(ref m)) if m.contains("start over")), "{r:?}");
}

#[test]
fn rescue_uses_device_sector_size_4kn() {
    let mut src = FaultyDevice::new(
        MemDevice::from_vec(pattern()).with_sector_size(4096),
        vec![BAD],
        HashMap::new(),
    );
    assert_eq!(src.sector_size(), 4096);
    let mut dst = MemDevice::new(LEN);
    // opts.sector stays at the 512 default: the device's 4096 wins
    let st = rescue(&mut src, &mut dst, 0, &RescueOpts::default(), &NullProgress, &CancelToken::new())
        .unwrap();
    assert_eq!(st.bad_bytes, 4096);
    assert_eq!(st.bad_areas, 1);
    assert_copied_except(dst.as_slice(), &[(MIB, 4096)]);
    let small: Vec<_> = src.read_log().into_iter().filter(|&(_, l)| l < (64 << 10)).collect();
    assert!(!small.is_empty());
    assert!(small.iter().all(|&(o, l)| l == 4096 && o % 4096 == 0), "{small:?}");
}

#[test]
fn rescue_block_size_clamped() {
    let mut src = faulty(&[], &[]);
    let mut dst = MemDevice::new(LEN);
    let opts = RescueOpts { block_size: usize::MAX, ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(st.rescued, LEN);
    // capped at 64 MiB (and at the device size): one read of the whole 4 MiB source
    assert_eq!(src.read_log(), vec![(0, LEN as usize)]);
    assert!(dst.as_slice() == &pattern()[..]);
}

#[test]
fn rescue_skip_zero_writes_leaves_zero_blocks_unwritten() {
    let dir = tempfile::tempdir().unwrap();
    let mf = dir.path().join("zero.map");
    let len = 1u64 << 20;
    let mut src = MemDevice::new(len);
    // the destination counts its writes
    let mut dst = FaultyDevice::new(MemDevice::new(len), vec![], HashMap::new());
    let opts = RescueOpts { mapfile: Some(mf.clone()), skip_zero_writes: true, ..RescueOpts::default() };
    let st = rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!((st.rescued, st.bad_bytes, st.bad_areas), (len, 0, 0));
    assert_eq!(dst.write_count(), 0, "an all-zero source writes nothing");
    let m = MapFile::load(&mf).unwrap();
    assert_eq!(m.ranges(BlockStatus::Finished), vec![(0, len)], "zero blocks are still rescued");

    // non-zero blocks are written; without the option every block is
    src.write_at(300 << 10, &[7; 4096]).unwrap();
    let mf2 = dir.path().join("some.map");
    let mut dst = FaultyDevice::new(MemDevice::new(len), vec![], HashMap::new());
    let opts = RescueOpts { mapfile: Some(mf2), skip_zero_writes: true, ..RescueOpts::default() };
    rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(dst.write_count(), 1, "only the 128 KiB block holding data");
    assert_eq!(dst.inner().as_slice(), src.as_slice());
    let mut dst = FaultyDevice::new(MemDevice::new(len), vec![], HashMap::new());
    rescue(&mut src, &mut dst, 0, &RescueOpts::default(), &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(dst.write_count(), len / (128 << 10));
}

#[test]
fn rescue_detail_counts_unrescued_areas_and_read_errors_separately() {
    // two bad sectors far apart: 2 unrescued areas, more failed reads than that
    let mut src = faulty(&[0x10000, 0x40000], &[]);
    let mut dst = MemDevice::new(LEN);
    let reports = Mutex::new(Vec::<ProgressReport>::new());
    let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
    let st = rescue(&mut src, &mut dst, 0, &RescueOpts::default(), &sink, &CancelToken::new()).unwrap();
    assert_eq!(st.bad_areas, 2);
    assert!(st.read_errors > 2, "{}", st.read_errors);
    let last = reports.lock().unwrap().last().unwrap().detail.clone();
    assert!(last.contains("errors 2,") && last.contains(&format!("read errors {}", st.read_errors)), "{last}");
}
