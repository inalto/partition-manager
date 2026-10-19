//! Long-running work (enumeration, Apply, scans) on a worker thread.
//!
//! The UI starts a job with [`spawn_job`] and polls the returned [`JobHandle`] once per frame:
//! [`JobHandle::progress`] for the latest [`ProgressReport`], [`JobHandle::poll`] for the
//! result. The UI thread never blocks on a job; cancellation is cooperative through the
//! job's [`CancelToken`].

use pm_core::{CancelToken, Error, ProgressReport, ProgressSink, Result};
use pm_ops::{ExecReport, Model};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What a finished job hands back to the UI.
#[derive(Debug)]
pub enum JobOutput {
    /// A fresh enumeration of the disks.
    Model(Model),
    /// Apply finished (possibly with a failed operation, see `report.failed`). `refreshed` is
    /// the re-enumeration done afterwards: `None` for a dry run (nothing changed), `Some(Err)`
    /// when enumeration failed.
    Applied {
        report: ExecReport,
        refreshed: Option<std::result::Result<Model, String>>,
    },
    /// Free-form result text (tool windows).
    Text(String),
}

/// [`ProgressSink`] that stores the latest report for the UI to read.
#[derive(Clone, Default)]
pub struct SharedProgress(pub Arc<Mutex<ProgressReport>>);

impl ProgressSink for SharedProgress {
    fn report(&self, p: &ProgressReport) {
        // a poisoned lock only means a reader panicked; the report itself is still valid
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        *g = p.clone();
    }
}

/// A running (or finished) job.
pub struct JobHandle {
    pub name: String,
    pub progress: Arc<Mutex<ProgressReport>>,
    pub cancel: CancelToken,
    pub result: Receiver<Result<JobOutput>>,
    pub started: Instant,
    finished: bool,
}

impl JobHandle {
    /// Snapshot of the latest progress report.
    pub fn snapshot(&self) -> ProgressReport {
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Ask the job to stop at its next cancellation check.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// The job's result once it has finished, without blocking. A job that panicked yields
    /// an error. Returns `None` while the job is still running (and after the result was
    /// taken).
    pub fn poll(&mut self) -> Option<Result<JobOutput>> {
        if self.finished {
            return None;
        }
        let r = match self.result.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(Error::Invalid(format!(
                "job \"{}\" stopped unexpectedly (panic)",
                self.name
            ))),
        };
        self.finished = true;
        Some(r)
    }
}

/// Run `f` on a new worker thread named after `name`.
pub fn spawn_job<F>(name: &str, f: F) -> JobHandle
where
    F: FnOnce(&dyn ProgressSink, &CancelToken) -> Result<JobOutput> + Send + 'static,
{
    let progress = Arc::new(Mutex::new(ProgressReport {
        phase: name.to_string(),
        ..Default::default()
    }));
    let cancel = CancelToken::new();
    let (tx, rx) = mpsc::channel();
    let sink = SharedProgress(progress.clone());
    let token = cancel.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("job: {name}"))
        .spawn(move || {
            let r = f(&sink, &token);
            // the UI may have gone away; nothing to do then
            let _ = tx.send(r);
        });
    if let Err(e) = spawned {
        // the closure, and with it the sender, is dropped: poll() reports the failure
        log::error!("cannot start job {name}: {e}");
    }
    JobHandle {
        name: name.to_string(),
        progress,
        cancel,
        result: rx,
        started: Instant::now(),
        finished: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Poll `h` until it yields a result (the test-side equivalent of the UI's frame loop).
    fn wait(h: &mut JobHandle) -> Result<JobOutput> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(r) = h.poll() {
                return r;
            }
            assert!(Instant::now() < deadline, "job did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn fake_job_reports_progress_and_completes() {
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let mut h = spawn_job("fake", move |p, _c| {
            p.report(&ProgressReport { phase: "copy".into(), done: 50, total: 100, ..Default::default() });
            go_rx.recv().unwrap(); // hold until the test has looked at the progress
            p.report(&ProgressReport { phase: "copy".into(), done: 100, total: 100, ..Default::default() });
            Ok(JobOutput::Text("done".into()))
        });
        assert_eq!(h.name, "fake");
        // the first report becomes visible while the job is still running
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.snapshot().done != 50 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(h.poll().is_none(), "still running");
        assert_eq!(h.snapshot().phase, "copy");
        go_tx.send(()).unwrap();
        match wait(&mut h) {
            Ok(JobOutput::Text(t)) => assert_eq!(t, "done"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(h.snapshot().done, 100);
        assert!(h.poll().is_none(), "result is delivered once");
    }

    #[test]
    fn fake_job_cancels() {
        let mut h = spawn_job("cancellable", |p, c| {
            let mut done = 0;
            loop {
                c.check()?;
                done += 1;
                p.report(&ProgressReport { done, total: u64::MAX, ..Default::default() });
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        assert!(!h.is_cancelled());
        h.cancel();
        assert!(h.is_cancelled());
        assert!(matches!(wait(&mut h), Err(Error::Cancelled)));
    }

    #[test]
    fn panicking_job_reports_an_error() {
        let mut h = spawn_job("boom", |_p, _c| panic!("worker exploded"));
        match wait(&mut h) {
            Err(e) => assert!(e.to_string().contains("boom"), "{e}"),
            Ok(o) => panic!("unexpected {o:?}"),
        }
    }
}
