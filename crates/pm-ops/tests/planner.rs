//! Planner, sizing and operation tests over a hand-built [`Model`].

use pm_core::{
    gpt_types, DiskInfo, Error, FsKind, PartFlags, PartitionInfo, PartitionType, TableKind,
    VolumeId, VolumeInfo, GIB, MIB,
};
use pm_image::ImageFormat;
use pm_ops::{
    plan_disk_clone, table_from_disk, BootFix, CloneMode, LayoutSpec, Model, Operation, Planner,
    Sizing, WipeMethod,
};
use pm_platform::MountPoint;
use std::path::PathBuf;

const WIN_VOL: &str = r"\\?\Volume{11111111-0000-0000-0000-000000000003}\";

fn gpt(t: uuid::Uuid) -> Option<PartitionType> {
    Some(PartitionType::Gpt(t))
}

fn part(number: u32, start: u64, len: u64, ptype: Option<PartitionType>) -> PartitionInfo {
    PartitionInfo {
        number,
        start,
        len,
        ptype,
        guid: Some(uuid::Uuid::new_v4()),
        ..Default::default()
    }
}

fn disk(index: u32, size: u64, table: TableKind, partitions: Vec<PartitionInfo>) -> DiskInfo {
    DiskInfo {
        index,
        path: format!(r"\\.\PhysicalDrive{index}"),
        model: format!("Test Disk {index}"),
        size,
        sector_size: 512,
        physical_sector_size: 512,
        table,
        partitions,
        ..Default::default()
    }
}

/// Disk 0: 100 GiB GPT system disk (EFI, MSR, Windows C:, WinRE). Disks 1, 2, 3: empty GPT
/// disks of 50, 200 and 20 GiB.
fn model() -> Model {
    let mut efi = part(1, MIB, 100 * MIB, gpt(gpt_types::EFI_SYSTEM));
    efi.fs = Some(FsKind::Fat32);
    efi.used = Some(30 * MIB);
    let msr = part(2, 101 * MIB, 16 * MIB, gpt(gpt_types::MSR));
    let mut win = part(3, 117 * MIB, 60 * GIB, gpt(gpt_types::BASIC_DATA));
    win.fs = Some(FsKind::Ntfs);
    win.label = "Windows".into();
    win.used = Some(25 * GIB);
    // GPT "required" attribute (kept by clones); not used to recognise Windows
    win.flags.required = true;
    win.mount_points = vec!["C:\\".into()];
    win.volume = Some(VolumeId(WIN_VOL.into()));
    let mut re = part(4, 117 * MIB + 60 * GIB, GIB, gpt(gpt_types::WINDOWS_RECOVERY));
    re.fs = Some(FsKind::Ntfs);
    re.used = Some(500 * MIB);
    Model {
        disks: vec![
            disk(0, 100 * GIB, TableKind::Gpt, vec![efi, msr, win, re]),
            disk(1, 50 * GIB, TableKind::Gpt, vec![]),
            disk(2, 200 * GIB, TableKind::Gpt, vec![]),
            disk(3, 20 * GIB, TableKind::Gpt, vec![]),
        ],
        volumes: vec![VolumeInfo {
            id: VolumeId(WIN_VOL.into()),
            disk: Some(0),
            start: 117 * MIB,
            len: 60 * GIB,
            fs: Some(FsKind::Ntfs),
            label: "Windows".into(),
            mount_points: vec!["C:\\".into()],
            total: 60 * GIB,
            free: 35 * GIB,
        }],
        system_disk: Some(0),
        windows_found: Vec::new(),
    }
}

fn create(disk: u32, start: u64, len: u64, letter: Option<char>) -> Operation {
    Operation::CreatePartition {
        disk,
        start,
        len,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        fs: Some(FsKind::Ntfs),
        label: "Data".into(),
        letter,
        name: "Basic data partition".into(),
    }
}

fn invalid_msg<T: std::fmt::Debug>(r: pm_core::Result<T>) -> String {
    match r {
        Err(Error::Invalid(m)) => {
            assert!(!m.contains("  "), "broken message spacing: {m:?}");
            m
        }
        other => panic!("expected Error::Invalid, got {other:?}"),
    }
}

/// End of the area usable for partitions on an empty GPT disk of `size` bytes.
fn gpt_usable_end(size: u64) -> u64 {
    size - 33 * 512
}

fn used_or_len(p: &PartitionInfo) -> u64 {
    p.used.unwrap_or(p.len)
}

fn assert_layout_sane(t: &pm_parttable::Table) {
    t.validate().expect("planned table validates");
    let mut entries = t.entries.clone();
    entries.sort_by_key(|e| e.start);
    for e in &entries {
        assert_eq!(e.start % MIB, 0, "partition {} start aligned", e.number);
        assert!(e.start >= t.gpt_first_usable && e.end() <= t.gpt_last_usable);
    }
    for w in entries.windows(2) {
        assert!(w[0].end() <= w[1].start, "no overlap");
    }
}

#[test]
fn simulate_create_in_free_space_assigns_number() {
    let m = model();
    // Free space on disk 0 starts after WinRE.
    let free_start = 117 * MIB + 61 * GIB;
    let out = Planner::simulate(&m, &create(0, free_start, 10 * GIB, Some('d'))).unwrap();
    let d0 = out.disk(0).unwrap();
    assert_eq!(d0.partitions.len(), 5);
    let p = out.partition(0, 5).expect("new partition is number 5");
    assert_eq!((p.start, p.len), (free_start, 10 * GIB));
    assert_eq!(p.fs, Some(FsKind::Ntfs));
    assert_eq!(p.label, "Data");
    assert_eq!(p.name, "Basic data partition");
    assert_eq!(p.mount_points, vec!["D:\\".to_string()]);
    assert!(p.guid.is_some());
    assert!(!p.flags.active);
    // The input model is untouched.
    assert_eq!(m.disk(0).unwrap().partitions.len(), 4);

    // Empty disk: first partition is number 1; unaligned values are aligned (start up, len down).
    let out = Planner::simulate(&m, &create(1, MIB + 5, 2 * GIB, None)).unwrap();
    let p = out.partition(1, 1).unwrap();
    assert_eq!(p.start, 2 * MIB);
    assert_eq!(p.len, 2 * GIB - MIB);
    assert!(p.mount_points.is_empty());

    // Whole free range of an empty disk (end not MiB aligned on GPT) is accepted.
    let (s, l) = m.disk(2).unwrap().free_ranges(MIB)[0];
    let out = Planner::simulate(&m, &create(2, s, l, None)).unwrap();
    let p = out.partition(2, 1).unwrap();
    assert!(p.end() <= gpt_usable_end(200 * GIB));
    assert_eq!(p.len % MIB, 0);

    // Less than 1 MiB after alignment.
    let msg = invalid_msg(Planner::simulate(&m, &create(1, MIB + 1, MIB, None)));
    assert!(msg.contains("Disk 1"), "{msg}");
}

#[test]
fn simulate_create_overlap_rejected() {
    let m = model();
    // Overlaps the Windows partition.
    let msg = invalid_msg(Planner::simulate(&m, &create(0, 10 * GIB, GIB, None)));
    assert!(msg.contains("Disk 0"), "{msg}");
    // Past the end of the disk.
    let msg = invalid_msg(Planner::simulate(&m, &create(1, 49 * GIB, 2 * GIB, None)));
    assert!(msg.contains("Disk 1"), "{msg}");
    // Unknown disk.
    let msg = invalid_msg(Planner::simulate(&m, &create(9, MIB, GIB, None)));
    assert!(msg.contains("Disk 9"), "{msg}");
    // Drive letter already in use.
    let msg = invalid_msg(Planner::simulate(&m, &create(1, MIB, GIB, Some('C'))));
    assert!(msg.contains("C:"), "{msg}");
    // Wrong partition type family for the table.
    let mut op = create(1, MIB, GIB, None);
    if let Operation::CreatePartition { ptype, .. } = &mut op {
        *ptype = PartitionType::Mbr(0x07);
    }
    invalid_msg(Planner::simulate(&m, &op));
    // Two creates in the same space: the second overlaps the first.
    let ops = [create(1, MIB, GIB, None), create(1, 512 * MIB, GIB, None)];
    invalid_msg(Planner::simulate_all(&m, &ops));
    // A drive letter taken by an earlier queued create.
    let ops = [create(1, MIB, GIB, Some('E')), create(2, MIB, GIB, Some('E'))];
    let msg = invalid_msg(Planner::simulate_all(&m, &ops));
    assert!(msg.contains("E:"), "{msg}");

    // MBR: at most 4 primaries.
    let mut m = model();
    let parts = (1..=4)
        .map(|n| {
            let mut p = part(n, n as u64 * GIB, GIB, Some(PartitionType::Mbr(0x07)));
            p.guid = None;
            p
        })
        .collect();
    m.disks[1] = disk(1, 50 * GIB, TableKind::Mbr, parts);
    let mut op = create(1, 10 * GIB, GIB, None);
    if let Operation::CreatePartition { ptype, .. } = &mut op {
        *ptype = PartitionType::Mbr(0x07);
    }
    let msg = invalid_msg(Planner::simulate(&m, &op));
    assert!(msg.contains("4 primary"), "{msg}");
}

#[test]
fn simulate_delete_then_create_reuses_space() {
    let m = model();
    let re = m.partition(0, 4).unwrap().clone();
    let ops = [
        Operation::DeletePartition { disk: 0, number: 4 },
        create(0, re.start, re.len, Some('R')),
    ];
    let out = Planner::simulate_all(&m, &ops).unwrap();
    let p = out.partition(0, 4).expect("max(existing) + 1 = 4");
    assert_eq!((p.start, p.len), (re.start, re.len));
    assert_eq!(p.fs, Some(FsKind::Ntfs));
    assert_eq!(out.disk(0).unwrap().partitions.len(), 4);

    // Deleting a partition releases its letter for a later create.
    let mut m2 = model();
    m2.disks[1].partitions.push({
        let mut p = part(1, MIB, GIB, gpt(gpt_types::BASIC_DATA));
        p.mount_points = vec!["E:\\".into()];
        p
    });
    let ops = [
        Operation::DeletePartition { disk: 1, number: 1 },
        create(2, MIB, GIB, Some('E')),
    ];
    let out = Planner::simulate_all(&m2, &ops).unwrap();
    assert_eq!(out.partition(2, 1).unwrap().mount_points, vec!["E:\\".to_string()]);

    // Missing partition and the running Windows partition cannot be deleted.
    let msg = invalid_msg(Planner::simulate(&m, &Operation::DeletePartition { disk: 1, number: 7 }));
    assert!(msg.contains("partition 7") && msg.contains("Disk 1"), "{msg}");
    let msg = invalid_msg(Planner::simulate(&m, &Operation::DeletePartition { disk: 0, number: 3 }));
    assert!(msg.contains("partition 3") && msg.contains("Disk 0"), "{msg}");
    // Other partitions of the system disk can be deleted.
    Planner::simulate(&m, &Operation::DeletePartition { disk: 0, number: 4 }).unwrap();
}

#[test]
fn plan_clone_rejects_small_target() {
    let m = model();
    let src = m.disk(0).unwrap();
    // 25 GiB used + 10 % does not fit on 20 GiB.
    let err = plan_disk_clone(src, 20 * GIB, 512, LayoutSpec::new(Sizing::Fit, false), &used_or_len, None);
    let msg = invalid_msg(err);
    assert!(msg.contains("partition 3"), "{msg}");
    let err = plan_disk_clone(src, 20 * GIB, 512, LayoutSpec::new(Sizing::Proportional, true), &used_or_len, None);
    let msg = invalid_msg(err);
    assert!(msg.contains("partition 3"), "{msg}");
    // Exact needs the whole source layout.
    let msg = invalid_msg(plan_disk_clone(src, 50 * GIB, 512, LayoutSpec::new(Sizing::Exact, false), &used_or_len, None));
    assert!(msg.contains("partition 3"), "{msg}");

    // Through the planner.
    let op = Operation::CloneDisk {
        src_disk: 0,
        dst_disk: 3,
        sizing: Sizing::Fit,
        mode: CloneMode::Smart,
        boot_fix: BootFix::Uefi,
        target_table: None,
    };
    let msg = invalid_msg(Planner::simulate(&m, &op));
    assert!(msg.contains("partition 3"), "{msg}");

    // Partition clone: target smaller than the used data.
    let op = Operation::ClonePartition {
        src_disk: 0,
        src_number: 3,
        dst_disk: 1,
        dst_start: MIB,
        dst_len: 20 * GIB,
        mode: CloneMode::Smart,
    };
    let msg = invalid_msg(Planner::simulate(&m, &op));
    assert!(msg.contains("Target smaller than used data"), "{msg}");
    assert!(msg.contains("partition 3"), "{msg}");
    // Sector mode needs the full partition length.
    let op = Operation::ClonePartition {
        src_disk: 0,
        src_number: 3,
        dst_disk: 1,
        dst_start: MIB,
        dst_len: 30 * GIB,
        mode: CloneMode::Sector,
    };
    invalid_msg(Planner::simulate(&m, &op));
    // Smart clone into enough space creates a partition with the source's fs/label/used.
    let op = Operation::ClonePartition {
        src_disk: 0,
        src_number: 3,
        dst_disk: 1,
        dst_start: MIB,
        dst_len: 30 * GIB,
        mode: CloneMode::Smart,
    };
    let out = Planner::simulate(&m, &op).unwrap();
    let p = out.partition(1, 1).unwrap();
    assert_eq!((p.start, p.len), (MIB, 30 * GIB));
    assert_eq!(p.fs, Some(FsKind::Ntfs));
    assert_eq!(p.label, "Windows");
    assert_eq!(p.used, Some(25 * GIB));
    assert!(p.mount_points.is_empty());
}

#[test]
fn planner_guards_system_disk() {
    let m = model();
    let ops = [
        Operation::CloneDisk {
            src_disk: 2,
            dst_disk: 0,
            sizing: Sizing::Fit,
            mode: CloneMode::Smart,
            boot_fix: BootFix::None,
            target_table: None,
        },
        Operation::WipeDisk { disk: 0, method: WipeMethod::Zero },
        Operation::InitializeDisk { disk: 0, kind: TableKind::Mbr },
        Operation::RestoreImage {
            path: PathBuf::from(r"C:\does-not-exist.vhd"),
            dst_disk: 0,
            dst_start: None,
        },
        Operation::MigrateOs {
            src_disk: 2,
            dst_disk: 0,
            only_system_parts: true,
            sizing: Sizing::Proportional,
            boot_fix: BootFix::All,
            target_table: None,
            keep_ids: false,
        },
        Operation::ConvertTable { disk: 0, to: TableKind::Mbr },
    ];
    for op in &ops {
        let msg = invalid_msg(Planner::simulate(&m, op));
        assert!(msg.contains("Disk 0 is the running system disk"), "{op:?}: {msg}");
    }
    // The Windows partition is protected against format / wipe / delete / move.
    for op in [
        Operation::FormatPartition {
            disk: 0,
            number: 3,
            fs: FsKind::Ntfs,
            label: "x".into(),
            cluster: None,
            quick: true,
        },
        Operation::WipePartition { disk: 0, number: 3, method: WipeMethod::Zero },
        Operation::DeletePartition { disk: 0, number: 3 },
        Operation::ResizeMove { disk: 0, number: 3, new_start: 200 * MIB, new_len: 60 * GIB },
    ] {
        let msg = invalid_msg(Planner::simulate(&m, &op));
        assert!(msg.contains("partition 3") && msg.contains("Disk 0"), "{op:?}: {msg}");
    }
    // Harmless or repair operations on the system disk are allowed.
    let out = Planner::simulate(
        &m,
        &Operation::SetLabel { disk: 0, number: 3, label: "Win11".into() },
    )
    .unwrap();
    assert_eq!(out.partition(0, 3).unwrap().label, "Win11");
    Planner::simulate(&m, &Operation::RebuildMbr { disk: 0 }).unwrap();
    // Growing the Windows partition in place over WinRE overlaps it.
    invalid_msg(Planner::simulate(
        &m,
        &Operation::ResizeMove { disk: 0, number: 3, new_start: 117 * MIB, new_len: 61 * GIB },
    ));
    // Once WinRE is gone, extending the Windows partition in place (no move) is allowed.
    let ops = [
        Operation::DeletePartition { disk: 0, number: 4 },
        Operation::ResizeMove { disk: 0, number: 3, new_start: 117 * MIB, new_len: 70 * GIB },
    ];
    let out = Planner::simulate_all(&m, &ops).unwrap();
    assert_eq!(out.partition(0, 3).unwrap().len, 70 * GIB);
    // Formatting a non-Windows partition of the system disk is allowed.
    Planner::simulate(
        &m,
        &Operation::FormatPartition {
            disk: 0,
            number: 4,
            fs: FsKind::Ntfs,
            label: String::new(),
            cluster: None,
            quick: true,
        },
    )
    .unwrap();
    // Cloning the system disk to another disk is allowed (it is the source).
    Planner::simulate(
        &m,
        &Operation::CloneDisk {
            src_disk: 0,
            dst_disk: 2,
            sizing: Sizing::Proportional,
            mode: CloneMode::Smart,
            boot_fix: BootFix::Uefi,
            target_table: None,
        },
    )
    .unwrap();
}

#[test]
fn plan_disk_clone_proportional_keeps_efi_msr() {
    let m = model();
    let src = m.disk(0).unwrap();
    let layout =
        plan_disk_clone(src, 200 * GIB, 512, LayoutSpec::new(Sizing::Proportional, false), &used_or_len, None).unwrap();
    let t = &layout.table;
    assert_eq!(t.kind, TableKind::Gpt);
    assert_eq!(t.entries.len(), 4);
    assert_eq!(layout.map, vec![(1, 1), (2, 2), (3, 3), (4, 4)]);
    let by = |n: u32| t.entries.iter().find(|e| e.number == n).unwrap();
    assert!(by(1).ptype.is_efi());
    assert_eq!(by(1).len, 100 * MIB);
    assert!(by(2).ptype.is_msr());
    assert_eq!(by(2).len, 16 * MIB);
    assert!(by(4).ptype.is_recovery());
    assert_eq!(by(4).len, GIB);
    let win = by(3);
    assert!(win.len > 60 * GIB, "Windows grows: {}", win.len);
    assert_eq!(win.len % MIB, 0);
    // Required attribute preserved, GUIDs regenerated.
    assert_eq!(win.attrs & 1, 1);
    assert_ne!(win.guid, src.partitions[2].guid);
    assert!(t.disk_guid.is_some());
    // Source order preserved.
    assert!(by(1).start < by(2).start && by(2).start < win.start && win.start < by(4).start);
    // The whole target is used (only alignment slack left).
    assert!(gpt_usable_end(200 * GIB) - by(4).end() < 4 * MIB);
    assert_layout_sane(t);

    // Shrinking proportionally to 50 GiB keeps the fixed partitions too.
    let layout =
        plan_disk_clone(src, 50 * GIB, 512, LayoutSpec::new(Sizing::Proportional, false), &used_or_len, None).unwrap();
    let t = &layout.table;
    assert_eq!(t.entries.iter().find(|e| e.ptype.is_efi()).unwrap().len, 100 * MIB);
    let win = t.entries.iter().find(|e| e.number == 3).unwrap();
    assert!(win.len < 50 * GIB && win.len >= 25 * GIB + 25 * GIB / 10);
    assert_layout_sane(t);

    // only_system drops data partitions that are not Windows.
    let mut src2 = src.clone();
    let mut data = part(5, 117 * MIB + 61 * GIB, 10 * GIB, gpt(gpt_types::BASIC_DATA));
    data.fs = Some(FsKind::Ntfs);
    data.used = Some(GIB);
    data.mount_points = vec!["D:\\".into()];
    src2.partitions.push(data);
    let all =
        plan_disk_clone(&src2, 200 * GIB, 512, LayoutSpec::new(Sizing::Proportional, false), &used_or_len, None).unwrap();
    assert_eq!(all.table.entries.len(), 5);
    let sys =
        plan_disk_clone(&src2, 200 * GIB, 512, LayoutSpec::new(Sizing::Proportional, true), &used_or_len, None).unwrap();
    assert_eq!(sys.table.entries.len(), 4);
    assert!(sys.map.iter().all(|(s, _)| *s != 5));
    assert_layout_sane(&all.table);
    assert_layout_sane(&sys.table);

    // Exact keeps starts and lengths.
    let exact = plan_disk_clone(src, 200 * GIB, 512, LayoutSpec::new(Sizing::Exact, false), &used_or_len, None).unwrap();
    for p in &src.partitions {
        let e = exact.table.entries.iter().find(|e| e.number == p.number).unwrap();
        assert_eq!((e.start, e.len), (p.start, p.len));
    }
}

#[test]
fn plan_disk_clone_fit_uses_used_sizes() {
    let m = model();
    let src = m.disk(0).unwrap();
    let fixed = 100 * MIB + 16 * MIB + GIB;
    let layout = plan_disk_clone(src, 50 * GIB, 512, LayoutSpec::new(Sizing::Fit, false), &used_or_len, None).unwrap();
    let t = &layout.table;
    let win = t.entries.iter().find(|e| e.number == 3).unwrap();
    let min = 25 * GIB + 25 * GIB / 10; // used + 10 %
    assert!(win.len >= min, "Windows {} >= used + 10 %", win.len);
    assert!(win.len <= 50 * GIB - fixed);
    assert_eq!(win.len % MIB, 0);
    assert_eq!(t.entries.iter().find(|e| e.ptype.is_efi()).unwrap().len, 100 * MIB);
    assert_eq!(t.entries.iter().find(|e| e.ptype.is_msr()).unwrap().len, 16 * MIB);
    assert_eq!(t.entries.iter().find(|e| e.ptype.is_recovery()).unwrap().len, GIB);
    assert_layout_sane(t);

    // A bigger target: every partition keeps at least its source size, leftover to Windows.
    let big = plan_disk_clone(src, 200 * GIB, 512, LayoutSpec::new(Sizing::Fit, false), &used_or_len, None).unwrap();
    let win = big.table.entries.iter().find(|e| e.number == 3).unwrap();
    assert!(win.len > 60 * GIB);
    assert_layout_sane(&big.table);

    // Through the planner: the destination disk takes the planned layout and drops volumes.
    let op = Operation::MigrateOs {
        src_disk: 0,
        dst_disk: 1,
        only_system_parts: true,
        sizing: Sizing::Fit,
        boot_fix: BootFix::Uefi,
        target_table: None,
        keep_ids: false,
    };
    let out = Planner::simulate(&m, &op).unwrap();
    let d1 = out.disk(1).unwrap();
    assert_eq!(d1.table, TableKind::Gpt);
    assert_eq!(d1.partitions.len(), 4);
    let w = out.partition(1, 3).unwrap();
    assert_eq!(w.fs, Some(FsKind::Ntfs));
    assert_eq!(w.used, Some(25 * GIB));
    assert!(w.mount_points.is_empty());
    assert!(out.volumes.iter().all(|v| v.disk != Some(1)));
}

#[test]
fn simulate_all_chains_models() {
    let m = model();
    let ops = vec![
        Operation::WipeDisk { disk: 1, method: WipeMethod::Zero },
        Operation::InitializeDisk { disk: 1, kind: TableKind::Mbr },
        Operation::CreatePartition {
            disk: 1,
            start: MIB,
            len: 10 * GIB,
            ptype: PartitionType::Mbr(0x07),
            fs: Some(FsKind::Ntfs),
            label: "Data".into(),
            letter: Some('F'),
            name: String::new(),
        },
        Operation::SetActive { disk: 1, number: 1 },
        Operation::FormatPartition {
            disk: 1,
            number: 1,
            fs: FsKind::Fat32,
            label: "USB".into(),
            cluster: Some(4096),
            quick: true,
        },
        Operation::SetLetter { disk: 1, number: 1, letter: Some('G') },
        Operation::ConvertTable { disk: 1, to: TableKind::Gpt },
        Operation::ResizeMove { disk: 1, number: 1, new_start: 2 * MIB, new_len: 5 * GIB },
    ];
    let out = Planner::simulate_all(&m, &ops).unwrap();
    let d1 = out.disk(1).unwrap();
    assert_eq!(d1.table, TableKind::Gpt);
    assert_eq!(d1.partitions.len(), 1);
    let p = &d1.partitions[0];
    assert_eq!((p.start, p.len), (2 * MIB, 5 * GIB));
    assert_eq!(p.fs, Some(FsKind::Fat32));
    assert_eq!(p.label, "USB");
    assert_eq!(p.mount_points, vec!["G:\\".to_string()]);
    assert_eq!(p.ptype, Some(PartitionType::Gpt(gpt_types::BASIC_DATA)));

    // Same result as folding simulate by hand.
    let mut acc = m.clone();
    for op in &ops {
        acc = Planner::simulate(&acc, op).unwrap();
    }
    assert_eq!(acc.disks[1].partitions[0].start, p.start);

    // An error in the middle of the list stops the fold.
    let bad = [
        Operation::DeletePartition { disk: 0, number: 4 },
        Operation::DeletePartition { disk: 0, number: 4 },
    ];
    let msg = invalid_msg(Planner::simulate_all(&m, &bad));
    assert!(msg.contains("partition 4"), "{msg}");

    // Empty list returns the model unchanged.
    let same = Planner::simulate_all(&m, &[]).unwrap();
    assert_eq!(same.disks, m.disks);

    // SetActive is MBR only.
    invalid_msg(Planner::simulate(&m, &Operation::SetActive { disk: 0, number: 3 }));

    // Mount / unmount use the model's volumes.
    let vol = VolumeId(WIN_VOL.into());
    let out = Planner::simulate(
        &m,
        &Operation::Mount { volume: vol.clone(), at: MountPoint::Letter('W') },
    )
    .unwrap();
    assert!(out.volumes[0].mount_points.contains(&"W:\\".to_string()));
    let out = Planner::simulate(
        &out,
        &Operation::Unmount { volume: vol.clone(), at: MountPoint::Letter('W') },
    )
    .unwrap();
    assert!(!out.volumes[0].mount_points.contains(&"W:\\".to_string()));
    invalid_msg(Planner::simulate(
        &m,
        &Operation::Mount { volume: VolumeId("nope".into()), at: MountPoint::Letter('W') },
    ));
    invalid_msg(Planner::simulate(
        &m,
        &Operation::Mount { volume: vol, at: MountPoint::Letter('C') },
    ));

    // Images: the folder must exist; the model is unchanged.
    let dir = tempfile::tempdir().unwrap();
    let op = Operation::ImageDisk {
        disk: 0,
        path: dir.path().join("disk0.vhd"),
        format: ImageFormat::VhdDynamic,
        mode: CloneMode::Smart,
        rescue: None,
    };
    let out = Planner::simulate(&m, &op).unwrap();
    assert_eq!(out.disks, m.disks);
    let op = Operation::ImagePartition {
        disk: 0,
        number: 3,
        path: dir.path().join("missing").join("p3.img"),
        format: ImageFormat::Raw,
        mode: CloneMode::Sector,
        rescue: None,
    };
    invalid_msg(Planner::simulate(&m, &op));

    // Restore: the planner reads the image capacity.
    let raw = dir.path().join("small.img");
    drop(
        pm_image::create(
            &raw,
            &pm_image::CreateOpts { format: ImageFormat::Raw, capacity: 4 * MIB, sparse: false },
        )
        .unwrap(),
    );
    let out = Planner::simulate(
        &m,
        &Operation::RestoreImage { path: raw.clone(), dst_disk: 2, dst_start: None },
    )
    .unwrap();
    assert_eq!(out.disk(2).unwrap().table, TableKind::Unknown);
    assert!(out.disk(2).unwrap().partitions.is_empty());
    let out = Planner::simulate(
        &m,
        &Operation::RestoreImage { path: raw.clone(), dst_disk: 2, dst_start: Some(MIB) },
    )
    .unwrap();
    assert_eq!(out.disk(2).unwrap().partitions.len(), 1);
    // Target disk smaller than the image.
    let mut tiny = m.clone();
    tiny.disks[3].size = 2 * MIB;
    let msg = invalid_msg(Planner::simulate(
        &tiny,
        &Operation::RestoreImage { path: raw, dst_disk: 3, dst_start: None },
    ));
    assert!(msg.contains("Disk 3"), "{msg}");
}

#[test]
fn operation_summary_text() {
    let cases: Vec<(Operation, &str)> = vec![
        (Operation::InitializeDisk { disk: 2, kind: TableKind::Gpt }, "Initialize Disk 2 as GPT"),
        (
            Operation::CreatePartition {
                disk: 2,
                start: MIB,
                len: 500 * GIB,
                ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
                fs: Some(FsKind::Ntfs),
                label: "Data".into(),
                letter: None,
                name: String::new(),
            },
            "Create partition on Disk 2 at 1.00 MB, 500.00 GB, NTFS \"Data\"",
        ),
        (Operation::DeletePartition { disk: 3, number: 2 }, "Delete partition 2 on Disk 3"),
        (
            Operation::FormatPartition {
                disk: 3,
                number: 1,
                fs: FsKind::Ntfs,
                label: "windows".into(),
                cluster: None,
                quick: true,
            },
            "Format partition 1 on Disk 3 as NTFS \"windows\"",
        ),
        (
            // 65 MiB .. 3 TiB - 20 GiB (end shown, not length).
            Operation::ResizeMove {
                disk: 3,
                number: 1,
                new_start: 65 * MIB,
                new_len: 3 * (1u64 << 40) - 20 * GIB - 65 * MIB,
            },
            "Resize/move partition 1 on Disk 3 to 65.00 MB – 2.98 TB",
        ),
        (Operation::SetActive { disk: 3, number: 1 }, "Set partition 1 on Disk 3 active"),
        (
            Operation::Mount {
                volume: VolumeId(r"\\?\Volume{abc}\".into()),
                at: MountPoint::Letter('e'),
            },
            r"Mount \\?\Volume{abc}\ at E:\",
        ),
        (
            Operation::Unmount {
                volume: VolumeId(r"\\?\Volume{abc}\".into()),
                at: MountPoint::Letter('E'),
            },
            r"Unmount \\?\Volume{abc}\ from E:\",
        ),
        (Operation::WipeDisk { disk: 2, method: WipeMethod::Zero }, "Wipe Disk 2 (zeros)"),
        (Operation::ConvertTable { disk: 2, to: TableKind::Gpt }, "Convert Disk 2 to GPT"),
        (
            Operation::CloneDisk {
                src_disk: 1,
                dst_disk: 2,
                sizing: Sizing::Fit,
                mode: CloneMode::Smart,
                boot_fix: BootFix::Uefi,
                target_table: None,
            },
            "Clone Disk 1 → Disk 2 (fit, smart)",
        ),
        (
            Operation::MigrateOs {
                src_disk: 1,
                dst_disk: 2,
                only_system_parts: true,
                sizing: Sizing::Proportional,
                boot_fix: BootFix::Uefi,
                target_table: None,
                keep_ids: false,
            },
            "Migrate OS from Disk 1 → Disk 2 (system partitions only, proportional)",
        ),
        (
            Operation::CloneDisk {
                src_disk: 1,
                dst_disk: 2,
                sizing: Sizing::Fit,
                mode: CloneMode::Smart,
                boot_fix: BootFix::Uefi,
                target_table: Some(TableKind::Gpt),
            },
            "Clone Disk 1 → Disk 2 (fit, smart, to GPT)",
        ),
        (
            Operation::MigrateOs {
                src_disk: 1,
                dst_disk: 2,
                only_system_parts: false,
                sizing: Sizing::Exact,
                boot_fix: BootFix::None,
                target_table: Some(TableKind::Mbr),
                keep_ids: true,
            },
            "Migrate OS from Disk 1 → Disk 2 (all partitions, exact, to MBR, keep ids)",
        ),
        (
            Operation::ImageDisk {
                disk: 1,
                path: PathBuf::from(r"C:\img.vhd"),
                format: ImageFormat::VhdDynamic,
                mode: CloneMode::Smart,
                rescue: None,
            },
            r"Image Disk 1 → C:\img.vhd (VHD (dynamic), smart)",
        ),
        (
            Operation::RestoreImage {
                path: PathBuf::from(r"C:\img.vhd"),
                dst_disk: 2,
                dst_start: None,
            },
            r"Restore C:\img.vhd → Disk 2",
        ),
    ];
    for (op, want) in &cases {
        assert_eq!(op.summary(), *want);
    }

    // Destructive classification.
    assert!(Operation::DeletePartition { disk: 1, number: 1 }.is_destructive());
    assert!(Operation::WipeDisk { disk: 1, method: WipeMethod::Dod3 }.is_destructive());
    assert!(Operation::ConvertTable { disk: 1, to: TableKind::Mbr }.is_destructive());
    assert!(!create(1, MIB, GIB, None).is_destructive());
    assert!(!Operation::SetLabel { disk: 1, number: 1, label: String::new() }.is_destructive());
    // Align moves data and trims partition tails.
    assert!(Operation::AlignPartitions { disk: 1 }.is_destructive());
    assert!(!Operation::RebuildMbr { disk: 1 }.is_destructive());
    // ResizeMove: growing in place is not destructive; shrinking or moving is.
    let m = model();
    let re_start = 117 * MIB + 60 * GIB;
    let grow = Operation::ResizeMove { disk: 0, number: 4, new_start: re_start, new_len: 2 * GIB };
    let shrink =
        Operation::ResizeMove { disk: 0, number: 4, new_start: re_start, new_len: 512 * MIB };
    let mv = Operation::ResizeMove { disk: 0, number: 4, new_start: re_start + GIB, new_len: GIB };
    assert!(!grow.is_destructive_in(&m));
    assert!(shrink.is_destructive_in(&m));
    assert!(mv.is_destructive_in(&m));
    assert!(grow.is_destructive(), "without a model ResizeMove is conservatively destructive");

    // touches_disk.
    let clone = Operation::CloneDisk {
        src_disk: 1,
        dst_disk: 2,
        sizing: Sizing::Exact,
        mode: CloneMode::Sector,
        boot_fix: BootFix::None,
        target_table: None,
    };
    assert!(clone.touches_disk(1) && clone.touches_disk(2) && !clone.touches_disk(0));
    assert!(Operation::SetLabel { disk: 3, number: 1, label: String::new() }.touches_disk(3));

    // Operation lists round-trip through JSON (the CLI saves/loads them).
    let ops: Vec<Operation> = cases.into_iter().map(|(op, _)| op).collect();
    let json = serde_json::to_string(&ops).unwrap();
    let back: Vec<Operation> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ops);
}

#[test]
fn table_from_disk_round_trips() {
    let m = model();
    let d0 = m.disk(0).unwrap();
    let t = table_from_disk(d0);
    assert_eq!(t.kind, TableKind::Gpt);
    assert_eq!(t.entries.len(), 4);
    t.validate().unwrap();
    let back = t.to_disk_info(d0.clone());
    for (a, b) in back.partitions.iter().zip(&d0.partitions) {
        assert_eq!(
            (a.number, a.start, a.len, a.ptype, a.guid),
            (b.number, b.start, b.len, b.ptype, b.guid)
        );
        assert_eq!(a.flags.required, b.flags.required);
    }
}

#[test]
fn model_from_platform_and_serde() {
    let p = pm_platform::MockPlatform::new();
    p.add_disk(0, pm_core::MemDevice::new(8 * MIB), DiskInfo::default());
    p.set_system_disk(0);
    let m = Model::from_platform(&p).unwrap();
    assert_eq!(m.system_disk, Some(0));
    assert_eq!(m.disks.len(), 1);
    assert!(m.disk(0).is_some() && m.disk(1).is_none());

    let m = model();
    let json = serde_json::to_string(&m).unwrap();
    let back: Model = serde_json::from_str(&json).unwrap();
    assert_eq!(back.disks, m.disks);
    assert_eq!(back.system_disk, Some(0));
    let mut m2 = m.clone();
    m2.disk_mut(1).unwrap().model = "changed".into();
    assert_eq!(m2.disk(1).unwrap().model, "changed");
}

// ---- Fix round 1 -------------------------------------------------------------------------

fn mbr(b: u8) -> Option<PartitionType> {
    Some(PartitionType::Mbr(b))
}

fn mbr_part(number: u32, start: u64, len: u64, b: u8, logical: bool) -> PartitionInfo {
    PartitionInfo {
        number,
        start,
        len,
        ptype: mbr(b),
        logical,
        ..Default::default()
    }
}

/// `model()` plus Disk 4: a 20 GiB MBR disk with a primary, an extended container and two
/// logicals laid out with 1 MiB EBR gaps (`legacy == false`) or 63-sector gaps.
fn model_with_mbr(legacy: bool) -> Model {
    let gap = if legacy { 63 * 512 } else { MIB };
    let ext_start = 2 * GIB;
    let l5 = ext_start + gap;
    let l6 = l5 + GIB + gap;
    let mut m = model();
    m.disks.push(disk(
        4,
        20 * GIB,
        TableKind::Mbr,
        vec![
            mbr_part(1, MIB, GIB, 0x07, false),
            mbr_part(2, ext_start, 4 * GIB, 0x0F, false),
            mbr_part(5, l5, GIB, 0x07, true),
            mbr_part(6, l6, GIB, 0x07, true),
        ],
    ));
    m
}

#[test]
fn mbr_logicals_need_ebr_room() {
    let m = model_with_mbr(false);
    // Moving logical 6 into the EBR gap behind logical 5.
    let l6 = m.partition(4, 6).unwrap().clone();
    let op = Operation::ResizeMove { disk: 4, number: 6, new_start: l6.start - MIB, new_len: GIB };
    let msg = invalid_msg(Planner::simulate(&m, &op));
    assert!(msg.contains("Disk 4") && msg.contains("logical partition 6"), "{msg}");
    assert!(msg.contains("EBR"), "{msg}");
    // Growing logical 5 in place up to logical 6 eats the EBR slot too.
    let l5 = m.partition(4, 5).unwrap().clone();
    let op = Operation::ResizeMove { disk: 4, number: 5, new_start: l5.start, new_len: GIB + MIB };
    let msg = invalid_msg(Planner::simulate(&m, &op));
    assert!(msg.contains("Disk 4") && msg.contains("EBR"), "{msg}");
    // Moving it by a whole MiB while keeping the gap is fine.
    let op = Operation::ResizeMove { disk: 4, number: 6, new_start: l6.start + MIB, new_len: GIB };
    Planner::simulate(&m, &op).unwrap();

    // Exact clone of a legacy (63-sector gap) MBR layout cannot be written.
    let legacy = model_with_mbr(true);
    let src = legacy.disk(4).unwrap();
    let msg = invalid_msg(plan_disk_clone(src, 50 * GIB, 512, LayoutSpec::new(Sizing::Exact, false), &used_or_len, None));
    assert!(msg.contains("Disk 4") && msg.contains("Fit or Proportional"), "{msg}");
    let op = Operation::CloneDisk {
        src_disk: 4,
        dst_disk: 1,
        sizing: Sizing::Exact,
        mode: CloneMode::Sector,
        boot_fix: BootFix::Bios,
        target_table: None,
    };
    let msg = invalid_msg(Planner::simulate(&legacy, &op));
    assert!(msg.contains("Fit or Proportional"), "{msg}");
    // Proportional / Fit rebuild the container with 1 MiB EBR slots.
    for sizing in [Sizing::Proportional, Sizing::Fit] {
        let layout = plan_disk_clone(src, 50 * GIB, 512, LayoutSpec::new(sizing, false), &used_or_len, None).unwrap();
        let t = &layout.table;
        t.validate().unwrap();
        let ext = t.entries.iter().find(|e| e.ptype.is_extended_mbr()).unwrap();
        let mut logicals: Vec<_> = t.entries.iter().filter(|e| e.logical).collect();
        logicals.sort_by_key(|e| e.start);
        assert_eq!(logicals.iter().map(|e| e.number).collect::<Vec<_>>(), vec![5, 6]);
        assert!(logicals[0].start >= ext.start + MIB);
        assert!(logicals[1].start >= logicals[0].end() + MIB);
        assert!(logicals[1].end() <= ext.end());
        assert_eq!(layout.map.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![1, 5, 6]);
    }
}

#[test]
fn resize_refuses_shrink_with_unknown_used() {
    let mut m = model();
    let mut p = part(1, MIB, 10 * GIB, gpt(gpt_types::BASIC_DATA));
    p.fs = Some(FsKind::Ntfs);
    p.used = None;
    m.disks[1].partitions.push(p);
    let shrink = Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 5 * GIB };
    let msg = invalid_msg(Planner::simulate(&m, &shrink));
    assert!(msg.contains("partition 1") && msg.contains("Disk 1"), "{msg}");
    assert!(msg.contains("used space is unknown"), "{msg}");
    // Growing is fine.
    let grow = Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 12 * GIB };
    Planner::simulate(&m, &grow).unwrap();
    // A partition whose filesystem is unknown may not shrink either (only the table would
    // follow); it may grow.
    m.disks[1].partitions[0].fs = None;
    let msg = invalid_msg(Planner::simulate(&m, &shrink));
    assert!(msg.contains("filesystem unknown; refresh or check the volume"), "{msg}");
    Planner::simulate(&m, &grow).unwrap();
}

#[test]
fn system_disk_fallbacks() {
    let wipe = |d: u32| Operation::WipeDisk { disk: d, method: WipeMethod::Zero };
    // 1. No platform answer: the disk with a partition mounted at C:\ is the system disk.
    let mut m = model();
    m.system_disk = None;
    assert_eq!(m.effective_system_disk(), Some(0));
    let msg = invalid_msg(Planner::simulate(&m, &wipe(0)));
    assert!(msg.contains("Disk 0 is the running system disk"), "{msg}");
    Planner::simulate(&m, &wipe(1)).unwrap();
    // The Windows-partition guard keeps working too.
    invalid_msg(Planner::simulate(&m, &Operation::DeletePartition { disk: 0, number: 3 }));

    // 2. No partition carries C:\, but a volume on Disk 0 does.
    for p in &mut m.disks[0].partitions {
        p.mount_points.clear();
    }
    assert_eq!(m.effective_system_disk(), Some(0));
    let msg = invalid_msg(Planner::simulate(&m, &wipe(0)));
    assert!(msg.contains("Disk 0 is the running system disk"), "{msg}");
    Planner::simulate(&m, &wipe(2)).unwrap();

    // 3. Nothing known: every destructive whole-disk operation is refused on every disk.
    m.volumes.clear();
    assert_eq!(m.effective_system_disk(), None);
    for op in [
        wipe(1),
        Operation::InitializeDisk { disk: 2, kind: TableKind::Gpt },
        Operation::ConvertTable { disk: 1, to: TableKind::Mbr },
        Operation::CloneDisk {
            src_disk: 0,
            dst_disk: 2,
            sizing: Sizing::Fit,
            mode: CloneMode::Smart,
            boot_fix: BootFix::None,
            target_table: None,
        },
        Operation::MigrateOs {
            src_disk: 0,
            dst_disk: 2,
            only_system_parts: true,
            sizing: Sizing::Fit,
            boot_fix: BootFix::None,
            target_table: None,
            keep_ids: false,
        },
        Operation::RestoreImage { path: PathBuf::from("x.img"), dst_disk: 3, dst_start: None },
    ] {
        let msg = invalid_msg(Planner::simulate(&m, &op));
        assert!(msg.contains("cannot determine the running system disk"), "{op:?}: {msg}");
    }
    // Non-destructive operations still work.
    Planner::simulate(&m, &create(1, MIB, GIB, None)).unwrap();
}

#[test]
fn move_and_restore_are_mib_aligned() {
    let mut m = model();
    let mut p = part(1, MIB, 10 * GIB, gpt(gpt_types::BASIC_DATA));
    p.fs = Some(FsKind::Ntfs);
    p.used = Some(GIB);
    m.disks[1].partitions.push(p);
    let mv = |start: u64, len: u64| Operation::ResizeMove { disk: 1, number: 1, new_start: start, new_len: len };
    // Unaligned start on a move.
    let msg = invalid_msg(Planner::simulate(&m, &mv(2 * MIB + 512, 10 * GIB)));
    assert!(msg.to_lowercase().contains("partition 1") && msg.contains("Disk 1"), "{msg}");
    // Aligned start, unaligned end.
    invalid_msg(Planner::simulate(&m, &mv(2 * MIB, 10 * GIB + 512)));
    // Aligned move.
    Planner::simulate(&m, &mv(2 * MIB, 10 * GIB)).unwrap();
    // A move may end exactly at the (unaligned) end of the GPT usable area.
    let end = gpt_usable_end(50 * GIB);
    Planner::simulate(&m, &mv(10 * GIB, end - 10 * GIB)).unwrap();
    // In-place resize of a partition with a legacy start keeps the exemption.
    let mut legacy = model();
    let mut q = part(1, 63 * 512 * 1000, 10 * GIB, gpt(gpt_types::BASIC_DATA));
    q.fs = Some(FsKind::Ntfs);
    q.used = Some(GIB);
    legacy.disks[1].partitions.push(q.clone());
    Planner::simulate(
        &legacy,
        &Operation::ResizeMove { disk: 1, number: 1, new_start: q.start, new_len: 8 * GIB },
    )
    .unwrap();

    // RestoreImage into free space: the start is aligned up to 1 MiB.
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("p.img");
    let opts = |cap| pm_image::CreateOpts { format: ImageFormat::Raw, capacity: cap, sparse: false };
    drop(pm_image::create(&raw, &opts(4 * MIB)).unwrap());
    let out = Planner::simulate(
        &model(),
        &Operation::RestoreImage { path: raw, dst_disk: 2, dst_start: Some(MIB + 4096) },
    )
    .unwrap();
    let p = out.partition(2, 1).unwrap();
    assert_eq!((p.start, p.len), (2 * MIB, 4 * MIB));
    // Images under 1 MiB cannot become a partition.
    let tiny = dir.path().join("tiny.img");
    drop(pm_image::create(&tiny, &opts(512 * 1024)).unwrap());
    let msg = invalid_msg(Planner::simulate(
        &model(),
        &Operation::RestoreImage { path: tiny, dst_disk: 2, dst_start: Some(MIB) },
    ));
    assert!(msg.contains("Disk 2") && msg.contains("1 MB"), "{msg}");
}

#[test]
fn windows_partition_detection_order() {
    // Disk 0 with a 30 GiB NTFS data partition (flagged required + boot, as some OEM utility
    // partitions are) after a 10 GiB Windows partition.
    let with_data = |win_c: bool| {
        let mut m = model();
        let d0 = &mut m.disks[0];
        d0.partitions[2].len = 10 * GIB;
        d0.partitions[2].used = Some(5 * GIB);
        if !win_c {
            d0.partitions[2].mount_points.clear();
        }
        let mut data = part(5, 117 * MIB + 11 * GIB, 30 * GIB, gpt(gpt_types::BASIC_DATA));
        data.fs = Some(FsKind::Ntfs);
        data.used = Some(GIB);
        data.flags = PartFlags { required: true, boot: true, ..Default::default() };
        d0.partitions.insert(3, data);
        m.volumes.clear();
        m
    };
    let win_start = 117 * MIB;
    let says_windows = |_: u32, p: &PartitionInfo| p.start == win_start;

    // Rule 1: mounted at C:\ (the probe is not even asked).
    let m = with_data(true);
    let d = m.disk(0).unwrap();
    assert_eq!(pm_ops::windows_partition(d, None), Some(3));
    let never = |_: u32, _: &PartitionInfo| -> bool { panic!("rule 1 decides") };
    assert_eq!(pm_ops::windows_partition(d, Some(&never)), Some(3));
    invalid_msg(Planner::simulate(&m, &Operation::DeletePartition { disk: 0, number: 3 }));
    Planner::simulate(&m, &Operation::DeletePartition { disk: 0, number: 5 }).unwrap();

    // Rule 2: no C:\, the filesystem holding \Windows\System32 wins over the larger,
    // flagged data partition.
    let mut m = with_data(false);
    assert_eq!(pm_ops::windows_partition(m.disk(0).unwrap(), Some(&says_windows)), Some(3));
    m.probe_windows(&says_windows);
    assert_eq!(m.windows_found, vec![(0, win_start)]);
    assert_eq!(m.windows_partition_of(0), Some(3));
    invalid_msg(Planner::simulate(&m, &Operation::DeletePartition { disk: 0, number: 3 }));
    Planner::simulate(&m, &Operation::DeletePartition { disk: 0, number: 5 }).unwrap();
    // the probe answer survives serialization of the model
    let back: Model = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
    assert_eq!(back.windows_partition_of(0), Some(3));

    // Rule 3: nothing found: the largest NTFS data partition; the flags do not count.
    let mut m = with_data(false);
    m.probe_windows(&|_, _| false);
    assert!(m.windows_found.is_empty());
    assert_eq!(m.windows_partition_of(0), Some(5));
    assert_eq!(pm_ops::windows_partition(m.disk(0).unwrap(), None), Some(5));

    // Sizing uses the same rule: with only_system the data partition is dropped when C:\ or
    // the probe identifies Windows.
    for (m, probe) in [(with_data(true), None), (with_data(false), Some(&says_windows as &dyn Fn(u32, &PartitionInfo) -> bool))] {
        let sys = plan_disk_clone(m.disk(0).unwrap(), 200 * GIB, 512, LayoutSpec::new(Sizing::Fit, true), &used_or_len, probe)
            .unwrap();
        assert!(sys.map.iter().any(|(s, _)| *s == 3));
        assert!(sys.map.iter().all(|(s, _)| *s != 5));
    }
}

// ---- Target table kind and kept ids ------------------------------------------------------

fn mbr_kind(t: &PartitionInfo) -> u8 {
    match t.ptype {
        Some(PartitionType::Mbr(b)) => b,
        other => panic!("expected an MBR type, got {other:?}"),
    }
}

#[test]
fn plan_disk_clone_gpt_source_to_mbr_target() {
    let m = model();
    let src = m.disk(0).unwrap();
    let spec = LayoutSpec { target_table: Some(TableKind::Mbr), ..LayoutSpec::new(Sizing::Fit, false) };
    let l = plan_disk_clone(src, 200 * GIB, 512, spec, &used_or_len, None).unwrap();
    assert_eq!(l.table.kind, TableKind::Mbr);
    assert_ne!(l.table.mbr_signature, 0);
    // the MSR has no MBR equivalent and is dropped; types are mapped; Windows is active
    assert_eq!(l.map, vec![(1, 1), (3, 2), (4, 3)]);
    let e: Vec<_> = l.table.entries.iter().map(|e| (e.number, e.ptype, e.attrs, e.logical)).collect();
    assert_eq!(e[0], (1, PartitionType::Mbr(0xEF), 0, false));
    assert_eq!(e[1], (2, PartitionType::Mbr(0x07), 1, false), "the Windows partition is active");
    assert_eq!(e[2], (3, PartitionType::Mbr(0x27), 0, false));
    assert!(l.table.entries.iter().all(|e| e.guid.is_none() && e.name.is_empty()));
    assert_layout_sane(&l.table);

    // through the planner: the target becomes an MBR disk with 3 partitions
    let op = Operation::MigrateOs {
        src_disk: 0,
        dst_disk: 2,
        only_system_parts: true,
        sizing: Sizing::Fit,
        boot_fix: BootFix::None,
        target_table: Some(TableKind::Mbr),
        keep_ids: false,
    };
    let out = Planner::simulate(&m, &op).unwrap();
    let d2 = out.disk(2).unwrap();
    assert_eq!(d2.table, TableKind::Mbr);
    assert_eq!(d2.partitions.len(), 3);
    assert_eq!(mbr_kind(&d2.partitions[0]), 0xEF);
    assert!(d2.partitions[1].flags.active);
    assert_eq!(d2.partitions[1].fs, Some(FsKind::Ntfs));
    assert_eq!(op.shrinks_source(&m), Vec::<String>::new());

    // an MBR cannot address past 2 TiB: Exact placement there is refused
    let far = {
        let mut d = src.clone();
        d.partitions[3].start = 3 * 1024 * GIB;
        d
    };
    let spec_exact = LayoutSpec { target_table: Some(TableKind::Mbr), ..LayoutSpec::new(Sizing::Exact, false) };
    let msg = invalid_msg(plan_disk_clone(&far, 4 * 1024 * GIB, 512, spec_exact, &used_or_len, None));
    assert!(msg.contains("partition 4"), "{msg}");
    // Fit lays it out within the first 2 TiB instead
    let l = plan_disk_clone(&far, 4 * 1024 * GIB, 512, spec, &used_or_len, None).unwrap();
    assert!(l.table.entries.iter().all(|e| e.end() <= 2 * 1024 * GIB), "{:?}", l.table.entries);

    // more than four partitions do not fit an MBR
    let mut many = src.clone();
    for n in 5..=7 {
        let mut p = part(n, 80 * GIB + (n as u64) * GIB, GIB, gpt(gpt_types::BASIC_DATA));
        p.fs = Some(FsKind::Ntfs);
        many.partitions.push(p);
    }
    let msg = invalid_msg(plan_disk_clone(&many, 200 * GIB, 512, spec, &used_or_len, None));
    assert!(msg.contains("at most 4 partitions") && msg.contains("6 would be copied"), "{msg}");
    // ... unless only the system partitions go
    let sys = LayoutSpec { target_table: Some(TableKind::Mbr), ..LayoutSpec::new(Sizing::Fit, true) };
    assert_eq!(plan_disk_clone(&many, 200 * GIB, 512, sys, &used_or_len, None).unwrap().map.len(), 3);
}

#[test]
fn plan_disk_clone_mbr_source_to_gpt_target() {
    let mut m = model_with_mbr(false);
    m.disk_mut(4).unwrap().partitions[2].ptype = mbr(0x0B);
    m.disk_mut(4).unwrap().partitions[3].ptype = mbr(0x83);
    let src = m.disk(4).unwrap();
    for sizing in [Sizing::Fit, Sizing::Proportional, Sizing::Exact] {
        let spec = LayoutSpec { target_table: Some(TableKind::Gpt), ..LayoutSpec::new(sizing, false) };
        let l = plan_disk_clone(src, 50 * GIB, 512, spec, &used_or_len, None).unwrap();
        assert_eq!(l.table.kind, TableKind::Gpt, "{sizing:?}");
        assert!(l.table.disk_guid.is_some());
        // no container, the logicals are ordinary entries numbered in layout order
        assert_eq!(l.map, vec![(1, 1), (5, 2), (6, 3)], "{sizing:?}");
        let types: Vec<_> = l.table.entries.iter().map(|e| e.ptype).collect();
        assert_eq!(
            types,
            vec![
                PartitionType::Gpt(gpt_types::BASIC_DATA),
                PartitionType::Gpt(gpt_types::BASIC_DATA),
                PartitionType::Gpt(gpt_types::LINUX_FS)
            ],
            "{sizing:?}"
        );
        assert!(l.table.entries.iter().all(|e| !e.logical && e.guid.is_some()));
        assert_layout_sane(&l.table);
        if sizing == Sizing::Exact {
            let starts: Vec<_> = l.table.entries.iter().map(|e| e.start).collect();
            assert_eq!(starts, src.partitions.iter().filter(|p| p.number != 2).map(|p| p.start).collect::<Vec<_>>());
        }
    }
    // the planner's view
    let op = Operation::CloneDisk {
        src_disk: 4,
        dst_disk: 1,
        sizing: Sizing::Fit,
        mode: CloneMode::Sector,
        boot_fix: BootFix::None,
        target_table: Some(TableKind::Gpt),
    };
    let out = Planner::simulate(&m, &op).unwrap();
    let d1 = out.disk(1).unwrap();
    assert_eq!(d1.table, TableKind::Gpt);
    assert_eq!(d1.partitions.iter().map(|p| p.number).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert!(d1.partitions.iter().all(|p| !p.logical));
    // the same kind as the source is a no-op choice
    let same = LayoutSpec { target_table: Some(TableKind::Mbr), ..LayoutSpec::new(Sizing::Fit, false) };
    let a = plan_disk_clone(src, 50 * GIB, 512, same, &used_or_len, None).unwrap();
    let b = plan_disk_clone(src, 50 * GIB, 512, LayoutSpec::new(Sizing::Fit, false), &used_or_len, None).unwrap();
    assert_eq!(a.map, b.map);
    assert_eq!(a.table.entries.len(), b.table.entries.len());
    // a target kind that is no table at all
    let none = LayoutSpec { target_table: Some(TableKind::None), ..LayoutSpec::new(Sizing::Fit, false) };
    assert!(invalid_msg(plan_disk_clone(src, 50 * GIB, 512, none, &used_or_len, None)).contains("GPT or MBR"));
}

#[test]
fn plan_disk_clone_keeps_source_ids_on_request() {
    let mut m = model();
    let keep = LayoutSpec { keep_ids: true, ..LayoutSpec::new(Sizing::Fit, false) };
    // the model's disk ids are unknown: refused with a reason
    let msg = invalid_msg(plan_disk_clone(m.disk(0).unwrap(), 200 * GIB, 512, keep, &used_or_len, None));
    assert!(msg.contains("Cannot keep the ids of Disk 0") && msg.contains("GUID is unknown"), "{msg}");

    let disk_guid = uuid::Uuid::new_v4();
    m.disk_mut(0).unwrap().disk_id = disk_guid.to_string();
    let src = m.disk(0).unwrap();
    let l = plan_disk_clone(src, 200 * GIB, 512, keep, &used_or_len, None).unwrap();
    assert_eq!(l.table.disk_guid, Some(disk_guid));
    for &(sn, dn) in &l.map {
        let sp = src.partitions.iter().find(|p| p.number == sn).unwrap();
        let e = l.table.entries.iter().find(|e| e.number == dn).unwrap();
        assert_eq!(e.guid, sp.guid, "partition {sn} keeps its GUID");
    }
    // without the flag every id is fresh
    let fresh = plan_disk_clone(src, 200 * GIB, 512, LayoutSpec::new(Sizing::Fit, false), &used_or_len, None).unwrap();
    assert_ne!(fresh.table.disk_guid, Some(disk_guid));
    assert!(fresh.table.entries.iter().all(|e| !src.partitions.iter().any(|p| p.guid == e.guid)));
    // a partition without a GUID cannot be kept
    let mut no_guid = src.clone();
    no_guid.partitions[2].guid = None;
    let msg = invalid_msg(plan_disk_clone(&no_guid, 200 * GIB, 512, keep, &used_or_len, None));
    assert!(msg.contains("partition 3 has no GUID"), "{msg}");
    // not across table kinds
    let conv = LayoutSpec { target_table: Some(TableKind::Mbr), ..keep };
    let msg = invalid_msg(plan_disk_clone(src, 200 * GIB, 512, conv, &used_or_len, None));
    assert!(msg.contains("same partition table kind"), "{msg}");

    // MBR: the signature
    let mut m4 = model_with_mbr(false);
    m4.disk_mut(4).unwrap().disk_id = "DEADBEEF".into();
    let l = plan_disk_clone(m4.disk(4).unwrap(), 50 * GIB, 512, keep, &used_or_len, None).unwrap();
    assert_eq!(l.table.mbr_signature, 0xDEAD_BEEF);
    m4.disk_mut(4).unwrap().disk_id = "00000000".into();
    let msg = invalid_msg(plan_disk_clone(m4.disk(4).unwrap(), 50 * GIB, 512, keep, &used_or_len, None));
    assert!(msg.contains("signature is unknown"), "{msg}");

    // through the planner the copy carries the source's disk id
    let op = Operation::MigrateOs {
        src_disk: 0,
        dst_disk: 2,
        only_system_parts: false,
        sizing: Sizing::Fit,
        boot_fix: BootFix::None,
        target_table: None,
        keep_ids: true,
    };
    let out = Planner::simulate(&m, &op).unwrap();
    assert_eq!(out.disk(2).unwrap().disk_id, disk_guid.to_string());
    assert!(matches!(op.layout_spec(), Some(LayoutSpec { keep_ids: true, only_system: false, .. })));
}

#[test]
fn clone_operations_without_the_new_fields_still_deserialize() {
    let json = r#"{"MigrateOs":{"src_disk":0,"dst_disk":1,"only_system_parts":true,"sizing":"Fit","boot_fix":"Uefi"}}"#;
    let op: Operation = serde_json::from_str(json).unwrap();
    assert_eq!(
        op,
        Operation::MigrateOs {
            src_disk: 0,
            dst_disk: 1,
            only_system_parts: true,
            sizing: Sizing::Fit,
            boot_fix: BootFix::Uefi,
            target_table: None,
            keep_ids: false,
        }
    );
    let json = r#"{"CloneDisk":{"src_disk":0,"dst_disk":1,"sizing":"Exact","mode":"Sector","boot_fix":"None"}}"#;
    let op: Operation = serde_json::from_str(json).unwrap();
    assert!(matches!(op, Operation::CloneDisk { target_table: None, .. }));
    // and the new fields round-trip
    let op = Operation::CloneDisk {
        src_disk: 0,
        dst_disk: 1,
        sizing: Sizing::Fit,
        mode: CloneMode::Smart,
        boot_fix: BootFix::None,
        target_table: Some(TableKind::Gpt),
    };
    let back: Operation = serde_json::from_str(&serde_json::to_string(&op).unwrap()).unwrap();
    assert_eq!(back, op);
}

/// A disk whose table could not be read (`TableKind::Unknown`, macOS / Linux without root):
/// partition 1 at an approximate start, plus a superfloppy-style disk (`TableKind::None`, one
/// whole-disk partition at offset 0) and a Windows superfloppy (MBR, one partition at 0).
fn model_with_tableless_disks() -> Model {
    let mut m = model();
    let mut approx = part(1, 20480, 10 * GIB, None);
    approx.fs = Some(FsKind::Fat32);
    approx.used = Some(GIB);
    m.disks.push(disk(4, 20 * GIB, TableKind::Unknown, vec![approx]));
    let mut whole = part(1, 0, 8 * GIB, None);
    whole.guid = None;
    whole.fs = Some(FsKind::Fat32);
    whole.used = Some(GIB);
    m.disks.push(disk(5, 8 * GIB, TableKind::None, vec![whole.clone()]));
    let mut win_floppy = whole;
    win_floppy.ptype = mbr(0x0C);
    m.disks.push(disk(6, 8 * GIB, TableKind::Mbr, vec![win_floppy]));
    // a table-less disk whose single partition does not start at 0: not a whole-disk volume
    let mut odd = part(1, MIB, 4 * GIB, None);
    odd.fs = Some(FsKind::Fat32);
    odd.used = Some(GIB);
    m.disks.push(disk(7, 8 * GIB, TableKind::None, vec![odd]));
    m
}

fn wipe_part(disk: u32) -> Operation {
    Operation::WipePartition { disk, number: 1, method: WipeMethod::Zero }
}

fn image_part(disk: u32) -> Operation {
    Operation::ImagePartition {
        disk,
        number: 1,
        path: std::env::temp_dir().join("pm-planner-tableless.img"),
        format: ImageFormat::Raw,
        mode: CloneMode::Sector,
        rescue: None,
    }
}

fn clone_part(src_disk: u32) -> Operation {
    Operation::ClonePartition {
        src_disk,
        src_number: 1,
        dst_disk: 2,
        dst_start: MIB,
        dst_len: 20 * GIB,
        mode: CloneMode::Sector,
    }
}

#[test]
fn approximate_partitions_are_never_read_or_written() {
    let m = model_with_tableless_disks();
    for op in [
        wipe_part(4),
        image_part(4),
        clone_part(4),
        Operation::DeletePartition { disk: 4, number: 1 },
        Operation::ResizeMove { disk: 4, number: 1, new_start: 20480, new_len: 11 * GIB },
    ] {
        let msg = invalid_msg(Planner::simulate(&m, &op));
        assert!(msg.contains("Disk 4") && msg.contains("could not be read"), "{op:?}: {msg}");
    }
    // a table-less disk whose partition is not the whole disk
    for op in [wipe_part(7), image_part(7), clone_part(7)] {
        let msg = invalid_msg(Planner::simulate(&m, &op));
        assert!(msg.contains("Disk 7 is not initialized"), "{op:?}: {msg}");
    }
}

#[test]
fn whole_disk_volume_without_a_table_can_be_wiped_imaged_and_cloned() {
    let m = model_with_tableless_disks();
    for d in [5, 6] {
        Planner::simulate(&m, &wipe_part(d)).unwrap_or_else(|e| panic!("wipe disk {d}: {e}"));
        Planner::simulate(&m, &image_part(d)).unwrap_or_else(|e| panic!("image disk {d}: {e}"));
        Planner::simulate(&m, &clone_part(d)).unwrap_or_else(|e| panic!("clone disk {d}: {e}"));
    }
    // but no table edit on the table-less one: deleting or resizing would need a table
    for op in [
        Operation::DeletePartition { disk: 5, number: 1 },
        Operation::ResizeMove { disk: 5, number: 1, new_start: 0, new_len: 4 * GIB },
    ] {
        let msg = invalid_msg(Planner::simulate(&m, &op));
        assert!(msg.contains("Disk 5 is not initialized"), "{op:?}: {msg}");
    }
    // the Windows superfloppy (an MBR disk) keeps every operation it had
    Planner::simulate(&m, &Operation::DeletePartition { disk: 6, number: 1 }).unwrap();
}

#[test]
fn disk_indexes_need_not_be_contiguous() {
    // Linux keeps a removed disk's index for the life of the process: disks 0, 2 and 5
    let p = pm_platform::MockPlatform::new();
    for i in [5, 0, 2] {
        p.add_disk(i, pm_core::MemDevice::new(64 * MIB), DiskInfo::default());
    }
    p.set_system_disk(0);
    let m = Model::from_platform(&p).unwrap();
    assert_eq!(m.disks.iter().map(|d| d.index).collect::<Vec<_>>(), [0, 2, 5]);
    assert_eq!(m.disk(5).map(|d| d.index), Some(5));
    assert!(m.disk(1).is_none() && m.disk(3).is_none());
    // operations address disks by index, never by position
    let m = Planner::simulate(&m, &Operation::InitializeDisk { disk: 5, kind: TableKind::Gpt }).unwrap();
    assert_eq!(m.disk(5).unwrap().table, TableKind::Gpt);
    assert_eq!(m.disk(2).unwrap().table, TableKind::None);
    let msg = invalid_msg(Planner::simulate(&m, &Operation::InitializeDisk { disk: 1, kind: TableKind::Gpt }));
    assert!(msg.contains("Disk 1 does not exist"), "{msg}");
}
