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
/// How long to wait for the OS to expose the volume of a new or changed partition, and for the
/// volumes of a disk to come back after a write session.
const VOLUME_WAIT: Duration = Duration::from_secs(30);
const VOLUME_POLL: Duration = Duration::from_millis(250);
/// The volume set of a disk counts as settled once it has not changed for this long (after it
/// changed at least once).
const VOLUME_SETTLE: Duration = Duration::from_secs(1);
/// BitLocker's signature at byte 3 of an encrypted volume (no filesystem reader knows it).
const BITLOCKER_SIGNATURE: &[u8] = b"-FVE-FS-";

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

/// Wait (up to 30 s) for the OS to expose a volume at byte `start` of `disk`.
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

/// After a write session on `disk`: wait (up to 30 s, polling every 250 ms, cancellable) until
/// the OS reports at least `expected` volumes on `disk` again — Windows re-arrives them
/// asynchronously after the disk comes back online — or until their set has stopped changing
/// for 1 s after changing at least once. Returns whether the expected count was reached; a
/// timeout is not an error (the next operation re-reads the disks and re-validates).
pub(crate) fn wait_for_volumes(
    p: &dyn Platform,
    disk: u32,
    expected: usize,
    cancel: &CancelToken,
) -> Result<bool> {
    wait_for_volumes_with(p, disk, expected, cancel, VOLUME_WAIT)
}

fn wait_for_volumes_with(
    p: &dyn Platform,
    disk: u32,
    expected: usize,
    cancel: &CancelToken,
    limit: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + limit;
    let mut last: Option<Vec<(String, u64)>> = None;
    let mut changed_at: Option<Instant> = None;
    loop {
        let mut now_set: Vec<(String, u64)> = p
            .enumerate_volumes()?
            .into_iter()
            .filter(|v| v.disk == Some(disk))
            .map(|v| (v.id.0, v.start))
            .collect();
        now_set.sort();
        if now_set.len() >= expected {
            return Ok(true);
        }
        let now = Instant::now();
        match &last {
            Some(prev) if *prev != now_set => changed_at = Some(now),
            _ => {}
        }
        if changed_at.is_some_and(|t| now.duration_since(t) >= VOLUME_SETTLE) {
            return Ok(false);
        }
        last = Some(now_set);
        cancel.check()?;
        if now >= deadline {
            return Ok(false);
        }
        std::thread::sleep(VOLUME_POLL);
    }
}

/// Refuse a table-only shrink of partition `number` (at byte `start` of the session's device)
/// when its head is unreadable or carries a filesystem signature ([`pm_fs::detect`], or
/// BitLocker): the disk list did not know its filesystem, so cutting the table would truncate
/// it. Runs inside the write session, before anything is written.
pub(crate) fn ensure_no_filesystem(
    dev: &mut dyn BlockDevice,
    start: u64,
    disk: u32,
    number: u32,
) -> Result<()> {
    let mut head = vec![0u8; (dev.sector_size() as usize).max(512)];
    dev.read_at(start, &mut head).map_err(|e| {
        Error::Invalid(format!(
            "partition {number} on Disk {disk}: cannot read its start to check for a filesystem \
             ({}); refusing to shrink it",
            err_text(e)
        ))
    })?;
    let found = if head.get(3..3 + BITLOCKER_SIGNATURE.len()) == Some(BITLOCKER_SIGNATURE) {
        Some("BitLocker")
    } else {
        pm_fs::detect(dev, start).map(|p| p.kind.display_name())
    };
    match found {
        Some(name) => Err(Error::Invalid(format!(
            "partition {number} on Disk {disk} holds a {name} filesystem the disk list did not \
             show; refusing to shrink it in the partition table only (refresh or check the volume)"
        ))),
        None => Ok(()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{gpt_types, MemDevice, NullProgress, PartitionType, VolumeInfo, MIB};
    use pm_platform::MockPlatform;

    const DISK: u64 = 48 * MIB;

    fn gpt_disk(parts: &[(u32, u64, u64)]) -> MemDevice {
        let mut dev = MemDevice::new(DISK);
        let mut t = Table::empty(TableKind::Gpt, DISK, 512);
        for &(number, start, len) in parts {
            t.entries.push(Entry {
                number,
                start,
                len,
                ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
                guid: Some(Uuid::new_v4()),
                name: String::new(),
                attrs: 0,
                logical: false,
            });
        }
        pm_parttable::write_table(&mut dev, &t).unwrap();
        dev
    }

    fn boot_sector(dev: &mut MemDevice, at: u64, oem: &[u8]) {
        let mut bs = vec![0u8; 512];
        bs[3..3 + oem.len()].copy_from_slice(oem);
        bs[510] = 0x55;
        bs[511] = 0xAA;
        dev.write_at(at, &bs).unwrap();
    }

    fn entries(p: &MockPlatform) -> Vec<(u32, u64, u64)> {
        let mut dev = MemDevice::from_vec(p.disk_bytes(1));
        let t = pm_parttable::read_table(&mut dev).unwrap();
        t.entries.iter().map(|e| (e.number, e.start, e.len)).collect()
    }

    /// Run one step the way the executor does, but without its precheck (which refuses a
    /// shrink of an unknown filesystem before any step runs).
    fn run_step(p: &MockPlatform, op: &Operation, f: impl FnOnce(&mut Ctx) -> Result<()>) -> Result<()> {
        let live = Model::from_platform(p).unwrap();
        let after = live.clone();
        let (mut log, mut stats) = (Vec::new(), Vec::new());
        let cancel = CancelToken::new();
        let mut ctx =
            Ctx::new(p, &NullProgress, &cancel, (0, 1), op, &mut log, &mut stats, (&live, &after));
        f(&mut ctx)
    }

    #[test]
    fn table_only_shrink_checks_the_partition_head_in_session() {
        let p = MockPlatform::new();
        let mut dev = gpt_disk(&[(1, MIB, 20 * MIB), (2, 30 * MIB, 8 * MIB)]);
        // partition 1: an NTFS boot sector the disk list did not report (no volume)
        boot_sector(&mut dev, MIB, b"NTFS    ");
        // partition 2: raw data without any signature
        dev.write_at(30 * MIB, &[0x5A; 4096]).unwrap();
        p.add_disk(1, dev, Default::default());
        let model = Model::from_platform(&p).unwrap();
        assert_eq!(model.partition(1, 1).unwrap().fs, None);

        let shrink1 = Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 10 * MIB };
        let before = p.disk_bytes(1);
        let err = run_step(&p, &shrink1, |ctx| resize::resize_move(ctx, 1, 1, MIB, 10 * MIB))
            .unwrap_err();
        let msg = message(err);
        assert!(msg.contains("NTFS") && msg.contains("partition 1"), "{msg}");
        assert_eq!(p.disk_bytes(1), before, "nothing written");
        assert_eq!(p.write_sessions_log(), p.finished_sessions_log());

        // a truly raw partition is trimmed in the table
        let shrink2 = Operation::ResizeMove { disk: 1, number: 2, new_start: 30 * MIB, new_len: 4 * MIB };
        run_step(&p, &shrink2, |ctx| resize::resize_move(ctx, 1, 2, 30 * MIB, 4 * MIB)).unwrap();
        assert_eq!(entries(&p), vec![(1, MIB, 20 * MIB), (2, 30 * MIB, 4 * MIB)]);
        assert_eq!(p.write_sessions_log(), p.finished_sessions_log());
    }

    #[test]
    fn table_only_align_trim_checks_every_partition_head_first() {
        let p = MockPlatform::new();
        let legacy = 63 * 512;
        let mut dev = MemDevice::new(DISK);
        let mut t = Table::empty(TableKind::Gpt, DISK, 512);
        t.gpt_first_usable = 34 * 512;
        for (number, start) in [(1, legacy), (2, 10 * MIB + legacy)] {
            t.entries.push(Entry {
                number,
                start,
                len: 8 * MIB,
                ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
                guid: Some(Uuid::new_v4()),
                name: String::new(),
                attrs: 0,
                logical: false,
            });
        }
        pm_parttable::write_table(&mut dev, &t).unwrap();
        // partition 2 is a locked BitLocker volume: no reader knows it
        boot_sector(&mut dev, 10 * MIB + legacy, b"-FVE-FS-");
        p.add_disk(1, dev, Default::default());
        let op = Operation::AlignPartitions { disk: 1 };
        let before = p.disk_bytes(1);
        let msg = message(run_step(&p, &op, |ctx| table_ops::align(ctx, 1)).unwrap_err());
        assert!(msg.contains("BitLocker") && msg.contains("partition 2"), "{msg}");
        assert_eq!(p.disk_bytes(1), before, "partition 1 was not moved either");
        assert_eq!(p.write_sessions_log(), p.finished_sessions_log());
    }

    #[test]
    fn waiting_for_volumes_returns_once_they_are_back() {
        let p = MockPlatform::new();
        p.add_disk(1, gpt_disk(&[(1, MIB, 8 * MIB)]), Default::default());
        let v = VolumeInfo { id: VolumeId("v1".into()), ..Default::default() };
        p.add_volume(v, 1, MIB, 8 * MIB);
        let cancel = CancelToken::new();
        assert!(wait_for_volumes(&p, 1, 1, &cancel).unwrap());
        // a volume that never comes back: bounded wait, not an error
        let t = Instant::now();
        assert!(!wait_for_volumes_with(&p, 1, 2, &cancel, Duration::from_millis(600)).unwrap());
        assert!(t.elapsed() < Duration::from_secs(5));
        // cancellable
        cancel.cancel();
        assert!(wait_for_volumes(&p, 1, 2, &cancel).is_err());
    }
}
