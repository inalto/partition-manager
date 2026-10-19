//! Identity checks between the model operations were queued against and the disks they are
//! about to run on.
//!
//! Operations address disks by index and partitions by number. Both can silently point at
//! something else by the time an operation runs: a disk may have been swapped, a partition
//! deleted and another created in its place, and MBR logical partitions are renumbered on
//! every re-read. [`layout_matches`] compares everything an operation references on both
//! models; the executor and the GUI refuse (or drop) the operation when it fails.

use crate::model::Model;
use crate::operation::Operation;
use pm_core::{DiskInfo, Error, PartitionInfo, Result, TableKind};

/// What an operation refers to by index or number.
struct Refs {
    /// Disks whose identity (size, serial, disk id) must not have changed.
    disks: Vec<u32>,
    /// Partitions `(disk, number)` that must still be the same partition.
    parts: Vec<(u32, u32)>,
    /// Disks whose whole partition list the operation acts on.
    layouts: Vec<u32>,
}

fn refs(op: &Operation) -> Refs {
    use Operation::*;
    let mut r = Refs { disks: op.disks(), parts: Vec::new(), layouts: Vec::new() };
    match op {
        DeletePartition { disk, number }
        | FormatPartition { disk, number, .. }
        | ResizeMove { disk, number, .. }
        | SetActive { disk, number }
        | SetLabel { disk, number, .. }
        | SetLetter { disk, number, .. }
        | SetType { disk, number, .. }
        | WipePartition { disk, number, .. }
        | ImagePartition { disk, number, .. } => r.parts.push((*disk, *number)),
        ClonePartition { src_disk, src_number, .. } => r.parts.push((*src_disk, *src_number)),
        CloneDisk { src_disk, .. } | MigrateOs { src_disk, .. } => r.layouts.push(*src_disk),
        AlignPartitions { disk } | ConvertTable { disk, .. } | RebuildMbr { disk } => {
            r.layouts.push(*disk)
        }
        InitializeDisk { .. }
        | CreatePartition { .. }
        | WipeDisk { .. }
        | ImageDisk { .. }
        | RestoreImage { .. }
        | Mount { .. }
        | Unmount { .. } => {}
    }
    r
}

fn changed(detail: String) -> Error {
    Error::Invalid(format!(
        "disk layout changed since the operations were queued; refresh and queue again ({detail})"
    ))
}

fn same_disk(e: &DiskInfo, l: &DiskInfo) -> Result<()> {
    let d = e.index;
    if e.size != l.size {
        return Err(changed(format!("Disk {d} is now {} bytes, was {}", l.size, e.size)));
    }
    if e.serial.trim() != l.serial.trim() {
        return Err(changed(format!("Disk {d} has a different serial number")));
    }
    if !e.disk_id.trim().eq_ignore_ascii_case(l.disk_id.trim()) {
        return Err(changed(format!("Disk {d} has a different disk identifier")));
    }
    Ok(())
}

fn same_partition(kind: TableKind, d: u32, e: Option<&PartitionInfo>, l: Option<&PartitionInfo>, n: u32) -> Result<()> {
    match (e, l) {
        (None, None) => Ok(()),
        (Some(_), None) => Err(changed(format!("partition {n} on Disk {d} is gone"))),
        (None, Some(_)) => Err(changed(format!("Disk {d} has a new partition {n}"))),
        (Some(e), Some(l)) => {
            if (e.start, e.len) != (l.start, l.len) {
                return Err(changed(format!(
                    "partition {n} on Disk {d} is now at {}+{}, was {}+{}",
                    l.start, l.len, e.start, e.len
                )));
            }
            if kind == TableKind::Gpt && e.guid != l.guid {
                return Err(changed(format!("partition {n} on Disk {d} has a different GUID")));
            }
            Ok(())
        }
    }
}

/// Check that everything `op` references is the same on `live` as on `expected` (the model
/// the operation was planned against): every referenced disk by size, serial number and disk
/// id (GPT GUID / MBR signature), every referenced partition by start and length, plus its
/// GUID on GPT disks; whole-disk operations that act on every partition (clone / migrate
/// source, align, convert, rebuild) compare the whole partition list. Something referenced
/// that exists on neither model is left to the planner to report.
///
/// Fails with `Error::Invalid("disk layout changed since the operations were queued; refresh
/// and queue again (...)")`.
pub fn layout_matches(expected: &Model, live: &Model, op: &Operation) -> Result<()> {
    let r = refs(op);
    for &d in &r.disks {
        match (expected.disk(d), live.disk(d)) {
            (None, None) => {}
            (Some(_), None) => return Err(changed(format!("Disk {d} is gone"))),
            (None, Some(_)) => return Err(changed(format!("Disk {d} was not there before"))),
            (Some(e), Some(l)) => same_disk(e, l)?,
        }
    }
    for &(d, n) in &r.parts {
        let kind = live.disk(d).or(expected.disk(d)).map_or(TableKind::Unknown, |x| x.table);
        same_partition(kind, d, expected.partition(d, n), live.partition(d, n), n)?;
    }
    for &d in &r.layouts {
        let (Some(e), Some(l)) = (expected.disk(d), live.disk(d)) else { continue };
        if e.table != l.table || e.partitions.len() != l.partitions.len() {
            return Err(changed(format!("the partitions of Disk {d} changed")));
        }
        for p in &e.partitions {
            same_partition(l.table, d, Some(p), l.partitions.iter().find(|q| q.number == p.number), p.number)?;
        }
    }
    Ok(())
}

/// Take over, from `actual`, the identifiers a simulation generated: `simulated` is `before`
/// with one operation applied in memory, which invents GUIDs for new or converted partitions
/// and new disk ids; the real write (or another simulation) invents different ones. For each
/// disk in `disks`, a partition GUID of `simulated` that `before` did not have is replaced by
/// the GUID of the `actual` partition at the same start and length, and a disk id that
/// changed is replaced by `actual`'s. Everything else — what [`layout_matches`] checks — is
/// left as simulated.
pub fn adopt_generated_ids(before: &Model, simulated: &mut Model, actual: &Model, disks: &[u32]) {
    for &d in disks {
        let (Some(b), Some(a)) = (before.disk(d), actual.disk(d)) else { continue };
        let Some(s) = simulated.disk_mut(d) else { continue };
        if s.disk_id != b.disk_id {
            s.disk_id = a.disk_id.clone();
        }
        for p in &mut s.partitions {
            let generated = p.guid.is_some_and(|g| !b.partitions.iter().any(|q| q.guid == Some(g)));
            if !generated {
                continue;
            }
            if let Some(q) = a.partitions.iter().find(|q| (q.start, q.len) == (p.start, p.len)) {
                p.guid = q.guid;
            }
        }
    }
}
