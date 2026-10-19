use pm_core::gpt_types::BASIC_DATA;
use pm_core::units::MIB;
use pm_core::{
    BlockDevice, CancelToken, Error, FnProgress, FsKind, MemDevice, NullProgress, PartitionType,
    TableKind,
};
use pm_parttable::{search_lost, write_gpt, CandidateSource, Entry, Table};
use std::sync::Mutex;
use uuid::Uuid;

const DISK: u64 = 64 * MIB;

type Probe<'a> = dyn Fn(&mut dyn BlockDevice, u64) -> Option<(FsKind, String, u64)> + 'a;

#[test]
fn search_finds_fs_at_offset_with_injected_probe() {
    let mut dev = MemDevice::new(DISK);
    let probe: &Probe<'_> =
        &|_d, off| (off == 3 * MIB).then(|| (FsKind::Ntfs, "X".to_string(), 10 * MIB));
    let found = search_lost(&mut dev, probe, MIB, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(found.len(), 1);
    let c = &found[0];
    assert_eq!((c.start, c.len), (3 * MIB, 10 * MIB));
    assert_eq!(c.fs, FsKind::Ntfs);
    assert_eq!(c.label, "X");
    assert_eq!(c.source, CandidateSource::BootSector);
    assert_eq!(c.confidence, 70);
}

#[test]
fn search_offsets_follow_step_and_skip_zero() {
    let mut dev = MemDevice::new(DISK);
    let seen = Mutex::new(Vec::new());
    let probe: &Probe<'_> = &|_d, off| {
        seen.lock().unwrap().push(off);
        None
    };
    let found = search_lost(&mut dev, probe, 16 * MIB, &NullProgress, &CancelToken::new()).unwrap();
    assert!(found.is_empty());
    assert_eq!(*seen.lock().unwrap(), vec![16 * MIB, 32 * MIB, 48 * MIB]);
    assert!(matches!(
        search_lost(&mut dev, probe, 0, &NullProgress, &CancelToken::new()),
        Err(Error::Invalid(_))
    ));
}

fn gpt_dev_with_entries() -> (MemDevice, Table) {
    let mut t = Table::empty(TableKind::Gpt, DISK, 512);
    t.entries = vec![
        Entry {
            number: 1,
            start: 2 * MIB,
            len: 10 * MIB,
            ptype: PartitionType::Gpt(BASIC_DATA),
            guid: Some(Uuid::new_v4()),
            name: "Backup entry".into(),
            attrs: 0,
            logical: false,
        },
        Entry {
            number: 2,
            start: 20 * MIB,
            len: 5 * MIB,
            ptype: PartitionType::Gpt(BASIC_DATA),
            guid: Some(Uuid::new_v4()),
            name: String::new(),
            attrs: 0,
            logical: false,
        },
    ];
    let mut dev = MemDevice::new(DISK);
    write_gpt(&mut dev, &t).unwrap();
    // Destroy the protective MBR and the primary header: only the backup survives.
    dev.write_at(0, &[0u8; 512]).unwrap();
    dev.write_at(512, &[0u8; 512]).unwrap();
    (dev, t)
}

#[test]
fn search_reports_backup_gpt_entries_even_without_primary() {
    let (mut dev, t) = gpt_dev_with_entries();
    let probe: &Probe<'_> = &|_d, _off| None;
    let found = search_lost(&mut dev, probe, MIB, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(found.len(), 2);
    for (c, e) in found.iter().zip(&t.entries) {
        assert_eq!((c.start, c.len), (e.start, e.len));
        assert_eq!(c.source, CandidateSource::GptBackupEntry);
        assert_eq!(c.confidence, 90);
    }
    assert_eq!(found[0].label, "Backup entry");
    assert_eq!(found[0].fs, FsKind::Unknown);
}

#[test]
fn search_prefers_gpt_entry_over_overlapping_boot_sector_and_uses_probe_info() {
    let (mut dev, _t) = gpt_dev_with_entries();
    // Boot sector hit at the very start of entry 1 and another inside entry 2.
    let probe: &Probe<'_> = &|_d, off| match off {
        o if o == 2 * MIB => Some((FsKind::Ntfs, "Vol".to_string(), 10 * MIB)),
        o if o == 22 * MIB => Some((FsKind::Fat32, "inner".to_string(), MIB)),
        _ => None,
    };
    let found = search_lost(&mut dev, probe, MIB, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].source, CandidateSource::GptBackupEntry);
    assert_eq!(found[0].fs, FsKind::Ntfs);
    assert_eq!(found[0].label, "Vol");
    assert_eq!(found[0].confidence, 90);
    assert_eq!(found[1].start, 20 * MIB);
}

#[test]
fn search_dedupes_overlaps_and_sorts() {
    let mut dev = MemDevice::new(DISK);
    let probe: &Probe<'_> = &|_d, off| match off {
        o if o == 5 * MIB => Some((FsKind::Ntfs, "big".to_string(), 20 * MIB)),
        // fully inside the first one
        o if o == 8 * MIB => Some((FsKind::Fat32, "inside".to_string(), 2 * MIB)),
        // partial overlap with equal confidence: the earlier start wins
        o if o == 20 * MIB => Some((FsKind::Ext4, "partial".to_string(), 10 * MIB)),
        o if o == 40 * MIB => Some((FsKind::Ext4, "sep".to_string(), 4 * MIB)),
        // runs past the end of the disk: clamped
        o if o == 62 * MIB => Some((FsKind::Ext4, "tail".to_string(), 10 * MIB)),
        _ => None,
    };
    let found = search_lost(&mut dev, probe, MIB, &NullProgress, &CancelToken::new()).unwrap();
    let got: Vec<(u64, u64)> = found.iter().map(|c| (c.start, c.len)).collect();
    assert_eq!(got, vec![(5 * MIB, 20 * MIB), (40 * MIB, 4 * MIB), (62 * MIB, 2 * MIB)]);
}

#[test]
fn search_reports_progress_and_honours_cancel() {
    let mut dev = MemDevice::new(DISK);
    let probe: &Probe<'_> = &|_d, _off| None;
    let reports = Mutex::new(Vec::new());
    let sink = FnProgress(|r: &pm_core::ProgressReport| reports.lock().unwrap().push(r.clone()));
    search_lost(&mut dev, probe, MIB, &sink, &CancelToken::new()).unwrap();
    let r = reports.lock().unwrap();
    let last = r.last().expect("final report");
    assert_eq!(last.phase, "Searching");
    assert_eq!(last.total, DISK);
    assert!(last.done >= 63 * MIB);

    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(
        search_lost(&mut dev, probe, MIB, &NullProgress, &cancel),
        Err(Error::Cancelled)
    ));
}
