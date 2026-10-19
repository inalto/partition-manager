//! The per-operation steps run by [`crate::execute`], plus the machinery they share: the
//! write-session guard, the step progress adapter, volume lookup and in-place data moves.

pub(crate) mod clone;
pub(crate) mod format;
pub(crate) mod image;
pub(crate) mod migrate;
pub(crate) mod resize;
pub(crate) mod table_ops;
pub(crate) mod wipe;

use crate::executor::StepEvent;
use crate::model::Model;
use crate::operation::Operation;
use crate::planner::err_text;
use pm_core::{
    BlockDevice, CancelToken, DiskInfo, Error, PartitionInfo, ProgressReport, ProgressSink,
    ProgressTracker, Result, SliceDevice, TableKind, VolumeId,
};
use pm_fs::UsedMap;
use pm_image::RescueStats;
use pm_parttable::{Entry, Table};
use pm_platform::{OpenMode, Platform, WriteSession};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Chunk size of wipes and in-place moves.
pub(crate) const CHUNK: u64 = 4 << 20;
/// How long to wait for the OS to expose the volume of a new or changed partition.
const VOLUME_WAIT: Duration = Duration::from_secs(5);
const VOLUME_POLL: Duration = Duration::from_millis(250);

/// Everything a step needs while one operation runs.
pub(crate) struct Ctx<'a> {
    pub platform: &'a dyn Platform,
    progress: &'a dyn ProgressSink,
    pub cancel: &'a CancelToken,
    index: usize,
    count: usize,
    summary: String,
    pub log: &'a mut Vec<String>,
    pub rescue_stats: &'a mut Vec<RescueStats>,
    /// The live model, re-read just before this operation.
    pub live: &'a Model,
    /// `live` with this operation simulated (the planner's numbers).
    pub after: &'a Model,
}

impl<'a> Ctx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        platform: &'a dyn Platform,
        progress: &'a dyn ProgressSink,
        cancel: &'a CancelToken,
        (index, count): (usize, usize),
        op: &Operation,
        log: &'a mut Vec<String>,
        rescue_stats: &'a mut Vec<RescueStats>,
        (live, after): (&'a Model, &'a Model),
    ) -> Self {
        Ctx {
            platform,
            progress,
            cancel,
            index,
            count,
            summary: op.summary(),
            log,
            rescue_stats,
            live,
            after,
        }
    }

    /// `[i/n] <summary>: <what>`.
    pub(crate) fn prefix(&self, what: &str) -> String {
        format!("[{}/{}] {}: {}", self.index + 1, self.count, self.summary, what)
    }

    /// Start step `name`: log one line and return the progress sink for the step (which
    /// immediately receives a "started" report).
    pub(crate) fn step(&mut self, name: &str) -> StepSink<'a> {
        let line = self.prefix(name);
        self.log.push(line.clone());
        let sink = StepSink {
            inner: self.progress,
            op_index: self.index,
            step: name.to_string(),
            phase: line,
        };
        sink.report(&ProgressReport::default());
        sink
    }

    /// A free-form log line (not a step).
    pub(crate) fn note(&mut self, text: &str) {
        let line = self.prefix(text);
        self.log.push(line);
    }

    /// Ask the OS to re-read the layout of `disk`.
    pub(crate) fn rescan(&mut self, disk: u32) -> Result<()> {
        self.step(&format!("re-read the layout of Disk {disk}"));
        self.platform.rescan(disk)
    }

    pub(crate) fn live_disk(&self, d: u32) -> Result<&'a DiskInfo> {
        self.live
            .disk(d)
            .ok_or_else(|| Error::Invalid(format!("Disk {d} does not exist")))
    }

    pub(crate) fn live_partition(&self, d: u32, n: u32) -> Result<&'a PartitionInfo> {
        self.live
            .partition(d, n)
            .ok_or_else(|| Error::Invalid(format!("Disk {d} has no partition {n}")))
    }

    /// The partition the simulated operation adds to disk `d` (the planner's number,
    /// aligned start/length, type and GUID).
    pub(crate) fn new_partition(&self, d: u32) -> Result<&'a PartitionInfo> {
        let old = self.live_disk(d)?;
        let new = self
            .after
            .disk(d)
            .ok_or_else(|| Error::Invalid(format!("Disk {d} does not exist")))?;
        added_partition(old, new)
            .ok_or_else(|| Error::Invalid(format!("the planner added no partition to Disk {d}")))
    }
}

/// The partition of `after` that `before` does not have (same number and start).
pub(crate) fn added_partition<'m>(before: &DiskInfo, after: &'m DiskInfo) -> Option<&'m PartitionInfo> {
    after
        .partitions
        .iter()
        .find(|p| !before.partitions.iter().any(|o| o.number == p.number && o.start == p.start))
}

/// Progress adapter for one step: rewrites every report's phase to
/// `[i/n] <operation summary>: <step>` and moves the engine's own phase into `detail`.
pub(crate) struct StepSink<'a> {
    inner: &'a dyn ProgressSink,
    op_index: usize,
    step: String,
    phase: String,
}

impl StepSink<'_> {
    fn event(&self, p: &ProgressReport) -> StepEvent {
        StepEvent { op_index: self.op_index, step: self.step.clone(), progress: p.clone() }
    }
}

impl ProgressSink for StepSink<'_> {
    fn report(&self, p: &ProgressReport) {
        let ev = self.event(p);
        let detail = match (ev.progress.phase.is_empty(), ev.progress.detail.is_empty()) {
            (true, _) => ev.progress.detail.clone(),
            (false, true) => ev.progress.phase.clone(),
            (false, false) => format!("{}: {}", ev.progress.phase, ev.progress.detail),
        };
        self.inner.report(&ProgressReport {
            phase: self.phase.clone(),
            detail,
            ..ev.progress
        });
    }
}

/// A [`WriteSession`] that is always finished: explicitly through [`Session::finish`] (whose
/// error is reported) or, on an early return or a panic, when dropped (error ignored).
pub(crate) struct Session {
    inner: Option<Box<dyn WriteSession>>,
}

impl Session {
    fn begin(p: &dyn Platform, disk: u32) -> Result<Session> {
        Ok(Session { inner: Some(p.begin_write(disk)?) })
    }

    fn live(&mut self) -> &mut Box<dyn WriteSession> {
        self.inner.as_mut().expect("session is open until finish")
    }

    pub(crate) fn device(&mut self) -> &mut dyn BlockDevice {
        self.live().device()
    }

    pub(crate) fn commit_table(&mut self, t: &Table) -> Result<()> {
        self.live().commit_table(t)
    }

    /// The partition table currently on the disk.
    pub(crate) fn read_table(&mut self) -> Result<Table> {
        pm_parttable::read_table(self.device())
    }

    fn finish(mut self) -> Result<()> {
        match self.inner.take() {
            Some(s) => s.finish(),
            None => Ok(()),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(s) = self.inner.take() {
            let _ = s.finish();
        }
    }
}

/// Run `f` inside a write session on `disk`. The session is finished whatever `f` returns;
/// the error of `f` wins over an error of `finish`.
pub(crate) fn with_session<T>(
    p: &dyn Platform,
    disk: u32,
    f: impl FnOnce(&mut Session) -> Result<T>,
) -> Result<T> {
    let mut s = Session::begin(p, disk)?;
    let r = f(&mut s);
    let fin = s.finish();
    let v = r?;
    fin?;
    Ok(v)
}

/// A table entry for partition `p` of the model (attributes cleared).
pub(crate) fn entry_of(kind: TableKind, p: &PartitionInfo) -> Entry {
    let gpt = kind == TableKind::Gpt;
    Entry {
        number: p.number,
        start: p.start,
        len: p.len,
        ptype: p.ptype.unwrap_or_else(|| crate::model::default_ptype(kind)),
        guid: if gpt { Some(p.guid.unwrap_or_else(Uuid::new_v4)) } else { None },
        name: if gpt { p.name.clone() } else { String::new() },
        attrs: 0,
        logical: p.logical,
    }
}

/// The entry numbered `n` of `t`.
pub(crate) fn entry_mut(t: &mut Table, disk: u32, n: u32) -> Result<&mut Entry> {
    t.entries
        .iter_mut()
        .find(|e| e.number == n)
        .ok_or_else(|| Error::Invalid(format!("Disk {disk} has no partition {n} on the device")))
}

/// Wait (up to 5 s) for the OS to expose a volume at byte `start` of `disk`.
pub(crate) fn locate_volume(
    p: &dyn Platform,
    disk: u32,
    start: u64,
    cancel: &CancelToken,
) -> Result<VolumeId> {
    let deadline = Instant::now() + VOLUME_WAIT;
    loop {
        if let Some(v) = p
            .enumerate_volumes()?
            .into_iter()
            .find(|v| v.disk == Some(disk) && v.start == start)
        {
            return Ok(v.id);
        }
        cancel.check()?;
        if Instant::now() >= deadline {
            return Err(Error::NotFound(format!(
                "no volume appeared for the partition at byte {start} of Disk {disk}"
            )));
        }
        std::thread::sleep(VOLUME_POLL);
    }
}

/// The volume of live partition `part` (as enumerated, or looked up by position).
pub(crate) fn volume_of(ctx: &Ctx, disk: u32, part: &PartitionInfo) -> Result<VolumeId> {
    match &part.volume {
        Some(v) => Ok(v.clone()),
        None => locate_volume(ctx.platform, disk, part.start, ctx.cancel),
    }
}

/// Filesystem facts used to copy only what matters.
pub(crate) struct FsFacts {
    pub used: UsedMap,
    /// Filesystem size in bytes.
    pub total: u64,
}

/// Used map and size of the filesystem at `[start, start + len)` of `disk`; `None` when it
/// cannot be read (unknown filesystem, damaged metadata, ...).
pub(crate) fn fs_facts(p: &dyn Platform, disk: u32, start: u64, len: u64) -> Option<FsFacts> {
    let dev = p.open_disk(disk, OpenMode::ReadOnly).ok()?;
    let slice = SliceDevice::new(dev, start, len).ok()?;
    let mut fs = pm_fs::open(Box::new(slice)).ok()?;
    let used = fs.used_map().ok()?;
    Some(FsFacts { used, total: fs.total() })
}

/// Copy `len` bytes from `from` to `to` on the same device in 4 MiB chunks, overlap-safe
/// (backwards when moving towards the end, forwards otherwise). Not cancellable once started:
/// a half-moved partition would be lost, so `cancel` is only checked before the first chunk.
pub(crate) fn move_data(
    dev: &mut dyn BlockDevice,
    from: u64,
    to: u64,
    len: u64,
    sink: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<()> {
    cancel.check()?;
    if from == to || len == 0 {
        return Ok(());
    }
    let mut tracker = ProgressTracker::new(sink, "Moving", len);
    let mut buf = vec![0u8; CHUNK.min(len) as usize];
    let mut chunk = |off: u64, n: usize| -> Result<()> {
        dev.read_at(from + off, &mut buf[..n])?;
        dev.write_at(to + off, &buf[..n])?;
        tracker.add(n as u64);
        Ok(())
    };
    if to > from {
        let mut off = len;
        while off > 0 {
            let n = off.min(CHUNK);
            off -= n;
            chunk(off, n as usize)?;
        }
    } else {
        let mut off = 0;
        while off < len {
            let n = (len - off).min(CHUNK);
            chunk(off, n as usize)?;
            off += n;
        }
    }
    dev.flush()?;
    tracker.finish();
    Ok(())
}

/// The message reported for a failed operation (without the `invalid:` prefix).
pub(crate) fn message(e: Error) -> String {
    err_text(e)
}
