//! Partition-table operations: initialize, create, delete, set active, set type, convert,
//! align and rebuild. Each reads the table from the device inside a write session, changes it
//! with the planner's numbers and commits it.

use crate::operation::table_name;
use super::{entry_mut, entry_of, format, move_data, with_session, Ctx};
use pm_core::units::MIB;
use pm_core::{Error, FsKind, PartitionType, Result, TableKind};
use pm_parttable::{align_table, gpt_to_mbr, mbr_to_gpt, Table};

/// Sectors at the start of a disk that hold the MBR, the GPT header and its entry array.
const GPT_HEAD_SECTORS: u64 = 34;
/// Sectors at the end of a disk that hold the backup GPT entry array and header.
const GPT_TAIL_SECTORS: u64 = 33;

/// Zero `[start, start + len)` of `dev` (clipped to the device).
pub(crate) fn zero(dev: &mut dyn pm_core::BlockDevice, start: u64, len: u64) -> Result<()> {
    let end = start.saturating_add(len).min(dev.len());
    if start >= end {
        return Ok(());
    }
    dev.write_at(start, &vec![0u8; (end - start) as usize])
}

/// Zero the primary GPT area (LBA 0..34, or LBA 1..34 with `keep_mbr`) and the backup GPT
/// (last 33 sectors) of `dev`.
pub(crate) fn zero_table_areas(dev: &mut dyn pm_core::BlockDevice, keep_mbr: bool) -> Result<()> {
    let ss = (dev.sector_size() as u64).max(512);
    let len = dev.len();
    let from = if keep_mbr { ss } else { 0 };
    zero(dev, from, GPT_HEAD_SECTORS * ss - from)?;
    let tail = (GPT_TAIL_SECTORS * ss).min(len);
    zero(dev, len - tail, tail)
}

pub(crate) fn initialize(ctx: &mut Ctx, disk: u32, kind: TableKind) -> Result<()> {
    ctx.step("write an empty partition table");
    with_session(ctx.platform, disk, |s| {
        let dev = s.device();
        let t = Table::empty(kind, dev.len(), dev.sector_size());
        s.commit_table(&t)
    })?;
    ctx.rescan(disk)
}

/// Read the table, let `change` edit it, commit it; then re-read the layout.
fn edit_table(
    ctx: &mut Ctx,
    disk: u32,
    what: &str,
    change: impl FnOnce(&mut Table) -> Result<()>,
) -> Result<()> {
    ctx.step(what);
    with_session(ctx.platform, disk, |s| {
        let mut t = s.read_table()?;
        change(&mut t)?;
        s.commit_table(&t)
    })?;
    ctx.rescan(disk)
}

pub(crate) fn create(
    ctx: &mut Ctx,
    disk: u32,
    fs: Option<FsKind>,
    label: &str,
    letter: Option<char>,
) -> Result<()> {
    let p = ctx.new_partition(disk)?.clone();
    edit_table(ctx, disk, "write partition table", |t| {
        let e = entry_of(t.kind, &p);
        t.entries.push(e);
        Ok(())
    })?;
    if let Some(fs) = fs {
        format::format_new(ctx, disk, p.start, fs, label)?;
    }
    if let Some(c) = letter {
        format::assign_letter(ctx, disk, p.start, c)?;
    }
    Ok(())
}

pub(crate) fn delete(ctx: &mut Ctx, disk: u32, number: u32) -> Result<()> {
    edit_table(ctx, disk, "write partition table", |t| {
        entry_mut(t, disk, number)?;
        t.entries.retain(|e| e.number != number);
        Ok(())
    })
}

pub(crate) fn set_active(ctx: &mut Ctx, disk: u32, number: u32) -> Result<()> {
    edit_table(ctx, disk, "write partition table", |t| {
        if t.kind != TableKind::Mbr {
            return Err(Error::Invalid(format!("Disk {disk} is not an MBR disk on the device")));
        }
        entry_mut(t, disk, number)?;
        for e in t.entries.iter_mut().filter(|e| !e.logical) {
            e.attrs = (e.attrs & !1) | (e.number == number) as u64;
        }
        Ok(())
    })
}

pub(crate) fn set_type(ctx: &mut Ctx, disk: u32, number: u32, ptype: PartitionType) -> Result<()> {
    edit_table(ctx, disk, "write partition table", |t| {
        entry_mut(t, disk, number)?.ptype = ptype;
        Ok(())
    })
}

pub(crate) fn convert(ctx: &mut Ctx, disk: u32, to: TableKind) -> Result<()> {
    ctx.step("write converted partition table");
    with_session(ctx.platform, disk, |s| {
        let t = s.read_table()?;
        let converted = match (t.kind, to) {
            (TableKind::Mbr, TableKind::Gpt) => mbr_to_gpt(&t)?,
            (TableKind::Gpt, TableKind::Mbr) => gpt_to_mbr(&t)?,
            (from, to) => {
                return Err(Error::Invalid(format!(
                    "Disk {disk}: cannot convert {} to {}",
                    table_name(from),
                    table_name(to)
                )))
            }
        };
        s.commit_table(&converted)?;
        if to == TableKind::Mbr {
            // the primary and backup GPT would otherwise survive next to the new MBR; they
            // are cleared only once the MBR is written, so the disk always has a table
            zero_table_areas(s.device(), true)?;
            s.device().flush()?;
        }
        Ok(())
    })?;
    ctx.rescan(disk)
}

/// What happens to a filesystem whose partition shrinks from `old` to `new` bytes.
fn check_shrink(fs: Option<FsKind>, disk: u32, number: u32, old: u64, new: u64) -> Result<bool> {
    match fs {
        _ if new >= old => Ok(false),
        None => Ok(false),
        Some(FsKind::Ntfs | FsKind::ReFs) => Ok(true),
        Some(other) => Err(Error::Unsupported(format!(
            "partition {number} on Disk {disk}: resize of {} is not supported (aligning it would \
             shrink it by {} bytes)",
            other.display_name(),
            old - new
        ))),
    }
}

/// Align every partition to 1 MiB. Partitions only shrink (start up, end down), so each new
/// range lies inside its old one: NTFS/ReFS filesystems are shrunk first, then the data of
/// every moved partition is copied to its new start inside the session, then the table is
/// written.
pub(crate) fn align(ctx: &mut Ctx, disk: u32) -> Result<()> {
    let d = ctx.live_disk(disk)?;
    let target = align_table(&crate::model::table_from_disk(d), MIB);
    // (number, old start, old len, new start, new len)
    let mut changes = Vec::new();
    for e in &target.entries {
        let Some(p) = d.partitions.iter().find(|p| p.number == e.number) else {
            continue;
        };
        if (p.start, p.len) == (e.start, e.len) {
            continue;
        }
        if check_shrink(p.fs, disk, p.number, p.len, e.len)? {
            let vol = super::volume_of(ctx, disk, p)?;
            ctx.step(&format!("shrink the filesystem of partition {}", p.number));
            ctx.platform.shrink_volume(&vol, e.len)?;
        }
        changes.push((p.number, p.start, p.len, e.start, e.len));
    }
    if changes.is_empty() {
        ctx.note("nothing to align");
        return Ok(());
    }
    let platform = ctx.platform;
    with_session(platform, disk, |s| {
        let mut t = s.read_table()?;
        for &(n, old_start, old_len, ..) in &changes {
            let e = entry_mut(&mut t, disk, n)?;
            if (e.start, e.len) != (old_start, old_len) {
                return Err(Error::Invalid(format!(
                    "partition {n} on Disk {disk} changed on the device"
                )));
            }
        }
        for &(n, old_start, old_len, new_start, new_len) in &changes {
            let sink = ctx.step(&format!("move the data of partition {n}"));
            move_data(s.device(), old_start, new_start, new_len.min(old_len), &sink, ctx.cancel)?;
            let e = entry_mut(&mut t, disk, n)?;
            e.start = new_start;
            e.len = new_len;
        }
        ctx.step("write partition table");
        s.commit_table(&t)
    })?;
    ctx.rescan(disk)
}

/// Rewrite the MBR (or the protective MBR and both GPT copies) from the table read off the
/// disk. The existing boot code is kept as read; no boot code is embedded in this
/// application, so an empty boot code stays empty (use a clone/migrate boot fix or
/// `bootsect /nt60 <drive>: /mbr` to install one).
pub(crate) fn rebuild_mbr(ctx: &mut Ctx, disk: u32) -> Result<()> {
    let mut empty_bootcode = false;
    edit_table(ctx, disk, "rewrite partition table", |t| {
        if !matches!(t.kind, TableKind::Mbr | TableKind::Gpt) {
            return Err(Error::Invalid(format!("Disk {disk} has no partition table to rebuild")));
        }
        empty_bootcode = t.kind == TableKind::Mbr && t.bootcode.iter().all(|&b| b == 0);
        Ok(())
    })?;
    if empty_bootcode {
        ctx.note("the MBR has no boot code; the disk is not BIOS-bootable until one is installed");
    }
    Ok(())
}
