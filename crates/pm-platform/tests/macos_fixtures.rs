//! macOS platform logic driven by hand-written `diskutil -plist` fixtures
//! (`tests/fixtures/macos/`). Runs on every OS: the parsers, the enumeration, the system-disk
//! lookup and the `diskutil` mount/format calls go through a [`DiskutilRunner`].

use pm_core::{gpt_types, BlockDevice, DiskBus, Error, FsKind, PartitionType, Result, TableKind, VolumeId};
use pm_parttable::{read_table, write_table, Entry, Table};
use pm_platform::macos::diskutil::{parse_info, parse_list};
use pm_platform::macos::{enumerate, format, mount, system_disk, AlignedDevice, DiskutilRunner};
use pm_platform::MountPoint;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

const D0: u64 = 500_277_790_720;
const EFI0: u64 = 314_572_800;
const APFS0: u64 = 499_963_174_912;
const D2: u64 = 16_008_609_792;
const S2: u64 = D2 - 1_048_576;
const D3: u64 = 1_000_204_886_016;
const EFI3: u64 = 209_715_200;
const NTFS3: u64 = 999_994_949_632;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/macos")
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Answers `diskutil list/info -plist` from the fixtures and records every call.
#[derive(Default)]
struct FixtureRunner {
    calls: Mutex<Vec<Vec<String>>>,
    /// Identifiers whose `info` fails.
    broken: Vec<&'static str>,
    /// Make every non-`-plist` command fail with this message.
    fail_commands: Option<&'static str>,
}

impl DiskutilRunner for FixtureRunner {
    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.calls.lock().unwrap().push(args.iter().map(|s| s.to_string()).collect());
        let not_found = |what: &str| Error::Platform { code: 1, msg: format!("diskutil failed: Could not find disk: {what}") };
        match args {
            ["list", "-plist"] => Ok(fixture("list.plist")),
            ["info", "-plist", what] => {
                let name = match *what {
                    "/" => "info-root.plist".to_string(),
                    "/Volumes/STICK" => "info-disk2s1.plist".to_string(),
                    w if w.starts_with("disk") && !self.broken.contains(&w) => format!("info-{w}.plist"),
                    _ => return Err(not_found(what)),
                };
                std::fs::read(fixture_dir().join(name)).map_err(|_| not_found(what))
            }
            _ => match self.fail_commands {
                Some(msg) => Err(Error::Platform { code: 1, msg: format!("diskutil failed: {msg}") }),
                None => Ok(Vec::new()),
            },
        }
    }
}

impl FixtureRunner {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

fn no_device(_: &str) -> Option<Box<dyn BlockDevice>> {
    None
}

#[test]
fn list_skips_synthesized_apfs_disks() {
    let disks = parse_list(&fixture("list.plist")).unwrap();
    let ids: Vec<&str> = disks.iter().map(|d| d.identifier.as_str()).collect();
    assert_eq!(ids, ["disk0", "disk2", "disk3"]);
    let d0 = &disks[0];
    assert_eq!((d0.size, d0.content.as_str()), (D0, "GUID_partition_scheme"));
    assert_eq!(d0.partitions.len(), 2);
    assert_eq!(d0.partitions[1].content, "Apple_APFS");
    assert_eq!(d0.partitions[1].mount_point, None);
    let s = &disks[1].partitions[0];
    assert_eq!(
        (s.identifier.as_str(), s.size, s.content.as_str(), s.volume_name.as_str()),
        ("disk2s1", S2, "Windows_FAT_32", "STICK")
    );
    assert_eq!(s.mount_point.as_deref(), Some("/Volumes/STICK"));
    assert_eq!(s.disk_uuid, None, "MBR slices have no partition GUID");
    assert_eq!(disks[2].partitions[1].disk_uuid.as_deref(), Some("B1B2C3D4-0000-4000-8000-000000000002"));
}

#[test]
fn info_field_mapping() {
    let d = parse_info(&fixture("info-disk0.plist")).unwrap();
    assert_eq!((d.identifier.as_str(), d.size, d.device_block_size), ("disk0", D0, 4096));
    assert_eq!((d.media_name.as_str(), d.bus_protocol.as_str()), ("APPLE SSD AP0512M", "PCI-Express"));
    assert!(d.whole_disk && d.internal && d.writable && d.solid_state && !d.removable);
    assert_eq!(d.mount_point, None, "empty MountPoint is no mount point");
    assert_eq!(d.content, "GUID_partition_scheme");

    let s = parse_info(&fixture("info-disk2s1.plist")).unwrap();
    assert_eq!((s.identifier.as_str(), s.parent_whole_disk.as_str()), ("disk2s1", "disk2"));
    assert!(!s.whole_disk && !s.internal && s.removable, "external slices are ejectable");
    assert_eq!(s.mount_point.as_deref(), Some("/Volumes/STICK"));
    assert_eq!((s.filesystem_type.as_str(), s.filesystem_name.as_str()), ("msdos", "MS-DOS FAT32"));
    assert_eq!((s.volume_name.as_str(), s.total_size, s.free_space), ("STICK", S2, Some(12_007_561_216)));
    assert_eq!(s.bus_protocol, "USB");

    let n = parse_info(&fixture("info-disk3s2.plist")).unwrap();
    assert_eq!(n.filesystem_type, "ntfs");
    assert_eq!(n.mount_point, None);
    assert_eq!(n.free_space, None);
    assert_eq!(n.disk_uuid.as_deref(), Some("B1B2C3D4-0000-4000-8000-000000000002"));
    assert_eq!(n.media_name, "Basic data partition");

    let root = parse_info(&fixture("info-root.plist")).unwrap();
    assert_eq!(root.identifier, "disk1s5");
    assert_eq!(root.parent_whole_disk, "disk1", "the synthesized container disk");
    assert_eq!(root.apfs_physical_stores, ["disk0s2"]);
    assert_eq!(root.mount_point.as_deref(), Some("/"));
}

#[test]
fn enumeration_without_device_access_is_approximate() {
    let r = FixtureRunner::default();
    let e = enumerate(&r, &no_device, Some(0)).unwrap();
    let idx: Vec<u32> = e.disks.iter().map(|d| d.index).collect();
    assert_eq!(idx, [0, 2, 3], "disk1 is a synthesized APFS container");

    let d0 = &e.disks[0];
    assert_eq!(d0.path, "/dev/rdisk0");
    assert_eq!((d0.model.as_str(), d0.bus, d0.size), ("APPLE SSD AP0512M", DiskBus::Nvme, D0));
    assert_eq!((d0.sector_size, d0.physical_sector_size), (4096, 4096));
    assert!(d0.flags.system && !d0.flags.removable && !d0.flags.read_only && d0.flags.trim);
    assert_eq!(d0.table, TableKind::Unknown, "offsets unknown without reading the table");
    // laid end to end from Apple's 4Kn GPT start (LBA 6)
    let p: Vec<(u32, u64, u64)> = d0.partitions.iter().map(|p| (p.number, p.start, p.len)).collect();
    assert_eq!(p, [(1, 6 * 4096, EFI0), (2, 6 * 4096 + EFI0, APFS0)]);
    assert_eq!(d0.partitions[0].ptype, Some(PartitionType::Gpt(gpt_types::EFI_SYSTEM)));
    assert_eq!(d0.partitions[0].fs, Some(FsKind::Fat32));
    assert_eq!(d0.partitions[0].name, "EFI System Partition");
    assert_eq!(d0.partitions[1].fs, Some(FsKind::Apfs), "APFS physical store");
    assert_eq!(d0.partitions[1].ptype, Some(PartitionType::Gpt(gpt_types::APPLE_APFS)));
    assert!(d0.partitions[1].mount_points.is_empty(), "nothing attached from the synthesized disk");
    assert_eq!(
        d0.partitions[1].guid,
        Some(Uuid::parse_str("A1B2C3D4-0000-4000-8000-000000000002").unwrap())
    );

    let d2 = &e.disks[1];
    assert_eq!((d2.index, d2.bus, d2.size, d2.sector_size), (2, DiskBus::Usb, D2, 512));
    assert!(d2.flags.removable && !d2.flags.system && !d2.flags.trim);
    let stick = &d2.partitions[0];
    assert_eq!((stick.number, stick.start, stick.len), (1, 512, S2));
    assert_eq!(stick.ptype, Some(PartitionType::Mbr(0x0C)));
    assert_eq!((stick.fs, stick.label.as_str()), (Some(FsKind::Fat32), "STICK"));
    assert_eq!(stick.mount_points, ["/Volumes/STICK"]);
    assert_eq!(stick.used, Some(S2 - 12_007_561_216));
    assert_eq!(stick.volume, Some(VolumeId("/dev/disk2s1".into())));

    let d3 = &e.disks[2];
    assert_eq!((d3.index, d3.bus, d3.size), (3, DiskBus::Usb, D3));
    assert_eq!(d3.model, "WD Elements 25A3");
    assert!(d3.flags.removable, "ejectable USB disk");
    let data = &d3.partitions[1];
    assert_eq!((data.number, data.start, data.len), (2, 40 * 512 + EFI3, NTFS3));
    assert_eq!((data.fs, data.label.as_str()), (Some(FsKind::Ntfs), "DATA"));
    assert!(data.mount_points.is_empty());
    assert_eq!(data.used, None, "fill level unknown while unmounted");
    assert_eq!(data.volume, Some(VolumeId("/dev/disk3s2".into())), "unmounted slices still get a volume id");
    assert_eq!(data.ptype, Some(PartitionType::Gpt(gpt_types::BASIC_DATA)));
    for d in &e.disks {
        assert!(d.partitions.iter().all(|p| p.end() <= d.size), "approximate layout stays inside disk {}", d.index);
    }

    // volumes: one per slice, the mounted one with total / free
    let ids: Vec<&str> = e.volumes.iter().map(|v| v.id.0.as_str()).collect();
    assert_eq!(ids, ["/dev/disk0s1", "/dev/disk0s2", "/dev/disk2s1", "/dev/disk3s1", "/dev/disk3s2"]);
    let v = &e.volumes[2];
    assert_eq!((v.disk, v.start, v.len, v.total, v.free), (Some(2), 512, S2, S2, 12_007_561_216));
    assert_eq!(v.mount_points, ["/Volumes/STICK"]);
    assert_eq!((e.volumes[4].total, e.volumes[4].free), (0, 0));

    // one list call, then info for every whole disk and slice (never for disk1)
    let calls = r.calls();
    assert_eq!(calls[0], ["list", "-plist"]);
    assert!(calls.iter().all(|c| !c.iter().any(|a| a.starts_with("disk1"))));
    assert_eq!(calls.len(), 1 + 3 + 5);
}

/// Sparse in-memory disk: unwritten sectors read as zeros.
#[derive(Clone)]
struct Sparse {
    len: u64,
    sectors: BTreeMap<u64, Vec<u8>>,
}

impl BlockDevice for Sparse {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        assert!(offset + buf.len() as u64 <= self.len);
        for (i, b) in buf.iter_mut().enumerate() {
            let pos = offset + i as u64;
            *b = self.sectors.get(&(pos / 512)).map_or(0, |s| s[(pos % 512) as usize]);
        }
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        assert!(offset + buf.len() as u64 <= self.len);
        for (i, b) in buf.iter().enumerate() {
            let pos = offset + i as u64;
            self.sectors.entry(pos / 512).or_insert_with(|| vec![0; 512])[(pos % 512) as usize] = *b;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

fn disk3_with_gpt() -> Sparse {
    let mut t = Table::empty(TableKind::Gpt, D3, 512);
    t.gpt_first_usable = 34 * 512;
    let entry = |number, start, len, ptype, guid: &str, name: &str| Entry {
        number,
        start,
        len,
        ptype: PartitionType::Gpt(ptype),
        guid: Some(Uuid::parse_str(guid).unwrap()),
        name: name.into(),
        attrs: 0,
        logical: false,
    };
    t.entries = vec![
        entry(1, 40 * 512, EFI3, gpt_types::EFI_SYSTEM, "B1B2C3D4-0000-4000-8000-000000000001", "EFI System Partition"),
        entry(2, 409_640 * 512, NTFS3, gpt_types::BASIC_DATA, "B1B2C3D4-0000-4000-8000-000000000002", "Basic data partition"),
    ];
    let mut dev = Sparse { len: D3, sectors: BTreeMap::new() };
    write_table(&mut dev, &t).unwrap();
    dev
}

#[test]
fn enumeration_reads_the_table_when_the_disk_is_readable() {
    let disk3 = disk3_with_gpt();
    let open = move |path: &str| -> Option<Box<dyn BlockDevice>> {
        (path == "/dev/rdisk3").then(|| Box::new(disk3.clone()) as Box<dyn BlockDevice>)
    };
    let e = enumerate(&FixtureRunner::default(), &open, Some(0)).unwrap();
    let d3 = e.disks.iter().find(|d| d.index == 3).unwrap();
    assert_eq!(d3.table, TableKind::Gpt);
    let p: Vec<(u32, u64, u64)> = d3.partitions.iter().map(|p| (p.number, p.start, p.len)).collect();
    assert_eq!(p, [(1, 40 * 512, EFI3), (2, 409_640 * 512, NTFS3)], "real offsets from the table");
    let data = &d3.partitions[1];
    assert_eq!((data.fs, data.label.as_str(), data.name.as_str()), (Some(FsKind::Ntfs), "DATA", "Basic data partition"));
    assert_eq!(data.volume, Some(VolumeId("/dev/disk3s2".into())));
    assert_eq!(e.volumes.iter().find(|v| v.id.0 == "/dev/disk3s2").unwrap().start, 409_640 * 512);
    // the other disks stay approximate
    assert_eq!(e.disks[0].table, TableKind::Unknown);
}

#[test]
fn failures_skip_disks_or_fail_the_listing() {
    let r = FixtureRunner { broken: vec!["disk2"], ..Default::default() };
    let e = enumerate(&r, &no_device, None).unwrap();
    assert_eq!(e.disks.iter().map(|d| d.index).collect::<Vec<_>>(), [0, 3]);
    assert!(e.disks.iter().all(|d| !d.flags.system), "no system disk known");

    struct NoDiskutil;
    impl DiskutilRunner for NoDiskutil {
        fn run(&self, _: &[&str]) -> Result<Vec<u8>> {
            Err(Error::Unsupported("diskutil is not installed".into()))
        }
    }
    assert!(matches!(enumerate(&NoDiskutil, &no_device, None), Err(Error::Unsupported(_))));
    assert_eq!(system_disk::system_disk(&NoDiskutil), None);
    assert_eq!(system_disk::disk_of_path(&NoDiskutil, Path::new("/Volumes/STICK")), None);
}

#[test]
fn system_disk_is_the_apfs_physical_store() {
    let r = FixtureRunner::default();
    assert_eq!(system_disk::system_disk(&r), Some(0));
    assert_eq!(system_disk::disk_of_path(&r, Path::new("/Volumes/STICK")), Some((2, Some(1))));
    // a path inside the volume: diskutil only knows the mount point, ancestors are tried
    assert_eq!(system_disk::disk_of_path(&r, Path::new("/Volumes/STICK/photos/a.jpg")), Some((2, Some(1))));
    // a path on the root volume resolves through the APFS physical store disk0s2
    assert_eq!(system_disk::disk_of_path(&r, Path::new("/Users/me/out.img")), Some((0, Some(2))));
}

#[test]
fn mount_unmount_format_through_diskutil() {
    let r = FixtureRunner::default();
    let id = VolumeId("/dev/disk2s1".into());
    mount::mount(&r, &id, &MountPoint::Folder("/Volumes/x".into())).unwrap();
    mount::unmount(&r, &VolumeId("/dev/rdisk2s1".into()), &MountPoint::Folder("/Volumes/x".into())).unwrap();
    format::format(&r, &id, FsKind::ExFat, "BACKUP", Some(131_072)).unwrap();
    format::format(&r, &id, FsKind::Fat32, "", None).unwrap();
    format::format(&r, &id, FsKind::HfsPlus, "", None).unwrap();
    assert_eq!(
        r.calls(),
        [
            vec!["mount", "-mountPoint", "/Volumes/x", "disk2s1"],
            vec!["unmount", "disk2s1"],
            vec!["eraseVolume", "ExFAT", "BACKUP", "disk2s1"],
            vec!["eraseVolume", "FAT32", "UNTITLED", "disk2s1"],
            vec!["eraseVolume", "JHFS+", "Untitled", "disk2s1"],
        ]
    );

    let r = FixtureRunner::default();
    assert!(matches!(mount::mount(&r, &id, &MountPoint::Letter('E')), Err(Error::Unsupported(_))));
    match format::format(&r, &id, FsKind::Ntfs, "X", None) {
        Err(Error::Unsupported(m)) => assert!(m.contains("third-party"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(format::format(&r, &id, FsKind::Ext4, "X", None), Err(Error::Unsupported(_))));
    assert!(matches!(
        mount::unmount(&r, &VolumeId(r"\\?\Volume{x}\".into()), &MountPoint::Letter('E')),
        Err(Error::NotFound(_))
    ));
    assert!(r.calls().is_empty(), "nothing ran for refused requests");

    // diskutil failures surface as platform errors
    let r = FixtureRunner { fail_commands: Some("Volume on disk2s1 failed to mount"), ..Default::default() };
    match mount::mount(&r, &id, &MountPoint::Folder("/Volumes/x".into())) {
        Err(Error::Platform { msg, .. }) => assert!(msg.contains("failed to mount"), "{msg}"),
        other => panic!("{other:?}"),
    }
}

/// Like a macOS raw disk node: offset and length must be multiples of the block size, else
/// `EINVAL`.
struct StrictRaw {
    data: Vec<u8>,
    block: u64,
    read_only: bool,
    ops: Vec<(char, u64, usize)>,
}

impl StrictRaw {
    fn new(len: usize, block: u64) -> Self {
        StrictRaw { data: (0..len).map(|i| (i % 251) as u8).collect(), block, read_only: false, ops: Vec::new() }
    }
    fn check(&self, offset: u64, len: usize) -> Result<()> {
        if offset % self.block != 0 || len as u64 % self.block != 0 {
            return Err(Error::io(offset, len, io::Error::from_raw_os_error(22)));
        }
        Ok(())
    }
}

impl BlockDevice for StrictRaw {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }
    fn sector_size(&self) -> u32 {
        self.block as u32
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check(offset, buf.len())?;
        self.ops.push(('r', offset, buf.len()));
        buf.copy_from_slice(&self.data[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.check(offset, buf.len())?;
        self.ops.push(('w', offset, buf.len()));
        self.data[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

#[test]
fn raw_device_io_is_bounced_to_block_alignment() {
    // straight to the raw node, an unaligned read fails the way pread on /dev/rdiskN does
    let mut raw = StrictRaw::new(64 * 1024, 4096);
    assert!(raw.read_at(300, &mut [0u8; 100]).is_err());

    let mut dev = AlignedDevice::new(StrictRaw::new(64 * 1024, 4096));
    let mut expect = dev.inner().data.clone();
    assert_eq!((dev.len(), dev.sector_size()), (64 * 1024, 4096));
    let mut b = vec![0u8; 5000];
    dev.read_at(300, &mut b).unwrap();
    assert_eq!(b, expect[300..5300]);
    dev.write_at(4000, &[0xAB; 200]).unwrap();
    expect[4000..4200].copy_from_slice(&[0xAB; 200]);
    dev.write_at(10, b"hello").unwrap();
    expect[10..15].copy_from_slice(b"hello");
    let mut one = [0u8; 1];
    dev.read_at(64 * 1024 - 1, &mut one).unwrap();
    assert_eq!(one[0], expect[64 * 1024 - 1]);
    assert!(dev.read_at(64 * 1024 - 1, &mut [0u8; 2]).is_err(), "past the end");
    let inner = dev.into_inner();
    assert_eq!(inner.data, expect, "neighbouring bytes preserved by read-modify-write");
    assert!(!inner.ops.is_empty());
    assert!(
        inner.ops.iter().all(|&(_, off, n)| off % 4096 == 0 && n % 4096 == 0),
        "every request reaching the raw node is block-aligned: {:?}",
        inner.ops
    );

    // a pm-parttable round trip (sector-sized I/O at 512-byte offsets on a 4Kn node)
    let mut dev = AlignedDevice::new(StrictRaw::new(8 << 20, 4096));
    let t = Table::empty(TableKind::Gpt, 8 << 20, 4096);
    write_table(&mut dev, &t).unwrap();
    assert_eq!(read_table(&mut dev).unwrap().disk_guid, t.disk_guid);

    let mut ro = StrictRaw::new(8192, 512);
    ro.read_only = true;
    let mut dev = AlignedDevice::new(ro);
    assert!(dev.is_read_only());
    assert!(matches!(dev.write_at(1, b"x"), Err(Error::Invalid(_))));
}
