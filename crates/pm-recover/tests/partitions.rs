mod common;

use common::*;
use pm_core::units::MIB;
use pm_core::{
    gpt_types, BlockDevice, CancelToken, Error, FsKind, MemDevice, NullProgress, PartitionType,
    ReadOnly, TableKind,
};
use pm_parttable::{read_table, write_table, Candidate, CandidateSource, Entry, Table};
use pm_recover::{apply_candidates, find_partitions};

fn spans(t: &Table) -> Vec<(u64, u64)> {
    let mut v: Vec<_> = t.entries.iter().map(|e| (e.start, e.len)).collect();
    v.sort();
    v
}

fn cand(start: u64, len: u64, fs: FsKind) -> Candidate {
    Candidate {
        start,
        len,
        fs,
        label: "X".into(),
        source: CandidateSource::BootSector,
        confidence: 70,
    }
}

#[test]
fn find_partitions_after_table_wiped() {
    let (mut dev, orig) = gpt_two_fat32();
    wipe_primary(&mut dev);
    wipe_backup(&mut dev);
    // the table really is gone
    assert!(read_table(&mut dev).unwrap().entries.is_empty());

    let found = find_partitions(&mut dev, false, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(found.len(), 2, "{found:?}");
    for (c, (s, l)) in found.iter().zip([P1, P2]) {
        assert_eq!(c.start, s);
        assert!(c.len.abs_diff(l) <= 512, "len {} vs {l}", c.len);
        assert_eq!(c.fs, FsKind::Fat32);
        assert!(c.confidence >= 70, "{}", c.confidence);
        assert_eq!(c.source, CandidateSource::BootSector);
    }
    assert_eq!(found[0].label, "ONE");
    assert_eq!(found[1].label, "TWO");

    // apply to an empty GPT, write, read back: same starts and lens as the original table
    let empty = Table::empty(TableKind::Gpt, dev.len(), 512);
    let rebuilt = apply_candidates(&empty, &found).unwrap();
    assert_eq!(rebuilt.entries.len(), 2);
    assert_eq!(rebuilt.entries.iter().map(|e| e.number).collect::<Vec<_>>(), vec![1, 2]);
    for e in &rebuilt.entries {
        assert_eq!(e.ptype, PartitionType::Gpt(gpt_types::BASIC_DATA));
        assert!(e.guid.is_some());
    }
    assert_eq!(rebuilt.entries[0].name, "ONE");
    write_table(&mut dev, &rebuilt).unwrap();
    let back = read_table(&mut dev).unwrap();
    assert_eq!(back.kind, TableKind::Gpt);
    // the filesystem length equals the partition length here
    assert_eq!(spans(&back), spans(&orig));
}

#[test]
fn find_partitions_quick_uses_backup_gpt() {
    let (mut dev, orig) = gpt_two_fat32();
    wipe_primary(&mut dev);
    let found = find_partitions(&mut dev, true, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(found.len(), 2, "{found:?}");
    for (c, e) in found.iter().zip(&orig.entries) {
        assert_eq!((c.start, c.len), (e.start, e.len));
        assert_eq!(c.fs, FsKind::Fat32);
        assert_eq!(c.source, CandidateSource::GptBackupEntry);
        assert_eq!(c.confidence, 90);
    }
}

#[test]
fn find_partitions_quick_probes_free_ranges_when_backup_is_gone() {
    let (mut dev, _) = gpt_two_fat32();
    wipe_primary(&mut dev);
    wipe_backup(&mut dev);
    let found = find_partitions(&mut dev, true, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(found.iter().map(|c| c.start).collect::<Vec<_>>(), vec![P1.0, P2.0]);
}

#[test]
fn existing_partitions_are_not_candidates() {
    let (mut dev, orig) = gpt_two_fat32();
    for quick in [true, false] {
        let found = find_partitions(&mut dev, quick, &NullProgress, &CancelToken::new()).unwrap();
        assert!(found.is_empty(), "quick={quick}: {found:?}");
    }
    // delete the second entry from the table: only it comes back, from both modes
    let mut t = orig.clone();
    t.entries.pop();
    write_table(&mut dev, &t).unwrap();
    for quick in [true, false] {
        let found = find_partitions(&mut dev, quick, &NullProgress, &CancelToken::new()).unwrap();
        assert_eq!(found.len(), 1, "quick={quick}: {found:?}");
        assert_eq!(found[0].start, P2.0);
        assert_eq!(found[0].label, "TWO");
    }
}

#[test]
fn candidate_longer_than_gap_is_capped_and_penalised() {
    let mut dev = MemDevice::new(DISK_LEN);
    // a FAT32 volume of 40 MiB at 41 MiB, but the table has a partition starting at 61 MiB
    format_part(&mut dev, P2.0, P2.1, "TWO");
    let mut t = Table::empty(TableKind::Gpt, DISK_LEN, 512);
    t.entries.push(Entry {
        number: 1,
        start: 61 * MIB,
        len: 30 * MIB,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        guid: Some(uuid::Uuid::new_v4()),
        name: "blocker".into(),
        attrs: 0,
        logical: false,
    });
    write_table(&mut dev, &t).unwrap();
    // the surviving backup GPT lists the blocking entry, which must not hide the candidate
    let found = find_partitions(&mut dev, true, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!((found[0].start, found[0].len, found[0].confidence), (41 * MIB, 20 * MIB, 50));
    // a full scan lets the backup entry win any overlap (search_lost keeps the most confident
    // candidate), so check it without a backup copy
    wipe_backup(&mut dev);
    for quick in [true, false] {
        let found = find_partitions(&mut dev, quick, &NullProgress, &CancelToken::new()).unwrap();
        assert_eq!(found.len(), 1, "quick={quick}: {found:?}");
        assert_eq!(found[0].start, 41 * MIB);
        assert_eq!(found[0].len, 20 * MIB);
        assert_eq!(found[0].confidence, 50);
    }
}

#[test]
fn find_partitions_never_writes_and_honours_cancel() {
    let (mut dev, _) = gpt_two_fat32();
    wipe_primary(&mut dev);
    let mut ro = ReadOnly(dev);
    let found = find_partitions(&mut ro, false, &NullProgress, &CancelToken::new()).unwrap();
    assert!(!found.is_empty());
    let cancel = CancelToken::new();
    cancel.cancel();
    for quick in [true, false] {
        assert!(matches!(
            find_partitions(&mut ro, quick, &NullProgress, &cancel),
            Err(Error::Cancelled)
        ));
    }
}

#[test]
fn apply_candidates_mbr_types() {
    let t = Table::empty(TableKind::Mbr, 2000 * MIB, 512);
    let kinds = [
        (FsKind::Ntfs, 0x07u8),
        (FsKind::Fat32, 0x0C),
        (FsKind::Fat16, 0x06),
        (FsKind::Ext4, 0x83),
    ];
    let chosen: Vec<Candidate> = kinds
        .iter()
        .enumerate()
        .map(|(i, (fs, _))| cand((1 + 100 * i as u64) * MIB, 50 * MIB + 100, *fs))
        .collect();
    let t = apply_candidates(&t, &chosen).unwrap();
    assert_eq!(t.entries.len(), 4);
    for (e, (_, ty)) in t.entries.iter().zip(kinds) {
        assert_eq!(e.ptype, PartitionType::Mbr(ty));
        assert_eq!(e.len, 50 * MIB, "len rounded down to a sector multiple");
        assert!(e.guid.is_none());
    }
    assert_eq!(t.entries.iter().map(|e| e.number).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    // a fifth primary has no room
    let extra = cand(500 * MIB, 10 * MIB, FsKind::Ntfs);
    assert!(matches!(apply_candidates(&t, &[extra]), Err(Error::Invalid(_))));
}

#[test]
fn apply_candidates_gpt_types() {
    let t = Table::empty(TableKind::Gpt, 2000 * MIB, 512);
    let cases = [
        (FsKind::Ntfs, gpt_types::BASIC_DATA),
        (FsKind::ExFat, gpt_types::BASIC_DATA),
        (FsKind::Ext4, gpt_types::LINUX_FS),
        (FsKind::Btrfs, gpt_types::LINUX_FS),
        (FsKind::LinuxSwap, gpt_types::LINUX_SWAP),
        (FsKind::HfsPlus, gpt_types::APPLE_HFS),
        (FsKind::Apfs, gpt_types::APPLE_APFS),
    ];
    let chosen: Vec<Candidate> = cases
        .iter()
        .enumerate()
        .map(|(i, (fs, _))| cand((1 + 100 * i as u64) * MIB, 50 * MIB, *fs))
        .collect();
    let out = apply_candidates(&t, &chosen).unwrap();
    for (e, (_, g)) in out.entries.iter().zip(cases) {
        assert_eq!(e.ptype, PartitionType::Gpt(g));
    }
    // the input table is untouched
    assert!(t.entries.is_empty());
}

#[test]
fn apply_candidates_rejects_overlaps() {
    let t = Table::empty(TableKind::Gpt, 1000 * MIB, 512);
    let t = apply_candidates(&t, &[cand(10 * MIB, 100 * MIB, FsKind::Ntfs)]).unwrap();
    // overlaps the existing entry
    let e = apply_candidates(&t, &[cand(50 * MIB, 100 * MIB, FsKind::Ntfs)]);
    assert!(matches!(e, Err(Error::Invalid(_))), "{e:?}");
    // two chosen overlap each other
    let a = cand(200 * MIB, 100 * MIB, FsKind::Ntfs);
    let b = cand(250 * MIB, 100 * MIB, FsKind::Ntfs);
    assert!(matches!(apply_candidates(&t, &[a.clone(), b]), Err(Error::Invalid(_))));
    // adjacent is fine, and numbering continues in start order
    let c = cand(300 * MIB, 100 * MIB, FsKind::Ext4);
    let out = apply_candidates(&t, &[c, a]).unwrap();
    assert_eq!(out.entries.len(), 3);
    assert_eq!(out.entries[1].start, 200 * MIB);
    assert_eq!(out.entries[1].number, 2);
    assert_eq!(out.entries[2].number, 3);
    // beyond the usable area
    let bad = cand(990 * MIB, 100 * MIB, FsKind::Ntfs);
    assert!(matches!(apply_candidates(&t, &[bad]), Err(Error::Invalid(_))));
}
