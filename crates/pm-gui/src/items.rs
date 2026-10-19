//! The rows of one disk as the user sees them: partitions (MBR extended containers are
//! hidden behind their logical partitions) and unallocated gaps of at least 1 MiB, in disk
//! order. Shared by the disk table and the disk map so both always agree.

use crate::selection::Selected;
use pm_core::{DiskInfo, PartitionInfo, TableKind, MIB};

/// Smallest unallocated gap shown as its own row / segment.
pub const MIN_FREE: u64 = MIB;

/// How an item is drawn (and colored) in the disk map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// GPT data or MBR primary partition.
    Primary,
    /// MBR logical partition.
    Logical,
    Unallocated,
    /// EFI system or Microsoft reserved partition.
    System,
    /// Windows recovery partition.
    Recovery,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiskItem {
    pub start: u64,
    pub len: u64,
    pub kind: ItemKind,
    /// `Selected::Partition` or `Selected::Free` for this item.
    pub sel: Selected,
    /// Used bytes, when known (partitions only).
    pub used: Option<u64>,
}

/// MBR extended container (holds logical partitions, no data of its own).
pub fn is_container(disk: &DiskInfo, p: &PartitionInfo) -> bool {
    disk.table == TableKind::Mbr && !p.logical && p.ptype.map_or(false, |t| t.is_extended_mbr())
}

pub fn kind_of(p: &PartitionInfo) -> ItemKind {
    match p.ptype {
        Some(t) if t.is_efi() || t.is_msr() => ItemKind::System,
        Some(t) if t.is_recovery() => ItemKind::Recovery,
        _ if p.logical => ItemKind::Logical,
        _ => ItemKind::Primary,
    }
}

/// Partitions and unallocated gaps of `disk`, sorted by start offset.
pub fn disk_items(disk: &DiskInfo) -> Vec<DiskItem> {
    let mut items: Vec<DiskItem> = disk
        .partitions
        .iter()
        .filter(|p| p.len > 0 && !is_container(disk, p))
        .map(|p| DiskItem {
            start: p.start,
            len: p.len,
            kind: kind_of(p),
            sel: Selected::Partition(disk.index, p.number),
            used: p.used,
        })
        .collect();
    let free = |start: u64, len: u64| DiskItem {
        start,
        len,
        kind: ItemKind::Unallocated,
        sel: Selected::Free(disk.index, start, len),
        used: None,
    };
    for (start, len) in disk.free_ranges(MIN_FREE) {
        items.push(free(start, len));
    }
    // Free space inside extended containers (room for new logical partitions). Gaps of
    // exactly 1 MiB are the EBR slack in front of each logical partition, not free space.
    for c in disk.partitions.iter().filter(|p| is_container(disk, p)) {
        let mut logicals: Vec<(u64, u64)> = disk
            .partitions
            .iter()
            .filter(|p| p.logical && p.start >= c.start && p.end() <= c.end())
            .map(|p| (p.start, p.end()))
            .collect();
        logicals.sort_unstable();
        let mut cursor = c.start;
        for (s, e) in logicals.into_iter().chain(std::iter::once((c.end(), c.end()))) {
            if s > cursor && s - cursor > MIN_FREE {
                items.push(free(cursor, s - cursor));
            }
            cursor = cursor.max(e);
        }
    }
    items.sort_by_key(|i| (i.start, i.len));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk};

    #[test]
    fn gpt_items_in_order_with_tail_gap() {
        let d = gpt_disk(1);
        let items = disk_items(&d);
        let kinds: Vec<ItemKind> = items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            [ItemKind::System, ItemKind::System, ItemKind::Primary, ItemKind::Unallocated]
        );
        let last = items.last().unwrap();
        assert_eq!(last.start, 801 * MIB);
        assert_eq!(last.len, 1024 * MIB - 801 * MIB - 33 * 512);
        assert_eq!(last.sel, Selected::Free(1, last.start, last.len));
        assert_eq!(items[2].sel, Selected::Partition(1, 3));
        assert_eq!(items[2].used, Some(342 * MIB));
    }

    #[test]
    fn mbr_container_hidden_and_inner_free_space_shown() {
        let d = mbr_disk(0);
        let items = disk_items(&d);
        let summary: Vec<(u64, ItemKind)> = items.iter().map(|i| (i.start / MIB, i.kind)).collect();
        assert_eq!(
            summary,
            [
                (1, ItemKind::Primary),
                (201, ItemKind::Unallocated),
                (301, ItemKind::Logical),
                (402, ItemKind::Logical),
                (502, ItemKind::Unallocated),
                (700, ItemKind::Unallocated),
            ]
        );
        assert!(items.iter().all(|i| i.sel != Selected::Partition(0, 2)));
    }

    #[test]
    fn uninitialized_disk_is_one_free_item() {
        let d = DiskInfo { index: 3, size: 64 * MIB, sector_size: 512, table: TableKind::None, ..Default::default() };
        let items = disk_items(&d);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::Unallocated);
        assert_eq!(items[0].len, 63 * MIB);
    }
}
