//! Disk benchmark: write then read passes per transfer size, sequential or random.

use pm_core::{align_up, format_size, BlockDevice, CancelToken, Error, ProgressSink, ProgressTracker, Result};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Transfer sizes are rounded up to this (the physical sector size of unbuffered file I/O).
const IO_ALIGN: u64 = 4096;
/// Size of the writes that pre-fill a scratch device before the timed passes.
const WARMUP_CHUNK: u64 = 1 << 20;
/// Sanity bound for one request.
const MAX_TRANSFER: u64 = 256 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BenchMode {
    Sequential,
    Random,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchOpts {
    /// Bytes per request; each is rounded up to a multiple of 4096.
    pub transfer_sizes: Vec<u32>,
    /// Bytes transferred by every write pass and by every read pass (and the size of the test area).
    pub total_len: u64,
    /// Requests a worker issues back to back before it checks for cancellation and reports progress.
    pub queue_depth: u32,
    /// Worker threads, each on its own device from [`BenchTarget::open_for_bench`].
    pub threads: u32,
    pub mode: BenchMode,
    /// Pause between transfer sizes.
    pub cooldown_secs: u32,
}

impl Default for BenchOpts {
    fn default() -> Self {
        BenchOpts {
            transfer_sizes: vec![4096, 8192, 16384, 32768],
            total_len: 200 << 20,
            queue_depth: 8,
            threads: 4,
            mode: BenchMode::Sequential,
            cooldown_secs: 5,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchPoint {
    /// Transfer size in bytes (after rounding).
    pub size: u32,
    pub read_mbps: f64,
    pub write_mbps: f64,
    pub read_iops: f64,
    pub write_iops: f64,
}

/// Something that can provide scratch devices to benchmark (an unbuffered temp file on the
/// platform; a `MemDevice` in tests). Every returned device is independent and exactly (at least)
/// `len` bytes long; offsets and lengths of requests are multiples of 4096.
pub trait BenchTarget: Send {
    fn open_for_bench(&self, len: u64) -> Result<Box<dyn BlockDevice>>;
}

impl BenchTarget for pm_platform::FileBenchTarget {
    fn open_for_bench(&self, len: u64) -> Result<Box<dyn BlockDevice>> {
        pm_platform::FileBenchTarget::open_for_bench(self, len)
    }
}

/// xorshift64 (Marsaglia); never yields 0 for a non-zero seed.
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        XorShift64(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What every worker of one pass shares.
struct Pass<'a> {
    progress: Mutex<ProgressTracker<'a>>,
    cancel: &'a CancelToken,
    abort: AtomicBool,
    write: bool,
    mode: BenchMode,
    size: u64,
    queue_depth: u64,
}

/// One worker: `ops` requests inside its own `slots`-slot region of `dev`.
fn run_worker(pass: &Pass<'_>, dev: &mut dyn BlockDevice, index: u64, slots: u64, ops: u64) -> Result<()> {
    let size = pass.size as usize;
    let mut rng = XorShift64::new(0x9E37_79B9_7F4A_7C15u64.wrapping_mul(index + 1));
    // incompressible payload so devices that compress do not flatter the write numbers
    let mut buf = vec![0u8; size];
    for chunk in buf.chunks_mut(8) {
        let r = rng.next().to_le_bytes();
        chunk.copy_from_slice(&r[..chunk.len()]);
    }
    let mut slot = 0u64;
    let mut done = 0u64;
    while done < ops {
        if pass.abort.load(Ordering::SeqCst) {
            return Ok(());
        }
        pass.cancel.check()?;
        let batch = pass.queue_depth.min(ops - done);
        for _ in 0..batch {
            let s = match pass.mode {
                BenchMode::Sequential => {
                    let s = slot;
                    slot = (slot + 1) % slots;
                    s
                }
                BenchMode::Random => rng.next() % slots,
            };
            let offset = s * pass.size;
            if pass.write {
                dev.write_at(offset, &buf)?;
            } else {
                dev.read_at(offset, &mut buf)?;
            }
        }
        done += batch;
        lock(&pass.progress).add(batch * pass.size);
    }
    if pass.write {
        dev.flush()?;
    }
    Ok(())
}

/// Run one timed pass over `devs`; returns the wall-clock seconds.
fn run_pass(
    devs: &mut [Box<dyn BlockDevice>],
    ops_per_worker: &[u64],
    slots: u64,
    pass: &Pass<'_>,
) -> Result<f64> {
    let started = Instant::now();
    let results: Vec<Result<()>> = std::thread::scope(|s| {
        let handles: Vec<_> = devs
            .iter_mut()
            .zip(ops_per_worker)
            .enumerate()
            .map(|(i, (dev, &ops))| {
                s.spawn(move || {
                    let r = run_worker(pass, dev.as_mut(), i as u64, slots, ops);
                    if r.is_err() {
                        pass.abort.store(true, Ordering::SeqCst);
                    }
                    r
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    let secs = started.elapsed().as_secs_f64();
    // prefer a real error over the Cancelled that a sibling may report
    let mut first_err = None;
    for r in results {
        match r {
            Err(Error::Cancelled) => first_err = first_err.or(Some(Error::Cancelled)),
            Err(e) => return Err(e),
            Ok(()) => {}
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(secs),
    }
}

/// Untimed sequential pre-fill of every device in `WARMUP_CHUNK` writes. A freshly sized scratch
/// file has no valid data yet, and the file system would otherwise zero-fill it inside the timed
/// write pass (e.g. up to the first random offset), biasing write throughput downwards. It also
/// gives the read pass real data. The warm-up is not part of any reported figure.
fn warm_up(devs: &mut [Box<dyn BlockDevice>], progress: &dyn ProgressSink, size: u64, cancel: &CancelToken) -> Result<()> {
    let total: u64 = devs.iter().map(|d| d.len()).sum();
    let phase = format!("Benchmark {} warm-up", format_size(size));
    let tracker = Mutex::new(ProgressTracker::new(progress, &phase, total));
    let abort = AtomicBool::new(false);
    let results: Vec<Result<()>> = std::thread::scope(|s| {
        let handles: Vec<_> = devs
            .iter_mut()
            .enumerate()
            .map(|(i, dev)| {
                let (tracker, abort) = (&tracker, &abort);
                s.spawn(move || {
                    let r = (|| {
                        let len = dev.len();
                        let mut rng = XorShift64::new(0xD1B5_4A32_D192_ED03u64.wrapping_mul(i as u64 + 1));
                        let mut buf = vec![0u8; WARMUP_CHUNK.min(len) as usize];
                        for chunk in buf.chunks_mut(8) {
                            let r = rng.next().to_le_bytes();
                            chunk.copy_from_slice(&r[..chunk.len()]);
                        }
                        let mut pos = 0u64;
                        while pos < len {
                            if abort.load(Ordering::SeqCst) {
                                return Ok(());
                            }
                            cancel.check()?;
                            let n = (buf.len() as u64).min(len - pos) as usize;
                            dev.write_at(pos, &buf[..n])?;
                            pos += n as u64;
                            lock(tracker).add(n as u64);
                        }
                        dev.flush()
                    })();
                    if r.is_err() {
                        abort.store(true, Ordering::SeqCst);
                    }
                    r
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    lock(&tracker).finish();
    let mut cancelled = false;
    for r in results {
        match r {
            Err(Error::Cancelled) => cancelled = true,
            Err(e) => return Err(e),
            Ok(()) => {}
        }
    }
    if cancelled {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn cooldown(secs: u32, cancel: &CancelToken) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(secs as u64);
    loop {
        cancel.check()?;
        let now = Instant::now();
        if now >= end {
            return Ok(());
        }
        std::thread::sleep((end - now).min(Duration::from_millis(100)));
    }
}

/// Benchmark `target`: for every transfer size a write pass, then a read pass, each moving
/// `total_len` bytes split over `threads` workers (each with its own device).
///
/// The `BlockDevice` API is synchronous, so the effective concurrency is `threads`;
/// `queue_depth` is the number of requests a worker issues back to back between cancellation
/// and progress checks. Sequential mode walks consecutive offsets (wrapping inside the worker's
/// region); random mode picks size-aligned offsets from a xorshift64 generator. Before the timed
/// passes each device gets an untimed sequential warm-up write of its whole region (so that
/// lazily allocated or zero-filled files do not distort the write numbers); warm-up I/O is not
/// counted in the results. Write timing includes the final flush. MB/s is 1e6 bytes per second.
pub fn benchmark(
    target: &dyn BenchTarget,
    opts: &BenchOpts,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Vec<BenchPoint>> {
    if opts.transfer_sizes.is_empty() {
        return Err(Error::Invalid("benchmark needs at least one transfer size".into()));
    }
    if opts.total_len == 0 || opts.queue_depth == 0 || opts.threads == 0 {
        return Err(Error::Invalid("benchmark total length, queue depth and threads must be non-zero".into()));
    }
    let mut points = Vec::with_capacity(opts.transfer_sizes.len());
    for (n, &requested) in opts.transfer_sizes.iter().enumerate() {
        if n > 0 {
            cooldown(opts.cooldown_secs, cancel)?;
        }
        cancel.check()?;
        if requested == 0 {
            return Err(Error::Invalid("transfer size must be non-zero".into()));
        }
        let size = align_up(requested as u64, IO_ALIGN);
        if size > MAX_TRANSFER {
            return Err(Error::Invalid(format!("transfer size {requested} is too large")));
        }
        // work split: `workers` devices, each covering `slots` request-sized slots
        let total_ops = opts.total_len.div_ceil(size).max(1);
        let workers = (opts.threads as u64).min(total_ops);
        let slots = total_ops.div_ceil(workers);
        let ops_per_worker: Vec<u64> = (0..workers)
            .map(|i| total_ops / workers + u64::from(i < total_ops % workers))
            .collect();
        let mut devs = (0..workers)
            .map(|_| target.open_for_bench(slots * size))
            .collect::<Result<Vec<_>>>()?;

        warm_up(&mut devs, progress, size, cancel)?;

        let mut secs = [0.0f64; 2];
        for (pass_no, write) in [true, false].into_iter().enumerate() {
            cancel.check()?;
            let phase = format!("Benchmark {} {}", format_size(size), if write { "write" } else { "read" });
            let pass = Pass {
                progress: Mutex::new(ProgressTracker::new(progress, &phase, total_ops * size)),
                cancel,
                abort: AtomicBool::new(false),
                write,
                mode: opts.mode,
                size,
                queue_depth: opts.queue_depth as u64,
            };
            secs[pass_no] = run_pass(&mut devs, &ops_per_worker, slots, &pass)?.max(1e-9);
            lock(&pass.progress).finish();
        }
        drop(devs);
        let bytes = (total_ops * size) as f64;
        points.push(BenchPoint {
            size: size as u32,
            write_mbps: bytes / secs[0] / 1e6,
            read_mbps: bytes / secs[1] / 1e6,
            write_iops: total_ops as f64 / secs[0],
            read_iops: total_ops as f64 / secs[1],
        });
    }
    Ok(points)
}
