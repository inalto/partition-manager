//! Helpers shared by the end-to-end tests: a minimal FAT32 file writer, a GPT image builder and
//! a JPEG generator, all independent of the code under test.
#![allow(dead_code)]

use pm_core::{gpt_types, BlockDevice, FileDevice, PartitionType, SliceDevice, TableKind};
use pm_fs::fat::format::{format_fat32, FormatOpts};
use pm_parttable::{write_table, Entry, Table};
use std::path::Path;
use std::process::{Command, Output};

pub fn pm(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pm")).args(args).output().expect("spawn pm")
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

pub fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

pub fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 31 + seed as u64 * 7) % 251) as u8 + 1)
        .collect()
}

pub fn read_all(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

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

/// A minimal FAT32 file writer (short names only) for a freshly formatted volume.
pub struct Fat32Builder<'a> {
    dev: &'a mut dyn BlockDevice,
    fat_offs: Vec<u64>,
    data_off: u64,
    cluster: u64,
    next_free: u32,
}

impl<'a> Fat32Builder<'a> {
    pub fn new(dev: &'a mut dyn BlockDevice) -> Self {
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

    pub fn add_file(&mut self, dir_cluster: u32, name: &str, data: &[u8]) {
        let c = self.write_chain(data);
        self.put_entries(dir_cluster, &[Self::entry(name, 0x20, c, data.len() as u32)]);
    }

    pub fn add_dir(&mut self, parent: u32, name: &str) -> u32 {
        let c = self.write_chain(&[]);
        let parent_ref = if parent == 2 { 0 } else { parent };
        self.put_entries(c, &[Self::entry(".", 0x10, c, 0), Self::entry("..", 0x10, parent_ref, 0)]);
        self.put_entries(parent, &[Self::entry(name, 0x10, c, 0)]);
        c
    }

    /// Mark the entry called `name` in directory cluster `dir` deleted (first byte 0xE5),
    /// leaving its cluster chain in place.
    pub fn delete(&mut self, dir: u32, name: &str) {
        let base = self.cluster_off(dir);
        let want = dos_name(name);
        for slot in 0..self.cluster / 32 {
            let mut e = [0u8; 32];
            self.dev.read_at(base + slot * 32, &mut e).unwrap();
            if e[..11] == want {
                self.dev.write_at(base + slot * 32, &[0xE5]).unwrap();
                return;
            }
        }
        panic!("entry {name} not found");
    }
}

pub const HELLO: &[u8] = b"hello from fat32\n";

pub fn data_bin() -> Vec<u8> {
    pattern(10_000, 9)
}

/// Format `dev` (>= 33 MiB) as FAT32 and add HELLO.TXT, DATA.BIN, SUB/INNER.TXT.
pub fn populate_fat32(dev: &mut dyn BlockDevice) {
    populate_fat32_with(dev, |_| {});
}

/// Like [`populate_fat32`], calling `extra` with the builder before the volume is closed.
pub fn populate_fat32_with(dev: &mut dyn BlockDevice, extra: impl FnOnce(&mut Fat32Builder)) {
    format_fat32(
        dev,
        &FormatOpts { label: "PMTEST".into(), cluster_size: None, volume_id: Some(0x1234_5678) },
    )
    .unwrap();
    let mut b = Fat32Builder::new(dev);
    b.add_file(2, "HELLO.TXT", HELLO);
    b.add_file(2, "DATA.BIN", &data_bin());
    let sub = b.add_dir(2, "SUB");
    b.add_file(sub, "INNER.TXT", b"inner file\n");
    extra(&mut b);
    dev.flush().unwrap();
}

/// A GPT image of `disk_len` bytes with one FAT32 partition `[start, start + len)` holding the
/// files of [`populate_fat32`].
pub fn gpt_fat32_image(path: &Path, disk_len: u64, start: u64, len: u64) {
    gpt_fat32_image_with(path, disk_len, start, len, |_| {});
}

/// Like [`gpt_fat32_image`], calling `extra` with the filesystem builder.
pub fn gpt_fat32_image_with(
    path: &Path,
    disk_len: u64,
    start: u64,
    len: u64,
    extra: impl FnOnce(&mut Fat32Builder),
) {
    let mut dev = FileDevice::create(path, disk_len).unwrap();
    {
        let mut slice = SliceDevice::new(&mut dev, start, len).unwrap();
        populate_fat32_with(&mut slice, extra);
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
}

/// A structurally valid JPEG: SOI, JFIF APP0, SOS + data, EOI.
pub fn jpeg(body: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    v.extend(b"JFIF\0\x01\x01\x01\x00\x48\x00\x48\x00\x00");
    v.extend([0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
    v.extend((0..body).map(|i| (i % 200) as u8 + 1));
    v.extend([0xFF, 0xD9]);
    v
}
