use pm_core::gpt_types::*;
use pm_core::units::{GIB, MIB};
use pm_core::{Error, PartitionType, TableKind};
use pm_parttable::{align_table, gpt_to_mbr, mbr_to_gpt, Entry, Table};
use std::collections::HashSet;
use uuid::Uuid;

const DISK: u64 = 256 * MIB;

fn m(number: u32, start: u64, len: u64, t: u8, logical: bool) -> Entry {
    Entry {
        number,
        start,
        len,
        ptype: PartitionType::Mbr(t),
        guid: None,
        name: String::new(),
        attrs: 0,
        logical,
    }
}

fn g(number: u32, start: u64, len: u64, t: Uuid) -> Entry {
    Entry {
        number,
        start,
        len,
        ptype: PartitionType::Gpt(t),
        guid: Some(Uuid::new_v4()),
        name: format!("p{number}"),
        attrs: 0,
        logical: false,
    }
}

fn mbr_bytes(t: &Table) -> Vec<u8> {
    t.entries
        .iter()
        .map(|e| match e.ptype {
            PartitionType::Mbr(b) => b,
            _ => panic!("not an MBR type"),
        })
        .collect()
}

#[test]
fn mbr_to_gpt_maps_types() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.bootcode = vec![0xAB; 440];
    let mut active = m(1, MIB, 10 * MIB, 0x07, false);
    active.attrs = 1;
    t.entries = vec![
        active,
        m(2, 11 * MIB, 5 * MIB, 0xEF, false),
        m(3, 16 * MIB, 5 * MIB, 0x83, false),
        m(4, 30 * MIB, 100 * MIB, 0x0F, false), // extended container
        m(5, 32 * MIB, 4 * MIB, 0x0C, true),
        m(6, 40 * MIB, 4 * MIB, 0x82, true),
        m(7, 50 * MIB, 4 * MIB, 0x27, true),
        m(8, 60 * MIB, 4 * MIB, 0x8E, true),
        m(9, 70 * MIB, 4 * MIB, 0xAF, true),
        m(10, 80 * MIB, 4 * MIB, 0x42, true),
        m(11, 90 * MIB, 4 * MIB, 0x06, true),
    ];
    t.validate().unwrap();
    let out = mbr_to_gpt(&t).unwrap();
    assert_eq!(out.kind, TableKind::Gpt);
    assert_eq!(out.disk_len, DISK);
    assert!(out.disk_guid.is_some());
    assert_eq!(out.bootcode, t.bootcode);
    out.validate().unwrap();

    // Extended container dropped; 10 entries remain, renumbered 1..=10 by start.
    assert_eq!(out.entries.len(), 10);
    assert_eq!(
        out.entries.iter().map(|e| e.number).collect::<Vec<_>>(),
        (1..=10).collect::<Vec<u32>>()
    );
    assert!(out.entries.windows(2).all(|w| w[0].start < w[1].start));
    assert!(out.entries.iter().all(|e| !e.logical));
    let types: Vec<Uuid> = out
        .entries
        .iter()
        .map(|e| match e.ptype {
            PartitionType::Gpt(g) => g,
            _ => panic!("not GPT"),
        })
        .collect();
    assert_eq!(
        types,
        vec![
            BASIC_DATA,       // 0x07
            EFI_SYSTEM,       // 0xEF
            LINUX_FS,         // 0x83
            BASIC_DATA,       // 0x0C
            LINUX_SWAP,       // 0x82
            WINDOWS_RECOVERY, // 0x27
            LINUX_LVM,        // 0x8E
            APPLE_HFS,        // 0xAF
            BASIC_DATA,       // 0x42 unknown
            BASIC_DATA,       // 0x06
        ]
    );
    // Geometry preserved, GUIDs fresh and unique, MBR active flag does not become "required".
    assert_eq!((out.entries[0].start, out.entries[0].len), (MIB, 10 * MIB));
    assert_eq!(out.entries[0].attrs, 0);
    let guids: HashSet<Uuid> = out.entries.iter().map(|e| e.guid.unwrap()).collect();
    assert_eq!(guids.len(), 10);
    // The original type byte survives in the name only for unmapped types.
    assert_eq!(out.entries[8].name, "mbr-0x42");
    assert_eq!(out.entries[0].name, "");
    assert_eq!(out.entries[9].name, "");
    // Input is untouched.
    assert_eq!(t.entries.len(), 11);
}

#[test]
fn mbr_to_gpt_checks_room() {
    // Layout starting at sector 63 is fine: the usable range starts right after the entry array.
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.entries = vec![m(1, 63 * 512, 10 * MIB, 0x07, false)];
    let out = mbr_to_gpt(&t).unwrap();
    assert!(out.gpt_first_usable <= 63 * 512);
    out.validate().unwrap();

    // Partition that runs into the backup GPT area.
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.entries = vec![m(1, MIB, DISK - MIB, 0x07, false)];
    assert!(matches!(mbr_to_gpt(&t), Err(Error::Invalid(_))));

    // Partition inside the primary GPT area.
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.entries = vec![m(1, 512 * 10, 10 * MIB, 0x07, false)];
    assert!(matches!(mbr_to_gpt(&t), Err(Error::Invalid(_))));

    // Wrong source kind.
    let gpt = Table::empty(TableKind::Gpt, DISK, 512);
    assert!(matches!(mbr_to_gpt(&gpt), Err(Error::Invalid(_))));

    // More than 128 partitions cannot fit (MBR logicals can exceed this).
    let mut t = Table::empty(TableKind::Mbr, 1024 * MIB, 512);
    t.entries = vec![m(1, MIB, 1000 * MIB, 0x0F, false)];
    for i in 0..129u32 {
        t.entries.push(m(5 + i, (2 + 2 * i as u64) * MIB, MIB, 0x07, true));
    }
    assert!(matches!(mbr_to_gpt(&t), Err(Error::Invalid(_))));
}

#[test]
fn mbr_to_gpt_carries_gpt_types_unchanged() {
    // A (malformed) GPT type inside an MBR table is carried over unchanged.
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    let mut e = m(1, MIB, 10 * MIB, 0x07, false);
    e.ptype = PartitionType::Gpt(MSR);
    t.entries = vec![e];
    let out = mbr_to_gpt(&t).unwrap();
    assert_eq!(out.entries[0].ptype, PartitionType::Gpt(MSR));
}

#[test]
fn gpt_to_mbr_maps_types_and_drops_msr() {
    let mut t = Table::empty(TableKind::Gpt, DISK, 512);
    t.bootcode = vec![0x11; 440];
    let mut efi = g(1, MIB, 10 * MIB, EFI_SYSTEM);
    efi.attrs = 1;
    t.entries = vec![
        efi,
        g(2, 11 * MIB, 16 * MIB, MSR),
        g(3, 30 * MIB, 20 * MIB, BASIC_DATA),
        g(4, 60 * MIB, 20 * MIB, LINUX_SWAP),
        g(5, 90 * MIB, 20 * MIB, LINUX_FS),
    ];
    let out = gpt_to_mbr(&t).unwrap();
    assert_eq!(out.kind, TableKind::Mbr);
    assert_eq!(out.bootcode, t.bootcode);
    assert_ne!(out.mbr_signature, 0);
    out.validate().unwrap();
    assert_eq!(out.entries.len(), 4);
    assert_eq!(out.entries.iter().map(|e| e.number).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert_eq!(mbr_bytes(&out), vec![0xEF, 0x07, 0x82, 0x83]);
    assert!(out
        .entries
        .iter()
        .all(|e| e.attrs == 0 && e.guid.is_none() && e.name.is_empty() && !e.logical));
    assert_eq!((out.entries[1].start, out.entries[1].len), (30 * MIB, 20 * MIB));

    // Other GUIDs map to 0x07; recovery / LVM / HFS map inversely.
    let mut t2 = Table::empty(TableKind::Gpt, DISK, 512);
    t2.entries = vec![
        g(1, MIB, 10 * MIB, WINDOWS_RECOVERY),
        g(2, 20 * MIB, 10 * MIB, LINUX_LVM),
        g(3, 40 * MIB, 10 * MIB, APPLE_HFS),
        g(4, 60 * MIB, 10 * MIB, Uuid::new_v4()),
    ];
    assert_eq!(mbr_bytes(&gpt_to_mbr(&t2).unwrap()), vec![0x27, 0x8E, 0xAF, 0x07]);
}

#[test]
fn gpt_to_mbr_rejects_five_entries() {
    let mut t = Table::empty(TableKind::Gpt, DISK, 512);
    t.entries = (0..5u32)
        .map(|i| g(i + 1, MIB + i as u64 * 20 * MIB, 10 * MIB, BASIC_DATA))
        .collect();
    assert!(matches!(gpt_to_mbr(&t), Err(Error::Invalid(_))));
    // Four plus an MSR is fine.
    t.entries[4].ptype = PartitionType::Gpt(MSR);
    gpt_to_mbr(&t).unwrap();
    // Wrong source kind.
    let mbr = Table::empty(TableKind::Mbr, DISK, 512);
    assert!(matches!(gpt_to_mbr(&mbr), Err(Error::Invalid(_))));
}

#[test]
fn gpt_to_mbr_rejects_disks_over_2_tib() {
    let len = 2 * 1024 * GIB + MIB;
    let mut t = Table::empty(TableKind::Gpt, len, 512);
    t.entries = vec![g(1, MIB, 10 * MIB, BASIC_DATA)];
    assert!(matches!(gpt_to_mbr(&t), Err(Error::Invalid(_))));

    // Exactly 2 TiB is fine unless an end LBA does not fit 32 bits.
    let len = 2 * 1024 * GIB;
    let mut t = Table::empty(TableKind::Gpt, len, 512);
    t.entries = vec![g(1, MIB, 10 * MIB, BASIC_DATA)];
    gpt_to_mbr(&t).unwrap();
    t.entries = vec![g(1, len - 2 * MIB, 2 * MIB, BASIC_DATA)];
    t.gpt_last_usable = len; // pretend the partition fits the GPT range
    assert!(matches!(gpt_to_mbr(&t), Err(Error::Invalid(_))));
}

#[test]
fn align_table_moves_starts() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.entries = vec![
        m(1, 63 * 512, 10 * MIB, 0x07, false),              // starts at 31.5 KiB
        m(2, 12 * MIB + 512, 20 * MIB + 1024, 0x83, false), // start up, end down
        m(3, 40 * MIB, 8 * MIB, 0x07, false),               // already aligned
        m(4, 60 * MIB + 512, MIB / 2, 0x07, false),         // too small after aligning: unchanged
    ];
    let out = align_table(&t, MIB);
    assert_eq!(out.entries.len(), 4);
    // end = 63*512 + 10 MiB aligns down to 10 MiB, start up to 1 MiB.
    assert_eq!((out.entries[0].start, out.entries[0].len), (MIB, 9 * MIB));
    assert_eq!((out.entries[1].start, out.entries[1].len), (13 * MIB, 19 * MIB));
    assert_eq!((out.entries[2].start, out.entries[2].len), (40 * MIB, 8 * MIB));
    assert_eq!(out.entries[3], t.entries[3]);
    assert_eq!(out.entries.iter().map(|e| e.number).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert_eq!(out.kind, t.kind);
    assert_eq!(out.mbr_signature, t.mbr_signature);
    // Input untouched; idempotent; zero alignment is a no-op.
    assert_eq!(t.entries[0].start, 63 * 512);
    assert_eq!(align_table(&out, MIB), out);
    assert_eq!(align_table(&t, 0), t);
}

#[test]
fn align_table_leaves_extended_container_alone() {
    let mut t = Table::empty(TableKind::Mbr, DISK, 512);
    t.entries = vec![
        m(1, 10 * MIB + 512, 100 * MIB, 0x0F, false),
        m(5, 12 * MIB, 4 * MIB, 0x07, true),
    ];
    let out = align_table(&t, MIB);
    assert_eq!(out.entries[0], t.entries[0]);
    assert_eq!(out.entries[1], t.entries[1]);
}
