use pm_core::{
    gpt_types, BlockDevice, DiskBus, Error, FileDevice, FsKind, NullProgress, PartitionType,
    TableKind, VolumeId, MIB,
};
use pm_parttable::{read_table, write_table, Entry, Table};
use pm_platform::{ExternalTool, FilePlatform, MountPoint, OpenMode, Platform};
use std::path::PathBuf;

fn gpt_image(path: &PathBuf, len: u64) {
    let mut dev = FileDevice::create(path, len).unwrap();
    let mut t = Table::empty(TableKind::Gpt, len, 512);
    t.entries.push(Entry {
        number: 1,
        start: 2 * MIB,
        len: 10 * MIB,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        guid: Some(uuid::Uuid::from_u128(7)),
        name: "Data".into(),
        attrs: 0,
        logical: false,
    });
    write_table(&mut dev, &t).unwrap();
    dev.flush().unwrap();
}

#[test]
fn file_platform_enumerates_image() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.img");
    let b = dir.path().join("b.img");
    gpt_image(&a, 64 * MIB);
    FileDevice::create(&b, 32 * MIB).unwrap();

    let p = FilePlatform::new(vec![a.clone(), b]).unwrap();
    let disks = p.enumerate_disks().unwrap();
    assert_eq!(disks.len(), 2);
    assert_eq!(disks[0].index, 0);
    assert_eq!(disks[0].model, "a.img");
    assert_eq!(disks[0].bus, DiskBus::Virtual);
    assert_eq!(disks[0].size, 64 * MIB);
    assert_eq!(disks[0].sector_size, 512);
    assert_eq!(disks[0].table, TableKind::Gpt);
    assert_eq!(disks[0].partitions.len(), 1);
    assert_eq!(disks[0].partitions[0].start, 2 * MIB);
    assert_eq!(disks[0].partitions[0].name, "Data");
    assert_eq!(disks[1].index, 1);
    assert_eq!(disks[1].size, 32 * MIB);
    assert_eq!(disks[1].table, TableKind::None);
    assert!(disks[1].partitions.is_empty());

    assert!(p.enumerate_volumes().unwrap().is_empty());
    assert!(p.is_elevated());
    assert!(p.free_letters().contains(&'E'));
    // a sentinel that matches no file: the planner treats the system disk as known
    let sys = p.system_disk().expect("known system disk");
    assert!(disks.iter().all(|d| d.index != sys));
}

#[test]
fn file_platform_open_and_write_session() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.img");
    FileDevice::create(&a, 16 * MIB).unwrap();
    let p = FilePlatform::new(vec![a.clone()]).unwrap();

    let mut ro = p.open_disk(0, OpenMode::ReadOnly).unwrap();
    assert_eq!(ro.len(), 16 * MIB);
    assert!(ro.write_at(0, &[1]).is_err());
    let mut rw = p.open_disk(0, OpenMode::ReadWrite).unwrap();
    rw.write_at(100, b"hello").unwrap();
    rw.flush().unwrap();
    drop(rw);
    let mut buf = [0u8; 5];
    ro.read_at(100, &mut buf).unwrap();
    assert_eq!(&buf, b"hello");

    let mut s = p.begin_write(0).unwrap();
    assert_eq!(s.disk(), 0);
    let t = Table::empty(TableKind::Mbr, 16 * MIB, 512);
    s.commit_table(&t).unwrap();
    s.finish().unwrap();
    let mut dev = FileDevice::open(&a, true).unwrap();
    assert_eq!(read_table(&mut dev).unwrap().kind, TableKind::Mbr);
    assert_eq!(p.enumerate_disks().unwrap()[0].table, TableKind::Mbr);

    assert!(matches!(p.open_disk(5, OpenMode::ReadOnly), Err(Error::NotFound(_))));
    assert!(matches!(p.begin_write(5), Err(Error::NotFound(_))));
}

#[test]
fn file_platform_rejects_bad_files_and_unsupported_ops() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(FilePlatform::new(vec![]), Err(Error::Invalid(_))));
    assert!(matches!(
        FilePlatform::new(vec![dir.path().join("missing.img")]),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(FilePlatform::new(vec![dir.path().to_path_buf()]), Err(Error::Invalid(_))));

    let a = dir.path().join("a.img");
    FileDevice::create(&a, MIB).unwrap();
    let p = FilePlatform::new(vec![a]).unwrap();
    let id = VolumeId("x".into());
    let un = |r: Result<(), Error>| matches!(r, Err(Error::Unsupported(m)) if m.contains("image files"));
    assert!(un(p.mount(&id, &MountPoint::Letter('E'))));
    assert!(un(p.unmount(&id, &MountPoint::Letter('E'))));
    assert!(un(p.format(&id, FsKind::Ntfs, "", None, true, &NullProgress)));
    assert!(un(p.shrink_volume(&id, MIB)));
    assert!(un(p.extend_volume(&id, MIB)));
    assert!(matches!(p.run_tool(ExternalTool::BcdBoot, &[]), Err(Error::Unsupported(_))));
    assert!(p.smart(0).unwrap().is_none());
    p.rescan(0).unwrap();
    assert!(p.open_volume(&id, OpenMode::ReadOnly).is_err());
}
