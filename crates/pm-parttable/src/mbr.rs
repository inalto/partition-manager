//! MBR parsing and writing, including the extended-partition EBR chain.

use crate::bytes::{rd_u32, wr_u32};
use crate::table::{Entry, Table};
use pm_core::units::MIB;
use pm_core::{BlockDevice, Error, PartitionType, Result, TableKind};
use std::collections::HashSet;

const BOOTCODE_LEN: usize = 440;
const SIGNATURE_OFF: usize = 440;
const ENTRIES_OFF: usize = 0x1BE;
const ENTRY_LEN: usize = 16;
const BOOT_SIG_OFF: usize = 510;
const MAX_LOGICALS: usize = 128;
const TYPE_PROTECTIVE: u8 = 0xEE;
const STATUS_ACTIVE: u8 = 0x80;

/// A decoded 16-byte partition entry (LBA values in sectors).
struct RawEntry {
    status: u8,
    ptype: u8,
    start_lba: u32,
    len_lba: u32,
}

impl RawEntry {
    fn read(sector: &[u8], i: usize) -> RawEntry {
        let o = ENTRIES_OFF + i * ENTRY_LEN;
        RawEntry {
            status: sector[o],
            ptype: sector[o + 4],
            start_lba: rd_u32(sector, o + 8),
            len_lba: rd_u32(sector, o + 12),
        }
    }

    fn is_empty(&self) -> bool {
        self.ptype == 0 || self.len_lba == 0
    }
}

pub(crate) fn has_signature(sector: &[u8]) -> bool {
    sector.len() >= 512 && sector[BOOT_SIG_OFF] == 0x55 && sector[BOOT_SIG_OFF + 1] == 0xAA
}

/// True if `sector` is a valid MBR whose only partition entry is type 0xEE (GPT protective).
pub fn is_protective_mbr(sector: &[u8]) -> bool {
    if !has_signature(sector) {
        return false;
    }
    let used: Vec<RawEntry> = (0..4)
        .map(|i| RawEntry::read(sector, i))
        .filter(|e| e.ptype != 0)
        .collect();
    used.len() == 1 && used[0].ptype == TYPE_PROTECTIVE
}

/// CHS bytes `[head, sector | cylinder_hi << 6, cylinder_lo]` for a 255-head, 63-sector geometry.
/// LBAs beyond cylinder 1023 (about 8 GiB) clamp to `[0xFE, 0xFF, 0xFF]`.
pub fn chs_from_lba(lba: u64) -> [u8; 3] {
    const HEADS: u64 = 255;
    const SECTORS: u64 = 63;
    let cyl = lba / (HEADS * SECTORS);
    if cyl > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    let head = (lba / SECTORS) % HEADS;
    let sector = lba % SECTORS + 1;
    [
        head as u8,
        (sector as u8) | (((cyl >> 8) as u8) << 6),
        (cyl & 0xFF) as u8,
    ]
}

fn read_sector(dev: &mut dyn BlockDevice, offset: u64) -> Result<Vec<u8>> {
    let ss = dev.sector_size() as usize;
    if ss < 512 {
        return Err(Error::Invalid(format!("unsupported sector size {ss}")));
    }
    let mut buf = vec![0u8; ss];
    dev.read_at(offset, &mut buf)?;
    Ok(buf)
}

/// Parse the MBR at sector 0 and follow the EBR chain of the first extended partition.
///
/// Primaries get numbers 1..=4 by slot; logicals continue at 5 in chain order. The chain stops at
/// a missing EBR signature, an empty link entry, a repeated EBR offset or after 128 logicals.
pub fn parse_mbr(dev: &mut dyn BlockDevice) -> Result<Table> {
    let sector_size = dev.sector_size();
    let ss = sector_size as u64;
    let s0 = read_sector(dev, 0)?;
    if !has_signature(&s0) {
        return Err(Error::Parse("missing MBR boot signature 0x55AA".into()));
    }
    let mut t = Table::empty(TableKind::Mbr, dev.len(), sector_size);
    t.mbr_signature = rd_u32(&s0, SIGNATURE_OFF);
    t.bootcode = s0[..BOOTCODE_LEN].to_vec();

    let mut extended: Option<(u64, u8)> = None; // (start in bytes, type byte)
    for slot in 0..4 {
        let raw = RawEntry::read(&s0, slot);
        if raw.is_empty() {
            continue;
        }
        let e = Entry {
            number: slot as u32 + 1,
            start: raw.start_lba as u64 * ss,
            len: raw.len_lba as u64 * ss,
            ptype: PartitionType::Mbr(raw.ptype),
            guid: None,
            name: String::new(),
            attrs: (raw.status == STATUS_ACTIVE) as u64,
            logical: false,
        };
        if extended.is_none() && e.ptype.is_extended_mbr() {
            extended = Some((e.start, raw.ptype));
        }
        t.entries.push(e);
    }
    if let Some((ext_start, _)) = extended {
        parse_ebr_chain(dev, &mut t, ext_start, ss)?;
    }
    Ok(t)
}

fn parse_ebr_chain(dev: &mut dyn BlockDevice, t: &mut Table, ext_start: u64, ss: u64) -> Result<()> {
    let mut seen = HashSet::new();
    let mut ebr_off = ext_start;
    let mut count = 0usize;
    let mut number = 5u32;
    while count < MAX_LOGICALS && seen.insert(ebr_off) {
        if ebr_off.checked_add(ss).map_or(true, |end| end > dev.len()) {
            return Err(Error::Parse(format!(
                "EBR at byte offset {ebr_off} lies beyond the end of the device"
            )));
        }
        let s = read_sector(dev, ebr_off)?;
        if !has_signature(&s) {
            break;
        }
        let logical = RawEntry::read(&s, 0);
        let link = RawEntry::read(&s, 1);
        if !logical.is_empty() {
            t.entries.push(Entry {
                number,
                // Logical starts are relative to their own EBR.
                start: ebr_off + logical.start_lba as u64 * ss,
                len: logical.len_lba as u64 * ss,
                ptype: PartitionType::Mbr(logical.ptype),
                guid: None,
                name: String::new(),
                attrs: (logical.status == STATUS_ACTIVE) as u64,
                logical: true,
            });
            number += 1;
            count += 1;
        }
        if link.is_empty() {
            break;
        }
        // Link starts are relative to the extended partition start.
        ebr_off = ext_start + link.start_lba as u64 * ss;
    }
    Ok(())
}

/// Encode one partition entry. CHS is derived from the absolute `abs_start`/`len` (sectors);
/// `rel_start` is the start value stored on disk (relative for EBR entries).
fn put_entry(buf: &mut [u8], i: usize, status: u8, ptype: u8, abs_start: u64, rel_start: u64, len: u64) {
    let o = ENTRIES_OFF + i * ENTRY_LEN;
    buf[o] = status;
    buf[o + 1..o + 4].copy_from_slice(&chs_from_lba(abs_start));
    buf[o + 4] = ptype;
    buf[o + 5..o + 8].copy_from_slice(&chs_from_lba((abs_start + len).saturating_sub(1)));
    wr_u32(buf, o + 8, rel_start as u32);
    wr_u32(buf, o + 12, len as u32);
}

fn type_byte(e: &Entry) -> Result<u8> {
    match e.ptype {
        PartitionType::Mbr(b) => Ok(b),
        PartitionType::Gpt(_) => Err(Error::Invalid(format!(
            "partition {} has a GPT type in an MBR table",
            e.number
        ))),
    }
}

fn status_byte(e: &Entry) -> u8 {
    if e.attrs & 1 != 0 {
        STATUS_ACTIVE
    } else {
        0
    }
}

/// Write `t` as an MBR with an EBR chain for its logical partitions.
///
/// Primaries are placed by slot (`number - 1` when 1..=4 and free, otherwise the first free slot).
/// The first EBR lives at the extended partition start; every later EBR sits 1 MiB before its
/// logical partition, which must therefore start at least 1 MiB after the previous logical's end
/// (and after the extended start). Sector 0 is rewritten last and its reserved bytes are kept.
pub fn write_mbr(dev: &mut dyn BlockDevice, t: &Table) -> Result<()> {
    if t.kind != TableKind::Mbr {
        return Err(Error::Invalid("not an MBR table".into()));
    }
    if t.sector_size != dev.sector_size() {
        return Err(Error::Invalid(format!(
            "table sector size {} differs from device sector size {}",
            t.sector_size,
            dev.sector_size()
        )));
    }
    if t.bootcode.len() > BOOTCODE_LEN {
        return Err(Error::Invalid(format!(
            "boot code is {} bytes, at most {BOOTCODE_LEN} allowed",
            t.bootcode.len()
        )));
    }
    t.validate()?;
    let ss = t.sector_size as u64;
    let lba = |bytes: u64| bytes / ss;

    // Primary slots.
    let primaries: Vec<&Entry> = t.entries.iter().filter(|e| !e.logical).collect();
    let mut slots: [Option<&Entry>; 4] = [None; 4];
    let mut unplaced = Vec::new();
    for e in &primaries {
        let slot = e.number.checked_sub(1).map(|n| n as usize).filter(|&n| n < 4);
        match slot {
            Some(n) if slots[n].is_none() => slots[n] = Some(e),
            _ => unplaced.push(*e),
        }
    }
    for e in unplaced {
        let free = slots.iter().position(Option::is_none).expect("validate allows at most 4 primaries");
        slots[free] = Some(e);
    }

    // EBR chain: (ebr_offset, logical) pairs, plus the extended container.
    let ext = primaries.iter().find(|e| e.is_extended()).copied();
    let mut logicals: Vec<&Entry> = t.entries.iter().filter(|e| e.logical).collect();
    logicals.sort_by_key(|e| e.start);
    let mut ebrs: Vec<(u64, &Entry)> = Vec::new();
    if let Some(ext) = ext {
        for (i, l) in logicals.iter().enumerate() {
            if l.start < ext.start + MIB {
                return Err(Error::Invalid(format!(
                    "logical partition {} must start at least 1 MiB after the extended partition start",
                    l.number
                )));
            }
            let pos = if i == 0 { ext.start } else { l.start - MIB };
            if i > 0 && pos < logicals[i - 1].end() {
                return Err(Error::Invalid(format!(
                    "no room for the EBR of logical partition {} (needs 1 MiB after the previous logical)",
                    l.number
                )));
            }
            ebrs.push((pos, l));
        }
        let first_ebr = ext.start;
        for off in std::iter::once(first_ebr).chain(ebrs.iter().map(|(p, _)| *p)) {
            if off + ss > dev.len() {
                return Err(Error::Invalid(format!("EBR at byte offset {off} lies beyond the device")));
            }
        }
    }

    // Read sector 0 up front so a failure leaves the disk untouched.
    let mut s0 = read_sector(dev, 0)?;

    if let Some(ext) = ext {
        // Links reuse the container's own type byte (0x05, 0x0F or 0x85).
        let link_type = type_byte(ext)?;
        if ebrs.is_empty() {
            // Empty extended partition: a blank EBR keeps stale data from resurrecting logicals.
            let mut blank = vec![0u8; ss as usize];
            blank[BOOT_SIG_OFF] = 0x55;
            blank[BOOT_SIG_OFF + 1] = 0xAA;
            dev.write_at(ext.start, &blank)?;
        }
        for (i, (pos, l)) in ebrs.iter().enumerate() {
            let mut buf = vec![0u8; ss as usize];
            // The logical's start is relative to its own EBR.
            put_entry(
                &mut buf,
                0,
                status_byte(l),
                type_byte(l)?,
                lba(l.start),
                lba(l.start - *pos),
                lba(l.len),
            );
            if let Some((next_pos, next)) = ebrs.get(i + 1) {
                // The link covers the next EBR plus its logical; it is relative to the extended start.
                put_entry(
                    &mut buf,
                    1,
                    0,
                    link_type,
                    lba(*next_pos),
                    lba(*next_pos - ext.start),
                    lba(next.end() - *next_pos),
                );
            }
            buf[BOOT_SIG_OFF] = 0x55;
            buf[BOOT_SIG_OFF + 1] = 0xAA;
            dev.write_at(*pos, &buf)?;
        }
    }

    // Sector 0 last.
    s0[..BOOTCODE_LEN].fill(0);
    s0[..t.bootcode.len()].copy_from_slice(&t.bootcode);
    wr_u32(&mut s0, SIGNATURE_OFF, t.mbr_signature);
    s0[ENTRIES_OFF..BOOT_SIG_OFF].fill(0);
    for (i, slot) in slots.iter().enumerate() {
        if let Some(e) = slot {
            put_entry(&mut s0, i, status_byte(e), type_byte(e)?, lba(e.start), lba(e.start), lba(e.len));
        }
    }
    s0[BOOT_SIG_OFF] = 0x55;
    s0[BOOT_SIG_OFF + 1] = 0xAA;
    dev.write_at(0, &s0)?;
    dev.flush()
}
