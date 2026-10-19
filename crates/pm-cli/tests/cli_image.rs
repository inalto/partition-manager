//! End-to-end tests for the imaging commands: run the `pm` binary on temp files (no admin).

use pm_core::{gpt_types, BlockDevice, FileDevice, PartitionType, SliceDevice, TableKind, MIB};
use pm_fs::fat::format::{format_fat32, FormatOpts};
use pm_image::{identify, ImageFormat, MapFile};
use pm_parttable::{write_table, Entry, Table};
use std::path::Path;
use std::process::{Command, Output};

fn pm(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pm"))
        .args(args)
        .output()
        .expect("spawn pm")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 31 + seed as u64 * 7) % 251) as u8 + 1)
        .collect()
}

/// A raw file of `len` bytes: a pattern in a few places, zeros elsewhere.
fn make_pattern_raw(path: &Path, len: u64) {
    let mut dev = FileDevice::create(path, len).unwrap();
    dev.write_at(0, &pattern(4096, 1)).unwrap();
    dev.write_at(3 * MIB + 17, &pattern(300_000, 2)).unwrap();
    dev.write_at(len - 4096, &pattern(4096, 3)).unwrap();
    dev.flush().unwrap();
}

fn read_all(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

// ---------------------------------------------------------------------------------------------
// A minimal FAT32 file writer (short names only), independent of the reader under test.
// ---------------------------------------------------------------------------------------------

fn le16(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap()) as u64
}

fn le32(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}

fn dos_name(name: &str) -> [u8; 11] {
    let (base, ext) = name.split_once('.').unwrap_or((name, ""));
    let mut n = [b' '; 11];
    n[..base.len()].copy_from_slice(base.as_bytes());
    n[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    n
}

struct Fat32Builder<'a> {
    dev: &'a mut dyn BlockDevice,
    fat_offs: Vec<u64>,
    data_off: u64,
    cluster: u64,
    next_free: u32,
}

impl<'a> Fat32Builder<'a> {
    fn new(dev: &'a mut dyn BlockDevice) -> Self {
        let mut boot = [0u8; 512];
        dev.read_at(0, &mut boot).unwrap();
        let (bps, spc) = (le16(&boot, 11), boot[13] as u64);
        let (reserved, nfats, fat_sectors) = (le16(&boot, 14), boot[16] as u64, le32(&boot, 36));
        assert_eq!(le32(&boot, 44), 2, "root cluster");
        Fat32Builder {
            fat_offs: (0..nfats).map(|i| (reserved + i * fat_sectors) * bps).collect(),
            data_off: (reserved + nfats * fat_sectors) * bps,
            cluster: bps * spc,
            next_free: 3,
            dev,
        }
    }

    fn cluster_off(&self, c: u32) -> u64 {
        self.data_off + (c as u64 - 2) * self.cluster
    }

    /// Allocate a chain for `data` (at least one cluster), write it and link the FATs.
    fn write_chain(&mut self, data: &[u8]) -> u32 {
        let n = (data.len() as u64).div_ceil(self.cluster).max(1) as u32;
        let first = self.next_free;
        self.next_free += n;
        for i in 0..n {
            let c = first + i;
            let next = if i + 1 == n { 0x0FFF_FFFF } else { c + 1 };
            for fo in &self.fat_offs {
                self.dev.write_at(fo + c as u64 * 4, &next.to_le_bytes()).unwrap();
            }
            let start = (i as u64 * self.cluster) as usize;
            if start < data.len() {
                let end = (start + self.cluster as usize).min(data.len());
                self.dev.write_at(self.cluster_off(c), &data[start..end]).unwrap();
            }
        }
        first
    }

    fn entry(name: &str, attr: u8, cluster: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(&dos_name(name));
        e[11] = attr;
        let time: u16 = (10 << 11) | (20 << 5) | 15;
        let date: u16 = ((2024 - 1980) << 9) | (3 << 5) | 15;
        e[22..24].copy_from_slice(&time.to_le_bytes());
        e[24..26].copy_from_slice(&date.to_le_bytes());
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    fn put_entries(&mut self, dir_cluster: u32, entries: &[[u8; 32]]) {
        let base = self.cluster_off(dir_cluster);
        let mut slot = 0u64;
        loop {
            let mut first = [0u8; 1];
            self.dev.read_at(base + slot * 32, &mut first).unwrap();
            if first[0] == 0 {
                break;
            }
            slot += 1;
        }
        assert!((slot + entries.len() as u64) * 32 <= self.cluster, "directory full");
        for (i, e) in entries.iter().enumerate() {
            self.dev.write_at(base + (slot + i as u64) * 32, e).unwrap();
        }
    }

    fn add_file(&mut self, dir_cluster: u32, name: &str, data: &[u8]) {
        let c = self.write_chain(data);
        self.put_entries(dir_cluster, &[Self::entry(name, 0x20, c, data.len() as u32)]);
    }

    fn add_dir(&mut self, parent: u32, name: &str) -> u32 {
        let c = self.write_chain(&[]);
        let parent_ref = if parent == 2 { 0 } else { parent };
        self.put_entries(c, &[Self::entry(".", 0x10, c, 0), Self::entry("..", 0x10, parent_ref, 0)]);
        self.put_entries(parent, &[Self::entry(name, 0x10, c, 0)]);
        c
    }
}

const HELLO: &[u8] = b"hello from fat32\n";

fn data_bin() -> Vec<u8> {
    pattern(10_000, 9)
}

/// Format `dev` (>= 33 MiB) as FAT32 and add HELLO.TXT, DATA.BIN, SUB/INNER.TXT.
fn populate_fat32(dev: &mut dyn BlockDevice) {
    format_fat32(
        dev,
        &FormatOpts {
            label: "PMTEST".into(),
            cluster_size: None,
            volume_id: Some(0x1234_5678),
        },
    )
    .unwrap();
    let mut b = Fat32Builder::new(dev);
    b.add_file(2, "HELLO.TXT", HELLO);
    b.add_file(2, "DATA.BIN", &data_bin());
    let sub = b.add_dir(2, "SUB");
    b.add_file(sub, "INNER.TXT", b"inner file\n");
    dev.flush().unwrap();
}

fn make_fat32_image(path: &Path) {
    let mut dev = FileDevice::create(path, 40 * MIB).unwrap();
    populate_fat32(&mut dev);
}

// ---------------------------------------------------------------------------------------------

#[test]
fn cli_image_and_restore_roundtrip_qcow2() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.raw");
    let img = dir.path().join("disk.qcow2");
    let dst = dir.path().join("dst.raw");
    make_pattern_raw(&src, 8 * MIB);
    FileDevice::create(&dst, 8 * MIB).unwrap();

    let out = pm(&["image", s(&src), s(&img), "--format", "qcow2"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(identify(&img).unwrap(), ImageFormat::Qcow2);

    let out = pm(&["restore", s(&img), s(&dst)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Restored"), "{}", stdout(&out));
    assert_eq!(read_all(&dst), read_all(&src));

    // the extension picks the format when --format is absent
    let img2 = dir.path().join("auto.vhd");
    let out = pm(&["image", s(&src), s(&img2), "--sector"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(identify(&img2).unwrap(), ImageFormat::VhdDynamic);

    // no extension hint and no --format: a usage error that creates nothing
    let bad = dir.path().join("noext");
    let out = pm(&["image", s(&src), s(&bad)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(!bad.exists());
}

#[test]
fn cli_convert_vhd_to_vmdk() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.raw");
    let vhd = dir.path().join("a.vhd");
    let vmdk = dir.path().join("a.vmdk");
    let back = dir.path().join("back.raw");
    make_pattern_raw(&src, 6 * MIB);

    let out = pm(&["image", s(&src), s(&vhd), "--format", "vhd"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(identify(&vhd).unwrap(), ImageFormat::VhdDynamic);

    let out = pm(&["convert", s(&vhd), s(&vmdk), "--format", "vmdk"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(identify(&vmdk).unwrap(), ImageFormat::VmdkSparse);

    let out = pm(&["convert", s(&vmdk), s(&back), "--format", "raw"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(read_all(&back), read_all(&src));

    // converting a file onto itself is refused and leaves it intact
    let before = read_all(&vhd);
    let out = pm(&["convert", s(&vhd), s(&vhd), "--format", "vhd"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(read_all(&vhd), before);

    // a missing input is an argument error
    let out = pm(&["convert", s(&dir.path().join("missing.vhd")), s(&back), "--format", "raw"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn cli_rescue_on_file_writes_mapfile() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.raw");
    let dst = dir.path().join("dst.raw");
    let map = dir.path().join("rescue.map");
    make_pattern_raw(&src, 4 * MIB);
    FileDevice::create(&dst, 4 * MIB).unwrap();

    let out = pm(&["rescue", s(&src), s(&dst), s(&map), "-b", "64K", "-r", "1"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Rescued"), "{}", stdout(&out));
    assert_eq!(read_all(&dst), read_all(&src));

    assert!(map.exists());
    let m = MapFile::load(&map).unwrap();
    assert_eq!(m.len(), 4 * MIB);
    assert!(!m.blocks.is_empty());
    assert!(m.blocks.iter().all(|b| b.2 == pm_image::BlockStatus::Finished), "{:?}", m.blocks);

    // resuming from the finished mapfile succeeds and changes nothing
    let out = pm(&["rescue", s(&src), s(&dst), s(&map), "-R", "-n", "-N"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(read_all(&dst), read_all(&src));
}

#[test]
fn cli_rescue_refuses_unsafe_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.raw");
    let small = dir.path().join("small.raw");
    let big = dir.path().join("big.raw");
    let map = dir.path().join("rescue.map");
    make_pattern_raw(&src, 4 * MIB);
    FileDevice::create(&small, MIB).unwrap();
    FileDevice::create(&big, 4 * MIB).unwrap();
    let before = read_all(&src);

    // destination too small
    let out = pm(&["rescue", s(&src), s(&small), s(&map)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(!map.exists());
    assert!(read_all(&small).iter().all(|&b| b == 0));
    // source as destination
    let out = pm(&["rescue", s(&src), s(&src), s(&map)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(!map.exists());
    // the mapfile must be neither the source nor the destination
    for bad in [&src, &big] {
        let out = pm(&["rescue", s(&src), s(&big), s(bad)]);
        assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
        assert!(stderr(&out).contains("mapfile"), "{}", stderr(&out));
    }
    assert_eq!(read_all(&src), before);
    assert!(read_all(&big).iter().all(|&b| b == 0));
    // the mapfile's directory must exist
    let nodir = dir.path().join("nodir").join("rescue.map");
    let out = pm(&["rescue", s(&src), s(&big), s(&nodir)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(read_all(&big).iter().all(|&b| b == 0));
    // sizes are validated
    let out = pm(&["rescue", s(&src), s(&big), s(&map), "-b", "xyz"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let out = pm(&["rescue", s(&src), s(&big), s(&map), "-b", "0"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    // a mapfile of another size cannot be resumed
    std::fs::write(&map, "# Mapfile.
0x0  ?  1
0     0x1000  ?
").unwrap();
    let out = pm(&["rescue", s(&src), s(&big), s(&map)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(read_all(&big).iter().all(|&b| b == 0));
}

#[test]
fn cli_ls_fat32_image_lists_files() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("fat.img");
    make_fat32_image(&img);
    let dev = format!("file:{}", s(&img));

    let out = pm(&["ls", &dev]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("HELLO.TXT") && text.contains("DATA.BIN") && text.contains("SUB"), "{text}");
    assert!(text.contains("2024-03-15"), "{text}");
    assert!(text.contains("10000"), "{text}");
    assert!(!text.contains("INNER.TXT"), "{text}");
    let sub_line = text.lines().find(|l| l.ends_with("SUB")).unwrap();
    assert!(sub_line.starts_with('d'), "{sub_line}");

    // a bare path works too, as do sub-directories (case-insensitively)
    let out = pm(&["ls", s(&img), "/sub"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("INNER.TXT"), "{}", stdout(&out));

    let out = pm(&["cat", &dev, "/HELLO.TXT"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(out.stdout, HELLO);
    let out = pm(&["cat", &dev, "/data.bin"]);
    assert_eq!(out.stdout, data_bin());
    let out = pm(&["cat", &dev, "/SUB/INNER.TXT"]);
    assert_eq!(out.stdout, b"inner file\n");

    // missing path and cat of a directory are argument errors
    let out = pm(&["ls", &dev, "/nope"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let out = pm(&["cat", &dev, "/SUB"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));

    let out = pm(&["probe", &dev]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("FAT32") && text.contains("PMTEST"), "{text}");
}

#[test]
fn cli_extract_file_and_directory() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("fat.img");
    make_fat32_image(&img);
    let dev = format!("file:{}", s(&img));
    let dest = dir.path().join("out");

    let out = pm(&["extract", &dev, "/DATA.BIN", s(&dest)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(read_all(&dest.join("DATA.BIN")), data_bin());

    let out = pm(&["extract", &dev, "/SUB", s(&dest)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(read_all(&dest.join("SUB").join("INNER.TXT")), b"inner file\n");

    let all = dir.path().join("all");
    let out = pm(&["extract", &dev, "/", s(&all)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(read_all(&all.join("HELLO.TXT")), HELLO);
    assert_eq!(read_all(&all.join("SUB").join("INNER.TXT")), b"inner file\n");
}

#[test]
fn cli_probe_reports_no_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("empty.img");
    FileDevice::create(&img, MIB).unwrap();
    let out = pm(&["probe", s(&img)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("no filesystem detected"), "{}", stdout(&out));
    let out = pm(&["ls", s(&img)]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn cli_clone_smart_fat32_leaves_unused_zero() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("fat.img");
    let dst = dir.path().join("clone.img");
    make_fat32_image(&src);
    // the destination starts as 0xAA everywhere: a smart clone copies only used regions
    let mut d = FileDevice::create(&dst, 40 * MIB).unwrap();
    let fill = vec![0xAAu8; MIB as usize];
    for i in 0..40 {
        d.write_at(i * MIB, &fill).unwrap();
    }
    d.flush().unwrap();
    drop(d);

    let out = pm(&["clone", s(&src), s(&dst)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Cloned") && text.contains("smart"), "{text}");

    let a = read_all(&src);
    let b = read_all(&dst);
    let used = {
        let dev = FileDevice::open(&src, true).unwrap();
        let mut fs = pm_fs::open(Box::new(dev)).unwrap();
        fs.used_map().unwrap()
    };
    let mut ranges = 0;
    let mut copied = 0u64;
    for (st, len) in used.ranges() {
        let (st, end) = (st as usize, ((st + len) as usize).min(a.len()));
        assert_eq!(a[st..end], b[st..end], "used range {st}..{end}");
        ranges += 1;
        copied += (end - st) as u64;
    }
    assert!(ranges > 0);
    assert!(copied < 4 * MIB, "only a small part of the volume is used ({copied})");
    // the tail is unused: untouched by a smart clone
    let tail = a.len() - MIB as usize;
    assert!(b[tail..].iter().all(|&x| x == 0xAA));
    assert!(a[tail..].iter().all(|&x| x == 0));

    let out = pm(&["ls", &format!("file:{}", s(&dst))]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("HELLO.TXT") && text.contains("DATA.BIN") && text.contains("SUB"), "{text}");
    let out = pm(&["cat", s(&dst), "/DATA.BIN"]);
    assert_eq!(out.stdout, data_bin());

    // --sector copies everything
    let out = pm(&["clone", s(&src), s(&dst), "--sector"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("sector"), "{}", stdout(&out));
    assert_eq!(read_all(&dst), a);
}

#[test]
fn cli_clone_refuses_same_file_and_small_destination() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.raw");
    let small = dir.path().join("small.raw");
    let big = dir.path().join("big.raw");
    make_pattern_raw(&src, 4 * MIB);
    FileDevice::create(&small, 3 * MIB).unwrap();
    FileDevice::create(&big, 5 * MIB).unwrap();
    let before = read_all(&src);

    let out = pm(&["clone", s(&src), s(&src)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    // the same file spelled differently
    let alias = dir.path().join(".").join("src.raw");
    let out = pm(&["clone", s(&src), &format!("file:{}", s(&alias))]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(read_all(&src), before);

    let out = pm(&["clone", s(&src), s(&small)]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(read_all(&small).iter().all(|&b| b == 0), "nothing may be written");

    // 5 MiB fits at offset 1 MiB but not at 2 MiB
    let out = pm(&["clone", s(&src), s(&big), "--dst-offset", "2M"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(read_all(&big).iter().all(|&b| b == 0));
    let out = pm(&["clone", s(&src), s(&big), "--dst-offset", "1M", "--sector"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let b = read_all(&big);
    assert_eq!(b[MIB as usize..5 * MIB as usize], before[..]);
    assert!(b[..MIB as usize].iter().all(|&x| x == 0));

    // conflicting mode flags and an unaligned offset are usage errors
    let out = pm(&["clone", s(&src), s(&big), "--smart", "--sector"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let out = pm(&["clone", s(&src), s(&big), "--dst-offset", "100", "--sector"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    // --smart on something that is not a filesystem fails rather than silently copying everything
    let out = pm(&["clone", s(&src), s(&big), "--smart"]);
    assert!(!out.status.success());
}

#[test]
fn cli_partition_addressing_on_image_file() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk.img");
    let mut dev = FileDevice::create(&disk, 64 * MIB).unwrap();
    let (start, len) = (2 * MIB, 40 * MIB);
    {
        let mut slice = SliceDevice::new(&mut dev, start, len).unwrap();
        populate_fat32(&mut slice);
    }
    let mut t = Table::empty(TableKind::Gpt, dev.len(), 512);
    t.entries.push(Entry {
        number: 1,
        start,
        len,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        guid: Some(uuid::Uuid::from_u128(0x77)),
        name: "Data".into(),
        attrs: 0,
        logical: false,
    });
    write_table(&mut dev, &t).unwrap();
    dev.flush().unwrap();
    drop(dev);

    let spec = format!("file:{}:1", s(&disk));
    let out = pm(&["ls", &spec]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("HELLO.TXT"), "{}", stdout(&out));
    let out = pm(&["probe", &spec]);
    assert!(stdout(&out).contains("FAT32"), "{}", stdout(&out));
    // the whole disk has no filesystem at offset 0
    let out = pm(&["probe", s(&disk)]);
    assert!(stdout(&out).contains("no filesystem detected"), "{}", stdout(&out));

    // a partition that is not in the table is an argument error
    let out = pm(&["ls", &format!("file:{}:2", s(&disk))]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let out = pm(&["ls", &format!("file:{}:0", s(&disk))]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));

    // clone the partition into a new file; it is a plain 40 MiB FAT32 image
    let part = dir.path().join("part.img");
    FileDevice::create(&part, 40 * MIB).unwrap();
    let out = pm(&["clone", &spec, s(&part)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let out = pm(&["cat", s(&part), "/SUB/INNER.TXT"]);
    assert_eq!(out.stdout, b"inner file\n");

    // a partition cannot be cloned onto its own disk image
    let out = pm(&["clone", &spec, s(&disk), "--dst-offset", "4M", "--sector"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    // a copy that does not fit behind the offset is refused too
    let out = pm(&["clone", s(&part), s(&disk), "--dst-offset", "30M", "--sector"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn cli_rescue_with_used_map_zeroes_unused() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("fat.img");
    let dst = dir.path().join("rescued.img");
    let map = dir.path().join("fat.map");
    make_fat32_image(&src);
    let mut d = FileDevice::create(&dst, 40 * MIB).unwrap();
    let fill = vec![0xAAu8; MIB as usize];
    for i in 0..40 {
        d.write_at(i * MIB, &fill).unwrap();
    }
    d.flush().unwrap();
    drop(d);

    let out = pm(&["rescue", s(&src), s(&dst), s(&map), "--used-map"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // used areas are copied, unused ones zero-filled
    assert_eq!(read_all(&dst), read_all(&src));
    let m = MapFile::load(&map).unwrap();
    assert!(m.blocks.iter().all(|b| b.2 == pm_image::BlockStatus::Finished));
}

#[test]
fn cli_image_smart_fat32_to_qcow2_is_sparse_and_browsable() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("fat.img");
    let img = dir.path().join("fat.qcow2");
    let back = dir.path().join("back.img");
    make_fat32_image(&src);
    FileDevice::create(&back, 40 * MIB).unwrap();

    let out = pm(&["image", s(&src), s(&img), "--smart"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("smart"), "{}", stdout(&out));
    assert_eq!(identify(&img).unwrap(), ImageFormat::Qcow2);
    let size = std::fs::metadata(&img).unwrap().len();
    assert!(size < 8 * MIB, "unused clusters stay unallocated, got {size} bytes");

    // the image is browsable directly and restores to the original bytes
    let out = pm(&["cat", s(&img), "/HELLO.TXT"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(out.stdout, HELLO);
    let out = pm(&["restore", s(&img), s(&back)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(read_all(&back), read_all(&src));
}
