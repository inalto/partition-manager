//! Pure parsers for the raw output buffers of Windows storage IOCTLs.
//!
//! The IOCTLs return variable-length C structures; parsing them from bytes (instead of casting
//! pointers) keeps the unsafe surface tiny and lets the logic be unit-tested with hand-built
//! buffers. Offsets are those of the x86-64 layouts (checked against `windows-sys` in tests).

use pm_core::{DiskBus, Error, FsKind, PartFlags, PartitionInfo, PartitionType, Result, TableKind};
use uuid::Uuid;

/// `sizeof` of the fixed part of `DRIVE_LAYOUT_INFORMATION_EX` (offset of `PartitionEntry`).
pub const LAYOUT_HEADER_LEN: usize = 48;
/// `sizeof(PARTITION_INFORMATION_EX)`.
pub const PARTITION_ENTRY_LEN: usize = 144;
/// `sizeof(DISK_EXTENT)`; extents start at offset 8 of `VOLUME_DISK_EXTENTS`.
pub const DISK_EXTENT_LEN: usize = 24;

/// GPT attribute bits mapped to [`PartFlags`].
pub const GPT_ATTR_REQUIRED: u64 = 1 << 0;
pub const GPT_ATTR_READ_ONLY: u64 = 1 << 60;
pub const GPT_ATTR_HIDDEN: u64 = 1 << 62;
pub const GPT_ATTR_NO_LETTER: u64 = 1 << 63;

/// Partition table information decoded from `IOCTL_DISK_GET_DRIVE_LAYOUT_EX`.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedLayout {
    pub table: TableKind,
    /// MBR signature as `{:08X}` or the GPT disk GUID.
    pub disk_id: String,
    /// Partitions sorted by start offset (MBR extended containers and empty slots omitted).
    pub partitions: Vec<PartitionInfo>,
}

/// Fields of `STORAGE_DEVICE_DESCRIPTOR` we use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub model: String,
    pub serial: String,
    pub bus: DiskBus,
    pub removable: bool,
}

fn short(what: &str) -> Error {
    Error::Parse(format!("{what}: buffer too short"))
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// Decode a Windows `GUID` (mixed-endian: `Data1..Data3` little-endian, `Data4` as bytes).
pub fn guid_from_bytes(b: &[u8; 16]) -> Uuid {
    Uuid::from_fields(
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        &[b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]],
    )
}

fn guid_at(b: &[u8], o: usize) -> Option<Uuid> {
    let bytes: &[u8; 16] = b.get(o..o + 16)?.try_into().ok()?;
    Some(guid_from_bytes(bytes))
}

/// Map GPT attribute bits to partition flags.
pub fn gpt_flags(attrs: u64) -> PartFlags {
    PartFlags {
        required: attrs & GPT_ATTR_REQUIRED != 0,
        read_only: attrs & GPT_ATTR_READ_ONLY != 0,
        hidden: attrs & GPT_ATTR_HIDDEN != 0,
        no_letter: attrs & GPT_ATTR_NO_LETTER != 0,
        ..Default::default()
    }
}

/// Parse the output buffer of `IOCTL_DISK_GET_DRIVE_LAYOUT_EX`.
///
/// MBR partition numbers follow `pm_parttable`'s convention (primaries use their slot 1..=4,
/// logicals continue at 5), so they match `read_table`; GPT numbers are Windows' own.
pub fn parse_drive_layout(buf: &[u8]) -> Result<ParsedLayout> {
    const WHAT: &str = "DRIVE_LAYOUT_INFORMATION_EX";
    if buf.len() < LAYOUT_HEADER_LEN {
        return Err(short(WHAT));
    }
    let style = u32_at(buf, 0).ok_or_else(|| short(WHAT))?;
    let count = u32_at(buf, 4).ok_or_else(|| short(WHAT))? as usize;
    let (table, disk_id) = match style {
        0 => (TableKind::Mbr, format!("{:08X}", u32_at(buf, 8).ok_or_else(|| short(WHAT))?)),
        1 => (TableKind::Gpt, guid_at(buf, 8).ok_or_else(|| short(WHAT))?.to_string()),
        2 => {
            return Ok(ParsedLayout {
                table: TableKind::None,
                disk_id: String::new(),
                partitions: Vec::new(),
            })
        }
        other => return Err(Error::Parse(format!("unknown partition style {other}"))),
    };
    let needed = count
        .checked_mul(PARTITION_ENTRY_LEN)
        .and_then(|n| n.checked_add(LAYOUT_HEADER_LEN))
        .ok_or_else(|| short(WHAT))?;
    if buf.len() < needed {
        return Err(short(WHAT));
    }

    let mut partitions = Vec::new();
    let mut next_logical = 5u32;
    for slot in 0..count {
        let e = &buf[LAYOUT_HEADER_LEN + slot * PARTITION_ENTRY_LEN..][..PARTITION_ENTRY_LEN];
        let start = u64_at(e, 8).ok_or_else(|| short(WHAT))?;
        let len = u64_at(e, 16).ok_or_else(|| short(WHAT))?;
        let win_number = u32_at(e, 24).ok_or_else(|| short(WHAT))?;
        let part = match table {
            TableKind::Mbr => {
                let ty = e[32];
                let ptype = PartitionType::Mbr(ty);
                // empty slots and extended containers / EBR links are not partitions
                if ty == 0 || len == 0 || ptype.is_extended_mbr() {
                    continue;
                }
                let logical = slot >= 4;
                let number = if logical {
                    next_logical += 1;
                    next_logical - 1
                } else {
                    slot as u32 + 1
                };
                PartitionInfo {
                    number,
                    start,
                    len,
                    ptype: Some(ptype),
                    flags: PartFlags {
                        active: e[33] != 0,
                        ..Default::default()
                    },
                    logical,
                    ..Default::default()
                }
            }
            _ => {
                let ty = guid_at(e, 32).ok_or_else(|| short(WHAT))?;
                if ty.is_nil() || len == 0 {
                    continue;
                }
                let name_units: Vec<u16> = (0..36)
                    .map_while(|k| u16_at(e, 72 + 2 * k).filter(|&u| u != 0))
                    .collect();
                PartitionInfo {
                    number: win_number,
                    start,
                    len,
                    ptype: Some(PartitionType::Gpt(ty)),
                    guid: guid_at(e, 48).filter(|g| !g.is_nil()),
                    name: String::from_utf16_lossy(&name_units),
                    flags: gpt_flags(u64_at(e, 64).ok_or_else(|| short(WHAT))?),
                    ..Default::default()
                }
            }
        };
        partitions.push(part);
    }
    partitions.sort_by_key(|p| p.start);
    Ok(ParsedLayout {
        table,
        disk_id,
        partitions,
    })
}

/// Parse `VOLUME_DISK_EXTENTS` into `(disk number, start, len)` triples.
pub fn parse_disk_extents(buf: &[u8]) -> Result<Vec<(u32, u64, u64)>> {
    const WHAT: &str = "VOLUME_DISK_EXTENTS";
    let count = u32_at(buf, 0).ok_or_else(|| short(WHAT))? as usize;
    (0..count)
        .map(|i| {
            let o = 8 + i * DISK_EXTENT_LEN;
            Some((u32_at(buf, o)?, u64_at(buf, o + 8)?, u64_at(buf, o + 16)?))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| short(WHAT))
}

/// Map a `STORAGE_BUS_TYPE` value to [`DiskBus`].
pub fn bus_from_storage(bus: i32) -> DiskBus {
    use windows_sys::Win32::Storage::FileSystem as fs;
    match bus {
        fs::BusTypeNvme => DiskBus::Nvme,
        fs::BusTypeSata | fs::BusTypeAta => DiskBus::Sata,
        fs::BusTypeUsb => DiskBus::Usb,
        fs::BusTypeScsi | fs::BusTypeSas => DiskBus::Scsi,
        fs::BusTypeSd | fs::BusTypeMmc => DiskBus::Sd,
        fs::BusTypeVirtual | fs::BusTypeFileBackedVirtual => DiskBus::Virtual,
        _ => DiskBus::Unknown,
    }
}

/// NUL-terminated ASCII string at `offset` inside `buf` (offset 0 means "absent"), trimmed.
fn c_string_at(buf: &[u8], offset: u32) -> String {
    let o = offset as usize;
    if o == 0 || o >= buf.len() {
        return String::new();
    }
    let s = &buf[o..];
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..end]).trim().to_string()
}

/// Parse a `STORAGE_DEVICE_DESCRIPTOR` (from `StorageDeviceProperty`).
pub fn parse_device_descriptor(buf: &[u8]) -> Result<DeviceDescriptor> {
    const WHAT: &str = "STORAGE_DEVICE_DESCRIPTOR";
    if buf.len() < 32 {
        return Err(short(WHAT));
    }
    let field = |o| u32_at(buf, o).ok_or_else(|| short(WHAT));
    let vendor = c_string_at(buf, field(12)?);
    let product = c_string_at(buf, field(16)?);
    let model = [vendor, product]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(DeviceDescriptor {
        model,
        serial: c_string_at(buf, field(24)?),
        bus: bus_from_storage(field(28)? as i32),
        removable: buf[10] != 0,
    })
}

/// `(BytesPerLogicalSector, BytesPerPhysicalSector)` from a `STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR`.
pub fn parse_alignment_descriptor(buf: &[u8]) -> Option<(u32, u32)> {
    let logical = u32_at(buf, 16)?;
    let physical = u32_at(buf, 20)?;
    (logical != 0 && physical != 0).then_some((logical, physical))
}

/// `TrimEnabled` from a `DEVICE_TRIM_DESCRIPTOR`.
pub fn parse_trim_descriptor(buf: &[u8]) -> Option<bool> {
    buf.get(8).map(|&b| b != 0)
}

/// `STORAGE_PROPERTY_QUERY { PropertyId: id, QueryType: PropertyStandardQuery }`.
pub fn encode_property_query(property_id: i32) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0..4].copy_from_slice(&property_id.to_le_bytes());
    b
}

/// `SET_DISK_ATTRIBUTES` (`Version` = its size).
pub fn encode_set_disk_attributes(attributes: u64, mask: u64, persist: bool) -> [u8; 40] {
    let mut b = [0u8; 40];
    b[0..4].copy_from_slice(&40u32.to_le_bytes());
    b[4] = persist as u8;
    b[8..16].copy_from_slice(&attributes.to_le_bytes());
    b[16..24].copy_from_slice(&mask.to_le_bytes());
    b
}

/// `SHRINK_VOLUME_INFORMATION { ShrinkRequestType, Flags: 0, NewNumberOfSectors }`.
pub fn encode_shrink_request(request_type: i32, new_sectors: u64) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[0..4].copy_from_slice(&request_type.to_le_bytes());
    b[16..24].copy_from_slice(&new_sectors.to_le_bytes());
    b
}

/// `GET_LENGTH_INFORMATION.Length`.
pub fn parse_length_info(buf: &[u8]) -> Option<u64> {
    u64_at(buf, 0)
}

/// `(BytesPerSector, DiskSize)` from `DISK_GEOMETRY_EX`.
pub fn parse_geometry_ex(buf: &[u8]) -> Option<(u32, u64)> {
    Some((u32_at(buf, 20)?, u64_at(buf, 24)?))
}

/// `GET_DISK_ATTRIBUTES.Attributes`.
pub fn parse_disk_attributes(buf: &[u8]) -> Option<u64> {
    u64_at(buf, 8)
}

/// Map a `GetVolumeInformationW` filesystem name to [`FsKind`].
pub fn fs_from_name(name: &str) -> Option<FsKind> {
    match name.to_ascii_uppercase().as_str() {
        "NTFS" => Some(FsKind::Ntfs),
        "FAT32" => Some(FsKind::Fat32),
        "FAT" | "FAT16" => Some(FsKind::Fat16),
        "FAT12" => Some(FsKind::Fat12),
        "EXFAT" => Some(FsKind::ExFat),
        "REFS" => Some(FsKind::ReFs),
        "CDFS" => Some(FsKind::Iso9660),
        "UDF" => Some(FsKind::Udf),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::gpt_types;
    use std::mem::{offset_of, size_of};
    use windows_sys::Win32::System::Ioctl::*;

    const MIB: u64 = 1 << 20;

    fn put32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }
    /// Windows in-memory GUID bytes for `u`.
    fn guid_bytes(u: Uuid) -> [u8; 16] {
        let (d1, d2, d3, d4) = u.as_fields();
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&d1.to_le_bytes());
        b[4..6].copy_from_slice(&d2.to_le_bytes());
        b[6..8].copy_from_slice(&d3.to_le_bytes());
        b[8..16].copy_from_slice(d4);
        b
    }

    #[test]
    fn offsets_match_windows_sys() {
        assert_eq!(offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry), LAYOUT_HEADER_LEN);
        assert_eq!(offset_of!(DRIVE_LAYOUT_INFORMATION_EX, Anonymous), 8);
        assert_eq!(size_of::<PARTITION_INFORMATION_EX>(), PARTITION_ENTRY_LEN);
        assert_eq!(offset_of!(PARTITION_INFORMATION_EX, StartingOffset), 8);
        assert_eq!(offset_of!(PARTITION_INFORMATION_EX, PartitionLength), 16);
        assert_eq!(offset_of!(PARTITION_INFORMATION_EX, PartitionNumber), 24);
        assert_eq!(offset_of!(PARTITION_INFORMATION_EX, Anonymous), 32);
        assert_eq!(offset_of!(PARTITION_INFORMATION_MBR, BootIndicator), 1);
        assert_eq!(offset_of!(PARTITION_INFORMATION_GPT, PartitionId), 16);
        assert_eq!(offset_of!(PARTITION_INFORMATION_GPT, Attributes), 32);
        assert_eq!(offset_of!(PARTITION_INFORMATION_GPT, Name), 40);
        assert_eq!(size_of::<DISK_EXTENT>(), DISK_EXTENT_LEN);
        assert_eq!(offset_of!(VOLUME_DISK_EXTENTS, Extents), 8);
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia), 10);
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset), 12);
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, SerialNumberOffset), 24);
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType), 28);
        assert_eq!(offset_of!(STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, BytesPerLogicalSector), 16);
        assert_eq!(offset_of!(STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, BytesPerPhysicalSector), 20);
        assert_eq!(offset_of!(DEVICE_TRIM_DESCRIPTOR, TrimEnabled), 8);
    }

    #[test]
    fn guid_conversion_is_mixed_endian() {
        // EFI system partition type as stored on disk / in a Windows GUID struct
        let raw = [
            0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
            0x3B,
        ];
        assert_eq!(guid_from_bytes(&raw), gpt_types::EFI_SYSTEM);
        assert_eq!(guid_bytes(gpt_types::EFI_SYSTEM), raw);
    }

    fn mbr_entry(b: &mut [u8], slot: usize, ty: u8, boot: bool, start: u64, len: u64, number: u32) {
        let o = LAYOUT_HEADER_LEN + slot * PARTITION_ENTRY_LEN;
        put32(b, o, 0); // PARTITION_STYLE_MBR
        put64(b, o + 8, start);
        put64(b, o + 16, len);
        put32(b, o + 24, number);
        b[o + 32] = ty;
        b[o + 33] = boot as u8;
        b[o + 34] = 1; // RecognizedPartition
    }

    #[test]
    fn parses_mbr_layout_with_logicals() {
        let count = 8;
        let mut b = vec![0u8; LAYOUT_HEADER_LEN + count * PARTITION_ENTRY_LEN];
        put32(&mut b, 0, 0);
        put32(&mut b, 4, count as u32);
        put32(&mut b, 8, 0x1234_ABCD);
        mbr_entry(&mut b, 0, 0x07, true, MIB, 100 * MIB, 1);
        mbr_entry(&mut b, 1, 0x0F, false, 200 * MIB, 300 * MIB, 0); // extended container
        mbr_entry(&mut b, 4, 0x83, false, 201 * MIB, 50 * MIB, 2); // first logical
        mbr_entry(&mut b, 5, 0x05, false, 260 * MIB, 40 * MIB, 0); // link to next EBR
        mbr_entry(&mut b, 6, 0x00, false, 0, 0, 0); // empty slot
        let l = parse_drive_layout(&b).unwrap();
        assert_eq!(l.table, TableKind::Mbr);
        assert_eq!(l.disk_id, "1234ABCD");
        assert_eq!(l.partitions.len(), 2);
        let p = &l.partitions[0];
        assert_eq!((p.number, p.start, p.len), (1, MIB, 100 * MIB));
        assert_eq!(p.ptype, Some(PartitionType::Mbr(0x07)));
        assert!(p.flags.active);
        assert!(!p.logical);
        assert_eq!(p.guid, None);
        let q = &l.partitions[1];
        assert_eq!((q.number, q.start, q.len), (5, 201 * MIB, 50 * MIB));
        assert_eq!(q.ptype, Some(PartitionType::Mbr(0x83)));
        assert!(q.logical);
        assert!(!q.flags.active);
    }

    #[test]
    fn parses_gpt_layout() {
        let count = 2;
        let mut b = vec![0u8; LAYOUT_HEADER_LEN + count * PARTITION_ENTRY_LEN];
        put32(&mut b, 0, 1);
        put32(&mut b, 4, count as u32);
        let disk = Uuid::parse_str("11223344-5566-7788-99aa-bbccddeeff00").unwrap();
        b[8..24].copy_from_slice(&guid_bytes(disk));
        let part_guid = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        let entries = [
            (gpt_types::BASIC_DATA, 600 * MIB, 1000 * MIB, 3u32, GPT_ATTR_HIDDEN | GPT_ATTR_READ_ONLY, "Data"),
            (gpt_types::EFI_SYSTEM, MIB, 100 * MIB, 1u32, GPT_ATTR_REQUIRED | GPT_ATTR_NO_LETTER, "EFI system partition"),
        ];
        for (i, (ty, start, len, num, attrs, name)) in entries.iter().enumerate() {
            let o = LAYOUT_HEADER_LEN + i * PARTITION_ENTRY_LEN;
            put32(&mut b, o, 1);
            put64(&mut b, o + 8, *start);
            put64(&mut b, o + 16, *len);
            put32(&mut b, o + 24, *num);
            b[o + 32..o + 48].copy_from_slice(&guid_bytes(*ty));
            b[o + 48..o + 64].copy_from_slice(&guid_bytes(part_guid));
            put64(&mut b, o + 64, *attrs);
            for (k, u) in name.encode_utf16().enumerate() {
                b[o + 72 + 2 * k..o + 74 + 2 * k].copy_from_slice(&u.to_le_bytes());
            }
        }
        let l = parse_drive_layout(&b).unwrap();
        assert_eq!(l.table, TableKind::Gpt);
        assert_eq!(l.disk_id, "11223344-5566-7788-99aa-bbccddeeff00");
        assert_eq!(l.partitions.len(), 2);
        // sorted by start
        let efi = &l.partitions[0];
        assert_eq!((efi.number, efi.start, efi.len), (1, MIB, 100 * MIB));
        assert_eq!(efi.ptype, Some(PartitionType::Gpt(gpt_types::EFI_SYSTEM)));
        assert_eq!(efi.guid, Some(part_guid));
        assert_eq!(efi.name, "EFI system partition");
        assert!(efi.flags.required && efi.flags.no_letter);
        assert!(!efi.flags.hidden && !efi.flags.read_only && !efi.flags.active);
        let data = &l.partitions[1];
        assert_eq!(data.number, 3);
        assert_eq!(data.name, "Data");
        assert!(data.flags.hidden && data.flags.read_only && !data.flags.required);
    }

    #[test]
    fn parses_raw_layout_and_rejects_short_buffers() {
        let mut b = vec![0u8; LAYOUT_HEADER_LEN];
        put32(&mut b, 0, 2);
        let l = parse_drive_layout(&b).unwrap();
        assert_eq!(l.table, TableKind::None);
        assert!(l.partitions.is_empty());
        assert_eq!(l.disk_id, "");
        assert!(parse_drive_layout(&b[..20]).is_err());
        // count claims more entries than the buffer holds
        put32(&mut b, 0, 0);
        put32(&mut b, 4, 4);
        assert!(parse_drive_layout(&b).is_err());
        // unknown style
        put32(&mut b, 0, 9);
        put32(&mut b, 4, 0);
        assert!(parse_drive_layout(&b).is_err());
    }

    #[test]
    fn parses_disk_extents() {
        let mut b = vec![0u8; 8 + 2 * DISK_EXTENT_LEN];
        put32(&mut b, 0, 2);
        put32(&mut b, 8, 1);
        put64(&mut b, 16, MIB);
        put64(&mut b, 24, 5 * MIB);
        put32(&mut b, 32, 3);
        put64(&mut b, 40, 7 * MIB);
        put64(&mut b, 48, 9 * MIB);
        assert_eq!(
            parse_disk_extents(&b).unwrap(),
            vec![(1, MIB, 5 * MIB), (3, 7 * MIB, 9 * MIB)]
        );
        assert!(parse_disk_extents(&b[..30]).is_err());
        assert!(parse_disk_extents(&[0u8; 2]).is_err());
    }

    #[test]
    fn parses_device_descriptor() {
        let mut b = vec![0u8; 128];
        b[10] = 1; // removable
        put32(&mut b, 12, 40); // vendor
        put32(&mut b, 16, 52); // product
        put32(&mut b, 24, 80); // serial
        put32(&mut b, 28, 17); // NVMe
        b[40..48].copy_from_slice(b"Samsung ");
        b[52..76].copy_from_slice(b"SSD 980 PRO 1TB         ");
        b[80..95].copy_from_slice(b"  S5GXNX0R1234 ");
        let d = parse_device_descriptor(&b).unwrap();
        assert_eq!(d.model, "Samsung SSD 980 PRO 1TB");
        assert_eq!(d.serial, "S5GXNX0R1234");
        assert_eq!(d.bus, DiskBus::Nvme);
        assert!(d.removable);
        // no vendor (offset 0), no serial
        put32(&mut b, 12, 0);
        put32(&mut b, 24, 0);
        b[10] = 0;
        let d = parse_device_descriptor(&b).unwrap();
        assert_eq!(d.model, "SSD 980 PRO 1TB");
        assert_eq!(d.serial, "");
        assert!(!d.removable);
        // offsets beyond the buffer are ignored, short header is an error
        put32(&mut b, 16, 4000);
        assert_eq!(parse_device_descriptor(&b).unwrap().model, "");
        assert!(parse_device_descriptor(&b[..20]).is_err());
    }

    #[test]
    fn bus_types_map() {
        assert_eq!(bus_from_storage(17), DiskBus::Nvme);
        assert_eq!(bus_from_storage(11), DiskBus::Sata);
        assert_eq!(bus_from_storage(3), DiskBus::Sata); // ATA
        assert_eq!(bus_from_storage(7), DiskBus::Usb);
        assert_eq!(bus_from_storage(1), DiskBus::Scsi);
        assert_eq!(bus_from_storage(10), DiskBus::Scsi); // SAS
        assert_eq!(bus_from_storage(12), DiskBus::Sd);
        assert_eq!(bus_from_storage(13), DiskBus::Sd); // MMC
        assert_eq!(bus_from_storage(14), DiskBus::Virtual);
        assert_eq!(bus_from_storage(15), DiskBus::Virtual); // file-backed virtual (VHD)
        assert_eq!(bus_from_storage(0), DiskBus::Unknown);
        assert_eq!(bus_from_storage(8), DiskBus::Unknown); // RAID
    }

    #[test]
    fn alignment_and_trim_descriptors() {
        let mut b = vec![0u8; 28];
        put32(&mut b, 16, 512);
        put32(&mut b, 20, 4096);
        assert_eq!(parse_alignment_descriptor(&b), Some((512, 4096)));
        assert_eq!(parse_alignment_descriptor(&b[..18]), None);
        put32(&mut b, 16, 0);
        assert_eq!(parse_alignment_descriptor(&b), None);
        let mut t = vec![0u8; 12];
        assert_eq!(parse_trim_descriptor(&t), Some(false));
        t[8] = 1;
        assert_eq!(parse_trim_descriptor(&t), Some(true));
        assert_eq!(parse_trim_descriptor(&t[..8]), None);
    }

    #[test]
    fn encoders_and_small_parsers_match_windows_sys() {
        assert_eq!(size_of::<STORAGE_PROPERTY_QUERY>(), 12);
        let q = encode_property_query(StorageAccessAlignmentProperty);
        assert_eq!(u32_at(&q, 0), Some(6));
        assert_eq!(u32_at(&q, 4), Some(PropertyStandardQuery as u32));

        assert_eq!(size_of::<SET_DISK_ATTRIBUTES>(), 40);
        assert_eq!(offset_of!(SET_DISK_ATTRIBUTES, Persist), 4);
        assert_eq!(offset_of!(SET_DISK_ATTRIBUTES, Attributes), 8);
        assert_eq!(offset_of!(SET_DISK_ATTRIBUTES, AttributesMask), 16);
        let a = encode_set_disk_attributes(DISK_ATTRIBUTE_OFFLINE, DISK_ATTRIBUTE_OFFLINE, false);
        assert_eq!(u32_at(&a, 0), Some(40));
        assert_eq!(a[4], 0);
        assert_eq!(u64_at(&a, 8), Some(1));
        assert_eq!(u64_at(&a, 16), Some(1));
        assert_eq!(encode_set_disk_attributes(0, 1, true)[4], 1);

        assert_eq!(size_of::<SHRINK_VOLUME_INFORMATION>(), 24);
        assert_eq!(offset_of!(SHRINK_VOLUME_INFORMATION, NewNumberOfSectors), 16);
        let s = encode_shrink_request(ShrinkPrepare, 123_456);
        assert_eq!(u32_at(&s, 0), Some(1));
        assert_eq!(u64_at(&s, 8), Some(0));
        assert_eq!(u64_at(&s, 16), Some(123_456));

        assert_eq!(offset_of!(DISK_GEOMETRY_EX, DiskSize), 24);
        assert_eq!(offset_of!(DISK_GEOMETRY, BytesPerSector), 20);
        let mut g = vec![0u8; 32];
        put32(&mut g, 20, 4096);
        put64(&mut g, 24, 1 << 40);
        assert_eq!(parse_geometry_ex(&g), Some((4096, 1 << 40)));
        assert_eq!(parse_geometry_ex(&g[..30]), None);

        assert_eq!(offset_of!(GET_DISK_ATTRIBUTES, Attributes), 8);
        let mut at = vec![0u8; 16];
        put64(&mut at, 8, 3);
        assert_eq!(parse_disk_attributes(&at), Some(3));
        assert_eq!(parse_length_info(&(5u64 << 30).to_le_bytes()), Some(5 << 30));
        assert_eq!(parse_length_info(&[0u8; 4]), None);
    }

    #[test]
    fn fs_names_map() {
        assert_eq!(fs_from_name("NTFS"), Some(FsKind::Ntfs));
        assert_eq!(fs_from_name("FAT32"), Some(FsKind::Fat32));
        assert_eq!(fs_from_name("FAT"), Some(FsKind::Fat16));
        assert_eq!(fs_from_name("exFAT"), Some(FsKind::ExFat));
        assert_eq!(fs_from_name("ReFS"), Some(FsKind::ReFs));
        assert_eq!(fs_from_name("ntfs"), Some(FsKind::Ntfs));
        assert_eq!(fs_from_name("RAW"), None);
        assert_eq!(fs_from_name(""), None);
        assert_eq!(fs_from_name("CDFS"), Some(FsKind::Iso9660));
        assert_eq!(fs_from_name("UDF"), Some(FsKind::Udf));
    }
}
