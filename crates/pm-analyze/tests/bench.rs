use pm_analyze::{benchmark, BenchMode, BenchOpts, BenchTarget};
use pm_core::{BlockDevice, CancelToken, Error, FnProgress, MemDevice, NullProgress, ProgressReport, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Bench target on `MemDevice`s that counts the I/O performed.
struct MemTarget {
    ops: Arc<AtomicU64>,
    warm: Arc<AtomicU64>,
    opened: Arc<AtomicU64>,
    slow_bulk_write: bool,
}

/// Counts requests of at most 8 KiB (the benchmark's transfers); larger writes are the untimed
/// warm-up and are counted separately, as are the bytes they carry.
struct Counting {
    inner: MemDevice,
    ops: Arc<AtomicU64>,
    warm: Arc<AtomicU64>,
    slow_bulk_write: bool,
}

impl Counting {
    fn note(&self, len: usize) {
        if len <= 8192 {
            self.ops.fetch_add(1, Ordering::SeqCst);
        } else {
            self.warm.fetch_add(len as u64, Ordering::SeqCst);
        }
    }
}

impl BlockDevice for Counting {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn sector_size(&self) -> u32 {
        4096
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        assert_eq!(offset % 4096, 0);
        assert_eq!(buf.len() % 4096, 0);
        self.note(buf.len());
        self.inner.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        assert_eq!(offset % 4096, 0);
        assert_eq!(buf.len() % 4096, 0);
        self.note(buf.len());
        if self.slow_bulk_write && buf.len() > 8192 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        self.inner.write_at(offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

impl BenchTarget for MemTarget {
    fn open_for_bench(&self, len: u64) -> Result<Box<dyn BlockDevice>> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Counting {
            inner: MemDevice::new(len),
            ops: self.ops.clone(),
            warm: self.warm.clone(),
            slow_bulk_write: self.slow_bulk_write,
        }))
    }
}

fn target() -> MemTarget {
    MemTarget {
        ops: Arc::new(AtomicU64::new(0)),
        warm: Arc::new(AtomicU64::new(0)),
        opened: Arc::new(AtomicU64::new(0)),
        slow_bulk_write: false,
    }
}

fn opts(mode: BenchMode) -> BenchOpts {
    BenchOpts {
        transfer_sizes: vec![4096, 8192],
        total_len: 256 * 1024,
        queue_depth: 4,
        threads: 2,
        mode,
        cooldown_secs: 0,
    }
}

#[test]
fn benchmark_runs_on_mem_device_returns_points() {
    for mode in [BenchMode::Sequential, BenchMode::Random] {
        let t = target();
        let pts = benchmark(&t, &opts(mode), &NullProgress, &CancelToken::new()).unwrap();
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].size, 4096);
        assert_eq!(pts[1].size, 8192);
        for p in &pts {
            assert!(p.read_mbps > 0.0 && p.write_mbps > 0.0, "{p:?}");
            assert!(p.read_iops > 0.0 && p.write_iops > 0.0, "{p:?}");
            assert!(p.read_mbps.is_finite() && p.write_iops.is_finite());
        }
        // each size: write + read, each transferring total_len bytes
        let ops = t.ops.load(Ordering::SeqCst);
        assert_eq!(ops, 2 * (256 * 1024 / 4096) + 2 * (256 * 1024 / 8192), "{mode:?}");
        // one device per worker per size
        assert_eq!(t.opened.load(Ordering::SeqCst), 2 * 2);
    }
}

#[test]
fn warm_up_writes_each_device_once_and_is_not_timed_or_counted() {
    let mut t = target();
    t.slow_bulk_write = true; // every warm-up write takes 100 ms
    let mut o = opts(BenchMode::Random);
    o.transfer_sizes = vec![4096];
    let pts = benchmark(&t, &o, &NullProgress, &CancelToken::new()).unwrap();
    // the whole of both devices (256 KiB) was pre-filled with large writes ...
    assert_eq!(t.warm.load(Ordering::SeqCst), 256 * 1024);
    // ... which are not among the counted transfers (64 writes + 64 reads) ...
    assert_eq!(t.ops.load(Ordering::SeqCst), 128);
    // ... nor in the timing: 64 ops would take >= 0.1 s (<= 640 IOPS) if the 100 ms warm-up were timed
    assert!(pts[0].write_iops > 5_000.0, "warm-up leaked into timing: {:?}", pts[0]);
}

#[test]
fn benchmark_rounds_sizes_up_to_4k_and_reports_phases() {
    let t = target();
    let mut o = opts(BenchMode::Sequential);
    o.transfer_sizes = vec![512];
    let reports = Mutex::new(Vec::<ProgressReport>::new());
    let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
    let pts = benchmark(&t, &o, &sink, &CancelToken::new()).unwrap();
    assert_eq!(pts[0].size, 4096);
    let r = reports.lock().unwrap();
    let phases: Vec<&str> = r.iter().map(|p| p.phase.as_str()).collect();
    assert!(phases.iter().any(|p| p.starts_with("Benchmark ") && p.ends_with(" write")), "{phases:?}");
    assert!(phases.iter().any(|p| p.starts_with("Benchmark ") && p.ends_with(" read")), "{phases:?}");
    assert_eq!(r.last().unwrap().done, r.last().unwrap().total);
}

#[test]
fn benchmark_validates_and_cancels() {
    let t = target();
    let mut bad = opts(BenchMode::Sequential);
    bad.transfer_sizes.clear();
    assert!(matches!(benchmark(&t, &bad, &NullProgress, &CancelToken::new()), Err(Error::Invalid(_))));
    let mut bad = opts(BenchMode::Sequential);
    bad.total_len = 0;
    assert!(matches!(benchmark(&t, &bad, &NullProgress, &CancelToken::new()), Err(Error::Invalid(_))));
    let c = CancelToken::new();
    c.cancel();
    assert!(matches!(benchmark(&t, &opts(BenchMode::Random), &NullProgress, &c), Err(Error::Cancelled)));
}

#[test]
fn bench_defaults_match_brief() {
    let d = BenchOpts::default();
    assert_eq!(d.total_len, 200 << 20);
    assert_eq!((d.queue_depth, d.threads, d.cooldown_secs), (8, 4, 5));
    assert_eq!(d.transfer_sizes.first(), Some(&4096));
    assert_eq!(d.transfer_sizes.last(), Some(&(32 * 1024)));
}

#[cfg(windows)]
#[test]
fn benchmark_runs_on_unbuffered_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let t = pm_platform::FileBenchTarget::new(dir.path().to_path_buf());
    let mut o = opts(BenchMode::Random);
    o.transfer_sizes = vec![4096];
    o.total_len = 512 * 1024;
    let pts = benchmark(&t, &o, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(pts.len(), 1);
    assert!(pts[0].write_mbps > 0.0 && pts[0].read_mbps > 0.0);
    // temp files are deleted when the devices drop
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
