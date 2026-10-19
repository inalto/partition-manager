mod common;

use common::*;
use pm_core::{BlockDevice, CancelToken, Error, FsKind, MemDevice, NullProgress};
use pm_fs::{detect, format_fat32, open, Entry, FileSystem, FormatOpts};
use std::sync::Mutex;

fn open_mem(dev: MemDevice) -> Box<dyn FileSystem> {
    open(Box::new(dev)).unwrap()
}

fn names(v: &[Entry]) -> Vec<String> {
    let mut n: Vec<String> = v.iter().map(|e| e.name.clone()).collect();
    n.sort();
    n
}

fn read_all(fs: &mut dyn FileSystem, e: &Entry) -> Vec<u8> {
    let mut v = Vec::new();
    let n = fs.extract(e, &mut v).unwrap();
    assert_eq!(n as usize, v.len());
    v
}

#[test]
fn format_fat32_then_detect_and_list_empty_root() {
    let mut dev = MemDevice::from_vec(vec![0xAA; (40 * MIB) as usize]);
    format_fat32(
        &mut dev,
        &FormatOpts { label: "MyDisk".into(), cluster_size: None, volume_id: Some(0x1234_ABCD) },
    )
    .unwrap();

    let p = detect(&mut dev, 0).unwrap();
    assert_eq!(p.kind, FsKind::Fat32);
    assert_eq!(p.label, "MYDISK");
    assert_eq!(p.uuid, "1234-ABCD");
    assert_eq!(p.total, 40 * MIB);
    assert_eq!(p.used, None);
    // 40 MiB cannot hold 65525 clusters of 4 KiB, so the formatter falls back to a smaller cluster
    let cs = p.cluster.unwrap();
    assert!(cs.is_power_of_two() && (512..4096).contains(&cs), "cluster {cs}");

    // layout
    let b = dev.as_slice();
    assert_eq!(&b[0..3], &[0xEB, 0x58, 0x90]);
    assert_eq!(&b[3..11], b"MSWIN4.1");
    assert_eq!(&b[510..512], &[0x55, 0xAA]);
    assert_eq!(b[16], 2, "two FATs");
    assert_eq!(u16::from_le_bytes([b[14], b[15]]), 32, "reserved sectors");
    assert_eq!(u16::from_le_bytes([b[48], b[49]]), 1, "FSInfo sector");
    assert_eq!(u16::from_le_bytes([b[50], b[51]]), 6, "backup boot sector");
    assert_eq!(u32::from_le_bytes([b[44], b[45], b[46], b[47]]), 2, "root cluster");
    assert_eq!(b[21], 0xF8);
    assert_eq!(b[66], 0x29);
    assert_eq!(&b[71..82], b"MYDISK     ");
    assert_eq!(&b[82..90], b"FAT32   ");
    assert_eq!(&b[6 * 512..7 * 512], &b[0..512], "backup boot sector");
    // FSInfo
    let fi = &b[512..1024];
    assert_eq!(&fi[0..4], b"RRaA");
    assert_eq!(&fi[484..488], b"rrAa");
    assert_eq!(&fi[508..512], &[0x00, 0x00, 0x55, 0xAA]);
    assert_eq!(u32::from_le_bytes([fi[492], fi[493], fi[494], fi[495]]), 3, "next free");
    // FAT entries
    let fatsz = u32::from_le_bytes([b[36], b[37], b[38], b[39]]) as usize;
    let fat0 = 32 * 512;
    let rd = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    assert_eq!(rd(fat0), 0x0FFF_FFF8);
    assert_eq!(rd(fat0 + 4), 0x0FFF_FFFF);
    assert_eq!(rd(fat0 + 8), 0x0FFF_FFFF);
    assert_eq!(rd(fat0 + 12), 0);
    assert_eq!(rd(fat0 + fatsz * 512), 0x0FFF_FFF8, "second FAT mirrors the first");
    // root cluster: label entry only
    let data = (32 + 2 * fatsz) * 512;
    assert_eq!(&b[data..data + 11], b"MYDISK     ");
    assert_eq!(b[data + 11], 0x08);
    assert!(b[data + 32..data + cs as usize].iter().all(|&x| x == 0));
    // the data region beyond the root cluster is untouched
    assert!(b[data + cs as usize..data + cs as usize + 4096].iter().all(|&x| x == 0xAA));
    assert!(b.len() == (40 * MIB) as usize && b[b.len() - 1] == 0xAA);

    let mut fs = open_mem(dev);
    assert_eq!(fs.kind(), FsKind::Fat32);
    assert_eq!(fs.label(), "MYDISK");
    assert_eq!(fs.total(), 40 * MIB);
    assert_eq!(fs.cluster_size(), cs);
    let root = fs.root();
    assert!(root.is_dir);
    assert_eq!(root.path, "/");
    assert_eq!(root.id, 2);
    assert!(fs.list(&root).unwrap().is_empty());
    assert!(fs.deleted_entries(&NullProgress, &CancelToken::new()).unwrap().is_empty());
}

#[test]
fn format_fat32_validates_inputs() {
    let mut small = MemDevice::new(30 * MIB);
    let o = FormatOpts { label: String::new(), cluster_size: None, volume_id: None };
    match format_fat32(&mut small, &o) {
        Err(Error::Invalid(m)) => assert_eq!(m, "FAT32 needs at least 33 MiB"),
        other => panic!("{other:?}"),
    }
    assert!(small.as_slice().iter().all(|&b| b == 0), "nothing written on error");

    let mut dev = MemDevice::new(40 * MIB);
    // 4 KiB clusters would give fewer than 65525 clusters
    let bad = FormatOpts { cluster_size: Some(4096), ..FormatOpts { label: String::new(), cluster_size: None, volume_id: None } };
    assert!(matches!(format_fat32(&mut dev, &bad), Err(Error::Invalid(_))));
    let bad = FormatOpts { cluster_size: Some(1000), ..FormatOpts { label: String::new(), cluster_size: None, volume_id: None } };
    assert!(matches!(format_fat32(&mut dev, &bad), Err(Error::Invalid(_))));
    let long = FormatOpts { label: "WAY TOO LONG LABEL".into(), cluster_size: None, volume_id: None };
    assert!(matches!(format_fat32(&mut dev, &long), Err(Error::Invalid(_))));
    // empty label: "NO NAME" in the BPB reads back as empty, no root label entry
    format_fat32(&mut dev, &o).unwrap();
    let p = detect(&mut dev, 0).unwrap();
    assert_eq!(p.label, "");
    assert_eq!(p.kind, FsKind::Fat32);
    // volume id derived from the clock when not given
    assert_eq!(p.uuid.len(), 9);
    // 512-byte clusters honoured
    let o512 = FormatOpts { label: "X".into(), cluster_size: Some(512), volume_id: Some(1) };
    format_fat32(&mut dev, &o512).unwrap();
    assert_eq!(detect(&mut dev, 0).unwrap().cluster, Some(512));
}

#[test]
fn fat32_write_entries_with_builder_then_list() {
    let fx = fat32_fixture();
    let mut fs = open_mem(fx.img.dev);
    assert_eq!(fs.kind(), FsKind::Fat32);
    assert_eq!(fs.label(), "TESTVOL");
    assert_eq!(fs.cluster_size(), 512);

    let root = fs.root();
    let list = fs.list(&root).unwrap();
    assert_eq!(names(&list), vec!["HELLO.TXT", "Long File Name.txt", "SUBDIR", "lower.txt"]);
    let by = |n: &str| list.iter().find(|e| e.name == n).unwrap().clone();

    let hello = by("HELLO.TXT");
    assert!(!hello.is_dir && !hello.deleted);
    assert_eq!(hello.size, 11);
    assert_eq!(hello.id, 3);
    assert_eq!(hello.path, "/HELLO.TXT");
    assert_eq!(hello.attrs, 0x20);
    assert_eq!(hello.modified, Some(1_710_510_320));

    let lfn = by("Long File Name.txt");
    assert_eq!(lfn.path, "/Long File Name.txt");
    assert_eq!(lfn.size, 5);
    assert_eq!(lfn.id, 4);
    assert_eq!(lfn.modified, None, "zero date means unknown");

    let sub = by("SUBDIR");
    assert!(sub.is_dir);
    assert_eq!(sub.id, 5);
    assert_eq!(sub.attrs, 0x10);

    assert_eq!(by("lower.txt").path, "/lower.txt");

    assert_eq!(read_all(fs.as_mut(), &hello), fx.hello);
    assert_eq!(read_all(fs.as_mut(), &lfn), b"lfn!!");

    let sub_list = fs.list(&sub).unwrap();
    assert_eq!(names(&sub_list), vec!["BIG.BIN"], "no dot entries, no deleted entries");
    let big = &sub_list[0];
    assert_eq!(big.path, "/SUBDIR/BIG.BIN");
    assert_eq!(big.size, 1300);
    assert_eq!(read_all(fs.as_mut(), big), fx.big);

    // partial reads across cluster boundaries and the non-contiguous chain 7 -> 9 -> 8
    let mut buf = vec![0u8; 700];
    let n = fs.read_file(big, 400, &mut buf).unwrap();
    assert_eq!(n, 700);
    assert_eq!(&buf[..], &fx.big[400..1100]);
    // read ending past EOF is clamped
    let n = fs.read_file(big, 1200, &mut buf).unwrap();
    assert_eq!(n, 100);
    assert_eq!(&buf[..100], &fx.big[1200..]);
    // at / past EOF
    assert_eq!(fs.read_file(big, 1300, &mut buf).unwrap(), 0);
    assert_eq!(fs.read_file(big, 5000, &mut buf).unwrap(), 0);
    // sequential small reads exercise the chain cursor
    let mut got = Vec::new();
    let mut off = 0u64;
    loop {
        let mut chunk = [0u8; 97];
        let n = fs.read_file(big, off, &mut chunk).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&chunk[..n]);
        off += n as u64;
    }
    assert_eq!(got, fx.big);
    // directories cannot be read as files
    assert!(fs.read_file(&sub, 0, &mut buf).is_err());
}

#[test]
fn fat32_used_map_counts_reserved_fats_and_root() {
    let fx = fat32_fixture();
    let data_off = fx.img.data_off;
    let fat1 = fx.img.fat_offs[1];
    let cs = fx.img.cluster_size();
    let total = fx.img.dev.len();
    let mut fs = open_mem(fx.img.dev);
    let m = fs.used_map().unwrap();
    assert_eq!(m.unit as u64, cs);
    assert_eq!(m.len, total);
    // reserved area, both FATs, root cluster, files
    assert!(m.is_used(0));
    assert!(m.is_used(6 * 512), "backup boot sector");
    assert!(m.is_used(fat1));
    assert!(m.is_used(data_off - 1), "end of FAT 2");
    assert!(m.is_used(data_off), "root cluster");
    for c in [3u32, 4, 5, 7, 8, 9, 10] {
        assert!(m.is_used(data_off + (c as u64 - 2) * cs), "cluster {c}");
    }
    // free clusters
    assert!(!m.is_used(data_off + (1000 - 2) * cs));
    assert!(!m.is_used(total - 1));
    assert!(!m.is_used(total + 4096), "beyond the filesystem");
    // first range starts at 0 and covers reserved + FATs + clusters 2..=5 contiguously
    let ranges: Vec<(u64, u64)> = m.ranges().collect();
    assert_eq!(ranges[0].0, 0);
    assert!(ranges[0].1 >= data_off + 4 * cs, "{ranges:?}");
    // cluster 6 is free, so there is a gap, then 7..=10 and nothing else
    assert_eq!(ranges.len(), 2, "{ranges:?}");
    assert!(ranges[1].0 > ranges[0].0 + ranges[0].1);
    let used: u64 = ranges.iter().map(|r| r.1).sum();
    assert_eq!(m.used_bytes(), used);
    // 9 used clusters (2..=5, 7..=10) plus metadata, at unit granularity
    assert!(used >= data_off + 8 * cs && used <= data_off + 10 * cs + 2 * cs, "used {used}");
}

#[test]
fn fat_deleted_entries_lists_e5() {
    let fx = fat32_fixture();
    let mut fs = open_mem(fx.img.dev);
    let seen = Mutex::new(0u32);
    let sink = pm_core::FnProgress(|_p: &pm_core::ProgressReport| *seen.lock().unwrap() += 1);
    let d = fs.deleted_entries(&sink, &CancelToken::new()).unwrap();
    assert!(*seen.lock().unwrap() >= 2, "progress per directory (root + SUBDIR)");
    let mut d: Vec<Entry> = d;
    d.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(d.len(), 2, "{d:?}");
    assert!(d.iter().all(|e| e.deleted && !e.is_dir));
    assert_eq!(d[0].name, "?one.txt");
    assert_eq!(d[0].path, "/?one.txt");
    assert_eq!(d[0].size, 77);
    assert_eq!(d[0].id, 6);
    assert_eq!(d[1].name, "?ld.bin");
    assert_eq!(d[1].path, "/SUBDIR/?ld.bin");
    assert_eq!(d[1].size, 42);
    assert_eq!(d[1].id, 11);
    // cancellation is honoured
    let c = CancelToken::new();
    c.cancel();
    assert!(matches!(fs.deleted_entries(&NullProgress, &c), Err(Error::Cancelled)));
}

#[test]
fn fat16_small_image_parses() {
    let mut fx = fat1x_image(16);
    let content = b"hello fat16, spanning two clusters: ".repeat(20);
    assert!(content.len() > 512 && content.len() < 1024);
    let chain = fx.img.alloc(2);
    fx.img.write_chain(&chain, &content);
    let mut e = Short::file(dos_name("TEST", "TXT"), chain[0], content.len() as u32);
    e.date = dos_date(1999, 12, 31);
    e.time = dos_time(23, 59, 58);
    let mut vol = Short::file(*b"SMALLVOL   ", 0, 0);
    vol.attr = 0x08;
    fx.img.put_entries(fx.root, &[vol.bytes(), e.bytes()]);

    let mut dev = fx.img.dev;
    let p = detect(&mut dev, 0).unwrap();
    assert_eq!(p.kind, FsKind::Fat16);
    assert_eq!(p.label, "SMALLVOL");
    assert_eq!(p.uuid, "1234-5678");
    assert_eq!(p.total, 8 * MIB);
    assert_eq!(p.cluster, Some(512));

    let mut fs = open_mem(dev);
    assert_eq!(fs.kind(), FsKind::Fat16);
    assert_eq!(fs.label(), "SMALLVOL");
    let root = fs.root();
    assert_eq!(root.id, 0);
    let list = fs.list(&root).unwrap();
    assert_eq!(list.len(), 1, "volume label entry is skipped");
    assert_eq!(list[0].name, "TEST.TXT");
    assert_eq!(list[0].size as usize, content.len());
    assert_eq!(list[0].id, chain[0] as u64);
    assert_eq!(list[0].modified, Some(946_684_798)); // 1999-12-31T23:59:58Z
    assert_eq!(read_all(fs.as_mut(), &list[0]), content);

    let m = fs.used_map().unwrap();
    assert!(m.is_used(0));
    assert!(m.is_used(512 + 65 * 512), "second FAT");
    assert!(m.is_used((1 + 130) * 512), "fixed root region");
    let data = (1 + 130 + 32) * 512u64;
    assert!(m.is_used(data));
    assert!(m.is_used(data + 512));
    assert!(!m.is_used(data + 2 * 512));
    assert!(!m.is_used(data + 4000 * 512));
}

#[test]
fn fat12_chain_with_odd_and_even_clusters() {
    let mut fx = fat1x_image(12);
    let data = sample_bytes(512 * 5 + 100, 3);
    // deliberately out of order so odd and even FAT12 entries both appear mid-chain
    let chain = [2u32, 5, 3, 4, 7, 6];
    fx.img.write_chain(&chain, &data);
    fx.img.put_entries(fx.root, &[Short::file(dos_name("DATA", "BIN"), 2, data.len() as u32).bytes()]);
    let mut dev = fx.img.dev;
    assert_eq!(detect(&mut dev, 0).unwrap().kind, FsKind::Fat12);
    let mut fs = open_mem(dev);
    assert_eq!(fs.kind(), FsKind::Fat12);
    let root = fs.root();
    let list = fs.list(&root).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(read_all(fs.as_mut(), &list[0]), data);
    // mid-chain free and non-allocated clusters are marked used, the rest are not
    let m = fs.used_map().unwrap();
    let data_off = (1 + 18 + 14) * 512u64;
    for c in 2..=7u64 {
        assert!(m.is_used(data_off + (c - 2) * 512), "cluster {c}");
    }
    assert!(!m.is_used(data_off + 100 * 512));
}

#[test]
fn fat_chain_loop_is_reported() {
    let mut fx = fat32_fixture();
    // BIG.BIN claims 100 MB but its chain is 7 -> 9 -> 7
    fx.img.set_fat(9, 7);
    // rewrite BIG.BIN's size field (third entry slot: ".", "..", "BIG.BIN")
    let off = fx.img.cluster_off(fx.subdir_cluster) + 2 * 32 + 28;
    fx.img.dev.write_at(off, &100_000_000u32.to_le_bytes()).unwrap();
    let mut fs = open_mem(fx.img.dev);
    let root = fs.root();
    let sub = fs.list(&root).unwrap().into_iter().find(|e| e.name == "SUBDIR").unwrap();
    let big = fs.list(&sub).unwrap().remove(0);
    assert_eq!(big.size, 100_000_000);
    let mut buf = [0u8; 16];
    match fs.read_file(&big, 90_000_000, &mut buf) {
        Err(Error::Parse(m)) => assert_eq!(m, "FAT chain loop"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn open_rejects_unknown_and_unsupported() {
    assert!(matches!(open(Box::new(MemDevice::new(1 << 20))), Err(Error::Unsupported(_))));
    let mut ext = MemDevice::new(1 << 20);
    ext.write_at(0x400 + 0x38, &0xEF53u16.to_le_bytes()).unwrap();
    ext.write_at(0x400 + 0x18, &2u32.to_le_bytes()).unwrap();
    ext.write_at(0x400 + 4, &256u32.to_le_bytes()).unwrap();
    assert!(matches!(open(Box::new(ext)), Err(Error::Unsupported(_))));
}

#[test]
fn fat_works_at_a_nonzero_partition_offset_via_slice() {
    let fx = fat32_fixture();
    let inner = fx.img.dev;
    let len = inner.len();
    let mut host = MemDevice::new(len + MIB);
    host.write_at(MIB, inner.as_slice()).unwrap();
    assert!(detect(&mut host, MIB).is_some());
    assert_eq!(detect(&mut host, MIB).unwrap().kind, FsKind::Fat32);
    let slice = pm_core::SliceDevice::new(host, MIB, len).unwrap();
    let mut fs = open(Box::new(slice)).unwrap();
    let root = fs.root();
    assert_eq!(fs.list(&root).unwrap().len(), 4);
}
