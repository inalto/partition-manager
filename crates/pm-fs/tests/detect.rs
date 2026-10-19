mod common;

use common::*;
use pm_core::{BlockDevice, FsKind, MemDevice};
use pm_fs::{detect, probe_for_search};

const IMG: u64 = 128 * 1024;

fn blank() -> MemDevice {
    MemDevice::new(IMG)
}

fn kind_of(dev: &mut MemDevice) -> FsKind {
    detect(dev, 0).map(|p| p.kind).unwrap_or(FsKind::Unknown)
}

fn ext_sb(compat: u32, incompat: u32, ro: u32) -> MemDevice {
    let mut d = blank();
    let mut sb = vec![0u8; 1024];
    put32(&mut sb, 4, 1000); // blocks count
    put32(&mut sb, 0x18, 2); // log block size -> 4096
    put16(&mut sb, 0x38, 0xEF53);
    put32(&mut sb, 0x5C, compat);
    put32(&mut sb, 0x60, incompat);
    put32(&mut sb, 0x64, ro);
    sb[0x68..0x78].copy_from_slice(&[
        0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
    ]);
    sb[0x78..0x78 + 6].copy_from_slice(b"extlbl");
    d.write_at(0x400, &sb).unwrap();
    d
}

#[test]
fn detect_recognizes_all_signatures() {
    // NTFS
    let mut d = blank();
    d.write_at(3, b"NTFS    ").unwrap();
    d.write_at(11, &512u16.to_le_bytes()).unwrap();
    d.write_at(13, &[8]).unwrap();
    d.write_at(40, &2048u64.to_le_bytes()).unwrap();
    d.write_at(72, &0x1122_3344_5566_7788u64.to_le_bytes()).unwrap();
    let p = detect(&mut d, 0).unwrap();
    assert_eq!(p.kind, FsKind::Ntfs);
    assert_eq!(p.total, 2048 * 512);
    assert_eq!(p.cluster, Some(4096));
    assert_eq!(p.uuid, "1122334455667788");
    assert_eq!(p.label, "");
    assert_eq!(p.used, None);

    // exFAT: 512-byte sectors, 8 sectors per cluster
    let mut d = blank();
    d.write_at(3, b"EXFAT   ").unwrap();
    d.write_at(72, &10_000u64.to_le_bytes()).unwrap();
    d.write_at(108, &[9, 3]).unwrap();
    let p = detect(&mut d, 0).unwrap();
    assert_eq!(p.kind, FsKind::ExFat);
    assert_eq!(p.total, 10_000 * 512);
    assert_eq!(p.cluster, Some(4096));

    // ReFS, both ways
    let mut d = blank();
    d.write_at(3, b"ReFS").unwrap();
    assert_eq!(kind_of(&mut d), FsKind::ReFs);
    let mut d = blank();
    d.write_at(0x10, b"FSRS").unwrap();
    assert_eq!(kind_of(&mut d), FsKind::ReFs);

    // ext2 / ext3 / ext4
    let mut d = ext_sb(0, 0, 0);
    let p = detect(&mut d, 0).unwrap();
    assert_eq!(p.kind, FsKind::Ext2);
    assert_eq!(p.total, 1000 * 4096);
    assert_eq!(p.cluster, Some(4096));
    assert_eq!(p.label, "extlbl");
    assert_eq!(p.uuid, "12345678-9abc-def0-1122-334455667788");
    assert_eq!(kind_of(&mut ext_sb(0x4, 0, 0)), FsKind::Ext3);
    assert_eq!(kind_of(&mut ext_sb(0x4, 0x40, 0)), FsKind::Ext4);
    assert_eq!(kind_of(&mut ext_sb(0, 0x80, 0)), FsKind::Ext4);
    assert_eq!(kind_of(&mut ext_sb(0x4, 0, 0x8)), FsKind::Ext4);
    assert_eq!(kind_of(&mut ext_sb(0x4, 0, 0x40)), FsKind::Ext4);
    assert_eq!(kind_of(&mut ext_sb(0x4, 0x2, 0x1)), FsKind::Ext3, "filetype/sparse flags do not imply ext4");

    // btrfs
    let mut d = blank();
    d.write_at(0x10040, b"_BHRfS_M").unwrap();
    assert_eq!(kind_of(&mut d), FsKind::Btrfs);

    // XFS
    let mut d = blank();
    d.write_at(0, b"XFSB").unwrap();
    assert_eq!(kind_of(&mut d), FsKind::Xfs);

    // HFS+ and HFSX
    for sig in [b"H+", b"HX"] {
        let mut d = blank();
        d.write_at(0x400, sig).unwrap();
        assert_eq!(kind_of(&mut d), FsKind::HfsPlus);
    }

    // APFS
    let mut d = blank();
    d.write_at(0x20, b"NXSB").unwrap();
    assert_eq!(kind_of(&mut d), FsKind::Apfs);

    // LUKS
    let mut d = blank();
    d.write_at(0, b"LUKS\xba\xbe").unwrap();
    assert_eq!(kind_of(&mut d), FsKind::Luks);

    // swap, both spellings
    for sig in [&b"SWAPSPACE2"[..], &b"SWAP-SPACE"[..]] {
        let mut d = blank();
        d.write_at(4096 - 10, sig).unwrap();
        assert_eq!(kind_of(&mut d), FsKind::LinuxSwap);
    }

    // ISO 9660
    let mut d = blank();
    d.write_at(0x8001, b"CD001").unwrap();
    assert_eq!(kind_of(&mut d), FsKind::Iso9660);

    // UDF
    let mut d = blank();
    d.write_at(0x8001, b"BEA01").unwrap();
    assert_eq!(kind_of(&mut d), FsKind::Udf);
    for k in 0..4u64 {
        for sig in [b"NSR02", b"NSR03"] {
            let mut d = blank();
            d.write_at(0x8001 + 0x800 * k, sig).unwrap();
            assert_eq!(kind_of(&mut d), FsKind::Udf, "k={k}");
        }
    }

    // nothing recognisable
    assert!(detect(&mut blank(), 0).is_none());
}

#[test]
fn detect_fat_variants_by_cluster_count() {
    for (bits, kind) in [(12u8, FsKind::Fat12), (16, FsKind::Fat16)] {
        let mut dev = fat1x_image(bits).img.dev;
        let p = detect(&mut dev, 0).unwrap();
        assert_eq!(p.kind, kind);
        assert_eq!(p.label, "SMALLVOL");
        assert_eq!(p.uuid, "1234-5678");
        assert_eq!(p.cluster, Some(512));
        assert_eq!(p.used, None);
    }
    let mut dev = fat32_fixture().img.dev;
    let p = detect(&mut dev, 0).unwrap();
    assert_eq!(p.kind, FsKind::Fat32);
    assert_eq!(p.label, "TESTVOL");
    assert_eq!(p.uuid, "CAFE-F00D");
}

#[test]
fn detect_fat_rejects_bad_bpb() {
    let good = fat1x_image(16).img.dev;
    let mutate = |off: usize, bytes: &[u8]| {
        let mut d = MemDevice::from_vec(good.as_slice().to_vec());
        d.write_at(off as u64, bytes).unwrap();
        d
    };
    assert!(detect(&mut mutate(11, &1000u16.to_le_bytes()), 0).is_none(), "bytes per sector");
    assert!(detect(&mut mutate(13, &[3]), 0).is_none(), "sectors per cluster not a power of two");
    assert!(detect(&mut mutate(13, &[0]), 0).is_none());
    assert!(detect(&mut mutate(14, &0u16.to_le_bytes()), 0).is_none(), "reserved sectors");
    assert!(detect(&mut mutate(16, &[0]), 0).is_none(), "num fats");
    assert!(detect(&mut mutate(16, &[3]), 0).is_none());
    assert!(detect(&mut mutate(19, &0u16.to_le_bytes()), 0).is_none(), "total sectors");
    assert!(detect(&mut mutate(510, &[0, 0]), 0).is_none(), "boot signature");
}

#[test]
fn detect_survives_short_devices_and_offsets() {
    // device shorter than the 68 KiB window
    let mut d = MemDevice::new(2048);
    d.write_at(3, b"NTFS    ").unwrap();
    assert_eq!(detect(&mut d, 0).unwrap().kind, FsKind::Ntfs);
    // swap needs a full page
    let mut d = MemDevice::new(4096);
    d.write_at(4086, b"SWAPSPACE2").unwrap();
    assert_eq!(detect(&mut d, 0).unwrap().kind, FsKind::LinuxSwap);
    // tiny and empty devices never panic
    assert!(detect(&mut MemDevice::new(0), 0).is_none());
    assert!(detect(&mut MemDevice::new(10), 0).is_none());
    // offset at or beyond the end
    assert!(detect(&mut MemDevice::new(4096), 4096).is_none());
    assert!(detect(&mut MemDevice::new(4096), 1 << 40).is_none());
    // detection relative to an offset
    let mut d = MemDevice::new(MIB + 4096);
    d.write_at(MIB + 3, b"EXFAT   ").unwrap();
    d.write_at(MIB + 108, &[9, 0]).unwrap();
    assert_eq!(detect(&mut d, MIB).unwrap().kind, FsKind::ExFat);
    assert!(detect(&mut d, 0).is_none());
}

#[test]
fn probe_for_search_adapts_detect() {
    let mut dev = fat1x_image(16).img.dev;
    let (k, label, total) = probe_for_search(&mut dev, 0).unwrap();
    assert_eq!(k, FsKind::Fat16);
    assert_eq!(label, "SMALLVOL");
    assert_eq!(total, 8 * MIB);
    assert!(probe_for_search(&mut blank(), 0).is_none());
}

#[test]
fn fs_probe_serializes() {
    let mut dev = fat1x_image(16).img.dev;
    let p = detect(&mut dev, 0).unwrap();
    let json = serde_json::to_string(&p).unwrap();
    assert!(json.contains("Fat16"));
}
