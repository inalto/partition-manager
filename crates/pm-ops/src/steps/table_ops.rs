//! Partition-table operations: initialize, create, delete, set active, set type, convert,
//! align and rebuild. Each reads the table from the device inside a write session, changes it
//! with the planner's numbers and commits it.

use crate::operation::table_name;
use super::resize::{is_container_part, unknown_fs_shrink};
use super::{ensure_no_filesystem, entry_mut, entry_of, format, move_data, with_session, Ctx};
use pm_core::units::MIB;
use pm_core::{BlockDevice, DiskInfo, Error, FsKind, PartitionInfo, PartitionType, Result, TableKind};
use pm_parttable::{align_table, gpt_to_mbr, mbr_to_gpt, Table};

/// Largest GPT entry array believed from a header (a corrupt header must not make us zero
/// half the disk): 16 MiB.
const GPT_MAX_ARRAY: u64 = 16 * MIB;

/// Zero `[start, start + len)` of `dev` (clipped to the device).
pub(crate) fn zero(dev: &mut dyn BlockDevice, start: u64, len: u64) -> Result<()> {
    let end = start.saturating_add(len).min(dev.len());
    if start >= end {
        return Ok(());
    }
    dev.write_at(start, &vec![0u8; (end - start) as usize])
}

/// A GPT header: `(its own LBA, the other header's LBA, entry array start LBA, array bytes)`.
fn gpt_header(dev: &mut dyn BlockDevice, lba: u64, ss: u64) -> Option<(u64, u64, u64, u64)> {
    let mut b = vec![0u8; ss as usize];
    dev.read_at(lba.checked_mul(ss)?, &mut b).ok()?;
    if b.get(..8)? != b"EFI PART" {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64;
    let bytes = u32_at(80).checked_mul(u32_at(84))?;
    (u64_at(24) == lba && bytes <= GPT_MAX_ARRAY).then(|| (lba, u64_at(32), u64_at(72), bytes))
}

/// The byte ranges `dev`'s GPT structures occupy: header and entry array of the primary GPT
/// (from LBA 1, or LBA 0 too unless `keep_mbr`) and of the backup GPT, as their headers
/// describe them. Where a header cannot be read, the areas a GPT written for this disk's
/// sector size would use ([`Table::empty`]'s usable bounds) are taken instead.
fn gpt_areas(dev: &mut dyn BlockDevice, keep_mbr: bool) -> Vec<(u64, u64)> {
    let ss = (dev.sector_size() as u64).max(512);
    let len = dev.len();
    let last = (len / ss).saturating_sub(1);
    let default = Table::empty(TableKind::Gpt, len, ss as u32);
    let mut areas = Vec::new();
    if !keep_mbr {
        areas.push((0, ss));
    }
    let array = |areas: &mut Vec<(u64, u64)>, h: (u64, u64, u64, u64)| {
        areas.push((h.0 * ss, (h.0 + 1) * ss));
        let start = h.2.saturating_mul(ss);
        areas.push((start, start.saturating_add(h.3.div_ceil(ss) * ss)));
    };
    let primary = gpt_header(dev, 1, ss);
    match primary {
        Some(h) => array(&mut areas, h),
        None => areas.push((ss, default.gpt_first_usable)),
    }
    let mut backups = vec![last];
    if let Some((_, alt, _, _)) = primary {
        if alt != last && alt > 1 && alt < last {
            backups.push(alt);
        }
    }
    let mut found = false;
    for lba in backups {
        if let Some(h) = gpt_header(dev, lba, ss) {
            array(&mut areas, h);
            found = true;
        }
    }
    if !found {
        areas.push((default.gpt_last_usable, len));
    }
    areas
}

/// Zero the GPT structures of `dev` ([`gpt_areas`]: primary and backup header and entry
/// array, LBA 0 too unless `keep_mbr`) without touching any byte of a partition of `keep`
/// (the table now on, or about to be on, the disk).
pub(crate) fn zero_table_areas(dev: &mut dyn BlockDevice, keep_mbr: bool, keep: &Table) -> Result<()> {
    let len = dev.len();
    let protected: Vec<(u64, u64)> = keep.entries.iter().map(|e| (e.start, e.end())).collect();
    for (start, end) in gpt_areas(dev, keep_mbr) {
        let mut pieces = vec![(start.min(len), end.min(len))];
        for &(ps, pe) in &protected {
            pieces = pieces
                .into_iter()
                .flat_map(|(s, e)| {
                    if pe <= s || ps >= e {
                        vec![(s, e)]
                    } else {
                        vec![(s, ps.max(s)), (pe.min(e), e)]
                    }
                })
                .filter(|(s, e)| s < e)
                .collect();
        }
        for (s, e) in pieces {
            zero(dev, s, e - s)?;
        }
    }
    Ok(())
}

pub(crate) fn initialize(ctx: &mut Ctx, disk: u32, kind: TableKind) -> Result<()> {
    ctx.step("write an empty partition table");
    with_session(ctx.platform, disk, |s| {
        let dev = s.device();
        let t = Table::empty(kind, dev.len(), dev.sector_size());
        s.commit_table(&t)?;
        if kind == TableKind::Mbr {
            // a former GPT would otherwise survive behind the new MBR
            zero_table_areas(s.device(), true, &t)?;
            s.device().flush()?;
        }
        Ok(())
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
            zero_table_areas(s.device(), true, &converted)?;
            s.device().flush()?;
        }
        Ok(())
    })?;
    ctx.rescan(disk)
}

/// What happens to the filesystem of `p` (on `disk`) when its partition shrinks to `new`
/// bytes: `Ok(true)` when NTFS must be shrunk through the platform first, `Ok(false)` when
/// nothing has to follow (an extended container, or — with `verified_later`, for the
/// executor, which checks the device inside the session — an unknown filesystem),
/// `Unsupported` otherwise (only NTFS can be shrunk online; ReFS cannot). An unknown
/// filesystem is refused without `verified_later`.
fn check_shrink(p: &PartitionInfo, disk: u32, new: u64, verified_later: bool) -> Result<bool> {
    let (number, old) = (p.number, p.len);
    match p.fs {
        _ if new >= old => Ok(false),
        None if is_container_part(p) || verified_later => Ok(false),
        None => Err(unknown_fs_shrink(disk, number)),
        Some(FsKind::Ntfs) => Ok(true),
        Some(other) => Err(Error::Unsupported(format!(
            "partition {number} on Disk {disk}: resize of {} is not supported (aligning it would \
             shrink it by {} bytes; only NTFS volumes can be shrunk online)",
            other.display_name(),
            old - new
        ))),
    }
}

/// One partition that alignment changes.
pub(crate) struct AlignChange {
    pub number: u32,
    pub old: (u64, u64),
    pub new: (u64, u64),
    /// Its NTFS filesystem must be shrunk first.
    pub shrink_fs: bool,
    /// Only the table follows the shrink (no recognised filesystem): the executor verifies
    /// inside the session that the partition head carries no filesystem signature.
    pub verify_raw: bool,
}

/// What aligning `d` to 1 MiB changes, refusing (without side effects) partitions whose
/// filesystem cannot follow a shrink. Shared by the executor (`verified_later`) and
/// [`crate::precheck`].
pub(crate) fn align_changes(d: &DiskInfo, verified_later: bool) -> Result<Vec<AlignChange>> {
    let target = align_table(&crate::model::table_from_disk(d), MIB);
    let mut out = Vec::new();
    for e in &target.entries {
        let Some(p) = d.partitions.iter().find(|p| p.number == e.number) else {
            continue;
        };
        if (p.start, p.len) == (e.start, e.len) {
            continue;
        }
        let shrink_fs = check_shrink(p, d.index, e.len, verified_later)?;
        let verify_raw = !shrink_fs && e.len < p.len && p.fs.is_none() && !is_container_part(p);
        out.push(AlignChange {
            number: p.number,
            old: (p.start, p.len),
            new: (e.start, e.len),
            shrink_fs,
            verify_raw,
        });
    }
    Ok(out)
}

/// Align every partition to 1 MiB. Partitions only shrink (start up, end down), so each new
/// range lies inside its old one.
///
/// Every decision is taken first (nothing is written if a partition cannot be aligned), then
/// NTFS filesystems that shrink are shrunk through the platform, then, inside one
/// session, each partition in turn has its data copied to its new start and **the table is
/// committed right after that partition** — so the table on disk always matches the data, even
/// when a later move fails or the run is cancelled. Cancellation is honoured between
/// partitions only: a single move is not cancellable once started.
pub(crate) fn align(ctx: &mut Ctx, disk: u32) -> Result<()> {
    let d = ctx.live_disk(disk)?;
    let changes = align_changes(d, true)?;
    if changes.is_empty() {
        ctx.note("nothing to align");
        return Ok(());
    }
    for c in changes.iter().filter(|c| c.shrink_fs) {
        let p = ctx.live_partition(disk, c.number)?;
        let vol = super::volume_of(ctx, disk, p)?;
        ctx.step(&format!("shrink the filesystem of partition {}", c.number));
        ctx.platform.shrink_volume(&vol, c.new.1)?;
    }
    let platform = ctx.platform;
    with_session(platform, disk, |s| {
        let mut t = s.read_table()?;
        for c in &changes {
            let e = entry_mut(&mut t, disk, c.number)?;
            if (e.start, e.len) != c.old {
                return Err(Error::Invalid(format!(
                    "partition {} on Disk {disk} changed on the device",
                    c.number
                )));
            }
        }
        // every table-only trim is verified before the first byte moves
        for c in changes.iter().filter(|c| c.verify_raw) {
            ensure_no_filesystem(s.device(), c.old.0, disk, c.number)?;
        }
        for c in &changes {
            let ((old_start, old_len), (new_start, new_len)) = (c.old, c.new);
            ctx.cancel.check()?;
            let sink = ctx.step(&format!("move the data of partition {}", c.number));
            move_data(s.device(), old_start, new_start, new_len.min(old_len), &sink, ctx.cancel)?;
            ctx.step(&format!("write partition table (partition {})", c.number));
            let e = entry_mut(&mut t, disk, c.number)?;
            e.start = new_start;
            e.len = new_len;
            s.commit_table(&t)?;
        }
        Ok(())
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
