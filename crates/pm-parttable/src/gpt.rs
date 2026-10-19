//! GUID Partition Table: parsing (with backup fallback), writing, health checks and repair.
//!
//! On disk everything is addressed in sectors of `dev.sector_size()`; the [`Table`] model uses
//! bytes. The writer always emits the canonical layout of 128 entries x 128 bytes (16 KiB):
//! protective MBR at LBA 0, primary header at LBA 1, primary entries from LBA 2, backup entries
//! just before the backup header, backup header in the last sector.

use crate::bytes::{rd_u32, rd_u64, wr_u32, wr_u64};
use crate::mbr;
use crate::table::{Entry, Table};
use pm_core::{BlockDevice, Error, PartitionType, Result, TableKind};
use uuid::Uuid;

const SIGNATURE: &[u8; 8] = b"EFI PART";
const REVISION: u32 = 0x0001_0000;
const HEADER_SIZE: usize = 92;
/// Entry slots written by [`write_gpt`].
const WRITE_ENTRIES: u32 = 128;
const WRITE_ENTRY_SIZE: u32 = 128;
const WRITE_ARRAY_BYTES: u64 = WRITE_ENTRIES as u64 * WRITE_ENTRY_SIZE as u64;
/// Larger arrays are accepted when reading (only non-empty entries are kept).
const MAX_READ_ENTRIES: u32 = 1024;
const MAX_READ_ENTRY_SIZE: u32 = 4096;
const NAME_UNITS: usize = 36;
const BOOTCODE_LEN: usize = 440;
const MBR_ENTRY_OFF: usize = 0x1BE;
const TYPE_PROTECTIVE: u8 = 0xEE;

/// Which copies of the GPT are usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GptHealth {
    /// Primary header (LBA 1) is valid and its entry array matches the array CRC.
    pub primary_ok: bool,
    /// Backup header (last LBA) is valid and its entry array matches the array CRC.
    pub backup_ok: bool,
    /// At least one header is valid and every valid header's entry array matches its CRC.
    pub entries_crc_ok: bool,
}

/// A decoded, CRC-checked GPT header (LBAs in sectors, `last_usable` inclusive).
struct Header {
    first_usable: u64,
    last_usable: u64,
    disk_guid: Uuid,
    entries_lba: u64,
    num_entries: u32,
    entry_size: u32,
    entries_crc: u32,
}

/// One header plus its entry array.
struct Side {
    hdr: Header,
    array: Vec<u8>,
    array_ok: bool,
}

type SideResult<T> = std::result::Result<T, String>;

fn check_sector_size(dev: &dyn BlockDevice) -> Result<u64> {
    let ss = dev.sector_size();
    if ss < 512 {
        return Err(Error::Invalid(format!("unsupported sector size {ss}")));
    }
    Ok(ss as u64)
}

/// Read and validate the header stored at sector `lba` (which must be its own MyLBA).
fn read_header(dev: &mut dyn BlockDevice, lba: u64) -> SideResult<Header> {
    let ss = dev.sector_size() as u64;
    let off = lba.checked_mul(ss).ok_or("header offset overflows")?;
    if off.checked_add(ss).map_or(true, |end| end > dev.len()) {
        return Err(format!("header at LBA {lba} lies beyond the end of the device"));
    }
    let mut buf = vec![0u8; ss as usize];
    dev.read_at(off, &mut buf).map_err(|e| format!("cannot read LBA {lba}: {e}"))?;
    if &buf[..8] != SIGNATURE {
        return Err(format!("no GPT signature at LBA {lba}"));
    }
    let header_size = rd_u32(&buf, 12) as usize;
    if header_size < HEADER_SIZE || header_size > ss as usize {
        return Err(format!("invalid GPT header size {header_size}"));
    }
    let stored_crc = rd_u32(&buf, 16);
    let mut copy = buf[..header_size].to_vec();
    copy[16..20].fill(0);
    if crc32fast::hash(&copy) != stored_crc {
        return Err(format!("GPT header CRC mismatch at LBA {lba}"));
    }
    let revision = rd_u32(&buf, 8);
    if revision != REVISION {
        return Err(format!(
            "GPT header revision {revision:#010x} is not supported (expected {REVISION:#010x})"
        ));
    }
    if rd_u64(&buf, 24) != lba {
        return Err(format!("GPT header at LBA {lba} claims MyLBA {}", rd_u64(&buf, 24)));
    }
    let num_entries = rd_u32(&buf, 80);
    let entry_size = rd_u32(&buf, 84);
    if num_entries == 0 || num_entries > MAX_READ_ENTRIES {
        return Err(format!("unsupported number of GPT entries {num_entries}"));
    }
    if entry_size < 128 || entry_size % 8 != 0 || entry_size > MAX_READ_ENTRY_SIZE {
        return Err(format!("unsupported GPT entry size {entry_size}"));
    }
    let guid: [u8; 16] = buf[56..72].try_into().unwrap();
    Ok(Header {
        first_usable: rd_u64(&buf, 40),
        last_usable: rd_u64(&buf, 48),
        disk_guid: Uuid::from_bytes_le(guid),
        entries_lba: rd_u64(&buf, 72),
        num_entries,
        entry_size,
        entries_crc: rd_u32(&buf, 88),
    })
}

/// Header at `lba` plus its entry array; the array being unreadable or corrupt is not an error
/// here (`array_ok` is false).
fn load_side(dev: &mut dyn BlockDevice, lba: u64) -> SideResult<Side> {
    let hdr = read_header(dev, lba)?;
    let ss = dev.sector_size() as u64;
    let size = hdr.num_entries as u64 * hdr.entry_size as u64;
    let in_range = hdr
        .entries_lba
        .checked_mul(ss)
        .and_then(|off| off.checked_add(size).map(|end| (off, end)))
        .filter(|&(_, end)| end <= dev.len());
    let (array, array_ok) = match in_range {
        Some((off, _)) => {
            let mut array = vec![0u8; size as usize];
            match dev.read_at(off, &mut array) {
                Ok(()) => {
                    let ok = crc32fast::hash(&array) == hdr.entries_crc;
                    (array, ok)
                }
                Err(_) => (Vec::new(), false),
            }
        }
        None => (Vec::new(), false),
    };
    Ok(Side { hdr, array, array_ok })
}

/// Decode a fully valid side into a [`Table`].
fn build_table(dev: &mut dyn BlockDevice, side: &Side) -> SideResult<Table> {
    if !side.array_ok {
        return Err("GPT entry array is unreadable or fails its CRC".into());
    }
    let ss = dev.sector_size() as u64;
    let total = dev.len() / ss;
    let h = &side.hdr;
    if h.first_usable > h.last_usable || h.last_usable >= total {
        return Err(format!(
            "invalid usable range {}..={} on a disk of {total} sectors",
            h.first_usable, h.last_usable
        ));
    }
    let mut t = Table::empty(TableKind::Gpt, dev.len(), ss as u32);
    let mut s0 = vec![0u8; ss as usize];
    dev.read_at(0, &mut s0).map_err(|e| format!("cannot read sector 0: {e}"))?;
    t.bootcode = s0[..BOOTCODE_LEN].to_vec();
    t.mbr_signature = rd_u32(&s0, BOOTCODE_LEN);
    t.disk_guid = Some(h.disk_guid);
    t.gpt_first_usable = h.first_usable * ss;
    t.gpt_last_usable = (h.last_usable + 1) * ss;
    for slot in 0..h.num_entries as usize {
        let o = slot * h.entry_size as usize;
        let raw = &side.array[o..o + h.entry_size as usize];
        let type_guid = Uuid::from_bytes_le(raw[0..16].try_into().unwrap());
        if type_guid.is_nil() {
            continue;
        }
        let first = rd_u64(raw, 32);
        let last = rd_u64(raw, 40);
        if last < first || last >= total {
            return Err(format!("GPT entry {} has an invalid LBA range {first}..={last}", slot + 1));
        }
        let units: Vec<u16> = raw[56..56 + NAME_UNITS * 2]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        t.entries.push(Entry {
            number: slot as u32 + 1,
            start: first * ss,
            len: (last - first + 1) * ss,
            ptype: PartitionType::Gpt(type_guid),
            guid: Some(Uuid::from_bytes_le(raw[16..32].try_into().unwrap())),
            name: String::from_utf16_lossy(&units),
            attrs: rd_u64(raw, 48),
            logical: false,
        });
    }
    Ok(t)
}

fn side_table(dev: &mut dyn BlockDevice, lba: u64) -> SideResult<Table> {
    let side = load_side(dev, lba)?;
    build_table(dev, &side)
}

fn backup_lba(dev: &dyn BlockDevice) -> Option<u64> {
    let total = dev.len() / dev.sector_size().max(1) as u64;
    (total >= 3).then(|| total - 1)
}

/// True if LBA 1 holds a CRC-valid GPT header (used when sector 0 carries no MBR signature).
pub(crate) fn has_valid_primary_header(dev: &mut dyn BlockDevice) -> bool {
    dev.sector_size() >= 512 && read_header(dev, 1).is_ok()
}

fn parse_failure(primary: &str, backup: &str) -> Error {
    Error::Parse(format!("no valid GPT (primary: {primary}; backup: {backup})"))
}

/// Parse the GPT of `dev`: the primary copy, or the backup at the last LBA if the primary fails
/// its signature / CRC checks. `Error::Parse` if neither copy is valid.
pub fn parse_gpt(dev: &mut dyn BlockDevice) -> Result<Table> {
    check_sector_size(dev)?;
    let primary = match side_table(dev, 1) {
        Ok(t) => return Ok(t),
        Err(e) => e,
    };
    match backup_lba(dev) {
        Some(lba) => side_table(dev, lba).map_err(|b| parse_failure(&primary, &b)),
        None => Err(parse_failure(&primary, "device too small")),
    }
}

/// Parse only the backup copy at the last LBA (ignores the primary entirely).
pub(crate) fn parse_gpt_backup(dev: &mut dyn BlockDevice) -> Result<Table> {
    check_sector_size(dev)?;
    match backup_lba(dev) {
        Some(lba) => side_table(dev, lba).map_err(|b| Error::Parse(format!("no valid backup GPT: {b}"))),
        None => Err(Error::Parse("device too small for a backup GPT".into())),
    }
}

/// Inspect both GPT copies without modifying anything. A disk without any GPT reports all
/// fields false rather than an error.
///
/// "ok" means the copy is readable (valid header and entry-array CRCs, sane ranges). It does not
/// imply the table is writable: the repair functions additionally require `Table::validate` to
/// pass (for example no overlapping partitions) and fail with `Error::Invalid` otherwise.
pub fn gpt_health(dev: &mut dyn BlockDevice) -> Result<GptHealth> {
    check_sector_size(dev)?;
    let backup = backup_lba(dev);
    let mut headers_valid = 0;
    let mut arrays_ok = 0;
    let mut sides = [false, false];
    for (i, lba) in [Some(1u64), backup].into_iter().enumerate() {
        let Some(lba) = lba else { continue };
        if let Ok(side) = load_side(dev, lba) {
            headers_valid += 1;
            if side.array_ok {
                arrays_ok += 1;
                sides[i] = build_table(dev, &side).is_ok();
            }
        }
    }
    Ok(GptHealth {
        primary_ok: sides[0],
        backup_ok: sides[1],
        entries_crc_ok: headers_valid > 0 && arrays_ok == headers_valid,
    })
}

/// Refuse to rewrite a GPT that parses but would not pass `Table::validate`.
fn ensure_writable(t: &Table) -> Result<()> {
    t.validate()
        .map_err(|e| Error::Invalid(format!("source GPT is readable but not writable: {e}")))
}

/// Rewrite the backup GPT (entries + header) from a valid primary.
///
/// The primary must be readable (`Error::Parse` otherwise) and its table must pass
/// `Table::validate` (`Error::Invalid` otherwise, nothing is written): a copy that
/// [`gpt_health`] reports as ok can still be refused here.
pub fn gpt_repair_backup_from_primary(dev: &mut dyn BlockDevice) -> Result<()> {
    check_sector_size(dev)?;
    let t = side_table(dev, 1).map_err(|e| Error::Parse(format!("primary GPT is not usable: {e}")))?;
    ensure_writable(&t)?;
    write_gpt(dev, &t)
}

/// Rewrite the primary GPT (header + entries, protective MBR) from a valid backup.
///
/// The backup must be readable (`Error::Parse` otherwise) and its table must pass
/// `Table::validate` (`Error::Invalid` otherwise, nothing is written): a copy that
/// [`gpt_health`] reports as ok can still be refused here.
pub fn gpt_repair_primary_from_backup(dev: &mut dyn BlockDevice) -> Result<()> {
    let t = parse_gpt_backup(dev)?;
    ensure_writable(&t)?;
    write_gpt(dev, &t)
}

fn encode_entry(e: &Entry, ss: u64, out: &mut [u8]) -> Result<()> {
    let PartitionType::Gpt(type_guid) = e.ptype else {
        return Err(Error::Invalid(format!("partition {} has an MBR type in a GPT table", e.number)));
    };
    if type_guid.is_nil() {
        return Err(Error::Invalid(format!("partition {} has an empty type GUID", e.number)));
    }
    let guid = e
        .guid
        .ok_or_else(|| Error::Invalid(format!("partition {} has no unique GUID", e.number)))?;
    let units: Vec<u16> = e.name.encode_utf16().collect();
    if units.len() > NAME_UNITS {
        return Err(Error::Invalid(format!(
            "name of partition {} is {} UTF-16 units, at most {NAME_UNITS} fit",
            e.number,
            units.len()
        )));
    }
    out[0..16].copy_from_slice(&type_guid.to_bytes_le());
    out[16..32].copy_from_slice(&guid.to_bytes_le());
    wr_u64(out, 32, e.start / ss);
    wr_u64(out, 40, e.end() / ss - 1);
    wr_u64(out, 48, e.attrs);
    for (i, u) in units.iter().enumerate() {
        out[56 + i * 2..58 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_header(
    ss: usize,
    my_lba: u64,
    alt_lba: u64,
    first_usable: u64,
    last_usable: u64,
    disk_guid: Uuid,
    entries_lba: u64,
    entries_crc: u32,
) -> Vec<u8> {
    let mut b = vec![0u8; ss];
    b[..8].copy_from_slice(SIGNATURE);
    wr_u32(&mut b, 8, REVISION);
    wr_u32(&mut b, 12, HEADER_SIZE as u32);
    wr_u64(&mut b, 24, my_lba);
    wr_u64(&mut b, 32, alt_lba);
    wr_u64(&mut b, 40, first_usable);
    wr_u64(&mut b, 48, last_usable);
    b[56..72].copy_from_slice(&disk_guid.to_bytes_le());
    wr_u64(&mut b, 72, entries_lba);
    wr_u32(&mut b, 80, WRITE_ENTRIES);
    wr_u32(&mut b, 84, WRITE_ENTRY_SIZE);
    wr_u32(&mut b, 88, entries_crc);
    let crc = crc32fast::hash(&b[..HEADER_SIZE]);
    wr_u32(&mut b, 16, crc);
    b
}

/// Write `t` as a GPT: protective MBR (boot code and disk signature kept), primary header and
/// entries, backup entries and header at the end of the device.
///
/// Entries go to slot `number - 1` (1..=128), so partition numbers survive a round trip. Backup
/// copies are written first and the protective MBR last, so an interrupted write leaves a disk
/// whose GPT can still be recovered.
pub fn write_gpt(dev: &mut dyn BlockDevice, t: &Table) -> Result<()> {
    if t.kind != TableKind::Gpt {
        return Err(Error::Invalid("not a GPT table".into()));
    }
    let ss = check_sector_size(dev)?;
    if t.sector_size as u64 != ss {
        return Err(Error::Invalid(format!(
            "table sector size {} differs from device sector size {ss}",
            t.sector_size
        )));
    }
    if !t.bootcode.is_empty() && t.bootcode.len() != BOOTCODE_LEN {
        return Err(Error::Invalid(format!(
            "boot code is {} bytes, expected {BOOTCODE_LEN}",
            t.bootcode.len()
        )));
    }
    let disk_guid = t
        .disk_guid
        .ok_or_else(|| Error::Invalid("GPT table has no disk GUID".into()))?;
    t.validate()?;

    let total = dev.len() / ss;
    let array_sectors = WRITE_ARRAY_BYTES.div_ceil(ss);
    let too_small = || Error::Invalid(format!("device of {total} sectors is too small for a GPT"));
    // Primary: LBA 0 + header + array; backup: array + header; at least one usable sector.
    let backup_hdr = total.checked_sub(1).ok_or_else(too_small)?;
    let backup_entries = backup_hdr.checked_sub(array_sectors).ok_or_else(too_small)?;
    let min_first = 2 + array_sectors;
    if backup_entries < min_first + 1 {
        return Err(too_small());
    }
    if t.gpt_first_usable % ss != 0 || t.gpt_last_usable % ss != 0 {
        return Err(Error::Invalid("GPT usable range is not sector aligned".into()));
    }
    let (first_lba, end_lba) = (t.gpt_first_usable / ss, t.gpt_last_usable / ss);
    if first_lba < min_first || end_lba > backup_entries || first_lba >= end_lba {
        return Err(Error::Invalid(format!(
            "usable range LBA {first_lba}..{end_lba} does not fit between the GPT structures \
             ({min_first}..{backup_entries})"
        )));
    }

    let mut array = vec![0u8; WRITE_ARRAY_BYTES as usize];
    let mut used = [false; WRITE_ENTRIES as usize];
    for e in &t.entries {
        if e.number == 0 || e.number > WRITE_ENTRIES {
            return Err(Error::Invalid(format!(
                "partition number {} does not fit the {WRITE_ENTRIES} GPT slots",
                e.number
            )));
        }
        let slot = e.number as usize - 1;
        if std::mem::replace(&mut used[slot], true) {
            return Err(Error::Invalid(format!("partition number {} is used twice", e.number)));
        }
        let o = slot * WRITE_ENTRY_SIZE as usize;
        encode_entry(e, ss, &mut array[o..o + WRITE_ENTRY_SIZE as usize])?;
    }
    let crc = crc32fast::hash(&array);

    let primary = build_header(ss as usize, 1, backup_hdr, first_lba, end_lba - 1, disk_guid, 2, crc);
    let backup = build_header(
        ss as usize,
        backup_hdr,
        1,
        first_lba,
        end_lba - 1,
        disk_guid,
        backup_entries,
        crc,
    );

    let mut s0 = vec![0u8; ss as usize];
    if !t.bootcode.is_empty() {
        s0[..BOOTCODE_LEN].copy_from_slice(&t.bootcode);
    }
    wr_u32(&mut s0, BOOTCODE_LEN, t.mbr_signature);
    let o = MBR_ENTRY_OFF;
    s0[o + 1..o + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    s0[o + 4] = TYPE_PROTECTIVE;
    s0[o + 5..o + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    wr_u32(&mut s0, o + 8, 1);
    wr_u32(&mut s0, o + 12, (total - 1).min(u32::MAX as u64) as u32);
    s0[510] = 0x55;
    s0[511] = 0xAA;
    debug_assert!(mbr::is_protective_mbr(&s0));

    dev.write_at(backup_entries * ss, &array)?;
    dev.write_at(backup_hdr * ss, &backup)?;
    dev.write_at(2 * ss, &array)?;
    dev.write_at(ss, &primary)?;
    dev.write_at(0, &s0)?;
    dev.flush()
}
