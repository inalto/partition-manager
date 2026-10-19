#[path = "ntfs_builder.rs"]
mod ntfs_builder;

use ntfs_builder::*;
use pm_core::{CancelToken, Error, FnProgress, FsKind, MemDevice, NullProgress, ProgressReport};
use pm_fs::ntfs::{decode_runs, open_ntfs, Ntfs};
use pm_fs::{Entry, FileSystem};
use std::sync::Mutex;

fn open_image(image: Vec<u8>) -> Ntfs {
    open_ntfs(Box::new(MemDevice::from_vec(image))).unwrap()
}

fn open_default() -> (Ntfs, Built) {
    let built = build(&NtfsOpts::default());
    (open_image(built.image.clone()), built)
}

fn find<'a>(v: &'a [Entry], name: &str) -> &'a Entry {
    v.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("{name} not listed"))
}

fn read_all(fs: &mut dyn FileSystem, e: &Entry) -> Vec<u8> {
    let mut v = Vec::new();
    let n = fs.extract(e, &mut v).unwrap();
    assert_eq!(n as usize, v.len());
    v
}

fn upcase(s: &str) -> Vec<u16> {
    s.to_uppercase().encode_utf16().collect()
}

#[test]
fn decode_runs_examples() {
    assert_eq!(decode_runs(&[0x21, 0x18, 0x34, 0x56, 0x00]).unwrap(), vec![(Some(0x5634), 0x18)]);
    // second run: length 0x0114, offset +0x0211E5 relative to 0x342573
    assert_eq!(
        decode_runs(&[0x31, 0x38, 0x73, 0x25, 0x34, 0x32, 0x14, 0x01, 0xE5, 0x11, 0x02, 0x00]).unwrap(),
        vec![(Some(0x342573), 0x38), (Some(0x363758), 0x114)]
    );
    // negative relative offset: 0x20 - 0x10
    assert_eq!(
        decode_runs(&[0x11, 0x30, 0x20, 0x11, 0x10, 0xF0, 0x00]).unwrap(),
        vec![(Some(0x20), 0x30), (Some(0x10), 0x10)]
    );
    assert_eq!(decode_runs(&[0x01, 0x05, 0x00]).unwrap(), vec![(None, 5)]);
    // a sparse run does not move the LCN base
    assert_eq!(
        decode_runs(&[0x11, 0x04, 0x40, 0x01, 0x02, 0x11, 0x03, 0x10, 0x00]).unwrap(),
        vec![(Some(0x40), 4), (None, 2), (Some(0x50), 3)]
    );
    // a 2-byte length whose high byte has the sign bit clear, an 8-bit offset that is positive
    // only thanks to its second byte
    assert_eq!(decode_runs(&[0x22, 0x80, 0x00, 0x80, 0x00, 0x00]).unwrap(), vec![(Some(0x80), 0x80)]);
    assert_eq!(decode_runs(&[0x00]).unwrap(), vec![]);
    assert_eq!(decode_runs(&[]).unwrap(), vec![]);
    // the end of the buffer terminates the list as well
    assert_eq!(decode_runs(&[0x21, 0x18, 0x34, 0x56]).unwrap(), vec![(Some(0x5634), 0x18)]);

    // malformed
    for bad in [
        &[0x21, 0x18, 0x34][..],         // truncated offset
        &[0x31, 0x38][..],               // truncated
        &[0x10, 0x05, 0x00][..],         // zero-sized length field
        &[0x91, 0x05, 0, 0, 0, 0, 0, 0, 0, 0, 0][..], // offset field wider than 8 bytes
        &[0x19, 0x05][..],               // length field wider than 8 bytes
        &[0x11, 0x05, 0xF0, 0x00][..],   // first LCN negative
        &[0x11, 0x00, 0x10, 0x00][..],   // zero-length run
        &[0x11, 0xFF, 0x10, 0x00][..],   // negative length
    ] {
        assert!(matches!(decode_runs(bad), Err(Error::Parse(_))), "{bad:02X?}");
    }
}

#[test]
fn ntfs_lists_root_and_reads_resident_file() {
    let (mut fs, built) = open_default();
    assert_eq!(fs.kind(), FsKind::Ntfs);
    assert_eq!(fs.label(), "TESTVOL");
    assert_eq!(fs.cluster_size(), 4096);
    assert_eq!(fs.total(), built.total_sectors * 512);
    let root = fs.root();
    assert_eq!(root.path, "/");
    assert!(root.is_dir);
    assert_eq!(root.id, ROOT);

    let list = fs.list(&root).unwrap();
    let names: Vec<String> = list.iter().map(|e| e.name.clone()).collect();
    // in-order B-tree walk yields collation order; "." and the DOS alias are dropped
    let expected: Vec<String> = built
        .root_index_names
        .iter()
        .filter(|n| *n != "." && *n != "DOCUME~1.TXT")
        .cloned()
        .collect();
    assert_eq!(names, expected);
    let mut sorted = names.clone();
    sorted.sort_by_key(|n| upcase(n));
    assert_eq!(names, sorted);
    for want in ["$MFT", "$Bitmap", "hello.txt", "big.bin", "split.dat", "docs", "Document Long Name.txt"] {
        assert!(names.iter().any(|n| n == want), "{want} missing from {names:?}");
    }
    assert!(!names.iter().any(|n| n == "gone.txt"));

    let hello = find(&list, "hello.txt").clone();
    assert_eq!(hello.path, "/hello.txt");
    assert!(!hello.is_dir);
    assert!(!hello.deleted);
    assert_eq!(hello.size, 20);
    assert_eq!(hello.id, REC_HELLO);
    assert_eq!(hello.modified, Some(MODIFIED_UNIX));
    assert_eq!(hello.attrs, FILE_ATTR_ARCHIVE);
    assert_eq!(read_all(&mut fs, &hello), HELLO.to_vec());
    let mut buf = [0u8; 5];
    assert_eq!(fs.read_file(&hello, 7, &mut buf).unwrap(), 5);
    assert_eq!(&buf, b"NTFS ");
    assert_eq!(fs.read_file(&hello, 18, &mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], b"\r\n");
    assert_eq!(fs.read_file(&hello, 20, &mut buf).unwrap(), 0);
    assert_eq!(fs.read_file(&hello, 1 << 40, &mut buf).unwrap(), 0);

    let long = find(&list, "Document Long Name.txt");
    assert_eq!(long.id, REC_LONG);
    assert_eq!(long.size, 5);

    let docs = find(&list, "docs").clone();
    assert!(docs.is_dir);
    assert_eq!(docs.id, REC_DOCS);
    assert_eq!(docs.size, 0);
    assert!(fs.read_file(&docs, 0, &mut buf).is_err());
    let sub = fs.list(&docs).unwrap();
    assert_eq!(sub.len(), 1);
    assert_eq!(sub[0].name, "readme.txt");
    assert_eq!(sub[0].path, "/docs/readme.txt");
    assert_eq!(read_all(&mut fs, &sub[0]), README_DATA.to_vec());
    assert!(fs.list(&hello).is_err());

    // detection and the generic open() agree
    let mut dev = MemDevice::from_vec(built.image.clone());
    let probe = pm_fs::detect(&mut dev, 0).unwrap();
    assert_eq!(probe.kind, FsKind::Ntfs);
    let mut generic = pm_fs::open(Box::new(MemDevice::from_vec(built.image.clone()))).unwrap();
    assert_eq!(generic.kind(), FsKind::Ntfs);
    let r = generic.root();
    assert_eq!(generic.list(&r).unwrap().len(), list.len());

    // a torn write (sector end no longer matching the update sequence number) is a parse error
    let mut image = built.image.clone();
    let off = record_offset(&NtfsOpts::default(), REC_HELLO) as usize + 510;
    image[off] ^= 0xFF;
    let mut torn = open_image(image);
    match torn.read_file(&hello, 0, &mut buf) {
        Err(Error::Parse(m)) => assert!(m.contains("fixup"), "{m}"),
        other => panic!("expected a fixup error, got {other:?}"),
    }
    // listing still works, falling back to the index copy of the name
    let l = torn.list(&torn.root()).unwrap();
    assert!(l.iter().any(|e| e.name == "hello.txt"));

    // not NTFS at all
    assert!(matches!(open_ntfs(Box::new(MemDevice::new(1 << 20))), Err(Error::Parse(_))));
}

#[test]
fn ntfs_reads_nonresident_file_across_runs() {
    let (mut fs, _) = open_default();
    let list = fs.list(&fs.root()).unwrap();
    let big = find(&list, "big.bin").clone();
    // the index copy of the size is stale (0); the record's $DATA wins
    assert_eq!(big.size, BIG_SIZE);
    assert_eq!(big.modified, Some(MODIFIED_UNIX));
    let expected = expected_big();
    assert_eq!(read_all(&mut fs, &big), expected);

    // a read straddling the boundary between run A (3 clusters) and run B
    let mut buf = vec![0u8; 100];
    let at = 3 * 4096 - 40;
    assert_eq!(fs.read_file(&big, at, &mut buf).unwrap(), 100);
    assert_eq!(buf, expected[at as usize..at as usize + 100]);
    // clipped at the real size
    assert_eq!(fs.read_file(&big, BIG_SIZE - 5, &mut buf).unwrap(), 5);
    assert_eq!(buf[..5], expected[(BIG_SIZE - 5) as usize..]);
    assert_eq!(fs.read_file(&big, BIG_SIZE, &mut buf).unwrap(), 0);
    // small odd-sized reads walk the whole file identically
    let mut acc = Vec::new();
    let mut off = 0u64;
    let mut chunk = vec![0u8; 777];
    loop {
        let n = fs.read_file(&big, off, &mut chunk).unwrap();
        if n == 0 {
            break;
        }
        acc.extend_from_slice(&chunk[..n]);
        off += n as u64;
    }
    assert_eq!(acc, expected);

    // sparse runs and bytes past the initialized size read as zeros
    let sparse = find(&list, "sparse.bin").clone();
    assert_eq!(sparse.size, SPARSE_SIZE);
    assert_eq!(read_all(&mut fs, &sparse), expected_sparse());

    let comp = find(&list, "comp.bin").clone();
    match fs.read_file(&comp, 0, &mut buf) {
        Err(Error::Unsupported(m)) => assert_eq!(m, "NTFS compressed"),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    let enc = find(&list, "enc.bin").clone();
    match fs.read_file(&enc, 0, &mut buf) {
        Err(Error::Unsupported(m)) => assert_eq!(m, "NTFS encrypted"),
        other => panic!("expected Unsupported, got {other:?}"),
    }

    // the unnamed $DATA is read even when a named stream exists ($BadClus:$Bad)
    let bad = find(&list, "$BadClus").clone();
    assert_eq!(bad.size, 0);
    assert_eq!(read_all(&mut fs, &bad), Vec::<u8>::new());
}

#[test]
fn ntfs_fragmented_mft_runs() {
    let opts = NtfsOpts { mft_second_lcn: Some(100), ..NtfsOpts::default() };
    let built = build(&opts);
    let mut fs = open_image(built.image.clone());
    let root = fs.root();
    let list = fs.list(&root).unwrap();
    // records 16.. live in the second fragment
    let hello = find(&list, "hello.txt").clone();
    assert_eq!(read_all(&mut fs, &hello), HELLO.to_vec());
    let big = find(&list, "big.bin").clone();
    assert_eq!(read_all(&mut fs, &big), expected_big());
    let split = find(&list, "split.dat").clone();
    assert_eq!(read_all(&mut fs, &split), expected_split());
    let mft = find(&list, "$MFT");
    assert_eq!(mft.size, MFT_RECORDS * 1024);

    let map = fs.used_map().unwrap();
    for c in [16u64, 17, 100, 101, 105] {
        assert!(map.is_used(c * 4096), "cluster {c}");
    }
    for c in 18..24u64 {
        assert!(!map.is_used(c * 4096), "cluster {c} lies in the gap between MFT fragments");
    }
    let del = fs.deleted_entries(&NullProgress, &CancelToken::new()).unwrap();
    assert!(del.iter().any(|e| e.name == "gone.txt"));
}

#[test]
fn ntfs_used_map_from_bitmap() {
    let (mut fs, built) = open_default();
    let map = fs.used_map().unwrap();
    assert_eq!(map.unit, 4096);
    // the map covers the backup boot sector that follows the last cluster
    assert_eq!(map.len, (built.total_sectors + 1) * 512);
    assert_eq!(map.len, 16 * MIB);
    let clusters = built.total_sectors * 512 / 4096;
    for c in 0..clusters {
        assert_eq!(map.is_used(c * 4096), built.used_clusters.contains(&c), "cluster {c}");
    }
    assert!(map.is_used(LCN_ORPHAN * 4096));
    assert!(!map.is_used(1000 * 4096));
    assert!(map.is_used(built.total_sectors * 512), "backup boot sector");
    assert_eq!(map.used_bytes(), (built.used_clusters.len() as u64 + 1) * 4096);

    // a $Bitmap whose real or initialized size falls short of the volume: every cluster it
    // does not describe must count as used (a clone must never drop data)
    for (real, init, covered) in [(Some(256u64), None, 256u64), (None, Some(300u64), 300)] {
        let short = build(&NtfsOpts { bitmap_real: real, bitmap_init: init, ..NtfsOpts::default() });
        let mut fs = open_image(short.image.clone());
        let map = fs.used_map().unwrap();
        for c in 0..clusters {
            let want = c >= covered * 8 || short.used_clusters.contains(&c);
            assert_eq!(map.is_used(c * 4096), want, "cluster {c}, $Bitmap covers {covered} bytes");
        }
    }
}

#[test]
fn ntfs_deleted_record_listed() {
    let (mut fs, _) = open_default();
    let reports = Mutex::new(Vec::<ProgressReport>::new());
    let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
    let mut del = fs.deleted_entries(&sink, &CancelToken::new()).unwrap();
    del.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = del.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["gone.txt", "old file.txt"]);

    let gone = &del[0];
    assert!(gone.deleted);
    assert!(!gone.is_dir);
    assert_eq!(gone.path, "/gone.txt");
    assert_eq!(gone.size, GONE_DATA.len() as u64);
    assert_eq!(gone.id, REC_GONE);
    assert_eq!(gone.modified, Some(MODIFIED_UNIX));
    let old = &del[1];
    assert_eq!(old.path, "/docs/old file.txt");
    assert_eq!(old.id, REC_OLD);
    assert_eq!(old.size, OLD_DATA.len() as u64);

    // deleted files remain readable from their record
    let gone = gone.clone();
    assert_eq!(read_all(&mut fs, &gone), GONE_DATA.to_vec());

    let r = reports.lock().unwrap();
    let last = r.last().expect("progress reported");
    assert_eq!(last.done, MFT_RECORDS);
    assert_eq!(last.total, MFT_RECORDS);
    drop(r);

    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(fs.deleted_entries(&NullProgress, &cancel), Err(Error::Cancelled)));

    // a corrupt, huge $MFT size: the scan is bounded by the mapped runs, not by the size field
    let huge = 1u64 << 60;
    let built = build(&NtfsOpts { mft_real: Some(huge), mft_alloc: Some(huge), ..NtfsOpts::default() });
    let mut fs = open_image(built.image);
    let cancel = CancelToken::new();
    let guard = cancel.clone();
    // stop a runaway scan instead of hanging the test
    let sink = FnProgress(move |p: &ProgressReport| {
        if p.total > MFT_RECORDS {
            guard.cancel()
        }
    });
    let mut again = fs.deleted_entries(&sink, &cancel).expect("bounded scan");
    again.sort_by(|a, b| a.name.cmp(&b.name));
    let again: Vec<&str> = again.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(again, names);
    let hello = find(&fs.list(&fs.root()).unwrap(), "hello.txt").clone();
    assert_eq!(read_all(&mut fs, &hello), HELLO.to_vec());

    // a real size beyond the allocated size is rejected when opening
    let built = build(&NtfsOpts { mft_real: Some(2 * MFT_RECORDS * 1024), ..NtfsOpts::default() });
    assert!(matches!(open_ntfs(Box::new(MemDevice::from_vec(built.image))), Err(Error::Parse(_))));
}

#[test]
fn ntfs_attribute_list_followed() {
    let (mut fs, _) = open_default();
    let list = fs.list(&fs.root()).unwrap();
    let split = find(&list, "split.dat").clone();
    assert_eq!(split.id, REC_SPLIT);
    assert_eq!(split.size, SPLIT_SIZE);
    assert_eq!(split.modified, Some(MODIFIED_UNIX));
    let expected = expected_split();
    assert_eq!(read_all(&mut fs, &split), expected);
    // straight across the second extent (extension record 27) into the third
    let mut buf = vec![0u8; 5000];
    assert_eq!(fs.read_file(&split, 4000, &mut buf).unwrap(), 5000);
    assert_eq!(buf, expected[4000..9000]);
    assert_eq!(fs.read_file(&split, SPLIT_SIZE - 100, &mut buf).unwrap(), 100);
    assert_eq!(buf[..100], expected[(SPLIT_SIZE - 100) as usize..]);
    // extension records are not files of their own
    assert!(!list.iter().any(|e| [REC_SPLIT_EXT1, REC_SPLIT_EXT2, REC_SPLIT_EXT3].contains(&e.id)));

    // the attribute list lost its middle $DATA extent: a parse error, never silent zeros,
    // even for reads that only touch the extents still present
    let broken = build(&NtfsOpts { split_drop_middle_extent: true, ..NtfsOpts::default() });
    let mut fs = open_image(broken.image);
    let mut buf = vec![0u8; 100];
    for off in [0u64, 4096, 2 * 4096] {
        assert!(matches!(fs.read_file(&split, off, &mut buf), Err(Error::Parse(_))), "offset {off}");
    }
    // the sparse runs inside one extent still read fine
    let list = fs.list(&fs.root()).unwrap();
    let sparse = find(&list, "sparse.bin").clone();
    assert_eq!(read_all(&mut fs, &sparse), expected_sparse());
}
