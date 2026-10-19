use pm_core::{DiskInfo, Error, FsKind};
use pm_fs::Entry;
use pm_parttable::{Candidate, CandidateSource};
use pm_recover::{CarveReport, CarvedFile, Category, DeletedFile, DeviceIdentity, ScanSession, SessionKind};

fn identity() -> DeviceIdentity {
    DeviceIdentity {
        model: "Samsung SSD 980".into(),
        serial: "S64DNX0R123456".into(),
        size: 500_107_862_016,
        table_id: "8A1B2C3D".into(),
    }
}

fn disk(model: &str, serial: &str, size: u64, id: &str) -> DiskInfo {
    DiskInfo {
        model: model.into(),
        serial: serial.into(),
        size,
        disk_id: id.into(),
        ..Default::default()
    }
}

fn sessions() -> Vec<ScanSession> {
    let parts = SessionKind::Partitions(vec![Candidate {
        start: 1 << 20,
        len: 40 << 20,
        fs: FsKind::Ntfs,
        label: "Data".into(),
        source: CandidateSource::BootSector,
        confidence: 70,
    }]);
    let deleted = SessionKind::Deleted(vec![DeletedFile {
        entry: Entry {
            name: "?gone.txt".into(),
            path: "/?gone.txt".into(),
            is_dir: false,
            size: 77,
            modified: Some(1_700_000_000),
            id: 5,
            deleted: true,
            attrs: 0x20,
        },
        integrity: 80,
        fs_offset: 1 << 20,
    }]);
    let carved = SessionKind::Carved(CarveReport {
        files: vec![CarvedFile {
            offset: 0x1234_5000,
            len: 4096,
            sig: "JPEG".into(),
            ext: "jpg".into(),
            category: Category::Image,
            complete: true,
        }],
        unreadable: vec![(0x2000, 512)],
        bytes_scanned: 500_107_862_016 - 512,
    });
    [parts, deleted, carved].into_iter().map(|k| ScanSession::new(identity(), k)).collect()
}

#[test]
fn session_roundtrip_json() {
    let dir = tempfile::tempdir().unwrap();
    for (i, s) in sessions().into_iter().enumerate() {
        let p = dir.path().join(format!("s{i}.json"));
        s.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("Samsung SSD 980"));
        let back = ScanSession::load(&p).unwrap();
        assert_eq!(serde_json::to_value(&s).unwrap(), serde_json::to_value(&back).unwrap());
        assert_eq!(back.created, s.created);
        assert_eq!(back.device.size, 500_107_862_016);
        // overwriting an existing session works and leaves no temp file behind
        s.save(&p).unwrap();
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
    match ScanSession::load(&dir.path().join("s2.json")).unwrap().kind {
        SessionKind::Carved(r) => {
            let v = &r.files;
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].offset, 0x1234_5000);
            assert_eq!(v[0].category, Category::Image);
            assert!(v[0].complete);
            assert_eq!(r.unreadable, vec![(0x2000, 512)]);
            assert_eq!(r.bytes_scanned, 500_107_862_016 - 512);
        }
        other => panic!("{other:?}"),
    }
    match ScanSession::load(&dir.path().join("s1.json")).unwrap().kind {
        SessionKind::Deleted(v) => {
            assert_eq!(v[0].entry.name, "?gone.txt");
            assert_eq!(v[0].integrity, 80);
            assert_eq!(v[0].fs_offset, 1 << 20);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn created_is_an_iso_8601_utc_timestamp() {
    let s = &sessions()[0];
    let c = s.created.as_bytes();
    assert_eq!(c.len(), 20, "{}", s.created);
    assert_eq!((c[4], c[7], c[10], c[13], c[16], c[19]), (b'-', b'-', b'T', b':', b':', b'Z'));
    assert!(s.created.starts_with("20"));
}

#[test]
fn load_rejects_garbage_and_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("bad.json");
    std::fs::write(&p, b"{ not json").unwrap();
    assert!(matches!(ScanSession::load(&p), Err(Error::Parse(_))));
    assert!(ScanSession::load(&dir.path().join("none.json")).is_err());
}

#[test]
fn matches_compares_model_serial_size() {
    let s = &sessions()[0];
    let ok = disk("Samsung SSD 980", "S64DNX0R123456", 500_107_862_016, "8A1B2C3D");
    assert!(s.matches(&ok));
    assert!(!s.matches(&disk("Other", "S64DNX0R123456", 500_107_862_016, "8A1B2C3D")));
    assert!(!s.matches(&disk("Samsung SSD 980", "XXXX", 500_107_862_016, "8A1B2C3D")));
    assert!(!s.matches(&disk("Samsung SSD 980", "S64DNX0R123456", 1, "8A1B2C3D")));
    // the table id is ignored when either side is empty (it changes when a table is recovered)
    assert!(s.matches(&disk("Samsung SSD 980", "S64DNX0R123456", 500_107_862_016, "")));
    let mut wiped = identity();
    wiped.table_id.clear();
    let s2 = ScanSession::new(wiped, SessionKind::Carved(CarveReport::default()));
    assert!(s2.matches(&ok));
    // both present and different: not the same disk state
    assert!(!s.matches(&disk("Samsung SSD 980", "S64DNX0R123456", 500_107_862_016, "FFFF0000")));
    // an identity built from a DiskInfo matches that disk
    let id = DeviceIdentity::from_disk(&ok);
    assert_eq!(id.table_id, "8A1B2C3D");
    assert!(ScanSession::new(id, SessionKind::Carved(CarveReport::default())).matches(&ok));
}

#[test]
fn session_region_roundtrip_and_legacy_files() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("part.json");
    let s = ScanSession::new(identity(), SessionKind::Carved(CarveReport::default())).with_region(1 << 20, 40 << 20);
    s.save(&p).unwrap();
    let back = ScanSession::load(&p).unwrap();
    assert_eq!((back.offset, back.scanned_len), (1 << 20, 40 << 20));
    assert_eq!(back.region(64 << 20).unwrap(), (1 << 20, 40 << 20));
    assert!(back.region(32 << 20).is_err(), "the region no longer fits a smaller device");
    // a file written before the fields existed: the whole device
    let mut v = serde_json::to_value(&s).unwrap();
    let obj = v.as_object_mut().unwrap();
    obj.remove("offset");
    obj.remove("scanned_len");
    std::fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    let legacy = ScanSession::load(&p).unwrap();
    assert_eq!((legacy.offset, legacy.scanned_len), (0, 0));
    assert_eq!(legacy.region(64 << 20).unwrap(), (0, 64 << 20));
    assert!(legacy.region(0).is_err());
}
