//! Physical disk enumeration (`\\.\PhysicalDriveN`).
//!
//! Disk metadata comes from query IOCTLs that work on a handle opened with no access rights
//! (so enumeration works without elevation); the partition list comes from
//! `IOCTL_DISK_GET_DRIVE_LAYOUT_EX`, which is authoritative for MBR and GPT alike.
//! `pm_parttable::read_table` (needs read access, i.e. elevation) only enriches the result
//! (partition numbers in table-slot order) or serves as a fallback when the layout IOCTL fails.

use super::device::WinDevice;
use super::ioctl::{self, Handle, GENERIC_READ};
use super::layout::{
    parse_alignment_descriptor, parse_device_descriptor, parse_drive_layout, parse_trim_descriptor,
    ParsedLayout,
};
use crate::{detect_filesystems, merge_volumes};
use crate::traits::OpenMode;
use pm_core::{BlockDevice, DiskInfo, Error, TableKind, VolumeInfo};
use pm_parttable::{is_protective_mbr, read_table};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
use windows_sys::Win32::System::Ioctl::{
    StorageAccessAlignmentProperty, StorageDeviceProperty, StorageDeviceTrimProperty,
    DISK_ATTRIBUTE_OFFLINE, DISK_ATTRIBUTE_READ_ONLY,
};

/// Highest `PhysicalDriveN` probed.
const MAX_DISK_INDEX: u32 = 63;
/// Stop probing after this many consecutive missing drives.
const MAX_CONSECUTIVE_MISSING: u32 = 8;

pub fn disk_path(index: u32) -> String {
    format!(r"\\.\PhysicalDrive{index}")
}

/// Open a disk for querying: read access when allowed (elevated), else query-only.
/// Returns the handle and whether it can read sectors.
fn open_for_query(path: &str) -> Result<(Handle, bool), u32> {
    match ioctl::open(path, GENERIC_READ) {
        Ok(h) => Ok((h, true)),
        Err(ERROR_ACCESS_DENIED) => ioctl::open(path, 0).map(|h| (h, false)),
        Err(e) => Err(e),
    }
}

/// Whether disk `index` reports removable media (storage device descriptor); `None` when it
/// cannot be queried.
pub fn is_removable(index: u32) -> Option<bool> {
    let (h, _) = open_for_query(&disk_path(index)).ok()?;
    let b = ioctl::query_property(&h, StorageDeviceProperty).ok()?;
    parse_device_descriptor(&b).ok().map(|d| d.removable)
}

/// Layout of disk `index` alone (no volume merge, no filesystem probing); partition numbers
/// follow the on-disk table when it is readable.
pub fn disk_info(index: u32) -> Option<DiskInfo> {
    let path = disk_path(index);
    let (handle, readable) = open_for_query(&path).ok()?;
    query_disk(index, &path, &handle, readable).ok()
}

/// Every disk with partitions, merged with `volumes` (by disk and start offset).
pub fn enumerate_disks(volumes: &[VolumeInfo], system_disk: Option<u32>) -> Vec<DiskInfo> {
    let mut out = Vec::new();
    let mut missing = 0;
    for index in 0..=MAX_DISK_INDEX {
        let path = disk_path(index);
        let (handle, readable) = match open_for_query(&path) {
            Ok(h) => {
                missing = 0;
                h
            }
            Err(ERROR_FILE_NOT_FOUND) => {
                missing += 1;
                if missing >= MAX_CONSECUTIVE_MISSING {
                    break;
                }
                continue;
            }
            Err(code) => {
                log::warn!("skipping {path}: open failed ({})", super::friendly_error(code));
                continue;
            }
        };
        match query_disk(index, &path, &handle, readable) {
            Ok(mut d) => {
                d.flags.system = system_disk == Some(index);
                merge_volumes(&mut d, volumes);
                // partitions without a mounted volume: identify the filesystem from its
                // signature (needs read access; a query-only handle skips this silently)
                if readable {
                    if let Some(mut dev) = open_disk_device(&d.path) {
                        detect_filesystems(&mut d, &mut dev);
                    }
                }
                out.push(d);
            }
            Err(code) => log::warn!("skipping {path}: query failed ({})", super::friendly_error(code)),
        }
    }
    out
}

/// Fill a [`DiskInfo`] from the query IOCTLs. Fails only if the geometry query fails.
fn query_disk(index: u32, path: &str, h: &Handle, readable: bool) -> Result<DiskInfo, u32> {
    let (bytes_per_sector, geometry_size) = ioctl::geometry(h)?;
    let size = ioctl::length_info(h).unwrap_or(geometry_size);
    let mut d = DiskInfo {
        index,
        path: path.to_string(),
        size,
        sector_size: bytes_per_sector,
        physical_sector_size: bytes_per_sector,
        ..Default::default()
    };

    if let Some(desc) = ioctl::query_property(h, StorageDeviceProperty)
        .ok()
        .and_then(|b| parse_device_descriptor(&b).ok())
    {
        d.model = desc.model;
        d.serial = desc.serial;
        d.bus = desc.bus;
        d.flags.removable = desc.removable;
    }
    if let Some((logical, physical)) = ioctl::query_property(h, StorageAccessAlignmentProperty)
        .ok()
        .and_then(|b| parse_alignment_descriptor(&b))
    {
        d.sector_size = logical;
        d.physical_sector_size = physical;
    }
    d.flags.trim = ioctl::query_property(h, StorageDeviceTrimProperty)
        .ok()
        .and_then(|b| parse_trim_descriptor(&b))
        .unwrap_or(false);
    if let Ok(attrs) = ioctl::disk_attributes(h) {
        d.flags.offline = attrs & DISK_ATTRIBUTE_OFFLINE != 0;
        d.flags.read_only = attrs & DISK_ATTRIBUTE_READ_ONLY != 0;
    }

    let layout = ioctl::drive_layout(h)
        .map_err(|c| format!("layout IOCTL failed ({})", super::friendly_error(c)))
        .and_then(|b| parse_drive_layout(&b).map_err(|e| e.to_string()));
    match layout {
        Ok(ParsedLayout {
            table,
            disk_id,
            partitions,
        }) => {
            d.table = table;
            d.disk_id = disk_id;
            d.partitions = partitions;
            if readable {
                enrich_from_table(&mut d);
            }
        }
        Err(e) => {
            log::warn!("{path}: {e}; falling back to reading the table directly");
            if readable {
                table_fallback(&mut d);
            }
        }
    }
    Ok(d)
}

fn open_disk_device(path: &str) -> Option<WinDevice> {
    WinDevice::open(path, OpenMode::ReadOnly)
        .map_err(|e| log::debug!("{path}: {e}"))
        .ok()
}

/// Align partition numbers with `pm_parttable` (table-slot order) when the on-disk table can be
/// parsed and agrees with the layout. Errors (e.g. GPT before Task 4) leave `d` unchanged.
fn enrich_from_table(d: &mut DiskInfo) {
    let Some(mut dev) = open_disk_device(&d.path) else { return };
    match read_table(&mut dev) {
        Ok(t) if t.kind == d.table => {
            for p in &mut d.partitions {
                if let Some(e) = t.entries.iter().find(|e| e.start == p.start && e.len == p.len) {
                    p.number = e.number;
                }
            }
        }
        Ok(_) => {}
        Err(e) => log::debug!("{}: read_table: {e}", d.path),
    }
}

/// Without a drive layout, take table and partitions from `read_table`; for GPT (not yet
/// parsed by pm-parttable) at least report the table kind from the protective MBR.
fn table_fallback(d: &mut DiskInfo) {
    let Some(mut dev) = open_disk_device(&d.path) else { return };
    match read_table(&mut dev) {
        Ok(t) => {
            let base = std::mem::take(d);
            *d = t.to_disk_info(base);
        }
        Err(Error::Unsupported(_)) => {
            let mut sector = vec![0u8; dev.sector_size() as usize];
            if dev.read_at(0, &mut sector).is_ok() && is_protective_mbr(&sector) {
                d.table = TableKind::Gpt;
            }
        }
        Err(e) => log::warn!("{}: read_table: {e}", d.path),
    }
}
