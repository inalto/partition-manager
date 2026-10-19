use pm_core::units::MIB;
use pm_core::{BlockDevice, DiskInfo, Error, MemDevice, PartitionType, TableKind};
use pm_parttable::{
    chs_from_lba, is_protective_mbr, parse_mbr, read_table, write_mbr, write_table, Entry, Table,
};

const DISK: u64 = 64 * MIB;

/// Write a 16-byte MBR/EBR partition entry at 0x1BE + i*16 (LBA values in sectors).
fn put_entry(img: &mut [u8], i: usize, status: u8, ptype: u8, start_lba: u32, len_lba: u32) {
    let o = 0x1BE + i * 16;
    img[o] = status;
    img[o + 4] = ptype;
    img[o + 8..o + 12].copy_from_slice(&start_lba.to_le_bytes());
    img[o + 12..o + 16].copy_from_slice(&len_lba.to_le_bytes());
}

fn put_sig(img: &mut [u8]) {
    img[510] = 0x55;
    img[511] = 0xAA;
}

fn entry(number: u32, start: u64, len: u64, ptype: u8, logical: bool) -> Entry {
    Entry {
        number,
        start,
        len,
        ptype: PartitionType::Mbr(ptype),
        guid: None,
        name: String::new(),
        attrs: 0,
        logical,
    }
}

fn image_with_four_primaries() -> MemDevice {
    let mut img = vec![0u8; DISK as usize];
    put_entry(&mut img, 0, 0x80, 0x07, 2048, 8192); // 1 MiB, 4 MiB
    put_entry(&mut img, 1, 0x00, 0x83, 12288, 4096); // 6 MiB, 2 MiB
    put_entry(&mut img, 2, 0x00, 0x0B, 20480, 2048); // 10 MiB, 1 MiB
    put_entry(&mut img, 3, 0x00, 0xEF, 30720, 6144); // 15 MiB, 3 MiB
    img[440..444].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    put_sig(&mut img);
    MemDevice::from_vec(img)
}

/// Extended partition at 10 MiB (20 MiB long) with two logicals at 11 MiB and 17 MiB.
fn image_with_extended_chain() -> MemDevice {
    let mut img = vec![0u8; DISK as usize];
    put_entry(&mut img, 0, 0x00, 0x07, 2048, 8192);
    put_entry(&mut img, 1, 0x00, 0x0F, 20480, 40960);
    put_sig(&mut img);
    // EBR 0 at the extended start (10 MiB)
    let e0 = 20480 * 512;
    let mut ebr0 = vec![0u8; 512];
    put_entry(&mut ebr0, 0, 0x00, 0x07, 2048, 8192);
    put_entry(&mut ebr0, 1, 0x00, 0x05, 12288, 10240); // next EBR at ext + 12288 sectors = 16 MiB
    put_sig(&mut ebr0);
    img[e0..e0 + 512].copy_from_slice(&ebr0);
    // EBR 1 at 16 MiB
    let e1 = (20480 + 12288) * 512;
    let mut ebr1 = vec![0u8; 512];
    put_entry(&mut ebr1, 0, 0x80, 0x83, 2048, 8192);
    put_sig(&mut ebr1);
    img[e1..e1 + 512].copy_from_slice(&ebr1);
    MemDevice::from_vec(img)
}

#[test]
fn mbr_parse_four_primaries() {
    let mut dev = image_with_four_primaries();
    let t = parse_mbr(&mut dev).unwrap();
    assert_eq!(t.kind, TableKind::Mbr);
    assert_eq!(t.sector_size, 512);
    assert_eq!(t.disk_len, DISK);
    assert_eq!(t.mbr_signature, 0x1234_5678);
    assert_eq!(t.bootcode.len(), 440);
    assert_eq!(t.entries.len(), 4);
    let want = [
        (1, MIB, 4 * MIB, 0x07, 1),
        (2, 6 * MIB, 2 * MIB, 0x83, 0),
        (3, 10 * MIB, MIB, 0x0B, 0),
        (4, 15 * MIB, 3 * MIB, 0xEF, 0),
    ];
    for (e, (n, s, l, ty, attrs)) in t.entries.iter().zip(want) {
        assert_eq!(e.number, n);
        assert_eq!(e.start, s);
        assert_eq!(e.len, l);
        assert_eq!(e.ptype, PartitionType::Mbr(ty));
        assert_eq!(e.attrs, attrs);
        assert!(!e.logical);
        assert_eq!(e.guid, None);
        assert_eq!(e.name, "");
    }
    t.validate().unwrap();
}

#[test]
fn mbr_parse_skips_empty_slots_and_keeps_slot_numbers() {
    let mut img = vec![0u8; DISK as usize];
    put_entry(&mut img, 1, 0x00, 0x07, 2048, 2048);
    put_entry(&mut img, 3, 0x00, 0x83, 8192, 2048);
    put_sig(&mut img);
    let t = parse_mbr(&mut MemDevice::from_vec(img)).unwrap();
    let nums: Vec<u32> = t.entries.iter().map(|e| e.number).collect();
    assert_eq!(nums, vec![2, 4]);
    assert_eq!(t.next_number(), 1);
}

#[test]
fn mbr_parse_extended_chain_two_logicals() {
    let mut dev = image_with_extended_chain();
    let t = parse_mbr(&mut dev).unwrap();
    assert_eq!(t.entries.len(), 4);
    let ext = &t.entries[1];
    assert!(ext.ptype.is_extended_mbr());
    assert_eq!((ext.number, ext.start, ext.len), (2, 10 * MIB, 20 * MIB));
    let l1 = &t.entries[2];
    assert!(l1.logical);
    assert_eq!((l1.number, l1.start, l1.len), (5, 11 * MIB, 4 * MIB));
    assert_eq!(l1.ptype, PartitionType::Mbr(0x07));
    assert_eq!(l1.attrs, 0);
    let l2 = &t.entries[3];
    assert!(l2.logical);
    assert_eq!((l2.number, l2.start, l2.len), (6, 17 * MIB, 4 * MIB));
    assert_eq!(l2.ptype, PartitionType::Mbr(0x83));
    assert_eq!(l2.attrs, 1);
    t.validate().unwrap();
}

#[test]
fn mbr_parse_stops_on_ebr_loop() {
    let mut dev = image_with_extended_chain();
    // Make EBR 1 point back at EBR 0 (relative start 0).
    let e1 = ((20480 + 12288) * 512) as u64;
    let mut ebr1 = vec![0u8; 512];
    dev.read_at(e1, &mut ebr1).unwrap();
    put_entry(&mut ebr1, 1, 0x00, 0x05, 0, 4096);
    dev.write_at(e1, &ebr1).unwrap();
    let t = parse_mbr(&mut dev).unwrap();
    assert_eq!(t.entries.iter().filter(|e| e.logical).count(), 2);
}

#[test]
fn mbr_parse_extended_without_ebr_has_no_logicals() {
    let mut img = vec![0u8; DISK as usize];
    put_entry(&mut img, 0, 0x00, 0x0F, 2048, 40960);
    put_sig(&mut img);
    let t = parse_mbr(&mut MemDevice::from_vec(img)).unwrap();
    assert_eq!(t.entries.len(), 1);
}

fn sample_table() -> Table {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.mbr_signature = 0xCAFE_F00D;
    t.bootcode = (0..440).map(|i| (i % 251) as u8).collect();
    let mut p1 = entry(1, MIB, 4 * MIB, 0x07, false);
    p1.attrs = 1;
    t.entries = vec![
        p1,
        entry(2, 6 * MIB, 2 * MIB, 0x83, false),
        entry(3, 10 * MIB, 20 * MIB, 0x0F, false),
        entry(5, 11 * MIB, 4 * MIB, 0x07, true),
        entry(6, 17 * MIB, 4 * MIB, 0x83, true),
    ];
    t
}

#[test]
fn mbr_roundtrip_write_then_parse_equal() {
    let t = sample_table();
    let mut dev = MemDevice::new(DISK);
    write_mbr(&mut dev, &t).unwrap();
    let back = parse_mbr(&mut dev).unwrap();
    assert_eq!(back, t);
    // The second EBR sits 1 MiB before its logical.
    let mut ebr = vec![0u8; 512];
    dev.read_at(16 * MIB, &mut ebr).unwrap();
    assert_eq!(&ebr[510..], &[0x55, 0xAA]);
    assert_eq!(ebr[0x1BE + 4], 0x83);
    // The first EBR is at the extended start and links to the second.
    dev.read_at(10 * MIB, &mut ebr).unwrap();
    assert_eq!(ebr[0x1BE + 16 + 4], 0x0F);
    // Via the generic entry points too.
    let mut dev2 = MemDevice::new(DISK);
    write_table(&mut dev2, &t).unwrap();
    assert_eq!(read_table(&mut dev2).unwrap(), t);
    assert_eq!(dev2.as_slice(), dev.as_slice());
}

#[test]
fn mbr_roundtrip_places_primaries_by_slot_number() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.mbr_signature = 7;
    t.entries = vec![
        entry(2, MIB, 2 * MIB, 0x07, false),
        entry(4, 10 * MIB, 2 * MIB, 0x83, false),
    ];
    let mut dev = MemDevice::new(DISK);
    write_mbr(&mut dev, &t).unwrap();
    let s = dev.as_slice();
    assert_eq!(s[0x1BE + 4], 0);
    assert_eq!(s[0x1BE + 16 + 4], 0x07);
    assert_eq!(s[0x1BE + 32 + 4], 0);
    assert_eq!(s[0x1BE + 48 + 4], 0x83);
    assert_eq!(parse_mbr(&mut dev).unwrap(), t);
}

#[test]
fn mbr_roundtrip_extended_without_logicals() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.entries = vec![entry(1, 10 * MIB, 20 * MIB, 0x0F, false)];
    let mut dev = MemDevice::new(DISK);
    // Stale EBR at the extended start must not resurrect a logical.
    let mut stale = vec![0u8; 512];
    put_entry(&mut stale, 0, 0, 0x07, 2048, 2048);
    put_sig(&mut stale);
    dev.write_at(10 * MIB, &stale).unwrap();
    write_mbr(&mut dev, &t).unwrap();
    assert_eq!(parse_mbr(&mut dev).unwrap(), t);
}

#[test]
fn mbr_roundtrip_4k_sectors() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 4096);
    t.mbr_signature = 0x0BAD_CAFE;
    t.entries = vec![
        entry(1, MIB, 4 * MIB, 0x07, false),
        entry(2, 10 * MIB, 20 * MIB, 0x0F, false),
        entry(5, 11 * MIB, 4 * MIB, 0x83, true),
    ];
    let mut dev = MemDevice::new(DISK).with_sector_size(4096);
    write_mbr(&mut dev, &t).unwrap();
    assert_eq!(&dev.as_slice()[510..512], &[0x55, 0xAA]);
    assert_eq!(parse_mbr(&mut dev).unwrap(), t);
}

#[test]
fn mbr_preserves_bootcode_and_signature() {
    let mut img = vec![0u8; DISK as usize];
    for (i, b) in img[..440].iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(1);
    }
    img[440..444].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    img[444] = 0x5A;
    img[445] = 0x5A;
    put_entry(&mut img, 0, 0x80, 0x07, 2048, 8192);
    put_sig(&mut img);
    let boot = img[..440].to_vec();
    let mut dev = MemDevice::from_vec(img);

    let mut t = parse_mbr(&mut dev).unwrap();
    assert_eq!(t.bootcode, boot);
    assert_eq!(t.mbr_signature, 0xDEAD_BEEF);
    t.entries.push(entry(2, 10 * MIB, 4 * MIB, 0x83, false));
    write_mbr(&mut dev, &t).unwrap();

    let s = dev.as_slice();
    assert_eq!(&s[..440], &boot[..]);
    assert_eq!(&s[440..444], &0xDEAD_BEEFu32.to_le_bytes());
    assert_eq!(&s[444..446], &[0x5A, 0x5A], "reserved bytes are kept");
    assert_eq!(&s[510..512], &[0x55, 0xAA]);
    let back = parse_mbr(&mut dev).unwrap();
    assert_eq!(back, t);
    assert_eq!(back.entries[0].attrs, 1);
}

#[test]
fn mbr_writes_chs_and_status() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    let mut p = entry(1, MIB, 4 * MIB, 0x07, false);
    p.attrs = 1;
    t.entries = vec![p];
    let mut dev = MemDevice::new(DISK);
    write_mbr(&mut dev, &t).unwrap();
    let s = dev.as_slice();
    assert_eq!(s[0x1BE], 0x80);
    assert_eq!(&s[0x1BE + 1..0x1BE + 4], &chs_from_lba(2048));
    assert_eq!(&s[0x1BE + 5..0x1BE + 8], &chs_from_lba(2048 + 8192 - 1));
    assert_eq!(u32::from_le_bytes(s[0x1BE + 8..0x1BE + 12].try_into().unwrap()), 2048);
    assert_eq!(u32::from_le_bytes(s[0x1BE + 12..0x1BE + 16].try_into().unwrap()), 8192);
}

#[test]
fn chs_values() {
    assert_eq!(chs_from_lba(0), [0, 1, 0]);
    assert_eq!(chs_from_lba(2048), [32, 33, 0]);
    // cylinder 1000, head 3, sector 5
    let lba = 1000 * 255 * 63 + 3 * 63 + 4;
    let c = chs_from_lba(lba);
    assert_eq!(c[0], 3);
    assert_eq!(c[1] & 0x3F, 5);
    assert_eq!(((c[1] as u16 >> 6) << 8) | c[2] as u16, 1000);
    // last addressable cylinder 1023, then clamp
    assert_eq!(chs_from_lba(1024 * 255 * 63 - 1), [254, 0xFF, 0xFF]);
    assert_eq!(chs_from_lba(1024 * 255 * 63), [0xFE, 0xFF, 0xFF]);
    assert_eq!(chs_from_lba(u64::MAX), [0xFE, 0xFF, 0xFF]);
}

#[test]
fn mbr_rejects_no_55aa() {
    let mut dev = MemDevice::new(DISK);
    assert!(matches!(parse_mbr(&mut dev), Err(Error::Parse(_))));
    // read_table treats an unsigned sector 0 as "no partition table", not an error.
    let t = read_table(&mut dev).unwrap();
    assert_eq!(t.kind, TableKind::None);
    assert!(t.entries.is_empty());
    assert_eq!(t.disk_len, DISK);
}

#[test]
fn protective_mbr_is_detected_and_read_table_defers_to_gpt() {
    let mut img = vec![0u8; DISK as usize];
    put_entry(&mut img, 0, 0, 0xEE, 1, (DISK / 512 - 1) as u32);
    put_sig(&mut img);
    assert!(is_protective_mbr(&img[..512]));
    let mut dev = MemDevice::from_vec(img);
    // Protective MBR but no GPT behind it: the GPT parser reports it.
    assert!(matches!(read_table(&mut dev), Err(Error::Parse(_))));

    // A normal MBR is not protective, nor is one without a signature.
    assert!(!is_protective_mbr(image_with_four_primaries().as_slice()));
    assert!(!is_protective_mbr(&[0u8; 512]));
    let mut hybrid = vec![0u8; 512];
    put_entry(&mut hybrid, 0, 0, 0xEE, 1, 100);
    put_entry(&mut hybrid, 1, 0, 0x07, 200, 100);
    put_sig(&mut hybrid);
    assert!(!is_protective_mbr(&hybrid));
}

#[test]
fn read_table_dispatches_to_mbr() {
    let mut dev = image_with_extended_chain();
    let t = read_table(&mut dev).unwrap();
    assert_eq!(t.kind, TableKind::Mbr);
    assert_eq!(t.entries.len(), 4);
}

#[test]
fn write_table_dispatch() {
    let mut dev = MemDevice::new(DISK);
    let none = Table::empty(TableKind::None, DISK, 512);
    assert!(matches!(write_table(&mut dev, &none), Err(Error::Invalid(_))));
    assert!(dev.as_slice().iter().all(|&b| b == 0), "nothing written on error");
    // GPT goes to the GPT writer.
    let gpt = Table::empty(TableKind::Gpt, DISK, 512);
    write_table(&mut dev, &gpt).unwrap();
    assert!(is_protective_mbr(&dev.as_slice()[..512]));
    assert_eq!(&dev.as_slice()[512..520], b"EFI PART");
}

#[test]
fn mbr_parse_stops_at_ebr_link_past_device() {
    let mut dev = image_with_extended_chain();
    // Point EBR 0's link far beyond the 64 MiB device: the chain ends, logical 1 is kept.
    let e0 = 20480 * 512;
    let mut ebr0 = vec![0u8; 512];
    dev.read_at(e0, &mut ebr0).unwrap();
    put_entry(&mut ebr0, 1, 0x00, 0x05, 4_000_000, 10240);
    dev.write_at(e0, &ebr0).unwrap();
    let t = parse_mbr(&mut dev).unwrap();
    assert_eq!(t.entries.len(), 3);
    assert!(t.entries[2].logical);
    assert_eq!(t.entries[2].number, 5);
}

#[test]
fn write_mbr_rejects_short_bootcode() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.entries = vec![entry(1, MIB, 4 * MIB, 0x07, false)];
    t.bootcode = vec![0xAA; 100];
    let mut dev = MemDevice::new(DISK);
    assert!(matches!(write_mbr(&mut dev, &t), Err(Error::Invalid(_))));
    t.bootcode = Vec::new();
    assert!(matches!(write_mbr(&mut dev, &t), Err(Error::Invalid(_))));
    assert!(dev.as_slice().iter().all(|&b| b == 0));
}

#[test]
fn mbr_overlap_validate_fails() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.entries = vec![
        entry(1, MIB, 4 * MIB, 0x07, false),
        entry(2, 4 * MIB, 4 * MIB, 0x83, false), // starts inside partition 1
    ];
    assert!(matches!(t.validate(), Err(Error::Invalid(_))));
    // write_mbr refuses and leaves the device untouched
    let mut dev = MemDevice::new(DISK);
    assert!(matches!(write_mbr(&mut dev, &t), Err(Error::Invalid(_))));
    assert!(dev.as_slice().iter().all(|&b| b == 0));
    // Adjacent is fine.
    t.entries[1].start = 5 * MIB;
    t.validate().unwrap();
}

#[test]
fn validate_rules() {
    let base = || Table::empty(TableKind::Mbr, DISK, 512);
    let bad = |t: &Table| assert!(matches!(t.validate(), Err(Error::Invalid(_))), "{:?}", t.entries);

    // beyond disk
    let mut t = base();
    t.entries = vec![entry(1, 60 * MIB, 8 * MIB, 0x07, false)];
    bad(&t);
    // zero length
    let mut t = base();
    t.entries = vec![entry(1, MIB, 0, 0x07, false)];
    bad(&t);
    // unaligned start / len
    let mut t = base();
    t.entries = vec![entry(1, MIB + 1, MIB, 0x07, false)];
    bad(&t);
    let mut t = base();
    t.entries = vec![entry(1, MIB, MIB + 100, 0x07, false)];
    bad(&t);
    // five primaries
    let mut t = base();
    t.entries = (0..5)
        .map(|i| entry(i + 1, MIB + i as u64 * 2 * MIB, MIB, 0x07, false))
        .collect();
    bad(&t);
    // two extended
    let mut t = base();
    t.entries = vec![
        entry(1, MIB, MIB, 0x05, false),
        entry(2, 4 * MIB, MIB, 0x0F, false),
    ];
    bad(&t);
    // logical without extended
    let mut t = base();
    t.entries = vec![entry(5, MIB, MIB, 0x07, true)];
    bad(&t);
    // logical outside extended
    let mut t = base();
    t.entries = vec![
        entry(1, 10 * MIB, 10 * MIB, 0x0F, false),
        entry(5, 21 * MIB, MIB, 0x07, true),
    ];
    bad(&t);
    // overlapping logicals
    let mut t = base();
    t.entries = vec![
        entry(1, 10 * MIB, 20 * MIB, 0x0F, false),
        entry(5, 12 * MIB, 4 * MIB, 0x07, true),
        entry(6, 14 * MIB, 4 * MIB, 0x07, true),
    ];
    bad(&t);
    // primary inside the extended container
    let mut t = base();
    t.entries = vec![
        entry(1, 10 * MIB, 20 * MIB, 0x0F, false),
        entry(2, 12 * MIB, MIB, 0x07, false),
    ];
    bad(&t);
    // GPT type in an MBR table
    let mut t = base();
    let mut e = entry(1, MIB, MIB, 0x07, false);
    e.ptype = PartitionType::Gpt(pm_core::gpt_types::BASIC_DATA);
    t.entries = vec![e];
    bad(&t);
    // sample table is valid
    sample_table().validate().unwrap();
}

#[test]
fn write_mbr_rejects_logical_too_close_to_extended_start() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.entries = vec![
        entry(1, 10 * MIB, 20 * MIB, 0x0F, false),
        entry(5, 10 * MIB + 512 * 63, 4 * MIB, 0x07, true), // no room for a 1 MiB EBR gap
    ];
    // validate applies the same EBR-room rule as write_mbr
    assert!(matches!(t.validate(), Err(Error::Invalid(_))));
    let mut dev = MemDevice::new(DISK);
    assert!(matches!(write_mbr(&mut dev, &t), Err(Error::Invalid(_))));
    // second logical too close to the first
    t.entries = vec![
        entry(1, 10 * MIB, 20 * MIB, 0x0F, false),
        entry(5, 11 * MIB, 4 * MIB, 0x07, true),
        entry(6, 15 * MIB + 512, MIB, 0x07, true),
    ];
    assert!(matches!(t.validate(), Err(Error::Invalid(_))));
    assert!(matches!(write_mbr(&mut dev, &t), Err(Error::Invalid(_))));
    assert!(dev.as_slice().iter().all(|&b| b == 0));
}

#[test]
fn free_ranges_mbr_with_extended() {
    let t = sample_table();
    // used: p1 [1,5), p2 [6,8), ext [10,30) with logicals [11,15) [17,21)
    let want = vec![
        (5 * MIB, MIB),
        (8 * MIB, 2 * MIB),
        (10 * MIB, MIB),      // inside ext, before first logical
        (15 * MIB, 2 * MIB),  // inside ext between logicals
        (21 * MIB, 9 * MIB),  // inside ext after last logical
        (30 * MIB, 34 * MIB), // after ext to the end of the disk
    ];
    assert_eq!(t.free_ranges(1), want);
    assert_eq!(
        t.free_ranges(2 * MIB),
        vec![(8 * MIB, 2 * MIB), (15 * MIB, 2 * MIB), (21 * MIB, 9 * MIB), (30 * MIB, 34 * MIB)]
    );
    // empty MBR: everything after the 1 MiB front reserve
    let e = Table::empty(TableKind::Mbr, DISK, 512);
    assert_eq!(e.free_ranges(1), vec![(MIB, DISK - MIB)]);
    // an extended partition with no logicals is entirely free inside
    let mut x = Table::empty(TableKind::Mbr, DISK, 512);
    x.entries = vec![entry(1, 10 * MIB, 20 * MIB, 0x0F, false)];
    assert_eq!(
        x.free_ranges(1),
        vec![(MIB, 9 * MIB), (10 * MIB, 20 * MIB), (30 * MIB, 34 * MIB)]
    );
}

#[test]
fn free_ranges_gpt_uses_usable_bounds() {
    let mut t = Table::empty(TableKind::Gpt, DISK, 512);
    assert_eq!(t.gpt_first_usable, MIB);
    assert_eq!(t.gpt_last_usable, DISK - 33 * 512);
    t.entries = vec![entry(1, MIB, 10 * MIB, 0x07, false)];
    t.entries[0].ptype = PartitionType::Gpt(pm_core::gpt_types::BASIC_DATA);
    assert_eq!(t.free_ranges(1), vec![(11 * MIB, DISK - 33 * 512 - 11 * MIB)]);
}

#[test]
fn table_empty_defaults() {
    let t = Table::empty(TableKind::Mbr, DISK, 512);
    assert_eq!(t.kind, TableKind::Mbr);
    assert_ne!(t.mbr_signature, 0);
    assert_eq!(t.bootcode, vec![0u8; 440]);
    assert_eq!(t.disk_guid, None);
    assert_eq!(t.gpt_first_usable, MIB);
    assert_eq!(t.gpt_last_usable, DISK);
    assert!(t.entries.is_empty());

    let g = Table::empty(TableKind::Gpt, DISK, 4096);
    assert!(g.disk_guid.is_some());
    assert_eq!(g.gpt_first_usable, MIB);
    assert_eq!(g.gpt_last_usable, DISK - 33 * 4096);

    let n = Table::empty(TableKind::None, DISK, 512);
    assert_eq!(n.kind, TableKind::None);
    assert_eq!(n.mbr_signature, 0);
}

#[test]
fn next_number_is_lowest_unused() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    assert_eq!(t.next_number(), 1);
    t.entries = vec![entry(1, MIB, MIB, 7, false), entry(3, 4 * MIB, MIB, 7, false)];
    assert_eq!(t.next_number(), 2);
    t.entries.push(entry(2, 8 * MIB, MIB, 7, false));
    assert_eq!(t.next_number(), 4);
}

#[test]
fn to_disk_info_fills_table_id_and_partitions() {
    let t = sample_table();
    let base = DiskInfo {
        index: 3,
        model: "Test".into(),
        size: DISK,
        sector_size: 512,
        ..Default::default()
    };
    let info = t.to_disk_info(base);
    assert_eq!(info.index, 3);
    assert_eq!(info.model, "Test");
    assert_eq!(info.table, TableKind::Mbr);
    assert_eq!(info.disk_id, "CAFEF00D");
    assert_eq!(info.partitions.len(), 5);
    let p1 = &info.partitions[0];
    assert_eq!((p1.number, p1.start, p1.len), (1, MIB, 4 * MIB));
    assert_eq!(p1.ptype, Some(PartitionType::Mbr(0x07)));
    assert!(p1.flags.active);
    assert!(!p1.logical);
    assert!(!info.partitions[1].flags.active);
    assert!(info.partitions[3].logical);
    assert_eq!(info.partitions[3].number, 5);

    let mut g = Table::empty(TableKind::Gpt, DISK, 512);
    let id = g.disk_guid.unwrap();
    let ginfo = g.to_disk_info(DiskInfo::default());
    assert_eq!(ginfo.table, TableKind::Gpt);
    assert_eq!(ginfo.disk_id, id.to_string());
    g.disk_guid = None;
    assert_eq!(g.to_disk_info(DiskInfo::default()).disk_id, "");
}

#[test]
fn regenerate_ids_changes_signature_and_guids() {
    let mut t = sample_table();
    let old = t.mbr_signature;
    t.regenerate_ids();
    assert_ne!(t.mbr_signature, old);
    assert_ne!(t.mbr_signature, 0);

    let mut g = Table::empty(TableKind::Gpt, DISK, 512);
    let mut e = entry(1, MIB, MIB, 0, false);
    e.ptype = PartitionType::Gpt(pm_core::gpt_types::BASIC_DATA);
    e.guid = Some(uuid::Uuid::new_v4());
    g.entries = vec![e];
    let (dg, pg) = (g.disk_guid, g.entries[0].guid);
    g.regenerate_ids();
    assert_ne!(g.disk_guid, dg);
    assert!(g.disk_guid.is_some());
    assert_ne!(g.entries[0].guid, pg);
    assert!(g.entries[0].guid.is_some());
}

#[test]
fn table_serde_roundtrip() {
    let t = sample_table();
    let json = serde_json::to_string(&t).unwrap();
    let back: Table = serde_json::from_str(&json).unwrap();
    assert_eq!(back, t);
}

#[test]
fn validate_requires_ebr_room() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    // Exactly 1 MiB for every EBR is enough.
    t.entries = vec![
        entry(1, 10 * MIB, 20 * MIB, 0x0F, false),
        entry(5, 11 * MIB, 4 * MIB, 0x07, true),
        entry(6, 16 * MIB, MIB, 0x07, true),
    ];
    t.validate().unwrap();
    let mut dev = MemDevice::new(DISK);
    write_mbr(&mut dev, &t).unwrap();
    // Legacy 63-sector gap after the extended start.
    t.entries[1].start = 10 * MIB + 63 * 512;
    match t.validate() {
        Err(Error::Invalid(m)) => assert!(m.contains("logical partition 5") && m.contains("EBR"), "{m}"),
        other => panic!("{other:?}"),
    }
    // Second logical with less than 1 MiB after the first (entries in any order).
    t.entries = vec![
        entry(6, 15 * MIB + 512 * 63, MIB, 0x07, true),
        entry(1, 10 * MIB, 20 * MIB, 0x0F, false),
        entry(5, 11 * MIB, 4 * MIB, 0x07, true),
    ];
    match t.validate() {
        Err(Error::Invalid(m)) => assert!(m.contains("logical partition 6"), "{m}"),
        other => panic!("{other:?}"),
    }
}
