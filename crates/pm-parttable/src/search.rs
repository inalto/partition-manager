//! Lost-partition search: backup GPT entries plus a probe-driven scan for filesystem boot
//! sectors. The filesystem probe is injected so this crate does not depend on `pm-fs`.

use crate::gpt;
use pm_core::{BlockDevice, CancelToken, Error, FsKind, ProgressSink, ProgressTracker, Result};
use serde::{Deserialize, Serialize};

const CONFIDENCE_GPT_BACKUP: u8 = 90;
const CONFIDENCE_BOOT_SECTOR: u8 = 70;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateSource {
    /// A filesystem boot sector found by the scan.
    BootSector,
    /// A backup boot sector at the end of a filesystem (for example NTFS). Not produced by
    /// [`search_lost`]: a probe cannot tell a backup sector from a primary one, so pm-recover
    /// may add these candidates itself.
    BackupBootSector,
    /// An entry of a valid backup GPT.
    GptBackupEntry,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Byte offset of the partition.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
    pub fs: FsKind,
    pub label: String,
    pub source: CandidateSource,
    /// 0-100, higher is more trustworthy.
    pub confidence: u8,
}

impl Candidate {
    fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }

    fn overlaps(&self, other: &Candidate) -> bool {
        self.start < other.end() && other.start < self.end()
    }
}

/// Filesystem probe: `(fs, label, fs_len_bytes)` for a filesystem starting at the given offset.
type Probe<'a> = &'a dyn Fn(&mut dyn BlockDevice, u64) -> Option<(FsKind, String, u64)>;

/// Look for partitions on a disk whose table is lost.
///
/// 1. If a valid backup GPT exists (even when the primary is gone) every entry becomes a
///    `GptBackupEntry` candidate with confidence 90; `probe` supplies the filesystem kind and
///    label when it recognises the entry's start.
/// 2. `probe` is called at `step, 2 * step, ...` below the end of the device; each hit becomes
///    a `BootSector` candidate (confidence 70, `len` = the reported filesystem length, clamped
///    to the device).
///
/// Overlapping candidates are resolved in favour of the higher confidence (the earlier start on
/// a tie); the result is sorted by start. Progress is reported as phase "Searching" over the
/// device length, and `cancel` is checked on every iteration.
pub fn search_lost(
    dev: &mut dyn BlockDevice,
    probe: Probe<'_>,
    step: u64,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Vec<Candidate>> {
    if step == 0 {
        return Err(Error::Invalid("search step must be greater than zero".into()));
    }
    let dev_len = dev.len();
    let mut tracker = ProgressTracker::new(progress, "Searching", dev_len);
    cancel.check()?;
    let mut found: Vec<Candidate> = Vec::new();

    if let Ok(backup) = gpt::parse_gpt_backup(dev) {
        for e in &backup.entries {
            cancel.check()?;
            let (fs, label) = match probe(dev, e.start) {
                Some((fs, label, _)) => (fs, if label.is_empty() { e.name.clone() } else { label }),
                None => (FsKind::Unknown, e.name.clone()),
            };
            found.push(Candidate {
                start: e.start,
                len: e.len,
                fs,
                label,
                source: CandidateSource::GptBackupEntry,
                confidence: CONFIDENCE_GPT_BACKUP,
            });
        }
    }

    // The scan starts at `step`; the leading chunk counts as done.
    tracker.add(step.min(dev_len));
    let mut off = step;
    while off < dev_len {
        cancel.check()?;
        if let Some((fs, label, fs_len)) = probe(dev, off) {
            let len = fs_len.min(dev_len - off);
            if len > 0 {
                found.push(Candidate {
                    start: off,
                    len,
                    fs,
                    label,
                    source: CandidateSource::BootSector,
                    confidence: CONFIDENCE_BOOT_SECTOR,
                });
            }
        }
        tracker.add(step.min(dev_len - off));
        off = off.saturating_add(step);
    }
    tracker.finish();
    Ok(dedupe(found))
}

/// Keep the highest-confidence candidates; anything overlapping an already kept one is dropped.
fn dedupe(mut all: Vec<Candidate>) -> Vec<Candidate> {
    all.sort_by(|a, b| b.confidence.cmp(&a.confidence).then(a.start.cmp(&b.start)));
    let mut kept: Vec<Candidate> = Vec::new();
    for c in all {
        if !kept.iter().any(|k| k.overlaps(&c)) {
            kept.push(c);
        }
    }
    kept.sort_by_key(|c| c.start);
    kept
}
