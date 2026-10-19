//! Test-only image builder. It writes directory entries and FAT chains directly so the
//! reader under test is never used to construct the images it is tested against.
#![allow(dead_code)]

use pm_core::{BlockDevice, MemDevice};

pub const MIB: u64 = 1 << 20;

pub fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
pub fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

pub fn dos_name(base: &str, ext: &str) -> [u8; 11] {
    let mut n = [b' '; 11];
    n[..base.len()].copy_from_slice(base.as_bytes());
    n[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    n
}

pub fn dos_date(y: u16, m: u16, d: u16) -> u16 {
    ((y - 1980) << 9) | (m << 5) | d
}
pub fn dos_time(h: u16, m: u16, s: u16) -> u16 {
    (h << 11) | (m << 5) | (s / 2)
}

pub fn lfn_checksum(name11: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &b in name11 {
        sum = (sum >> 1).wrapping_add((sum & 1) << 7).wrapping_add(b);
    }
    sum
}

pub struct Short {
    pub name: [u8; 11],
    pub attr: u8,
    pub nt: u8,
    pub cluster: u32,
    pub size: u32,
    pub date: u16,
    pub time: u16,
}

impl Short {
    pub fn file(name: [u8; 11], cluster: u32, size: u32) -> Short {
        Short { name, attr: 0x20, nt: 0, cluster, size, date: 0, time: 0 }
    }
    pub fn dir(name: [u8; 11], cluster: u32) -> Short {
        Short { name, attr: 0x10, nt: 0, cluster, size: 0, date: 0, time: 0 }
    }
    pub fn bytes(&self) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(&self.name);
        e[11] = self.attr;
        e[12] = self.nt;
        put16(&mut e, 20, (self.cluster >> 16) as u16);
        put16(&mut e, 22, self.time);
        put16(&mut e, 24, self.date);
        put16(&mut e, 26, self.cluster as u16);
        put32(&mut e, 28, self.size);
        e
    }
}

/// LFN entries for `long` in on-disk order (highest sequence number first).
pub fn lfn_entries(long: &str, short11: &[u8; 11]) -> Vec<[u8; 32]> {
    let mut units: Vec<u16> = long.encode_utf16().collect();
    if units.len() % 13 != 0 {
        units.push(0);
        while units.len() % 13 != 0 {
            units.push(0xFFFF);
        }
    }
    let n = units.len() / 13;
    let ck = lfn_checksum(short11);
    let mut out = Vec::new();
    for i in (1..=n).rev() {
        let mut e = [0u8; 32];
        e[0] = i as u8 | if i == n { 0x40 } else { 0 };
        e[11] = 0x0F;
        e[13] = ck;
        let chunk = &units[(i - 1) * 13..i * 13];
        let slots: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (k, &u) in chunk.iter().enumerate() {
            put16(&mut e, slots[k], u);
        }
        out.push(e);
    }
    out
}

/// Where a directory's entries live.
#[derive(Clone, Copy)]
pub enum DirLoc {
    /// Fixed FAT12/16 root: (byte offset, byte length).
    Fixed(u64, u64),
    /// A single-cluster directory.
    Cluster(u32),
}

/// A FAT image under construction. All offsets are bytes from the start of the device.
pub struct Img {
    pub dev: MemDevice,
    pub bits: u8,
    pub bps: u64,
    pub spc: u64,
    pub fat_offs: Vec<u64>,
    pub data_off: u64,
    pub next_free: u32,
}

impl Img {
    pub fn cluster_size(&self) -> u64 {
        self.bps * self.spc
    }
    pub fn cluster_off(&self, c: u32) -> u64 {
        self.data_off + (c as u64 - 2) * self.cluster_size()
    }

    pub fn set_fat(&mut self, cluster: u32, val: u32) {
        for &fo in &self.fat_offs.clone() {
            match self.bits {
                12 => {
                    let off = fo + cluster as u64 * 3 / 2;
                    let mut b = [0u8; 2];
                    self.dev.read_at(off, &mut b).unwrap();
                    let mut v = u16::from_le_bytes(b);
                    if cluster % 2 == 0 {
                        v = (v & 0xF000) | (val as u16 & 0x0FFF);
                    } else {
                        v = (v & 0x000F) | ((val as u16 & 0x0FFF) << 4);
                    }
                    self.dev.write_at(off, &v.to_le_bytes()).unwrap();
                }
                16 => self.dev.write_at(fo + cluster as u64 * 2, &(val as u16).to_le_bytes()).unwrap(),
                _ => self.dev.write_at(fo + cluster as u64 * 4, &val.to_le_bytes()).unwrap(),
            }
        }
    }

    fn eoc(&self) -> u32 {
        match self.bits {
            12 => 0xFFF,
            16 => 0xFFFF,
            _ => 0x0FFF_FFFF,
        }
    }

    /// Link `clusters` into a chain in the given order, terminated by end-of-chain.
    pub fn link(&mut self, clusters: &[u32]) {
        for w in clusters.windows(2) {
            self.set_fat(w[0], w[1]);
        }
        let eoc = self.eoc();
        self.set_fat(*clusters.last().unwrap(), eoc);
    }

    /// Allocate `n` consecutive clusters from the free pointer.
    pub fn alloc(&mut self, n: u32) -> Vec<u32> {
        let v: Vec<u32> = (self.next_free..self.next_free + n).collect();
        self.next_free += n;
        v
    }

    /// Write `data` across `clusters` (in order) and link them.
    pub fn write_chain(&mut self, clusters: &[u32], data: &[u8]) {
        let cs = self.cluster_size() as usize;
        for (i, &c) in clusters.iter().enumerate() {
            let start = i * cs;
            if start >= data.len() {
                break;
            }
            let end = (start + cs).min(data.len());
            let off = self.cluster_off(c);
            self.dev.write_at(off, &data[start..end]).unwrap();
        }
        self.link(clusters);
    }

    /// Append raw 32-byte entries to the first free slots of a directory.
    pub fn put_entries(&mut self, loc: DirLoc, entries: &[[u8; 32]]) {
        let (base, len) = match loc {
            DirLoc::Fixed(o, l) => (o, l),
            DirLoc::Cluster(c) => (self.cluster_off(c), self.cluster_size()),
        };
        let mut slot = 0u64;
        loop {
            assert!((slot + entries.len() as u64) * 32 <= len, "directory full");
            let mut first = [0u8; 1];
            self.dev.read_at(base + slot * 32, &mut first).unwrap();
            if first[0] == 0 {
                break;
            }
            slot += 1;
        }
        for (i, e) in entries.iter().enumerate() {
            self.dev.write_at(base + (slot + i as u64) * 32, e).unwrap();
        }
    }
}

pub fn sample_bytes(n: usize, mul: usize) -> Vec<u8> {
    (0..n).map(|i| (i * mul % 251) as u8).collect()
}

/// Everything the FAT32 fixture contains, for assertions.
pub struct Fat32Fixture {
    pub img: Img,
    pub hello: Vec<u8>,
    pub big: Vec<u8>,
    pub big_chain: Vec<u32>,
    pub root_cluster: u32,
    pub subdir_cluster: u32,
}

/// 40 MiB FAT32 volume with 512-byte clusters, formatted by `format_fat32` and populated
/// by writing entries directly:
///
/// ```text
/// /                (cluster 2, label TESTVOL)
///   HELLO.TXT         11 bytes, cluster 3, 2024-03-15 13:45:20
///   Long File Name.txt  5 bytes, cluster 4, LFN set + LONGFI~1.TXT
///   SUBDIR/           cluster 5
///     BIG.BIN         1300 bytes, chain 7 -> 9 -> 8
///     (deleted) OLD.BIN, 42 bytes
///   LOWER.TXT         3 bytes, cluster 10, NT lowercase flags
///   (deleted) GONE.TXT, 77 bytes
/// ```
pub fn fat32_fixture() -> Fat32Fixture {
    let mut dev = MemDevice::new(40 * MIB);
    pm_fs::format_fat32(
        &mut dev,
        &pm_fs::FormatOpts { label: "TESTVOL".into(), cluster_size: Some(512), volume_id: Some(0xCAFE_F00D) },
    )
    .unwrap();
    let mut s0 = [0u8; 512];
    dev.read_at(0, &mut s0).unwrap();
    let bps = u16::from_le_bytes([s0[11], s0[12]]) as u64;
    let spc = s0[13] as u64;
    let reserved = u16::from_le_bytes([s0[14], s0[15]]) as u64;
    let nfats = s0[16] as u64;
    let fatsz = u32::from_le_bytes([s0[36], s0[37], s0[38], s0[39]]) as u64;
    let root_cluster = u32::from_le_bytes([s0[44], s0[45], s0[46], s0[47]]);
    let fat_offs = (0..nfats).map(|i| (reserved + i * fatsz) * bps).collect();
    let mut img = Img {
        dev,
        bits: 32,
        bps,
        spc,
        fat_offs,
        data_off: (reserved + nfats * fatsz) * bps,
        next_free: 3,
    };

    let hello = b"hello world".to_vec();
    img.write_chain(&[3], &hello);
    img.write_chain(&[4], b"lfn!!");
    // SUBDIR
    img.link(&[5]);
    let big = sample_bytes(1300, 7);
    let big_chain = vec![7, 9, 8];
    img.write_chain(&big_chain, &big);
    img.write_chain(&[10], b"low");

    let root = DirLoc::Cluster(root_cluster);
    let mut hello_e = Short::file(dos_name("HELLO", "TXT"), 3, hello.len() as u32);
    hello_e.date = dos_date(2024, 3, 15);
    hello_e.time = dos_time(13, 45, 20);
    img.put_entries(root, &[hello_e.bytes()]);

    let lfn_short = dos_name("LONGFI~1", "TXT");
    let mut set = lfn_entries("Long File Name.txt", &lfn_short);
    set.push(Short::file(lfn_short, 4, 5).bytes());
    img.put_entries(root, &set);

    img.put_entries(root, &[Short::dir(dos_name("SUBDIR", ""), 5).bytes()]);

    let mut lower = Short::file(dos_name("LOWER", "TXT"), 10, 3);
    lower.nt = 0x18;
    img.put_entries(root, &[lower.bytes()]);

    let mut gone = Short::file(dos_name("GONE", "TXT"), 6, 77).bytes();
    gone[0] = 0xE5;
    img.put_entries(root, &[gone]);

    let sub = DirLoc::Cluster(5);
    let mut dot = Short::dir(dos_name(".", ""), 5);
    dot.name = *b".          ";
    let mut dotdot = Short::dir(dos_name("..", ""), 0);
    dotdot.name = *b"..         ";
    img.put_entries(sub, &[dot.bytes(), dotdot.bytes()]);
    img.put_entries(sub, &[Short::file(dos_name("BIG", "BIN"), 7, big.len() as u32).bytes()]);
    let mut old = Short::file(dos_name("OLD", "BIN"), 11, 42).bytes();
    old[0] = 0xE5;
    img.put_entries(sub, &[old]);

    Fat32Fixture { img, hello, big, big_chain, root_cluster, subdir_cluster: 5 }
}

/// Hand-built FAT12/FAT16 image (no formatter involved). `bits` is 12 or 16.
/// Layout: 1 reserved sector, 2 FATs, fixed root, 512-byte clusters.
pub struct Fat1x {
    pub img: Img,
    pub root: DirLoc,
}

pub fn fat1x_image(bits: u8) -> Fat1x {
    match bits {
        12 => fat1x_custom(12, 2880, 1, 1, 9, 224, 0xF0),
        _ => fat1x_custom(16, 16384, 1, 1, 65, 512, 0xF8),
    }
}

/// Hand-built FAT12/16 image with explicit geometry (512-byte sectors, 2 FATs, fixed root).
pub fn fat1x_custom(
    bits: u8,
    total: u32,
    spc: u8,
    reserved: u16,
    fatsz: u32,
    root_entries: u32,
    media: u8,
) -> Fat1x {
    let mut dev = MemDevice::new(total as u64 * 512);
    let mut s0 = [0u8; 512];
    s0[..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    s0[3..11].copy_from_slice(b"MSDOS5.0");
    put16(&mut s0, 11, 512);
    s0[13] = spc;
    put16(&mut s0, 14, reserved);
    s0[16] = 2;
    put16(&mut s0, 17, root_entries as u16);
    put16(&mut s0, 19, total as u16);
    s0[21] = media;
    put16(&mut s0, 22, fatsz as u16);
    put16(&mut s0, 24, 63);
    put16(&mut s0, 26, 255);
    s0[36] = 0x80;
    s0[38] = 0x29;
    put32(&mut s0, 39, 0x1234_5678);
    s0[43..54].copy_from_slice(b"SMALLVOL   ");
    s0[54..62].copy_from_slice(if bits == 12 { b"FAT12   " } else { b"FAT16   " });
    s0[510] = 0x55;
    s0[511] = 0xAA;
    dev.write_at(0, &s0).unwrap();
    let fat_offs: Vec<u64> = vec![reserved as u64 * 512, (reserved as u64 + fatsz as u64) * 512];
    let root_off = (reserved as u64 + 2 * fatsz as u64) * 512;
    let root_len = root_entries as u64 * 32;
    let data_off = root_off + root_len;
    let mut img = Img { dev, bits, bps: 512, spc: spc as u64, fat_offs, data_off, next_free: 2 };
    img.set_fat(0, 0xFFFF_FF00 | media as u32);
    img.set_fat(1, 0xFFFF_FFFF);
    Fat1x { img, root: DirLoc::Fixed(root_off, root_len) }
}
