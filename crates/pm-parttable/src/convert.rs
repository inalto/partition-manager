//! MBR <-> GPT conversion and partition alignment. All functions are pure: they return a new
//! [`Table`] and never touch a device (moving data is the caller's job).

use crate::table::{Entry, Table};
use pm_core::gpt_types as g;
use pm_core::units::{align_down, align_up, MIB};
use pm_core::{Error, PartitionType, Result, TableKind};
use uuid::Uuid;

/// Most partitions a GPT written by this crate can hold.
const GPT_MAX_ENTRIES: usize = 128;
/// Smallest first-usable byte offset of a GPT: LBA 0, the header and the 16 KiB entry array.
fn gpt_min_first_usable(ss: u64) -> u64 {
    (2 + (128u64 * 128).div_ceil(ss)) * ss
}
/// 32-bit LBAs at 512 bytes per sector: the largest disk an MBR can describe.
const MBR_MAX_DISK: u64 = 1 << 41;

fn gpt_type_for(mbr: u8) -> Uuid {
    match mbr {
        0x27 => g::WINDOWS_RECOVERY,
        0x82 => g::LINUX_SWAP,
        0x83 => g::LINUX_FS,
        0x8E => g::LINUX_LVM,
        0xEF => g::EFI_SYSTEM,
        0xAF => g::APPLE_HFS,
        // FAT12/16/32, NTFS/exFAT and everything unknown.
        _ => g::BASIC_DATA,
    }
}

/// True if `mbr` has a dedicated GPT type (anything else becomes basic data).
fn mbr_type_is_mapped(mbr: u8) -> bool {
    matches!(
        mbr,
        0x01 | 0x04 | 0x06 | 0x07 | 0x0B | 0x0C | 0x0E | 0x27 | 0x82 | 0x83 | 0x8E | 0xAF | 0xEF
    )
}

/// `t` expressed in the type system of a `to` table: an MBR type byte becomes its GPT type
/// GUID ([`mbr_to_gpt`]'s mapping: FAT/NTFS and unknown bytes become basic data), a GPT GUID
/// its MBR byte ([`gpt_to_mbr`]'s mapping: anything without one becomes 0x07). A type that
/// already belongs to `to` (or any other `to`) is returned unchanged.
pub fn convert_type(t: PartitionType, to: TableKind) -> PartitionType {
    match (t, to) {
        (PartitionType::Mbr(b), TableKind::Gpt) => PartitionType::Gpt(gpt_type_for(b)),
        (PartitionType::Gpt(g), TableKind::Mbr) => PartitionType::Mbr(mbr_type_for(g)),
        (t, _) => t,
    }
}

fn mbr_type_for(gpt: Uuid) -> u8 {
    match gpt {
        g::EFI_SYSTEM => 0xEF,
        g::WINDOWS_RECOVERY => 0x27,
        g::LINUX_FS => 0x83,
        g::LINUX_SWAP => 0x82,
        g::LINUX_LVM => 0x8E,
        g::APPLE_HFS => 0xAF,
        _ => 0x07,
    }
}

/// Convert an MBR table to GPT: new disk and partition GUIDs, mapped types, the extended
/// container dropped and logicals promoted to ordinary entries, numbered 1..=n by start.
///
/// Unmapped MBR type bytes become basic data with the original byte kept as the name
/// `mbr-0xNN` (only while the name is empty). The usable range is the default one
/// ([`Table::empty`]), lowered to the minimum a GPT needs when a legacy layout (for example a
/// first partition at sector 63) starts earlier. `Error::Invalid` if any entry does not fit
/// the usable range or there are more than 128.
pub fn mbr_to_gpt(t: &Table) -> Result<Table> {
    if t.kind != TableKind::Mbr {
        return Err(Error::Invalid("mbr_to_gpt needs an MBR table".into()));
    }
    let mut out = Table::empty(TableKind::Gpt, t.disk_len, t.sector_size);
    if t.bootcode.len() == out.bootcode.len() {
        out.bootcode = t.bootcode.clone();
    }
    out.mbr_signature = t.mbr_signature;

    let mut src: Vec<&Entry> = t.entries.iter().filter(|e| !e.is_extended()).collect();
    src.sort_by_key(|e| e.start);
    if src.len() > GPT_MAX_ENTRIES {
        return Err(Error::Invalid(format!(
            "{} partitions do not fit in a GPT (at most {GPT_MAX_ENTRIES})",
            src.len()
        )));
    }
    let ss = t.sector_size as u64;
    if let Some(first) = src.first() {
        if first.start < out.gpt_first_usable {
            out.gpt_first_usable = gpt_min_first_usable(ss);
        }
    }
    for (i, e) in src.iter().enumerate() {
        if e.start < out.gpt_first_usable || e.end() > out.gpt_last_usable {
            return Err(Error::Invalid(format!(
                "partition {} ({}..{}) does not fit the GPT usable area {}..{}",
                e.number,
                e.start,
                e.end(),
                out.gpt_first_usable,
                out.gpt_last_usable
            )));
        }
        let (ptype, mut name) = match e.ptype {
            PartitionType::Mbr(b) => {
                let name = if !mbr_type_is_mapped(b) && e.name.is_empty() {
                    format!("mbr-0x{b:02X}")
                } else {
                    e.name.clone()
                };
                (PartitionType::Gpt(gpt_type_for(b)), name)
            }
            gpt @ PartitionType::Gpt(_) => (gpt, e.name.clone()),
        };
        if name.encode_utf16().count() > 36 {
            name = name.chars().take(36).collect();
        }
        out.entries.push(Entry {
            number: i as u32 + 1,
            start: e.start,
            len: e.len,
            ptype,
            guid: Some(Uuid::new_v4()),
            name,
            attrs: 0,
            logical: false,
        });
    }
    out.validate()?;
    Ok(out)
}

/// Convert a GPT table to MBR: Microsoft reserved partitions are dropped, types mapped back,
/// the rest become primaries numbered 1..=n by start with a fresh disk signature.
///
/// `Error::Invalid` for more than 4 remaining entries, a disk over 2 TiB, or any partition
/// whose start or end sector does not fit 32 bits.
pub fn gpt_to_mbr(t: &Table) -> Result<Table> {
    if t.kind != TableKind::Gpt {
        return Err(Error::Invalid("gpt_to_mbr needs a GPT table".into()));
    }
    if t.disk_len > MBR_MAX_DISK {
        return Err(Error::Invalid("MBR cannot address a disk larger than 2 TiB".into()));
    }
    let mut src: Vec<&Entry> = t.entries.iter().filter(|e| !e.ptype.is_msr()).collect();
    if src.len() > 4 {
        return Err(Error::Invalid(format!(
            "MBR holds at most 4 primary partitions, found {}",
            src.len()
        )));
    }
    src.sort_by_key(|e| e.start);
    let ss = (t.sector_size as u64).max(1);
    let mut out = Table::empty(TableKind::Mbr, t.disk_len, t.sector_size);
    if t.bootcode.len() == out.bootcode.len() {
        out.bootcode = t.bootcode.clone();
    }
    for (i, e) in src.iter().enumerate() {
        if e.start / ss > u32::MAX as u64 || e.len / ss > u32::MAX as u64 || e.end() / ss > u32::MAX as u64 {
            return Err(Error::Invalid(format!(
                "partition {} does not fit 32-bit MBR sector addressing",
                e.number
            )));
        }
        let byte = match e.ptype {
            PartitionType::Gpt(guid) => mbr_type_for(guid),
            PartitionType::Mbr(b) => b,
        };
        out.entries.push(Entry {
            number: i as u32 + 1,
            start: e.start,
            len: e.len,
            ptype: PartitionType::Mbr(byte),
            guid: None,
            name: String::new(),
            // The active flag is the caller's choice.
            attrs: 0,
            logical: false,
        });
    }
    out.validate()?;
    Ok(out)
}

/// A copy of `t` with every partition moved to `align` boundaries: the start rounds up and the
/// end rounds down, so partitions only ever shrink. An entry that would end up smaller than
/// 1 MiB (or empty) is left unchanged, as are MBR extended containers (their logicals decide
/// their extent). Numbers are preserved; `align == 0` returns an unchanged copy.
pub fn align_table(t: &Table, align: u64) -> Table {
    let mut out = t.clone();
    for e in &mut out.entries {
        if e.is_extended() {
            continue;
        }
        let start = align_up(e.start, align);
        let end = align_down(e.end(), align);
        if end > start && end - start >= MIB {
            e.start = start;
            e.len = end - start;
        }
    }
    out
}
