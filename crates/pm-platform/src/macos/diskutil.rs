//! Pure parsers for `diskutil list -plist` and `diskutil info -plist <dev>` output.
//!
//! These compile on every OS so the fixtures in `tests/fixtures/macos/` are checked everywhere.
//! Key names follow Apple's `diskutil` property lists (`AllDisksAndPartitions`,
//! `DeviceIdentifier`, `Size`, `Content`, `BusProtocol`, `DeviceBlockSize`, ...). Unknown keys
//! are ignored and every key is optional, because the set varies between macOS releases.

use pm_core::{gpt_types, DiskBus, Error, FsKind, PartitionType, Result, TableKind};
use serde::Deserialize;
use uuid::Uuid;

/// Runs `diskutil` with `args` and returns its stdout (a property list for `-plist` calls).
/// The real runner shells out to `/usr/sbin/diskutil`; tests feed fixtures.
pub trait DiskutilRunner: Send + Sync {
    fn run(&self, args: &[&str]) -> Result<Vec<u8>>;
}

/// Content of a synthesized APFS container disk (`diskutil list` shows it as `Apple_APFS
/// Container`): the APFS container partition-scheme GUID.
const APFS_CONTAINER_SCHEME: &str = "EF57347C-0000-11AA-AA11-00306543ECAC";

/// One slice (`diskNsM`) from `diskutil list -plist`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListedPartition {
    pub identifier: String,
    pub size: u64,
    /// Partition type name (`EFI`, `Apple_APFS`, `Microsoft Basic Data`, `Windows_FAT_32`, ...).
    pub content: String,
    pub volume_name: String,
    pub mount_point: Option<String>,
    /// GPT unique partition GUID.
    pub disk_uuid: Option<String>,
}

/// A physical whole disk (`diskN`) from `diskutil list -plist`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListedDisk {
    pub identifier: String,
    pub size: u64,
    /// Partition scheme (`GUID_partition_scheme`, `FDisk_partition_scheme`,
    /// `Apple_partition_scheme`), a filesystem type for a partitionless disk, or empty.
    pub content: String,
    pub partitions: Vec<ListedPartition>,
}

/// The plan's name for a listed whole disk.
pub type WholeDisk = ListedDisk;

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawStore {
    #[serde(rename = "DeviceIdentifier")]
    device_identifier: Option<String>,
    #[serde(rename = "APFSPhysicalStore")]
    apfs_physical_store: Option<String>,
}

impl RawStore {
    fn identifier(&self) -> Option<String> {
        self.apfs_physical_store.clone().or_else(|| self.device_identifier.clone())
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct RawListPartition {
    device_identifier: String,
    size: u64,
    content: String,
    volume_name: String,
    mount_point: Option<String>,
    #[serde(rename = "DiskUUID")]
    disk_uuid: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct RawListDisk {
    device_identifier: String,
    size: u64,
    content: String,
    partitions: Vec<RawListPartition>,
    #[serde(rename = "APFSPhysicalStores")]
    apfs_physical_stores: Option<Vec<RawStore>>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct RawList {
    all_disks_and_partitions: Vec<RawListDisk>,
}

fn parse_plist<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T> {
    plist::from_bytes(bytes).map_err(|e| Error::Parse(format!("diskutil {what} plist: {e}")))
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Physical whole disks from `diskutil list -plist`, in listing order. Synthesized APFS
/// container disks (those with `APFSPhysicalStores`, or the APFS container scheme as content)
/// are skipped: their storage is already shown as the physical store's `Apple_APFS` slice.
pub fn parse_list(xml: &[u8]) -> Result<Vec<ListedDisk>> {
    let raw: RawList = parse_plist(xml, "list")?;
    Ok(raw
        .all_disks_and_partitions
        .into_iter()
        .filter(|d| {
            let synthesized = d.apfs_physical_stores.as_ref().is_some_and(|s| !s.is_empty())
                || d.content.eq_ignore_ascii_case(APFS_CONTAINER_SCHEME);
            if synthesized {
                log::debug!("{}: synthesized APFS container disk, skipped", d.device_identifier);
            }
            !synthesized && parse_identifier(&d.device_identifier).is_some_and(|(_, s)| s.is_none())
        })
        .map(|d| ListedDisk {
            identifier: d.device_identifier,
            size: d.size,
            content: d.content,
            partitions: d
                .partitions
                .into_iter()
                .map(|p| ListedPartition {
                    identifier: p.device_identifier,
                    size: p.size,
                    content: p.content,
                    volume_name: p.volume_name,
                    mount_point: non_empty(p.mount_point),
                    disk_uuid: non_empty(p.disk_uuid),
                })
                .collect(),
        })
        .collect())
}

/// Properties of one disk or slice from `diskutil info -plist <dev>`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// `disk2`, `disk2s1`, `disk3s1s1`, ...
    pub identifier: String,
    pub size: u64,
    /// Logical block size (`DeviceBlockSize`); 512 when missing.
    pub device_block_size: u32,
    /// Model for whole disks, GPT partition name for slices.
    pub media_name: String,
    pub io_registry_entry_name: String,
    /// `USB`, `SATA`, `PCI-Express`, `Apple Fabric`, `Disk Image`, ...
    pub bus_protocol: String,
    /// `Removable`, `RemovableMedia` or `Ejectable`.
    pub removable: bool,
    pub writable: bool,
    pub whole_disk: bool,
    pub parent_whole_disk: String,
    pub mount_point: Option<String>,
    pub volume_name: String,
    /// Short type (`apfs`, `hfs`, `msdos`, `exfat`, `ntfs`, ...).
    pub filesystem_type: String,
    /// Long name (`MS-DOS FAT32`, `Mac OS Extended (Journaled)`, ...).
    pub filesystem_name: String,
    pub total_size: u64,
    pub free_space: Option<u64>,
    pub content: String,
    /// GPT unique partition GUID of a slice.
    pub disk_uuid: Option<String>,
    pub internal: bool,
    pub solid_state: bool,
    /// Physical stores (`disk0s2`) of an APFS volume or container.
    pub apfs_physical_stores: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
struct RawInfo {
    device_identifier: String,
    size: u64,
    device_block_size: u32,
    media_name: String,
    #[serde(rename = "IORegistryEntryName")]
    io_registry_entry_name: String,
    bus_protocol: String,
    removable: bool,
    removable_media: bool,
    ejectable: bool,
    writable: bool,
    whole_disk: bool,
    parent_whole_disk: String,
    mount_point: Option<String>,
    volume_name: String,
    filesystem_type: String,
    filesystem_name: String,
    total_size: u64,
    free_space: Option<u64>,
    content: String,
    #[serde(rename = "DiskUUID")]
    disk_uuid: Option<String>,
    internal: bool,
    solid_state: bool,
    #[serde(rename = "APFSPhysicalStores")]
    apfs_physical_stores: Vec<RawStore>,
}

/// Parse `diskutil info -plist <dev>`.
pub fn parse_info(xml: &[u8]) -> Result<DeviceInfo> {
    let r: RawInfo = parse_plist(xml, "info")?;
    if r.device_identifier.is_empty() {
        return Err(Error::Parse("diskutil info plist: no DeviceIdentifier".into()));
    }
    Ok(DeviceInfo {
        identifier: r.device_identifier,
        size: if r.size > 0 { r.size } else { r.total_size },
        device_block_size: if r.device_block_size > 0 { r.device_block_size } else { 512 },
        media_name: r.media_name.trim().to_string(),
        io_registry_entry_name: r.io_registry_entry_name,
        bus_protocol: r.bus_protocol,
        removable: r.removable || r.removable_media || r.ejectable,
        writable: r.writable,
        whole_disk: r.whole_disk,
        parent_whole_disk: r.parent_whole_disk,
        mount_point: non_empty(r.mount_point),
        volume_name: r.volume_name,
        filesystem_type: r.filesystem_type,
        filesystem_name: r.filesystem_name,
        total_size: r.total_size,
        free_space: r.free_space,
        content: r.content,
        disk_uuid: non_empty(r.disk_uuid),
        internal: r.internal,
        solid_state: r.solid_state,
        apfs_physical_stores: r.apfs_physical_stores.iter().filter_map(RawStore::identifier).collect(),
    })
}

/// `diskN` / `diskNsM` (also with `/dev/`, `/dev/r` in front; `disk3s1s1` snapshots give the
/// slice `1`) → `(N, Some(M))`, or `(N, None)` for a whole disk.
pub fn parse_identifier(s: &str) -> Option<(u32, Option<u32>)> {
    let s = s.strip_prefix("/dev/").unwrap_or(s);
    let s = s.strip_prefix('r').filter(|r| r.starts_with("disk")).unwrap_or(s);
    let rest = s.strip_prefix("disk")?;
    let mut parts = rest.split('s');
    let disk = parts.next().filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))?;
    let disk: u32 = disk.parse().ok()?;
    let mut slice = None;
    for (i, p) in parts.enumerate() {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if i == 0 {
            slice = Some(p.parse().ok()?);
        }
    }
    Some((disk, slice))
}

/// The `diskNsM` identifier `diskutil` takes for a volume id (`/dev/diskNsM`, `/dev/rdiskNsM`
/// or `diskNsM`).
pub fn volume_identifier(id: &str) -> Option<String> {
    let s = id.strip_prefix("/dev/").unwrap_or(id);
    let s = s.strip_prefix('r').filter(|r| r.starts_with("disk")).unwrap_or(s);
    parse_identifier(s).map(|_| s.to_string())
}

/// Raw (character) device node of a volume id: `/dev/diskNsM` → `/dev/rdiskNsM`.
pub fn raw_node(id: &str) -> Option<String> {
    volume_identifier(id).map(|s| format!("/dev/r{s}"))
}

/// `BusProtocol` → [`DiskBus`].
pub fn bus_of(protocol: &str) -> DiskBus {
    match protocol.trim() {
        "USB" => DiskBus::Usb,
        "SATA" | "ATA" => DiskBus::Sata,
        "PCI-Express" | "PCI" | "Apple Fabric" | "NVMe" | "NVM Express" => DiskBus::Nvme,
        "Disk Image" | "Virtual Interface" => DiskBus::Virtual,
        "SD" | "Secure Digital" => DiskBus::Sd,
        "SAS" | "SCSI" | "Fibre Channel" | "Fibre Channel Interface" => DiskBus::Scsi,
        _ => DiskBus::Unknown,
    }
}

/// Partition scheme of a whole disk's `Content`: `Some(Gpt / Mbr)`, `Some(Unknown)` for an
/// Apple Partition Map (not handled by pm-parttable), `Some(None)` for a disk without a
/// partition table (blank, or a filesystem on the whole disk).
pub fn scheme_of(content: &str) -> TableKind {
    match content {
        "GUID_partition_scheme" => TableKind::Gpt,
        "FDisk_partition_scheme" => TableKind::Mbr,
        "Apple_partition_scheme" => TableKind::Unknown,
        _ => TableKind::None,
    }
}

/// Filesystem from `FilesystemType` / `FilesystemName`, else from the partition type name
/// (`Apple_APFS`, `Apple_HFS`: containers and unmounted volumes have no `FilesystemType`).
pub fn fs_of(filesystem_type: &str, filesystem_name: &str, content: &str) -> Option<FsKind> {
    let by_type = match filesystem_type.to_ascii_lowercase().as_str() {
        "apfs" => Some(FsKind::Apfs),
        "hfs" => Some(FsKind::HfsPlus),
        "msdos" => Some(if filesystem_name.contains("FAT16") {
            FsKind::Fat16
        } else if filesystem_name.contains("FAT12") {
            FsKind::Fat12
        } else {
            FsKind::Fat32
        }),
        "exfat" => Some(FsKind::ExFat),
        "ntfs" => Some(FsKind::Ntfs),
        "udf" => Some(FsKind::Udf),
        "cd9660" => Some(FsKind::Iso9660),
        _ => None,
    };
    by_type.or(match content {
        "Apple_APFS" | "Apple_APFS_ISC" | "Apple_APFS_Recovery" => Some(FsKind::Apfs),
        "Apple_HFS" | "Apple_HFSX" => Some(FsKind::HfsPlus),
        _ => None,
    })
}

/// Partition type from a slice's `Content` on a disk with the given scheme (only used when
/// the table itself cannot be read). A raw GUID string is taken as the GPT type.
pub fn ptype_of(content: &str, scheme: TableKind) -> Option<PartitionType> {
    match scheme {
        TableKind::Gpt => {
            let guid = match content {
                "EFI" => gpt_types::EFI_SYSTEM,
                "Apple_APFS" => gpt_types::APPLE_APFS,
                "Apple_HFS" => gpt_types::APPLE_HFS,
                "Microsoft Basic Data" => gpt_types::BASIC_DATA,
                "Microsoft Reserved" => gpt_types::MSR,
                "Windows Recovery" => gpt_types::WINDOWS_RECOVERY,
                "Linux Filesystem" | "Linux" => gpt_types::LINUX_FS,
                "Linux Swap" => gpt_types::LINUX_SWAP,
                "Linux LVM" => gpt_types::LINUX_LVM,
                other => Uuid::parse_str(other).ok()?,
            };
            Some(PartitionType::Gpt(guid))
        }
        TableKind::Mbr => {
            let id = match content {
                "DOS_FAT_12" => 0x01,
                "DOS_FAT_16" | "Windows_FAT_16" => 0x06,
                "Windows_NTFS" | "Windows_exFAT" => 0x07,
                "DOS_FAT_32" => 0x0B,
                "Windows_FAT_32" => 0x0C,
                "Linux_Swap" => 0x82,
                "Linux" => 0x83,
                "EFI" => 0xEF,
                "Apple_HFS" => 0xAF,
                _ => return None,
            };
            Some(PartitionType::Mbr(id))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        assert_eq!(parse_identifier("disk0"), Some((0, None)));
        assert_eq!(parse_identifier("disk12s3"), Some((12, Some(3))));
        assert_eq!(parse_identifier("/dev/rdisk2s1"), Some((2, Some(1))));
        assert_eq!(parse_identifier("/dev/disk3s1s1"), Some((3, Some(1))));
        for bad in ["", "disk", "disks1", "disk1s", "sda1", "/dev/rsda", "disk1x2", "disk1s2s"] {
            assert_eq!(parse_identifier(bad), None, "{bad}");
        }
        assert_eq!(volume_identifier("/dev/disk2s1").as_deref(), Some("disk2s1"));
        assert_eq!(volume_identifier("/dev/rdisk2s1").as_deref(), Some("disk2s1"));
        assert_eq!(volume_identifier("disk2s1").as_deref(), Some("disk2s1"));
        assert_eq!(volume_identifier("/dev/sda1"), None);
        assert_eq!(raw_node("/dev/disk4s2").as_deref(), Some("/dev/rdisk4s2"));
        assert_eq!(raw_node("/dev/rdisk4s2").as_deref(), Some("/dev/rdisk4s2"));
    }

    #[test]
    fn buses_schemes_filesystems_types() {
        assert_eq!(bus_of("USB"), DiskBus::Usb);
        assert_eq!(bus_of("SATA"), DiskBus::Sata);
        assert_eq!(bus_of("PCI-Express"), DiskBus::Nvme);
        assert_eq!(bus_of("Apple Fabric"), DiskBus::Nvme);
        assert_eq!(bus_of("Disk Image"), DiskBus::Virtual);
        assert_eq!(bus_of("Virtual Interface"), DiskBus::Virtual);
        assert_eq!(bus_of("Secure Digital"), DiskBus::Sd);
        assert_eq!(bus_of("Thunderbolt"), DiskBus::Unknown);
        assert_eq!(scheme_of("GUID_partition_scheme"), TableKind::Gpt);
        assert_eq!(scheme_of("FDisk_partition_scheme"), TableKind::Mbr);
        assert_eq!(scheme_of("Apple_partition_scheme"), TableKind::Unknown);
        assert_eq!(scheme_of(""), TableKind::None);
        assert_eq!(fs_of("msdos", "MS-DOS FAT32", "Windows_FAT_32"), Some(FsKind::Fat32));
        assert_eq!(fs_of("msdos", "MS-DOS FAT16", ""), Some(FsKind::Fat16));
        assert_eq!(fs_of("hfs", "Mac OS Extended (Journaled)", ""), Some(FsKind::HfsPlus));
        assert_eq!(fs_of("", "", "Apple_APFS"), Some(FsKind::Apfs));
        assert_eq!(fs_of("exfat", "", ""), Some(FsKind::ExFat));
        assert_eq!(fs_of("ntfs", "", "Microsoft Basic Data"), Some(FsKind::Ntfs));
        assert_eq!(fs_of("", "", "EFI"), None);
        assert_eq!(ptype_of("EFI", TableKind::Gpt), Some(PartitionType::Gpt(gpt_types::EFI_SYSTEM)));
        assert_eq!(
            ptype_of("0FC63DAF-8483-4772-8E79-3D69D8477DE4", TableKind::Gpt),
            Some(PartitionType::Gpt(gpt_types::LINUX_FS))
        );
        assert_eq!(ptype_of("Windows_FAT_32", TableKind::Mbr), Some(PartitionType::Mbr(0x0C)));
        assert_eq!(ptype_of("Windows_FAT_32", TableKind::Unknown), None);
        assert_eq!(ptype_of("Something", TableKind::Gpt), None);
    }

    #[test]
    fn bad_plists_are_parse_errors() {
        assert!(matches!(parse_list(b"not a plist"), Err(Error::Parse(_))));
        assert!(matches!(parse_info(b"<plist version=\"1.0\"><dict/></plist>"), Err(Error::Parse(_))));
    }
}
