//! Test-only synthetic exFAT image builder. It writes the boot sector, FAT, allocation bitmap
//! and directory entry sets byte by byte, so the reader under test is never used to construct
//! the images it is tested against.
//!
//! Default image: 16 MiB, 512-byte sectors, 4 KiB clusters, one FAT, cluster heap at sector 256.
//! - cluster 2 allocation bitmap, 3 up-case table, 4 root directory
//! - `hello.txt`  clusters 5-6, contiguous (NoFatChain), 6000 bytes, modified 2024-03-15 12:34:56 UTC
//! - `frag.bin`   FAT chain 8 -> 10 -> 12, 3 * 4096 - 100 bytes
//! - `docs/`      cluster 14, FAT-chained; holds `note.txt` (cluster 15, contiguous) and a deleted `old.txt` (16)
//! - `contig/`    cluster 20, contiguous (NoFatChain) directory; holds `c.txt`
//! - `contig/c.txt` spans clusters 21-22
//! - a file with a 34-character name (cluster 17), and a deleted `gone.txt` (cluster 18)
//!
//! Clusters 7, 9, 11, 13, 16, 18 and everything from 23 up are free.
#![allow(dead_code)]

pub const BPS: usize = 512;
pub const SPC: usize = 8;
pub const CS: usize = BPS * SPC;
pub const VOL_SECTORS: u64 = 32768;
pub const FAT_OFFSET: u32 = 128;
pub const FAT_LENGTH: u32 = 32;

pub const HELLO_MODIFIED: i64 = 1_710_506_096; // 2024-03-15 12:34:56 UTC
pub const LONG_NAME: &str = "this_is_a_rather_long_file_name.txt";

pub fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
pub fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Deterministic, position-dependent test data.
pub fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed).wrapping_add((i >> 8) as u8)).collect()
}

pub fn exfat_ts(y: u32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> u32 {
    ((y - 1980) << 25) | (mo << 21) | (d << 16) | (h << 11) | (mi << 5) | (s / 2)
}

/// One directory entry set: primary 0x85, stream 0xC0 and the 0xC1 name entries. `in_use ==
/// false` clears bit 7 of every entry type, as a deleting driver does.
pub fn file_set(name: &str, attrs: u16, first: u32, len: u64, nofat: bool, modified: u32, in_use: bool) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let name_entries = units.len().div_ceil(15);
    let mut set = vec![0u8; 32 * (2 + name_entries)];
    set[0] = 0x85;
    set[1] = (1 + name_entries) as u8;
    put16(&mut set, 4, attrs);
    put32(&mut set, 8, modified);
    put32(&mut set, 12, modified);
    put32(&mut set, 16, modified);
    set[23] = 0x80;
    set[24] = 0x80; // UTC offset valid, 0 minutes
    set[25] = 0x80;
    set[32] = 0xC0;
    set[33] = 0x01 | if nofat { 0x02 } else { 0 };
    set[35] = units.len() as u8;
    put64(&mut set, 32 + 8, len);
    put32(&mut set, 32 + 20, first);
    put64(&mut set, 32 + 24, len);
    for (k, chunk) in units.chunks(15).enumerate() {
        let o = 64 + 32 * k;
        set[o] = 0xC1;
        for (j, &u) in chunk.iter().enumerate() {
            put16(&mut set, o + 2 + 2 * j, u);
        }
    }
    // SetChecksum over the whole set, skipping bytes 2 and 3 of the primary entry
    let mut ck = 0u16;
    for (i, &b) in set.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        ck = ck.rotate_right(1).wrapping_add(b as u16);
    }
    put16(&mut set, 2, ck);
    if !in_use {
        for e in 0..2 + name_entries {
            set[32 * e] &= 0x7F;
        }
    }
    set
}

pub struct ExOpts {
    /// Cluster heap start in sectors (a non-multiple of 8 puts the heap off the cluster grid).
    pub heap_sectors: u32,
}

impl Default for ExOpts {
    fn default() -> Self {
        ExOpts { heap_sectors: 256 }
    }
}

pub struct Built {
    pub image: Vec<u8>,
    pub heap_start: usize,
    pub cluster_count: u32,
    pub hello: Vec<u8>,
    pub frag: Vec<u8>,
    pub frag_clusters: [u32; 3],
    pub note: Vec<u8>,
    pub long: Vec<u8>,
    pub c_txt: Vec<u8>,
    pub gone: Vec<u8>,
    pub old: Vec<u8>,
    /// Clusters whose allocation bitmap bit is set.
    pub used: Vec<u32>,
}

pub fn build(opts: &ExOpts) -> Built {
    let heap_sectors = opts.heap_sectors;
    let heap_start = heap_sectors as usize * BPS;
    let cluster_count = ((VOL_SECTORS - heap_sectors as u64) / SPC as u64) as u32;
    assert!(cluster_count + 2 <= FAT_LENGTH * BPS as u32 / 4);
    let mut img = vec![0u8; VOL_SECTORS as usize * BPS];

    let coff = |c: u32| heap_start + (c as usize - 2) * CS;
    let set_fat = |img: &mut Vec<u8>, c: u32, v: u32| {
        put32(img, FAT_OFFSET as usize * BPS + c as usize * 4, v);
    };

    // boot sector
    {
        let b = &mut img[..BPS];
        b[0] = 0xEB;
        b[1] = 0x76;
        b[2] = 0x90;
        b[3..11].copy_from_slice(b"EXFAT   ");
        put64(b, 64, 0);
        put64(b, 72, VOL_SECTORS);
        put32(b, 80, FAT_OFFSET);
        put32(b, 84, FAT_LENGTH);
        put32(b, 88, heap_sectors);
        put32(b, 92, cluster_count);
        put32(b, 96, 4);
        put32(b, 100, 0x1234_5678);
        put16(b, 104, 0x0100);
        put16(b, 106, 0);
        b[108] = 9;
        b[109] = 3;
        b[110] = 1;
        b[111] = 0x80;
        b[510] = 0x55;
        b[511] = 0xAA;
    }

    // FAT: media, EOC, bitmap / up-case / root are single-cluster chains
    set_fat(&mut img, 0, 0xFFFF_FFF8);
    set_fat(&mut img, 1, 0xFFFF_FFFF);
    for c in [2, 3, 4] {
        set_fat(&mut img, c, 0xFFFF_FFFF);
    }

    let t_hello = exfat_ts(2024, 3, 15, 12, 34, 56);
    let t_other = exfat_ts(2021, 7, 4, 1, 2, 4);

    // files
    let hello = pattern(1, 6000);
    img[coff(5)..coff(5) + 6000].copy_from_slice(&hello);

    let frag_clusters = [8u32, 10, 12];
    let frag = pattern(2, 3 * CS - 100);
    for (k, &c) in frag_clusters.iter().enumerate() {
        let part = &frag[k * CS..((k + 1) * CS).min(frag.len())];
        img[coff(c)..coff(c) + part.len()].copy_from_slice(part);
    }
    set_fat(&mut img, 8, 10);
    set_fat(&mut img, 10, 12);
    set_fat(&mut img, 12, 0xFFFF_FFFF);

    let note = pattern(3, 100);
    img[coff(15)..coff(15) + 100].copy_from_slice(&note);
    let old = pattern(4, 50);
    img[coff(16)..coff(16) + 50].copy_from_slice(&old);
    let long = pattern(5, 777);
    img[coff(17)..coff(17) + 777].copy_from_slice(&long);
    let gone = pattern(6, 80);
    img[coff(18)..coff(18) + 80].copy_from_slice(&gone);
    let c_txt = pattern(7, 4097); // one byte over a cluster: 2 clusters (21, 22)
    img[coff(21)..coff(21) + 4097].copy_from_slice(&c_txt);

    // docs/ (FAT-chained, cluster 14)
    set_fat(&mut img, 14, 0xFFFF_FFFF);
    {
        let mut d = Vec::new();
        d.extend(file_set("note.txt", 0x20, 15, 100, true, t_other, true));
        d.extend(file_set("old.txt", 0x20, 16, 50, true, t_other, false));
        img[coff(14)..coff(14) + d.len()].copy_from_slice(&d);
    }
    // contig/ (NoFatChain, cluster 20)
    {
        let d = file_set("c.txt", 0x20, 21, 4097, true, t_other, true);
        img[coff(20)..coff(20) + d.len()].copy_from_slice(&d);
    }

    // root directory (cluster 4)
    {
        let mut d = Vec::new();
        let mut label = [0u8; 32];
        label[0] = 0x83;
        label[1] = 8;
        for (i, u) in "EXFATVOL".encode_utf16().enumerate() {
            put16(&mut label, 2 + 2 * i, u);
        }
        d.extend(label);
        let mut bm = [0u8; 32];
        bm[0] = 0x81;
        bm[1] = 0;
        put32(&mut bm, 20, 2);
        put64(&mut bm, 24, cluster_count.div_ceil(8) as u64);
        d.extend(bm);
        let mut up = [0u8; 32];
        up[0] = 0x82;
        put32(&mut up, 20, 3);
        put64(&mut up, 24, 128);
        d.extend(up);
        d.extend(file_set("hello.txt", 0x20, 5, 6000, true, t_hello, true));
        d.extend(file_set("frag.bin", 0x20, 8, frag.len() as u64, false, t_other, true));
        d.extend(file_set("docs", 0x10, 14, CS as u64, false, t_other, true));
        d.extend(file_set("contig", 0x10, 20, CS as u64, true, t_other, true));
        d.extend(file_set(LONG_NAME, 0x20, 17, 777, true, t_other, true));
        d.extend(file_set("gone.txt", 0x20, 18, 80, true, t_other, false));
        assert!(d.len() < CS);
        img[coff(4)..coff(4) + d.len()].copy_from_slice(&d);
    }

    // up-case table (identity for the first 64 code units)
    for i in 0..64u16 {
        put16(&mut img, coff(3) + 2 * i as usize, i);
    }

    // allocation bitmap
    let used: Vec<u32> = vec![2, 3, 4, 5, 6, 8, 10, 12, 14, 15, 17, 20, 21, 22];
    for &c in &used {
        let i = (c - 2) as usize;
        img[coff(2) + i / 8] |= 1 << (i % 8);
    }

    // FAT chain for the 2-cluster contiguous file is deliberately absent (NoFatChain)
    Built { image: img, heap_start, cluster_count, hello, frag, frag_clusters, note, long, c_txt, gone, old, used }
}
