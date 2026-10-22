//! macOS platform logic driven by hand-written `diskutil -plist` fixtures
//! (`tests/fixtures/macos/`). Runs on every OS: the parsers, the enumeration, the system-disk
//! lookup and the `diskutil` mount/format calls go through a [`DiskutilRunner`].

use pm_core::{gpt_types, BlockDevice, DiskBus, Error, FsKind, PartitionType, Result, TableKind, VolumeId};
use pm_parttable::{read_table, write_table, Entry, Table};
use pm_platform::macos::diskutil::{canonical_volume_id, parse_info, parse_list};
use pm_platform::macos::write::{mounted_slices, prepare_write, remount_args};
use pm_platform::macos::{enumerate, format, mount, system_disk, AlignedDevice, DiskutilRunner};
use pm_platform::{MountPoint, NullPlatform, Platform};
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
    /// `(verb, message)`: that command (`mount`, `unmountDisk`, ...) fails with the message.
    failing: Vec<(&'static str, &'static str)>,
    /// Replies by the space-joined argument vector, before the fixtures.
    overrides: Vec<(&'static str, Vec<u8>)>,
    /// What `list -plist` returns once an `unmountDisk` has run.
    list_after_unmount: Option<Vec<u8>>,
}

impl DiskutilRunner for FixtureRunner {
    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let unmount_ran = self.calls.lock().unwrap().iter().any(|c| c[0] == "unmountDisk");
        self.calls.lock().unwrap().push(args.iter().map(|s| s.to_string()).collect());
        let not_found = |what: &str| Error::Platform { code: 1, msg: format!("diskutil failed: Could not find disk: {what}") };
        let joined = args.join(" ");
        if let Some((_, bytes)) = self.overrides.iter().find(|(k, _)| *k == joined) {
            return Ok(bytes.clone());
        }
        if let Some((_, msg)) = self.failing.iter().find(|(verb, _)| *verb == args[0]) {
            return Err(Error::Platform { code: 1, msg: format!("diskutil failed: {msg}") });
        }
        match args {
            ["list", "-plist"] => match (&self.list_after_unmount, unmount_ran) {
                (Some(after), true) => Ok(after.clone()),
                _ => Ok(fixture("list.plist")),
            },
            ["info", "-plist", what] => {
                let name = match *what {
                    "/" => "info-root.plist".to_string(),
                    "/Volumes/STICK" => "info-disk2s1.plist".to_string(),
                    w if w.starts_with("disk") && !self.broken.contains(&w) => format!("info-{w}.plist"),
                    _ => return Err(not_found(what)),
                };
                std::fs::read(fixture_dir().join(name)).map_err(|_| not_found(what))
            }
            _ => Ok(Vec::new()),
        }
    }
}

impl FixtureRunner {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
    /// The calls that are not `info -plist` queries.
    fn commands(&self) -> Vec<Vec<String>> {
        self.calls().into_iter().filter(|c| c[0] != "info").collect()
    }
}

fn info_call(what: &str) -> Vec<String> {
    vec!["info".into(), "-plist".into(), what.into()]
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
    assert_eq!((root.virtual_or_physical.as_str(), d.virtual_or_physical.as_str()), ("Virtual", "Physical"));
}

#[test]
fn enumeration_without_device_access_is_approximate() {
    let r = FixtureRunner::default();
    let e = enumerate(&r, &no_device, Some(0), false).unwrap();
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
    // the store slice carries the mount points and usage of the container's volumes
    assert_eq!(d0.partitions[1].mount_points, ["/System/Volumes/Data", "/"]);
    assert_eq!(d0.partitions[1].used, Some(211_812_909_056 + 15_123_456_000));
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
    let mut expected = vec![vec!["list".to_string(), "-plist".to_string()]];
    for d in ["disk0", "disk0s1", "disk0s2", "disk2", "disk2s1", "disk3", "disk3s1", "disk3s2"] {
        expected.push(info_call(d));
    }
    assert_eq!(r.calls(), expected);
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
    gpt_disk3(&[
        (1, 40 * 512, EFI3, gpt_types::EFI_SYSTEM, "B1B2C3D4-0000-4000-8000-000000000001", "EFI System Partition"),
        (2, 409_640 * 512, NTFS3, gpt_types::BASIC_DATA, "B1B2C3D4-0000-4000-8000-000000000002", "Basic data partition"),
    ])
}

/// A sparse disk3 with a GPT of `(number, start, len, type, guid, name)` entries.
fn gpt_disk3(entries: &[(u32, u64, u64, Uuid, &str, &str)]) -> Sparse {
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
    t.entries = entries.iter().map(|&(n, start, len, ty, guid, name)| entry(n, start, len, ty, guid, name)).collect();
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
    let e = enumerate(&FixtureRunner::default(), &open, Some(0), false).unwrap();
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
    let e = enumerate(&r, &no_device, None, false).unwrap();
    assert_eq!(e.disks.iter().map(|d| d.index).collect::<Vec<_>>(), [0, 3]);
    assert!(e.disks.iter().all(|d| !d.flags.system), "no system disk known");

    struct NoDiskutil;
    impl DiskutilRunner for NoDiskutil {
        fn run(&self, _: &[&str]) -> Result<Vec<u8>> {
            Err(Error::Unsupported("diskutil is not installed".into()))
        }
    }
    assert!(matches!(enumerate(&NoDiskutil, &no_device, None, false), Err(Error::Unsupported(_))));
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
    format::format(&r, &id, FsKind::ExFat, "BACKUP", Some(131_072), false).unwrap();
    format::format(&r, &id, FsKind::Fat32, "", None, true).unwrap();
    format::format(&r, &id, FsKind::HfsPlus, "", None, true).unwrap();
    // every target is checked first: the slice, then its parent whole disk
    assert_eq!(r.calls()[..2], [info_call("disk2s1"), info_call("disk2")]);
    assert_eq!(
        r.commands(),
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
    match format::format(&r, &id, FsKind::Ntfs, "X", None, true) {
        Err(Error::Unsupported(m)) => assert!(m.contains("third-party"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(format::format(&r, &id, FsKind::Ext4, "X", None, true), Err(Error::Unsupported(_))));
    assert!(matches!(
        mount::unmount(&r, &VolumeId(r"\\?\Volume{x}\".into()), &MountPoint::Letter('E')),
        Err(Error::NotFound(_))
    ));
    assert!(r.calls().is_empty(), "nothing ran for refused requests");

    // diskutil failures surface as platform errors
    let r = FixtureRunner { failing: vec![("mount", "Volume on disk2s1 failed to mount")], ..Default::default() };
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
    assert!(matches!(dev.write_at(0, &[0u8; 512]), Err(Error::Invalid(_))));
    assert!(dev.into_inner().ops.is_empty(), "a refused write reaches no I/O on the raw node");
}

/// `list.plist` with both disk3 slices resized to 100 MiB (two equal-size partitions).
fn list_with_equal_disk3_slices() -> Vec<u8> {
    let s = String::from_utf8(fixture("list.plist")).unwrap();
    let s = s
        .replace(&format!("<integer>{EFI3}</integer>"), "<integer>104857600</integer>")
        .replace(&format!("<integer>{NTFS3}</integer>"), "<integer>104857600</integer>");
    assert_eq!(s.matches("<integer>104857600</integer>").count(), 2);
    s.into_bytes()
}

const MIB100: u64 = 104_857_600;

#[test]
fn readable_disk_without_a_usable_table_has_no_partitions() {
    // the raw node opens (root) but holds no partition table diskutil's slices fit in
    let open = |path: &str| -> Option<Box<dyn BlockDevice>> {
        (path == "/dev/rdisk3").then(|| Box::new(Sparse { len: D3, sectors: BTreeMap::new() }) as Box<dyn BlockDevice>)
    };
    let e = enumerate(&FixtureRunner::default(), &open, Some(0), false).unwrap();
    let d3 = e.disks.iter().find(|d| d.index == 3).unwrap();
    assert_eq!(d3.table, TableKind::Unknown);
    assert!(d3.partitions.is_empty(), "no approximate offsets on a disk this process can write");
    for id in ["/dev/disk3s1", "/dev/disk3s2"] {
        let v = e.volumes.iter().find(|v| v.id.0 == id).unwrap();
        assert_eq!((v.disk, v.start, v.len), (None, 0, 0), "{id} is not placed on any disk");
    }
    let data = e.volumes.iter().find(|v| v.id.0 == "/dev/disk3s2").unwrap();
    assert_eq!((data.fs, data.label.as_str()), (Some(FsKind::Ntfs), "DATA"), "diskutil's facts are kept");
    // the unreadable disks stay approximate and placed
    assert_eq!(e.disks[0].partitions.len(), 2);
    assert_eq!(e.volumes.iter().find(|v| v.id.0 == "/dev/disk2s1").unwrap().disk, Some(2));
}

#[test]
fn slices_match_partitions_by_guid_then_unambiguously() {
    let runner = || FixtureRunner { overrides: vec![("list -plist", list_with_equal_disk3_slices())], ..Default::default() };
    let disk3_of = |dev: Sparse| {
        let open = move |path: &str| -> Option<Box<dyn BlockDevice>> {
            (path == "/dev/rdisk3").then(|| Box::new(dev.clone()) as Box<dyn BlockDevice>)
        };
        let e = enumerate(&runner(), &open, None, false).unwrap();
        let d = e.disks.into_iter().find(|d| d.index == 3).unwrap();
        (d, e.volumes)
    };
    let vol = |d: &pm_core::DiskInfo, number: u32| {
        d.partitions.iter().find(|p| p.number == number).unwrap().volume.as_ref().map(|v| v.0.clone())
    };

    // GUIDs win over numbers: entry 1 carries disk3s2's partition GUID and vice versa
    let (d, _) = disk3_of(gpt_disk3(&[
        (1, 40 * 512, MIB100, gpt_types::EFI_SYSTEM, "B1B2C3D4-0000-4000-8000-000000000002", "a"),
        (2, 409_640 * 512, MIB100, gpt_types::BASIC_DATA, "B1B2C3D4-0000-4000-8000-000000000001", "b"),
    ]));
    assert_eq!(vol(&d, 1).as_deref(), Some("/dev/disk3s2"));
    assert_eq!(vol(&d, 2).as_deref(), Some("/dev/disk3s1"));

    // equal sizes, no GUID and no number match: nothing is attached rather than a guess
    let (d, volumes) = disk3_of(gpt_disk3(&[
        (3, 40 * 512, MIB100, gpt_types::BASIC_DATA, "D0000000-0000-4000-8000-000000000003", "a"),
        (4, 409_640 * 512, MIB100, gpt_types::BASIC_DATA, "D0000000-0000-4000-8000-000000000004", "b"),
    ]));
    assert_eq!((vol(&d, 3), vol(&d, 4)), (None, None));
    for id in ["/dev/disk3s1", "/dev/disk3s2"] {
        let v = volumes.iter().find(|v| v.id.0 == id).unwrap();
        assert_eq!((v.disk, v.len), (None, 0), "{id} unplaced");
    }

    // sizes unique on both sides: matched even without GUID / number agreement
    let dev = gpt_disk3(&[
        (5, 40 * 512, EFI3, gpt_types::EFI_SYSTEM, "D0000000-0000-4000-8000-000000000005", "a"),
        (6, 409_640 * 512, NTFS3, gpt_types::BASIC_DATA, "D0000000-0000-4000-8000-000000000006", "b"),
    ]);
    let open = move |path: &str| -> Option<Box<dyn BlockDevice>> {
        (path == "/dev/rdisk3").then(|| Box::new(dev.clone()) as Box<dyn BlockDevice>)
    };
    let e = enumerate(&FixtureRunner::default(), &open, None, false).unwrap();
    let d = e.disks.iter().find(|d| d.index == 3).unwrap();
    assert_eq!(vol(d, 5).as_deref(), Some("/dev/disk3s1"));
    assert_eq!(vol(d, 6).as_deref(), Some("/dev/disk3s2"));
}

#[test]
fn whole_disks_and_container_volumes_are_not_volumes() {
    let r = FixtureRunner::default();
    let folder = MountPoint::Folder("/Volumes/x".into());
    for whole in ["/dev/disk2", "/dev/rdisk2", "disk2"] {
        let id = VolumeId(whole.into());
        assert!(matches!(format::format(&r, &id, FsKind::Fat32, "X", None, true), Err(Error::NotFound(_))), "{whole}");
        assert!(matches!(mount::mount(&r, &id, &folder), Err(Error::NotFound(_))), "{whole}");
        assert!(matches!(mount::unmount(&r, &id, &folder), Err(Error::NotFound(_))), "{whole}");
    }
    assert!(r.calls().is_empty());

    // a volume of the synthesized APFS container (parent disk1) is refused after the checks
    let apfs = VolumeId("/dev/disk1s1".into());
    assert!(matches!(format::format(&r, &apfs, FsKind::ExFat, "X", None, true), Err(Error::Invalid(_))));
    assert!(matches!(mount::unmount(&r, &apfs, &folder), Err(Error::Invalid(_))));
    // a slice diskutil does not know
    assert!(matches!(mount::mount(&r, &VolumeId("/dev/disk9s1".into()), &folder), Err(Error::Platform { .. })));
    assert!(r.commands().is_empty(), "no eraseVolume / mount / unmount ran: {:?}", r.calls());
}

#[test]
fn begin_write_refusals() {
    // not root: refused before anything is unmounted
    let r = FixtureRunner::default();
    match prepare_write(&r, 2, false, |_| Ok(())) {
        Err(Error::Platform { code, msg }) => {
            assert_eq!(code, 13);
            assert!(msg.contains("access denied"), "{msg}");
        }
        other => panic!("{other:?}"),
    }
    assert!(r.calls().is_empty());

    // a synthesized APFS container is not a disk to partition
    let r = FixtureRunner::default();
    match prepare_write(&r, 1, true, |_| Ok(())) {
        Err(Error::Invalid(m)) => assert!(m.contains("synthesized APFS container"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.calls(), [info_call("disk1")]);
    // an unknown disk
    let r = FixtureRunner::default();
    assert!(prepare_write(&r, 7, true, |_| Ok(())).is_err());
    assert!(r.commands().is_empty());
}

#[test]
fn begin_write_unmounts_and_remounts_on_failure() {
    let unmount_disk2 = || vec!["unmountDisk".to_string(), "disk2".to_string()];
    let list = || vec!["list".to_string(), "-plist".to_string()];
    let mount_stick = || vec!["mount".to_string(), "disk2s1".to_string()];

    // success: checks, record, unmount, open
    let r = FixtureRunner::default();
    let opened = prepare_write(&r, 2, true, |path| {
        assert_eq!(path, "/dev/rdisk2");
        Ok(42)
    });
    assert_eq!(opened.unwrap(), 42);
    assert_eq!(r.calls(), [info_call("disk2"), list(), unmount_disk2()]);

    // open fails (EACCES): everything recorded is mounted again, the error is kept
    let r = FixtureRunner::default();
    let e = prepare_write::<()>(&r, 2, true, |_| Err(Error::Platform { code: 13, msg: "open: access denied".into() }));
    assert!(matches!(e, Err(Error::Platform { code: 13, .. })), "{e:?}");
    assert_eq!(r.commands(), [list(), unmount_disk2(), mount_stick()]);

    // open fails busy: Busy saying the volumes were mounted again
    let r = FixtureRunner::default();
    match prepare_write::<()>(&r, 2, true, |_| Err(Error::Busy("in use".into()))) {
        Err(Error::Busy(m)) => assert!(m.starts_with("in use") && m.contains("mounted again"), "{m}"),
        other => panic!("{other:?}"),
    }

    // open fails and the remount fails too: Busy naming the volume left unmounted
    let r = FixtureRunner { failing: vec![("mount", "busy")], ..Default::default() };
    match prepare_write::<()>(&r, 2, true, |_| Err(Error::Platform { code: 13, msg: "denied".into() })) {
        Err(Error::Busy(m)) => assert!(m.contains("could NOT mount again: disk2s1"), "{m}"),
        other => panic!("{other:?}"),
    }

    // unmountDisk fails half-way: only what it unmounted is mounted again
    let after = String::from_utf8(fixture("list.plist"))
        .unwrap()
        .replace("<string>/Volumes/STICK</string>", "<string></string>");
    let r = FixtureRunner {
        failing: vec![("unmountDisk", "Unmount of disk2 failed: at least one volume could not be unmounted")],
        list_after_unmount: Some(after.into_bytes()),
        ..Default::default()
    };
    match prepare_write(&r, 2, true, |_| Ok(())) {
        Err(Error::Busy(m)) => assert!(m.contains("Unmount of disk2 failed") && m.contains("mounted again"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.commands(), [list(), unmount_disk2(), list(), mount_stick()]);

    // unmountDisk fails with everything still mounted: nothing to mount again
    let r = FixtureRunner { failing: vec![("unmountDisk", "busy")], ..Default::default() };
    match prepare_write(&r, 2, true, |_| Ok(())) {
        Err(Error::Busy(m)) => assert!(!m.contains("mounted again"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.commands(), [list(), unmount_disk2(), list()]);
}

#[test]
fn mounted_slices_include_apfs_container_volumes() {
    let list = fixture("list.plist");
    let pairs = |v: Vec<(String, String)>| v.iter().map(|(a, b)| format!("{a}@{b}")).collect::<Vec<_>>();
    assert_eq!(pairs(mounted_slices(&list, 0).unwrap()), ["disk1s1@/System/Volumes/Data", "disk1s5@/"]);
    assert_eq!(pairs(mounted_slices(&list, 2).unwrap()), ["disk2s1@/Volumes/STICK"]);
    assert!(mounted_slices(&list, 3).unwrap().is_empty());
}

#[test]
fn system_disk_fails_closed_on_fusion_and_virtual_roots() {
    let r = FixtureRunner { overrides: vec![("info -plist /", fixture("info-root-fusion.plist"))], ..Default::default() };
    assert_eq!(system_disk::system_disk(&r), None, "stores on disk0 and disk2");
    // no APFS stores but a virtual volume (CoreStorage)
    let corestorage = String::from_utf8(fixture("info-root.plist"))
        .unwrap()
        .replace("<key>APFSPhysicalStores</key>", "<key>NotAPFSPhysicalStores</key>");
    let r = FixtureRunner { overrides: vec![("info -plist /", corestorage.into_bytes())], ..Default::default() };
    assert_eq!(system_disk::system_disk(&r), None);
}

#[test]
fn unopenable_disk_as_root_is_unplaced_not_approximate() {
    let e = enumerate(&FixtureRunner::default(), &no_device, Some(0), true).unwrap();
    for d in &e.disks {
        assert!(d.partitions.is_empty(), "disk {}: no offsets a root process could act on", d.index);
        assert_eq!(d.table, TableKind::Unknown);
    }
    assert_eq!(e.volumes.len(), 5);
    assert!(e.volumes.iter().all(|v| v.disk.is_none() && v.len == 0));
    let stick = e.volumes.iter().find(|v| v.id.0 == "/dev/disk2s1").unwrap();
    assert_eq!(stick.mount_points, ["/Volumes/STICK"], "diskutil's facts are kept");
}

#[test]
fn guid_with_another_size_is_never_matched_later() {
    // slices disk3s1 / disk3s2 are 100 MiB each with GUIDs ..01 / ..02. Entry 1 carries ..01
    // but is 200 MiB: disk3s1 must stay unmatched instead of falling through to entry 3
    // (the only other unmatched 100 MiB partition)
    let dev = gpt_disk3(&[
        (1, 40 * 512, 2 * MIB100, gpt_types::BASIC_DATA, "B1B2C3D4-0000-4000-8000-000000000001", "a"),
        (2, 409_640 * 512, MIB100, gpt_types::BASIC_DATA, "B1B2C3D4-0000-4000-8000-000000000002", "b"),
        (3, 614_440 * 512, MIB100, gpt_types::BASIC_DATA, "D0000000-0000-4000-8000-000000000003", "c"),
    ]);
    let open = move |path: &str| -> Option<Box<dyn BlockDevice>> {
        (path == "/dev/rdisk3").then(|| Box::new(dev.clone()) as Box<dyn BlockDevice>)
    };
    let r = FixtureRunner { overrides: vec![("list -plist", list_with_equal_disk3_slices())], ..Default::default() };
    let e = enumerate(&r, &open, None, false).unwrap();
    let d = e.disks.iter().find(|d| d.index == 3).unwrap();
    let vol = |n: u32| d.partitions.iter().find(|p| p.number == n).unwrap().volume.as_ref().map(|v| v.0.clone());
    assert_eq!((vol(1), vol(2).as_deref(), vol(3)), (None, Some("/dev/disk3s2"), None));
    let s1 = e.volumes.iter().find(|v| v.id.0 == "/dev/disk3s1").unwrap();
    assert_eq!((s1.disk, s1.len), (None, 0));
}

#[test]
fn disk_of_path_fails_closed_on_fusion() {
    let r = FixtureRunner { overrides: vec![("info -plist /", fixture("info-root-fusion.plist"))], ..Default::default() };
    assert_eq!(system_disk::disk_of_path(&r, Path::new("/Users/me/out.img")), None);
    // other volumes are unaffected
    assert_eq!(system_disk::disk_of_path(&r, Path::new("/Volumes/STICK/a")), Some((2, Some(1))));
}

#[test]
fn volume_ids_are_canonicalised() {
    assert_eq!(canonical_volume_id("/dev/rdisk2s1").as_deref(), Some("/dev/disk2s1"));
    assert_eq!(canonical_volume_id("/dev/disk2"), None);
    // the trait default leaves ids alone (Windows, mock, null)
    let id = VolumeId("/dev/rdisk2s1".into());
    assert_eq!(NullPlatform.canonical_volume_id(&id), id);
}

#[cfg(target_os = "macos")]
#[test]
fn mac_platform_canonicalises_volume_ids() {
    let p = pm_platform::MacPlatform::with_runner(Box::new(FixtureRunner::default()));
    assert_eq!(p.canonical_volume_id(&VolumeId("/dev/rdisk2s1".into())), VolumeId("/dev/disk2s1".into()));
    assert_eq!(p.canonical_volume_id(&VolumeId("disk2s1".into())), VolumeId("/dev/disk2s1".into()));
    assert_eq!(p.canonical_volume_id(&VolumeId("/dev/disk2".into())), VolumeId("/dev/disk2".into()));
}

#[test]
fn slices_are_mounted_again_where_they_were() {
    assert_eq!(remount_args("disk2s1", "/Volumes/STICK"), ["mount", "disk2s1"]);
    assert_eq!(remount_args("disk2s1", "/mnt/stick"), ["mount", "-mountPoint", "/mnt/stick", "disk2s1"]);

    // a stick mounted outside /Volumes: the open fails, it goes back to /mnt/stick
    let moved = String::from_utf8(fixture("list.plist")).unwrap().replace("/Volumes/STICK", "/mnt/stick");
    let r = FixtureRunner { overrides: vec![("list -plist", moved.into_bytes())], ..Default::default() };
    let e = prepare_write::<()>(&r, 2, true, |_| Err(Error::Platform { code: 13, msg: "denied".into() }));
    assert!(e.is_err());
    assert_eq!(
        r.commands().last().unwrap(),
        &["mount", "-mountPoint", "/mnt/stick", "disk2s1"].map(String::from).to_vec()
    );

    // unmountDisk fails and the second listing fails too: everything is mounted again and
    // "already mounted" counts as success
    let r = FixtureRunner {
        failing: vec![("unmountDisk", "busy"), ("mount", "Volume STICK on disk2s1 is already mounted")],
        list_after_unmount: Some(b"not a plist".to_vec()),
        ..Default::default()
    };
    match prepare_write(&r, 2, true, |_| Ok(())) {
        Err(Error::Busy(m)) => assert!(m.contains("mounted again") && !m.contains("could NOT"), "{m}"),
        other => panic!("{other:?}"),
    }
    let mount_stick = vec!["mount".to_string(), "disk2s1".to_string()];
    assert_eq!(r.commands().last().unwrap(), &mount_stick);

    // without the second listing failing, "already mounted" is still a failure to report
    let after = String::from_utf8(fixture("list.plist")).unwrap().replace("<string>/Volumes/STICK</string>", "<string></string>");
    let r = FixtureRunner {
        failing: vec![("unmountDisk", "busy"), ("mount", "Volume STICK on disk2s1 is already mounted")],
        list_after_unmount: Some(after.into_bytes()),
        ..Default::default()
    };
    match prepare_write(&r, 2, true, |_| Ok(())) {
        Err(Error::Busy(m)) => assert!(m.contains("could NOT mount again: disk2s1"), "{m}"),
        other => panic!("{other:?}"),
    }
}
