//! What the user has selected in the disk table or the disk map.

use pm_core::{DiskInfo, PartitionInfo};
use pm_ops::Model;

/// The current selection. Disks and partitions are addressed by `DiskInfo::index` and
/// `PartitionInfo::number`; free space by `(disk, start, len)` in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Selected {
    #[default]
    None,
    Disk(u32),
    Partition(u32, u32),
    Free(u32, u64, u64),
}

impl Selected {
    /// The disk the selection belongs to.
    pub fn disk(&self) -> Option<u32> {
        match *self {
            Selected::None => None,
            Selected::Disk(d) | Selected::Partition(d, _) | Selected::Free(d, _, _) => Some(d),
        }
    }

    /// The selected disk in `model`, if any.
    pub fn disk_in<'m>(&self, model: &'m Model) -> Option<&'m DiskInfo> {
        model.disk(self.disk()?)
    }

    /// The selected partition in `model`, if a partition is selected and still exists.
    pub fn partition_in<'m>(&self, model: &'m Model) -> Option<&'m PartitionInfo> {
        match *self {
            Selected::Partition(d, n) => model.partition(d, n),
            _ => None,
        }
    }

    /// The selection as it still applies to `model` (e.g. after an operation was queued):
    /// a partition or free range that no longer exists falls back to its disk, a disk that no
    /// longer exists to `None`.
    pub fn revalidate(&self, model: &Model) -> Selected {
        let Some(d) = self.disk() else {
            return Selected::None;
        };
        let Some(disk) = model.disk(d) else {
            return Selected::None;
        };
        match *self {
            Selected::Partition(_, n) if disk.partitions.iter().any(|p| p.number == n) => *self,
            Selected::Free(_, start, len)
                if crate::items::disk_items(disk)
                    .iter()
                    .any(|i| i.sel == Selected::Free(d, start, len)) =>
            {
                *self
            }
            Selected::Disk(_) => *self,
            _ => Selected::Disk(d),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, model_of};
    use pm_core::MIB;

    #[test]
    fn revalidate_falls_back_to_disk_then_none() {
        let m = model_of(vec![gpt_disk(1)]);
        assert_eq!(Selected::Partition(1, 2).revalidate(&m), Selected::Partition(1, 2));
        assert_eq!(Selected::Partition(1, 99).revalidate(&m), Selected::Disk(1));
        assert_eq!(Selected::Disk(7).revalidate(&m), Selected::None);
        assert_eq!(Selected::None.revalidate(&m), Selected::None);
        // free space after partition 3 (ends at 801 MiB) up to the GPT tail
        let free = crate::items::disk_items(m.disk(1).unwrap())
            .into_iter()
            .find(|i| matches!(i.sel, Selected::Free(..)))
            .unwrap()
            .sel;
        assert_eq!(free.revalidate(&m), free);
        assert_eq!(Selected::Free(1, 5 * MIB, MIB).revalidate(&m), Selected::Disk(1));
    }

    #[test]
    fn accessors() {
        let m = model_of(vec![gpt_disk(1)]);
        assert_eq!(Selected::Free(1, 0, 0).disk(), Some(1));
        assert_eq!(Selected::None.disk(), None);
        assert_eq!(Selected::Partition(1, 2).partition_in(&m).unwrap().number, 2);
        assert!(Selected::Disk(1).partition_in(&m).is_none());
        assert_eq!(Selected::Partition(1, 2).disk_in(&m).unwrap().index, 1);
    }
}
