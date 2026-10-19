//! Single-line stderr progress bar implementing [`ProgressSink`].

use pm_core::{format_size, ProgressReport, ProgressSink};
use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Minimum time between two redraws (~10 updates per second).
const MIN_INTERVAL: Duration = Duration::from_millis(100);

/// `1:05`, `1:02:03`, or `--` when unknown.
fn format_eta(eta_secs: Option<u64>) -> String {
    match eta_secs {
        None => "--".to_string(),
        Some(s) if s >= 3600 => format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
        Some(s) => format!("{}:{:02}", s / 60, s % 60),
    }
}

/// `"{phase}  {done}/{total}  {rate}/s  ETA {eta}"`.
pub fn format_line(p: &ProgressReport) -> String {
    format!(
        "{}  {}/{}  {}/s  ETA {}",
        p.phase,
        format_size(p.done),
        format_size(p.total),
        format_size(p.rate_bps.max(0.0) as u64),
        format_eta(p.eta_secs)
    )
}

/// Renders reports as one refreshing `\r` line on stderr and ends it with a newline once
/// `done >= total`.
pub struct StderrProgress {
    last: Mutex<Option<Instant>>,
}

impl StderrProgress {
    pub fn new() -> Self {
        StderrProgress { last: Mutex::new(None) }
    }

    /// The text to write for `p` at time `now`, or `None` when throttled.
    fn frame(&self, p: &ProgressReport, now: Instant) -> Option<String> {
        let finished = p.done >= p.total;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if !finished && last.map_or(false, |t| now.duration_since(t) < MIN_INTERVAL) {
            return None;
        }
        *last = Some(now);
        // pad so a shorter line fully overwrites a longer previous one
        let mut s = format!("\r{:<79}", format_line(p));
        if finished {
            s.push('\n');
        }
        Some(s)
    }
}

impl Default for StderrProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressSink for StderrProgress {
    fn report(&self, p: &ProgressReport) {
        if let Some(frame) = self.frame(p, Instant::now()) {
            let mut err = std::io::stderr().lock();
            let _ = err.write_all(frame.as_bytes());
            let _ = err.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::MIB;

    #[test]
    fn line_format() {
        let p = ProgressReport {
            phase: "Copying".into(),
            done: 512 * MIB,
            total: 1024 * MIB,
            rate_bps: (100 * MIB) as f64,
            eta_secs: Some(65),
            detail: String::new(),
        };
        assert_eq!(format_line(&p), "Copying  512.00 MB/1.00 GB  100.00 MB/s  ETA 1:05");
        let q = ProgressReport {
            eta_secs: None,
            ..p.clone()
        };
        assert!(format_line(&q).ends_with("ETA --"));
        assert_eq!(format_eta(Some(3723)), "1:02:03");
        assert_eq!(format_eta(Some(0)), "0:00");
    }

    #[test]
    fn frames_are_throttled_and_end_with_newline() {
        let s = StderrProgress::new();
        let t0 = Instant::now();
        let mut p = ProgressReport {
            phase: "x".into(),
            total: 10,
            ..Default::default()
        };
        let first = s.frame(&p, t0).unwrap();
        assert!(first.starts_with('\r') && !first.ends_with('\n'));
        p.done = 3;
        assert!(s.frame(&p, t0 + Duration::from_millis(50)).is_none());
        assert!(s.frame(&p, t0 + Duration::from_millis(150)).is_some());
        // the final report is never throttled and terminates the line
        p.done = 10;
        let last = s.frame(&p, t0 + Duration::from_millis(160)).unwrap();
        assert!(last.ends_with('\n'));
    }
}
