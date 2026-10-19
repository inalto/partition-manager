//! Partition table model shared by the MBR and GPT back ends.

use crate::mbr;
use pm_core::units::{align_up, MIB};
use pm_core::{
    BlockDevice, DiskInfo, Error, PartFlags, PartitionInfo, PartitionType, Result, TableKind,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Sectors reserved at the start (header + entry array) and at the end (backup) of a GPT disk.
const GPT_RESERVED_SECTORS: u64 = 34;
/// Size of the boot code area at the start of an MBR.
const MBR_BOOTCODE_LEN: usize = 440;

/// One partition. `start` and `len` are in bytes (LBA x sector size).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// 1-based position in table order: MBR primaries use their slot (1..=4), logicals continue
    /// at 5; GPT entries use their slot.
    pub number: u32,
    pub start: u64,
    pub len: u64,
    pub ptype: PartitionType,
    /// Unique partition GUID (GPT only).
    pub guid: Option<Uuid>,
    /// Partition name (GPT only).
    pub name: String,
    /// GPT attribute bits; for MBR bit 0 is the active (bootable) flag.
    pub attrs: u64,
    /// MBR logical partition (lives inside the extended container).
    pub logical: bool,
}

impl Entry {
    /// Exclusive end offset in bytes.
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }

    pub(crate) fn is_extended(&self) -> bool {
        !self.logical && self.ptype.is_extended_mbr()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Table {
    pub kind: TableKind,
    pub sector_size: u32,
    pub disk_len: u64,
    /// GPT disk GUID.
    pub disk_guid: Option<Uuid>,
    /// MBR disk signature (offset 0x1B8).
    pub mbr_signature: u32,
    /// The 440 bytes of MBR boot code, kept verbatim across rewrites.
    pub bootcode: Vec<u8>,
    pub entries: Vec<Entry>,
    /// First usable byte for partitions on a GPT disk.
    pub gpt_first_usable: u64,
    /// Exclusive end (in bytes) of the area usable for partitions on a GPT disk.
    pub gpt_last_usable: u64,
}

/// Random non-zero 32-bit disk signature derived from a v4 UUID.
fn random_signature() -> u32 {
    let b = Uuid::new_v4().into_bytes();
    let s = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    if s == 0 {
        1
    } else {
        s
    }
}

/// Gaps of at least `min_len` bytes in `[lo, hi)` not covered by `spans` (any order, may overlap).
fn gaps(lo: u64, hi: u64, mut spans: Vec<(u64, u64)>, min_len: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    if hi <= lo {
        return out;
    }
    spans.sort_unstable();
    let mut push = |start: u64, end: u64| {
        if end > start && end - start >= min_len {
            out.push((start, end - start));
        }
    };
    let mut cursor = lo;
    for (start, end) in spans {
        if start >= hi {
            break;
        }
        if start > cursor {
            push(cursor, start);
        }
        cursor = cursor.max(end);
    }
    if cursor < hi {
        push(cursor, hi);
    }
    out
}

/// Error if any two of `items` (`(start, end, number)`) overlap.
fn check_no_overlap(mut items: Vec<(u64, u64, u32)>, what: &str) -> Result<()> {
    items.sort_unstable();
    for w in items.windows(2) {
        if w[0].1 > w[1].0 {
            return Err(Error::Invalid(format!(
                "{what} {} overlaps {what} {}",
                w[0].2, w[1].2
            )));
        }
    }
    Ok(())
}

impl Table {
    /// A table with no partitions. MBR gets a random non-zero signature and zeroed boot code;
    /// GPT gets a random disk GUID and default usable bounds (the GPT writer may refine them).
    pub fn empty(kind: TableKind, disk_len: u64, sector_size: u32) -> Table {
        let ss = sector_size as u64;
        let mut t = Table {
            kind,
            sector_size,
            disk_len,
            disk_guid: None,
            mbr_signature: 0,
            bootcode: Vec::new(),
            entries: Vec::new(),
            gpt_first_usable: MIB,
            gpt_last_usable: disk_len,
        };
        match kind {
            TableKind::Mbr => {
                t.mbr_signature = random_signature();
                t.bootcode = vec![0; MBR_BOOTCODE_LEN];
            }
            TableKind::Gpt => {
                t.disk_guid = Some(Uuid::new_v4());
                t.gpt_first_usable = align_up(GPT_RESERVED_SECTORS * ss, MIB);
                t.gpt_last_usable = disk_len.saturating_sub(GPT_RESERVED_SECTORS * ss);
            }
            TableKind::None | TableKind::Unknown => {}
        }
        t
    }

    /// Unallocated ranges `(start, len)` in bytes of at least `min_len`, sorted by start.
    ///
    /// MBR (and empty) disks use `[1 MiB, disk_len)`, GPT uses `[gpt_first_usable,
    /// gpt_last_usable)`. The MBR extended partition is a container: it is not "used" itself;
    /// gaps outside it and gaps between logicals inside it are reported separately, so a
    /// reported gap never straddles the container boundary.
    pub fn free_ranges(&self, min_len: u64) -> Vec<(u64, u64)> {
        let (lo, hi) = match self.kind {
            TableKind::Gpt => (self.gpt_first_usable, self.gpt_last_usable),
            _ => (MIB, self.disk_len),
        };
        let outer: Vec<(u64, u64)> = self
            .entries
            .iter()
            .filter(|e| !e.logical && e.len > 0)
            .map(|e| (e.start, e.end()))
            .collect();
        let mut out = gaps(lo, hi, outer, min_len);
        if let Some(ext) = self.entries.iter().find(|e| e.is_extended()) {
            let inner: Vec<(u64, u64)> = self
                .entries
                .iter()
                .filter(|e| e.logical && e.len > 0)
                .map(|e| (e.start, e.end()))
                .collect();
            out.extend(gaps(ext.start, ext.end().min(self.disk_len), inner, min_len));
        }
        out.sort_unstable();
        out
    }

    /// Check bounds, alignment, overlaps and table-type limits.
    pub fn validate(&self) -> Result<()> {
        let invalid = |m: String| Err(Error::Invalid(m));
        if self.sector_size == 0 {
            return invalid("sector size is zero".into());
        }
        let ss = self.sector_size as u64;
        for e in &self.entries {
            if e.len == 0 {
                return invalid(format!("partition {} has zero length", e.number));
            }
            if e.start % ss != 0 || e.len % ss != 0 {
                return invalid(format!(
                    "partition {} is not aligned to the {ss}-byte sector size",
                    e.number
                ));
            }
            match e.start.checked_add(e.len) {
                Some(end) if end <= self.disk_len => {}
                _ => return invalid(format!("partition {} lies beyond the end of the disk", e.number)),
            }
        }
        let span = |e: &Entry| (e.start, e.end(), e.number);
        match self.kind {
            TableKind::Mbr => {
                for e in &self.entries {
                    match e.ptype {
                        PartitionType::Mbr(b) if b != 0 => {}
                        _ => {
                            return invalid(format!(
                                "partition {} has no valid MBR partition type",
                                e.number
                            ))
                        }
                    }
                    if e.start / ss > u32::MAX as u64 || e.len / ss > u32::MAX as u64 {
                        return invalid(format!(
                            "partition {} does not fit 32-bit MBR sector addressing",
                            e.number
                        ));
                    }
                }
                let primaries: Vec<&Entry> = self.entries.iter().filter(|e| !e.logical).collect();
                if primaries.len() > 4 {
                    return invalid(format!(
                        "MBR allows at most 4 primary partitions, found {}",
                        primaries.len()
                    ));
                }
                let exts: Vec<&&Entry> = primaries.iter().filter(|e| e.is_extended()).collect();
                if exts.len() > 1 {
                    return invalid("MBR allows at most one extended partition".into());
                }
                check_no_overlap(primaries.iter().map(|e| span(e)).collect(), "partition")?;
                let logicals: Vec<&Entry> = self.entries.iter().filter(|e| e.logical).collect();
                match exts.first() {
                    None if !logicals.is_empty() => {
                        return invalid("logical partitions require an extended partition".into())
                    }
                    Some(ext) => {
                        for l in &logicals {
                            if l.start < ext.start || l.end() > ext.end() {
                                return invalid(format!(
                                    "logical partition {} lies outside the extended partition",
                                    l.number
                                ));
                            }
                        }
                    }
                    None => {}
                }
                check_no_overlap(logicals.iter().map(|e| span(e)).collect(), "logical partition")?;
            }
            TableKind::Gpt => {
                if self.entries.iter().any(|e| e.logical) {
                    return invalid("GPT has no logical partitions".into());
                }
                check_no_overlap(self.entries.iter().map(span).collect(), "partition")?;
            }
            TableKind::None | TableKind::Unknown => {
                if !self.entries.is_empty() {
                    return invalid("a disk without a partition table cannot hold partitions".into());
                }
            }
        }
        Ok(())
    }

    /// Lowest partition number (starting at 1) not used by any entry.
    pub fn next_number(&self) -> u32 {
        (1u32..)
            .find(|n| !self.entries.iter().any(|e| e.number == *n))
            .expect("fewer than u32::MAX entries")
    }

    /// `base` with `table`, `disk_id` and `partitions` taken from this table.
    pub fn to_disk_info(&self, base: DiskInfo) -> DiskInfo {
        let disk_id = match self.kind {
            TableKind::Mbr => format!("{:08X}", self.mbr_signature),
            TableKind::Gpt => self.disk_guid.map(|g| g.to_string()).unwrap_or_default(),
            _ => String::new(),
        };
        let partitions = self
            .entries
            .iter()
            .map(|e| PartitionInfo {
                number: e.number,
                start: e.start,
                len: e.len,
                ptype: Some(e.ptype),
                guid: e.guid,
                name: e.name.clone(),
                // GPT attribute bits (required/hidden/...) are mapped by the GPT back end.
                flags: PartFlags {
                    active: self.kind == TableKind::Mbr && e.attrs & 1 != 0,
                    ..Default::default()
                },
                logical: e.logical,
                ..Default::default()
            })
            .collect();
        DiskInfo {
            table: self.kind,
            disk_id,
            partitions,
            ..base
        }
    }

    /// MBR: new random non-zero signature. GPT: new disk GUID and a new GUID for every entry.
    pub fn regenerate_ids(&mut self) {
        match self.kind {
            TableKind::Mbr => self.mbr_signature = random_signature(),
            TableKind::Gpt => {
                self.disk_guid = Some(Uuid::new_v4());
                for e in &mut self.entries {
                    e.guid = Some(Uuid::new_v4());
                }
            }
            TableKind::None | TableKind::Unknown => {}
        }
    }
}

/// Read the partition table of `dev`.
///
/// A device whose first sector lacks the 0x55AA signature has no table (`TableKind::None`).
pub fn read_table(dev: &mut dyn BlockDevice) -> Result<Table> {
    // Task 4: dispatch to gpt::parse_gpt here when a valid GPT header is found at LBA 1
    // (or its backup at the end of the disk), before falling back to the MBR logic below.
    let ss = dev.sector_size();
    let mut sector = vec![0u8; ss as usize];
    dev.read_at(0, &mut sector)?;
    if !mbr::has_signature(&sector) {
        return Ok(Table::empty(TableKind::None, dev.len(), ss));
    }
    if mbr::is_protective_mbr(&sector) {
        // Task 4: dispatch to gpt::parse_gpt
        return Err(Error::Unsupported("GPT".into()));
    }
    mbr::parse_mbr(dev)
}

/// Write `t` to `dev` using the table's own format.
pub fn write_table(dev: &mut dyn BlockDevice, t: &Table) -> Result<()> {
    match t.kind {
        TableKind::Mbr => mbr::write_mbr(dev, t),
        // Task 4: dispatch to gpt::write_gpt
        TableKind::Gpt => Err(Error::Unsupported("GPT".into())),
        TableKind::None | TableKind::Unknown => {
            Err(Error::Invalid("cannot write a table of kind None/Unknown".into()))
        }
    }
}
