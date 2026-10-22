//! Disk enumeration from `diskutil` (OS-neutral: the `diskutil` calls go through a
//! [`DiskutilRunner`] and the raw devices through an opener closure, so fixtures drive it in
//! tests on any OS).
//!
//! `diskutil list -plist` gives the physical whole disks and their slices; `diskutil info
//! -plist` each disk and slice. `diskutil` does not report partition offsets, so the table is
//! read from `/dev/rdiskN` with [`pm_parttable::read_table`], and slices are matched to its
//! entries (GPT partition GUID, then number + size, then a size that is unique on both sides;
//! a slice that matches nothing gets no partition).
//!
//! Three layouts result:
//! * **exact**: the raw node opened and the table was read;
//! * **approximate**: the raw node cannot be opened (not root). Slices are laid end to end,
//!   the table is reported [`TableKind::Unknown`]. Nothing in this process can write such a
//!   disk (opening it fails the same way), and a queue planned from it and run as root fails
//!   the identity check against the exact layout. No filesystem probing happens at the
//!   approximate offsets;
//! * **unplaced**: the raw node opened but the table is unusable (damaged, Apple Partition
//!   Map): no partitions at all (table `Unknown`), and the slices are reported only as volumes
//!   without a disk or extent (`disk: None`, `len: 0`), which every same-disk / extent lookup
//!   refuses.

use super::diskutil::{
    bus_of, fs_of, parse_apfs_containers, parse_identifier, parse_info, parse_list, ptype_of, scheme_of,
    ApfsContainer, DeviceInfo, DiskutilRunner, ListedDisk, ListedPartition,
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
    /// One per slice with a device node (mounted or not), `id` = `/dev/diskNsM`. A slice that
    /// could not be placed on its disk has `disk: None` and `len: 0`.
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
struct Slice<'a> {
    number: u32,
    listed: ListedPartition,
    info: Option<DeviceInfo>,
    /// The APFS container stored on this slice (it carries the mount points).
    container: Option<&'a ApfsContainer>,
}

impl Slice<'_> {
    fn fs(&self) -> Option<FsKind> {
        match &self.info {
            Some(i) => fs_of(&i.filesystem_type, &i.filesystem_name, &i.content),
            None => fs_of("", "", &self.listed.content),
        }
    }
    fn mount_points(&self) -> Vec<String> {
        let own = self.info.as_ref().and_then(|i| i.mount_point.clone()).or_else(|| self.listed.mount_point.clone());
        match (own, self.container) {
            (Some(m), _) => vec![m],
            (None, Some(c)) => c.volumes.iter().filter_map(|v| v.mount_point.clone()).collect(),
            (None, None) => Vec::new(),
        }
    }
    fn label(&self) -> String {
        match &self.info {
            Some(i) if !i.volume_name.is_empty() => i.volume_name.clone(),
            _ => self.listed.volume_name.clone(),
        }
    }
    fn guid(&self) -> Option<Uuid> {
        self.listed
            .disk_uuid
            .as_deref()
            .or_else(|| self.info.as_ref().and_then(|i| i.disk_uuid.as_deref()))
            .and_then(|g| Uuid::parse_str(g).ok())
    }
    /// `(total, free)` of a mounted filesystem.
    fn usage(&self) -> Option<(u64, u64)> {
        let i = self.info.as_ref()?;
        let free = i.free_space?;
        let mounted = i.mount_point.is_some() || self.listed.mount_point.is_some();
        (mounted && i.total_size > 0).then_some((i.total_size, free))
    }
    /// Used bytes: the mounted filesystem's, or the sum of `CapacityInUse` of the volumes of
    /// the APFS container stored here alone (unknown when a volume does not say, or when the
    /// container spans several stores).
    fn used(&self) -> Option<u64> {
        if let Some((total, free)) = self.usage() {
            return Some(total.saturating_sub(free));
        }
        let c = self.container.filter(|c| c.stores.len() == 1 && !c.volumes.is_empty())?;
        c.volumes.iter().map(|v| v.capacity_in_use).sum()
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
                guid: s.guid(),
                name: s.info.as_ref().map(|i| i.media_name.clone()).unwrap_or_default(),
                ..Default::default()
            };
            pos = pos.saturating_add(s.listed.size);
            p
        })
        .collect();
    (0..slices.len()).map(Some).collect()
}

/// Index into `d.partitions` of each slice, in three passes: the GPT partition GUID (with the
/// same size), the same number and size, then a size shared by exactly one unmatched slice and
/// one unmatched partition. `None` when nothing matches unambiguously: a wrong match would
/// attach the volume (and so formatting) to another partition.
fn match_slices(d: &DiskInfo, slices: &[Slice]) -> Vec<Option<usize>> {
    let mut found: Vec<Option<usize>> = vec![None; slices.len()];
    let mut taken = vec![false; d.partitions.len()];
    let assign = |found: &mut Vec<Option<usize>>, taken: &mut Vec<bool>, si: usize, pi: usize| {
        found[si] = Some(pi);
        taken[pi] = true;
    };
    if d.table == TableKind::Gpt {
        for (si, s) in slices.iter().enumerate() {
            let Some(g) = s.guid() else { continue };
            let hit = d.partitions.iter().enumerate().find(|(i, p)| !taken[*i] && p.guid == Some(g));
            match hit {
                Some((pi, p)) if p.len == s.listed.size => assign(&mut found, &mut taken, si, pi),
                Some(_) => log::warn!("{}: partition GUID matches but the size differs", s.listed.identifier),
                None => {}
            }
        }
    }
    for (si, s) in slices.iter().enumerate() {
        if found[si].is_some() {
            continue;
        }
        let hit = d
            .partitions
            .iter()
            .enumerate()
            .find(|(i, p)| !taken[*i] && p.number == s.number && p.len == s.listed.size);
        if let Some((pi, _)) = hit {
            assign(&mut found, &mut taken, si, pi);
        }
    }
    for si in 0..slices.len() {
        if found[si].is_some() {
            continue;
        }
        let size = slices[si].listed.size;
        let same_slices = (0..slices.len()).filter(|&j| found[j].is_none() && slices[j].listed.size == size).count();
        let same_parts: Vec<usize> =
            (0..d.partitions.len()).filter(|&i| !taken[i] && d.partitions[i].len == size).collect();
        if same_slices == 1 && same_parts.len() == 1 {
            assign(&mut found, &mut taken, si, same_parts[0]);
        }
    }
    for (s, f) in slices.iter().zip(&found) {
        if f.is_none() {
            log::warn!("{}: no unambiguous match in the partition table read from {}", s.listed.identifier, d.path);
        }
    }
    found
}

/// Copy what `diskutil` knows about each slice onto its partition (filesystem, label, mount
/// points, used bytes, the volume id `/dev/diskNsM`) and return the slices as volumes;
/// unmatched slices become volumes without a disk or extent.
fn apply_slices(d: &mut DiskInfo, slices: &[Slice], matches: &[Option<usize>]) -> Vec<VolumeInfo> {
    let mut volumes = Vec::new();
    for (s, m) in slices.iter().zip(matches) {
        let id = VolumeId(format!("/dev/{}", s.listed.identifier));
        let (total, free) = s.usage().unwrap_or((0, 0));
        let mut v = VolumeInfo {
            id: id.clone(),
            disk: None,
            start: 0,
            len: 0,
            fs: s.fs(),
            label: s.label(),
            mount_points: s.mount_points(),
            total,
            free,
        };
        if let Some(p) = m.and_then(|i| d.partitions.get_mut(i)) {
            p.fs = v.fs;
            p.label = v.label.clone();
            p.mount_points = v.mount_points.clone();
            p.used = s.used();
            p.volume = Some(id);
            if p.guid.is_none() {
                p.guid = s.guid();
            }
            v.disk = Some(d.index);
            v.start = p.start;
            v.len = p.len;
        }
        volumes.push(v);
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
    containers: &[ApfsContainer],
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
            Some((n, Some(m))) if n == index => Some(Slice {
                number: m,
                info: info(runner, &p.identifier),
                container: containers.iter().find(|c| c.stores.contains(&p.identifier)),
                listed: p,
            }),
            _ => {
                log::warn!("{}: unexpected slice {} skipped", listed.identifier, p.identifier);
                None
            }
        })
        .collect();
    slices.sort_by_key(|s| s.number);

    let Some(mut dev) = open(&d.path) else {
        // not readable: approximate offsets, no probing at them
        let matches = approximate(&mut d, scheme, &slices);
        let volumes = apply_slices(&mut d, &slices, &matches);
        return Some((d, volumes));
    };
    let table = match read_table(dev.as_mut()) {
        // a table pm-parttable cannot see (e.g. an Apple Partition Map) while diskutil lists
        // slices is as unusable as a damaged one
        Ok(t) if !(t.kind == TableKind::None && !slices.is_empty()) => Some(t),
        Ok(_) => {
            log::warn!("{}: diskutil lists slices but no partition table was found", d.path);
            None
        }
        Err(e) => {
            log::warn!("{}: read_table: {e}", d.path);
            None
        }
    };
    let Some(t) = table else {
        // readable but no usable table: no partitions, slices only as unplaced volumes
        d.table = TableKind::Unknown;
        let volumes = apply_slices(&mut d, &slices, &vec![None; slices.len()]);
        return Some((d, volumes));
    };
    d = t.to_disk_info(d);
    let matches = match_slices(&d, &slices);
    let volumes = apply_slices(&mut d, &slices, &matches);
    refine_fat(&mut d, dev.as_mut());
    detect_filesystems(&mut d, dev.as_mut());
    Some((d, volumes))
}

/// Every physical whole disk in `diskutil list` order (synthesized APFS disks skipped), with
/// its slices merged in, and the slices as volumes. `open` reads the table from `/dev/rdiskN`
/// when it can; `system_disk` sets [`DiskFlags::system`].
pub fn enumerate(runner: &dyn DiskutilRunner, open: DeviceOpener<'_>, system_disk: Option<u32>) -> Result<Enumerated> {
    let list = runner.run(&["list", "-plist"])?;
    let listed = parse_list(&list)?;
    let containers = parse_apfs_containers(&list)?;
    let mut out = Enumerated::default();
    for l in listed {
        let Some((index, None)) = parse_identifier(&l.identifier) else { continue };
        if let Some((d, v)) = disk_info(runner, open, index, l, &containers, system_disk) {
            out.disks.push(d);
            out.volumes.extend(v);
        }
    }
    Ok(out)
}
