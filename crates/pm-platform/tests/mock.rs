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

#[test]
fn mock_open_volume_handle_blocks_a_write_session_on_its_disk() {
    let p = MockPlatform::new();
    p.add_disk(3, MemDevice::new(4 * MIB), DiskInfo::default());
    p.add_disk(4, MemDevice::new(4 * MIB), DiskInfo::default());
    let id = VolumeId("v3".into());
    p.add_volume(VolumeInfo { id: id.clone(), ..Default::default() }, 3, MIB, MIB);

    let handle = p.open_volume(&id, OpenMode::ReadOnly).unwrap();
    assert_eq!(p.open_volume_handles(), vec![id.clone()]);
    // like FSCTL_LOCK_VOLUME: the volume cannot be locked while another handle is open
    match p.begin_write(3) {
        Err(pm_core::Error::Busy(m)) => assert!(m.contains("v3"), "{m}"),
        Err(e) => panic!("expected Busy, got {e}"),
        Ok(_) => panic!("the open volume handle must block the session"),
    }
    assert!(p.write_sessions_log().is_empty());
    // other disks are not affected
    p.begin_write(4).unwrap().finish().unwrap();

    drop(handle);
    assert!(p.open_volume_handles().is_empty());
    p.begin_write(3).unwrap().finish().unwrap();
    assert_eq!(p.write_sessions_log(), vec![4, 3]);
}

#[test]
fn mock_smart_returns_configured_data_per_disk() {
    use pm_platform::SmartData;
    let p = MockPlatform::new();
    assert!(p.smart(0).unwrap().is_none());
    p.set_smart(0, SmartData { healthy: Some(true), temperature_c: Some(40), ..Default::default() });
    let d = p.smart(0).unwrap().expect("data for disk 0");
    assert_eq!((d.healthy, d.temperature_c), (Some(true), Some(40)));
    assert!(p.smart(1).unwrap().is_none());
}

/// A device whose sector at byte 4096 cannot be read.
struct OneBadSector(MemDevice);

impl BlockDevice for OneBadSector {
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> pm_core::Result<()> {
        if offset < 4096 + 512 && 4096 < offset + buf.len() as u64 {
            return Err(pm_core::Error::Invalid("bad sector".into()));
        }
        self.0.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> pm_core::Result<()> {
        self.0.write_at(offset, buf)
    }
    fn flush(&mut self) -> pm_core::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

#[test]
fn mock_add_device_backs_a_disk_with_any_device() {
    let p = MockPlatform::new();
    let dev = mbr_disk(8 * MIB, &[(MIB, 4 * MIB, 0x07)]);
    let mut raw = dev.as_slice().to_vec();
    p.add_device(2, Box::new(OneBadSector(dev)), meta("Faulty"));
    p.add_disk(0, MemDevice::new(MIB), meta("Plain"));
    let disks = p.enumerate_disks().unwrap();
    assert_eq!(disks.iter().map(|d| d.index).collect::<Vec<_>>(), [0, 2]);
    let d = disks.iter().find(|d| d.index == 2).unwrap();
    assert_eq!((d.size, d.table, d.partitions.len()), (8 * MIB, TableKind::Mbr, 1));
    // reads go to the device, errors included
    let mut dev = p.open_disk(2, OpenMode::ReadOnly).unwrap();
    let mut buf = vec![0u8; 1024];
    assert!(dev.read_at(4096, &mut buf).is_err());
    assert!(dev.read_at(8192, &mut buf).is_ok());
    // a snapshot zero-fills what cannot be read
    raw[4096..4608].fill(0);
    assert!(p.disk_bytes(2) == raw);
}

#[test]
fn disk_of_path_answers_from_the_registered_map() {
    use std::path::Path;
    let p = MockPlatform::new();
    assert_eq!(p.disk_of_path(Path::new("/mnt/usb/file.img")), None);
    p.set_disk_of_path("/mnt/usb", 2, Some(1));
    p.set_disk_of_path("/mnt/usb/nested", 3, None);
    assert_eq!(p.disk_of_path(Path::new("/mnt/usb")), Some((2, Some(1))));
    assert_eq!(p.disk_of_path(Path::new("/mnt/usb/dir/file.img")), Some((2, Some(1))));
    assert_eq!(p.disk_of_path(Path::new("/mnt/usb/nested/x")), Some((3, None)), "deepest wins");
    assert_eq!(p.disk_of_path(Path::new("/mnt/other")), None);
    // the trait default: unknown
    assert_eq!(pm_platform::NullPlatform.disk_of_path(Path::new("/")), None);
}
