//! The actions offered by the sidebar and the right-click menu, when each one is available,
//! and what it does. Everything here is pure (no egui, no platform calls) and unit-tested.

use crate::items::is_container;
use crate::selection::Selected;
use pm_core::{TableKind, VolumeId};
use pm_ops::{Model, Operation};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    // Wizards
    MigrateOs,
    CopyDisk,
    CopyPartition,
    PartitionRecovery,
    // Convert disk
    ConvertMbrToGpt,
    ConvertGptToMbr,
    // Clean disk
    WipeDisk,
    DeleteAllPartitions,
    // Check disk
    AlignAllPartitions,
    SurfaceTest,
    RebuildMbr,
    DiskProperties,
    // Manage partition
    CreatePartition,
    DeletePartition,
    FormatPartition,
    ResizeMove,
    SetActive,
    ChangeLabel,
    ChangeLetter,
    ChangeType,
    WipePartition,
    Mount,
    Unmount,
    Explore,
}

/// A titled group of actions in the sidebar.
pub struct Group {
    pub title: &'static str,
    pub actions: &'static [Action],
}

pub const GROUPS: [Group; 5] = [
    Group {
        title: "Wizards",
        actions: &[Action::MigrateOs, Action::CopyDisk, Action::CopyPartition, Action::PartitionRecovery],
    },
    Group { title: "Convert disk", actions: &[Action::ConvertMbrToGpt, Action::ConvertGptToMbr] },
    Group { title: "Clean disk", actions: &[Action::WipeDisk, Action::DeleteAllPartitions] },
    Group {
        title: "Check disk",
        actions: &[Action::AlignAllPartitions, Action::SurfaceTest, Action::RebuildMbr, Action::DiskProperties],
    },
    Group {
        title: "Manage partition",
        actions: &[
            Action::CreatePartition,
            Action::DeletePartition,
            Action::FormatPartition,
            Action::ResizeMove,
            Action::SetActive,
            Action::ChangeLabel,
            Action::ChangeLetter,
            Action::ChangeType,
            Action::WipePartition,
            Action::Mount,
            Action::Unmount,
            Action::Explore,
        ],
    },
];

/// What the UI must do for an action on the current selection.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Push these operations onto the queue, all or none.
    Push(Vec<Operation>),
    /// Ask for a wipe method, then push `WipeDisk`.
    ChooseWipeMethod { disk: u32 },
    /// Ask for a free drive letter, then push `Mount`.
    ChooseMountLetter { volume: VolumeId },
    /// Ask which mount point to remove, then push `Unmount`.
    ChooseUnmount { volume: VolumeId, mount_points: Vec<String> },
    ShowProperties { disk: u32 },
    /// Open a file browser on this path.
    Explore { path: String },
    /// Needs a wizard or tool window (later step); remembered in `pending_action`.
    Wizard(Action),
}

impl Action {
    pub fn label(self) -> &'static str {
        use Action::*;
        match self {
            MigrateOs => "Migrate OS to SSD/HD",
            CopyDisk => "Copy Disk",
            CopyPartition => "Copy Partition",
            PartitionRecovery => "Partition Recovery",
            ConvertMbrToGpt => "Convert MBR to GPT",
            ConvertGptToMbr => "Convert GPT to MBR",
            WipeDisk => "Wipe Disk",
            DeleteAllPartitions => "Delete All Partitions",
            AlignAllPartitions => "Align All Partitions",
            SurfaceTest => "Surface Test",
            RebuildMbr => "Rebuild MBR",
            DiskProperties => "Disk Properties",
            CreatePartition => "Create",
            DeletePartition => "Delete",
            FormatPartition => "Format",
            ResizeMove => "Resize/Move",
            SetActive => "Set Active",
            ChangeLabel => "Change Label",
            ChangeLetter => "Change Letter",
            ChangeType => "Change Type",
            WipePartition => "Wipe Partition",
            Mount => "Mount",
            Unmount => "Unmount",
            Explore => "Explore",
        }
    }

    /// Whether the action makes sense for `sel` in `model` (the preview model).
    pub fn enabled(self, sel: &Selected, model: &Model) -> bool {
        use Action::*;
        let disk = sel.disk_in(model);
        let disk_sel = matches!(sel, Selected::Disk(_)) && disk.is_some();
        let has_parts = disk.map_or(false, |d| !d.partitions.is_empty());
        let table = disk.map(|d| d.table);
        let part = sel.partition_in(model).filter(|p| disk.map_or(false, |d| !is_container(d, p)));
        match self {
            MigrateOs => !model.disks.is_empty(),
            CopyDisk => disk_sel && has_parts,
            CopyPartition => part.is_some(),
            PartitionRecovery => disk_sel || (matches!(sel, Selected::Free(..)) && disk.is_some()),
            ConvertMbrToGpt => disk_sel && table == Some(TableKind::Mbr),
            ConvertGptToMbr => disk_sel && table == Some(TableKind::Gpt),
            WipeDisk => disk_sel,
            DeleteAllPartitions | AlignAllPartitions => disk_sel && has_parts,
            SurfaceTest => disk.is_some() && !matches!(sel, Selected::Free(..)),
            RebuildMbr => disk_sel && matches!(table, Some(TableKind::Mbr | TableKind::Gpt)),
            DiskProperties => disk.is_some(),
            CreatePartition => matches!(sel, Selected::Free(..)) && disk.is_some(),
            DeletePartition | FormatPartition | ResizeMove | ChangeLetter | ChangeType | WipePartition => {
                part.is_some()
            }
            SetActive => part.map_or(false, |p| table == Some(TableKind::Mbr) && !p.logical && !p.flags.active),
            ChangeLabel => part.map_or(false, |p| p.fs.is_some()),
            Mount => part.map_or(false, |p| p.volume.is_some() && p.letter().is_none()),
            Unmount => part.map_or(false, |p| p.volume.is_some() && !p.mount_points.is_empty()),
            Explore => part.map_or(false, |p| p.letter().is_some()),
        }
    }

    /// What dispatching the action on `sel` does; `None` when it is not [`Action::enabled`].
    pub fn effect(self, sel: &Selected, model: &Model) -> Option<Effect> {
        use Action::*;
        if !self.enabled(sel, model) {
            return None;
        }
        let d = sel.disk()?;
        let part = sel.partition_in(model);
        let one = |op: Operation| Some(Effect::Push(vec![op]));
        match self {
            DeletePartition => one(Operation::DeletePartition { disk: d, number: part?.number }),
            SetActive => one(Operation::SetActive { disk: d, number: part?.number }),
            AlignAllPartitions => one(Operation::AlignPartitions { disk: d }),
            RebuildMbr => one(Operation::RebuildMbr { disk: d }),
            ConvertMbrToGpt => one(Operation::ConvertTable { disk: d, to: TableKind::Gpt }),
            ConvertGptToMbr => one(Operation::ConvertTable { disk: d, to: TableKind::Mbr }),
            DeleteAllPartitions => {
                // Highest number first: logical partitions (5+) go before their extended
                // container, and deleting the highest never renumbers a lower one.
                let mut numbers: Vec<u32> = model.disk(d)?.partitions.iter().map(|p| p.number).collect();
                numbers.sort_unstable_by(|a, b| b.cmp(a));
                Some(Effect::Push(
                    numbers.into_iter().map(|number| Operation::DeletePartition { disk: d, number }).collect(),
                ))
            }
            WipeDisk => Some(Effect::ChooseWipeMethod { disk: d }),
            Mount => Some(Effect::ChooseMountLetter { volume: part?.volume.clone()? }),
            Unmount => {
                let p = part?;
                Some(Effect::ChooseUnmount { volume: p.volume.clone()?, mount_points: p.mount_points.clone() })
            }
            DiskProperties => Some(Effect::ShowProperties { disk: d }),
            Explore => Some(Effect::Explore { path: format!("{}:\\", part?.letter()?) }),
            MigrateOs | CopyDisk | CopyPartition | PartitionRecovery | SurfaceTest | CreatePartition
            | FormatPartition | ResizeMove | ChangeLabel | ChangeLetter | ChangeType | WipePartition => {
                Some(Effect::Wizard(self))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::disk_items;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};

    fn enabled_set(sel: Selected, m: &Model) -> Vec<Action> {
        GROUPS
            .iter()
            .flat_map(|g| g.actions.iter().copied())
            .filter(|a| a.enabled(&sel, m))
            .collect()
    }

    #[test]
    fn every_action_is_in_exactly_one_group() {
        let all: Vec<Action> = GROUPS.iter().flat_map(|g| g.actions.iter().copied()).collect();
        assert_eq!(all.len(), 24);
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(unique.len(), all.len());
    }

    #[test]
    fn nothing_selected_only_offers_migrate() {
        let m = model_of(vec![gpt_disk(1)]);
        assert_eq!(enabled_set(Selected::None, &m), vec![Action::MigrateOs]);
        assert!(enabled_set(Selected::None, &model_of(vec![])).is_empty());
    }

    #[test]
    fn disk_selection_enables_disk_actions() {
        let m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        use Action::*;
        assert_eq!(
            enabled_set(Selected::Disk(1), &m),
            vec![
                MigrateOs,
                CopyDisk,
                PartitionRecovery,
                ConvertGptToMbr,
                WipeDisk,
                DeleteAllPartitions,
                AlignAllPartitions,
                SurfaceTest,
                RebuildMbr,
                DiskProperties
            ]
        );
        assert!(ConvertMbrToGpt.enabled(&Selected::Disk(0), &m));
        assert!(!ConvertGptToMbr.enabled(&Selected::Disk(0), &m));
        assert!(!WipeDisk.enabled(&Selected::Disk(9), &m), "unknown disk");
    }

    #[test]
    fn partition_and_free_selection() {
        let m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        use Action::*;
        let data = Selected::Partition(1, 3);
        for a in [CopyPartition, DeletePartition, FormatPartition, ResizeMove, ChangeLabel, Unmount, Explore] {
            assert!(a.enabled(&data, &m), "{a:?}");
        }
        assert!(!Mount.enabled(&data, &m), "already has a letter");
        assert!(!SetActive.enabled(&data, &m), "GPT");
        assert!(!CreatePartition.enabled(&data, &m));
        // MSR: no fs, no volume
        let msr = Selected::Partition(1, 2);
        assert!(!ChangeLabel.enabled(&msr, &m) && !Unmount.enabled(&msr, &m) && !Explore.enabled(&msr, &m));
        // MBR: active primary cannot be set active again, a logical never
        assert!(!SetActive.enabled(&Selected::Partition(0, 1), &m));
        assert!(!SetActive.enabled(&Selected::Partition(0, 5), &m));
        let mut m2 = m.clone();
        m2.partition_mut(0, 1).unwrap().flags.active = false;
        assert!(SetActive.enabled(&Selected::Partition(0, 1), &m2));
        // the extended container is never a partition target
        assert!(enabled_set(Selected::Partition(0, 2), &m).iter().all(|a| matches!(a, MigrateOs | SurfaceTest | DiskProperties)));
        // free space
        let free = disk_items(m.disk(1).unwrap()).last().unwrap().sel;
        assert_eq!(enabled_set(free, &m), vec![MigrateOs, PartitionRecovery, DiskProperties, CreatePartition]);
    }

    #[test]
    fn effects_build_operations() {
        let m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        assert_eq!(
            Action::DeletePartition.effect(&Selected::Partition(1, 3), &m),
            Some(Effect::Push(vec![Operation::DeletePartition { disk: 1, number: 3 }]))
        );
        assert_eq!(
            Action::ConvertGptToMbr.effect(&Selected::Disk(1), &m),
            Some(Effect::Push(vec![Operation::ConvertTable { disk: 1, to: TableKind::Mbr }]))
        );
        assert_eq!(Action::WipeDisk.effect(&Selected::Disk(1), &m), Some(Effect::ChooseWipeMethod { disk: 1 }));
        assert_eq!(
            Action::Unmount.effect(&Selected::Partition(1, 3), &m),
            Some(Effect::ChooseUnmount { volume: VolumeId("vol-e".into()), mount_points: vec!["E:\\".into()] })
        );
        assert_eq!(
            Action::Explore.effect(&Selected::Partition(1, 3), &m),
            Some(Effect::Explore { path: "E:\\".into() })
        );
        assert_eq!(
            Action::FormatPartition.effect(&Selected::Partition(1, 3), &m),
            Some(Effect::Wizard(Action::FormatPartition))
        );
        assert_eq!(Action::DeletePartition.effect(&Selected::Disk(1), &m), None, "disabled");
    }

    #[test]
    fn delete_all_goes_highest_number_first_and_simulates() {
        let m = model_of(vec![mbr_disk(0)]);
        let Some(Effect::Push(ops)) = Action::DeleteAllPartitions.effect(&Selected::Disk(0), &m) else {
            panic!("expected ops");
        };
        let numbers: Vec<u32> = ops
            .iter()
            .map(|o| match o {
                Operation::DeletePartition { number, .. } => *number,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(numbers, [6, 5, 2, 1]);
        // the whole batch is accepted by the planner, leaving an empty disk
        let mut q = pm_ops::OpQueue::new(m);
        for op in ops {
            q.push(op).expect("planner accepts");
        }
        assert!(q.preview().disk(0).unwrap().partitions.is_empty());
    }
}
