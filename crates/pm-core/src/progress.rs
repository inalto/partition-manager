//! Progress reporting and cooperative cancellation for long operations.

use crate::error::{Error, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct ProgressReport {
    pub phase: String,
    pub done: u64,
    pub total: u64,
    /// Throughput in bytes per second over the recent moving window.
    pub rate_bps: f64,
    pub eta_secs: Option<u64>,
    pub detail: String,
}

pub trait ProgressSink: Send + Sync {
    fn report(&self, p: &ProgressReport);
}

/// A sink that discards every report.
pub struct NullProgress;

impl ProgressSink for NullProgress {
    fn report(&self, _p: &ProgressReport) {}
}

/// A sink backed by a closure.
pub struct FnProgress<F: Fn(&ProgressReport) + Send + Sync>(pub F);

impl<F: Fn(&ProgressReport) + Send + Sync> ProgressSink for FnProgress<F> {
    fn report(&self, p: &ProgressReport) {
        (self.0)(p)
    }
}

/// Cooperative cancellation flag shared between a worker and its controller.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(Error::Cancelled)` once cancelled; call this between work chunks.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

const REPORT_INTERVAL: Duration = Duration::from_millis(100);
const RATE_WINDOW: Duration = Duration::from_secs(2);

/// Accumulates progress and forwards throttled reports (at most one per 100 ms,
/// plus a final one from [`ProgressTracker::finish`]) to a sink.
pub struct ProgressTracker<'a> {
    sink: &'a dyn ProgressSink,
    phase: String,
    detail: String,
    total: u64,
    done: u64,
    last_report: Instant,
    /// `(time, done)` samples taken at report time, covering roughly the last 2 s.
    window: VecDeque<(Instant, u64)>,
}

impl<'a> ProgressTracker<'a> {
    pub fn new(sink: &'a dyn ProgressSink, phase: &str, total: u64) -> Self {
        let now = Instant::now();
        let mut window = VecDeque::new();
        window.push_back((now, 0));
        ProgressTracker {
            sink,
            phase: phase.to_string(),
            detail: String::new(),
            total,
            done: 0,
            last_report: now,
            window,
        }
    }

    /// Set the free-form detail text included in subsequent reports.
    pub fn set_detail(&mut self, detail: &str) {
        self.detail = detail.to_string();
    }

    pub fn done(&self) -> u64 {
        self.done
    }

    /// Record `n` more units of work; reports if at least 100 ms passed since the last report.
    pub fn add(&mut self, n: u64) {
        self.done = self.done.saturating_add(n);
        let now = Instant::now();
        if now.duration_since(self.last_report) >= REPORT_INTERVAL {
            self.emit(now);
        }
    }

    /// Send a final report unconditionally.
    pub fn finish(&mut self) {
        self.emit(Instant::now());
    }

    fn emit(&mut self, now: Instant) {
        while self.window.len() > 1 && now.duration_since(self.window[0].0) > RATE_WINDOW {
            self.window.pop_front();
        }
        let (t0, d0) = self.window[0];
        let secs = now.duration_since(t0).as_secs_f64();
        let rate_bps = if secs > 0.0 {
            self.done.saturating_sub(d0) as f64 / secs
        } else {
            0.0
        };
        let remaining = self.total.saturating_sub(self.done);
        let eta_secs = if remaining == 0 {
            Some(0)
        } else if rate_bps > 0.0 {
            Some((remaining as f64 / rate_bps).ceil() as u64)
        } else {
            None
        };
        self.window.push_back((now, self.done));
        self.last_report = now;
        self.sink.report(&ProgressReport {
            phase: self.phase.clone(),
            done: self.done,
            total: self.total,
            rate_bps,
            eta_secs,
            detail: self.detail.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn cancel_token_check_errs() {
        let t = CancelToken::default();
        assert!(!t.is_cancelled());
        assert!(t.check().is_ok());
        let t2 = t.clone();
        t2.cancel();
        assert!(t.is_cancelled());
        assert!(matches!(t.check(), Err(crate::Error::Cancelled)));
    }

    #[test]
    fn tracker_reports_throttled_and_finishes() {
        let reports = Mutex::new(Vec::<ProgressReport>::new());
        let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
        let mut t = ProgressTracker::new(&sink, "copy", 1000);
        for _ in 0..10 {
            t.add(100);
        }
        // rapid-fire adds are throttled: far fewer than 10 reports
        let n_before = reports.lock().unwrap().len();
        assert!(n_before <= 2, "got {n_before}");
        t.finish();
        let r = reports.lock().unwrap();
        let last = r.last().unwrap();
        assert_eq!(last.phase, "copy");
        assert_eq!(last.done, 1000);
        assert_eq!(last.total, 1000);
        assert_eq!(last.eta_secs, Some(0));
    }

    #[test]
    fn tracker_computes_rate_and_eta() {
        let reports = Mutex::new(Vec::<ProgressReport>::new());
        let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
        let mut t = ProgressTracker::new(&sink, "x", 1_000_000);
        std::thread::sleep(std::time::Duration::from_millis(150));
        t.add(100_000);
        let r = reports.lock().unwrap();
        let p = r.last().expect("report after >100ms");
        assert_eq!(p.done, 100_000);
        assert!(p.rate_bps > 0.0);
        assert!(p.eta_secs.is_some());
    }

    #[test]
    fn null_progress_is_a_sink() {
        NullProgress.report(&ProgressReport::default());
    }
}
