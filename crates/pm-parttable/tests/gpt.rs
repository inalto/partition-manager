use pm_core::gpt_types::{BASIC_DATA, EFI_SYSTEM, LINUX_FS};
use pm_core::units::MIB;
use pm_core::{BlockDevice, DiskInfo, Error, MemDevice, PartitionType, Result, TableKind};
use pm_parttable::{
    gpt_health, gpt_repair_backup_from_primary, gpt_repair_primary_from_backup, is_protective_mbr,
    parse_gpt, read_table, write_gpt, write_table, Entry, GptHealth, Table,
};
use uuid::Uuid;

const DISK: u64 = 64 * MIB;
const ENTRY_ARRAY: usize = 128 * 128;

fn gentry(number: u32, start: u64, len: u64, ptype: Uuid, name: &str, attrs: u64) -> Entry {
    Entry {
        number,
        start,
        len,
        ptype: PartitionType::Gpt(ptype),
        guid: Some(Uuid::new_v4()),
        name: name.to_string(),
        attrs,
        logical: false,
    }
}

fn sample_table(ss: u32) -> Table {
    let mut t = Table::empty(TableKind::Gpt, DISK, ss);
    t.entries = vec![
        gentry(1, MIB, 16 * MIB, EFI_SYSTEM, "EFI system", 1),
        gentry(2, 17 * MIB, 20 * MIB, BASIC_DATA, "Data", 0),
        gentry(4, 40 * MIB, 10 * MIB, LINUX_FS, "Linux", (1 << 62) | (1 << 63)),
    ];
    t
}

fn w32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn w64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}

/// Independently built expected entry array (128 x 128 bytes).
fn expected_entries(t: &Table) -> Vec<u8> {
    let ss = t.sector_size as u64;
    let mut arr = vec![0u8; ENTRY_ARRAY];
    for e in &t.entries {
        let o = (e.number as usize - 1) * 128;
        let PartitionType::Gpt(tg) = e.ptype else { panic!() };
        arr[o..o + 16].copy_from_slice(&tg.to_bytes_le());
        arr[o + 16..o + 32].copy_from_slice(&e.guid.unwrap().to_bytes_le());
        arr[o + 32..o + 40].copy_from_slice(&(e.start / ss).to_le_bytes());
        arr[o + 40..o + 48].copy_from_slice(&((e.start + e.len) / ss - 1).to_le_bytes());
        arr[o + 48..o + 56].copy_from_slice(&e.attrs.to_le_bytes());
        for (i, u) in e.name.encode_utf16().enumerate() {
            arr[o + 56 + i * 2..o + 58 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    arr
}

/// Independently built 92-byte header.
fn expected_header(t: &Table, my: u64, alt: u64, entries_lba: u64) -> Vec<u8> {
    let ss = t.sector_size as u64;
    let mut h = Vec::new();
    h.extend_from_slice(b"EFI PART");
    w32(&mut h, 0x0001_0000);
    w32(&mut h, 92);
    w32(&mut h, 0); // crc placeholder
    w32(&mut h, 0);
    w64(&mut h, my);
    w64(&mut h, alt);
    w64(&mut h, t.gpt_first_usable / ss);
    w64(&mut h, t.gpt_last_usable / ss - 1);
    h.extend_from_slice(&t.disk_guid.unwrap().to_bytes_le());
    w64(&mut h, entries_lba);
    w32(&mut h, 128);
    w32(&mut h, 128);
    w32(&mut h, crc32fast::hash(&expected_entries(t)));
    assert_eq!(h.len(), 92);
    let crc = crc32fast::hash(&h);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    h
}

fn verify_raw(img: &[u8], t: &Table) {
    let ss = t.sector_size as usize;
    let last = img.len() / ss - 1;
    let entry_sectors = ENTRY_ARRAY / ss;
    let arr = expected_entries(t);

    let prim = expected_header(t, 1, last as u64, 2);
    assert_eq!(&img[ss..ss + 8], b"EFI PART");
    assert_eq!(&img[ss..ss + 92], &prim[..]);
    assert!(img[ss + 92..2 * ss].iter().all(|&b| b == 0));
    assert_eq!(&img[2 * ss..2 * ss + ENTRY_ARRAY], &arr[..]);

    let back_entries_lba = last - entry_sectors;
    let back = expected_header(t, last as u64, 1, back_entries_lba as u64);
    assert_eq!(&img[last * ss..last * ss + 92], &back[..]);
    assert!(img[last * ss + 92..(last + 1) * ss].iter().all(|&b| b == 0));
    assert_eq!(&img[back_entries_lba * ss..back_entries_lba * ss + ENTRY_ARRAY], &arr[..]);
}

fn roundtrip(ss: u32) {
    let mut t = sample_table(ss);
    t.bootcode = vec![0; 440];
    let mut dev = MemDevice::new(DISK).with_sector_size(ss);
    write_gpt(&mut dev, &t).unwrap();
    verify_raw(dev.as_slice(), &t);
    assert_eq!(parse_gpt(&mut dev).unwrap(), t);
    assert_eq!(read_table(&mut dev).unwrap(), t);
}

#[test]
fn gpt_roundtrip_512() {
    roundtrip(512);
}

#[test]
fn gpt_roundtrip_4k_sectors() {
    roundtrip(4096);
    // Spot-check the 4K layout explicitly: header at 4096, entries at 8192.
    let t = sample_table(4096);
    let mut dev = MemDevice::new(DISK).with_sector_size(4096);
    write_gpt(&mut dev, &t).unwrap();
    let img = dev.as_slice();
    assert_eq!(&img[4096..4104], b"EFI PART");
    assert_eq!(&img[8192..8208], &EFI_SYSTEM.to_bytes_le());
    let last = img.len() - 4096;
    assert_eq!(&img[last..last + 8], b"EFI PART");
    assert_eq!(t.gpt_first_usable, MIB);
}

#[test]
fn gpt_uses_backup_when_primary_corrupt() {
    let mut t = sample_table(512);
    t.bootcode = vec![0; 440];
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    dev.write_at(512, &[0u8; 512]).unwrap();
    assert_eq!(parse_gpt(&mut dev).unwrap(), t);
    assert_eq!(read_table(&mut dev).unwrap(), t);

    // Corrupt entries of the primary instead (header CRC still fine, array CRC fails).
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    let mut b = [0u8; 1];
    dev.read_at(1024 + 5, &mut b).unwrap();
    dev.write_at(1024 + 5, &[b[0] ^ 0xFF]).unwrap();
    assert_eq!(parse_gpt(&mut dev).unwrap(), t);
}

#[test]
fn gpt_rejects_both_corrupt() {
    let t = sample_table(512);
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    dev.write_at(512, &[0u8; 512]).unwrap();
    dev.write_at(DISK - 512, &[0u8; 512]).unwrap();
    assert!(matches!(parse_gpt(&mut dev), Err(Error::Parse(_))));
    // Protective MBR remains, so read_table reports the same parse failure.
    assert!(matches!(read_table(&mut dev), Err(Error::Parse(_))));
}

#[test]
fn gpt_header_crc_is_checked() {
    let t = sample_table(512);
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    // Flip a bit in the primary's FirstUsableLBA (CRC no longer matches) and in the backup's.
    for off in [512u64 + 40, DISK - 512 + 40] {
        let mut b = [0u8; 1];
        dev.read_at(off, &mut b).unwrap();
        dev.write_at(off, &[b[0] ^ 1]).unwrap();
    }
    assert!(matches!(parse_gpt(&mut dev), Err(Error::Parse(_))));
}

#[test]
fn gpt_protective_mbr_written() {
    let mut t = sample_table(512);
    let mut code = vec![0u8; 440];
    for (i, b) in code.iter_mut().enumerate() {
        *b = (i % 251) as u8 | 1;
    }
    t.bootcode = code.clone();
    t.mbr_signature = 0xCAFE_F00D;
    let mut dev = MemDevice::new(DISK);
    // Pre-existing junk in the partition-entry area must be replaced.
    dev.write_at(0x1BE + 16, &[0x07; 16]).unwrap();
    write_gpt(&mut dev, &t).unwrap();
    let s0 = &dev.as_slice()[..512];
    assert_eq!(&s0[..440], &code[..]);
    assert_eq!(&s0[440..444], &0xCAFE_F00Du32.to_le_bytes());
    assert_eq!(&s0[444..446], &[0, 0]);
    let sectors = (DISK / 512 - 1) as u32;
    let mut e = vec![0x00, 0x00, 0x02, 0x00, 0xEE, 0xFF, 0xFF, 0xFF, 1, 0, 0, 0];
    e.extend_from_slice(&sectors.to_le_bytes());
    assert_eq!(&s0[0x1BE..0x1BE + 16], &e[..]);
    assert!(s0[0x1BE + 16..510].iter().all(|&b| b == 0));
    assert_eq!(&s0[510..], &[0x55, 0xAA]);
    assert!(is_protective_mbr(s0));

    // Rewriting keeps the boot code that parse_gpt picked up.
    let back = parse_gpt(&mut dev).unwrap();
    assert_eq!(back.bootcode, code);
    assert_eq!(back.mbr_signature, 0xCAFE_F00D);
}

/// Device reporting a huge size; remembers writes only (reads of the rest are zero).
struct Sparse {
    len: u64,
    writes: Vec<(u64, Vec<u8>)>,
}

impl BlockDevice for Sparse {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        buf.fill(0);
        for (o, d) in &self.writes {
            let (lo, hi) = ((*o).max(offset), (*o + d.len() as u64).min(offset + buf.len() as u64));
            if lo < hi {
                buf[(lo - offset) as usize..(hi - offset) as usize]
                    .copy_from_slice(&d[(lo - o) as usize..(hi - o) as usize]);
            }
        }
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.writes.push((offset, buf.to_vec()));
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

#[test]
fn gpt_protective_mbr_clamps_on_huge_disk() {
    let len = 3u64 << 40;
    let mut t = Table::empty(TableKind::Gpt, len, 512);
    t.entries = vec![gentry(1, MIB, 100 * MIB, BASIC_DATA, "big", 0)];
    let mut dev = Sparse { len, writes: Vec::new() };
    write_gpt(&mut dev, &t).unwrap();
    let mut s0 = [0u8; 512];
    dev.read_at(0, &mut s0).unwrap();
    assert_eq!(&s0[0x1BE + 12..0x1BE + 16], &0xFFFF_FFFFu32.to_le_bytes());
    let back = parse_gpt(&mut dev).unwrap();
    assert_eq!(back.entries, t.entries);
    assert_eq!(back.gpt_last_usable, t.gpt_last_usable);
}

#[test]
fn gpt_names_utf16() {
    let mut t = sample_table(512);
    t.entries[1].name = "Données \u{1F600} 日本".to_string();
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    let o = 1024 + 128 + 56;
    let units: Vec<u16> = t.entries[1].name.encode_utf16().collect();
    let mut expect = Vec::new();
    for u in &units {
        expect.extend_from_slice(&u.to_le_bytes());
    }
    assert_eq!(&dev.as_slice()[o..o + expect.len()], &expect[..]);
    assert!(dev.as_slice()[o + expect.len()..o + 72].iter().all(|&b| b == 0));
    assert_eq!(parse_gpt(&mut dev).unwrap().entries[1].name, t.entries[1].name);

    // Exactly 36 code units fit; 37 do not.
    t.entries[1].name = "x".repeat(36);
    write_gpt(&mut dev, &t).unwrap();
    assert_eq!(parse_gpt(&mut dev).unwrap().entries[1].name, "x".repeat(36));
    t.entries[1].name = "x".repeat(37);
    assert!(matches!(write_gpt(&mut dev, &t), Err(Error::Invalid(_))));
}

#[test]
fn gpt_numbers_follow_slots() {
    let mut t = sample_table(512);
    t.entries[2].number = 128;
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    let back = parse_gpt(&mut dev).unwrap();
    assert_eq!(back.entries.iter().map(|e| e.number).collect::<Vec<_>>(), vec![1, 2, 128]);
    t.entries[2].number = 129;
    assert!(matches!(write_gpt(&mut dev, &t), Err(Error::Invalid(_))));
    t.entries[2].number = 0;
    assert!(matches!(write_gpt(&mut dev, &t), Err(Error::Invalid(_))));
    t.entries[2].number = 2; // duplicate
    assert!(matches!(write_gpt(&mut dev, &t), Err(Error::Invalid(_))));
}

#[test]
fn gpt_empty_table_defaults_and_roundtrip() {
    let t = Table::empty(TableKind::Gpt, DISK, 512);
    assert_eq!(t.gpt_first_usable, MIB);
    assert_eq!(t.gpt_last_usable, DISK - 33 * 512);
    let mut dev = MemDevice::new(DISK);
    write_table(&mut dev, &t).unwrap();
    let back = read_table(&mut dev).unwrap();
    assert_eq!(back.kind, TableKind::Gpt);
    assert_eq!(back.disk_guid, t.disk_guid);
    assert_eq!(back.gpt_last_usable, t.gpt_last_usable);
    assert!(back.entries.is_empty());
    let t4 = Table::empty(TableKind::Gpt, DISK, 4096);
    assert_eq!(t4.gpt_first_usable, MIB);
    assert_eq!(t4.gpt_last_usable, DISK - 33 * 4096);
}

#[test]
fn gpt_validate_rules() {
    let bad = |t: &Table| assert!(matches!(t.validate(), Err(Error::Invalid(_))), "{:?}", t.entries);
    let ok = sample_table(512);
    ok.validate().unwrap();

    let mut t = ok.clone();
    t.entries[0].logical = true;
    bad(&t);

    let mut t = ok.clone();
    t.entries[0].start = 512 * 10; // below first usable
    bad(&t);

    let mut t = ok.clone();
    t.entries[2].start = DISK - MIB; // runs into the backup area
    t.entries[2].len = MIB;
    bad(&t);

    let mut t = Table::empty(TableKind::Gpt, 1024 * MIB, 512);
    t.entries = (0..129u64)
        .map(|i| gentry(i as u32 + 1, MIB + i * MIB, MIB, BASIC_DATA, "", 0))
        .collect();
    bad(&t);
    t.entries.pop();
    t.validate().unwrap();
}

#[test]
fn gpt_to_disk_info_maps_attribute_flags() {
    let t = sample_table(512);
    let info = t.to_disk_info(DiskInfo::default());
    assert_eq!(info.table, TableKind::Gpt);
    assert!(info.partitions[0].flags.required);
    assert!(!info.partitions[0].flags.active);
    assert!(!info.partitions[1].flags.required);
    assert!(info.partitions[2].flags.hidden);
    assert!(info.partitions[2].flags.no_letter);
    assert!(!info.partitions[2].flags.read_only);
    let mut t2 = sample_table(512);
    t2.entries[1].attrs = 1 << 60;
    assert!(t2.to_disk_info(DiskInfo::default()).partitions[1].flags.read_only);
}

#[test]
fn gpt_health_reports_each_side() {
    let t = sample_table(512);
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    assert_eq!(
        gpt_health(&mut dev).unwrap(),
        GptHealth { primary_ok: true, backup_ok: true, entries_crc_ok: true }
    );

    let mut d = MemDevice::from_vec(dev.as_slice().to_vec());
    d.write_at(512, &[0u8; 512]).unwrap();
    let h = gpt_health(&mut d).unwrap();
    assert!(!h.primary_ok && h.backup_ok && h.entries_crc_ok);

    let mut d = MemDevice::from_vec(dev.as_slice().to_vec());
    d.write_at(DISK - 512, &[0u8; 512]).unwrap();
    let h = gpt_health(&mut d).unwrap();
    assert!(h.primary_ok && !h.backup_ok && h.entries_crc_ok);

    // Primary entry array damaged: header valid but array CRC mismatches.
    let mut d = MemDevice::from_vec(dev.as_slice().to_vec());
    d.write_at(1024 + 40, &[0xAB]).unwrap();
    let h = gpt_health(&mut d).unwrap();
    assert!(!h.primary_ok && h.backup_ok && !h.entries_crc_ok);

    let mut d = MemDevice::new(DISK);
    assert_eq!(
        gpt_health(&mut d).unwrap(),
        GptHealth { primary_ok: false, backup_ok: false, entries_crc_ok: false }
    );
}

#[test]
fn gpt_repairs_either_side() {
    let mut t = sample_table(512);
    t.bootcode = vec![0; 440];
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    let good = dev.as_slice().to_vec();

    // Lost backup (e.g. the tail of the disk was blanked).
    dev.write_at(DISK - 512, &[0u8; 512]).unwrap();
    dev.write_at(DISK - 512 - ENTRY_ARRAY as u64, &vec![0u8; ENTRY_ARRAY]).unwrap();
    assert!(!gpt_health(&mut dev).unwrap().backup_ok);
    gpt_repair_backup_from_primary(&mut dev).unwrap();
    assert_eq!(dev.as_slice(), &good[..]);

    // Lost primary.
    dev.write_at(512, &[0u8; 512]).unwrap();
    dev.write_at(1024, &vec![0u8; ENTRY_ARRAY]).unwrap();
    assert!(!gpt_health(&mut dev).unwrap().primary_ok);
    gpt_repair_primary_from_backup(&mut dev).unwrap();
    assert_eq!(dev.as_slice(), &good[..]);

    // Repairing from a side that is itself bad is an error and writes nothing.
    dev.write_at(512, &[0u8; 512]).unwrap();
    let before = dev.as_slice().to_vec();
    assert!(matches!(gpt_repair_backup_from_primary(&mut dev), Err(Error::Parse(_))));
    assert_eq!(dev.as_slice(), &before[..]);
}

#[test]
fn read_table_finds_gpt_without_mbr_signature() {
    let mut t = sample_table(512);
    t.bootcode = vec![0; 440];
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    dev.write_at(0, &[0u8; 512]).unwrap(); // a tool wiped only the MBR
    let back = read_table(&mut dev).unwrap();
    assert_eq!(back.kind, TableKind::Gpt);
    assert_eq!(back.entries, t.entries);
}

#[test]
fn write_gpt_rejects_bad_input() {
    let mut dev = MemDevice::new(DISK);
    let mbr = Table::empty(TableKind::Mbr, DISK, 512);
    assert!(matches!(write_gpt(&mut dev, &mbr), Err(Error::Invalid(_))));
    let wrong_ss = sample_table(4096);
    assert!(matches!(write_gpt(&mut dev, &wrong_ss), Err(Error::Invalid(_))));
    let mut t = sample_table(512);
    t.entries[0].guid = None;
    assert!(matches!(write_gpt(&mut dev, &t), Err(Error::Invalid(_))));
    let mut t = sample_table(512);
    t.entries[0].ptype = PartitionType::Mbr(7);
    assert!(matches!(write_gpt(&mut dev, &t), Err(Error::Invalid(_))));
    let mut tiny = MemDevice::new(16 * 512);
    let t = Table::empty(TableKind::Gpt, 16 * 512, 512);
    assert!(matches!(write_gpt(&mut tiny, &t), Err(Error::Invalid(_))));
    assert!(dev.as_slice().iter().all(|&b| b == 0), "nothing written on error");
}

// ---- header validation and repair of unwritable sources ----

const BACKUP_ARRAY_OFF: u64 = DISK - 512 - ENTRY_ARRAY as u64;

/// Apply `f` to the header sector at `off`, then recompute its CRC (over header_size, clamped
/// to the sector) so only the patched field is wrong.
fn patch_header(dev: &mut MemDevice, off: u64, f: &dyn Fn(&mut [u8])) {
    let mut buf = vec![0u8; 512];
    dev.read_at(off, &mut buf).unwrap();
    f(&mut buf);
    let n = (u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize).clamp(20, 512);
    buf[16..20].fill(0);
    let crc = crc32fast::hash(&buf[..n]);
    buf[16..20].copy_from_slice(&crc.to_le_bytes());
    dev.write_at(off, &buf).unwrap();
}

/// Apply `f` to the entry array at `array_off`, then fix the array CRC in the header at
/// `header_off` and the header CRC.
fn patch_array(dev: &mut MemDevice, array_off: u64, header_off: u64, f: &dyn Fn(&mut [u8])) {
    let mut arr = vec![0u8; ENTRY_ARRAY];
    dev.read_at(array_off, &mut arr).unwrap();
    f(&mut arr);
    dev.write_at(array_off, &arr).unwrap();
    let crc = crc32fast::hash(&arr);
    patch_header(dev, header_off, &|h| h[88..92].copy_from_slice(&crc.to_le_bytes()));
}

fn valid_dev() -> (MemDevice, Table) {
    let mut t = sample_table(512);
    t.bootcode = vec![0; 440];
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    (dev, t)
}

#[test]
fn gpt_rejects_bad_revision() {
    let (mut dev, t) = valid_dev();
    let bad_rev = |h: &mut [u8]| h[8..12].copy_from_slice(&0x0002_0000u32.to_le_bytes());
    patch_header(&mut dev, 512, &bad_rev);
    assert_eq!(parse_gpt(&mut dev).unwrap(), t, "backup rescues a bad primary revision");
    assert!(!gpt_health(&mut dev).unwrap().primary_ok);
    patch_header(&mut dev, DISK - 512, &bad_rev);
    assert!(matches!(parse_gpt(&mut dev), Err(Error::Parse(_))));
}

#[test]
fn gpt_repair_refuses_invalid_source() {
    let (mut dev, _t) = valid_dev();
    // Make partition 2 start inside partition 1 on both copies; every CRC stays valid.
    let overlap = |a: &mut [u8]| {
        let first = u64::from_le_bytes(a[32..40].try_into().unwrap());
        a[128 + 32..128 + 40].copy_from_slice(&(first + 10).to_le_bytes());
    };
    patch_array(&mut dev, 1024, 512, &overlap);
    patch_array(&mut dev, BACKUP_ARRAY_OFF, DISK - 512, &overlap);
    let h = gpt_health(&mut dev).unwrap();
    assert!(h.primary_ok && h.backup_ok && h.entries_crc_ok, "readable");
    assert!(parse_gpt(&mut dev).unwrap().validate().is_err());

    let before = dev.as_slice().to_vec();
    for result in [
        gpt_repair_backup_from_primary(&mut dev),
        gpt_repair_primary_from_backup(&mut dev),
    ] {
        match result {
            Err(Error::Invalid(m)) => assert!(m.contains("readable but not writable"), "{m}"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
    assert_eq!(dev.as_slice(), &before[..], "device untouched");
}

#[test]
fn gpt_corrupt_headers_never_panic() {
    type Patch = Box<dyn Fn(&mut [u8])>;
    fn set32(off: usize, v: u32) -> Patch {
        Box::new(move |h| h[off..off + 4].copy_from_slice(&v.to_le_bytes()))
    }
    fn set64(off: usize, v: u64) -> Patch {
        Box::new(move |h| h[off..off + 8].copy_from_slice(&v.to_le_bytes()))
    }
    let header_cases: Vec<(&str, Patch)> = vec![
        ("num_entries = u32::MAX", set32(80, u32::MAX)),
        ("num_entries = 0", set32(80, 0)),
        ("entry_size = u32::MAX", set32(84, u32::MAX)),
        ("entry_size = 100", set32(84, 100)),
        ("entries LBA beyond device", set64(72, DISK / 512 + 1000)),
        ("entries LBA overflows", set64(72, u64::MAX)),
        ("header_size = 4000", set32(12, 4000)),
        ("header_size = 50", set32(12, 50)),
        ("last usable beyond device", set64(48, u64::MAX)),
        ("first usable > last usable", set64(40, DISK / 512)),
    ];
    for (name, patch) in &header_cases {
        let (mut dev, t) = valid_dev();
        patch_header(&mut dev, 512, patch.as_ref());
        assert_eq!(parse_gpt(&mut dev).unwrap(), t, "{name}: backup rescues the table");
        assert!(read_table(&mut dev).is_ok(), "{name}");
        patch_header(&mut dev, DISK - 512, patch.as_ref());
        assert!(matches!(parse_gpt(&mut dev), Err(Error::Parse(_))), "{name}: both bad");
        assert!(gpt_health(&mut dev).is_ok(), "{name}");
    }

    // An entry whose LastLBA is below its FirstLBA (CRCs recomputed).
    let (mut dev, t) = valid_dev();
    let bad_entry = |a: &mut [u8]| {
        let first = u64::from_le_bytes(a[32..40].try_into().unwrap());
        a[40..48].copy_from_slice(&(first - 1).to_le_bytes());
    };
    patch_array(&mut dev, 1024, 512, &bad_entry);
    assert_eq!(parse_gpt(&mut dev).unwrap(), t, "backup rescues a bad primary entry");
    patch_array(&mut dev, BACKUP_ARRAY_OFF, DISK - 512, &bad_entry);
    assert!(matches!(parse_gpt(&mut dev), Err(Error::Parse(_))));
    assert!(gpt_health(&mut dev).is_ok());
}
