//! Disk clone and OS migration, with the optional boot repair of the copy.

use super::clone::{copy_partition, grow_after_clone, prepare_copies};
use super::table_ops::zero_table_areas;
use super::{locate_volume, with_session, Ctx};
use crate::operation::{BootFix, CloneMode};
use crate::sizing::{plan_disk_clone, LayoutSpec, TargetLayout};
use pm_core::{DiskInfo, Error, PartitionInfo, Result, TableKind, VolumeId};
use pm_parttable::{Entry, Table};
use pm_platform::{ExternalTool, MountPoint, OpenMode};

/// Clone `src_disk` onto `dst_disk` with the layout [`plan_disk_clone`] computes (the same
/// call the planner made). Inside one session on the destination: an empty table of the
/// target's kind is committed first (a half-done clone is never mistaken for a good disk),
/// every mapped partition is copied, then the final table — with fresh disk and partition
/// ids, since Windows takes a disk with a duplicate signature offline, unless `spec.keep_ids`
/// keeps the source's (logged as a warning) — is committed. The boot fix runs afterwards;
/// its failure is logged and does not fail the operation.
pub(crate) fn clone_disk(
    ctx: &mut Ctx,
    src_disk: u32,
    dst_disk: u32,
    spec: LayoutSpec,
    mode: CloneMode,
    boot_fix: BootFix,
) -> Result<()> {
    let src = ctx.live_disk(src_disk)?;
    let dst = ctx.live_disk(dst_disk)?;
    let used_of = |p: &PartitionInfo| match mode {
        CloneMode::Smart => p.used.unwrap_or(p.len),
        CloneMode::Sector => p.len,
    };
    let probe = ctx.live.windows_probe();
    let layout = plan_disk_clone(src, dst.size, dst.sector_size, spec, &used_of, Some(&probe))?;
    let mut table = layout.table.clone();
    if spec.keep_ids {
        ctx.note(&format!(
            "warning: the copy keeps the disk and partition ids of Disk {src_disk}; detach one \
             of the two disks before Windows starts, which takes a disk with a duplicate id \
             offline"
        ));
    } else {
        table.regenerate_ids();
    }
    // keep the source's MBR boot code (BIOS boot) rather than the planner's zeroed one
    if let Ok(src_table) = ctx
        .platform
        .open_disk(src_disk, OpenMode::ReadOnly)
        .and_then(|mut d| pm_parttable::read_table(&mut *d))
    {
        if src_table.bootcode.len() == table.bootcode.len() {
            table.bootcode = src_table.bootcode;
        }
    }
    let copies = mapped(src, &layout)?;
    // every fit / shrink / refuse decision happens here, before the target is touched
    let items: Vec<_> = copies.iter().map(|(sp, e)| (src_disk, *sp, e.len)).collect();
    let plans = prepare_copies(ctx, &items, mode)?;

    let platform = ctx.platform;
    with_session(platform, dst_disk, |s| {
        let (len, ss) = {
            let dev = s.device();
            (dev.len(), dev.sector_size())
        };
        ctx.step("clear the partition table");
        s.commit_table(&Table::empty(table.kind, len, ss))?;
        if table.kind == TableKind::Mbr {
            // a former GPT would otherwise survive behind the new MBR; cleared before the
            // copies, which may legitimately cover the last sectors of the disk
            zero_table_areas(s.device(), true, &table)?;
        }
        for ((sp, e), plan) in copies.iter().zip(&plans) {
            let sink = ctx.step(&format!("copy partition {} to partition {}", sp.number, e.number));
            copy_partition(ctx, s, plan, e.start, &sink)?;
        }
        ctx.step("write partition table");
        s.commit_table(&table)
    })?;
    ctx.rescan(dst_disk)?;
    for (sp, e) in &copies {
        grow_after_clone(ctx, dst_disk, sp, (e.start, e.len));
    }
    if boot_fix != BootFix::None {
        ctx.step("repair boot files");
        if let Err(e) = fix_boot(ctx, boot_fix, src, &layout, &table, dst_disk) {
            let msg = super::message(e);
            ctx.note(&format!("boot fix failed: {msg}"));
        }
    }
    Ok(())
}

/// `(source partition, target entry)` for every partition the layout copies.
fn mapped<'a>(src: &'a DiskInfo, layout: &'a TargetLayout) -> Result<Vec<(&'a PartitionInfo, &'a Entry)>> {
    layout
        .map
        .iter()
        .map(|&(sn, dn)| {
            let sp = src.partitions.iter().find(|p| p.number == sn);
            let e = layout.table.entries.iter().find(|e| e.number == dn);
            match (sp, e) {
                (Some(sp), Some(e)) => Ok((sp, e)),
                _ => Err(Error::Invalid(format!(
                    "clone layout maps partition {sn} to {dn}, which does not exist"
                ))),
            }
        })
        .collect()
}

/// Make the copy bootable: give the cloned Windows partition (W) and its system partition
/// (S: the EFI partition for UEFI, the active partition for BIOS; W itself when there is
/// none) temporary drive letters, run `bcdboot W:\Windows /s S: /f <firmware>` and, for BIOS,
/// `bootsect /nt60 W: /mbr`, then remove the temporary letters (always).
fn fix_boot(
    ctx: &mut Ctx,
    fix: BootFix,
    src: &DiskInfo,
    layout: &TargetLayout,
    table: &Table,
    dst_disk: u32,
) -> Result<()> {
    let win_src = ctx
        .live
        .windows_partition_of(src.index)
        .ok_or_else(|| Error::Invalid(format!("Disk {} has no Windows partition", src.index)))?;
    let win_dst = layout
        .map
        .iter()
        .find(|(s, _)| *s == win_src)
        .map(|&(_, d)| d)
        .ok_or_else(|| Error::Invalid("the Windows partition was not copied".into()))?;
    let start_of = |n: u32| table.entries.iter().find(|e| e.number == n).map(|e| e.start);
    let win_start = start_of(win_dst)
        .ok_or_else(|| Error::Invalid("the Windows partition is missing on the target".into()))?;
    let efi = table.entries.iter().find(|e| e.ptype.is_efi()).map(|e| e.start);
    let active = table
        .entries
        .iter()
        .find(|e| table.kind == TableKind::Mbr && !e.logical && e.attrs & 1 != 0)
        .map(|e| e.start);
    let (firmware, sys_start) = match fix {
        BootFix::Uefi => (
            "UEFI",
            efi.ok_or_else(|| Error::Invalid("the target has no EFI system partition".into()))?,
        ),
        BootFix::Bios => ("BIOS", active.unwrap_or(win_start)),
        _ => ("ALL", efi.or(active).unwrap_or(win_start)),
    };

    let mut letters = ctx.platform.free_letters().into_iter();
    let mut mounted: Vec<(VolumeId, MountPoint)> = Vec::new();
    let r = (|| -> Result<()> {
        let mut assign = |start: u64| -> Result<char> {
            let vol = locate_volume(ctx.platform, dst_disk, start, ctx.cancel)?;
            let c = letters
                .next()
                .ok_or_else(|| Error::Busy("no free drive letter for the boot fix".into()))?;
            ctx.platform.mount(&vol, &MountPoint::Letter(c))?;
            mounted.push((vol, MountPoint::Letter(c)));
            Ok(c)
        };
        let w = assign(win_start)?;
        let s = if sys_start == win_start { w } else { assign(sys_start)? };
        let bcdboot: [String; 5] = [
            format!("{w}:\\Windows"),
            "/s".into(),
            format!("{s}:"),
            "/f".into(),
            firmware.into(),
        ];
        ctx.platform.run_tool(ExternalTool::BcdBoot, &bcdboot)?;
        if matches!(fix, BootFix::Bios | BootFix::All) {
            ctx.platform
                .run_tool(ExternalTool::BootSect, &["/nt60".into(), format!("{w}:"), "/mbr".into()])?;
        }
        Ok(())
    })();
    let mut cleanup = Ok(());
    for (vol, at) in mounted.iter().rev() {
        if let Err(e) = ctx.platform.unmount(vol, at) {
            cleanup = Err(e);
        }
    }
    r?;
    cleanup
}
