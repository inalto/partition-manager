#[path = "exfat_builder.rs"]
mod exfat_builder;

use exfat_builder::*;
use pm_core::{CancelToken, Error, FnProgress, FsKind, MemDevice, NullProgress, ProgressReport};
use pm_fs::exfat::ExFat;
use pm_fs::{Entry, FileSystem};
use std::sync::Mutex;

fn open_image(image: Vec<u8>) -> ExFat {
    ExFat::open(Box::new(MemDevice::from_vec(image))).unwrap()
}

fn find<'a>(v: &'a [Entry], name: &str) -> &'a Entry {
    v.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("{name} not listed in {v:?}"))
}

fn read_all(fs: &mut dyn FileSystem, e: &Entry) -> Vec<u8> {
    let mut v = Vec::new();
    let n = fs.extract(e, &mut v).unwrap();
    assert_eq!(n as usize, v.len());
    v
}

fn coff(b: &Built, c: u32) -> u64 {
    (b.heap_start + (c as usize - 2) * CS) as u64
}

#[test]
fn exfat_lists_and_reads() {
    let built = build(&ExOpts::default());
    let mut fs = open_image(built.image.clone());
    assert_eq!(fs.kind(), FsKind::ExFat);
    assert_eq!(fs.label(), "EXFATVOL");
    assert_eq!(fs.total(), 16 << 20);
    assert_eq!(fs.cluster_size(), CS as u32);

    let root = fs.root();
    assert!(root.is_dir);
    assert_eq!(root.path, "/");
    let list = fs.list(&root).unwrap();
    let mut names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["contig", "docs", "frag.bin", "hello.txt", LONG_NAME]);
    assert!(list.iter().all(|e| !e.deleted));

    // contiguous (NoFatChain) file spanning two clusters
    let hello = find(&list, "hello.txt");
    assert!(!hello.is_dir);
    assert_eq!(hello.size, 6000);
    assert_eq!(hello.path, "/hello.txt");
    assert_eq!(hello.modified, Some(HELLO_MODIFIED));
    assert_eq!(read_all(&mut fs, hello), built.hello);
    // partial reads, across the cluster boundary and at the end
    let mut buf = vec![0u8; 100];
    assert_eq!(fs.read_file(hello, 4090, &mut buf).unwrap(), 100);
    assert_eq!(&buf[..], &built.hello[4090..4190]);
    assert_eq!(fs.read_file(hello, 5990, &mut buf).unwrap(), 10);
    assert_eq!(&buf[..10], &built.hello[5990..]);
    assert_eq!(fs.read_file(hello, 6000, &mut buf).unwrap(), 0);
    assert_eq!(fs.read_file(hello, 1 << 40, &mut buf).unwrap(), 0);

    // FAT-chained file over three non-adjacent clusters
    let frag = find(&list, "frag.bin");
    assert_eq!(frag.size, built.frag.len() as u64);
    assert_eq!(read_all(&mut fs, frag), built.frag);
    // sequential reads through the chain cursor, then a backwards seek
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 1500];
    let mut off = 0u64;
    loop {
        let n = fs.read_file(frag, off, &mut chunk).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
        off += n as u64;
    }
    assert_eq!(out, built.frag);
    assert_eq!(fs.read_file(frag, 10, &mut chunk).unwrap(), 1500);
    assert_eq!(&chunk[..], &built.frag[10..1510]);

    // a name longer than one 0xC1 entry
    let long = find(&list, LONG_NAME);
    assert_eq!(read_all(&mut fs, long), built.long);

    // FAT-chained subdirectory
    let docs = find(&list, "docs");
    assert!(docs.is_dir);
    assert_eq!(docs.attrs & 0x10, 0x10);
    let inner = fs.list(docs).unwrap();
    assert_eq!(inner.len(), 1, "the deleted old.txt is not listed");
    assert_eq!(inner[0].name, "note.txt");
    assert_eq!(inner[0].path, "/docs/note.txt");
    assert_eq!(read_all(&mut fs, &inner[0]), built.note);

    // contiguous (NoFatChain) subdirectory with a two-cluster contiguous file
    let contig = find(&list, "contig");
    let cl = fs.list(contig).unwrap();
    assert_eq!(cl.len(), 1);
    assert_eq!(cl[0].path, "/contig/c.txt");
    assert_eq!(read_all(&mut fs, &cl[0]), built.c_txt);

    // listing a file or reading a directory is refused
    assert!(matches!(fs.list(hello), Err(Error::Invalid(_))));
    assert!(matches!(fs.read_file(docs, 0, &mut buf), Err(Error::Invalid(_))));
}

#[test]
fn exfat_open_dispatch() {
    let built = build(&ExOpts::default());
    let mut fs = pm_fs::open(Box::new(MemDevice::from_vec(built.image))).unwrap();
    assert_eq!(fs.kind(), FsKind::ExFat);
    let root = fs.root();
    assert_eq!(fs.list(&root).unwrap().len(), 5);
}

#[test]
fn exfat_used_map_from_bitmap() {
    let built = build(&ExOpts::default());
    let mut fs = open_image(built.image.clone());
    let m = fs.used_map().unwrap();
    assert_eq!(m.unit, CS as u32);
    assert_eq!(m.len, 16 << 20);
    // boot region, backup boot region and the FAT are used
    assert!(m.is_used(0));
    assert!(m.is_used(FAT_OFFSET as u64 * BPS as u64));
    assert!(m.is_used(built.heap_start as u64 - 1));
    for c in 2..2 + built.cluster_count {
        let expect = built.used.contains(&c);
        assert_eq!(m.is_used(coff(&built, c)), expect, "cluster {c}");
        assert_eq!(m.is_used(coff(&built, c) + CS as u64 - 1), expect, "cluster {c} end");
    }
    assert_eq!(m.used_bytes(), built.heap_start as u64 + built.used.len() as u64 * CS as u64);
    // ranges coalesce: heap start..cluster 7 is one run
    let first = m.ranges().next().unwrap();
    assert_eq!(first, (0, coff(&built, 7)));
}

#[test]
fn exfat_used_map_unaligned_heap() {
    // heap at sector 260 = 2 KiB past a cluster boundary: the grid is shifted by 2048
    let opts = ExOpts { heap_sectors: 260 };
    let built = build(&opts);
    let mut fs = open_image(built.image.clone());
    let m = fs.used_map().unwrap();
    assert_eq!(m.origin, 2048);
    for c in 2..2 + built.cluster_count {
        let expect = built.used.contains(&c);
        assert_eq!(m.is_used(coff(&built, c)), expect, "cluster {c}");
        assert_eq!(m.is_used(coff(&built, c) + CS as u64 - 1), expect, "cluster {c} end");
    }
    // every range starts and ends on a cluster boundary of the heap (or the volume edge)
    for (s, l) in m.ranges() {
        assert!(s == 0 || (s - built.heap_start as u64) % CS as u64 == 0, "start {s}");
        assert!(s + l == m.len || (s + l - built.heap_start as u64) % CS as u64 == 0, "end {}", s + l);
    }
}

#[test]
fn exfat_used_map_short_bitmap_is_error() {
    let mut built = build(&ExOpts::default());
    // the bitmap entry in the root claims a length too short for cluster_count bits
    let root_off = coff(&built, 4) as usize;
    // entries: 0x83 label, 0x81 bitmap
    assert_eq!(built.image[root_off + 32], 0x81);
    put64(&mut built.image, root_off + 32 + 24, 10);
    let mut fs = open_image(built.image);
    assert!(matches!(fs.used_map(), Err(Error::Parse(_))));
}

#[test]
fn exfat_deleted_entries_listed() {
    let built = build(&ExOpts::default());
    let mut fs = open_image(built.image.clone());
    let reports = Mutex::new(Vec::<ProgressReport>::new());
    let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
    let mut del = fs.deleted_entries(&sink, &CancelToken::new()).unwrap();
    del.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(del.len(), 2, "{del:?}");
    assert_eq!(del[0].path, "/docs/old.txt");
    assert_eq!(del[0].name, "old.txt");
    assert_eq!(del[0].size, 50);
    assert!(del[0].deleted && !del[0].is_dir);
    assert_eq!(del[1].path, "/gone.txt");
    assert_eq!(del[1].size, 80);
    assert!(!reports.lock().unwrap().is_empty());
    // best-effort recovery of a deleted contiguous file
    assert_eq!(read_all(&mut fs, &del[1]), built.gone);
    assert_eq!(read_all(&mut fs, &del[0]), built.old);

    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(fs.deleted_entries(&NullProgress, &cancel), Err(Error::Cancelled)));
}

#[test]
fn exfat_dir_cycle_is_bounded() {
    let mut built = build(&ExOpts::default());
    // docs/ (cluster 14) now chains to itself
    let fat = FAT_OFFSET as usize * BPS;
    put32(&mut built.image, fat + 14 * 4, 14);
    let mut fs = open_image(built.image);
    let root = fs.root();
    let list = fs.list(&root).unwrap();
    let docs = find(&list, "docs").clone();
    assert!(matches!(fs.list(&docs), Err(Error::Parse(_))));
    // the scan still completes and reports the root-level deletion
    let del = fs.deleted_entries(&NullProgress, &CancelToken::new()).unwrap();
    assert!(del.iter().any(|e| e.path == "/gone.txt"));
}

fn exercise(image: Vec<u8>) {
    let Ok(mut fs) = ExFat::open(Box::new(MemDevice::from_vec(image))) else { return };
    let _ = fs.label();
    let _ = fs.total();
    let _ = fs.used_map();
    let _ = fs.deleted_entries(&NullProgress, &CancelToken::new());
    let mut stack = vec![fs.root()];
    let mut seen = 0;
    let mut buf = vec![0u8; 4096];
    while let Some(d) = stack.pop() {
        seen += 1;
        if seen > 64 {
            break;
        }
        let Ok(list) = fs.list(&d) else { continue };
        for e in list {
            if e.is_dir {
                stack.push(e);
            } else {
                let _ = fs.read_file(&e, 0, &mut buf);
                let _ = fs.read_file(&e, e.size.saturating_sub(10), &mut buf);
            }
        }
    }
}

#[test]
fn exfat_corrupt_never_panics() {
    let built = build(&ExOpts::default());

    // header fields that must be rejected at open
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut mutate = |name: &'static str, f: &dyn Fn(&mut Vec<u8>)| {
        let mut img = built.image.clone();
        f(&mut img);
        cases.push((name, img));
    };
    mutate("bps shift 0", &|i| i[108] = 0);
    mutate("bps shift 20", &|i| i[108] = 20);
    mutate("cluster shift 0xFF", &|i| i[109] = 0xFF);
    mutate("cluster shift 30", &|i| i[109] = 30);
    mutate("fat offset beyond device", &|i| put32(i, 80, 0x7FFF_0000));
    mutate("fat length huge", &|i| put32(i, 84, u32::MAX));
    mutate("heap offset beyond device", &|i| put32(i, 88, u32::MAX));
    mutate("cluster count huge", &|i| put32(i, 92, u32::MAX));
    mutate("cluster count zero", &|i| put32(i, 92, 0));
    mutate("root cluster 0", &|i| put32(i, 96, 0));
    mutate("root cluster beyond", &|i| put32(i, 96, 0xFFFF_FFF0));
    mutate("volume length huge", &|i| put64(i, 72, u64::MAX));
    mutate("volume length zero", &|i| put64(i, 72, 0));
    mutate("zero fats", &|i| i[110] = 0);
    mutate("no signature", &|i| i[3] = b'X');
    for (name, img) in cases {
        match ExFat::open(Box::new(MemDevice::from_vec(img))) {
            Err(Error::Parse(_)) | Err(Error::Unsupported(_)) => {}
            other => panic!("{name}: expected Parse/Unsupported, got {:?}", other.map(|_| ())),
        }
    }

    // an empty, a tiny and a boot-sector-only device
    for len in [0usize, 100, 512, 4096] {
        let mut img = built.image.clone();
        img.truncate(len);
        exercise(img);
    }

    // a root directory that chains to itself, and a FAT full of garbage
    let fat = FAT_OFFSET as usize * BPS;
    let mut img = built.image.clone();
    put32(&mut img, fat + 4 * 4, 4);
    exercise(img);
    let mut img = built.image.clone();
    for k in 0..(FAT_LENGTH as usize * BPS) {
        img[fat + k] = 0xFF;
    }
    exercise(img);
    let mut img = built.image.clone();
    for k in (0..FAT_LENGTH as usize * BPS).step_by(4) {
        put32(&mut img, fat + k, (k as u32 / 4) ^ 0x8000);
    }
    exercise(img);

    // seeded random corruption of the metadata areas
    let regions: [(usize, usize); 4] = [
        (0, 512),
        (fat, fat + 4 * 40),
        (coff(&built, 2) as usize, coff(&built, 2) as usize + 64),
        (coff(&built, 4) as usize, coff(&built, 4) as usize + 9 * 32 * 4),
    ];
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..150 {
        let mut img = built.image.clone();
        let flips = 1 + rnd() % 6;
        for _ in 0..flips {
            let (lo, hi) = regions[(rnd() % regions.len() as u64) as usize];
            let at = lo + (rnd() % (hi - lo) as u64) as usize;
            img[at] = rnd() as u8;
        }
        exercise(img);
    }
}

#[test]
fn exfat_used_map_full_bitmap() {
    for heap_sectors in [256u32, 260] {
        let mut built = build(&ExOpts { heap_sectors });
        let bm = coff(&built, 2) as usize;
        for b in &mut built.image[bm..bm + CS] {
            *b = 0xFF; // bits past the cluster count are padding and must be ignored
        }
        let mut fs = open_image(built.image.clone());
        let m = fs.used_map().unwrap();
        let heap_end = built.heap_start as u64 + built.cluster_count as u64 * CS as u64;
        assert_eq!(m.used_bytes(), heap_end, "heap at sector {heap_sectors}");
        assert!(m.is_used(heap_end - 1));
        assert!(!m.is_used(heap_end) || heap_end == m.len);
    }
}

/// A minimal volume with 512-byte clusters whose allocation bitmap spans eight clusters.
fn small_cluster_image(chain_bitmap: bool) -> (Vec<u8>, Vec<bool>, u32, usize) {
    const SECTORS: u64 = 32768;
    const HEAP: u32 = 513;
    let count = (SECTORS - HEAP as u64) as u32; // one sector per cluster
    let mut img = vec![0u8; SECTORS as usize * 512];
    {
        let b = &mut img[..512];
        b[3..11].copy_from_slice(b"EXFAT   ");
        put64(b, 72, SECTORS);
        put32(b, 80, 24);
        put32(b, 84, 256);
        put32(b, 88, HEAP);
        put32(b, 92, count);
        put32(b, 96, 10);
        b[108] = 9;
        b[109] = 0;
        b[110] = 1;
    }
    let fat = 24 * 512;
    let coff = |c: u32| HEAP as usize * 512 + (c as usize - 2) * 512;
    let bitmap_bytes = (count as usize).div_ceil(8);
    assert_eq!(bitmap_bytes, 4032);
    let mut seed = 0xDEAD_BEEFu32;
    let mut bits = vec![false; count as usize];
    let mut pat = vec![0u8; bitmap_bytes];
    for (i, p) in pat.iter_mut().enumerate() {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *p = match (seed >> 24) % 4 {
            0 => 0x00,
            1 => 0xFF,
            _ => (seed >> 16) as u8,
        };
        for bit in 0..8 {
            let c = i * 8 + bit;
            if c < count as usize {
                bits[c] = (*p >> bit) & 1 != 0;
            }
        }
    }
    // padding bits of the last byte are ignored: make them 1 to prove it
    pat[bitmap_bytes - 1] |= 0xFF << (count % 8);
    img[coff(2)..coff(2) + bitmap_bytes].copy_from_slice(&pat);
    if chain_bitmap {
        for c in 2..9u32 {
            put32(&mut img, fat + 4 * c as usize, c + 1);
        }
        put32(&mut img, fat + 4 * 9, 0xFFFF_FFFF);
    }
    put32(&mut img, fat + 4 * 10, 0xFFFF_FFFF);
    let mut e = [0u8; 32];
    e[0] = 0x81;
    put32(&mut e, 20, 2);
    put64(&mut e, 24, bitmap_bytes as u64);
    img[coff(10)..coff(10) + 32].copy_from_slice(&e);
    (img, bits, count, HEAP as usize * 512)
}

#[test]
fn exfat_used_map_bitmap_spans_clusters() {
    for chain in [true, false] {
        let (img, bits, count, heap) = small_cluster_image(chain);
        let mut fs = open_image(img);
        assert_eq!(fs.cluster_size(), 512);
        let m = fs.used_map().unwrap();
        assert_eq!(m.unit, 512);
        for c in 0..count {
            // clusters 2..=9 hold the bitmap and 10 the root directory: always used
            let expect = bits[c as usize] || (0..9).contains(&c);
            assert_eq!(m.is_used((heap + 512 * c as usize) as u64), expect, "chain {chain} cluster {}", c + 2);
        }
        assert!(m.is_used(0) && m.is_used(heap as u64 - 1));
    }
}
