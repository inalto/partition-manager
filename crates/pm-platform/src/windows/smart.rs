//! S.M.A.R.T. health data: NVMe health log (storage protocol query) and ATA attributes
//! (`SMART_RCV_DRIVE_DATA`).
//!
//! Both transports are plain `DeviceIoControl` calls with byte buffers (through the safe
//! [`ioctl`] wrappers, so this module has no `unsafe`); the buffers are decoded by pure parsers
//! that are unit-tested with hand-built data. Querying usually needs elevation (ATA SMART
//! needs read+write access to the drive); when nothing can be read, [`smart`] returns `Ok(None)`.

use super::disk::disk_path;
use super::error::platform_error;
use super::ioctl::{self, Code, Handle, GENERIC_READ, GENERIC_WRITE};
use crate::traits::{SmartAttr, SmartData};
use pm_core::Result;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
use windows_sys::Win32::System::Ioctl::IOCTL_STORAGE_QUERY_PROPERTY;

/// `STORAGE_PROPERTY_ID::StorageAdapterProtocolSpecificProperty`.
const STORAGE_ADAPTER_PROTOCOL_SPECIFIC: u32 = 49;
/// `STORAGE_PROPERTY_ID::StorageDeviceProtocolSpecificProperty`.
const STORAGE_DEVICE_PROTOCOL_SPECIFIC: u32 = 50;
/// `STORAGE_PROTOCOL_TYPE::ProtocolTypeNvme`.
const PROTOCOL_TYPE_NVME: u32 = 3;
/// `STORAGE_PROTOCOL_NVME_DATA_TYPE::NVMeDataTypeLogPage`.
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
/// NVMe log page 02h: SMART / Health Information.
const NVME_LOG_HEALTH: u32 = 2;
/// `sizeof(STORAGE_PROTOCOL_SPECIFIC_DATA)`.
const PROTOCOL_DATA_LEN: usize = 40;
/// Offset of `STORAGE_PROPERTY_QUERY::AdditionalParameters` (after PropertyId and QueryType) and
/// of `STORAGE_PROTOCOL_DATA_DESCRIPTOR::ProtocolSpecificData` (after Version and Size).
const HEADER_LEN: usize = 8;
const NVME_LOG_LEN: usize = 512;

/// `SMART_RCV_DRIVE_DATA` = `CTL_CODE(IOCTL_DISK_BASE, 0x0022, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS)`.
const SMART_RCV_DRIVE_DATA: u32 = 0x0007_C088;
const SMART_READ_DATA: u8 = 0xD0;
const SMART_READ_THRESHOLDS: u8 = 0xD1;
/// `sizeof(SENDCMDINPARAMS) - 1` (without the trailing 1-byte buffer).
const SENDCMD_IN_LEN: usize = 32;
/// Offset of `SENDCMDOUTPARAMS::bBuffer` (4-byte size + 12-byte `DRIVERSTATUS`).
const SENDCMD_OUT_HEADER: usize = 16;
const ATA_SECTOR: usize = 512;
const ATA_ATTR_COUNT: usize = 30;
const ATA_ATTR_LEN: usize = 12;

fn le_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

/// Little-endian `u128` at `o`, saturated to `u64`.
fn le_u128_sat(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 16];
    a.copy_from_slice(&b[o..o + 16]);
    u64::try_from(u128::from_le_bytes(a)).unwrap_or(u64::MAX)
}

fn nvme_attr(id: u8, name: &str, raw: u64) -> SmartAttr {
    SmartAttr { id, name: name.to_string(), raw, ..Default::default() }
}

/// Decode the 512-byte NVMe SMART / Health Information log (log page 02h).
///
/// Attributes are synthesized with `id` = the field's byte offset in the log page (these never
/// collide with the ATA ids the summary treats specially).
pub fn parse_nvme_smart_log(log: &[u8; 512]) -> SmartData {
    let critical = log[0];
    let kelvin = le_u16(log, 1);
    let spare = log[3];
    let spare_threshold = log[4];
    let used = log[5];
    let hours = le_u128_sat(log, 128);
    let attrs = vec![
        nvme_attr(0, "Critical warning", critical as u64),
        SmartAttr {
            id: 3,
            name: "Available spare".into(),
            value: spare,
            worst: spare,
            threshold: spare_threshold,
            raw: spare as u64,
        },
        nvme_attr(32, "Data units read", le_u128_sat(log, 32)),
        nvme_attr(48, "Data units written", le_u128_sat(log, 48)),
        nvme_attr(112, "Power cycles", le_u128_sat(log, 112)),
        nvme_attr(128, "Power-on hours", hours),
        nvme_attr(144, "Unsafe shutdowns", le_u128_sat(log, 144)),
        nvme_attr(160, "Media and data integrity errors", le_u128_sat(log, 160)),
    ];
    SmartData {
        healthy: Some(critical == 0),
        temperature_c: (kelvin > 273).then(|| kelvin as u32 - 273),
        power_on_hours: Some(hours),
        nvme_percent_used: Some(used),
        attrs,
    }
}

/// Well-known ATA attribute names; others are shown as "Attribute N".
fn ata_attr_name(id: u8) -> String {
    match id {
        1 => "Read error rate",
        3 => "Spin-up time",
        4 => "Start/stop count",
        5 => "Reallocated sectors",
        7 => "Seek error rate",
        9 => "Power-on hours",
        10 => "Spin retry count",
        12 => "Power cycles",
        177 => "Wear leveling count",
        187 => "Reported uncorrectable errors",
        188 => "Command timeout",
        190 => "Airflow temperature",
        192 => "Power-off retract count",
        193 => "Load cycle count",
        194 => "Temperature",
        196 => "Reallocation events",
        197 => "Current pending sectors",
        198 => "Offline uncorrectable sectors",
        199 => "UDMA CRC errors",
        _ => return format!("Attribute {id}"),
    }
    .to_string()
}

/// The non-empty 12-byte entries of a SMART data / threshold sector.
fn ata_entries(sector: &[u8]) -> Vec<&[u8]> {
    (0..ATA_ATTR_COUNT)
        .map(|i| 2 + i * ATA_ATTR_LEN)
        .filter(|&o| o + ATA_ATTR_LEN <= sector.len() && sector[o] != 0)
        .map(|o| &sector[o..o + ATA_ATTR_LEN])
        .collect()
}

/// Decode ATA SMART data: `attrs` is the 512-byte SMART READ DATA sector (2-byte revision, then
/// 30 twelve-byte entries `id, flags u16, value, worst, raw[6], reserved`), `thresholds` the
/// 512-byte SMART READ THRESHOLDS sector (entries `id, threshold, ...`).
///
/// `healthy` is `None` without thresholds; otherwise `false` when any attribute with a non-zero
/// threshold has `value <= threshold`. Temperature is the low byte of attribute 194 (or 190);
/// power-on hours the low 32 bits of attribute 9's raw value.
pub fn parse_ata_smart(attrs: &[u8], thresholds: Option<&[u8]>) -> SmartData {
    let mut limits = [0u8; 256];
    if let Some(t) = thresholds {
        for e in ata_entries(t) {
            limits[e[0] as usize] = e[1];
        }
    }
    let mut data = SmartData::default();
    let mut failing = false;
    for e in ata_entries(attrs) {
        let mut raw_bytes = [0u8; 8];
        raw_bytes[..6].copy_from_slice(&e[5..11]);
        let a = SmartAttr {
            id: e[0],
            name: ata_attr_name(e[0]),
            value: e[3],
            worst: e[4],
            threshold: limits[e[0] as usize],
            raw: u64::from_le_bytes(raw_bytes),
        };
        if a.threshold > 0 && a.value <= a.threshold {
            failing = true;
        }
        match a.id {
            9 => data.power_on_hours = Some(a.raw & 0xFFFF_FFFF),
            194 => data.temperature_c = Some((a.raw & 0xFF) as u32),
            190 if data.temperature_c.is_none() => data.temperature_c = Some((a.raw & 0xFF) as u32),
            _ => {}
        }
        data.attrs.push(a);
    }
    data.healthy = thresholds.map(|_| !failing);
    data
}

/// `STORAGE_PROPERTY_QUERY` + `STORAGE_PROTOCOL_SPECIFIC_DATA` requesting the NVMe health log.
fn encode_nvme_health_query(property_id: u32) -> Vec<u8> {
    let mut b = vec![0u8; HEADER_LEN + PROTOCOL_DATA_LEN];
    b[0..4].copy_from_slice(&property_id.to_le_bytes());
    // QueryType = PropertyStandardQuery (0)
    let p = HEADER_LEN;
    b[p..p + 4].copy_from_slice(&PROTOCOL_TYPE_NVME.to_le_bytes());
    b[p + 4..p + 8].copy_from_slice(&NVME_DATA_TYPE_LOG_PAGE.to_le_bytes());
    b[p + 8..p + 12].copy_from_slice(&NVME_LOG_HEALTH.to_le_bytes()); // ProtocolDataRequestValue
    // ProtocolDataRequestSubValue = 0
    b[p + 16..p + 20].copy_from_slice(&(PROTOCOL_DATA_LEN as u32).to_le_bytes()); // ProtocolDataOffset
    b[p + 20..p + 24].copy_from_slice(&(NVME_LOG_LEN as u32).to_le_bytes()); // ProtocolDataLength
    b
}

/// Extract the log page from a `STORAGE_PROTOCOL_DATA_DESCRIPTOR` reply.
fn nvme_log_from_reply(reply: &[u8]) -> Option<[u8; 512]> {
    let u32_at = |o: usize| reply.get(o..o + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]));
    let offset = u32_at(HEADER_LEN + 16)? as usize;
    let length = u32_at(HEADER_LEN + 20)? as usize;
    if length < NVME_LOG_LEN {
        return None;
    }
    let start = HEADER_LEN.checked_add(offset)?;
    reply.get(start..start.checked_add(NVME_LOG_LEN)?)?.try_into().ok()
}

fn query_nvme(h: &Handle) -> std::result::Result<SmartData, Code> {
    let mut last = 0;
    for prop in [STORAGE_DEVICE_PROTOCOL_SPECIFIC, STORAGE_ADAPTER_PROTOCOL_SPECIFIC] {
        let mut out = vec![0u8; HEADER_LEN + PROTOCOL_DATA_LEN + NVME_LOG_LEN];
        match ioctl::ioctl(h, IOCTL_STORAGE_QUERY_PROPERTY, &encode_nvme_health_query(prop), &mut out) {
            Ok(n) => match nvme_log_from_reply(&out[..n]) {
                Some(log) => return Ok(parse_nvme_smart_log(&log)),
                None => last = windows_sys::Win32::Foundation::ERROR_INVALID_DATA,
            },
            Err(c) => last = c,
        }
    }
    Err(last)
}

/// `SENDCMDINPARAMS` for a SMART sub-command on `drive`.
fn encode_smart_cmd(feature: u8, drive: u32) -> [u8; SENDCMD_IN_LEN] {
    let mut b = [0u8; SENDCMD_IN_LEN];
    b[0..4].copy_from_slice(&(ATA_SECTOR as u32).to_le_bytes()); // cBufferSize
    b[4] = feature; // bFeaturesReg
    b[5] = 1; // bSectorCountReg
    b[6] = 1; // bSectorNumberReg
    b[7] = 0x4F; // bCylLowReg
    b[8] = 0xC2; // bCylHighReg
    b[9] = 0xA0 | (((drive & 1) as u8) << 4); // bDriveHeadReg
    b[10] = 0xB0; // bCommandReg: SMART
    b[12] = drive as u8; // bDriveNumber
    b
}

fn smart_sector(h: &Handle, feature: u8, drive: u32) -> std::result::Result<Vec<u8>, Code> {
    let mut out = vec![0u8; SENDCMD_OUT_HEADER + ATA_SECTOR];
    let n = ioctl::ioctl(h, SMART_RCV_DRIVE_DATA, &encode_smart_cmd(feature, drive), &mut out)?;
    // DRIVERSTATUS.bDriverError is at offset 4
    if n < out.len() || out[4] != 0 {
        return Err(windows_sys::Win32::Foundation::ERROR_IO_DEVICE);
    }
    Ok(out[SENDCMD_OUT_HEADER..].to_vec())
}

fn query_ata(h: &Handle, drive: u32) -> std::result::Result<SmartData, Code> {
    let attrs = smart_sector(h, SMART_READ_DATA, drive)?;
    let thresholds = smart_sector(h, SMART_READ_THRESHOLDS, drive)
        .map_err(|c| log::debug!("SMART thresholds unavailable (code {c})"))
        .ok();
    Ok(parse_ata_smart(&attrs, thresholds.as_deref()))
}

/// Try `query` on a fresh handle for each access mask in turn (a handle that opens may still be
/// refused the IOCTL, e.g. without write access), returning the first success or the error codes.
fn query_with<F>(path: &str, accesses: &[u32], query: F) -> std::result::Result<SmartData, Vec<Code>>
where
    F: Fn(&Handle) -> std::result::Result<SmartData, Code>,
{
    let mut codes = Vec::new();
    for &access in accesses {
        match ioctl::open(path, access) {
            Ok(h) => match query(&h) {
                Ok(d) => return Ok(d),
                Err(c) => codes.push(c),
            },
            Err(c) => codes.push(c),
        }
    }
    Err(codes)
}

/// SMART data of `PhysicalDrive<disk>`: NVMe health log first, then ATA SMART. Each query is
/// retried with every access mask (query-only, read, read+write). `Ok(None)` when the drive
/// exposes neither (or access was denied; the codes are logged), `Err` when the disk does not
/// exist.
pub fn smart(disk: u32) -> Result<Option<SmartData>> {
    let path = disk_path(disk);
    if let Err(c) = ioctl::open(&path, 0) {
        if c == ERROR_FILE_NOT_FOUND || c == ERROR_PATH_NOT_FOUND {
            return Err(platform_error(c, &format!("open {path}")));
        }
    }
    let nvme = match query_with(&path, &[0, GENERIC_READ, GENERIC_READ | GENERIC_WRITE], query_nvme) {
        Ok(d) => return Ok(Some(d)),
        Err(codes) => codes,
    };
    let ata = match query_with(&path, &[GENERIC_READ | GENERIC_WRITE, GENERIC_READ], |h| query_ata(h, disk)) {
        Ok(d) => return Ok(Some(d)),
        Err(codes) => codes,
    };
    log::info!("no SMART data for {path}: NVMe query codes {nvme:?}, ATA query codes {ata:?}");
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};
    use windows_sys::Win32::System::Ioctl::{
        SENDCMDINPARAMS, SENDCMDOUTPARAMS, STORAGE_PROTOCOL_SPECIFIC_DATA,
    };

    #[test]
    fn struct_layouts_match_windows_sys() {
        assert_eq!(size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>(), PROTOCOL_DATA_LEN);
        assert_eq!(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataOffset), 16);
        assert_eq!(offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataLength), 20);
        assert_eq!(offset_of!(SENDCMDINPARAMS, bDriveNumber), 12);
        assert_eq!(offset_of!(SENDCMDINPARAMS, bBuffer), SENDCMD_IN_LEN);
        assert_eq!(offset_of!(SENDCMDOUTPARAMS, DriverStatus), 4);
        assert_eq!(offset_of!(SENDCMDOUTPARAMS, bBuffer), SENDCMD_OUT_HEADER);
        assert_eq!(windows_sys::Win32::System::Ioctl::SMART_RCV_DRIVE_DATA, SMART_RCV_DRIVE_DATA);
    }

    fn nvme_log() -> [u8; 512] {
        let mut l = [0u8; 512];
        l[0] = 0; // critical warning
        l[1..3].copy_from_slice(&(273u16 + 38).to_le_bytes()); // 38 C
        l[3] = 97; // available spare
        l[4] = 10; // spare threshold
        l[5] = 7; // percentage used
        l[32..40].copy_from_slice(&1_000_000u64.to_le_bytes());
        l[48..56].copy_from_slice(&2_000_000u64.to_le_bytes());
        l[112..120].copy_from_slice(&321u64.to_le_bytes());
        l[128..136].copy_from_slice(&4567u64.to_le_bytes());
        l[144..152].copy_from_slice(&12u64.to_le_bytes());
        l[160..168].copy_from_slice(&3u64.to_le_bytes());
        l
    }

    #[test]
    fn nvme_log_is_decoded() {
        let d = parse_nvme_smart_log(&nvme_log());
        assert_eq!(d.healthy, Some(true));
        assert_eq!(d.temperature_c, Some(38));
        assert_eq!(d.power_on_hours, Some(4567));
        assert_eq!(d.nvme_percent_used, Some(7));
        let by = |name: &str| d.attrs.iter().find(|a| a.name == name).unwrap_or_else(|| panic!("{name}"));
        let spare = by("Available spare");
        assert_eq!((spare.value, spare.threshold, spare.raw), (97, 10, 97));
        assert_eq!(by("Data units read").raw, 1_000_000);
        assert_eq!(by("Data units written").raw, 2_000_000);
        assert_eq!(by("Power cycles").raw, 321);
        assert_eq!(by("Unsafe shutdowns").raw, 12);
        assert_eq!(by("Media and data integrity errors").raw, 3);
        // ids are distinct
        let mut ids: Vec<u8> = d.attrs.iter().map(|a| a.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), d.attrs.len());
    }

    #[test]
    fn nvme_critical_warning_and_saturation() {
        let mut l = nvme_log();
        l[0] = 0b100; // media in read-only mode
        l[1..3].copy_from_slice(&0u16.to_le_bytes());
        l[136..144].fill(0xFF); // power-on hours high half: u128 beyond u64
        let d = parse_nvme_smart_log(&l);
        assert_eq!(d.healthy, Some(false));
        assert_eq!(d.temperature_c, None);
        assert_eq!(d.power_on_hours, Some(u64::MAX));
    }

    #[test]
    fn nvme_query_and_reply_encoding() {
        let q = encode_nvme_health_query(STORAGE_DEVICE_PROTOCOL_SPECIFIC);
        assert_eq!(q.len(), 48);
        assert_eq!(u32::from_le_bytes(q[0..4].try_into().unwrap()), 50);
        assert_eq!(u32::from_le_bytes(q[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(q[8..12].try_into().unwrap()), 3); // NVMe
        assert_eq!(u32::from_le_bytes(q[12..16].try_into().unwrap()), 2); // log page
        assert_eq!(u32::from_le_bytes(q[16..20].try_into().unwrap()), 2); // SMART/Health
        assert_eq!(u32::from_le_bytes(q[24..28].try_into().unwrap()), 40);
        assert_eq!(u32::from_le_bytes(q[28..32].try_into().unwrap()), 512);

        // reply: descriptor header, the protocol data echoed back, then the log
        let mut reply = q.clone();
        reply.extend_from_slice(&nvme_log());
        let log = nvme_log_from_reply(&reply).expect("log extracted");
        assert_eq!(log, nvme_log());
        assert!(nvme_log_from_reply(&reply[..100]).is_none());
        assert!(nvme_log_from_reply(&[0u8; 8]).is_none());
    }

    fn put_attr(sector: &mut [u8], slot: usize, id: u8, value: u8, worst: u8, raw: u64) {
        let o = 2 + slot * ATA_ATTR_LEN;
        sector[o] = id;
        sector[o + 3] = value;
        sector[o + 4] = worst;
        sector[o + 5..o + 11].copy_from_slice(&raw.to_le_bytes()[..6]);
    }

    fn put_threshold(sector: &mut [u8], slot: usize, id: u8, threshold: u8) {
        let o = 2 + slot * ATA_ATTR_LEN;
        sector[o] = id;
        sector[o + 1] = threshold;
    }

    #[test]
    fn ata_attributes_are_decoded() {
        let mut a = [0u8; 512];
        let mut t = [0u8; 512];
        put_attr(&mut a, 0, 5, 100, 100, 8);
        put_attr(&mut a, 1, 9, 95, 95, 0x1_0000_1234); // upper bits ignored for hours
        put_attr(&mut a, 2, 194, 62, 50, 0x0001_0000_0026); // 38 C in the low byte
        put_attr(&mut a, 3, 197, 100, 100, 2);
        put_attr(&mut a, 4, 250, 80, 80, 0xAB_CDEF);
        for (i, (id, th)) in [(5u8, 36u8), (9, 0), (194, 0), (197, 0), (250, 0)].into_iter().enumerate() {
            put_threshold(&mut t, i, id, th);
        }
        let d = parse_ata_smart(&a, Some(&t));
        assert_eq!(d.healthy, Some(true));
        assert_eq!(d.temperature_c, Some(38));
        assert_eq!(d.power_on_hours, Some(0x1234));
        assert_eq!(d.nvme_percent_used, None);
        assert_eq!(d.attrs.len(), 5);
        let realloc = &d.attrs[0];
        assert_eq!((realloc.id, realloc.name.as_str()), (5, "Reallocated sectors"));
        assert_eq!((realloc.value, realloc.worst, realloc.threshold, realloc.raw), (100, 100, 36, 8));
        assert_eq!(d.attrs[3].name, "Current pending sectors");
        assert_eq!(d.attrs[4].name, "Attribute 250");
        assert_eq!(d.attrs[4].raw, 0xAB_CDEF);
    }

    #[test]
    fn ata_failing_attribute_and_missing_thresholds() {
        let mut a = [0u8; 512];
        let mut t = [0u8; 512];
        put_attr(&mut a, 0, 5, 30, 30, 5000); // value 30 <= threshold 36
        put_attr(&mut a, 1, 1, 100, 100, 0); // threshold 0 never fails
        put_threshold(&mut t, 0, 5, 36);
        put_threshold(&mut t, 1, 1, 0);
        let d = parse_ata_smart(&a, Some(&t));
        assert_eq!(d.healthy, Some(false));
        // equal to the threshold counts as failing
        put_attr(&mut a, 0, 5, 36, 36, 0);
        assert_eq!(parse_ata_smart(&a, Some(&t)).healthy, Some(false));
        put_attr(&mut a, 0, 5, 37, 37, 0);
        assert_eq!(parse_ata_smart(&a, Some(&t)).healthy, Some(true));
        // without thresholds nothing can be concluded
        assert_eq!(parse_ata_smart(&a, None).healthy, None);
        // airflow temperature is the fallback, 194 wins when present
        put_attr(&mut a, 2, 190, 60, 60, 41);
        assert_eq!(parse_ata_smart(&a, None).temperature_c, Some(41));
        put_attr(&mut a, 3, 194, 60, 60, 35);
        assert_eq!(parse_ata_smart(&a, None).temperature_c, Some(35));
        // short buffers do not panic
        assert!(parse_ata_smart(&a[..10], None).attrs.is_empty());
    }

    #[test]
    fn smart_command_encoding() {
        let c = encode_smart_cmd(SMART_READ_DATA, 0);
        assert_eq!(u32::from_le_bytes(c[0..4].try_into().unwrap()), 512);
        assert_eq!(&c[4..11], &[0xD0, 1, 1, 0x4F, 0xC2, 0xA0, 0xB0]);
        assert_eq!(c[12], 0);
        let c = encode_smart_cmd(SMART_READ_THRESHOLDS, 3);
        assert_eq!(c[4], 0xD1);
        assert_eq!(c[9], 0xB0);
        assert_eq!(c[12], 3);
    }
}
