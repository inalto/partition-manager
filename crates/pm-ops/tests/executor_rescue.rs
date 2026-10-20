//! Rescue-mode image export ([`pm_ops::RescueSettings`]) on [`MockPlatform`] disks backed by
//! [`FaultyDevice`]s: bad sectors become zeros in the image, a ddrescue mapfile records them,
//! and an interrupted export resumes from `<image>.partial` plus the mapfile.

use pm_core::{
    gpt_types, BlockDevice, CancelToken, DiskInfo, FnProgress, MemDevice, PartitionType,
    ProgressReport, Result, TableKind, MIB,
};
use pm_image::rescue::FaultyDevice;
use pm_image::{BlockStatus, ImageFormat, MapFile};
use pm_ops::{execute, CloneMode, ExecOpts, ExecReport, Operation, RescueSettings};
use pm_parttable::{write_table, Entry, Table};
use pm_platform::MockPlatform;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

const ELSEWHERE: u32 = 9;

fn meta(index: u32) -> DiskInfo {
    DiskInfo { index, model: format!("Mock Disk {index}"), ..Default::default() }
}

/// Recognisable, non-zero bytes.
fn pattern(len: u64) -> Vec<u8> {
    (0..len).map(|i| ((i % 251) as u8) ^ ((i >> 9) as u8) | 1).collect()
}

/// A `len`-byte disk of [`pattern`] bytes with a GPT holding one basic data partition.
fn gpt_source(len: u64) -> Vec<u8> {
    let mut dev = MemDevice::from_vec(pattern(len));
    let mut t = Table::empty(TableKind::Gpt, len, 512);
    t.entries.push(Entry {
        number: 1,
        start: MIB,
        len: len - 2 * MIB,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        guid: Some(Uuid::new_v4()),
        name: "data".into(),
        attrs: 0,
        logical: false,
    });
    write_table(&mut dev, &t).unwrap();
    dev.as_slice().to_vec()
}

fn platform_with(dev: Box<dyn BlockDevice + Send>) -> MockPlatform {
    let p = MockPlatform::new();
    p.add_device(1, dev, meta(1));
    p.set_system_disk(ELSEWHERE);
    p
}

fn run_with(p: &MockPlatform, op: &Operation, sink: &FnProgress<impl Fn(&ProgressReport) + Send + Sync>, cancel: &CancelToken) -> ExecReport {
    let opts = ExecOpts { dry_run: false, allow_system_disk: false };
    execute(p, std::slice::from_ref(op), &opts, sink, cancel).expect("execute returns a report")
}

fn run(p: &MockPlatform, op: &Operation) -> ExecReport {
    run_with(p, op, &FnProgress(|_: &ProgressReport| {}), &CancelToken::new())
}

fn image_bytes(path: &Path) -> Vec<u8> {
    let mut img = pm_image::open(path, true).unwrap();
    let mut buf = vec![0u8; img.len() as usize];
    img.read_at(0, &mut buf).unwrap();
    buf
}

fn mapfile(path: &Path) -> MapFile {
    MapFile::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn image_disk(path: &Path, format: ImageFormat, rescue: Option<RescueSettings>) -> Operation {
    Operation::ImageDisk { disk: 1, path: path.to_path_buf(), format, mode: CloneMode::Sector, rescue }
}

#[test]
fn image_in_rescue_mode_pads_bad_sectors_and_writes_mapfile() {
    let len = 4 * MIB;
    let src = gpt_source(len);
    let bad = [MIB + 3 * 512, 2 * MIB + 64 * 1024 + 7 * 512];
    let dev = FaultyDevice::new(MemDevice::from_vec(src.clone()), bad.to_vec(), HashMap::new());
    let p = platform_with(Box::new(dev));
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.vhd");

    let r = run(&p, &image_disk(&img, ImageFormat::VhdDynamic, Some(RescueSettings::default())));
    assert_eq!(r.failed, None, "{:#?}", r.log);
    assert!(img.exists() && !with_suffix(&img, ".partial").exists(), "partial renamed");

    let mut expected = src.clone();
    for b in bad {
        expected[b as usize..b as usize + 512].fill(0);
    }
    assert!(image_bytes(&img) == expected, "the image is the source with zeros at the bad sectors");

    let map_path = with_suffix(&img, ".map");
    let map = mapfile(&map_path);
    assert_eq!(map.ranges(BlockStatus::Bad), vec![(bad[0], 512), (bad[1], 512)]);
    assert_eq!(map.ranges(BlockStatus::Finished).iter().map(|r| r.1).sum::<u64>(), len - 1024);

    let line = r.log.iter().find(|l| l.contains("errsize")).expect("a rescue stats line");
    assert!(line.contains("errors 2") && line.contains(&map_path.display().to_string()), "{line}");
    assert_eq!(r.rescue_stats.len(), 1);
    let st = &r.rescue_stats[0];
    assert_eq!((st.bad_bytes, st.bad_areas), (1024, 2));
    assert_eq!(st.mapfile.as_deref(), Some(map_path.as_path()));
    assert!(p.write_sessions_log().is_empty(), "imaging never writes a disk");
}

#[test]
fn image_without_rescue_still_aborts_on_error() {
    let len = 4 * MIB;
    let dev = FaultyDevice::new(MemDevice::from_vec(gpt_source(len)), vec![2 * MIB], HashMap::new());
    let p = platform_with(Box::new(dev));
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");

    let r = run(&p, &image_disk(&img, ImageFormat::Raw, None));
    let (_, msg) = r.failed.expect("the first read error fails the operation");
    assert!(msg.contains("simulated bad sector"), "{msg}");
    assert!(!img.exists(), "no image left");
    assert!(!with_suffix(&img, ".partial").exists(), "partial removed");
    assert!(!with_suffix(&img, ".map").exists(), "no mapfile in plain mode");
}

/// Logs the reads of the device while `armed`, and sleeps on large reads while `slow`.
struct Watched {
    inner: FaultyDevice,
    armed: Arc<AtomicBool>,
    slow: Arc<AtomicBool>,
    reads: Arc<Mutex<Vec<(u64, u64)>>>,
}

impl BlockDevice for Watched {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.armed.load(Ordering::SeqCst) {
            self.reads.lock().unwrap().push((offset, buf.len() as u64));
        }
        if self.slow.load(Ordering::SeqCst) && buf.len() >= 64 << 10 {
            // longer than the progress tracker's report interval: the chunk gets reported
            std::thread::sleep(Duration::from_millis(150));
        }
        self.inner.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.inner.write_at(offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.0 + b.1 && b.0 < a.0 + a.1
}

#[test]
fn image_rescue_resume_continues_partial() {
    let len = 4 * MIB;
    let src = gpt_source(len);
    let bad = 3 * MIB + 5 * 512;
    let armed = Arc::new(AtomicBool::new(false));
    let slow = Arc::new(AtomicBool::new(true));
    let reads = Arc::new(Mutex::new(Vec::new()));
    let dev = Watched {
        inner: FaultyDevice::new(MemDevice::from_vec(src.clone()), vec![bad], HashMap::new()),
        armed: armed.clone(),
        slow: slow.clone(),
        reads: reads.clone(),
    };
    let p = platform_with(Box::new(dev));
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.qcow2");
    let partial = with_suffix(&img, ".partial");
    let map_path = with_suffix(&img, ".map");
    let op = image_disk(&img, ImageFormat::Qcow2, Some(RescueSettings { retries: 1, ..Default::default() }));

    // run 1: cancelled by the progress sink once the first chunk is reported
    let cancel = CancelToken::new();
    let c = cancel.clone();
    let sink = FnProgress(move |p: &ProgressReport| {
        if p.done > 0 {
            c.cancel();
        }
    });
    let r = run_with(&p, &op, &sink, &cancel);
    let (_, msg) = r.failed.expect("cancelled");
    assert!(msg.to_lowercase().contains("cancel"), "{msg}");
    assert!(partial.exists(), "the partial image is kept for resuming");
    assert!(map_path.exists(), "the mapfile is kept for resuming");
    assert!(!img.exists());
    assert!(r.log.iter().any(|l| l.contains("resume by running the same operation again")), "{:#?}", r.log);
    let done_before = mapfile(&map_path).ranges(BlockStatus::Finished);
    let rescued_before: u64 = done_before.iter().map(|r| r.1).sum();
    assert!(rescued_before > 0 && rescued_before < len, "{done_before:?}");

    // run 2: the same operation again; only the engine's reads are watched (armed by the
    // step's first progress report, after the executor has read the disk layout)
    slow.store(false, Ordering::SeqCst);
    let a = armed.clone();
    let sink = FnProgress(move |_: &ProgressReport| a.store(true, Ordering::SeqCst));
    let r = run_with(&p, &op, &sink, &CancelToken::new());
    assert_eq!(r.failed, None, "{:#?}", r.log);
    assert!(img.exists() && !partial.exists(), "finished with the rename");
    assert!(map_path.exists(), "the mapfile stays next to the image");

    let reads = reads.lock().unwrap().clone();
    assert!(!reads.is_empty(), "the second run read the rest");
    for r in &reads {
        assert!(
            !done_before.iter().any(|d| overlaps(*r, *d)),
            "read {r:?} of an area the first run already rescued ({done_before:?})"
        );
    }
    let mut expected = src;
    expected[bad as usize..bad as usize + 512].fill(0);
    assert!(image_bytes(&img) == expected, "the resumed image is complete");
    let map = mapfile(&map_path);
    assert_eq!(map.ranges(BlockStatus::Bad), vec![(bad, 512)]);
}

#[test]
fn image_rescue_all_bad_source_terminates() {
    let len = 2 * MIB;
    let every_sector: Vec<u64> = (0..len).step_by(512).collect();
    let dev = FaultyDevice::new(MemDevice::from_vec(pattern(len)), every_sector, HashMap::new());
    let p = platform_with(Box::new(dev));
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("dead.qcow2");

    let settings = RescueSettings { retries: 2, ..Default::default() };
    let r = run(&p, &image_disk(&img, ImageFormat::Qcow2, Some(settings)));
    assert_eq!(r.failed, None, "a damaged source is a success with a warning: {:#?}", r.log);
    let bytes = image_bytes(&img);
    assert_eq!(bytes.len() as u64, len, "the image has the source's size");
    assert!(bytes.iter().all(|&b| b == 0), "nothing was readable: all zeros");
    let size = std::fs::metadata(&img).unwrap().len();
    assert!(size < MIB, "unreadable blocks allocate no clusters (file is {size} bytes)");
    let map = mapfile(&with_suffix(&img, ".map"));
    assert_eq!(map.ranges(BlockStatus::Bad), vec![(0, len)]);
    let st = &r.rescue_stats[0];
    assert_eq!((st.rescued, st.bad_bytes, st.passes), (0, len, 2));
    assert!(r.log.iter().any(|l| l.contains("errsize 2.00 MB") && l.contains("errors 1")), "{:#?}", r.log);
}

#[test]
fn operation_rescue_settings_serde_roundtrip_and_default() {
    let d = RescueSettings::default();
    assert_eq!(
        d,
        RescueSettings {
            retries: 0,
            skip_size: None,
            cluster_size: None,
            no_trim: false,
            no_scrape: false,
            reverse: false,
            mapfile: None
        }
    );
    let s = RescueSettings {
        retries: 3,
        skip_size: Some(1 << 20),
        cluster_size: Some(64),
        no_trim: true,
        no_scrape: false,
        reverse: true,
        mapfile: Some(PathBuf::from(r"D:\maps\disk.map")),
    };
    let ops = vec![
        Operation::ImageDisk {
            disk: 1,
            path: PathBuf::from(r"D:\img\disk.vhd"),
            format: ImageFormat::VhdDynamic,
            mode: CloneMode::Sector,
            rescue: Some(s.clone()),
        },
        Operation::ImagePartition {
            disk: 1,
            number: 2,
            path: PathBuf::from(r"D:\img\part.qcow2"),
            format: ImageFormat::Qcow2,
            mode: CloneMode::Smart,
            rescue: None,
        },
    ];
    let json = serde_json::to_string(&ops).unwrap();
    let back: Vec<Operation> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ops);
    assert!(ops[0].summary().ends_with("sector by sector, rescue mode (3 retries))"), "{}", ops[0].summary());
    assert!(!ops[1].summary().contains("rescue"), "{}", ops[1].summary());

    // operations files written before rescue mode existed still load
    let old = r#"{"ImageDisk":{"disk":1,"path":"x.img","format":"Raw","mode":"Sector"}}"#;
    let op: Operation = serde_json::from_str(old).unwrap();
    assert!(matches!(op, Operation::ImageDisk { rescue: None, .. }));
    // partial settings fill in the defaults
    let partial = r#"{"retries":2}"#;
    assert_eq!(serde_json::from_str::<RescueSettings>(partial).unwrap(), RescueSettings { retries: 2, ..d });
}

#[test]
fn precheck_refuses_a_mapfile_in_a_missing_folder() {
    let p = platform_with(Box::new(MemDevice::from_vec(gpt_source(4 * MIB))));
    let model = pm_ops::Model::from_platform(&p).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    let settings = |m: PathBuf| RescueSettings { mapfile: Some(m), ..Default::default() };
    let bad = image_disk(&img, ImageFormat::Raw, Some(settings(dir.path().join("nope").join("disk.map"))));
    let msg = pm_ops::precheck(&model, &bad).unwrap_err().to_string();
    assert!(msg.contains("mapfile") && msg.contains("does not exist"), "{msg}");
    let same = image_disk(&img, ImageFormat::Raw, Some(settings(img.clone())));
    assert!(pm_ops::precheck(&model, &same).is_err(), "the mapfile cannot be the image");
    let good = image_disk(&img, ImageFormat::Raw, Some(settings(dir.path().join("disk.map"))));
    pm_ops::precheck(&model, &good).unwrap();
}
