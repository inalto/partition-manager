#[path = "ext_builder.rs"]
mod ext_builder;

use ext_builder::*;
use pm_core::{CancelToken, Error, FnProgress, FsKind, MemDevice, NullProgress, ProgressReport};
use pm_fs::ext::Ext;
use pm_fs::{Entry, FileSystem};
use std::sync::Mutex;

fn open_image(image: Vec<u8>) -> Ext {
    Ext::open(Box::new(MemDevice::from_vec(image))).unwrap()
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

fn root_names(fs: &mut Ext) -> Vec<String> {
    let root = fs.root();
    let mut names: Vec<String> = fs.list(&root).unwrap().into_iter().map(|e| e.name).collect();
    names.sort();
    names
}

fn variants() -> Vec<ExtOpts> {
    let mut v = Vec::new();
    for bs in [1024usize, 4096] {
        for ext4 in [true, false] {
            for desc64 in [false, true] {
                v.push(ExtOpts { block_size: bs, ext4, desc64, ..Default::default() });
            }
        }
    }
    v
}

fn inode_off(b: &ExtBuilt, n: u32) -> usize {
    let g = (n - 1) / IPG;
    b.itable[g as usize] as usize * b.bs + ((n - 1) % IPG) as usize * b.inode_size
}

#[test]
fn ext4_extent_file_read() {
    for bs in [1024usize, 4096] {
        let opts = ExtOpts { block_size: bs, ..Default::default() };
        let built = build(&opts);
        let mut fs = open_image(built.image.clone());
        assert_eq!(fs.kind(), FsKind::Ext4);
        assert_eq!(fs.label(), "EXTVOL1");
        assert_eq!(fs.total(), 16 << 20);
        assert_eq!(fs.cluster_size(), bs as u32);

        let root = fs.root();
        assert!(root.is_dir);
        assert_eq!(root.id, 2);
        assert_eq!(root.path, "/");
        let list = fs.list(&root).unwrap();
        let mut names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            vec!["extent.bin", "hello.txt", "htree", "indirect.bin", "link", "lost+found", "sparse.bin", "subdir"],
            "bs {bs}"
        );

        // single-extent file
        let hello = find(&list, "hello.txt");
        assert_eq!(hello.id, 12);
        assert_eq!(hello.size, built.hello.len() as u64);
        assert_eq!(hello.modified, Some(1_700_000_000));
        assert!(!hello.is_dir && !hello.deleted);
        assert_eq!(read_all(&mut fs, hello), built.hello);

        // depth-1 extent tree: index node -> leaf with two extents, a hole and an uninitialized extent
        let ext = find(&list, "extent.bin");
        assert_eq!(ext.size, built.extent_file.len() as u64);
        assert_eq!(read_all(&mut fs, ext), built.extent_file);
        // reads straddling extents, holes and the uninitialized tail
        for (off, len) in [(0usize, 10usize), (bs - 5, 20), (3 * bs - 1, 2 * bs + 2), (5 * bs - 3, 9), (7 * bs - 4, 3 * bs)] {
            let mut buf = vec![0u8; len];
            let n = fs.read_file(ext, off as u64, &mut buf).unwrap();
            let want = built.extent_file.len().saturating_sub(off).min(len);
            assert_eq!(n, want, "bs {bs} off {off}");
            assert_eq!(&buf[..n], &built.extent_file[off..off + n], "bs {bs} off {off}");
        }
        let mut buf = [0u8; 8];
        assert_eq!(fs.read_file(ext, built.extent_file.len() as u64, &mut buf).unwrap(), 0);

        // directory, nested file
        let sub = find(&list, "subdir");
        assert!(sub.is_dir);
        let inner = fs.list(sub).unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].path, "/subdir/inner.txt");
        assert_eq!(read_all(&mut fs, &inner[0]), built.inner);

        // fast symlink
        let link = find(&list, "link");
        assert!(!link.is_dir);
        assert_eq!(read_all(&mut fs, link), b"hello.txt");

        // htree-style directory is read linearly; the dx_root block yields nothing but "." and ".."
        let ht = find(&list, "htree");
        let hl = fs.list(ht).unwrap();
        let mut hn: Vec<&str> = hl.iter().map(|e| e.name.as_str()).collect();
        hn.sort();
        assert_eq!(hn, vec!["a.txt", "b.txt"]);
        assert_eq!(read_all(&mut fs, find(&hl, "b.txt")), built.inner);

        // lost+found is empty
        assert!(fs.list(find(&list, "lost+found")).unwrap().is_empty());
        // wrong-kind operations
        assert!(matches!(fs.list(hello), Err(Error::Invalid(_))));
        assert!(matches!(fs.read_file(sub, 0, &mut buf), Err(Error::Invalid(_))));
    }
}

#[test]
fn ext2_indirect_blocks_read() {
    for bs in [1024usize, 4096] {
        let opts = ExtOpts { block_size: bs, ext4: false, ..Default::default() };
        let built = build(&opts);
        let mut fs = open_image(built.image.clone());
        assert_eq!(fs.kind(), FsKind::Ext2);
        let names = root_names(&mut fs);
        assert!(!names.contains(&"extent.bin".to_string()));
        assert!(names.contains(&"indirect.bin".to_string()));
        let root = fs.root();
        let list = fs.list(&root).unwrap();

        // direct blocks + single indirect
        let ind = find(&list, "indirect.bin");
        assert_eq!(ind.size, built.indirect.len() as u64);
        assert_eq!(read_all(&mut fs, ind), built.indirect);
        let mut buf = vec![0u8; 3000];
        let off = 11 * bs as u64 + 100;
        let n = fs.read_file(ind, off, &mut buf).unwrap();
        assert_eq!(&buf[..n], &built.indirect[off as usize..off as usize + n]);

        // direct file (no extents)
        assert_eq!(read_all(&mut fs, find(&list, "hello.txt")), built.hello);

        // double and triple indirect blocks in a sparse file: holes read as zeros
        let sp = find(&list, "sparse.bin");
        assert_eq!(sp.size, built.sparse_size);
        for (off, data) in &built.sparse_data {
            let mut buf = vec![0u8; data.len() + 50];
            let n = fs.read_file(sp, *off, &mut buf).unwrap();
            assert_eq!(&buf[..n.min(data.len())], &data[..], "off {off}");
            let want_n = (built.sparse_size - off).min(buf.len() as u64) as usize;
            assert_eq!(n, want_n, "off {off}");
            assert!(buf[data.len().min(n)..n].iter().all(|&b| b == 0));
        }
        // a hole between mapped blocks
        let mut buf = vec![0xFFu8; 5000];
        let n = fs.read_file(sp, 5 * bs as u64, &mut buf).unwrap();
        assert_eq!(n, 5000);
        assert!(buf.iter().all(|&b| b == 0));
        // read across the boundary of the first (direct) block into the hole after it
        let mut buf = vec![0xFFu8; 100];
        fs.read_file(sp, bs as u64 - 50, &mut buf).unwrap();
        assert_eq!(&buf[..50], &built.sparse_data[0].1[bs - 50..]);
        assert!(buf[50..].iter().all(|&b| b == 0));
        // at and past the end
        assert_eq!(fs.read_file(sp, built.sparse_size, &mut buf).unwrap(), 0);
        assert_eq!(fs.read_file(sp, built.sparse_size + 10_000, &mut buf).unwrap(), 0);

        // dispatch through open() reports ext2
        let fs2 = pm_fs::open(Box::new(MemDevice::from_vec(built.image))).unwrap();
        assert_eq!(fs2.kind(), FsKind::Ext2);
    }
}

#[test]
fn ext_used_map_from_group_bitmaps() {
    let mut all = variants();
    all.push(ExtOpts { uninit_g1: true, ..Default::default() });
    all.push(ExtOpts { uninit_g1: true, ext4: false, ..Default::default() });
    for opts in all {
        let built = build(&opts);
        let mut fs = open_image(built.image.clone());
        let m = fs.used_map().unwrap();
        let bs = built.bs as u64;
        assert_eq!(m.unit as u64, bs);
        assert_eq!(m.origin, 0);
        assert_eq!(m.len, built.blocks_count * bs);
        for b in 0..built.blocks_count {
            assert_eq!(m.is_used(b * bs), built.used_blocks.contains(&b), "{opts:?} block {b}");
        }
        assert_eq!(m.used_bytes(), built.used_blocks.len() as u64 * bs, "{opts:?}");
        // block 0 is the boot block for 1 KiB blocks, which no bitmap covers
        assert!(m.is_used(0));
    }
}

#[test]
fn ext_used_map_marks_everything_when_journal_needs_replay() {
    let built = build(&ExtOpts { needs_recovery: true, ..Default::default() });
    let mut fs = open_image(built.image.clone());
    let m = fs.used_map().unwrap();
    assert_eq!(m.used_bytes(), built.blocks_count * built.bs as u64);
}

#[test]
fn ext_deleted_inodes_listed() {
    for opts in [
        ExtOpts::default(),
        ExtOpts { block_size: 4096, ..Default::default() },
        ExtOpts { ext4: false, ..Default::default() },
        ExtOpts { uninit_g1: true, ..Default::default() },
    ] {
        let built = build(&opts);
        let mut fs = open_image(built.image.clone());
        let reports = Mutex::new(Vec::<ProgressReport>::new());
        let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
        let mut del = fs.deleted_entries(&sink, &CancelToken::new()).unwrap();
        del.sort_by_key(|e| e.id);
        let got: Vec<(u32, bool, u64)> = del.iter().map(|e| (e.id as u32, e.is_dir, e.size)).collect();
        assert_eq!(got, built.deleted, "{opts:?}");
        for e in &del {
            assert!(e.deleted);
            assert_eq!(e.name, format!("inode-{}", e.id));
            assert_eq!(e.path, format!("/inode-{}", e.id));
        }
        assert_eq!(del[0].modified, Some(1_700_000_900));
        assert!(!reports.lock().unwrap().is_empty());

        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(fs.deleted_entries(&NullProgress, &cancel), Err(Error::Cancelled)));
    }
}

#[test]
fn ext_open_rejects_unsupported_features() {
    let built = build(&ExtOpts::default());
    let sb = 1024;
    // BIGALLOC (ro_compat 0x200): bitmaps would be per cluster, not per block
    let mut img = built.image.clone();
    let v = u32::from_le_bytes(img[sb + 100..sb + 104].try_into().unwrap());
    put32(&mut img, sb + 100, v | 0x200);
    assert!(matches!(Ext::open(Box::new(MemDevice::from_vec(img))), Err(Error::Unsupported(_))));
    // META_BG moves the descriptors
    let mut img = built.image.clone();
    let v = u32::from_le_bytes(img[sb + 96..sb + 100].try_into().unwrap());
    put32(&mut img, sb + 96, v | 0x10);
    assert!(matches!(Ext::open(Box::new(MemDevice::from_vec(img))), Err(Error::Unsupported(_))));
}

#[test]
fn ext_inline_data_and_encrypted_files_are_unsupported() {
    let built = build(&ExtOpts::default());
    let mut img = built.image.clone();
    let o = inode_off(&built, 12);
    put32(&mut img, o + 0x20, 0x1000_0000); // INLINE_DATA
    let mut fs = open_image(img);
    let root = fs.root();
    let list = fs.list(&root).unwrap();
    let mut buf = [0u8; 16];
    assert!(matches!(fs.read_file(find(&list, "hello.txt"), 0, &mut buf), Err(Error::Unsupported(_))));

    let mut img = built.image.clone();
    put32(&mut img, o + 0x20, 0x800 | 0x80000); // ENCRYPT
    let mut fs = open_image(img);
    let root = fs.root();
    let list = fs.list(&root).unwrap();
    assert!(matches!(fs.read_file(find(&list, "hello.txt"), 0, &mut buf), Err(Error::Unsupported(_))));
}

fn exercise(image: Vec<u8>) {
    let Ok(mut fs) = Ext::open(Box::new(MemDevice::from_vec(image))) else { return };
    let _ = fs.label();
    let _ = fs.total();
    let _ = fs.used_map();
    let _ = fs.deleted_entries(&NullProgress, &CancelToken::new());
    let mut stack = vec![fs.root()];
    let mut seen = 0;
    let mut buf = vec![0u8; 8192];
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
                let _ = fs.read_file(&e, 3 * 1024 + 7, &mut buf);
            }
        }
    }
}

#[test]
fn ext_corrupt_never_panics() {
    let built = build(&ExtOpts::default());
    let sb = 1024usize;
    let g = |img: &[u8], o: usize| u32::from_le_bytes(img[o..o + 4].try_into().unwrap());

    // superblock fields that must be rejected at open
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut mutate = |name: &'static str, f: &dyn Fn(&mut Vec<u8>)| {
        let mut img = built.image.clone();
        f(&mut img);
        cases.push((name, img));
    };
    mutate("bad magic", &|i| put16(i, sb + 56, 0));
    mutate("block size shift 30", &|i| put32(i, sb + 24, 30));
    mutate("block size shift 7", &|i| put32(i, sb + 24, 7));
    mutate("blocks per group 0", &|i| put32(i, sb + 32, 0));
    mutate("blocks per group huge", &|i| put32(i, sb + 32, u32::MAX));
    mutate("inodes per group 0", &|i| put32(i, sb + 40, 0));
    mutate("inodes per group huge", &|i| put32(i, sb + 40, u32::MAX));
    mutate("inode size 0", &|i| put16(i, sb + 88, 0));
    mutate("inode size 100", &|i| put16(i, sb + 88, 100));
    mutate("inode size 3000", &|i| put16(i, sb + 88, 3000));
    mutate("tiny groups: inode tables exceed the filesystem", &|i| put32(i, sb + 32, 8));
    mutate("blocks count 0", &|i| put32(i, sb + 4, 0));
    mutate("group count overflow", &|i| put32(i, sb + 4, u32::MAX));
    mutate("first data block beyond end", &|i| put32(i, sb + 20, u32::MAX));
    mutate("blocks beyond device", &|i| put32(i, sb + 4, 1 << 20));
    for (name, img) in cases {
        match Ext::open(Box::new(MemDevice::from_vec(img))) {
            Err(Error::Parse(_)) | Err(Error::Unsupported(_)) => {}
            other => panic!("{name}: expected Parse/Unsupported, got {:?}", other.map(|_| ())),
        }
    }
    // 64-bit descriptors with a nonsense size
    for ds in [0u16, 1, 63, 65, 4096, u16::MAX] {
        let b64 = build(&ExtOpts { desc64: true, ..Default::default() });
        let mut img = b64.image.clone();
        put16(&mut img, sb + 254, ds);
        exercise(img);
    }

    // 64-bit block count that overflows the byte size
    {
        let b64 = build(&ExtOpts { desc64: true, ..Default::default() });
        let mut img = b64.image.clone();
        put32(&mut img, sb + 0x150, u32::MAX);
        assert!(matches!(Ext::open(Box::new(MemDevice::from_vec(img))), Err(Error::Parse(_))));
    }

    // extent tree damage: depth 100, a self-referencing index block, bogus entry counts
    let ino13 = inode_off(&built, 13);
    let ib = ino13 + 0x28;
    let mut img = built.image.clone();
    put16(&mut img, ib + 6, 100);
    {
        let mut fs = open_image(img.clone());
        let root = fs.root();
        let list = fs.list(&root).unwrap();
        let mut buf = [0u8; 64];
        let r = fs.read_file(find(&list, "extent.bin"), 0, &mut buf);
        assert!(matches!(r, Err(Error::Parse(_))), "depth 100: {r:?}");
    }
    exercise(img);

    let leaf = g(&built.image, ib + 16) as usize;
    let mut img = built.image.clone();
    {
        // the leaf block becomes an index node (depth 1) pointing at itself
        let o = leaf * built.bs;
        put16(&mut img, o, 0xF30A);
        put16(&mut img, o + 2, 1);
        put16(&mut img, o + 4, 4);
        put16(&mut img, o + 6, 1);
        put32(&mut img, o + 12, 0);
        put32(&mut img, o + 16, leaf as u32);
        put16(&mut img, o + 20, 0);
    }
    {
        let mut fs = open_image(img.clone());
        let root = fs.root();
        let list = fs.list(&root).unwrap();
        let mut buf = [0u8; 64];
        let r = fs.read_file(find(&list, "extent.bin"), 0, &mut buf);
        assert!(matches!(r, Err(Error::Parse(_))), "self-referencing index: {r:?}");
    }
    exercise(img);

    // the root index entry itself points at itself through i_block depth 5 -> ... chain
    let mut img = built.image.clone();
    put16(&mut img, ib + 6, 5);
    exercise(img);
    let mut img = built.image.clone();
    put16(&mut img, ib + 2, 60000); // entries > max
    exercise(img);
    let mut img = built.image.clone();
    put16(&mut img, ib, 0); // bad magic
    {
        let mut fs = open_image(img.clone());
        let root = fs.root();
        let list = fs.list(&root).unwrap();
        let mut buf = [0u8; 64];
        let r = fs.read_file(find(&list, "extent.bin"), 0, &mut buf);
        assert!(matches!(r, Err(Error::Parse(_))), "bad extent magic: {r:?}");
    }
    exercise(img);

    // directory entries: rec_len 0, too small, unaligned, beyond the block, name past rec_len
    let ino2 = inode_off(&built, 2);
    let root_blk = g(&built.image, ino2 + 0x28 + 12 + 8) as usize;
    let dir_off = root_blk * built.bs;
    for (name, rec) in [("zero", 0u16), ("small", 4), ("unaligned", 13), ("past block", 4000)] {
        let mut img = built.image.clone();
        put16(&mut img, dir_off + 4, rec);
        let mut fs = open_image(img.clone());
        let root = fs.root();
        let r = fs.list(&root);
        assert!(matches!(r, Err(Error::Parse(_))), "rec_len {name}: {r:?}");
        exercise(img);
    }
    let mut img = built.image.clone();
    img[dir_off + 6] = 250; // name_len beyond rec_len
    {
        let mut fs = open_image(img.clone());
        let root = fs.root();
        assert!(matches!(fs.list(&root), Err(Error::Parse(_))));
    }
    exercise(img);

    // group descriptors pointing outside the device / at nonsense
    let gd = built.gdt_block as usize * built.bs;
    for field in [0usize, 4, 8] {
        for val in [0u32, 1, 0xFFFF_FFF0, 16384] {
            let mut img = built.image.clone();
            put32(&mut img, gd + field, val);
            exercise(img);
            let mut img = built.image.clone();
            put32(&mut img, gd + built.desc_size + field, val);
            exercise(img);
        }
    }

    // inode size field / sizes in the inode
    let mut img = built.image.clone();
    put32(&mut img, inode_off(&built, 12) + 4, u32::MAX);
    put32(&mut img, inode_off(&built, 12) + 0x6C, u32::MAX);
    exercise(img);
    let mut img = built.image.clone();
    put32(&mut img, ino2 + 4, u32::MAX); // a directory of 4 GiB on a 16 MiB device
    {
        let mut fs = open_image(img.clone());
        let root = fs.root();
        assert!(matches!(fs.list(&root), Err(Error::Parse(_))));
    }
    exercise(img);

    // tiny devices
    for len in [0usize, 1024, 2048, 3000, 8192] {
        let mut img = built.image.clone();
        img.truncate(len);
        exercise(img);
    }

    // seeded random corruption of every metadata area (both a 1 KiB/ext4 and a 4 KiB/ext2 image)
    for opts in [ExtOpts::default(), ExtOpts { block_size: 4096, ext4: false, ..Default::default() }] {
        let b = build(&opts);
        let bs = b.bs;
        let leaf = g(&b.image, inode_off(&b, 13) + 0x28 + 16) as usize;
        let mut regions: Vec<(usize, usize)> = vec![
            (1024, 1024 + 0x160),
            (b.gdt_block as usize * bs, b.gdt_block as usize * bs + 2 * b.desc_size),
            (inode_off(&b, 1), inode_off(&b, 23)),
        ];
        if opts.ext4 {
            regions.push((leaf * bs, leaf * bs + 64));
        }
        let ino2 = inode_off(&b, 2);
        let rb = if opts.ext4 { g(&b.image, ino2 + 0x28 + 12 + 8) } else { g(&b.image, ino2 + 0x28) } as usize;
        regions.push((rb * bs, rb * bs + 160));
        let mut seed = 0x1234_5678_9ABC_DEF1u64 ^ bs as u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let mut img = b.image.clone();
            let flips = 1 + rnd() % 6;
            for _ in 0..flips {
                let (lo, hi) = regions[(rnd() % regions.len() as u64) as usize];
                let at = lo + (rnd() % (hi - lo) as u64) as usize;
                img[at] = rnd() as u8;
            }
            exercise(img);
        }
    }
}

/// A read-only device of `len` zero bytes with `sb` at byte 1024: a "huge" ext superblock
/// without a huge backing file (a 600 GiB file is not sparse on every CI runner's disk).
struct HugeZero {
    len: u64,
    sb: Vec<u8>,
}

impl pm_core::BlockDevice for HugeZero {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> pm_core::Result<()> {
        let end = offset.checked_add(buf.len() as u64).filter(|e| *e <= self.len);
        if end.is_none() {
            let source = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "read past the end");
            return Err(Error::Io { offset, len: buf.len(), source });
        }
        for (i, b) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *b = if (1024..1024 + self.sb.len() as u64).contains(&at) { self.sb[(at - 1024) as usize] } else { 0 };
        }
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> pm_core::Result<()> {
        let source = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "read-only");
        Err(Error::Io { offset, len: buf.len(), source })
    }
    fn flush(&mut self) -> pm_core::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        true
    }
}

#[test]
fn ext_huge_geometry_is_a_parse_error_not_an_overflow() {
    // a ~600 GiB device; bpg = 1 makes a group per block, ipg and inode size maximal
    let len: u64 = 600 << 30;
    let mut sb = vec![0u8; 1024];
    put16(&mut sb, 56, 0xEF53);
    put32(&mut sb, 24, 6); // 64 KiB blocks
    put32(&mut sb, 4, (len / 65536) as u32);
    put32(&mut sb, 20, 0);
    put32(&mut sb, 32, 1);
    put32(&mut sb, 40, 8 * 65536);
    put32(&mut sb, 76, 1);
    put16(&mut sb, 88, 32768);
    let dev = HugeZero { len, sb: sb.clone() };
    assert!(matches!(Ext::open(Box::new(dev)), Err(Error::Parse(_))));
    // the same through the crate's dispatching entry point
    let dev = HugeZero { len, sb };
    assert!(matches!(pm_fs::open(Box::new(dev)).err(), Some(Error::Parse(_))));
}
