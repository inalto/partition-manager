//! Integration tests for the virtual disk image formats.

use pm_core::{BlockDevice, CancelToken, Error, MemDevice, NullProgress};
use pm_image::{convert, create, identify, open, CreateOpts, ImageFormat};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MIB: u64 = 1024 * 1024;
/// Deliberately not a multiple of any block/grain/cluster size.
const CAP: u64 = 37 * MIB + 5 * 512;

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes(b[o..o + 2].try_into().unwrap())
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 31 + seed as u64 * 7) % 251) as u8 + 1)
        .collect()
}

fn opts(format: ImageFormat, capacity: u64) -> CreateOpts {
    CreateOpts {
        format,
        capacity,
        sparse: true,
    }
}

/// The writes required by the brief (0, 2 MiB + 7, end - 4 KiB) plus one crossing a
/// 2 MiB block boundary.
fn test_writes(cap: u64) -> Vec<(u64, Vec<u8>)> {
    vec![
        (0, pattern(4096, 1)),
        (2 * MIB + 7, pattern(200_000, 2)),
        (cap - 4096, pattern(4096, 3)),
        (6 * MIB - 100, pattern(300, 4)),
    ]
}

fn assert_same(dev: &mut dyn BlockDevice, reference: &mut MemDevice) {
    assert_eq!(dev.len(), reference.len());
    let mut a = vec![0u8; MIB as usize];
    let mut b = vec![0u8; MIB as usize];
    let mut off = 0;
    while off < dev.len() {
        let n = (dev.len() - off).min(MIB) as usize;
        dev.read_at(off, &mut a[..n]).unwrap();
        reference.read_at(off, &mut b[..n]).unwrap();
        assert!(a[..n] == b[..n], "mismatch in chunk at {off}");
        off += n as u64;
    }
}

fn roundtrip(fmt: ImageFormat, name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    let mut reference = MemDevice::new(CAP);
    {
        let mut dev = create(&path, &opts(fmt, CAP)).unwrap();
        assert_eq!(dev.len(), CAP);
        assert!(!dev.is_read_only());
        for (off, data) in test_writes(CAP) {
            dev.write_at(off, &data).unwrap();
            reference.write_at(off, &data).unwrap();
        }
        // read back before closing
        assert_same(dev.as_mut(), &mut reference);
        // out-of-range access is rejected
        assert!(dev.read_at(CAP - 10, &mut [0u8; 11]).is_err());
        assert!(dev.write_at(CAP, &[1u8]).is_err());
        dev.flush().unwrap();
    }
    assert_eq!(identify(&path).unwrap(), fmt);
    {
        let mut dev = open(&path, true).unwrap();
        assert!(dev.is_read_only());
        assert_eq!(dev.len(), CAP);
        assert_same(dev.as_mut(), &mut reference);
        // unwritten areas read back as zeros
        let mut buf = vec![0xAAu8; 8192];
        dev.read_at(20 * MIB, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
        assert!(dev.write_at(0, b"x").is_err());
    }
    // reopen read-write: the existing map is loaded and new allocations append correctly
    {
        let mut dev = open(&path, false).unwrap();
        let extra = pattern(1000, 5);
        dev.write_at(10 * MIB + 3, &extra).unwrap();
        reference.write_at(10 * MIB + 3, &extra).unwrap();
        let over = pattern(512, 6);
        dev.write_at(0, &over).unwrap();
        reference.write_at(0, &over).unwrap();
        dev.flush().unwrap();
    }
    let mut dev = open(&path, true).unwrap();
    assert_same(dev.as_mut(), &mut reference);
}

#[test]
fn create_write_reopen_read_raw() {
    roundtrip(ImageFormat::Raw, "d.img");
}

#[test]
fn create_write_reopen_read_vhd_fixed() {
    roundtrip(ImageFormat::VhdFixed, "d.vhd");
}

#[test]
fn create_write_reopen_read_vhd_dynamic() {
    roundtrip(ImageFormat::VhdDynamic, "d.vhd");
}

#[test]
fn create_write_reopen_read_vmdk_flat() {
    roundtrip(ImageFormat::VmdkFlat, "d.vmdk");
}

#[test]
fn create_write_reopen_read_vmdk_sparse() {
    roundtrip(ImageFormat::VmdkSparse, "d.vmdk");
}

#[test]
fn create_write_reopen_read_qcow2() {
    roundtrip(ImageFormat::Qcow2, "d.qcow2");
}

// ---------------------------------------------------------------- VHD

fn vhd_checksum(b: &[u8], field: usize) -> u32 {
    let mut sum: u32 = 0;
    for (i, &x) in b.iter().enumerate() {
        if i < field || i >= field + 4 {
            sum = sum.wrapping_add(x as u32);
        }
    }
    !sum
}

fn check_vhd_footer(f: &[u8], cap: u64, disk_type: u32, data_offset: u64) {
    assert_eq!(f.len(), 512);
    assert_eq!(&f[0..8], b"conectix");
    assert_eq!(be32(f, 8), 2, "features");
    assert_eq!(be32(f, 12), 0x0001_0000, "format version");
    assert_eq!(be64(f, 16), data_offset, "data offset");
    // seconds since 2000-01-01: any date after 2020 is > 630M
    assert!(be32(f, 24) > 630_000_000, "timestamp");
    // what Windows writes; qemu/VirtualBox then trust current_size over CHS
    assert_eq!(&f[28..32], b"win ", "creator application");
    assert_eq!(be32(f, 32), 0x0001_0000, "creator version");
    assert_eq!(&f[36..40], b"Wi2k");
    assert_eq!(be64(f, 40), cap, "original size");
    assert_eq!(be64(f, 48), cap, "current size");
    assert_eq!(be32(f, 60), disk_type);
    assert_eq!(be32(f, 64), vhd_checksum(f, 64), "footer checksum");
    assert!(f[68..84].iter().any(|&b| b != 0), "unique id");
    assert_eq!(f[84], 0, "saved state");
}

#[test]
fn vhd_footer_checksum_valid() {
    let dir = tempfile::tempdir().unwrap();
    let cap = 40 * MIB; // 81920 sectors -> CHS 963/5/17 per the VHD spec algorithm

    let fixed = dir.path().join("fixed.vhd");
    drop(create(&fixed, &opts(ImageFormat::VhdFixed, cap)).unwrap());
    let raw = std::fs::read(&fixed).unwrap();
    assert_eq!(raw.len() as u64, cap + 512);
    let footer = &raw[raw.len() - 512..];
    check_vhd_footer(footer, cap, 2, u64::MAX);
    assert_eq!(be16(footer, 56), 963);
    assert_eq!(footer[58], 5);
    assert_eq!(footer[59], 17);

    let dynamic = dir.path().join("dyn.vhd");
    drop(create(&dynamic, &opts(ImageFormat::VhdDynamic, cap)).unwrap());
    let raw = std::fs::read(&dynamic).unwrap();
    // footer copy + dynamic header + BAT (20 entries -> 1 sector) + footer
    assert_eq!(raw.len(), 512 + 1024 + 512 + 512);
    let footer = &raw[raw.len() - 512..];
    check_vhd_footer(footer, cap, 3, 512);
    assert_eq!(&raw[..512], footer, "footer copy at offset 0 equals footer at EOF");
    assert_eq!((be16(footer, 56), footer[58], footer[59]), (963, 5, 17));
    let h = &raw[512..1536];
    assert_eq!(&h[0..8], b"cxsparse");
    assert_eq!(be64(h, 8), u64::MAX);
    assert_eq!(be64(h, 16), 1536);
    assert_eq!(be32(h, 24), 0x0001_0000);
    assert_eq!(be32(h, 28), 20, "max table entries");
    assert_eq!(be32(h, 32), 0x0020_0000, "block size");
    assert_eq!(be32(h, 36), vhd_checksum(h, 36), "header checksum");
    assert!(h[40..].iter().all(|&b| b == 0), "parent fields zero");
    for i in 0..20 {
        assert_eq!(be32(&raw, 1536 + i * 4), 0xFFFF_FFFF);
    }

    // large disks use the 255-sector branch of the algorithm and clamp at 65535*16*255
    let big = dir.path().join("big.vhd");
    drop(create(&big, &opts(ImageFormat::VhdDynamic, 1_000 * 1024 * MIB)).unwrap());
    let raw = std::fs::read(&big).unwrap();
    let footer = &raw[raw.len() - 512..];
    assert_eq!((be16(footer, 56), footer[58], footer[59]), (65535, 16, 255));
    assert_eq!(be32(footer, 64), vhd_checksum(footer, 64));
}

#[test]
fn vhd_dynamic_bat_grows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grow.vhd");
    let cap = 40 * MIB;
    let initial = 2560u64;
    let data = pattern(4096, 9);
    {
        let mut dev = create(&path, &opts(ImageFormat::VhdDynamic, cap)).unwrap();
        dev.flush().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), initial);
        dev.write_at(6 * MIB + 100, &data).unwrap(); // block 3
        dev.flush().unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            initial + 512 + 2 * MIB
        );
        dev.write_at(5, &data).unwrap(); // block 0
        dev.flush().unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(raw.len() as u64, initial + 2 * (512 + 2 * MIB));
    let bat = |i: usize| be32(&raw, 1536 + i * 4);
    // the first block replaces the old footer position (sector 4), the second follows it
    assert_eq!(bat(3), 4);
    assert_eq!(bat(0) as u64, (2048 + 512 + 2 * MIB) / 512);
    for i in (0..20).filter(|i| *i != 0 && *i != 3) {
        assert_eq!(bat(i), 0xFFFF_FFFF, "untouched block {i}");
    }
    for (blk, within) in [(3usize, 100usize), (0, 5)] {
        let start = bat(blk) as usize * 512;
        assert!(raw[start..start + 512].iter().all(|&b| b == 0xFF), "sector bitmap");
        let d = start + 512 + within;
        assert_eq!(&raw[d..d + data.len()], &data[..]);
        // zero padding around the written data inside the block
        assert!(raw[start + 512..d].iter().all(|&b| b == 0));
    }
    let footer = &raw[raw.len() - 512..];
    check_vhd_footer(footer, cap, 3, 512);
    assert_eq!(&raw[..512], footer);
}

// ---------------------------------------------------------------- VMDK

#[test]
fn vmdk_sparse_header_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.vmdk");
    let data = pattern(4096, 11);
    {
        let mut dev = create(&path, &opts(ImageFormat::VmdkSparse, CAP)).unwrap();
        dev.write_at(33 * MIB + 10, &data).unwrap(); // GT 1, grain 528
        dev.flush().unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(&raw[0..4], b"KDMV");
    assert_eq!(le32(&raw, 0), 0x564d_444b);
    assert_eq!(le32(&raw, 4), 1, "version");
    assert_eq!(le32(&raw, 8), 3, "flags");
    let sectors = CAP / 512;
    assert_eq!(le64(&raw, 12), sectors, "capacity");
    assert_eq!(le64(&raw, 20), 128, "grainSize");
    assert_eq!(le64(&raw, 28), 1, "descriptorOffset");
    assert_eq!(le64(&raw, 36), 20, "descriptorSize");
    assert_eq!(le32(&raw, 44), 512, "numGTEsPerGT");
    let rgd = le64(&raw, 48);
    let gd = le64(&raw, 56);
    let overhead = le64(&raw, 64);
    assert!(rgd >= 21 && gd > rgd);
    assert_eq!(overhead % 2048, 0, "overHead 1 MiB aligned");
    assert_eq!(raw[72], 0, "uncleanShutdown");
    assert_eq!(&raw[73..77], b"\n \r\n");
    assert_eq!(u16::from_le_bytes([raw[77], raw[78]]), 0, "compressAlgorithm");
    assert!(raw[79..512].iter().all(|&b| b == 0));

    let desc = String::from_utf8_lossy(&raw[512..512 + 20 * 512]);
    let desc = desc.trim_end_matches('\0');
    assert!(desc.starts_with("# Disk DescriptorFile\n"));
    assert!(desc.contains("createType=\"monolithicSparse\""));
    assert!(desc.contains(&format!("RW {sectors} SPARSE \"disk.vmdk\"")));
    assert!(desc.contains("parentCID=ffffffff"));
    assert!(desc.contains("ddb.geometry.heads = \"16\""));
    assert!(desc.contains("ddb.geometry.sectors = \"63\""));
    assert!(desc.contains("ddb.adapterType = \"lsilogic\""));
    let cid_line = desc.lines().find(|l| l.starts_with("CID=")).unwrap();
    assert_eq!(cid_line.len(), 4 + 8);

    // 37 MiB + 2.5 KiB needs two grain tables (32 MiB each)
    let n_gt = 2usize;
    let gt_sectors = 512 * 4 / 512;
    for g in 0..n_gt {
        let p = le32(&raw, gd as usize * 512 + g * 4) as u64;
        let r = le32(&raw, rgd as usize * 512 + g * 4) as u64;
        assert!(p != 0 && r != 0 && p != r);
        assert!(p + gt_sectors <= overhead && r + gt_sectors <= overhead);
    }
    let gte = |dir_off: u64, g: usize, e: usize| {
        let gt = le32(&raw, dir_off as usize * 512 + g * 4) as usize;
        le32(&raw, gt * 512 + e * 4) as u64
    };
    let grain = ((33 * MIB + 10) / (64 * 1024)) as usize;
    let (g, e) = (grain / 512, grain % 512);
    let p = gte(gd, g, e);
    assert_eq!(p, gte(rgd, g, e), "redundant grain table mirrors primary");
    assert!(p >= overhead);
    assert_eq!(p % 128, 0);
    let within = ((33 * MIB + 10) % (64 * 1024)) as usize;
    let at = p as usize * 512 + within;
    assert_eq!(&raw[at..at + data.len()], &data[..]);
    // every other grain is still unallocated
    let total_grains = (sectors as usize).div_ceil(128);
    for i in (0..total_grains).filter(|&i| i != grain) {
        assert_eq!(gte(gd, i / 512, i % 512), 0);
    }
    assert_eq!(raw.len() as u64, (overhead + 128) * 512);
}

#[test]
fn vmdk_flat_descriptor_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vm.vmdk");
    {
        let mut dev = create(&path, &opts(ImageFormat::VmdkFlat, CAP)).unwrap();
        dev.write_at(512, b"hello").unwrap();
        dev.flush().unwrap();
    }
    let text = std::fs::read_to_string(&path).unwrap();
    let sectors = CAP / 512;
    assert!(text.starts_with("# Disk DescriptorFile\n"));
    assert!(text.contains("version=1\n"));
    assert!(text.contains("createType=\"monolithicFlat\""));
    assert!(text.contains(&format!("RW {sectors} FLAT \"vm-flat.vmdk\" 0\n")));
    assert!(text.contains("ddb.adapterType = \"lsilogic\""));
    let flat = dir.path().join("vm-flat.vmdk");
    let raw = std::fs::read(&flat).unwrap();
    assert_eq!(raw.len() as u64, CAP);
    assert_eq!(&raw[512..517], b"hello");
    // open() resolves the extent relative to the descriptor directory, not the cwd
    let mut dev = open(&path, true).unwrap();
    let mut b = [0u8; 5];
    dev.read_at(512, &mut b).unwrap();
    assert_eq!(&b, b"hello");
}

// ---------------------------------------------------------------- qcow2

#[test]
fn qcow2_header_fields_and_refcounts_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.qcow2");
    // > 512 MiB so that more than one L2 table / L1 entry is used
    let cap = 1536 * MIB - 512 * 7;
    let writes: Vec<(u64, Vec<u8>)> = vec![
        (0, pattern(4096, 1)),
        (2 * MIB + 7, pattern(70_000, 2)),
        (600 * MIB, pattern(65536 * 3, 3)),
        (cap - 4096, pattern(4096, 4)),
    ];
    {
        let mut dev = create(&path, &opts(ImageFormat::Qcow2, cap)).unwrap();
        for (o, d) in &writes {
            dev.write_at(*o, d).unwrap();
        }
        dev.flush().unwrap();
    }
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(be32(&raw, 0), 0x5146_49fb);
    assert_eq!(&raw[0..4], b"QFI\xfb");
    assert_eq!(be32(&raw, 4), 3, "version");
    assert_eq!(be64(&raw, 8), 0, "backing_file_offset");
    assert_eq!(be32(&raw, 16), 0, "backing_file_size");
    assert_eq!(be32(&raw, 20), 16, "cluster_bits");
    assert_eq!(be64(&raw, 24), cap, "size");
    assert_eq!(be32(&raw, 32), 0, "crypt_method");
    let l1_size = be32(&raw, 36) as u64;
    assert_eq!(l1_size, cap.div_ceil(65536 * 8192));
    let l1_off = be64(&raw, 40);
    let rt_off = be64(&raw, 48);
    let rt_clusters = be32(&raw, 56) as u64;
    assert_eq!(l1_off, 65536, "L1 at cluster 1");
    assert_eq!(rt_clusters, 1);
    assert_eq!(be32(&raw, 60), 0, "nb_snapshots");
    assert_eq!(be64(&raw, 64), 0, "snapshots_offset");
    assert_eq!(be64(&raw, 72), 0, "incompatible");
    assert_eq!(be64(&raw, 80), 0, "compatible");
    assert_eq!(be64(&raw, 88), 0, "autoclear");
    assert_eq!(be32(&raw, 96), 4, "refcount_order");
    assert_eq!(be32(&raw, 100), 112, "header_length");
    assert!(raw[104..120].iter().all(|&b| b == 0), "no header extensions");

    let cs = 65536u64;
    assert_eq!(raw.len() as u64 % cs, 0, "file is cluster aligned");
    let n_clusters = raw.len() as u64 / cs;
    const OFF_MASK: u64 = 0x00ff_ffff_ffff_fe00;

    // refcounts from the refcount table / blocks
    let mut refcount: HashMap<u64, u16> = HashMap::new();
    let mut referenced: Vec<u64> = vec![0]; // header
    let l1_clusters = (l1_size * 8).div_ceil(cs);
    referenced.extend((0..l1_clusters).map(|i| l1_off / cs + i));
    referenced.extend((0..rt_clusters).map(|i| rt_off / cs + i));
    for i in 0..(rt_clusters * cs / 8) {
        let rb = be64(&raw, (rt_off + i * 8) as usize);
        if rb == 0 {
            continue;
        }
        assert_eq!(rb % cs, 0);
        referenced.push(rb / cs);
        for j in 0..cs / 2 {
            let rc = be16(&raw, (rb + j * 2) as usize);
            if rc != 0 {
                refcount.insert(i * (cs / 2) + j, rc);
            }
        }
    }
    let mut data_clusters = HashSet::new();
    let mut l1_used = 0;
    for i in 0..l1_size {
        let e = be64(&raw, (l1_off + i * 8) as usize);
        if e == 0 {
            continue;
        }
        l1_used += 1;
        assert!(e & (1 << 63) != 0, "L1 COPIED bit");
        let l2 = e & OFF_MASK;
        assert_eq!(l2 % cs, 0);
        referenced.push(l2 / cs);
        for j in 0..cs / 8 {
            let e2 = be64(&raw, (l2 + j * 8) as usize);
            if e2 == 0 {
                continue;
            }
            assert!(e2 & (1 << 63) != 0, "L2 COPIED bit");
            assert!(e2 & (1 << 62) == 0, "not compressed");
            let d = e2 & OFF_MASK;
            assert_eq!(d % cs, 0);
            assert!(data_clusters.insert(d / cs), "two L2 entries share a cluster");
            referenced.push(d / cs);
            // verify guest data lands where the tables say
            let guest = (i * (cs / 8) + j) * cs;
            for (o, w) in &writes {
                let lo = (*o).max(guest);
                let hi = (o + w.len() as u64).min(guest + cs);
                if lo < hi {
                    let host = (d + lo - guest) as usize;
                    let src = (lo - o) as usize;
                    assert_eq!(
                        &raw[host..host + (hi - lo) as usize],
                        &w[src..src + (hi - lo) as usize]
                    );
                }
            }
        }
    }
    assert_eq!(l1_used, 3, "writes touch L1 entries 0, 1 and 2");
    // 4 KiB@0, 70000@2MiB+7 (2 clusters), 3 clusters @600MiB, last cluster
    assert_eq!(data_clusters.len(), 1 + 2 + 3 + 1);

    let ref_set: HashSet<u64> = referenced.iter().copied().collect();
    assert_eq!(ref_set.len(), referenced.len(), "no cluster used twice");
    for c in &ref_set {
        assert!(*c < n_clusters, "cluster {c} beyond EOF");
        assert_eq!(refcount.get(c), Some(&1), "refcount of cluster {c}");
    }
    for (c, rc) in &refcount {
        assert!(ref_set.contains(c), "leaked cluster {c} with refcount {rc}");
    }
    assert_eq!(ref_set.len() as u64, n_clusters, "every cluster in the file is in use");

    // reopen and verify data through the device too
    let mut dev = open(&path, true).unwrap();
    assert_eq!(dev.len(), cap);
    for (o, d) in &writes {
        let mut b = vec![0u8; d.len()];
        dev.read_at(*o, &mut b).unwrap();
        assert!(&b == d);
    }
}

// ---------------------------------------------------------------- identify / convert

const ALL: [(ImageFormat, &str); 6] = [
    (ImageFormat::Raw, "x.img"),
    (ImageFormat::VhdFixed, "x.vhd"),
    (ImageFormat::VhdDynamic, "x.vhd"),
    (ImageFormat::VmdkFlat, "x.vmdk"),
    (ImageFormat::VmdkSparse, "x.vmdk"),
    (ImageFormat::Qcow2, "x.qcow2"),
];

#[test]
fn identify_each_format() {
    let dir = tempfile::tempdir().unwrap();
    for (i, (fmt, name)) in ALL.iter().enumerate() {
        let sub = dir.path().join(format!("{i}"));
        std::fs::create_dir(&sub).unwrap();
        let path = sub.join(name);
        drop(create(&path, &opts(*fmt, 4 * MIB)).unwrap());
        assert_eq!(identify(&path).unwrap(), *fmt, "{fmt:?}");
    }
    // tiny and garbage files are raw
    let small = dir.path().join("small.bin");
    std::fs::write(&small, b"abc").unwrap();
    assert_eq!(identify(&small).unwrap(), ImageFormat::Raw);
    let junk = dir.path().join("junk.bin");
    std::fs::write(&junk, vec![0x5Au8; 8192]).unwrap();
    assert_eq!(identify(&junk).unwrap(), ImageFormat::Raw);
    assert!(identify(&dir.path().join("missing")).is_err());

    let p = |s: &str| PathBuf::from(s);
    assert_eq!(ImageFormat::from_extension(&p("a.img")), Some(ImageFormat::Raw));
    assert_eq!(ImageFormat::from_extension(&p("a.RAW")), Some(ImageFormat::Raw));
    assert_eq!(ImageFormat::from_extension(&p("a.dd")), Some(ImageFormat::Raw));
    assert_eq!(ImageFormat::from_extension(&p("a.vhd")), Some(ImageFormat::VhdDynamic));
    assert_eq!(ImageFormat::from_extension(&p("a.vmdk")), Some(ImageFormat::VmdkSparse));
    assert_eq!(ImageFormat::from_extension(&p("a.qcow2")), Some(ImageFormat::Qcow2));
    assert_eq!(ImageFormat::from_extension(&p("a.iso")), None);
    assert_eq!(ImageFormat::from_extension(&p("noext")), None);
    let names: Vec<_> = ALL.iter().map(|(f, _)| f.display()).collect();
    assert_eq!(
        names,
        ["Raw", "VHD (fixed)", "VHD (dynamic)", "VMDK (flat)", "VMDK (sparse)", "QCOW2"]
    );
    for (fmt, name) in ALL {
        assert_eq!(Path::new(name).extension().unwrap(), fmt.extension());
    }
    let json = serde_json::to_string(&ImageFormat::VhdDynamic).unwrap();
    assert_eq!(serde_json::from_str::<ImageFormat>(&json).unwrap(), ImageFormat::VhdDynamic);
}

#[test]
fn create_rejects_bad_capacity() {
    let dir = tempfile::tempdir().unwrap();
    for (fmt, name) in ALL {
        let path = dir.path().join(name);
        assert!(matches!(create(&path, &opts(fmt, 4 * MIB + 1)), Err(Error::Invalid(_))));
        assert!(matches!(create(&path, &opts(fmt, 0)), Err(Error::Invalid(_))));
    }
}

#[test]
fn convert_raw_to_each_and_back_equal() {
    let dir = tempfile::tempdir().unwrap();
    let cap = 24 * MIB + 3 * 512;
    let src = dir.path().join("src.img");
    {
        let mut dev = create(&src, &opts(ImageFormat::Raw, cap)).unwrap();
        dev.write_at(0, &pattern(10_000, 1)).unwrap();
        dev.write_at(5 * MIB + 3, &pattern(3 * MIB as usize, 2)).unwrap();
        dev.write_at(cap - 1000, &pattern(1000, 3)).unwrap();
        dev.flush().unwrap();
    }
    let original = std::fs::read(&src).unwrap();
    for (i, (fmt, name)) in ALL.iter().enumerate().skip(1) {
        let sub = dir.path().join(format!("{i}"));
        std::fs::create_dir(&sub).unwrap();
        let dst = sub.join(name);
        convert(&src, &dst, *fmt, &NullProgress, &CancelToken::new()).unwrap();
        assert_eq!(identify(&dst).unwrap(), *fmt);
        assert_eq!(open(&dst, true).unwrap().len(), cap);
        if matches!(fmt, ImageFormat::VhdDynamic | ImageFormat::VmdkSparse | ImageFormat::Qcow2) {
            // all-zero chunks were skipped: only ~4 MiB of data plus metadata
            let size = std::fs::metadata(&dst).unwrap().len();
            assert!(size < 12 * MIB, "{fmt:?} image is {size} bytes");
        }
        let back = sub.join("back.img");
        convert(&dst, &back, ImageFormat::Raw, &NullProgress, &CancelToken::new()).unwrap();
        let roundtripped = std::fs::read(&back).unwrap();
        assert!(roundtripped == original, "{fmt:?} roundtrip differs");
    }
}

#[test]
fn convert_honours_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.img");
    drop(create(&src, &opts(ImageFormat::Raw, 4 * MIB)).unwrap());
    let cancel = CancelToken::new();
    cancel.cancel();
    let dst = dir.path().join("dst.qcow2");
    assert!(matches!(
        convert(&src, &dst, ImageFormat::Qcow2, &NullProgress, &cancel),
        Err(Error::Cancelled)
    ));
}

// ---------------------------------------------------------------- robustness (fix round 1)

fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}
fn put_be64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_be_bytes());
}
fn put_le32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_le64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Write `bytes` to a fresh file and assert that `open` fails cleanly (no panic, no abort
/// from a huge allocation) both read-only and read-write.
fn assert_open_fails(dir: &Path, name: &str, bytes: &[u8], what: &str) {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    for ro in [true, false] {
        assert!(
            open(&path, ro).is_err(),
            "{what}: open(read_only={ro}) unexpectedly succeeded"
        );
    }
}

/// Recompute the checksum of the VHD footer at EOF.
fn fix_vhd_footer(b: &mut [u8]) {
    let n = b.len();
    let sum = vhd_checksum(&b[n - 512..], 64);
    put_be32(b, n - 512 + 64, sum);
}

/// Recompute the checksum of the VHD dynamic header at offset 512.
fn fix_vhd_header(b: &mut [u8]) {
    let sum = vhd_checksum(&b[512..1536], 36);
    put_be32(b, 512 + 36, sum);
}

type Patch = Box<dyn Fn(&mut Vec<u8>)>;

#[test]
fn corrupt_headers_never_panic() {
    let dir = tempfile::tempdir().unwrap();
    let make = |fmt: ImageFormat, name: &str| -> Vec<u8> {
        let p = dir.path().join(format!("base-{name}"));
        {
            let mut d = create(&p, &opts(fmt, 4 * MIB)).unwrap();
            d.write_at(0, &pattern(512, 1)).unwrap();
            d.flush().unwrap();
        }
        std::fs::read(&p).unwrap()
    };

    // ---- qcow2 (big-endian)
    let q = make(ImageFormat::Qcow2, "q.qcow2");
    let qcases: Vec<(&str, Patch)> = vec![
        ("l1_size huge", Box::new(|b| put_be32(b, 36, u32::MAX))),
        ("l1 table over 32 MiB", Box::new(|b| put_be32(b, 36, (32 << 20) / 8 + 1))),
        ("refcount_table_clusters huge", Box::new(|b| put_be32(b, 56, u32::MAX))),
        ("refcount table over 8 MiB", Box::new(|b| put_be32(b, 56, 129))),
        ("l1_table_offset huge", Box::new(|b| put_be64(b, 40, u64::MAX - 7))),
        ("refcount_table_offset huge", Box::new(|b| put_be64(b, 48, u64::MAX - 7))),
        ("cluster_bits 0", Box::new(|b| put_be32(b, 20, 0))),
        ("cluster_bits 63", Box::new(|b| put_be32(b, 20, 63))),
        ("size huge", Box::new(|b| put_be64(b, 24, u64::MAX))),
        (
            "L1 entry beyond EOF",
            Box::new(|b| put_be64(b, 65536, (1 << 63) | 0x00ff_ffff_ffff_0000)),
        ),
    ];
    for (what, patch) in qcases {
        let mut b = q.clone();
        patch(&mut b);
        assert_open_fails(dir.path(), "c.qcow2", &b, what);
    }

    // ---- VHD dynamic (big-endian, checksummed); bool = patch is in the footer
    let v = make(ImageFormat::VhdDynamic, "v.vhd");
    let vcases: Vec<(&str, bool, Patch)> = vec![
        ("block size 0", false, Box::new(|b| put_be32(b, 512 + 32, 0))),
        ("block size not power of two", false, Box::new(|b| put_be32(b, 512 + 32, 3 << 20))),
        ("block size 2 GiB", false, Box::new(|b| put_be32(b, 512 + 32, 0x8000_0000))),
        ("block size 512 MiB", false, Box::new(|b| put_be32(b, 512 + 32, 512 << 20))),
        ("max table entries huge", false, Box::new(|b| put_be32(b, 512 + 28, u32::MAX))),
        ("table offset huge", false, Box::new(|b| put_be64(b, 512 + 16, u64::MAX - 3))),
        (
            "footer data offset huge",
            true,
            Box::new(|b| {
                let n = b.len();
                put_be64(b, n - 512 + 16, u64::MAX - 100)
            }),
        ),
        (
            "current size huge",
            true,
            Box::new(|b| {
                let n = b.len();
                put_be64(b, n - 512 + 48, u64::MAX)
            }),
        ),
        ("BAT entry beyond EOF", false, Box::new(|b| put_be32(b, 1536, 0xFFFF_FF00))),
    ];
    for (what, in_footer, patch) in vcases {
        let mut b = v.clone();
        patch(&mut b);
        if in_footer {
            fix_vhd_footer(&mut b);
        } else {
            fix_vhd_header(&mut b);
        }
        assert_open_fails(dir.path(), "c.vhd", &b, what);
    }
    let mut f = make(ImageFormat::VhdFixed, "f.vhd");
    let n = f.len();
    put_be64(&mut f, n - 512 + 48, u64::MAX);
    fix_vhd_footer(&mut f);
    assert_open_fails(dir.path(), "c.vhd", &f, "fixed current size huge");

    // ---- VMDK sparse (little-endian)
    let s = make(ImageFormat::VmdkSparse, "s.vmdk");
    let scases: Vec<(&str, Patch)> = vec![
        ("grain 0", Box::new(|b| put_le64(b, 20, 0))),
        ("grain not power of two", Box::new(|b| put_le64(b, 20, 100))),
        ("grain huge", Box::new(|b| put_le64(b, 20, u64::MAX))),
        ("grain over 0x200000 sectors", Box::new(|b| put_le64(b, 20, 0x40_0000))),
        ("numGTEsPerGT 0", Box::new(|b| put_le32(b, 44, 0))),
        ("numGTEsPerGT huge", Box::new(|b| put_le32(b, 44, u32::MAX))),
        ("capacity huge", Box::new(|b| put_le64(b, 12, u64::MAX))),
        ("gdOffset huge", Box::new(|b| put_le64(b, 56, u64::MAX / 256))),
        ("rgdOffset huge", Box::new(|b| put_le64(b, 48, u64::MAX / 256))),
    ];
    for (what, patch) in scases {
        let mut b = s.clone();
        patch(&mut b);
        assert_open_fails(dir.path(), "c.vmdk", &b, what);
    }

    // ---- VMDK flat descriptors whose extent numbers overflow
    let sub = dir.path().join("flat");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("x-flat.vmdk"), vec![0u8; 8192 * 512]).unwrap();
    for line in [
        "RW 8192 FLAT \"x-flat.vmdk\" 18446744073709551615",
        "RW 18446744073709551615 FLAT \"x-flat.vmdk\" 0",
        "RW 36028797018963968 FLAT \"x-flat.vmdk\" 36028797018963968",
    ] {
        let text =
            format!("# Disk DescriptorFile\nversion=1\ncreateType=\"monolithicFlat\"\n{line}\n");
        assert_open_fails(&sub, "x.vmdk", text.as_bytes(), line);
    }
}

#[test]
fn vhd_dynamic_footer_valid_after_each_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.vhd");
    let cap = 20 * MIB;
    let mut reference = MemDevice::new(cap);
    drop(create(&path, &opts(ImageFormat::VhdDynamic, cap)).unwrap());
    for (i, blk) in [4u64, 0, 9, 2].iter().enumerate() {
        {
            let mut dev = open(&path, false).unwrap();
            let data = pattern(3000, i as u8 + 1);
            let off = blk * 2 * MIB + 77;
            dev.write_at(off, &data).unwrap();
            reference.write_at(off, &data).unwrap();
            dev.flush().unwrap();
        }
        let raw = std::fs::read(&path).unwrap();
        check_vhd_footer(&raw[raw.len() - 512..], cap, 3, 512);
        assert_eq!(&raw[..512], &raw[raw.len() - 512..]);
        assert_eq!(identify(&path).unwrap(), ImageFormat::VhdDynamic);
        let mut dev = open(&path, true).unwrap();
        assert_same(dev.as_mut(), &mut reference);
    }
}

#[test]
fn qcow2_autoclear_features_cleared_on_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.qcow2");
    drop(create(&path, &opts(ImageFormat::Qcow2, 4 * MIB)).unwrap());
    let mut b = std::fs::read(&path).unwrap();
    put_be64(&mut b, 88, 1);
    std::fs::write(&path, &b).unwrap();

    // a read-only open leaves the header untouched
    {
        let mut dev = open(&path, true).unwrap();
        dev.read_at(0, &mut [0u8; 512]).unwrap();
    }
    assert_eq!(be64(&std::fs::read(&path).unwrap(), 88), 1);

    // a writer that does not understand autoclear bits must clear them
    {
        let mut dev = open(&path, false).unwrap();
        dev.write_at(512, &pattern(512, 7)).unwrap();
        dev.flush().unwrap();
    }
    assert_eq!(be64(&std::fs::read(&path).unwrap(), 88), 0);
    let mut dev = open(&path, true).unwrap();
    let mut got = vec![0u8; 512];
    dev.read_at(512, &mut got).unwrap();
    assert_eq!(got, pattern(512, 7));
}

// ---------------------------------------------------------------- fix round 2

fn assert_same_file_refused(src: &Path, dst: &Path, fmt: ImageFormat, what: &str) {
    match convert(src, dst, fmt, &NullProgress, &CancelToken::new()) {
        Err(Error::Invalid(msg)) => {
            assert_eq!(msg, "destination is the same file as the source", "{what}")
        }
        other => panic!("{what}: expected Invalid, got {other:?}"),
    }
}

#[test]
fn convert_refuses_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();

    // raw -> raw onto itself, spelled directly and through `..`
    let src = dir.path().join("src.img");
    {
        let mut d = create(&src, &opts(ImageFormat::Raw, MIB)).unwrap();
        d.write_at(0, &pattern(4096, 1)).unwrap();
        d.flush().unwrap();
    }
    let before = std::fs::read(&src).unwrap();
    assert_same_file_refused(&src, &src, ImageFormat::Raw, "raw onto itself");
    let dotted = sub.join("..").join("src.img");
    assert_same_file_refused(&src, &dotted, ImageFormat::Qcow2, "same file via ..");
    assert_eq!(std::fs::read(&src).unwrap(), before, "raw source untouched");

    // flat VMDK pair: vm.vmdk (descriptor) + vm-flat.vmdk (extent)
    let desc = dir.path().join("vm.vmdk");
    let flat = dir.path().join("vm-flat.vmdk");
    {
        let mut d = create(&desc, &opts(ImageFormat::VmdkFlat, MIB)).unwrap();
        d.write_at(512, &pattern(2048, 2)).unwrap();
        d.flush().unwrap();
    }
    let desc_before = std::fs::read(&desc).unwrap();
    let flat_before = std::fs::read(&flat).unwrap();
    // the destination's <stem>-flat.vmdk sibling is the source
    assert_same_file_refused(&flat, &desc, ImageFormat::VmdkFlat, "flat sibling is source");
    // the source descriptor's extent is the destination
    assert_same_file_refused(&desc, &flat, ImageFormat::Raw, "source extent is destination");
    assert_eq!(std::fs::read(&desc).unwrap(), desc_before, "descriptor untouched");
    assert_eq!(std::fs::read(&flat).unwrap(), flat_before, "flat extent untouched");

    // distinct files still convert, and an existing destination is overwritten
    let out = sub.join("out.vmdk");
    std::fs::write(&out, b"old contents").unwrap();
    convert(&desc, &out, ImageFormat::VmdkFlat, &NullProgress, &CancelToken::new()).unwrap();
    let mut d = open(&out, true).unwrap();
    let mut got = vec![0u8; 2048];
    d.read_at(512, &mut got).unwrap();
    assert_eq!(got, pattern(2048, 2));
    assert!(sub.join("out-flat.vmdk").exists());
}
