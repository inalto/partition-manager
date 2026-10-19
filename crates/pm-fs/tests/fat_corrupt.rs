//! Bounded reads, unit alignment and corrupt-input behaviour of the FAT reader and formatter.

mod common;

use common::*;
use pm_core::{BlockDevice, CancelToken, Error, FsKind, MemDevice, NullProgress};
use pm_fs::{detect, format_fat32, open, Entry, FileSystem, FormatOpts};

fn open_mem(dev: MemDevice) -> Box<dyn FileSystem> {
    open(Box::new(dev)).unwrap()
}

fn read_all(fs: &mut dyn FileSystem, e: &Entry) -> Vec<u8> {
    let mut v = Vec::new();
    fs.extract(e, &mut v).unwrap();
    v
}

fn find(fs: &mut Box<dyn FileSystem>, dir: &Entry, name: &str) -> Entry {
    fs.list(dir).unwrap().into_iter().find(|e| e.name == name).unwrap()
}

#[test]
fn fat_dir_chain_too_long_is_parse_error() {
    let mut fx = fat32_fixture();
    // SUBDIR's single cluster now points at itself: an endless directory
    fx.img.set_fat(5, 5);
    let mut fs = open_mem(fx.img.dev);
    let root = fs.root();
    let sub = find(&mut fs, &root, "SUBDIR");
    match fs.list(&sub) {
        Err(Error::Parse(m)) => assert_eq!(m, "directory chain too long"),
        other => panic!("{other:?}"),
    }
    // the deleted-entry scan skips the damaged directory and still reports the rest
    let d = fs.deleted_entries(&NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].name, "?one.txt");
}

#[test]
fn fat_used_map_clamps_to_device_len() {
    let fx = fat32_fixture();
    // keep the metadata and the first clusters, drop the second half of the volume
    let mut bytes = fx.img.dev.as_slice().to_vec();
    bytes.truncate((20 * MIB) as usize);
    let mut fs = open_mem(MemDevice::from_vec(bytes));
    assert_eq!(fs.total(), 40 * MIB, "total still comes from the BPB");
    let m = fs.used_map().unwrap();
    assert_eq!(m.len, 20 * MIB);
    assert!(m.is_used(0));
    assert!(!m.is_used(20 * MIB), "beyond the device");
    assert!(m.ranges().all(|(s, l)| s + l <= 20 * MIB));
}

#[test]
fn fat_used_map_is_exact_when_data_region_is_unaligned() {
    // 16 MiB FAT16, 2 KiB clusters; the data region starts at sector 99 = byte 50688,
    // which is not a multiple of the cluster size
    let mut fx = fat1x_custom(16, 32768, 4, 1, 33, 512, 0xF8);
    let cs = fx.img.cluster_size();
    assert_eq!(cs, 2048);
    let data_start = fx.img.data_off;
    assert_ne!(data_start % cs, 0);
    // two files: clusters 2..=4 and 7..=8
    let a = fx.img.alloc(3);
    fx.img.write_chain(&a, &sample_bytes(3 * 2048, 5));
    fx.img.next_free = 7;
    let b = fx.img.alloc(2);
    fx.img.write_chain(&b, &sample_bytes(2 * 2048, 3));
    fx.img.put_entries(
        fx.root,
        &[
            Short::file(dos_name("A", "BIN"), a[0], 3 * 2048).bytes(),
            Short::file(dos_name("B", "BIN"), b[0], 2 * 2048).bytes(),
        ],
    );
    let total = fx.img.dev.len();
    let mut fs = open_mem(fx.img.dev);
    assert_eq!(fs.kind(), FsKind::Fat16);
    assert_eq!(fs.cluster_size(), 2048);
    let m = fs.used_map().unwrap();
    // exactly reserved + FATs + root + five clusters
    assert_eq!(m.used_bytes(), data_start + 5 * cs);
    let r: Vec<_> = m.ranges().collect();
    assert_eq!(r, vec![(0, data_start + 3 * cs), (data_start + 5 * cs, 2 * cs)]);
    assert!(m.is_used(data_start - 1));
    assert!(m.is_used(data_start + 3 * cs - 1));
    assert!(!m.is_used(data_start + 3 * cs));
    assert!(!m.is_used(data_start + 5 * cs - 1));
    assert!(m.is_used(data_start + 5 * cs));
    assert!(!m.is_used(data_start + 7 * cs));
    assert_eq!(m.len, total);
}

fn fat16_root_fs(entries: &[[u8; 32]], tail: Option<(u64, [u8; 32])>) -> Box<dyn FileSystem> {
    let mut fx = fat1x_image(16);
    fx.img.put_entries(fx.root, entries);
    if let Some((slot, e)) = tail {
        let DirLoc::Fixed(off, _) = fx.root else { unreachable!() };
        fx.img.dev.write_at(off + slot * 32, &e).unwrap();
    }
    open_mem(fx.img.dev)
}

fn root_names(fs: &mut Box<dyn FileSystem>) -> Vec<String> {
    let root = fs.root();
    fs.list(&root).unwrap().into_iter().map(|e| e.name).collect()
}

#[test]
fn fat_lfn_corruption_falls_back_to_short_name() {
    let short = dos_name("LONGFI~1", "TXT");
    let file = Short::file(short, 2, 0).bytes();
    let good = lfn_entries("Long File Name.txt", &short);
    assert_eq!(good.len(), 2);

    // control: intact set
    let mut set = good.clone();
    set.push(file);
    assert_eq!(root_names(&mut fat16_root_fs(&set, None)), vec!["Long File Name.txt"]);

    // pieces in the wrong order
    let mut set: Vec<[u8; 32]> = good.iter().rev().cloned().collect();
    set.push(file);
    assert_eq!(root_names(&mut fat16_root_fs(&set, None)), vec!["LONGFI~1.TXT"]);

    // bad checksum in every piece, and in one piece only
    for bad in [vec![0, 1], vec![1]] {
        let mut set = good.clone();
        for i in bad {
            set[i][13] ^= 0xFF;
        }
        set.push(file);
        assert_eq!(root_names(&mut fat16_root_fs(&set, None)), vec!["LONGFI~1.TXT"]);
    }

    // a missing piece (only the first-on-disk piece present)
    let set = vec![good[0], file];
    assert_eq!(root_names(&mut fat16_root_fs(&set, None)), vec!["LONGFI~1.TXT"]);

    // sequence number 0 in the start piece
    let mut broken = good[0];
    broken[0] = 0x40;
    let set = vec![broken, good[1], file];
    assert_eq!(root_names(&mut fat16_root_fs(&set, None)), vec!["LONGFI~1.TXT"]);

    // an orphan set must not leak into the next, unrelated entry
    let other = Short::file(dos_name("OTHER", "TXT"), 2, 0).bytes();
    let mut set = good.clone();
    set.push(other);
    assert_eq!(root_names(&mut fat16_root_fs(&set, None)), vec!["OTHER.TXT"]);
}

#[test]
fn fat_orphan_lfn_before_terminator_is_ignored() {
    let short = dos_name("GHOST", "TXT");
    let set = lfn_entries("Ghost of a long name.txt", &short);
    // fragments, then the 0x00 terminator (the rest of the root is zero); an entry placed
    // after the terminator must not be seen
    let after = Short::file(dos_name("HIDDEN", "TXT"), 2, 0).bytes();
    let mut fs = fat16_root_fs(&set, Some((10, after)));
    assert!(root_names(&mut fs).is_empty());
}

#[test]
fn fat_kanji_escape_byte_0x05_means_0xe5() {
    let mut name = dos_name("AB", "TXT");
    name[0] = 0x05;
    let mut fs = fat16_root_fs(&[Short::file(name, 2, 0).bytes()], None);
    let names = root_names(&mut fs);
    assert_eq!(names.len(), 1, "an escaped name is a live entry, not a deleted one");
    assert_eq!(names[0].chars().next(), Some('\u{e5}'));
    assert!(names[0].ends_with("B.TXT"));
}

#[test]
fn fat_directory_chain_outside_data_region_is_parse_error() {
    for bad in [0x0FFF_FFF0u32, 0x0FFF_FFF7, 1, 0, 0x00FF_FFFF] {
        let mut fx = fat32_fixture();
        fx.img.set_fat(5, bad);
        let mut fs = open_mem(fx.img.dev);
        let root = fs.root();
        let sub = find(&mut fs, &root, "SUBDIR");
        assert!(matches!(fs.list(&sub), Err(Error::Parse(_))), "FAT[5] = {bad:#x}");
    }
    // a directory whose start cluster is outside the volume
    let mut fx = fat32_fixture();
    fx.img.put_entries(DirLoc::Cluster(2), &[Short::dir(dos_name("FAR", ""), 0x0FFF_FF00).bytes()]);
    let mut fs = open_mem(fx.img.dev);
    let root = fs.root();
    let far = find(&mut fs, &root, "FAR");
    assert!(matches!(fs.list(&far), Err(Error::Parse(_))));
}

#[test]
fn fat32_entries_with_high_bits_set_are_masked() {
    let mut fx = fat32_fixture();
    // BIG.BIN's chain 7 -> 9 -> 8 and SUBDIR's own chain, with the reserved top nibble set
    fx.img.set_fat(7, 9 | 0xF000_0000);
    fx.img.set_fat(9, 8 | 0xA000_0000);
    fx.img.set_fat(8, 0xFFFF_FFFF);
    fx.img.set_fat(5, 0xF000_0000 | 0x0FFF_FFFF);
    let big = fx.big.clone();
    let cluster9 = fx.img.cluster_off(9);
    let mut fs = open_mem(fx.img.dev);
    let root = fs.root();
    let sub = find(&mut fs, &root, "SUBDIR");
    let list = fs.list(&sub).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(read_all(fs.as_mut(), &list[0]), big);
    let m = fs.used_map().unwrap();
    assert!(m.is_used(cluster9));
}

/// Fresh image of the given FAT width plus the root location, with free clusters from 100.
fn width_image(bits: u8) -> (Img, DirLoc) {
    match bits {
        32 => {
            let mut fx = fat32_fixture();
            fx.img.next_free = 100;
            (fx.img, DirLoc::Cluster(fx.root_cluster))
        }
        b => {
            let mut fx = fat1x_image(b);
            fx.img.next_free = 100;
            (fx.img, fx.root)
        }
    }
}

#[test]
fn fat_eoc_and_bad_cluster_values_end_or_fail_chains_for_each_width() {
    // (width, smallest EOC value, a larger EOC value, bad-cluster marker)
    let cases = [
        (12u8, 0xFF8u32, 0xFFFu32, 0xFF7u32),
        (16, 0xFFF8, 0xFFFF, 0xFFF7),
        (32, 0x0FFF_FFF8, 0x0FFF_FFFF, 0x0FFF_FFF7),
    ];
    for (bits, eoc_lo, eoc_hi, bad) in cases {
        // a directory chain ends cleanly at either EOC value
        for eoc in [eoc_lo, eoc_hi] {
            let (mut img, root) = width_image(bits);
            let d = img.alloc(1)[0];
            img.set_fat(d, eoc);
            img.put_entries(root, &[Short::dir(dos_name("SUB", ""), d).bytes()]);
            img.put_entries(DirLoc::Cluster(d), &[Short::file(dos_name("F", "TXT"), 0, 0).bytes()]);
            let mut fs = open_mem(img.dev);
            let r = fs.root();
            let sub = find(&mut fs, &r, "SUB");
            assert_eq!(fs.list(&sub).unwrap().len(), 1, "width {bits} eoc {eoc:#x}");
        }
        // a directory chain running into a bad-cluster marker is an error
        let (mut img, root) = width_image(bits);
        let d = img.alloc(1)[0];
        img.set_fat(d, bad);
        img.put_entries(root, &[Short::dir(dos_name("SUB", ""), d).bytes()]);
        let mut fs = open_mem(img.dev);
        let r = fs.root();
        let sub = find(&mut fs, &r, "SUB");
        assert!(matches!(fs.list(&sub), Err(Error::Parse(_))), "width {bits} bad dir");

        // a three-cluster file whose middle FAT entry is EOC / bad: the clusters before the
        // break read fine, the rest is an error rather than a panic or an endless walk
        for (marker, label) in [(eoc_lo, "eoc_lo"), (eoc_hi, "eoc_hi"), (bad, "bad")] {
            let (mut img, root) = width_image(bits);
            let c = img.alloc(3);
            let data = sample_bytes(3 * 512, 11);
            img.write_chain(&c, &data);
            img.set_fat(c[1], marker);
            img.put_entries(root, &[Short::file(dos_name("F", "BIN"), c[0], 1536).bytes()]);
            let mut fs = open_mem(img.dev);
            let r = fs.root();
            let f = find(&mut fs, &r, "F.BIN");
            let mut buf = vec![0u8; 1024];
            assert_eq!(fs.read_file(&f, 0, &mut buf).unwrap(), 1024, "width {bits} {label}");
            assert_eq!(&buf[..], &data[..1024]);
            assert!(
                matches!(fs.read_file(&f, 1024, &mut buf), Err(Error::Parse(_))),
                "width {bits} {label}"
            );
        }
    }
}

#[test]
fn format_fat32_with_4096_byte_sectors_round_trips() {
    let mut dev = MemDevice::new(280 * MIB).with_sector_size(4096);
    format_fat32(
        &mut dev,
        &FormatOpts { label: "BIGSEC".into(), cluster_size: None, volume_id: Some(0x0BAD_CAFE) },
    )
    .unwrap();
    {
        let b = dev.as_slice();
        assert_eq!(u16::from_le_bytes([b[11], b[12]]), 4096);
        assert_eq!(&b[510..512], &[0x55, 0xAA]);
        assert_eq!(&b[6 * 4096..6 * 4096 + 512], &b[0..512], "backup boot sector");
        assert_eq!(&b[4096..4100], b"RRaA");
    }
    let p = detect(&mut dev, 0).unwrap();
    assert_eq!(p.kind, FsKind::Fat32);
    assert_eq!(p.cluster, Some(4096));
    assert_eq!(p.label, "BIGSEC");
    assert_eq!(p.uuid, "0BAD-CAFE");
    assert_eq!(p.total, 280 * MIB);
    let mut fs = open_mem(dev);
    assert_eq!(fs.label(), "BIGSEC");
    assert_eq!(fs.cluster_size(), 4096);
    let root = fs.root();
    assert!(fs.list(&root).unwrap().is_empty());
    assert!(fs.deleted_entries(&NullProgress, &CancelToken::new()).unwrap().is_empty());
    // reserved area + FATs + the root cluster, nothing else
    let m = fs.used_map().unwrap();
    assert!(m.is_used(0));
    assert!(!m.is_used(m.len - 1));
    let (start, len) = m.ranges().next().unwrap();
    assert_eq!(start, 0);
    assert_eq!(m.used_bytes(), len, "a single range");
    assert_eq!(len % 4096, 0);
}

#[test]
fn format_fat32_on_a_5_gib_sparse_file_follows_the_size_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.img");
    let size = 5 * pm_core::GIB;
    let mut dev = pm_core::FileDevice::create(&path, size).unwrap();
    format_fat32(&mut dev, &FormatOpts { label: "FIVEGIG".into(), cluster_size: None, volume_id: Some(7) })
        .unwrap();
    let mut s0 = [0u8; 512];
    dev.read_at(0, &mut s0).unwrap();
    let bps = u16::from_le_bytes([s0[11], s0[12]]) as u64;
    let spc = s0[13] as u64;
    let reserved = u16::from_le_bytes([s0[14], s0[15]]) as u64;
    let total = u32::from_le_bytes([s0[32], s0[33], s0[34], s0[35]]) as u64;
    let fat_sectors = u32::from_le_bytes([s0[36], s0[37], s0[38], s0[39]]) as u64;
    assert_eq!(total, size / 512);
    assert_eq!(bps * spc, 4096, "<= 8 GiB uses 4 KiB clusters");
    let clusters = (total - reserved - 2 * fat_sectors) / spc;
    assert!(
        fat_sectors * (bps / 4) >= clusters + 2,
        "FAT too small: {fat_sectors} sectors for {clusters} clusters"
    );
    // not wastefully large either: one sector fewer would no longer fit
    let fewer = fat_sectors - 1;
    let clusters_fewer = (total - reserved - 2 * fewer) / spc;
    assert!(fewer * (bps / 4) < clusters_fewer + 2, "FAT is more than one sector too large");
    let p = detect(&mut dev, 0).unwrap();
    assert_eq!(p.kind, FsKind::Fat32);
    assert_eq!(p.cluster, Some(4096));
    assert_eq!(p.total, size);
}
