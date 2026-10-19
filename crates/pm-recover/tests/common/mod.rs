//! Test-only helpers: GPT images with FAT32 partitions and a tiny FAT32 directory-entry writer.
//! The writer never uses the reader under test (`pm_fs::FatFs`) to build its images.
#![allow(dead_code)]

use pm_core::units::MIB;
use pm_core::{BlockDevice, MemDevice, SliceDevice, TableKind};
use pm_fs::{format_fat32, FormatOpts};
use pm_parttable::{write_table, Entry, Table};

pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn dos_name(base: &str, ext: &str) -> [u8; 11] {
    let mut n = [b' '; 11];
    n[..base.len()].copy_from_slice(base.as_bytes());
    n[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
    n
}

/// Deterministic printable filler that never forms a carving signature.
pub fn text_bytes(n: usize, seed: usize) -> Vec<u8> {
    (0..n).map(|i| b'a' + ((i * 7 + seed * 13) % 26) as u8).collect()
}

/// A FAT32 volume starting at offset 0 of `dev` (as formatted by `format_fat32` with 512-byte
/// clusters). Files go into the root directory cluster (16 entries).
pub struct Fat32Writer<'a> {
    dev: &'a mut dyn BlockDevice,
    bps: u64,
    spc: u64,
    fat_offs: [u64; 2],
    data_off: u64,
    root_cluster: u32,
    next_free: u32,
}

impl<'a> Fat32Writer<'a> {
    pub fn new(dev: &'a mut dyn BlockDevice) -> Self {
        let mut b = [0u8; 512];
        dev.read_at(0, &mut b).unwrap();
        let bps = le16(&b, 11) as u64;
        let spc = b[13] as u64;
        let reserved = le16(&b, 14) as u64;
        let nfats = b[16] as u64;
        assert_eq!(nfats, 2);
        let fat_sectors = le32(&b, 36) as u64;
        let root_cluster = le32(&b, 44);
        let fat0 = reserved * bps;
        let fat1 = fat0 + fat_sectors * bps;
        let data_off = fat1 + fat_sectors * bps;
        Fat32Writer { dev, bps, spc, fat_offs: [fat0, fat1], data_off, root_cluster, next_free: root_cluster + 1 }
    }

    pub fn cluster_size(&self) -> u64 {
        self.bps * self.spc
    }

    fn cluster_off(&self, c: u32) -> u64 {
        self.data_off + (c as u64 - 2) * self.cluster_size()
    }

    fn set_fat(&mut self, c: u32, v: u32) {
        for fo in self.fat_offs {
            self.dev.write_at(fo + c as u64 * 4, &v.to_le_bytes()).unwrap();
        }
    }

    /// Write `data` to freshly allocated consecutive clusters and return the first one.
    /// With `link` the FAT chain is written, otherwise the FAT entries stay free (as after a
    /// real delete).
    fn put_data(&mut self, data: &[u8], link: bool) -> u32 {
        let cs = self.cluster_size() as usize;
        let n = data.len().div_ceil(cs).max(1) as u32;
        let first = self.next_free;
        self.next_free += n;
        for i in 0..n {
            let c = first + i;
            let lo = i as usize * cs;
            let hi = (lo + cs).min(data.len());
            if lo < data.len() {
                self.dev.write_at(self.cluster_off(c), &data[lo..hi]).unwrap();
            }
            if link {
                self.set_fat(c, if i + 1 == n { 0x0FFF_FFFF } else { c + 1 });
            }
        }
        first
    }

    /// Number of the last valid data cluster.
    pub fn last_cluster(&mut self) -> u32 {
        ((self.dev.len() - self.data_off) / self.cluster_size()) as u32 + 1
    }

    /// Write `data` into cluster `c` without touching the FAT.
    pub fn write_cluster(&mut self, c: u32, data: &[u8]) {
        self.dev.write_at(self.cluster_off(c), data).unwrap();
    }

    /// Add a root directory entry pointing at `first` with the given size field and no data.
    pub fn add_raw_entry(&mut self, name: [u8; 11], first: u32, size: u32) {
        self.put_entry(name, first, size);
    }

    /// Add a file to the root directory; returns its first cluster.
    pub fn add_file(&mut self, name: [u8; 11], data: &[u8]) -> u32 {
        self.add_file_opts(name, data, true)
    }

    /// Like `add_file`, but with `link == false` the cluster chain is left unlinked in the FAT.
    pub fn add_file_opts(&mut self, name: [u8; 11], data: &[u8], link: bool) -> u32 {
        let first = self.put_data(data, link);
        self.put_entry(name, first, data.len() as u32);
        first
    }

    fn put_entry(&mut self, name: [u8; 11], first: u32, size: u32) {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(&name);
        e[11] = 0x20;
        e[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&(first as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        let root = self.cluster_off(self.root_cluster);
        let mut slot = 0u64;
        loop {
            assert!(slot < self.cluster_size() / 32, "root directory full");
            let mut b = [0u8; 1];
            self.dev.read_at(root + slot * 32, &mut b).unwrap();
            if b[0] == 0 {
                break;
            }
            slot += 1;
        }
        self.dev.write_at(root + slot * 32, &e).unwrap();
    }

    /// Mark the root entry called `name` deleted (first byte 0xE5). With `free_chain` its FAT
    /// chain is zeroed, as a real delete does.
    pub fn delete(&mut self, name: [u8; 11], free_chain: bool) {
        let root = self.cluster_off(self.root_cluster);
        for slot in 0..self.cluster_size() / 32 {
            let mut e = [0u8; 32];
            self.dev.read_at(root + slot * 32, &mut e).unwrap();
            if e[..11] == name {
                let first = ((le16(&e, 20) as u32) << 16) | le16(&e, 26) as u32;
                let size = le32(&e, 28) as u64;
                self.dev.write_at(root + slot * 32, &[0xE5]).unwrap();
                if free_chain {
                    let n = size.div_ceil(self.cluster_size()).max(1) as u32;
                    for i in 0..n {
                        self.set_fat(first + i, 0);
                    }
                }
                return;
            }
        }
        panic!("entry not found");
    }
}

/// Format `[start, start + len)` of `dev` as FAT32 with 512-byte clusters.
pub fn format_part(dev: &mut MemDevice, start: u64, len: u64, label: &str) {
    let mut s = SliceDevice::new(dev, start, len).unwrap();
    format_fat32(
        &mut s,
        &FormatOpts { label: label.into(), cluster_size: Some(512), volume_id: Some(0xABCD_0001) },
    )
    .unwrap();
}

pub const DISK_LEN: u64 = 100 * MIB;
pub const P1: (u64, u64) = (MIB, 40 * MIB);
pub const P2: (u64, u64) = (41 * MIB, 40 * MIB);

/// 100 MiB GPT disk with two FAT32 partitions (labels ONE and TWO, one file in the first).
pub fn gpt_two_fat32() -> (MemDevice, Table) {
    let mut dev = MemDevice::new(DISK_LEN);
    format_part(&mut dev, P1.0, P1.1, "ONE");
    format_part(&mut dev, P2.0, P2.1, "TWO");
    {
        let mut s = SliceDevice::new(&mut dev, P1.0, P1.1).unwrap();
        Fat32Writer::new(&mut s).add_file(dos_name("HELLO", "TXT"), b"hello world");
    }
    let mut t = Table::empty(TableKind::Gpt, DISK_LEN, 512);
    for (i, (s, l)) in [P1, P2].into_iter().enumerate() {
        t.entries.push(Entry {
            number: i as u32 + 1,
            start: s,
            len: l,
            ptype: pm_core::PartitionType::Gpt(pm_core::gpt_types::BASIC_DATA),
            guid: Some(uuid::Uuid::new_v4()),
            name: if i == 0 { "ONE".into() } else { "TWO".into() },
            attrs: 0,
            logical: false,
        });
    }
    t.validate().unwrap();
    write_table(&mut dev, &t).unwrap();
    (dev, t)
}

/// Zero LBA 0..34 (protective MBR, primary header and entry array).
pub fn wipe_primary(dev: &mut MemDevice) {
    dev.write_at(0, &vec![0u8; 34 * 512]).unwrap();
}

/// Zero the last 33 sectors (backup entry array and header).
pub fn wipe_backup(dev: &mut MemDevice) {
    let n = dev.len();
    dev.write_at(n - 33 * 512, &vec![0u8; 33 * 512]).unwrap();
}
