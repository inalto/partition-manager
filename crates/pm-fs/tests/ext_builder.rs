//! Test-only synthetic ext2/ext4 image builder. It writes the superblock, group descriptors,
//! bitmaps, inodes, extent trees, indirect blocks and directory blocks byte by byte, so the
//! reader under test is never used to construct the images it is tested against.
//!
//! 16 MiB, 256 inodes per group, rev 1. 1 KiB blocks give two groups (8192 blocks each, first
//! data block 1); 4 KiB blocks give one group. Per group: superblock (backup), one descriptor
//! block, block bitmap, inode bitmap, inode table, then data.
//!
//! Inodes: 2 root, 11 lost+found, 12 hello.txt, 13 extent.bin (ext4 only), 14 indirect.bin,
//! 15 subdir, 16 subdir/inner.txt, 17 link (fast symlink), 18 htree (two-block "indexed"
//! directory), 19 sparse.bin (double and triple indirect tail blocks), 20 deleted file,
//! 21 deleted directory, 22 orphan-list inode (dtime set, links 1: not deleted), 260 deleted
//! file in group 1 (two-group images).
#![allow(dead_code)]

use std::collections::BTreeSet;

pub const IMAGE_BYTES: usize = 16 << 20;
pub const IPG: u32 = 256;

pub fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
pub fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed).wrapping_add((i >> 8) as u8)).collect()
}

#[derive(Clone, Debug)]
pub struct ExtOpts {
    pub block_size: usize,
    /// Extents, flex-less ext4 feature set; false = plain ext2 (classic block maps only).
    pub ext4: bool,
    /// 64-byte group descriptors with the 64BIT feature.
    pub desc64: bool,
    /// Mark group 1 BLOCK_UNINIT | INODE_UNINIT with garbage bitmaps, keep all data in group 0.
    pub uninit_g1: bool,
    /// Set the "needs recovery" incompat flag.
    pub needs_recovery: bool,
}

impl Default for ExtOpts {
    fn default() -> Self {
        ExtOpts { block_size: 1024, ext4: true, desc64: false, uninit_g1: false, needs_recovery: false }
    }
}

pub struct ExtBuilt {
    pub image: Vec<u8>,
    pub bs: usize,
    pub blocks_count: u64,
    pub groups: u32,
    pub inode_size: usize,
    /// Every block the builder marked in use (including metadata and, for 1 KiB blocks, block 0).
    pub used_blocks: BTreeSet<u64>,
    pub hello: Vec<u8>,
    /// Expected full content of extent.bin (holes and the uninitialized extent read as zeros).
    pub extent_file: Vec<u8>,
    pub indirect: Vec<u8>,
    pub inner: Vec<u8>,
    pub sparse_size: u64,
    /// (offset, bytes) pairs that are mapped in sparse.bin; everything else reads as zeros.
    pub sparse_data: Vec<(u64, Vec<u8>)>,
    /// (inode, is_dir, size) of the inodes `deleted_entries` must list.
    pub deleted: Vec<(u32, bool, u64)>,
    /// First block of each group's inode table.
    pub itable: Vec<u64>,
    pub block_bitmap: Vec<u64>,
    pub gdt_block: u64,
    pub desc_size: usize,
    pub inode_table_blocks: u64,
}

const S_IFDIR: u16 = 0x4000;
const S_IFREG: u16 = 0x8000;
const S_IFLNK: u16 = 0xA000;

#[derive(Clone)]
struct Ino {
    mode: u16,
    size: u64,
    links: u16,
    dtime: u32,
    mtime: u32,
    flags: u32,
    block: [u8; 60],
}

impl Default for Ino {
    fn default() -> Ino {
        Ino { mode: 0, size: 0, links: 0, dtime: 0, mtime: 0, flags: 0, block: [0; 60] }
    }
}

struct B {
    opts: ExtOpts,
    img: Vec<u8>,
    bs: usize,
    isz: usize,
    groups: u32,
    bpg: u64,
    fdb: u64,
    blocks_count: u64,
    itable: Vec<u64>,
    itb: u64,
    next: Vec<u64>,
    used: BTreeSet<u64>,
}

impl B {
    fn group_first(&self, g: u32) -> u64 {
        self.fdb + g as u64 * self.bpg
    }

    fn alloc(&mut self, g: u32) -> u64 {
        let b = self.next[g as usize];
        assert!(b < self.group_first(g) + self.bpg && b < self.blocks_count, "group {g} full");
        self.next[g as usize] += 1;
        self.used.insert(b);
        b
    }

    fn write_block(&mut self, b: u64, data: &[u8]) {
        assert!(data.len() <= self.bs);
        let o = b as usize * self.bs;
        self.img[o..o + data.len()].copy_from_slice(data);
    }

    fn put_inode(&mut self, n: u32, i: &Ino) {
        let g = (n - 1) / IPG;
        let idx = ((n - 1) % IPG) as usize;
        let o = self.itable[g as usize] as usize * self.bs + idx * self.isz;
        let b = &mut self.img[o..o + self.isz];
        put16(b, 0, i.mode);
        put32(b, 4, i.size as u32);
        put32(b, 0x10, i.mtime);
        put32(b, 0x14, i.dtime);
        put16(b, 0x1A, i.links);
        put32(b, 0x20, i.flags);
        b[0x28..0x28 + 60].copy_from_slice(&i.block);
        put32(b, 0x6C, (i.size >> 32) as u32);
        if self.isz > 128 {
            put16(b, 0x80, 32);
        }
    }

    fn dir_block(&self, ents: &[(u32, &str, u8)]) -> Vec<u8> {
        let mut blk = vec![0u8; self.bs];
        let mut pos = 0;
        for (k, (ino, name, ft)) in ents.iter().enumerate() {
            let need = (8 + name.len() + 3) & !3;
            let rec = if k + 1 == ents.len() { self.bs - pos } else { need };
            put32(&mut blk, pos, *ino);
            put16(&mut blk, pos + 4, rec as u16);
            blk[pos + 6] = name.len() as u8;
            blk[pos + 7] = *ft;
            blk[pos + 8..pos + 8 + name.len()].copy_from_slice(name.as_bytes());
            pos += rec;
        }
        blk
    }

    fn leaf_extents(&self, ents: &[(u32, u16, u64)]) -> Vec<u8> {
        let mut v = vec![0u8; 12 + 12 * ents.len()];
        put16(&mut v, 0, 0xF30A);
        put16(&mut v, 2, ents.len() as u16);
        put16(&mut v, 4, ents.len() as u16);
        for (k, &(lb, len, phys)) in ents.iter().enumerate() {
            let o = 12 + 12 * k;
            put32(&mut v, o, lb);
            put16(&mut v, o + 4, len);
            put16(&mut v, o + 6, (phys >> 32) as u16);
            put32(&mut v, o + 8, phys as u32);
        }
        v
    }

    /// An i_block holding a depth-0 extent tree.
    fn extent_iblock(&self, ents: &[(u32, u16, u64)]) -> [u8; 60] {
        let mut ib = [0u8; 60];
        let v = self.leaf_extents(ents);
        ib[..v.len()].copy_from_slice(&v);
        put16(&mut ib, 4, 4);
        ib
    }

    /// Write `data` as a single contiguous file in group `g`, returning its i_block (extents or
    /// direct pointers, per the filesystem flavour) and flags.
    fn simple_file(&mut self, g: u32, data: &[u8]) -> ([u8; 60], u32) {
        let n = data.len().div_ceil(self.bs).max(1);
        let first = self.alloc(g);
        for k in 1..n {
            let b = self.alloc(g);
            assert_eq!(b, first + k as u64);
        }
        for k in 0..n {
            let part = &data[(k * self.bs).min(data.len())..((k + 1) * self.bs).min(data.len())];
            self.write_block(first + k as u64, part);
        }
        if self.opts.ext4 {
            (self.extent_iblock(&[(0, n as u16, first)]), 0x80000)
        } else {
            assert!(n <= 12);
            let mut ib = [0u8; 60];
            for k in 0..n {
                put32(&mut ib, 4 * k, (first + k as u64) as u32);
            }
            (ib, 0)
        }
    }

    fn ptr_block(&mut self, g: u32, ptrs: &[(usize, u64)]) -> u64 {
        let b = self.alloc(g);
        let mut blk = vec![0u8; self.bs];
        for &(i, p) in ptrs {
            put32(&mut blk, 4 * i, p as u32);
        }
        self.write_block(b, &blk);
        b
    }
}

pub fn build(opts: &ExtOpts) -> ExtBuilt {
    let bs = opts.block_size;
    let isz = if opts.ext4 { 256 } else { 128 };
    let desc_size = if opts.desc64 { 64 } else { 32 };
    let blocks_count = (IMAGE_BYTES / bs) as u64;
    let bpg = 8 * bs as u64;
    let fdb = if bs == 1024 { 1 } else { 0 };
    let groups = (blocks_count - fdb).div_ceil(bpg) as u32;
    let itb = (IPG as u64 * isz as u64).div_ceil(bs as u64);
    let mut b = B {
        opts: opts.clone(),
        img: vec![0u8; IMAGE_BYTES],
        bs,
        isz,
        groups,
        bpg,
        fdb,
        blocks_count,
        itable: vec![],
        itb,
        next: vec![],
        used: BTreeSet::new(),
    };
    let p = (bs / 4) as u64;

    // group metadata layout
    let gdt_block = fdb + 1;
    let mut bb_blocks = Vec::new();
    let mut ib_blocks = Vec::new();
    for g in 0..groups {
        let first = b.group_first(g);
        // superblock (+ backup in group 1) and the descriptor block
        b.used.insert(first);
        b.used.insert(first + 1);
        let bb = first + 2;
        let ib = first + 3;
        let it = first + 4;
        b.used.insert(bb);
        b.used.insert(ib);
        for k in 0..itb {
            b.used.insert(it + k);
        }
        bb_blocks.push(bb);
        ib_blocks.push(ib);
        b.itable.push(it);
        b.next.push(it + itb);
    }
    if fdb == 1 {
        b.used.insert(0); // boot block, outside every bitmap
    }
    let g_data = |g: u32| if opts.uninit_g1 { 0 } else { g.min(groups - 1) };

    // ---- files ----
    let hello = b"Hello, ext!\n".to_vec();
    let (ib, fl) = b.simple_file(0, &hello);
    b.put_inode(12, &Ino { mode: S_IFREG | 0o644, size: hello.len() as u64, links: 1, mtime: 1_700_000_000, flags: fl, block: ib, ..Default::default() });

    // extent.bin: depth-1 tree, an index node -> leaf with two extents and an uninitialized one
    let mut extent_file = Vec::new();
    if opts.ext4 {
        let ga = 0;
        let gb = g_data(1);
        let a = b.alloc(ga);
        b.alloc(ga);
        b.alloc(ga);
        let blk_b = b.alloc(gb);
        b.alloc(gb);
        let blk_c = b.alloc(gb);
        let leaf_blk = b.alloc(gb);
        let da = pattern(0x21, 3 * bs);
        for k in 0..3 {
            b.write_block(a + k as u64, &da[k * bs..(k + 1) * bs]);
        }
        let db = pattern(0x31, 2 * bs);
        b.write_block(blk_b, &db[..bs]);
        b.write_block(blk_b + 1, &db[bs..]);
        b.write_block(blk_c, &pattern(0x41, bs)); // uninitialized extent: must read as zeros
        // leaf: lblock 0..3 -> a, lblock 5..7 -> blk_b, lblock 8 (uninit, len 32768 + 1) -> blk_c
        let leaf = b.leaf_extents(&[(0, 3, a), (5, 2, blk_b), (8, 32768 + 1, blk_c)]);
        let mut leaf_full = leaf.clone();
        put16(&mut leaf_full, 4, ((bs - 12) / 12) as u16);
        b.write_block(leaf_blk, &leaf_full);
        // root: header (depth 1, 1 entry) + index entry
        let mut ibk = [0u8; 60];
        put16(&mut ibk, 0, 0xF30A);
        put16(&mut ibk, 2, 1);
        put16(&mut ibk, 4, 4);
        put16(&mut ibk, 6, 1);
        put32(&mut ibk, 12, 0);
        put32(&mut ibk, 16, leaf_blk as u32);
        put16(&mut ibk, 20, (leaf_blk >> 32) as u16);
        let size = 9 * bs as u64 - 10;
        b.put_inode(13, &Ino { mode: S_IFREG | 0o644, size, links: 1, mtime: 1_700_000_100, flags: 0x80000, block: ibk, ..Default::default() });
        extent_file = vec![0u8; size as usize];
        extent_file[..3 * bs].copy_from_slice(&da);
        extent_file[5 * bs..7 * bs].copy_from_slice(&db);
    }

    // indirect.bin: 12 direct blocks + 3 through the single indirect block (classic block map)
    let indirect = pattern(0x51, 15 * bs - 33);
    {
        let g = g_data(1);
        let mut ib = [0u8; 60];
        let mut data_blocks = Vec::new();
        for k in 0..15 {
            let blk = b.alloc(if k < 12 { 0 } else { g });
            data_blocks.push(blk);
            let part = &indirect[(k * bs).min(indirect.len())..((k + 1) * bs).min(indirect.len())];
            b.write_block(blk, part);
        }
        for (k, &blk) in data_blocks.iter().take(12).enumerate() {
            put32(&mut ib, 4 * k, blk as u32);
        }
        let ind = b.ptr_block(g, &[(0, data_blocks[12]), (1, data_blocks[13]), (2, data_blocks[14])]);
        put32(&mut ib, 48, ind as u32);
        b.put_inode(14, &Ino { mode: S_IFREG | 0o644, size: indirect.len() as u64, links: 1, mtime: 1_700_000_200, flags: 0, block: ib, ..Default::default() });
    }

    // sparse.bin: lblock 0, 12+p (double indirect) and 12+p+p*p (triple indirect)
    let sparse_size = (12 + p + p * p + 1) * bs as u64 - 7;
    let mut sparse_data = Vec::new();
    {
        let mut ib = [0u8; 60];
        let d0 = pattern(0x61, bs);
        let blk0 = b.alloc(0);
        b.write_block(blk0, &d0);
        put32(&mut ib, 0, blk0 as u32);
        sparse_data.push((0u64, d0));

        let d1 = pattern(0x62, bs);
        let blk1 = b.alloc(0);
        b.write_block(blk1, &d1);
        let ind1 = b.ptr_block(0, &[(0, blk1)]);
        let dind = b.ptr_block(0, &[(0, ind1)]);
        put32(&mut ib, 52, dind as u32);
        sparse_data.push(((12 + p) * bs as u64, d1));

        let tail_len = bs - 7;
        let d2 = pattern(0x63, bs);
        let blk2 = b.alloc(0);
        b.write_block(blk2, &d2);
        let ind2 = b.ptr_block(0, &[(0, blk2)]);
        let dind2 = b.ptr_block(0, &[(0, ind2)]);
        let tind = b.ptr_block(0, &[(0, dind2)]);
        put32(&mut ib, 56, tind as u32);
        sparse_data.push(((12 + p + p * p) * bs as u64, d2[..tail_len].to_vec()));
        b.put_inode(19, &Ino { mode: S_IFREG | 0o600, size: sparse_size, links: 1, mtime: 1_700_000_300, flags: 0, block: ib, ..Default::default() });
    }

    // directories
    let mkdir = |b: &mut B, ino: u32, g: u32, ents: &[(u32, &str, u8)], extra_flags: u32| {
        let blk = b.dir_block(ents);
        let first = b.alloc(g);
        b.write_block(first, &blk);
        let (ib, fl) = if b.opts.ext4 {
            (b.extent_iblock(&[(0, 1, first)]), 0x80000)
        } else {
            let mut ib = [0u8; 60];
            put32(&mut ib, 0, first as u32);
            (ib, 0)
        };
        b.put_inode(ino, &Ino { mode: S_IFDIR | 0o755, size: b.bs as u64, links: 2, mtime: 1_700_000_400, flags: fl | extra_flags, block: ib, ..Default::default() });
    };
    mkdir(&mut b, 11, 0, &[(11, ".", 2), (2, "..", 2)], 0);
    mkdir(&mut b, 15, 0, &[(15, ".", 2), (2, "..", 2), (16, "inner.txt", 1)], 0);

    let inner = pattern(0x71, 3000);
    let (ib, fl) = b.simple_file(0, &inner);
    b.put_inode(16, &Ino { mode: S_IFREG | 0o644, size: inner.len() as u64, links: 1, mtime: 1_700_000_500, flags: fl, block: ib, ..Default::default() });

    // fast symlink
    {
        let mut ib = [0u8; 60];
        ib[..9].copy_from_slice(b"hello.txt");
        b.put_inode(17, &Ino { mode: S_IFLNK | 0o777, size: 9, links: 1, mtime: 1_700_000_600, flags: 0, block: ib, ..Default::default() });
    }

    // htree-style directory: block 0 is a dx_root (only "." and ".." with a fake covering rest),
    // block 1 holds the real entries
    {
        let mut root_blk = vec![0u8; bs];
        put32(&mut root_blk, 0, 18);
        put16(&mut root_blk, 4, 12);
        root_blk[6] = 1;
        root_blk[7] = 2;
        root_blk[8] = b'.';
        put32(&mut root_blk, 12, 2);
        put16(&mut root_blk, 16, (bs - 12) as u16);
        root_blk[18] = 2;
        root_blk[19] = 2;
        root_blk[20] = b'.';
        root_blk[21] = b'.';
        // dx_root_info + a count/limit header where the entries would be
        put16(&mut root_blk, 32, 8);
        let leaf = b.dir_block(&[(12, "a.txt", 1), (16, "b.txt", 1)]);
        let b0 = b.alloc(0);
        let b1 = b.alloc(0);
        b.write_block(b0, &root_blk);
        b.write_block(b1, &leaf);
        let ib = if opts.ext4 {
            b.extent_iblock(&[(0, 2, b0)])
        } else {
            let mut ib = [0u8; 60];
            put32(&mut ib, 0, b0 as u32);
            put32(&mut ib, 4, b1 as u32);
            ib
        };
        let fl = if opts.ext4 { 0x80000 } else { 0 } | 0x1000;
        b.put_inode(18, &Ino { mode: S_IFDIR | 0o755, size: 2 * bs as u64, links: 2, mtime: 1_700_000_700, flags: fl, block: ib, ..Default::default() });
    }

    // root directory
    let mut root_ents: Vec<(u32, &str, u8)> = vec![
        (2, ".", 2),
        (2, "..", 2),
        (11, "lost+found", 2),
        (12, "hello.txt", 1),
    ];
    if opts.ext4 {
        root_ents.push((13, "extent.bin", 1));
    }
    root_ents.extend([(14, "indirect.bin", 1), (15, "subdir", 2), (17, "link", 7), (18, "htree", 2), (19, "sparse.bin", 1)]);
    mkdir(&mut b, 2, 0, &root_ents, 0);

    // deleted inodes (names are gone; block pointers are zeroed by the kernel on delete)
    let mut deleted = Vec::new();
    b.put_inode(20, &Ino { mode: S_IFREG | 0o644, size: 1234, links: 0, dtime: 1_700_001_000, mtime: 1_700_000_900, ..Default::default() });
    deleted.push((20, false, 1234));
    b.put_inode(21, &Ino { mode: S_IFDIR | 0o755, size: 0, links: 0, dtime: 1_700_001_100, mtime: 1_700_000_950, ..Default::default() });
    deleted.push((21, true, 0));
    b.put_inode(22, &Ino { mode: S_IFREG | 0o644, size: 10, links: 1, dtime: 5, ..Default::default() });
    if groups > 1 {
        let ino = IPG + 4;
        b.put_inode(ino, &Ino { mode: S_IFREG | 0o600, size: 77, links: 0, dtime: 1_700_001_200, mtime: 1_700_000_999, ..Default::default() });
        if !opts.uninit_g1 {
            deleted.push((ino, false, 77));
        }
    }

    // ---- bitmaps ----
    for g in 0..groups {
        let first = b.group_first(g);
        let count = bpg.min(blocks_count - first) as usize;
        let uninit = opts.uninit_g1 && g == 1;
        let mut bm = vec![0u8; bs];
        if uninit {
            bm.fill(0xFF); // garbage: must not be consulted
        } else {
            for i in 0..count {
                if b.used.contains(&(first + i as u64)) {
                    bm[i / 8] |= 1 << (i % 8);
                }
            }
            for i in count..bs * 8 {
                bm[i / 8] |= 1 << (i % 8); // padding past the end of the last group
            }
        }
        let o = bb_blocks[g as usize] as usize * bs;
        b.img[o..o + bs].copy_from_slice(&bm);
        let mut im = vec![0u8; bs];
        if g == 0 {
            for n in 1..=22usize {
                im[(n - 1) / 8] |= 1 << ((n - 1) % 8);
            }
            im[(21 - 1) / 8] &= !(1 << ((21 - 1) % 8)); // 21 deleted
            im[(20 - 1) / 8] &= !(1 << ((20 - 1) % 8)); // 20 deleted
        }
        let o = ib_blocks[g as usize] as usize * bs;
        b.img[o..o + bs].copy_from_slice(&im);
    }

    // ---- group descriptors ----
    for g in 0..groups {
        let o = gdt_block as usize * bs + g as usize * desc_size;
        let d = &mut b.img[o..o + desc_size];
        put32(d, 0, bb_blocks[g as usize] as u32);
        put32(d, 4, ib_blocks[g as usize] as u32);
        put32(d, 8, b.itable[g as usize] as u32);
        let flags: u16 = if opts.uninit_g1 && g == 1 { 0x1 | 0x2 } else { 0 };
        put16(d, 0x12, flags);
        if desc_size >= 64 {
            put32(d, 0x20, 0);
            put32(d, 0x24, 0);
            put32(d, 0x28, 0);
        }
    }

    // the garbage-inode-table marker for the INODE_UNINIT group: keep a scary deleted inode there
    // (already written above as inode 260 and left unlisted)

    // ---- superblock ----
    {
        let sb = &mut b.img[1024..2048];
        put32(sb, 0, groups * IPG);
        put32(sb, 4, blocks_count as u32);
        put32(sb, 20, fdb as u32);
        put32(sb, 24, (bs / 1024).trailing_zeros());
        put32(sb, 28, (bs / 1024).trailing_zeros());
        put32(sb, 32, bpg as u32);
        put32(sb, 36, bpg as u32);
        put32(sb, 40, IPG);
        put16(sb, 56, 0xEF53);
        put16(sb, 58, 1);
        put32(sb, 76, 1);
        put32(sb, 84, 11);
        put16(sb, 88, isz as u16);
        let mut incompat = 0x2u32;
        let mut compat = 0u32;
        let mut ro = 0x1u32 | 0x2;
        if opts.ext4 {
            incompat |= 0x40;
            compat |= 0x20 | 0x8;
            ro |= 0x8;
        }
        if opts.uninit_g1 {
            ro |= 0x10; // GDT_CSUM: makes the group flags meaningful
        }
        if opts.desc64 {
            incompat |= 0x80;
        }
        if opts.needs_recovery {
            incompat |= 0x4;
        }
        put32(sb, 92, compat);
        put32(sb, 96, incompat);
        put32(sb, 100, ro);
        sb[104..120].copy_from_slice(&[0xAB; 16]);
        sb[120..120 + 7].copy_from_slice(b"EXTVOL1");
        if opts.desc64 {
            put16(sb, 254, 64);
            put32(sb, 0x150, (blocks_count >> 32) as u32);
        }
    }

    ExtBuilt {
        image: b.img,
        bs,
        blocks_count,
        groups,
        inode_size: isz,
        used_blocks: b.used,
        hello,
        extent_file,
        indirect,
        inner,
        sparse_size,
        sparse_data,
        deleted,
        itable: b.itable,
        block_bitmap: bb_blocks,
        gdt_block,
        desc_size,
        inode_table_blocks: itb,
    }
}
