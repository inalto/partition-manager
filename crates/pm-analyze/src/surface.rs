//! Surface test: read every block of a device and time it.

use pm_core::{BlockDevice, CancelToken, Error, ProgressSink, ProgressTracker, Result};
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Default surface-test block size (1 MiB).
pub const DEFAULT_BLOCK: u64 = 1 << 20;
/// A block is only reported slow when it also took at least this long, so that nanosecond
/// jitter on very fast devices (RAM, cache hits) is not flagged.
const SLOW_FLOOR_MICROS: u64 = 1000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SurfaceReport {
    /// Block size in bytes (the last block may be shorter).
    pub block: u64,
    pub blocks: u32,
    /// Indexes of blocks whose read failed.
    pub bad: Vec<u32>,
    /// Indexes of blocks that took more than 10x the median read time (and at least 1 ms).
    pub slow: Vec<u32>,
    /// Read time per block in milliseconds, saturating at `u16::MAX`.
    pub ms_per_block: Vec<u16>,
    pub elapsed_secs: f64,
}

/// Read the whole device in `block`-byte steps (the last one clipped to the device end).
///
/// A read that fails with `Error::Io` marks the block bad and the test continues; any other
/// error aborts. Slow blocks are those taking more than 10x the median time of the good blocks.
/// Progress reports "Testing surface" in bytes; cancellation is checked before every block.
pub fn surface_test(
    dev: &mut dyn BlockDevice,
    block: u64,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<SurfaceReport> {
    if block == 0 {
        return Err(Error::Invalid("surface test block size must be non-zero".into()));
    }
    let len = dev.len();
    let blocks = u32::try_from(len.div_ceil(block))
        .map_err(|_| Error::Invalid(format!("block size {block} is too small for a device of {len} bytes")))?;
    let mut buf = vec![0u8; block.min(len.max(1)) as usize];
    let mut tracker = ProgressTracker::new(progress, "Testing surface", len);
    let started = Instant::now();
    let mut bad = Vec::new();
    let mut micros = Vec::with_capacity(blocks as usize);
    for i in 0..blocks {
        cancel.check()?;
        let offset = i as u64 * block;
        let n = block.min(len - offset) as usize;
        let t = Instant::now();
        match dev.read_at(offset, &mut buf[..n]) {
            Ok(()) => {}
            Err(Error::Io { .. }) => bad.push(i),
            Err(e) => return Err(e),
        }
        micros.push(t.elapsed().as_micros().min(u64::MAX as u128) as u64);
        tracker.add(n as u64);
    }
    tracker.finish();

    let mut good: Vec<u64> = micros
        .iter()
        .enumerate()
        .filter(|(i, _)| bad.binary_search(&(*i as u32)).is_err())
        .map(|(_, &m)| m)
        .collect();
    good.sort_unstable();
    let median = good.get(good.len() / 2).copied().unwrap_or(0);
    let slow = micros
        .iter()
        .enumerate()
        .filter(|(i, &m)| {
            bad.binary_search(&(*i as u32)).is_err()
                && m > median.saturating_mul(10)
                && m >= SLOW_FLOOR_MICROS
        })
        .map(|(i, _)| i as u32)
        .collect();
    Ok(SurfaceReport {
        block,
        blocks,
        bad,
        slow,
        ms_per_block: micros.iter().map(|&m| (m / 1000).min(u16::MAX as u64) as u16).collect(),
        elapsed_secs: started.elapsed().as_secs_f64(),
    })
}
