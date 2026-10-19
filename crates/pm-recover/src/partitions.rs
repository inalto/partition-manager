//! Lost-partition finder and the step that turns chosen candidates back into table entries.

use pm_core::units::{align_up, MIB};
use pm_core::{
    gpt_types, BlockDevice, CancelToken, Error, FsKind, NullProgress, PartitionType, ProgressSink,
    ProgressTracker, Result, TableKind,
};
use pm_fs::probe_for_search;
use pm_parttable::{read_table, search_lost, Candidate, CandidateSource, Entry, Table};
use uuid::Uuid;

const CONFIDENCE_BOOT_SECTOR: u8 = 70;
/// Confidence lost when a candidate is cut down to the gap it lies in.
const CAP_PENALTY: u8 = 20;
/// Probe step of both scans.
const STEP: u64 = MIB;
/// Longest GPT partition name, in UTF-16 code units.
const GPT_NAME_UNITS: usize = 36;
const GPT_MAX_ENTRIES: usize = 128;
const MBR_MAX_PRIMARIES: usize = 4;

/// Look for partitions that the partition table no longer describes.
///
/// * `quick`: entries of a surviving backup GPT that the current table lacks, plus a probe at
///   every 1 MiB step inside the table's free ranges (the whole disk when it has no table).
///   Existing partitions are never scanned, so the cost follows the amount of free space.
/// * full: `pm_parttable::search_lost` over the whole disk with `pm_fs::probe_for_search`, at
///   1 MiB steps.
///
/// Afterwards candidates that exactly match an existing table entry (same start, length within
/// 1 MiB) or do not start in unallocated space are dropped. A candidate longer than the gap it
/// lies in is kept with its length cut to the gap and its confidence reduced by 20. The result
/// is sorted by start. Partitions that start below 1 MiB are not searched for. The device is
/// only read.
pub fn find_partitions(
    dev: &mut dyn BlockDevice,
    quick: bool,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Vec<Candidate>> {
    cancel.check()?;
    let dev_len = dev.len();
    let table = match read_table(dev) {
        Ok(t) => t,
        // a damaged or unreadable table is exactly the situation this search is for
        Err(e @ (Error::Io { .. } | Error::Cancelled)) => return Err(e),
        Err(_) => Table::empty(TableKind::None, dev_len, dev.sector_size()),
    };
    let found = if quick {
        quick_scan(dev, &table, progress, cancel)?
    } else {
        search_lost(dev, &probe_for_search, STEP, progress, cancel)?
    };
    Ok(finish(found, &table, dev_len))
}

fn quick_scan(
    dev: &mut dyn BlockDevice,
    table: &Table,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Vec<Candidate>> {
    let dev_len = dev.len();
    // A step past the end makes search_lost report only the backup GPT entries.
    let mut found = search_lost(dev, &probe_for_search, dev_len.max(1), &NullProgress, cancel)?;
    // entries the table already has must not shadow lost partitions that overlap them
    found.retain(|c| !matches_existing(table, c));

    let ranges = table.free_ranges(MIB);
    let total: u64 = ranges.iter().map(|r| r.1).sum();
    let mut tracker = ProgressTracker::new(progress, "Searching", total);
    for (rs, rl) in ranges {
        let end = rs.saturating_add(rl).min(dev_len);
        let mut off = rs;
        while off < end {
            cancel.check()?;
            let mut next = align_up(off + 1, STEP);
            if let Some((fs, label, fs_len)) = probe_for_search(dev, off) {
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
                    // the filesystem occupies this stretch; do not mistake its insides for more
                    next = next.max(align_up(off + len, STEP));
                }
            }
            tracker.add(next.min(end) - off);
            off = next;
        }
    }
    tracker.finish();
    Ok(dedupe(found))
}

/// Keep the highest-confidence candidates; anything overlapping a kept one is dropped.
fn dedupe(mut all: Vec<Candidate>) -> Vec<Candidate> {
    all.sort_by(|a, b| b.confidence.cmp(&a.confidence).then(a.start.cmp(&b.start)));
    let mut kept: Vec<Candidate> = Vec::new();
    for c in all {
        let c_end = c.start.saturating_add(c.len);
        if !kept.iter().any(|k| c.start < k.start.saturating_add(k.len) && k.start < c_end) {
            kept.push(c);
        }
    }
    kept
}

/// Whether `c` is an entry of the current table (same start, length within 1 MiB).
fn matches_existing(table: &Table, c: &Candidate) -> bool {
    table.entries.iter().any(|e| e.start == c.start && e.len.abs_diff(c.len) <= MIB)
}

/// Drop what the table already describes, cap candidates to their gap and sort.
fn finish(found: Vec<Candidate>, table: &Table, dev_len: u64) -> Vec<Candidate> {
    let ss = table.sector_size.max(1) as u64;
    let gaps: Vec<(u64, u64)> = match table.kind {
        TableKind::Gpt | TableKind::Mbr => table.free_ranges(1),
        TableKind::None | TableKind::Unknown => vec![(0, dev_len)],
    };
    let mut out = Vec::new();
    for mut c in found {
        if matches_existing(table, &c) {
            continue;
        }
        let Some(&(gs, gl)) = gaps.iter().find(|&&(s, l)| c.start >= s && c.start - s < l) else {
            continue; // starts inside an existing partition (or outside the usable area)
        };
        let gap_end = gs + gl;
        if c.start.saturating_add(c.len) > gap_end {
            c.len = gap_end - c.start;
            c.confidence = c.confidence.saturating_sub(CAP_PENALTY);
        }
        if c.len < ss {
            continue;
        }
        out.push(c);
    }
    out.sort_by_key(|c| c.start);
    out
}

fn gpt_type(fs: FsKind) -> PartitionType {
    let g = match fs {
        FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4 | FsKind::Btrfs | FsKind::Xfs | FsKind::Luks => {
            gpt_types::LINUX_FS
        }
        FsKind::LinuxSwap => gpt_types::LINUX_SWAP,
        FsKind::HfsPlus => gpt_types::APPLE_HFS,
        FsKind::Apfs => gpt_types::APPLE_APFS,
        // NTFS, FAT, exFAT, ReFS and anything unrecognised
        _ => gpt_types::BASIC_DATA,
    };
    PartitionType::Gpt(g)
}

fn mbr_type(fs: FsKind) -> PartitionType {
    let b = match fs {
        FsKind::Fat12 => 0x01,
        FsKind::Fat16 => 0x06,
        FsKind::Fat32 => 0x0C,
        FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4 | FsKind::Btrfs | FsKind::Xfs | FsKind::Luks => 0x83,
        FsKind::LinuxSwap => 0x82,
        FsKind::HfsPlus | FsKind::Apfs => 0xAF,
        // NTFS, exFAT, ReFS and anything unrecognised
        _ => 0x07,
    };
    PartitionType::Mbr(b)
}

fn gpt_name(label: &str) -> String {
    let mut units = 0;
    let mut out = String::new();
    for ch in label.chars() {
        units += ch.len_utf16();
        if units > GPT_NAME_UNITS {
            break;
        }
        out.push(ch);
    }
    out
}

/// Add one entry per chosen candidate to a copy of `table`.
///
/// Start and length come from the candidate (the length rounded down to a whole sector); the
/// partition type follows the filesystem kind (GPT: BASIC_DATA for NTFS/FAT/exFAT/ReFS,
/// LINUX_FS for ext/btrfs/XFS/LUKS, LINUX_SWAP, APPLE_HFS, APPLE_APFS; MBR: 0x07 for
/// NTFS/exFAT/ReFS, 0x0C FAT32, 0x06 FAT16, 0x01 FAT12, 0x83 Linux, 0x82 swap, 0xAF HFS+/APFS;
/// unrecognised content gets the generic data type). GPT entries get a fresh GUID and the
/// candidate's label as name; numbers are the lowest free ones, assigned in start order.
///
/// `Error::Invalid` when a candidate overlaps an existing entry or another chosen one (this
/// includes the area inside an MBR extended partition, where logical partitions are not
/// recreated), when the table has no room (MBR: 4 primaries, GPT: 128 entries) or when the
/// result fails [`Table::validate`].
pub fn apply_candidates(table: &Table, chosen: &[Candidate]) -> Result<Table> {
    let ss = table.sector_size as u64;
    if ss == 0 {
        return Err(Error::Invalid("sector size is zero".into()));
    }
    match table.kind {
        TableKind::Mbr => {
            let primaries = table.entries.iter().filter(|e| !e.logical).count();
            if primaries + chosen.len() > MBR_MAX_PRIMARIES {
                return Err(Error::Invalid(format!(
                    "no room: an MBR holds at most {MBR_MAX_PRIMARIES} primary partitions \
                     ({primaries} present, {} to add)",
                    chosen.len()
                )));
            }
        }
        TableKind::Gpt => {
            if table.entries.len() + chosen.len() > GPT_MAX_ENTRIES {
                return Err(Error::Invalid(format!(
                    "no room: a GPT holds at most {GPT_MAX_ENTRIES} partitions \
                     ({} present, {} to add)",
                    table.entries.len(),
                    chosen.len()
                )));
            }
        }
        TableKind::None | TableKind::Unknown => {
            return Err(Error::Invalid("the disk has no partition table to add partitions to".into()));
        }
    }

    let mut sorted: Vec<&Candidate> = chosen.iter().collect();
    sorted.sort_by_key(|c| c.start);
    let mut out = table.clone();
    let existing = table.entries.len();
    for c in sorted {
        let len = c.len / ss * ss;
        let end = c
            .start
            .checked_add(len)
            .filter(|_| len > 0)
            .ok_or_else(|| Error::Invalid(format!("candidate at {} has no usable length", c.start)))?;
        for (i, e) in out.entries.iter().enumerate() {
            if c.start < e.end() && e.start < end {
                let what = if i >= existing {
                    format!("another chosen candidate (at {})", e.start)
                } else if table.kind == TableKind::Mbr && !e.logical && e.ptype.is_extended_mbr() {
                    format!("extended partition {} (logical partitions are not recreated)", e.number)
                } else {
                    format!("partition {}", e.number)
                };
                return Err(Error::Invalid(format!(
                    "candidate at byte {} overlaps {what}",
                    c.start
                )));
            }
        }
        let (ptype, guid, name) = match table.kind {
            TableKind::Gpt => (gpt_type(c.fs), Some(Uuid::new_v4()), gpt_name(&c.label)),
            _ => (mbr_type(c.fs), None, c.label.clone()),
        };
        out.entries.push(Entry {
            number: out.next_number(),
            start: c.start,
            len,
            ptype,
            guid,
            name,
            attrs: 0,
            logical: false,
        });
    }
    out.validate()?;
    Ok(out)
}
