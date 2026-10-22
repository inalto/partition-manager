//! Disk enumeration from `diskutil` (OS-neutral: the `diskutil` calls go through a
//! [`DiskutilRunner`] and the raw devices through an opener closure, so fixtures drive it in
//! tests on any OS).
//!
//! `diskutil list -plist` gives the physical whole disks and their slices; `diskutil info
//! -plist` each disk and slice. `diskutil` does not report partition offsets, so the table is
//! read from `/dev/rdiskN` with [`pm_parttable::read_table`] when the disk is readable (root),
//! and slices are matched to its entries by number and size. Without read access the starts
//! are only approximate (slices laid end to end) and the disk reports
//! [`TableKind::Unknown`], which keeps every write planner away from it.

use super::diskutil::{
    bus_of, fs_of, parse_identifier, parse_info, parse_list, ptype_of, scheme_of, DeviceInfo, DiskutilRunner,
    ListedDisk, ListedPartition,
};
use crate::detect_filesystems;
use pm_core::{BlockDevice, DiskFlags, DiskInfo, FsKind, PartitionInfo, Result, TableKind, VolumeId, VolumeInfo};
use pm_parttable::read_table;
use uuid::Uuid;

/// Opens a raw device node (`/dev/rdiskN`) read-only; `None` when it cannot be opened
/// (typically: not root).
pub type DeviceOpener<'a> = &'a dyn Fn(&str) -> Option<Box<dyn BlockDevice>>;

/// Disks and volumes found by one enumeration pass.
#[derive(Clone, Debug, Default)]
pub struct Enumerated {
    pub disks: Vec<DiskInfo>,
    /// One per slice with a device node (mounted or not), `id` = `/dev/diskNsM`.
    pub volumes: Vec<VolumeInfo>,
}

/// `diskutil info -plist <ident>`, logged and dropped on failure.
fn info(runner: &dyn DiskutilRunner, ident: &str) -> Option<DeviceInfo> {
    match runner.run(&["info", "-plist", ident]).and_then(|b| parse_info(&b)) {
        Ok(i) => Some(i),
        Err(e) => {
            log::warn!("diskutil info {ident}: {e}");
            None
        }
    }
}

/// A slice with everything `diskutil` says about it.
struct Slice {
    number: u32,
    listed: ListedPartition,
    info: Option<DeviceInfo>,
}

impl Slice {
    fn fs(&self) -> Option<FsKind> {
        match &self.info {
            Some(i) => fs_of(&i.filesystem_type, &i.filesystem_name, &i.content),
            None => fs_of("", "", &self.listed.content),
        }
    }
    fn mount_point(&self) -> Option<String> {
        self.info.as_ref().and_then(|i| i.mount_point.clone()).or_else(|| self.listed.mount_point.clone())
    }
    fn label(&self) -> String {
        match &self.info {
            Some(i) if !i.volume_name.is_empty() => i.volume_name.clone(),
            _ => self.listed.volume_name.clone(),
        }
    }
    /// `(total, free)` of a mounted filesystem.
    fn usage(&self) -> Option<(u64, u64)> {
        let i = self.info.as_ref()?;
        let free = i.free_space?;
        (self.mount_point().is_some() && i.total_size > 0).then_some((i.total_size, free))
    }
}

/// First byte of the first partition assumed when the table cannot be read: Apple's layout
/// (LBA 40 on 512-byte disks, LBA 6 on 4Kn) for GPT, sector 1 otherwise. Never later than a
/// real table's first partition, so the approximated layout stays inside the disk.
fn approximate_first_start(scheme: TableKind, sector_size: u32) -> u64 {
    let ss = sector_size.max(512) as u64;
    match scheme {
        TableKind::Gpt if ss == 512 => 40 * ss,
        TableKind::Gpt => 6 * ss,
        _ => ss,
    }
}

/// Partitions laid end to end in slice order (offsets approximate), table `Unknown` (or `None`
/// for a disk without a partition scheme).
fn approximate(d: &mut DiskInfo, scheme: TableKind, slices: &[Slice]) -> Vec<Option<usize>> {
    d.table = if scheme == TableKind::None && slices.is_empty() { TableKind::None } else { TableKind::Unknown };
    let mut pos = approximate_first_start(scheme, d.sector_size);
    d.partitions = slices
        .iter()
        .map(|s| {
            let p = PartitionInfo {
                number: s.number,
                start: pos,
                len: s.listed.size,
                ptype: ptype_of(&s.listed.content, scheme),
                guid: s.listed.disk_uuid.as_deref().and_then(|g| Uuid::parse_str(g).ok()),
                name: s.info.as_ref().map(|i| i.media_name.clone()).unwrap_or_default(),
                ..Default::default()
            };
            pos = pos.saturating_add(s.listed.size);
            p
        })
        .collect();
    (0..slices.len()).map(Some).collect()
}

/// Index into `d.partitions` of each slice: same number and size, else the first unmatched
/// partition of the same size. `None` for a slice the table does not have.
fn match_slices(d: &DiskInfo, slices: &[Slice]) -> Vec<Option<usize>> {
    let mut taken = vec![false; d.partitions.len()];
    slices
        .iter()
        .map(|s| {
            let size = s.listed.size;
            let found = d
                .partitions
                .iter()
                .enumerate()
                .find(|(i, p)| !taken[*i] && p.number == s.number && p.len == size)
                .or_else(|| d.partitions.iter().enumerate().find(|(i, p)| !taken[*i] && p.len == size))
                .map(|(i, _)| i);
            match found {
                Some(i) => taken[i] = true,
                None => log::warn!("{}: not in the partition table read from {}", s.listed.identifier, d.path),
            }
            found
        })
        .collect()
}

/// Copy what `diskutil` knows about each slice onto its partition: filesystem, label, mount
/// point, used bytes and the volume id `/dev/diskNsM`.
fn apply_slices(d: &mut DiskInfo, slices: &[Slice], matches: &[Option<usize>]) -> Vec<VolumeInfo> {
    let mut volumes = Vec::new();
    for (s, m) in slices.iter().zip(matches) {
        let Some(p) = m.and_then(|i| d.partitions.get_mut(i)) else { continue };
        let id = VolumeId(format!("/dev/{}", s.listed.identifier));
        p.fs = s.fs();
        p.label = s.label();
        p.mount_points = s.mount_point().into_iter().collect();
        let usage = s.usage();
        p.used = usage.map(|(total, free)| total.saturating_sub(free));
        p.volume = Some(id.clone());
        if p.guid.is_none() {
            p.guid = s.listed.disk_uuid.as_deref().and_then(|g| Uuid::parse_str(g).ok());
        }
        let (total, free) = usage.unwrap_or((0, 0));
        volumes.push(VolumeInfo {
            id,
            disk: Some(d.index),
            start: p.start,
            len: p.len,
            fs: p.fs,
            label: p.label.clone(),
            mount_points: p.mount_points.clone(),
            total,
            free,
        });
    }
    volumes
}

/// `msdos` says FAT but not always which width: take it from the boot sector when readable.
fn refine_fat(d: &mut DiskInfo, dev: &mut dyn BlockDevice) {
    for p in d.partitions.iter_mut().filter(|p| p.fs == Some(FsKind::Fat32)) {
        if let Some(probe) = pm_fs::detect(dev, p.start) {
            if matches!(probe.kind, FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32) {
                p.fs = Some(probe.kind);
            }
        }
    }
}

fn disk_info(
    runner: &dyn DiskutilRunner,
    open: DeviceOpener<'_>,
    index: u32,
    listed: ListedDisk,
    system_disk: Option<u32>,
) -> Option<(DiskInfo, Vec<VolumeInfo>)> {
    let di = info(runner, &listed.identifier)?;
    let sector = di.device_block_size;
    let mut d = DiskInfo {
        index,
        path: format!("/dev/rdisk{index}"),
        model: if di.media_name.is_empty() { di.io_registry_entry_name.clone() } else { di.media_name.clone() },
        bus: bus_of(&di.bus_protocol),
        size: if di.size > 0 { di.size } else { listed.size },
        sector_size: sector,
        physical_sector_size: sector,
        flags: DiskFlags {
            removable: di.removable,
            read_only: !di.writable,
            system: system_disk == Some(index),
            trim: di.solid_state,
            offline: false,
        },
        ..Default::default()
    };
    let scheme = scheme_of(&listed.content);
    let mut slices: Vec<Slice> = listed
        .partitions
        .into_iter()
        .filter_map(|p| match parse_identifier(&p.identifier) {
            Some((n, Some(m))) if n == index => Some(Slice { number: m, info: info(runner, &p.identifier), listed: p }),
            _ => {
                log::warn!("{}: unexpected slice {} skipped", listed.identifier, p.identifier);
                None
            }
        })
        .collect();
    slices.sort_by_key(|s| s.number);

    let mut dev = open(&d.path);
    let table = dev.as_mut().and_then(|dev| match read_table(dev.as_mut()) {
        Ok(t) => Some(t),
        Err(e) => {
            log::warn!("{}: read_table: {e}", d.path);
            None
        }
    });
    let matches = match table {
        // a table pm-parttable cannot see (e.g. an Apple Partition Map) while diskutil lists
        // slices: fall back to the approximate layout
        Some(t) if !(t.kind == TableKind::None && !slices.is_empty()) => {
            d = t.to_disk_info(d);
            match_slices(&d, &slices)
        }
        _ => approximate(&mut d, scheme, &slices),
    };
    let volumes = apply_slices(&mut d, &slices, &matches);
    if let Some(dev) = dev.as_mut() {
        refine_fat(&mut d, dev.as_mut());
        detect_filesystems(&mut d, dev.as_mut());
    }
    Some((d, volumes))
}

/// Every physical whole disk in `diskutil list` order (synthesized APFS disks skipped), with
/// its slices merged in, and the slices as volumes. `open` reads the table from `/dev/rdiskN`
/// when it can; `system_disk` sets [`DiskFlags::system`].
pub fn enumerate(runner: &dyn DiskutilRunner, open: DeviceOpener<'_>, system_disk: Option<u32>) -> Result<Enumerated> {
    let listed = parse_list(&runner.run(&["list", "-plist"])?)?;
    let mut out = Enumerated::default();
    for l in listed {
        let Some((index, None)) = parse_identifier(&l.identifier) else { continue };
        if let Some((d, v)) = disk_info(runner, open, index, l, system_disk) {
            out.disks.push(d);
            out.volumes.extend(v);
        }
    }
    Ok(out)
}
