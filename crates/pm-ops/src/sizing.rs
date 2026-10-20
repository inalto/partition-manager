//! Target layout planning for disk clones and OS migrations.

pub use crate::model::windows_partition;
use crate::model::{attrs_of, default_ptype, is_container, is_fixed_system};
use pm_core::units::{align_down, align_up, MIB};
use pm_core::{
    format_size, DiskInfo, Error, PartitionInfo, PartitionType, Result, TableKind,
};
use crate::operation::Sizing;
use crate::operation::table_name;
use crate::planner::err_text;
use pm_parttable::{convert_type, Entry, Table};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// What a disk clone / OS migration lays out on the target besides the partition sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutSpec {
    pub sizing: Sizing,
    /// Copy only the EFI, MSR, Windows recovery and Windows partitions.
    pub only_system: bool,
    /// Partition table kind of the target; `None` keeps the source's kind.
    pub target_table: Option<TableKind>,
    /// Keep the source's disk id and partition GUIDs instead of generating fresh ones
    /// (same table kind only).
    pub keep_ids: bool,
}

impl LayoutSpec {
    /// Sizing and partition choice only: the source's table kind, fresh ids.
    pub fn new(sizing: Sizing, only_system: bool) -> LayoutSpec {
        LayoutSpec { sizing, only_system, target_table: None, keep_ids: false }
    }

    /// The table kind the target gets for a source of kind `src`.
    pub fn table_kind(&self, src: TableKind) -> TableKind {
        self.target_table.unwrap_or(src)
    }
}

/// The partition table to write on the target disk, and which source partition is copied
/// into which target partition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetLayout {
    pub table: Table,
    /// `(source partition number, target partition number)` in layout order.
    pub map: Vec<(u32, u32)>,
}

/// One source partition to copy.
struct Item<'a> {
    p: &'a PartitionInfo,
    /// Data partitions are resized; system/other partitions keep their length.
    data: bool,
    /// Smallest acceptable target length (data partitions only).
    min: u64,
}

/// `used` plus a 10 % margin, but never more than the source length (the source size always
/// fits its data) and never less than `used`; rounded up to 1 MiB, at least 1 MiB.
fn min_target_len(used: u64, len: u64) -> u64 {
    let margin = used.saturating_add(used.div_ceil(10));
    align_up(used.max(margin.min(len)), MIB).max(MIB)
}

/// `[first, end)` usable for partitions on an empty table of the target.
fn usable(t: &Table) -> (u64, u64) {
    match t.kind {
        TableKind::Gpt => (t.gpt_first_usable, t.gpt_last_usable),
        _ => {
            // 32-bit LBAs: an MBR cannot describe anything past 2^32 sectors.
            let ss = (t.sector_size as u64).max(1);
            (MIB, t.disk_len.min((u32::MAX as u64).saturating_mul(ss)))
        }
    }
}

fn too_small(src: &DiskInfo, item: &Item, room: u64) -> Error {
    Error::Invalid(format!(
        "Target smaller than used data: partition {} of Disk {} needs at least {} (used data + 10 %) \
         but only {} is left on the target",
        item.p.number,
        src.index,
        format_size(item.min),
        format_size(room)
    ))
}

/// Plan the target partition table for cloning `src` onto a disk of `dst_len` bytes with
/// `dst_sector`-byte sectors.
///
/// * System partitions are EFI, MSR, Windows recovery and the Windows partition (see
///   [`windows_partition`]); `only_system` drops every other partition.
/// * Data partitions (`is_data` types and the Windows partition) are resized; every other
///   partition keeps its length.
/// * `Exact`: same starts and lengths; `Error::Invalid` if anything does not fit.
/// * `Proportional`: data partitions share `usable - fixed` in proportion to their source
///   lengths (aligned down to 1 MiB), each at least `used + 10 %`.
/// * `Fit`: each data partition gets `max(min, min(source length, what is left))`, the
///   leftover goes to the largest data partition.
///
/// Partitions are laid out in source order from the first usable byte, 1 MiB aligned (MBR
/// logicals inside a rebuilt extended container with 1 MiB EBR slots), numbered 1..n (MBR
/// logicals from 5), with fresh disk/partition GUIDs (GPT) or signature (MBR) unless
/// `spec.keep_ids` asks for the source's. `used_of` returns the bytes that must be copied
/// for a partition; `probe` is passed on to [`windows_partition`] (e.g.
/// [`crate::Model::windows_probe`]).
///
/// With a `spec.target_table` of the other kind the partitions are converted on the way:
/// types are mapped ([`pm_parttable::convert_type`]), MBR logicals become ordinary GPT
/// entries, a GPT Microsoft reserved partition is dropped on an MBR target (which has no
/// such thing) and the Windows partition becomes the active one there, an MBR target holds
/// at most four partitions and nothing past 2 TiB, and the ids cannot be kept.
pub fn plan_disk_clone(
    src: &DiskInfo,
    dst_len: u64,
    dst_sector: u32,
    spec: LayoutSpec,
    used_of: &dyn Fn(&PartitionInfo) -> u64,
    probe: Option<&dyn Fn(u32, &PartitionInfo) -> bool>,
) -> Result<TargetLayout> {
    let src_kind = src.table;
    if !matches!(src_kind, TableKind::Mbr | TableKind::Gpt) {
        return Err(Error::Invalid(format!(
            "Disk {} has no partition table to clone",
            src.index
        )));
    }
    let kind = spec.table_kind(src_kind);
    if !matches!(kind, TableKind::Mbr | TableKind::Gpt) {
        return Err(Error::Invalid(format!(
            "The target of a copy of Disk {} must use GPT or MBR",
            src.index
        )));
    }
    if spec.keep_ids && kind != src_kind {
        return Err(Error::Invalid(format!(
            "The ids of Disk {} can only be kept on a target with the same partition table \
             kind ({}); a {} copy gets new ids",
            src.index,
            table_name(src_kind),
            table_name(kind)
        )));
    }
    let sizing = spec.sizing;
    let ss = if dst_sector == 0 { 512 } else { dst_sector };
    let mut table = Table::empty(kind, dst_len, ss);
    let (lo, hi) = usable(&table);
    if hi <= lo {
        return Err(Error::Invalid(format!(
            "The target disk ({}) is too small to clone Disk {}",
            format_size(dst_len),
            src.index
        )));
    }

    let windows = windows_partition(src, probe);
    let mut parts: Vec<&PartitionInfo> = src
        .partitions
        .iter()
        .filter(|p| p.len > 0 && !is_container(src_kind, p))
        .filter(|p| !spec.only_system || is_fixed_system(p) || Some(p.number) == windows)
        // an MBR has no Microsoft reserved partition; Windows recreates it when needed
        .filter(|p| !(kind == TableKind::Mbr && p.ptype.is_some_and(|t| t.is_msr())))
        .collect();
    parts.sort_by_key(|p| p.start);
    if parts.is_empty() {
        return Err(Error::Invalid(format!(
            "Disk {} has no partitions to clone",
            src.index
        )));
    }
    if kind == TableKind::Mbr && src_kind == TableKind::Gpt && parts.len() > 4 {
        return Err(Error::Invalid(format!(
            "An MBR target holds at most 4 partitions; {} would be copied from Disk {} (copy \
             to GPT, or copy only the system partitions)",
            parts.len(),
            src.index
        )));
    }
    let items: Vec<Item> = parts
        .into_iter()
        .map(|p| {
            let data = Some(p.number) == windows
                || p.ptype.map_or(p.fs.is_some(), |t| t.is_data());
            let min = if data { min_target_len(used_of(p), p.len) } else { 0 };
            Item { p, data, min }
        })
        .collect();

    let src_container = src.partitions.iter().find(|p| is_container(src_kind, p));
    let entries = if sizing == Sizing::Exact {
        place_exact(src, kind, &items, src_container, lo, hi).map_err(|e| with_exact_hint(sizing, e))?
    } else {
        let sizes = match sizing {
            Sizing::Fit => fit_sizes(src, kind, &items, hi - lo)?,
            _ => proportional_sizes(src, kind, &items, hi - lo)?,
        };
        place_sequential(src, kind, &items, &sizes, src_container, lo, hi)?
    };

    // Number in layout order: GPT 1..n; MBR primaries (and the container) 1.., logicals 5...
    let mut map = Vec::new();
    let (mut next_primary, mut next_logical) = (1u32, 5u32);
    for (slot, source) in entries {
        let mut e = slot;
        if kind == TableKind::Mbr && e.logical {
            e.number = next_logical;
            next_logical += 1;
        } else {
            e.number = next_primary;
            next_primary += 1;
        }
        if let Some(n) = source {
            map.push((n, e.number));
            // a BIOS boots the active partition: on a GPT → MBR copy that is Windows
            if kind == TableKind::Mbr && src_kind == TableKind::Gpt && Some(n) == windows {
                e.attrs |= 1;
            }
        }
        table.entries.push(e);
    }
    if spec.keep_ids {
        keep_source_ids(src, &mut table, &map)?;
    } else {
        table.regenerate_ids();
    }
    table.validate().map_err(|e| {
        let e = Error::Invalid(format!(
            "Cannot clone Disk {} to the target: {}",
            src.index,
            err_text(e)
        ));
        with_exact_hint(sizing, e)
    })?;
    Ok(TargetLayout { table, map })
}

/// Give `table` (of the source's kind) the source's disk id and, on GPT, the GUID of the
/// source partition behind every mapped entry. `Error::Invalid` when the source's ids are
/// not known (a disk id that is not a GUID / signature, a partition without a GUID).
fn keep_source_ids(src: &DiskInfo, table: &mut Table, map: &[(u32, u32)]) -> Result<()> {
    let id = src.disk_id.trim();
    match table.kind {
        TableKind::Mbr => {
            table.mbr_signature = u32::from_str_radix(id, 16).ok().filter(|s| *s != 0).ok_or_else(|| {
                Error::Invalid(format!(
                    "Cannot keep the ids of Disk {}: its MBR signature is unknown ({id:?})",
                    src.index
                ))
            })?;
        }
        TableKind::Gpt => {
            table.disk_guid = Some(Uuid::parse_str(id).map_err(|_| {
                Error::Invalid(format!(
                    "Cannot keep the ids of Disk {}: its disk GUID is unknown ({id:?})",
                    src.index
                ))
            })?);
            for &(sn, dn) in map {
                let guid = src.partitions.iter().find(|p| p.number == sn).and_then(|p| p.guid);
                let Some(guid) = guid else {
                    return Err(Error::Invalid(format!(
                        "Cannot keep the ids of Disk {}: partition {sn} has no GUID",
                        src.index
                    )));
                };
                if let Some(e) = table.entries.iter_mut().find(|e| e.number == dn) {
                    e.guid = Some(guid);
                }
            }
        }
        TableKind::None | TableKind::Unknown => {}
    }
    Ok(())
}

/// For `Exact`, point the user at the sizings that re-lay the partitions out.
fn with_exact_hint(sizing: Sizing, e: Error) -> Error {
    if sizing != Sizing::Exact {
        return e;
    }
    Error::Invalid(format!(
        "{} (an exact copy keeps the source layout; use Fit or Proportional sizing, which lay \
         the partitions out on a 1 MB grid with room for MBR EBRs)",
        err_text(e)
    ))
}

/// The entry of a `kind` table for `p` (whose type is mapped when the kinds differ).
fn entry_for(kind: TableKind, p: &PartitionInfo, start: u64, len: u64) -> Entry {
    Entry {
        number: 0,
        start,
        len,
        ptype: p.ptype.map_or_else(|| default_ptype(kind), |t| convert_type(t, kind)),
        guid: None,
        name: if kind == TableKind::Gpt { p.name.clone() } else { String::new() },
        attrs: attrs_of(kind, p),
        logical: kind == TableKind::Mbr && p.logical,
    }
}

/// An MBR extended container entry (type from the source container, 0x0F otherwise).
fn container_entry(container: Option<&PartitionInfo>, start: u64, len: u64) -> Entry {
    Entry {
        number: 0,
        start,
        len,
        ptype: container.and_then(|c| c.ptype).unwrap_or(PartitionType::Mbr(0x0F)),
        guid: None,
        name: String::new(),
        attrs: 0,
        logical: false,
    }
}

type Slots = Vec<(Entry, Option<u32>)>;

fn place_exact(
    src: &DiskInfo,
    kind: TableKind,
    items: &[Item],
    container: Option<&PartitionInfo>,
    lo: u64,
    hi: u64,
) -> Result<Slots> {
    let mut out: Slots = Vec::new();
    let mut container_done = false;
    for it in items {
        let p = it.p;
        if p.start < lo || p.end() > hi {
            return Err(Error::Invalid(format!(
                "Target disk too small for an exact copy: partition {} of Disk {} spans {} – {} \
                 but the target's usable area is {} – {}",
                p.number,
                src.index,
                format_size(p.start),
                format_size(p.end()),
                format_size(lo),
                format_size(hi)
            )));
        }
        if kind == TableKind::Mbr && p.logical && !container_done {
            container_done = true;
            if let Some(c) = container {
                if c.end() > hi {
                    return Err(Error::Invalid(format!(
                        "Target disk too small for an exact copy: the extended partition {} of \
                         Disk {} ends at {}",
                        c.number,
                        src.index,
                        format_size(c.end())
                    )));
                }
                out.push((container_entry(Some(c), c.start, c.len), None));
            }
        }
        out.push((entry_for(kind, p, p.start, p.len), Some(p.number)));
    }
    Ok(out)
}

/// Bytes taken by fixed-size partitions (rounded up to the 1 MiB layout grid) and, on an MBR
/// target, the EBR slots of logicals.
fn fixed_overhead(kind: TableKind, items: &[Item]) -> u64 {
    items.iter().fold(0u64, |acc, it| {
        let own = if it.data { 0 } else { align_up(it.p.len, MIB) };
        let ebr = if kind == TableKind::Mbr && it.p.logical { MIB } else { 0 };
        acc.saturating_add(own).saturating_add(ebr)
    })
}

/// The data item with the largest minimum, for "too small" errors.
fn neediest<'a, 'b>(items: &'b [Item<'a>]) -> Option<&'b Item<'a>> {
    items.iter().filter(|i| i.data).max_by_key(|i| i.min)
}

fn fixed_too_small(src: &DiskInfo, avail: u64, fixed: u64) -> Error {
    Error::Invalid(format!(
        "The target is too small for the system partitions of Disk {}: they need {}, the target \
         has {}",
        src.index,
        format_size(fixed),
        format_size(avail)
    ))
}

fn fit_sizes(src: &DiskInfo, kind: TableKind, items: &[Item], avail: u64) -> Result<Vec<u64>> {
    let fixed = fixed_overhead(kind, items);
    let mins: u64 = items.iter().filter(|i| i.data).fold(0u64, |a, i| a.saturating_add(i.min));
    let needed = fixed.saturating_add(mins);
    if needed > avail {
        return Err(match neediest(items) {
            Some(it) => {
                let others = needed.saturating_sub(fixed).saturating_sub(it.min);
                too_small(src, it, avail.saturating_sub(fixed).saturating_sub(others))
            }
            None => fixed_too_small(src, avail, fixed),
        });
    }
    let mut slack = avail - needed;
    let mut sizes: Vec<u64> = items
        .iter()
        .map(|it| {
            if !it.data {
                return it.p.len;
            }
            let cap = align_down(it.p.len, MIB).max(it.min);
            let extra = (cap - it.min).min(align_down(slack, MIB));
            slack -= extra;
            it.min + extra
        })
        .collect();
    let leftover = align_down(slack, MIB);
    if leftover > 0 {
        let largest = items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.data)
            .max_by(|a, b| a.1.p.len.cmp(&b.1.p.len).then(b.0.cmp(&a.0)))
            .map(|(i, _)| i);
        if let Some(i) = largest {
            sizes[i] = sizes[i].saturating_add(leftover);
        }
    }
    Ok(sizes)
}

fn proportional_sizes(src: &DiskInfo, kind: TableKind, items: &[Item], avail: u64) -> Result<Vec<u64>> {
    let fixed = fixed_overhead(kind, items);
    let data_avail = avail
        .checked_sub(fixed)
        .ok_or_else(|| fixed_too_small(src, avail, fixed))?;
    let src_data_total: u128 = items.iter().filter(|i| i.data).map(|i| i.p.len as u128).sum();
    let mut sizes: Vec<u64> = items
        .iter()
        .map(|it| {
            if !it.data {
                return it.p.len;
            }
            let scaled = if src_data_total == 0 {
                0
            } else {
                (it.p.len as u128 * data_avail as u128 / src_data_total) as u64
            };
            align_down(scaled, MIB).max(it.min)
        })
        .collect();
    // Raising small partitions to their minimum may overshoot: take it back from the
    // partitions with the most room above their minimum.
    let total = |s: &[u64]| -> u64 {
        items
            .iter()
            .zip(s)
            .filter(|(i, _)| i.data)
            .fold(0u64, |a, (_, v)| a.saturating_add(*v))
    };
    let mut excess = total(&sizes).saturating_sub(data_avail);
    while excess > 0 {
        let donor = items
            .iter()
            .enumerate()
            .filter(|(i, it)| it.data && sizes[*i] > it.min)
            .max_by_key(|(i, it)| sizes[*i] - it.min)
            .map(|(i, _)| i);
        let Some(i) = donor else {
            return Err(match neediest(items) {
                Some(it) => {
                    let others = total(&sizes).saturating_sub(it.min);
                    too_small(src, it, data_avail.saturating_sub(others))
                }
                None => fixed_too_small(src, avail, fixed),
            });
        };
        let take = (sizes[i] - items[i].min).min(align_up(excess, MIB));
        sizes[i] -= take;
        excess = excess.saturating_sub(take);
    }
    Ok(sizes)
}

fn place_sequential(
    src: &DiskInfo,
    kind: TableKind,
    items: &[Item],
    sizes: &[u64],
    container: Option<&PartitionInfo>,
    lo: u64,
    hi: u64,
) -> Result<Slots> {
    let mut out: Slots = Vec::new();
    let mut cursor = lo;
    // Index in `out` of the open extended container.
    let mut open: Option<usize> = None;
    let close = |out: &mut Slots, open: &mut Option<usize>, end: u64| {
        if let Some(ci) = open.take() {
            let start = out[ci].0.start;
            out[ci].0.len = end - start;
        }
    };
    for (it, &size) in items.iter().zip(sizes) {
        let p = it.p;
        let logical = kind == TableKind::Mbr && p.logical;
        if logical {
            if open.is_none() {
                let s = align_up(cursor, MIB);
                out.push((container_entry(container, s, 0), None));
                open = Some(out.len() - 1);
                cursor = s;
            }
            // 1 MiB slot for the EBR in front of every logical.
            cursor = align_up(cursor, MIB).saturating_add(MIB);
        } else {
            close(&mut out, &mut open, cursor);
        }
        let start = align_up(cursor, MIB);
        let end = start.checked_add(size).filter(|e| *e <= hi).ok_or_else(|| {
            Error::Invalid(format!(
                "Target disk too small: partition {} of Disk {} ({}) does not fit before the end \
                 of the usable area",
                p.number,
                src.index,
                format_size(size)
            ))
        })?;
        out.push((entry_for(kind, p, start, size), Some(p.number)));
        cursor = end;
    }
    close(&mut out, &mut open, cursor);
    Ok(out)
}
