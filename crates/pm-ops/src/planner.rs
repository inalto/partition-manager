//! Validation and in-memory simulation of pending operations.

use crate::model::{
    carry_volume_fields, default_ptype, is_container, mount_letter, table_from_disk, Model,
};
use crate::operation::{table_name, CloneMode, Operation, Sizing};
use crate::sizing::plan_disk_clone;
use pm_core::units::{align_down, align_up, MIB};
use pm_core::{
    format_size, gpt_types, DiskInfo, Error, FsKind, PartitionInfo, PartitionType, Result,
    TableKind, VolumeId,
};
use pm_parttable::{align_table, gpt_to_mbr, mbr_to_gpt};
use pm_platform::MountPoint;
use std::path::Path;
use uuid::Uuid;

/// Most partitions a GPT written by this application can hold.
const GPT_MAX_ENTRIES: usize = 128;

/// Validates operations against a [`Model`] and computes the model they produce.
pub struct Planner;

fn invalid<T>(m: String) -> Result<T> {
    Err(Error::Invalid(m))
}

/// The message of an error without the `invalid:` prefix of its `Display`.
pub(crate) fn err_text(e: Error) -> String {
    match e {
        Error::Invalid(m) => m,
        other => other.to_string(),
    }
}

fn disk_ref(m: &Model, d: u32) -> Result<&DiskInfo> {
    m.disk(d)
        .ok_or_else(|| Error::Invalid(format!("Disk {d} does not exist")))
}

fn disk_mut(m: &mut Model, d: u32) -> Result<&mut DiskInfo> {
    m.disk_mut(d)
        .ok_or_else(|| Error::Invalid(format!("Disk {d} does not exist")))
}

fn part_ref(m: &Model, d: u32, n: u32) -> Result<&PartitionInfo> {
    disk_ref(m, d)?
        .partitions
        .iter()
        .find(|p| p.number == n)
        .ok_or_else(|| Error::Invalid(format!("Disk {d} has no partition {n}")))
}

fn part_mut(m: &mut Model, d: u32, n: u32) -> Result<&mut PartitionInfo> {
    disk_mut(m, d)?
        .partitions
        .iter_mut()
        .find(|p| p.number == n)
        .ok_or_else(|| Error::Invalid(format!("Disk {d} has no partition {n}")))
}

/// Refuse a whole-disk destructive operation on the running system disk, and on every
/// disk when the system disk cannot be determined.
fn guard_system_disk(m: &Model, d: u32) -> Result<()> {
    match m.effective_system_disk() {
        Some(s) if s == d => invalid(format!("Disk {d} is the running system disk")),
        Some(_) => Ok(()),
        None => invalid(format!(
            "Disk {d}: cannot determine the running system disk; refusing destructive operation"
        )),
    }
}

fn guard_windows(m: &Model, d: u32, p: &PartitionInfo, action: &str) -> Result<()> {
    if m.is_windows_partition(d, p) {
        return invalid(format!(
            "Cannot {action} partition {} on Disk {d}: it holds the running Windows installation",
            p.number
        ));
    }
    Ok(())
}

fn guard_container(d: &DiskInfo, p: &PartitionInfo, action: &str) -> Result<()> {
    if is_container(d.table, p) {
        return invalid(format!(
            "Cannot {action} partition {} on Disk {}: it is an extended partition container",
            p.number, d.index
        ));
    }
    Ok(())
}

fn require_table(d: &DiskInfo) -> Result<()> {
    match d.table {
        TableKind::Mbr | TableKind::Gpt => Ok(()),
        _ => invalid(format!(
            "Disk {} is not initialized (it has no partition table)",
            d.index
        )),
    }
}

fn ptype_fits(kind: TableKind, t: PartitionType) -> bool {
    match (kind, t) {
        (TableKind::Gpt, PartitionType::Gpt(g)) => !g.is_nil(),
        (TableKind::Mbr, PartitionType::Mbr(b)) => b != 0,
        _ => false,
    }
}

/// `src` type expressed for a `kind` table (identity when the families already match).
fn convert_ptype(src: Option<PartitionType>, kind: TableKind) -> PartitionType {
    match (src, kind) {
        (Some(t), k) if ptype_fits(k, t) => t,
        (Some(t), TableKind::Gpt) if t.is_efi() => PartitionType::Gpt(gpt_types::EFI_SYSTEM),
        (Some(t), TableKind::Gpt) if t.is_recovery() => {
            PartitionType::Gpt(gpt_types::WINDOWS_RECOVERY)
        }
        (Some(PartitionType::Mbr(0x83)), TableKind::Gpt) => PartitionType::Gpt(gpt_types::LINUX_FS),
        (Some(t), TableKind::Mbr) if t.is_efi() => PartitionType::Mbr(0xEF),
        (Some(t), TableKind::Mbr) if t.is_recovery() => PartitionType::Mbr(0x27),
        (Some(PartitionType::Gpt(gpt_types::LINUX_FS)), TableKind::Mbr) => PartitionType::Mbr(0x83),
        _ => default_ptype(kind),
    }
}

/// Re-validate the partition layout of `d` (bounds, overlaps, alignment, table limits).
fn validate_layout(d: &DiskInfo) -> Result<()> {
    table_from_disk(d)
        .validate()
        .map_err(|e| Error::Invalid(format!("Disk {}: {}", d.index, err_text(e))))
}

/// Start rounded up and length rounded down so the range stays within the requested one.
fn align_range(start: u64, len: u64) -> (u64, u64) {
    let s = align_up(start, MIB);
    let cut = s.saturating_sub(start);
    (s, align_down(len.saturating_sub(cut), MIB))
}

fn fits_free(d: &DiskInfo, start: u64, len: u64) -> bool {
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    d.free_ranges(MIB)
        .iter()
        .any(|&(fs, fl)| start >= fs && end <= fs.saturating_add(fl))
}

/// Error unless one more primary (or GPT) partition fits the table of `d`.
fn check_room_for_partition(d: &DiskInfo) -> Result<()> {
    match d.table {
        TableKind::Gpt if d.partitions.len() >= GPT_MAX_ENTRIES => invalid(format!(
            "Disk {} already has {GPT_MAX_ENTRIES} partitions, the GPT maximum",
            d.index
        )),
        TableKind::Mbr if d.partitions.iter().filter(|p| !p.logical).count() >= 4 => invalid(
            format!("Disk {}: MBR allows at most 4 primary partitions", d.index),
        ),
        _ => Ok(()),
    }
}

/// Number for a new partition: GPT `max(existing) + 1` (lowest free slot once past 128),
/// MBR the lowest free primary slot 1..=4.
fn new_number(d: &DiskInfo) -> Result<u32> {
    let used = |n: u32| d.partitions.iter().any(|p| p.number == n);
    let n = match d.table {
        TableKind::Gpt => {
            let next = d.partitions.iter().map(|p| p.number).max().unwrap_or(0).saturating_add(1);
            if next as usize <= GPT_MAX_ENTRIES {
                Some(next)
            } else {
                (1..=GPT_MAX_ENTRIES as u32).find(|n| !used(*n))
            }
        }
        _ => (1..=4).find(|n| !used(*n)),
    };
    n.ok_or_else(|| Error::Invalid(format!("Disk {} has no free partition slot", d.index)))
}

/// The upper-cased letter if it is a valid drive letter not used by anything but the
/// partition `own` (`(disk, number)`) and its volume.
fn check_letter(m: &Model, c: char, own: Option<(u32, u32)>, ctx: &str) -> Result<char> {
    let c = c.to_ascii_uppercase();
    if !c.is_ascii_uppercase() {
        return invalid(format!("'{c}' is not a valid drive letter ({ctx})"));
    }
    let own_volume = own
        .and_then(|(d, n)| m.partition(d, n))
        .and_then(|p| p.volume.clone());
    let in_partitions = m.disks.iter().any(|d| {
        d.partitions
            .iter()
            .any(|p| own != Some((d.index, p.number)) && p.letter() == Some(c))
    });
    let in_volumes = m.volumes.iter().any(|v| {
        Some(&v.id) != own_volume.as_ref()
            && v.mount_points.iter().any(|mp| mount_letter(mp) == Some(c))
    });
    if in_partitions || in_volumes {
        return invalid(format!("Drive letter {c}: is already in use ({ctx})"));
    }
    Ok(c)
}

/// Does volume `v` belong to partition `p` of disk `d`?
fn volume_matches(v: &pm_core::VolumeInfo, d: u32, p: &PartitionInfo) -> bool {
    match &p.volume {
        Some(id) => v.id == *id,
        None => v.disk == Some(d) && v.start == p.start && v.len == p.len,
    }
}

fn remove_volume_of(m: &mut Model, d: u32, p: &PartitionInfo) {
    m.volumes.retain(|v| !volume_matches(v, d, p));
}

fn for_volume_of(m: &mut Model, d: u32, p: &PartitionInfo, f: impl Fn(&mut pm_core::VolumeInfo)) {
    for v in m.volumes.iter_mut().filter(|v| volume_matches(v, d, p)) {
        f(v);
    }
}

/// Error if `label` is too long for `fs` (FAT/exFAT 11 characters, NTFS/ReFS 32).
fn check_label(fs: Option<FsKind>, label: &str, d: u32, n: u32) -> Result<()> {
    let max = match fs {
        Some(FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::ExFat) => 11,
        Some(FsKind::Ntfs | FsKind::ReFs) => 32,
        _ => return Ok(()),
    };
    if label.chars().count() > max {
        return invalid(format!(
            "Label \"{label}\" for partition {n} on Disk {d} is longer than {max} characters"
        ));
    }
    Ok(())
}

fn sort_partitions(d: &mut DiskInfo) {
    d.partitions.sort_by_key(|p| (p.start, p.logical));
}

fn check_image_path(path: &Path, what: &str) -> Result<()> {
    if path.file_name().is_none() {
        return invalid(format!("\"{}\" is not a valid image file name ({what})", path.display()));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return invalid(format!(
                "Folder {} for the image of {what} does not exist",
                parent.display()
            ));
        }
    }
    Ok(())
}

impl Planner {
    /// Validate `op` against `model` and return the model as it will be after `op` is
    /// applied. New partitions get provisional numbers, GUIDs and drive letters.
    /// Every rejection is an `Error::Invalid` naming the disk / partition concerned.
    pub fn simulate(model: &Model, op: &Operation) -> Result<Model> {
        let mut m = model.clone();
        match op {
            Operation::InitializeDisk { disk, kind } => initialize(&mut m, *disk, *kind)?,
            Operation::CreatePartition { disk, start, len, ptype, fs, label, letter, name } => {
                create(&mut m, *disk, *start, *len, *ptype, *fs, label, *letter, name)?
            }
            Operation::DeletePartition { disk, number } => {
                let p = part_ref(&m, *disk, *number)?.clone();
                guard_windows(&m, *disk, &p, "delete")?;
                let d = disk_mut(&mut m, *disk)?;
                if is_container(d.table, &p) && d.partitions.iter().any(|q| q.logical) {
                    return invalid(format!(
                        "Delete the logical partitions inside extended partition {number} on \
                         Disk {disk} first"
                    ));
                }
                d.partitions.retain(|q| q.number != *number);
                remove_volume_of(&mut m, *disk, &p);
            }
            Operation::FormatPartition { disk, number, fs, label, cluster, .. } => {
                let p = part_ref(&m, *disk, *number)?.clone();
                guard_windows(&m, *disk, &p, "format")?;
                guard_container(disk_ref(&m, *disk)?, &p, "format")?;
                if let Some(c) = cluster {
                    if *c < 512 || !c.is_power_of_two() {
                        return invalid(format!(
                            "Cluster size {c} for partition {number} on Disk {disk} must be a \
                             power of two of at least 512 bytes"
                        ));
                    }
                }
                check_label(Some(*fs), label, *disk, *number)?;
                let q = part_mut(&mut m, *disk, *number)?;
                q.fs = Some(*fs);
                q.label = label.clone();
                q.used = Some(0);
                for_volume_of(&mut m, *disk, &p, |v| {
                    v.fs = Some(*fs);
                    v.label = label.clone();
                    v.free = v.total;
                });
            }
            Operation::ResizeMove { disk, number, new_start, new_len } => {
                resize_move(&mut m, *disk, *number, *new_start, *new_len)?
            }
            Operation::SetActive { disk, number } => {
                let d = disk_ref(&m, *disk)?;
                if d.table != TableKind::Mbr {
                    return invalid(format!(
                        "Disk {disk} is not an MBR disk; only MBR partitions can be set active"
                    ));
                }
                let p = part_ref(&m, *disk, *number)?;
                if p.logical || is_container(d.table, p) {
                    return invalid(format!(
                        "Partition {number} on Disk {disk} is not a primary partition and cannot \
                         be set active"
                    ));
                }
                for q in &mut disk_mut(&mut m, *disk)?.partitions {
                    q.flags.active = q.number == *number;
                }
            }
            Operation::SetLabel { disk, number, label } => {
                let p = part_ref(&m, *disk, *number)?.clone();
                check_label(p.fs, label, *disk, *number)?;
                part_mut(&mut m, *disk, *number)?.label = label.clone();
                for_volume_of(&mut m, *disk, &p, |v| v.label = label.clone());
            }
            Operation::SetLetter { disk, number, letter } => {
                set_letter(&mut m, *disk, *number, *letter)?
            }
            Operation::SetType { disk, number, ptype } => {
                let d = disk_ref(&m, *disk)?;
                let p = part_ref(&m, *disk, *number)?.clone();
                guard_windows(&m, *disk, &p, "change the type of")?;
                guard_container(d, &p, "change the type of")?;
                if !ptype_fits(d.table, *ptype) || (!p.logical && ptype.is_extended_mbr()) {
                    return invalid(format!(
                        "Type {} cannot be used for partition {number} on the {} Disk {disk}",
                        ptype.name(),
                        table_name(d.table)
                    ));
                }
                part_mut(&mut m, *disk, *number)?.ptype = Some(*ptype);
            }
            Operation::Mount { volume, at } => mount(&mut m, volume, at)?,
            Operation::Unmount { volume, at } => unmount(&mut m, volume, at)?,
            Operation::WipeDisk { disk, .. } => {
                guard_system_disk(&m, *disk)?;
                let d = disk_mut(&mut m, *disk)?;
                d.partitions.clear();
                d.table = TableKind::None;
                d.disk_id.clear();
                m.volumes.retain(|v| v.disk != Some(*disk));
            }
            Operation::WipePartition { disk, number, .. } => {
                let p = part_ref(&m, *disk, *number)?.clone();
                guard_windows(&m, *disk, &p, "wipe")?;
                guard_container(disk_ref(&m, *disk)?, &p, "wipe")?;
                let q = part_mut(&mut m, *disk, *number)?;
                q.used = Some(0);
                q.fs = None;
                q.label.clear();
                for_volume_of(&mut m, *disk, &p, |v| {
                    v.fs = None;
                    v.label.clear();
                });
            }
            Operation::ConvertTable { disk, to } => convert(&mut m, *disk, *to)?,
            Operation::AlignPartitions { disk } => align(&mut m, *disk)?,
            Operation::RebuildMbr { disk } => {
                disk_ref(&m, *disk)?;
            }
            Operation::ClonePartition {
                src_disk,
                src_number,
                dst_disk,
                dst_start,
                dst_len,
                mode,
            } => clone_partition(
                &mut m,
                (*src_disk, *src_number),
                *dst_disk,
                *dst_start,
                *dst_len,
                *mode,
            )?,
            Operation::CloneDisk { src_disk, dst_disk, sizing, mode, .. } => {
                clone_disk(&mut m, *src_disk, *dst_disk, *sizing, *mode, false)?
            }
            Operation::MigrateOs { src_disk, dst_disk, only_system_parts, sizing, .. } => {
                let only = *only_system_parts;
                clone_disk(&mut m, *src_disk, *dst_disk, *sizing, CloneMode::Smart, only)?
            }
            Operation::ImageDisk { disk, path, .. } => {
                disk_ref(&m, *disk)?;
                check_image_path(path, &format!("Disk {disk}"))?;
            }
            Operation::ImagePartition { disk, number, path, .. } => {
                part_ref(&m, *disk, *number)?;
                check_image_path(path, &format!("partition {number} on Disk {disk}"))?;
            }
            Operation::RestoreImage { path, dst_disk, dst_start } => {
                restore(&mut m, path, *dst_disk, *dst_start)?
            }
        }
        if let Some(d) = rewritten_table(model, &m, op) {
            if let Some(disk) = m.disk_mut(d) {
                renumber_logicals(disk);
            }
        }
        Ok(m)
    }

    /// [`Planner::simulate`] folded over `ops`; stops at the first invalid operation.
    pub fn simulate_all(model: &Model, ops: &[Operation]) -> Result<Model> {
        ops.iter()
            .try_fold(model.clone(), |m, op| Planner::simulate(&m, op))
    }
}

/// The disk whose partition table the executor rewrites for `op` (from the table it reads off
/// the disk), when it does: `before` and `after` are the models around the simulation.
fn rewritten_table(before: &Model, after: &Model, op: &Operation) -> Option<u32> {
    use Operation::*;
    match op {
        CreatePartition { disk, .. }
        | DeletePartition { disk, .. }
        | ResizeMove { disk, .. }
        | SetActive { disk, .. }
        | SetType { disk, .. }
        | AlignPartitions { disk }
        | RebuildMbr { disk } => Some(*disk),
        ClonePartition { dst_disk, .. } => Some(*dst_disk),
        // over an existing partition only data is written; into free space a partition is added
        RestoreImage { dst_disk, dst_start: Some(_), .. } => {
            let count = |m: &Model| m.disk(*dst_disk).map(|d| d.partitions.len());
            (count(before) != count(after)).then_some(*dst_disk)
        }
        _ => None,
    }
}

/// Number the MBR logical partitions of `d` the way `pm_parttable::read_table` will after the
/// table is written: the EBR chain is written in start order and its logicals are numbered
/// from 5 in chain order. Deleting logical 5 of 5, 6, 7 therefore turns 6 into 5 and 7 into 6.
fn renumber_logicals(d: &mut DiskInfo) {
    if d.table != TableKind::Mbr {
        return;
    }
    let mut logicals: Vec<&mut PartitionInfo> = d.partitions.iter_mut().filter(|p| p.logical).collect();
    logicals.sort_by_key(|p| p.start);
    for (i, p) in logicals.into_iter().enumerate() {
        p.number = 5 + i as u32;
    }
}

fn initialize(m: &mut Model, disk: u32, kind: TableKind) -> Result<()> {
    guard_system_disk(m, disk)?;
    let d = disk_ref(m, disk)?;
    if !matches!(kind, TableKind::Mbr | TableKind::Gpt) {
        return invalid(format!("Disk {disk} can only be initialized as MBR or GPT"));
    }
    if !d.partitions.is_empty() {
        return invalid(format!(
            "Disk {disk} still has partitions; delete them or wipe the disk before initializing it"
        ));
    }
    let ss = (d.sector_size.max(512)) as u64;
    if kind == TableKind::Mbr && d.size / ss > u32::MAX as u64 {
        return invalid(format!(
            "Disk {disk} ({}) is too large for MBR; use GPT",
            format_size(d.size)
        ));
    }
    let t = pm_parttable::Table::empty(kind, d.size, d.sector_size.max(512));
    let new = t.to_disk_info(d.clone());
    *disk_mut(m, disk)? = new;
    m.volumes.retain(|v| v.disk != Some(disk));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create(
    m: &mut Model,
    disk: u32,
    start: u64,
    len: u64,
    ptype: PartitionType,
    fs: Option<FsKind>,
    label: &str,
    letter: Option<char>,
    name: &str,
) -> Result<()> {
    let d = disk_ref(m, disk)?;
    require_table(d)?;
    if !ptype_fits(d.table, ptype) {
        return invalid(format!(
            "Type {} cannot be used for a new partition on the {} Disk {disk}",
            ptype.name(),
            table_name(d.table)
        ));
    }
    let (s, l) = align_range(start, len);
    if l < MIB {
        return invalid(format!(
            "The new partition on Disk {disk} is smaller than 1 MB after 1 MB alignment"
        ));
    }
    if !fits_free(d, s, l) {
        return invalid(format!(
            "The new partition on Disk {disk} ({} – {}) overlaps an existing partition or lies \
             outside the unallocated space",
            format_size(s),
            format_size(s.saturating_add(l))
        ));
    }
    check_room_for_partition(d)?;
    check_label(fs, label, disk, 0).map_err(|_| {
        Error::Invalid(format!(
            "Label \"{label}\" for the new partition on Disk {disk} is too long for its filesystem"
        ))
    })?;
    let letter = letter
        .map(|c| check_letter(m, c, None, &format!("new partition on Disk {disk}")))
        .transpose()?;
    let number = new_number(d)?;
    let gpt = d.table == TableKind::Gpt;
    let p = PartitionInfo {
        number,
        start: s,
        len: l,
        ptype: Some(ptype),
        guid: gpt.then(Uuid::new_v4),
        name: if gpt { name.to_string() } else { String::new() },
        fs,
        label: label.to_string(),
        mount_points: letter.map(|c| vec![format!("{c}:\\")]).unwrap_or_default(),
        used: fs.map(|_| 0),
        ..Default::default()
    };
    let d = disk_mut(m, disk)?;
    d.partitions.push(p);
    sort_partitions(d);
    validate_layout(d)
}

fn resize_move(m: &mut Model, disk: u32, number: u32, new_start: u64, new_len: u64) -> Result<()> {
    let p = part_ref(m, disk, number)?.clone();
    let size = disk_ref(m, disk)?.size;
    let moving = new_start != p.start;
    if moving && m.is_windows_partition(disk, &p) {
        return invalid(format!(
            "Cannot move partition {number} on Disk {disk}: it holds the running Windows \
             installation (it can only be resized in place)"
        ));
    }
    if new_len < MIB {
        return invalid(format!("Partition {number} on Disk {disk} cannot be smaller than 1 MB"));
    }
    match p.used {
        Some(used) if new_len < used => {
            return invalid(format!(
                "Partition {number} on Disk {disk} cannot be smaller than its used data ({})",
                format_size(used)
            ));
        }
        // Same conservative rule as the clone paths: a filesystem of unknown fill level may
        // not be shrunk; a raw partition (no filesystem) may.
        None if new_len < p.len && p.fs.is_some() => {
            return invalid(format!(
                "Cannot shrink partition {number} on Disk {disk}: its used space is unknown; \
                 mount or check the volume first"
            ));
        }
        _ => {}
    }
    // An unknown filesystem may only be one the disk list could not read: never shrink it
    // (only the table would follow, truncating the data).
    if new_len < p.len && p.fs.is_none() && !is_container(disk_ref(m, disk)?.table, &p) {
        return Err(crate::steps::resize::unknown_fs_shrink(disk, number));
    }
    let end = match new_start.checked_add(new_len) {
        Some(end) if end <= size => end,
        _ => {
            return invalid(format!(
                "Partition {number} on Disk {disk} would extend past the end of the disk"
            ))
        }
    };
    if moving {
        // A moved partition lands on the 1 MiB grid; only an in-place resize may keep a
        // legacy (unaligned) start.
        let usable_end = {
            let t = table_from_disk(disk_ref(m, disk)?);
            match t.kind {
                TableKind::Gpt => t.gpt_last_usable,
                _ => t.disk_len,
            }
        };
        if new_start < MIB || new_start % MIB != 0 {
            return invalid(format!(
                "Partition {number} on Disk {disk} must move to a 1 MB-aligned start of at least \
                 1 MB (got {} bytes)",
                new_start
            ));
        }
        if end % MIB != 0 && end != usable_end {
            return invalid(format!(
                "Partition {number} on Disk {disk} must end on a 1 MB boundary or at the end of \
                 the usable area when it is moved"
            ));
        }
    }
    let q = part_mut(m, disk, number)?;
    q.start = new_start;
    q.len = new_len;
    let d = disk_mut(m, disk)?;
    sort_partitions(d);
    validate_layout(d)?;
    for_volume_of(m, disk, &p, |v| {
        v.start = new_start;
        v.len = new_len;
    });
    Ok(())
}

fn set_letter(m: &mut Model, disk: u32, number: u32, letter: Option<char>) -> Result<()> {
    let p = part_ref(m, disk, number)?.clone();
    guard_windows(m, disk, &p, "change the drive letter of")?;
    guard_container(disk_ref(m, disk)?, &p, "assign a drive letter to")?;
    let letter = letter
        .map(|c| {
            let ctx = format!("partition {number} on Disk {disk}");
            check_letter(m, c, Some((disk, number)), &ctx)
        })
        .transpose()?;
    let apply = |mps: &mut Vec<String>| {
        mps.retain(|mp| mount_letter(mp).is_none());
        if let Some(c) = letter {
            mps.insert(0, format!("{c}:\\"));
        }
    };
    apply(&mut part_mut(m, disk, number)?.mount_points);
    for_volume_of(m, disk, &p, |v| apply(&mut v.mount_points));
    Ok(())
}

/// Mount points compare case-insensitively, with or without the trailing backslash.
fn same_mount(a: &str, b: &str) -> bool {
    a.trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(b.trim_end_matches(['\\', '/']))
}

fn volume_name(m: &Model, id: &VolumeId) -> String {
    match m.volumes.iter().find(|v| v.id == *id).and_then(|v| v.disk) {
        Some(d) => format!("volume {} on Disk {d}", id.0),
        None => format!("volume {}", id.0),
    }
}

/// The partition backing volume `vi` of the model, as `(disk, number)`.
fn partition_of_volume(m: &Model, vi: usize) -> Option<(u32, u32)> {
    let v = &m.volumes[vi];
    m.disks.iter().find_map(|d| {
        d.partitions
            .iter()
            .find(|p| volume_matches(v, d.index, p))
            .map(|p| (d.index, p.number))
    })
}

fn mount(m: &mut Model, volume: &VolumeId, at: &MountPoint) -> Result<()> {
    let Some(vi) = m.volumes.iter().position(|v| v.id == *volume) else {
        return invalid(format!("Volume {} does not exist", volume.0));
    };
    let what = volume_name(m, volume);
    let path = at.to_path_string();
    match at {
        MountPoint::Letter(c) => {
            check_letter(m, *c, None, &format!("cannot mount {what}"))?;
        }
        MountPoint::Folder(_) => {
            let taken = m
                .volumes
                .iter()
                .flat_map(|v| v.mount_points.iter())
                .chain(
                    m.disks
                        .iter()
                        .flat_map(|d| d.partitions.iter())
                        .flat_map(|p| p.mount_points.iter()),
                )
                .any(|mp| same_mount(mp, &path));
            if taken {
                return invalid(format!(
                    "Mount point {path} is already in use (cannot mount {what})"
                ));
            }
        }
    }
    let owner = partition_of_volume(m, vi);
    m.volumes[vi].mount_points.push(path.clone());
    if let Some((d, n)) = owner {
        part_mut(m, d, n)?.mount_points.push(path);
    }
    Ok(())
}

fn unmount(m: &mut Model, volume: &VolumeId, at: &MountPoint) -> Result<()> {
    let Some(vi) = m.volumes.iter().position(|v| v.id == *volume) else {
        return invalid(format!("Volume {} does not exist", volume.0));
    };
    let what = volume_name(m, volume);
    let path = at.to_path_string();
    if !m.volumes[vi].mount_points.iter().any(|mp| same_mount(mp, &path)) {
        return invalid(format!("{what} is not mounted at {path}"));
    }
    let owner = partition_of_volume(m, vi);
    if let Some((d, n)) = owner {
        let p = part_ref(m, d, n)?;
        if m.is_windows_partition(d, p) && mount_letter(&path) == Some('C') {
            return invalid(format!(
                "Cannot unmount {path} from partition {n} on Disk {d}: it is the running Windows \
                 volume"
            ));
        }
        part_mut(m, d, n)?.mount_points.retain(|mp| !same_mount(mp, &path));
    }
    m.volumes[vi].mount_points.retain(|mp| !same_mount(mp, &path));
    Ok(())
}

fn convert(m: &mut Model, disk: u32, to: TableKind) -> Result<()> {
    guard_system_disk(m, disk)?;
    let d = disk_ref(m, disk)?;
    if d.table == to {
        return invalid(format!("Disk {disk} is already {}", table_name(to)));
    }
    let t = table_from_disk(d);
    let converted = match (d.table, to) {
        (TableKind::Mbr, TableKind::Gpt) => mbr_to_gpt(&t),
        (TableKind::Gpt, TableKind::Mbr) => gpt_to_mbr(&t),
        (TableKind::None | TableKind::Unknown, _) => {
            return invalid(format!(
                "Disk {disk} has no partition table to convert; initialize it instead"
            ))
        }
        _ => return invalid(format!("Disk {disk} can only be converted to MBR or GPT")),
    }
    .map_err(|e| {
        Error::Invalid(format!(
            "Cannot convert Disk {disk} to {}: {}",
            table_name(to),
            err_text(e)
        ))
    })?;
    let mut new = converted.to_disk_info(d.clone());
    carry_volume_fields(&d.partitions, &mut new.partitions, |p| p.start);
    *disk_mut(m, disk)? = new;
    Ok(())
}

fn align(m: &mut Model, disk: u32) -> Result<()> {
    let d = disk_ref(m, disk)?;
    require_table(d)?;
    let aligned = align_table(&table_from_disk(d), MIB);
    for e in &aligned.entries {
        let Some(p) = d.partitions.iter().find(|p| p.number == e.number) else {
            continue;
        };
        if e.start != p.start && m.is_windows_partition(disk, p) {
            return invalid(format!(
                "Cannot align partition {} on Disk {disk}: it would move the running Windows \
                 installation",
                p.number
            ));
        }
        if e.len < p.len && p.fs.is_none() && !is_container(d.table, p) {
            return Err(crate::steps::resize::unknown_fs_shrink(disk, p.number));
        }
        if p.used.map_or(false, |u| u > e.len) {
            return invalid(format!(
                "Cannot align partition {} on Disk {disk}: its used data does not fit the aligned \
                 size",
                p.number
            ));
        }
    }
    let old = d.partitions.clone();
    let mut new = aligned.to_disk_info(d.clone());
    carry_volume_fields(&old, &mut new.partitions, |p| p.number);
    for p in &old {
        if let Some(q) = new.partitions.iter().find(|q| q.number == p.number) {
            let (s, l) = (q.start, q.len);
            for_volume_of(m, disk, p, |v| {
                v.start = s;
                v.len = l;
            });
        }
    }
    *disk_mut(m, disk)? = new;
    Ok(())
}

fn clone_partition(
    m: &mut Model,
    (src_disk, src_number): (u32, u32),
    dst_disk: u32,
    dst_start: u64,
    dst_len: u64,
    mode: CloneMode,
) -> Result<()> {
    let src = part_ref(m, src_disk, src_number)?.clone();
    guard_container(disk_ref(m, src_disk)?, &src, "clone")?;
    let d = disk_ref(m, dst_disk)?;
    require_table(d)?;
    let ss = (d.sector_size.max(512)) as u64;
    let s = align_up(dst_start, MIB);
    let l = align_down(dst_len.saturating_sub(s.saturating_sub(dst_start)), ss);
    let need = match mode {
        CloneMode::Smart => src.used.unwrap_or(src.len),
        CloneMode::Sector => src.len,
    };
    if l < need {
        let what = match mode {
            CloneMode::Smart => "Target smaller than used data",
            CloneMode::Sector => "Target smaller than the source partition",
        };
        return invalid(format!(
            "{what}: partition {src_number} on Disk {src_disk} needs {}, the target on Disk \
             {dst_disk} has {}",
            format_size(need),
            format_size(l)
        ));
    }
    if l < MIB || !fits_free(d, s, l) {
        return invalid(format!(
            "The clone of partition {src_number} on Disk {src_disk} does not fit the unallocated \
             space of Disk {dst_disk} at {} – {}",
            format_size(s),
            format_size(s.saturating_add(l))
        ));
    }
    check_room_for_partition(d)?;
    let number = new_number(d)?;
    let gpt = d.table == TableKind::Gpt;
    let p = PartitionInfo {
        number,
        start: s,
        len: l,
        ptype: Some(convert_ptype(src.ptype, d.table)),
        guid: gpt.then(Uuid::new_v4),
        name: if gpt { src.name.clone() } else { String::new() },
        fs: src.fs,
        label: src.label.clone(),
        used: src.used,
        ..Default::default()
    };
    let d = disk_mut(m, dst_disk)?;
    d.partitions.push(p);
    sort_partitions(d);
    validate_layout(d)
}

fn clone_disk(
    m: &mut Model,
    src_disk: u32,
    dst_disk: u32,
    sizing: Sizing,
    mode: CloneMode,
    only_system: bool,
) -> Result<()> {
    if src_disk == dst_disk {
        return invalid(format!("Cannot clone Disk {src_disk} onto itself"));
    }
    guard_system_disk(m, dst_disk)?;
    let src = disk_ref(m, src_disk)?;
    let dst = disk_ref(m, dst_disk)?;
    let used_of = |p: &PartitionInfo| match mode {
        CloneMode::Smart => p.used.unwrap_or(p.len),
        CloneMode::Sector => p.len,
    };
    let layout = plan_disk_clone(src, dst.size, dst.sector_size, sizing, only_system, &used_of)
        .map_err(|e| {
            Error::Invalid(format!(
                "Cannot clone Disk {src_disk} to Disk {dst_disk}: {}",
                err_text(e)
            ))
        })?;
    let mut new = layout.table.to_disk_info(dst.clone());
    for (sn, dn) in &layout.map {
        let (Some(sp), Some(dp)) = (
            src.partitions.iter().find(|p| p.number == *sn),
            new.partitions.iter_mut().find(|p| p.number == *dn),
        ) else {
            continue;
        };
        dp.fs = sp.fs;
        dp.label = sp.label.clone();
        dp.used = sp.used;
    }
    *disk_mut(m, dst_disk)? = new;
    m.volumes.retain(|v| v.disk != Some(dst_disk));
    Ok(())
}

fn restore(m: &mut Model, path: &Path, dst_disk: u32, dst_start: Option<u64>) -> Result<()> {
    guard_system_disk(m, dst_disk)?;
    let d = disk_ref(m, dst_disk)?;
    let cap = pm_image::open(path, true).map(|dev| dev.len()).map_err(|e| {
        Error::Invalid(format!(
            "Cannot open image {} to restore to Disk {dst_disk}: {e}",
            path.display()
        ))
    })?;
    let Some(start) = dst_start else {
        if cap > d.size {
            return invalid(format!(
                "Image {} ({}) is larger than Disk {dst_disk} ({})",
                path.display(),
                format_size(cap),
                format_size(d.size)
            ));
        }
        let d = disk_mut(m, dst_disk)?;
        d.partitions.clear();
        d.table = TableKind::Unknown;
        d.disk_id.clear();
        m.volumes.retain(|v| v.disk != Some(dst_disk));
        return Ok(());
    };
    require_table(d)?;
    // Over an existing partition: the image must fit inside it.
    if let Some(p) = d.partitions.iter().find(|p| p.start == start).cloned() {
        guard_container(d, &p, "restore an image to")?;
        if cap > p.len {
            return invalid(format!(
                "Image {} ({}) is larger than partition {} on Disk {dst_disk} ({})",
                path.display(),
                format_size(cap),
                p.number,
                format_size(p.len)
            ));
        }
        let q = part_mut(m, dst_disk, p.number)?;
        q.fs = None;
        q.used = None;
        q.label.clear();
        for_volume_of(m, dst_disk, &p, |v| {
            v.fs = None;
            v.label.clear();
        });
        return Ok(());
    }
    // Into unallocated space: a new partition of the image's size.
    if cap < MIB {
        return invalid(format!(
            "Image {} ({}) is smaller than 1 MB and cannot become a partition on Disk {dst_disk}",
            path.display(),
            format_size(cap)
        ));
    }
    let start = align_up(start, MIB);
    let ss = (d.sector_size.max(512)) as u64;
    let len = align_up(cap, ss);
    if !fits_free(d, start, len) {
        return invalid(format!(
            "Image {} ({}) does not fit the unallocated space of Disk {dst_disk} at {}",
            path.display(),
            format_size(cap),
            format_size(start)
        ));
    }
    check_room_for_partition(d)?;
    let number = new_number(d)?;
    let gpt = d.table == TableKind::Gpt;
    let p = PartitionInfo {
        number,
        start,
        len,
        ptype: Some(default_ptype(d.table)),
        guid: gpt.then(Uuid::new_v4),
        ..Default::default()
    };
    let d = disk_mut(m, dst_disk)?;
    d.partitions.push(p);
    sort_partitions(d);
    validate_layout(d)
}
