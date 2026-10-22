//! Disk enumeration from sysfs (`/sys/block`).
//!
//! [`parse_block_dir`] is a pure function over a sysfs-shaped directory tree, so it is tested
//! with fixture trees. Per disk `<root>/<dev>/`: `size` (always in 512-byte units, whatever the
//! logical sector size), `ro`, `removable`, `queue/logical_block_size`,
//! `queue/physical_block_size`, `queue/discard_max_bytes`, `device/{vendor,model,name,serial}`
//! and the partition children `<dev>/<part>/{partition,start,size,ro}` (`start`/`size` again in
//! 512-byte units). Partitions carry their node as `volume` (`/dev/sda1`).

use pm_core::{DiskBus, DiskInfo, PartFlags, PartitionInfo, VolumeId};
use std::path::{Path, PathBuf};

/// sysfs reports partition and disk sizes in these units, regardless of the sector size.
pub const SYSFS_SECTOR: u64 = 512;

fn read_trimmed(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_u64(p: &Path) -> Option<u64> {
    read_trimmed(p)?.parse().ok()
}

fn read_flag(p: &Path) -> bool {
    read_u64(p).is_some_and(|v| v != 0)
}

/// Whether `/sys/block/<name>` is a disk this platform manages. RAM disks, zram, device-mapper
/// and md arrays (out of scope), optical drives, eMMC boot/RPMB areas, NVMe multipath
/// controller paths and unattached loop devices are skipped.
pub fn is_managed(name: &str, dir: &Path) -> bool {
    const SKIP_PREFIXES: [&str; 8] = ["ram", "zram", "dm-", "md", "sr", "fd", "nbd", "zd"];
    if SKIP_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return false;
    }
    if name.starts_with("mmcblk") && (name.contains("boot") || name.contains("rpmb")) {
        return false;
    }
    if let Some(rest) = name.strip_prefix("nvme") {
        // nvme0c0n1: a hidden per-controller path of a multipath namespace
        if rest.contains('c') {
            return false;
        }
    }
    if read_flag(&dir.join("hidden")) {
        return false;
    }
    if name.starts_with("loop") {
        return dir.join("loop/backing_file").exists();
    }
    true
}

/// Bus from the resolved sysfs device path of a disk (and its name for devices without one).
pub fn bus_from_path(device_path: &str, name: &str) -> DiskBus {
    let p = device_path.to_ascii_lowercase();
    if p.contains("/nvme/") || name.starts_with("nvme") {
        DiskBus::Nvme
    } else if p.contains("/usb") {
        DiskBus::Usb
    } else if p.contains("/ata") {
        DiskBus::Sata
    } else if p.contains("/mmc") || name.starts_with("mmcblk") {
        DiskBus::Sd
    } else if p.contains("virtio") || p.contains("/vmbus") || p.contains("/virtual/") || name.starts_with("loop") {
        DiskBus::Virtual
    } else {
        DiskBus::Unknown
    }
}

/// Natural sort key: digit runs compare numerically (`sdb` < `sdaa` is not attempted, but
/// `loop2` < `loop10` and `nvme0n2` < `nvme0n10`).
fn natural_key(s: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut num: Option<u64> = None;
    for c in s.chars() {
        if let Some(d) = c.to_digit(10) {
            num = Some(num.unwrap_or(0).saturating_mul(10).saturating_add(d as u64));
        } else {
            if let Some(n) = num.take() {
                out.push((std::mem::take(&mut text), n));
            }
            text.push(c);
        }
    }
    out.push((text, num.unwrap_or(0)));
    out
}

/// Names of the managed disks under `root`, in enumeration (index) order: real disks in natural
/// name order, then loop devices.
pub fn disk_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| is_managed(n, &root.join(n)))
                .collect()
        })
        .unwrap_or_default();
    // loop devices last: attaching an image must not renumber the real disks
    names.sort_by_key(|n| (n.starts_with("loop"), natural_key(n)));
    names
}

/// Model string: vendor + model (`ATA` vendor dropped), the MMC card name, or the loop
/// backing file.
fn model(dir: &Path, name: &str) -> String {
    if name.starts_with("loop") {
        let backing = read_trimmed(&dir.join("loop/backing_file")).unwrap_or_default();
        return format!("Loop device ({backing})");
    }
    let dev = dir.join("device");
    let vendor = read_trimmed(&dev.join("vendor")).unwrap_or_default();
    let model = read_trimmed(&dev.join("model"))
        .filter(|m| !m.is_empty())
        .or_else(|| read_trimmed(&dev.join("name")))
        .unwrap_or_default();
    let vendor = if vendor.eq_ignore_ascii_case("ata") || vendor.starts_with("0x") { "" } else { vendor.as_str() };
    format!("{vendor} {model}").trim().to_string()
}

fn partitions(dir: &Path) -> Vec<PartitionInfo> {
    let mut parts: Vec<PartitionInfo> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let pdir = e.path();
                    let number = read_u64(&pdir.join("partition"))? as u32;
                    let name = e.file_name().to_str()?.to_string();
                    let sectors = read_u64(&pdir.join("size"))?;
                    // an MBR extended container shows up as a 1 KiB stub: not a volume
                    let stub = number <= 4 && sectors <= 2;
                    Some(PartitionInfo {
                        number,
                        start: read_u64(&pdir.join("start"))?.saturating_mul(SYSFS_SECTOR),
                        len: sectors.saturating_mul(SYSFS_SECTOR),
                        flags: PartFlags { read_only: read_flag(&pdir.join("ro")), ..Default::default() },
                        volume: (!stub).then(|| VolumeId(format!("/dev/{name}"))),
                        ..Default::default()
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    parts.sort_by_key(|p| (p.number, p.start));
    parts
}

/// udev's device database.
pub const UDEV_DATA: &str = "/run/udev/data";

/// The unit serial number from a SCSI VPD page 0x80 (`device/vpd_pg80`): byte 1 is the page
/// code, bytes 2..4 the big-endian length, then the ASCII serial.
pub fn parse_vpd_pg80(page: &[u8]) -> Option<String> {
    if page.len() < 4 || page[1] != 0x80 {
        return None;
    }
    let len = u16::from_be_bytes([page[2], page[3]]) as usize;
    let raw = page.get(4..4 + len).unwrap_or(&page[4..]);
    let s: String = raw.iter().filter(|b| b.is_ascii_graphic() || **b == b' ').map(|&b| b as char).collect();
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `E:<key>=<value>` from a udev database record.
fn udev_property(record: &str, key: &str) -> Option<String> {
    record.lines().find_map(|l| {
        let (k, v) = l.strip_prefix("E:")?.split_once('=')?;
        (k == key && !v.trim().is_empty()).then(|| v.trim().to_string())
    })
}

/// The disk's serial number, so the same disk is recognised across enumerations even when
/// the kernel exposes no `serial` attribute (SATA, USB): udev's `ID_SERIAL_SHORT`
/// (`<udev>/b<major>:<minor>`), then `device/serial` (NVMe, MMC), `serial` (virtio),
/// `device/wwid` and the SCSI VPD page 0x80.
pub fn serial(dir: &Path, udev: &Path) -> Option<String> {
    let nonempty = |s: Option<String>| s.filter(|s| !s.is_empty());
    let from_udev = read_trimmed(&dir.join("dev"))
        .and_then(|majmin| std::fs::read_to_string(udev.join(format!("b{majmin}"))).ok())
        .and_then(|rec| udev_property(&rec, "ID_SERIAL_SHORT"));
    nonempty(from_udev)
        .or_else(|| nonempty(read_trimmed(&dir.join("device/serial"))))
        .or_else(|| nonempty(read_trimmed(&dir.join("serial"))))
        .or_else(|| nonempty(read_trimmed(&dir.join("device/wwid"))))
        .or_else(|| std::fs::read(dir.join("device/vpd_pg80")).ok().and_then(|p| parse_vpd_pg80(&p)))
}

/// What a device node may be used as for volume operations (mount, format, resize, open).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A partition of the managed disk `disk`.
    Partition { disk: String },
    /// A managed disk without kernel partitions (a filesystem on the whole disk).
    WholeDisk,
}

fn has_kernel_partitions(disk_dir: &Path) -> bool {
    std::fs::read_dir(disk_dir)
        .map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.path().join("partition").exists()))
        .unwrap_or(false)
}

/// Classify the block device `name` through `<class>/<name>` (`/sys/class/block`) against the
/// managed disks under `block_root` (`/sys/block`). Refused: unknown devices, partitions of
/// unmanaged devices, device-mapper / md / zram / optical / unattached loop nodes, and a whole
/// disk that has partitions (a volume operation on it would overwrite the partition table).
pub fn classify_node(class: &Path, block_root: &Path, name: &str) -> std::result::Result<NodeKind, String> {
    let entry = class.join(name);
    if !entry.exists() {
        return Err(format!("/dev/{name} is not a block device known to the kernel"));
    }
    let managed = |disk: &str| block_root.join(disk).exists() && is_managed(disk, &block_root.join(disk));
    if entry.join("partition").exists() {
        let canonical = std::fs::canonicalize(&entry).map_err(|e| format!("/dev/{name}: {e}"))?;
        let disk = canonical
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        return if managed(&disk) {
            Ok(NodeKind::Partition { disk })
        } else {
            Err(format!("/dev/{name} is a partition of /dev/{disk}, which this program does not manage"))
        };
    }
    if !managed(name) {
        return Err(format!("/dev/{name} is not a disk or partition this program manages"));
    }
    if has_kernel_partitions(&block_root.join(name)) {
        return Err(format!(
            "/dev/{name} is a whole disk with partitions: use one of its partitions"
        ));
    }
    Ok(NodeKind::WholeDisk)
}

/// The disk sequence number (`<root>/<name>/diskseq`, Linux 5.15+).
pub fn diskseq(root: &Path, name: &str) -> Option<u64> {
    read_u64(&root.join(name).join("diskseq"))
}

fn canonical_string(p: &Path) -> String {
    std::fs::canonicalize(p).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One disk from `<root>/<name>`.
fn disk(root: &Path, udev: &Path, name: &str, index: u32) -> Option<DiskInfo> {
    let dir: PathBuf = root.join(name);
    let size = read_u64(&dir.join("size"))?.saturating_mul(SYSFS_SECTOR);
    if size == 0 {
        return None; // empty card reader, ejected media
    }
    let q = dir.join("queue");
    let logical = read_u64(&q.join("logical_block_size")).filter(|&v| v > 0).unwrap_or(512) as u32;
    let physical = read_u64(&q.join("physical_block_size")).filter(|&v| v > 0).unwrap_or(logical as u64) as u32;
    let device_path = format!("{} {}", canonical_string(&dir), canonical_string(&dir.join("device")));
    let serial = serial(&dir, udev).unwrap_or_default();
    let mut d = DiskInfo {
        index,
        path: format!("/dev/{name}"),
        model: model(&dir, name),
        serial,
        bus: bus_from_path(&device_path, name),
        size,
        sector_size: logical,
        physical_sector_size: physical,
        partitions: partitions(&dir),
        ..Default::default()
    };
    d.flags.read_only = read_flag(&dir.join("ro"));
    d.flags.removable = read_flag(&dir.join("removable"));
    d.flags.trim = read_u64(&q.join("discard_max_bytes")).is_some_and(|v| v > 0);
    Some(d)
}

/// Every managed disk under `root` (normally `/sys/block`), `index` = position in the sorted
/// list. Table kind, partition types and filesystems are not known here (see
/// [`super::LinuxPlatform`], which reads the device); partitions come from sysfs.
pub fn parse_block_dir(root: &Path) -> Vec<DiskInfo> {
    parse_block_dir_with(root, Path::new(UDEV_DATA))
}

/// [`parse_block_dir`] with the udev database in `udev` instead of `/run/udev/data`.
pub fn parse_block_dir_with(root: &Path, udev: &Path) -> Vec<DiskInfo> {
    disk_names(root)
        .iter()
        .filter_map(|n| disk(root, udev, n, 0))
        .enumerate()
        .map(|(i, mut d)| {
            d.index = i as u32;
            d
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["loop10", "sdb", "loop2", "nvme0n10", "nvme0n2", "sda", "mmcblk0"];
        v.sort_by_key(|n| natural_key(n));
        assert_eq!(v, ["loop2", "loop10", "mmcblk0", "nvme0n2", "nvme0n10", "sda", "sdb"]);
    }

    #[test]
    fn bus_detection() {
        let b = |p: &str, n: &str| bus_from_path(p, n);
        assert_eq!(b("/sys/devices/pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/nvme0n1", "nvme0n1"), DiskBus::Nvme);
        assert_eq!(b("/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0", "sdb"), DiskBus::Usb);
        assert_eq!(b("/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0", "sda"), DiskBus::Sata);
        assert_eq!(b("/sys/devices/platform/fe320000.mmc/mmc_host/mmc0/mmc0:aaaa", "mmcblk0"), DiskBus::Sd);
        assert_eq!(b("/sys/devices/pci0000:00/0000:00:04.0/virtio1/block/vda", "vda"), DiskBus::Virtual);
        assert_eq!(b("/sys/devices/virtual/block/loop0 ", "loop0"), DiskBus::Virtual);
        assert_eq!(b("/sys/devices/LNXSYSTM:00/LNXSYBUS:00/ACPI0004:00/VMBUS:00/fd1d2cbd/host0/target0:0:0/0:0:0:0", "sda"), DiskBus::Virtual);
        assert_eq!(b("/sys/devices/platform/host3/target3:0:0/3:0:0:0", "sdc"), DiskBus::Unknown);
    }
}
