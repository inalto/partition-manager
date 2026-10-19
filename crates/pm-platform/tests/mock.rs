use pm_core::{
    BlockDevice, DiskInfo, FsKind, MemDevice, NullProgress, PartitionType, TableKind, VolumeId,
    VolumeInfo, MIB,
};
use pm_parttable::{write_table, Entry, Table};
use pm_platform::{ExternalTool, MockPlatform, MountPoint, OpenMode, Platform};

fn mbr_disk(len: u64, parts: &[(u64, u64, u8)]) -> MemDevice {
    let mut dev = MemDevice::new(len);
    let mut t = Table::empty(TableKind::Mbr, len, 512);
    for (i, &(start, plen, ty)) in parts.iter().enumerate() {
        t.entries.push(Entry {
            number: i as u32 + 1,
            start,
            len: plen,
            ptype: PartitionType::Mbr(ty),
            guid: None,
            name: String::new(),
            attrs: 0,
            logical: false,
        });
    }
    write_table(&mut dev, &t).unwrap();
    dev
}

fn meta(model: &str) -> DiskInfo {
    DiskInfo {
        model: model.into(),
        ..Default::default()
    }
}

#[test]
fn mock_enumerates_added_disks() {
    let p = MockPlatform::new();
    p.add_disk(0, mbr_disk(64 * MIB, &[(MIB, 10 * MIB, 0x07)]), meta("Disk A"));
    p.add_disk(1, MemDevice::new(32 * MIB), meta("Disk B"));
    p.set_system_disk(0);
    p.add_volume(
        VolumeInfo {
            id: VolumeId("vol-a1".into()),
            fs: Some(FsKind::Ntfs),
            label: "DATA".into(),
            mount_points: vec!["D:\\".into()],
            total: 10 * MIB,
            free: 4 * MIB,
            ..Default::default()
        },
        0,
        MIB,
        10 * MIB,
    );

    let disks = p.enumerate_disks().unwrap();
    assert_eq!(disks.len(), 2);
    let a = &disks[0];
    assert_eq!(a.index, 0);
    assert_eq!(a.model, "Disk A");
    assert_eq!(a.size, 64 * MIB);
    assert_eq!(a.sector_size, 512);
    assert_eq!(a.table, TableKind::Mbr);
    assert!(a.flags.system);
    assert_eq!(a.partitions.len(), 1);
    let part = &a.partitions[0];
    assert_eq!((part.start, part.len), (MIB, 10 * MIB));
    assert_eq!(part.ptype, Some(PartitionType::Mbr(0x07)));
    assert_eq!(part.volume, Some(VolumeId("vol-a1".into())));
    assert_eq!(part.fs, Some(FsKind::Ntfs));
    assert_eq!(part.label, "DATA");
    assert_eq!(part.mount_points, vec!["D:\\".to_string()]);
    assert_eq!(part.used, Some(6 * MIB));

    let b = &disks[1];
    assert_eq!(b.index, 1);
    assert_eq!(b.table, TableKind::None);
    assert!(!b.flags.system);
    assert!(b.partitions.is_empty());

    let vols = p.enumerate_volumes().unwrap();
    assert_eq!(vols.len(), 1);
    assert_eq!(vols[0].disk, Some(0));
    assert_eq!((vols[0].start, vols[0].len), (MIB, 10 * MIB));

    // the table is re-read on every enumeration
    {
        let mut s = p.begin_write(1).unwrap();
        let mut t = Table::empty(TableKind::Mbr, 32 * MIB, 512);
        t.entries.push(Entry {
            number: 1,
            start: MIB,
            len: 8 * MIB,
            ptype: PartitionType::Mbr(0x0C),
            guid: None,
            name: String::new(),
            attrs: 1,
            logical: false,
        });
        s.commit_table(&t).unwrap();
        s.finish().unwrap();
    }
    let disks = p.enumerate_disks().unwrap();
    assert_eq!(disks[1].table, TableKind::Mbr);
    assert_eq!(disks[1].partitions.len(), 1);
    assert!(disks[1].partitions[0].flags.active);
}

#[test]
fn mock_mount_unmount_tracked() {
    let p = MockPlatform::new();
    p.add_disk(0, mbr_disk(16 * MIB, &[(MIB, 8 * MIB, 0x07)]), meta("D"));
    let id = VolumeId("vol-1".into());
    p.add_volume(
        VolumeInfo {
            id: id.clone(),
            ..Default::default()
        },
        0,
        MIB,
        8 * MIB,
    );
    assert_eq!(p.free_letters(), vec!['E', 'F', 'G']);
    p.mount(&id, &MountPoint::Letter('F')).unwrap();
    p.mount(&id, &MountPoint::Folder(r"C:\mnt\x".into())).unwrap();
    assert_eq!(
        p.mounted(),
        vec![
            (id.clone(), MountPoint::Letter('F')),
            (id.clone(), MountPoint::Folder(r"C:\mnt\x".into()))
        ]
    );
    assert_eq!(p.free_letters(), vec!['E', 'G']);
    let vols = p.enumerate_volumes().unwrap();
    assert_eq!(vols[0].mount_points, vec![r"F:\".to_string(), r"C:\mnt\x\".to_string()]);
    let disks = p.enumerate_disks().unwrap();
    assert_eq!(disks[0].partitions[0].letter(), Some('F'));

    p.unmount(&id, &MountPoint::Letter('F')).unwrap();
    assert_eq!(p.mounted(), vec![(id.clone(), MountPoint::Folder(r"C:\mnt\x".into()))]);
    assert_eq!(p.free_letters(), vec!['E', 'F', 'G']);
    // unmounting something that is not mounted, or an unknown volume, is an error
    assert!(p.unmount(&id, &MountPoint::Letter('F')).is_err());
    assert!(p.mount(&VolumeId("nope".into()), &MountPoint::Letter('E')).is_err());
}

#[test]
fn mock_begin_write_logs_disk() {
    let p = MockPlatform::new();
    p.add_disk(2, MemDevice::new(8 * MIB), meta("X"));
    p.add_disk(5, MemDevice::new(8 * MIB), meta("Y"));
    assert!(p.write_sessions_log().is_empty());
    {
        let mut s = p.begin_write(5).unwrap();
        assert_eq!(s.disk(), 5);
        s.device().write_at(4096, b"session").unwrap();
        s.finish().unwrap();
    }
    p.begin_write(2).unwrap().finish().unwrap();
    assert_eq!(p.write_sessions_log(), vec![5, 2]);
    assert_eq!(&p.disk_bytes(5)[4096..4103], b"session");
    assert!(p.begin_write(9).is_err());

    // other side effects are recorded as calls
    let id = VolumeId("v".into());
    p.format(&id, FsKind::Ntfs, "Data", None, true, &NullProgress).unwrap();
    p.shrink_volume(&id, 4 * MIB).unwrap();
    p.extend_volume(&id, 6 * MIB).unwrap();
    p.run_tool(ExternalTool::BcdBoot, &[r"C:\Windows".into(), "/f".into(), "UEFI".into()])
        .unwrap();
    assert_eq!(
        p.calls(),
        vec![
            "format v NTFS label=Data quick=true".to_string(),
            format!("shrink v {}", 4 * MIB),
            format!("extend v {}", 6 * MIB),
            r#"run_tool BcdBoot ["C:\\Windows", "/f", "UEFI"]"#.to_string(),
        ]
    );
    p.fail_calls_matching("run_tool");
    assert!(p.run_tool(ExternalTool::BootSect, &[]).is_err());
}

#[test]
fn mock_open_disk_returns_device_with_len() {
    let p = MockPlatform::new();
    p.add_disk(3, MemDevice::new(4 * MIB).with_sector_size(4096), meta("Z"));
    let mut d = p.open_disk(3, OpenMode::ReadWrite).unwrap();
    assert_eq!(d.len(), 4 * MIB);
    assert_eq!(d.sector_size(), 4096);
    assert!(!d.is_read_only());
    d.write_at(MIB, b"abc").unwrap();
    drop(d);
    assert_eq!(&p.disk_bytes(3)[MIB as usize..MIB as usize + 3], b"abc");

    let mut ro = p.open_disk(3, OpenMode::ReadOnly).unwrap();
    assert!(ro.is_read_only());
    assert!(ro.write_at(0, b"x").is_err());
    let mut b = [0u8; 3];
    ro.read_at(MIB, &mut b).unwrap();
    assert_eq!(&b, b"abc");
    assert!(p.open_disk(7, OpenMode::ReadOnly).is_err());

    // volumes open as a window onto their disk
    let id = VolumeId("v3".into());
    p.add_volume(VolumeInfo { id: id.clone(), ..Default::default() }, 3, MIB, MIB);
    let mut v = p.open_volume(&id, OpenMode::ReadOnly).unwrap();
    assert_eq!(v.len(), MIB);
    v.read_at(0, &mut b).unwrap();
    assert_eq!(&b, b"abc");
}
