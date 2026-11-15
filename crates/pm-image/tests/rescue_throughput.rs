//! Engine throughput of `rescue` against a fragmented used map, with null devices so only
//! the engine's own overhead is measured.
//!
//! Ignored by default (it runs for about a minute); run it optimised:
//!
//! ```text
//! cargo test --release -p pm-image --test rescue_throughput -- --ignored --nocapture
//! ```
//!
//! `PM_BENCH_SECS` sets the measuring window per map (default 10), `PM_BENCH_RUNS` the
//! comma-separated numbers of used runs (default `1000,50000,500000,3000000`).

use pm_core::{BlockDevice, CancelToken, Error, NullProgress, Result};
use pm_fs::UsedMap;
use pm_image::rescue::{rescue, RescueOpts};
use std::time::{Duration, Instant};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const SRC_LEN: u64 = 1863 * GIB;
const UNIT: u32 = 64 << 10;

/// Reads as zeros without touching the buffer and discards writes. As a source it times
/// the copy from its first read and cancels the run once `window` has passed.
struct NullDevice {
    len: u64,
    window: Duration,
    cancel: CancelToken,
    first_read: Option<Instant>,
    last_read: Option<Instant>,
    bytes_read: u64,
}

impl NullDevice {
    fn new(len: u64, window: Duration, cancel: CancelToken) -> Self {
        NullDevice { len, window, cancel, first_read: None, last_read: None, bytes_read: 0 }
    }
}

impl BlockDevice for NullDevice {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, _offset: u64, buf: &mut [u8]) -> Result<()> {
        let now = Instant::now();
        let first = *self.first_read.get_or_insert(now);
        if now.duration_since(first) >= self.window {
            self.cancel.cancel();
        }
        self.last_read = Some(now);
        self.bytes_read += buf.len() as u64;
        Ok(())
    }
    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> Result<()> {
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

/// `runs` evenly spread used runs covering 60% of `SRC_LEN`.
fn fragmented(runs: u64) -> UsedMap {
    let units = SRC_LEN / UNIT as u64;
    let run_units = (units * 6 / 10 / runs).max(1);
    let mut used = UsedMap::new(SRC_LEN, UNIT);
    for i in 0..runs {
        // u128: i * units overflows u64 only for absurd inputs, but stay exact
        let start = (i as u128 * units as u128 / runs as u128) as u64;
        used.set(start * UNIT as u64, run_units * UNIT as u64);
    }
    used
}

/// MiB/s of the copy phase over the measuring window.
fn throughput(runs: u64, window: Duration) -> f64 {
    let cancel = CancelToken::new();
    let mut src = NullDevice::new(SRC_LEN, window, cancel.clone());
    let mut dst = NullDevice::new(SRC_LEN, window, CancelToken::new());
    let opts = RescueOpts {
        block_size: 4 << 20,
        skip_zero_writes: true,
        abort_on_error: true,
        used_map: Some(fragmented(runs)),
        ..RescueOpts::default()
    };
    match rescue(&mut src, &mut dst, 0, &opts, &NullProgress, &cancel) {
        Ok(_) | Err(Error::Cancelled) => {}
        Err(e) => panic!("rescue failed: {e}"),
    }
    let secs = match (src.first_read, src.last_read) {
        (Some(a), Some(b)) => b.duration_since(a).as_secs_f64(),
        _ => panic!("nothing was read"),
    };
    assert!(secs > 0.0, "copy phase too short to time");
    src.bytes_read as f64 / MIB as f64 / secs
}

#[test]
#[ignore = "benchmark: about a minute; run with --release -- --ignored --nocapture"]
fn rescue_throughput_survives_fragmented_used_map() {
    let window = Duration::from_secs(
        std::env::var("PM_BENCH_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(10),
    );
    let runs: Vec<u64> = match std::env::var("PM_BENCH_RUNS") {
        Ok(s) => s.split(',').map(|t| t.trim().parse().expect("PM_BENCH_RUNS")).collect(),
        Err(_) => vec![1_000, 50_000, 500_000, 3_000_000],
    };
    let mut results = Vec::new();
    for &n in &runs {
        let rate = throughput(n, window);
        println!("used runs {n:>9}: {rate:>8.0} MiB/s");
        results.push(rate);
    }
    let (first, last) = (results[0], *results.last().unwrap());
    assert!(
        last * 5.0 >= first,
        "throughput collapses with fragmentation: {first:.0} MiB/s at {} runs, {last:.0} MiB/s at {} runs",
        runs[0],
        runs[runs.len() - 1]
    );
}
