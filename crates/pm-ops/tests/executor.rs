//! Executor and queue tests on [`MockPlatform`]: every disk is an in-memory device, every
//! platform side effect is recorded.

use pm_core::{
    gpt_types, BlockDevice, CancelToken, DiskInfo, FnProgress, FsKind, MemDevice, NullProgress,
    PartitionType, ProgressReport, SliceDevice, TableKind, VolumeId, VolumeInfo, MIB,
};
use pm_fs::{format_fat32, FormatOpts};
use pm_image::ImageFormat;
use pm_ops::{
    execute, BootFix, CloneMode, ExecOpts, ExecReport, Model, OpQueue, Operation, Sizing,
    WipeMethod,
};
use pm_parttable::{read_table, write_table, Entry, Table};
use pm_platform::{MockPlatform, MountPoint, OpenMode, Platform};
use std::sync::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------- fixtures

const DISK_LEN: u64 = 48 * MIB;
/// A disk index that does not exist: makes the system disk known without it being a test disk.
const ELSEWHERE: u32 = 9;

fn meta(index: u32) -> DiskInfo {
    DiskInfo {
        index,
        model: format!("Mock Disk {index}"),
        ..Default::default()
    }
}

fn entry(number: u32, start: u64, len: u64, ptype: Uuid, attrs: u64) -> Entry {
    Entry {
        number,
        start,
        len,
        ptype: PartitionType::Gpt(ptype),
        guid: Some(Uuid::new_v4()),
        name: format!("part{number}"),
        attrs,
        logical: false,
    }
}

/// A `len`-byte device filled with `fill`, with a GPT holding `entries` (none: no table).
fn gpt_dev(len: u64, fill: u8, entries: Option<Vec<Entry>>) -> MemDevice {
    let mut dev = MemDevice::from_vec(vec![fill; len as usize]);
    if let Some(entries) = entries {
        let mut t = Table::empty(TableKind::Gpt, len, 512);
        t.entries = entries;
        write_table(&mut dev, &t).unwrap();
    }
    dev
}

fn volume(id: &str, fs: Option<FsKind>, mounts: &[&str], total: u64, free: u64) -> VolumeInfo {
    VolumeInfo {
        id: VolumeId(id.into()),
        fs,
        label: String::new(),
        mount_points: mounts.iter().map(|m| m.to_string()).collect(),
        total,
        free,
        ..Default::default()
    }
}

fn opts() -> ExecOpts {
    ExecOpts { dry_run: false, allow_system_disk: false }
}

fn run(p: &MockPlatform, ops: &[Operation]) -> ExecReport {
    execute(p, ops, &opts(), &NullProgress, &CancelToken::new()).expect("execute returns a report")
}

fn table_of(p: &MockPlatform, disk: u32) -> Table {
    let mut dev = MemDevice::from_vec(p.disk_bytes(disk));
    read_table(&mut dev).unwrap()
}

fn assert_sessions_finished(p: &MockPlatform) {
    assert_eq!(
        p.write_sessions_log(),
        p.finished_sessions_log(),
        "every write session must be finished"
    );
}

fn create(disk: u32, start: u64, len: u64, fs: Option<FsKind>, letter: Option<char>) -> Operation {
    Operation::CreatePartition {
        disk,
        start,
        len,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        fs,
        label: "Data".into(),
        letter,
        name: "Data".into(),
    }
}

/// Format `[start, start + len)` of `dev` as FAT32 with 512-byte clusters.
fn format_fat(dev: &mut MemDevice, start: u64, len: u64, label: &str) {
    let mut slice = SliceDevice::new(&mut *dev, start, len).unwrap();
    format_fat32(
        &mut slice,
        &FormatOpts { label: label.into(), cluster_size: Some(512), volume_id: Some(0x1234_5678) },
    )
    .unwrap();
}

/// Minimal FAT32 writer: a file `name83` (8.3, space padded) in the root directory, stored in
/// the first free clusters. Enough for the readers in pm-fs.
fn fat_add_file(dev: &mut MemDevice, part: u64, name83: &[u8; 11], data: &[u8]) {
    let mut bs = [0u8; 512];
    dev.read_at(part, &mut bs).unwrap();
    let u16_at = |o: usize| u16::from_le_bytes([bs[o], bs[o + 1]]) as u64;
    let u32_at = |o: usize| u32::from_le_bytes([bs[o], bs[o + 1], bs[o + 2], bs[o + 3]]) as u64;
    let (bps, spc, reserved, nfats, fat_sz, root) =
        (u16_at(11), bs[13] as u64, u16_at(14), bs[16] as u64, u32_at(36), u32_at(44));
    let cluster_bytes = bps * spc;
    let fat_off = |k: u64| part + (reserved + k * fat_sz) * bps;
    let data_start = part + (reserved + nfats * fat_sz) * bps;
    let cluster_off = |c: u64| data_start + (c - 2) * cluster_bytes;
    let fat_get = |dev: &mut MemDevice, c: u64| {
        let mut b = [0u8; 4];
        dev.read_at(fat_off(0) + 4 * c, &mut b).unwrap();
        u32::from_le_bytes(b) & 0x0FFF_FFFF
    };
    let n = (data.len() as u64).div_ceil(cluster_bytes).max(1);
    let mut first = 3;
    while (first..first + n).any(|c| fat_get(dev, c) != 0) {
        first += 1;
    }
    for i in 0..n {
        let c = first + i;
        let next: u32 = if i + 1 == n { 0x0FFF_FFFF } else { (c + 1) as u32 };
        for k in 0..nfats {
            dev.write_at(fat_off(k) + 4 * c, &next.to_le_bytes()).unwrap();
        }
    }
    let mut padded = data.to_vec();
    padded.resize((n * cluster_bytes) as usize, 0);
    dev.write_at(cluster_off(first), &padded).unwrap();
    // directory entry in the first free slot of the root cluster
    let mut dir = vec![0u8; cluster_bytes as usize];
    dev.read_at(cluster_off(root), &mut dir).unwrap();
    let slot = (0..dir.len() / 32).find(|i| dir[i * 32] == 0).expect("free root slot");
    let mut de = [0u8; 32];
    de[..11].copy_from_slice(name83);
    de[11] = 0x20;
    de[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
    de[26..28].copy_from_slice(&(first as u16).to_le_bytes());
    de[28..32].copy_from_slice(&(data.len() as u32).to_le_bytes());
    dev.write_at(cluster_off(root) + slot as u64 * 32, &de).unwrap();
}

/// `m`, after checking it has no run of spaces (a lost `\` line continuation in a message).
fn spaced(m: &str) -> &str {
    assert!(!m.contains("  "), "broken message spacing: {m:?}");
    m
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

/// Root listing `(name, size)` and the content of every file of the filesystem at
/// `[start, start + len)` of disk `disk`.
fn fs_files(p: &MockPlatform, disk: u32, start: u64, len: u64) -> Vec<(String, Vec<u8>)> {
    let dev = p.open_disk(disk, OpenMode::ReadOnly).unwrap();
    let slice = SliceDevice::new(dev, start, len).unwrap();
    let mut fs = pm_fs::open(Box::new(slice)).unwrap();
    let root = fs.root();
    let mut out = Vec::new();
    for e in fs.list(&root).unwrap() {
        if e.is_dir || e.deleted {
            continue;
        }
        let mut v = Vec::new();
        fs.extract(&e, &mut v).unwrap();
        out.push((e.name.clone(), v));
    }
    out.sort();
    out
}

fn used_ranges(p: &MockPlatform, disk: u32, start: u64, len: u64) -> Vec<(u64, u64)> {
    let dev = p.open_disk(disk, OpenMode::ReadOnly).unwrap();
    let slice = SliceDevice::new(dev, start, len).unwrap();
    let mut fs = pm_fs::open(Box::new(slice)).unwrap();
    fs.used_map().unwrap().ranges().collect()
}

// ---------------------------------------------------------------- tests

#[test]
fn execute_create_partition_writes_table_in_session() {
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![])), meta(1));
    p.set_system_disk(ELSEWHERE);
    // the volume Windows would mount once the partition exists
    p.add_volume(volume("vol-new", None, &[], 0, 0), 1, MIB, 32 * MIB);

    let op = create(1, MIB, 32 * MIB, Some(FsKind::Ntfs), Some('F'));
    let r = run(&p, &[op]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert_eq!(r.completed, 1);

    assert_eq!(p.write_sessions_log(), vec![1]);
    assert_sessions_finished(&p);
    let t = table_of(&p, 1);
    assert_eq!(t.kind, TableKind::Gpt);
    assert_eq!(t.entries.len(), 1);
    let e = &t.entries[0];
    assert_eq!((e.number, e.start, e.len), (1, MIB, 32 * MIB));
    assert_eq!(e.ptype, PartitionType::Gpt(gpt_types::BASIC_DATA));
    assert_eq!(e.name, "Data");

    let calls = p.calls();
    let fmt = calls
        .iter()
        .position(|c| c == "format vol-new NTFS label=Data quick=true")
        .unwrap_or_else(|| panic!("format call missing: {calls:?}"));
    let rescan = calls.iter().position(|c| c == "rescan 1").expect("rescan recorded");
    assert!(rescan < fmt, "the layout is re-read before formatting: {calls:?}");
    assert_eq!(p.mounted(), vec![(VolumeId("vol-new".into()), MountPoint::Letter('F'))]);
    assert!(r.log.iter().any(|l| l.contains("partition table")), "{:?}", r.log);
}

#[test]
fn execute_order_and_stop_on_first_failure() {
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![])), meta(1));
    p.set_system_disk(ELSEWHERE);
    p.add_volume(volume("vol-2", None, &[], 0, 0), 1, 9 * MIB, 8 * MIB);
    p.fail_calls_matching("format");

    let ops = [
        create(1, MIB, 8 * MIB, None, None),
        create(1, 9 * MIB, 8 * MIB, Some(FsKind::Fat32), None),
        create(1, 17 * MIB, 8 * MIB, None, None),
    ];
    let r = run(&p, &ops);
    assert_eq!(r.completed, 1);
    let (idx, msg) = r.failed.clone().expect("second op fails");
    assert_eq!(idx, 1);
    assert!(msg.contains("mock failure"), "{msg}");

    let t = table_of(&p, 1);
    let starts: Vec<(u32, u64)> = t.entries.iter().map(|e| (e.number, e.start)).collect();
    // op 1 done, op 2 wrote its table before the format failed, op 3 never ran
    assert_eq!(starts, vec![(1, MIB), (2, 9 * MIB)]);
    assert_eq!(p.write_sessions_log(), vec![1, 1]);
    assert_sessions_finished(&p);
    assert!(r.log.iter().any(|l| l.contains("mock failure")), "{:?}", r.log);
    assert!(!r.log.iter().any(|l| l.contains("[3/3]")), "op 3 must not run: {:?}", r.log);
}

#[test]
fn execute_dry_run_touches_nothing() {
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0x5A, Some(vec![])), meta(1));
    p.set_system_disk(ELSEWHERE);
    let before = p.disk_bytes(1);

    let ops = [
        create(1, MIB, 8 * MIB, Some(FsKind::Fat32), Some('F')),
        Operation::FormatPartition {
            disk: 1,
            number: 1,
            fs: FsKind::Ntfs,
            label: "X".into(),
            cluster: None,
            quick: true,
        },
        Operation::DeletePartition { disk: 1, number: 1 },
        Operation::WipeDisk { disk: 1, method: WipeMethod::Zero },
    ];
    let o = ExecOpts { dry_run: true, allow_system_disk: false };
    let r = execute(&p, &ops, &o, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert_eq!(r.completed, 4);
    assert_eq!(r.log.len(), 4, "{:?}", r.log);
    assert!(r.log.iter().all(|l| l.contains("would: ")), "{:?}", r.log);

    assert_eq!(p.disk_bytes(1), before, "dry run wrote to the disk");
    assert!(p.write_sessions_log().is_empty());
    assert!(p.calls().is_empty(), "{:?}", p.calls());
    assert!(p.mounted().is_empty());

    // a dry run still re-validates: deleting a partition that does not exist fails
    let bad = [Operation::DeletePartition { disk: 1, number: 7 }];
    let r = execute(&p, &bad, &o, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(r.completed, 0);
    assert_eq!(r.failed.as_ref().map(|f| f.0), Some(0));
}

/// Source: disk 0 (pattern 0x55) with a FAT32 partition holding two files; target: disk 1
/// pre-filled with 0xAA and no partition table.
fn clone_fixture() -> (MockPlatform, u64, u64) {
    let (start, len) = (MIB, 40 * MIB);
    let mut src = gpt_dev(DISK_LEN, 0x55, Some(vec![entry(1, start, len, gpt_types::BASIC_DATA, 0)]));
    format_fat(&mut src, start, len, "SRC");
    fat_add_file(&mut src, start, b"HELLO   TXT", b"hello, clone");
    fat_add_file(&mut src, start, b"DATA    BIN", &pattern(3000, 7));
    let p = MockPlatform::new();
    p.add_disk(0, src, meta(0));
    p.add_disk(1, MemDevice::from_vec(vec![0xAA; DISK_LEN as usize]), meta(1));
    p.set_system_disk(ELSEWHERE);
    (p, start, len)
}

#[test]
fn execute_clone_disk_copies_used_clusters_only() {
    // Smart: only the clusters the filesystem uses are copied.
    let (p, start, len) = clone_fixture();
    let op = Operation::CloneDisk {
        src_disk: 0,
        dst_disk: 1,
        sizing: Sizing::Exact,
        mode: CloneMode::Smart,
        boot_fix: BootFix::None,
    };
    let r = run(&p, &[op]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert_eq!(r.completed, 1);
    assert_sessions_finished(&p);

    let (st, dt) = (table_of(&p, 0), table_of(&p, 1));
    assert_eq!(dt.kind, TableKind::Gpt);
    assert_eq!(dt.entries.len(), 1);
    assert_eq!((dt.entries[0].start, dt.entries[0].len), (start, len));
    assert_ne!(dt.disk_guid, st.disk_guid, "fresh disk GUID");
    assert_ne!(dt.entries[0].guid, st.entries[0].guid, "fresh partition GUID");

    let (s, d) = (p.disk_bytes(0), p.disk_bytes(1));
    let used = used_ranges(&p, 0, start, len);
    let mut used_total = 0;
    for &(o, l) in &used {
        let (a, b) = ((start + o) as usize, (start + o + l) as usize);
        assert_eq!(&d[a..b], &s[a..b], "used range {o}+{l} copied");
        used_total += l;
    }
    assert!(used_total < len / 2, "the fixture filesystem is mostly empty");
    // a cluster in the middle of the (nearly empty) data area is unused: still 0xAA on the
    // target, 0x55 on the source
    let mid = start + len / 2;
    assert!(!used.iter().any(|&(o, l)| start + o < mid + 512 && mid < start + o + l));
    let mid = mid as usize..mid as usize + 512;
    assert!(s[mid.clone()].iter().all(|&b| b == 0x55));
    assert!(d[mid].iter().all(|&b| b == 0xAA), "unused clusters are not copied");
    assert_eq!(fs_files(&p, 1, start, len), fs_files(&p, 0, start, len));
    assert_eq!(fs_files(&p, 1, start, len).len(), 2);

    // Sector: every byte of the partition is copied.
    let (p, start, len) = clone_fixture();
    let op = Operation::CloneDisk {
        src_disk: 0,
        dst_disk: 1,
        sizing: Sizing::Exact,
        mode: CloneMode::Sector,
        boot_fix: BootFix::None,
    };
    let r = run(&p, &[op]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let (s, d) = (p.disk_bytes(0), p.disk_bytes(1));
    let part = start as usize..(start + len) as usize;
    assert!(d[part.clone()] == s[part], "sector clone copies everything");
    assert_eq!(fs_files(&p, 1, start, len).len(), 2);
    assert_sessions_finished(&p);
}

#[test]
fn execute_migrate_regenerates_disk_guid_and_calls_bcdboot() {
    const REQUIRED: u64 = 1;
    let (efi, msr, win, re) = ((MIB, 2 * MIB), (3 * MIB, MIB), (4 * MIB, 36 * MIB), (40 * MIB, 2 * MIB));
    let mut src = gpt_dev(
        DISK_LEN,
        0,
        Some(vec![
            entry(1, efi.0, efi.1, gpt_types::EFI_SYSTEM, 0),
            entry(2, msr.0, msr.1, gpt_types::MSR, 0),
            entry(3, win.0, win.1, gpt_types::BASIC_DATA, REQUIRED),
            entry(4, re.0, re.1, gpt_types::WINDOWS_RECOVERY, 0),
        ]),
    );
    format_fat(&mut src, win.0, win.1, "WINDOWS");
    fat_add_file(&mut src, win.0, b"BOOTMGR    ", b"not really bootmgr");
    let p = MockPlatform::new();
    p.add_disk(0, src, meta(0));
    p.add_disk(1, MemDevice::new(DISK_LEN), meta(1));
    p.set_system_disk(0);
    p.add_volume(volume("src-win", Some(FsKind::Fat32), &["C:\\"], win.1, win.1 / 2), 0, win.0, win.1);
    // the volumes Windows exposes once the clone's table is committed
    p.add_volume(volume("dst-efi", None, &[], 0, 0), 1, efi.0, efi.1);
    p.add_volume(volume("dst-win", None, &[], 0, 0), 1, win.0, win.1);

    let op = Operation::MigrateOs {
        src_disk: 0,
        dst_disk: 1,
        only_system_parts: true,
        sizing: Sizing::Exact,
        boot_fix: BootFix::Uefi,
    };
    let r = run(&p, &[op]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert_eq!(r.completed, 1);
    assert_sessions_finished(&p);
    assert_eq!(p.write_sessions_log(), vec![1], "only the destination is written");

    let (st, dt) = (table_of(&p, 0), table_of(&p, 1));
    assert_eq!(dt.entries.len(), 4);
    assert_ne!(dt.disk_guid, st.disk_guid, "fresh disk GUID");
    for e in &dt.entries {
        assert!(!st.entries.iter().any(|s| s.guid == e.guid), "partition {} GUID reused", e.number);
    }
    let w = dt.entries.iter().find(|e| e.start == win.0).unwrap();
    assert_eq!(w.attrs & REQUIRED, REQUIRED, "attributes are kept");
    assert_eq!(fs_files(&p, 1, win.0, win.1), fs_files(&p, 0, win.0, win.1));

    let calls = p.calls();
    let bcd = format!("run_tool BcdBoot {:?}", ["E:\\Windows", "/s", "F:", "/f", "UEFI"]);
    assert!(calls.contains(&bcd), "{bcd} not in {calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("run_tool BootSect")), "{calls:?}");
    assert!(p.mounted().is_empty(), "temporary letters removed: {:?}", p.mounted());
    assert!(!r.log.iter().any(|l| l.contains("boot fix failed")), "{:?}", r.log);

    // a failing boot fix is reported but does not fail the migration
    let p2 = MockPlatform::new();
    let mut src2 = gpt_dev(
        DISK_LEN,
        0,
        Some(vec![
            entry(1, efi.0, efi.1, gpt_types::EFI_SYSTEM, 0),
            entry(3, win.0, win.1, gpt_types::BASIC_DATA, REQUIRED),
        ]),
    );
    format_fat(&mut src2, win.0, win.1, "WINDOWS");
    p2.add_disk(0, src2, meta(0));
    p2.add_disk(1, MemDevice::new(DISK_LEN), meta(1));
    p2.set_system_disk(0);
    p2.add_volume(volume("dst-efi", None, &[], 0, 0), 1, efi.0, efi.1);
    p2.add_volume(volume("dst-win", None, &[], 0, 0), 1, MIB * 3, win.1);
    p2.fail_calls_matching("run_tool BcdBoot");
    let op = Operation::MigrateOs {
        src_disk: 0,
        dst_disk: 1,
        only_system_parts: false,
        sizing: Sizing::Fit,
        boot_fix: BootFix::All,
    };
    let r = run(&p2, &[op]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert!(r.log.iter().any(|l| l.contains("boot fix failed")), "{:?}", r.log);
    assert!(p2.mounted().is_empty(), "temporary letters removed after a failure");
    assert_sessions_finished(&p2);
}

#[test]
fn queue_push_undo_discard_preview() {
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![])), meta(1));
    p.set_system_disk(ELSEWHERE);
    let base = Model::from_platform(&p).unwrap();
    let mut q = OpQueue::new(base.clone());
    assert_eq!(q.len(), 0);
    assert!(q.is_empty());

    q.push(create(1, MIB, 8 * MIB, None, None)).unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q.preview().disk(1).unwrap().partitions.len(), 1);

    // an overlapping partition is rejected and leaves the queue alone
    assert!(q.push(create(1, 4 * MIB, 8 * MIB, None, None)).is_err());
    assert_eq!(q.len(), 1);

    q.push(Operation::DeletePartition { disk: 1, number: 1 }).unwrap();
    assert_eq!(q.ops().len(), 2);
    assert!(q.preview().disk(1).unwrap().partitions.is_empty());

    let undone = q.undo().unwrap();
    assert_eq!(undone, Operation::DeletePartition { disk: 1, number: 1 });
    assert_eq!(q.len(), 1);
    assert_eq!(q.preview().disk(1).unwrap().partitions.len(), 1);

    q.discard();
    assert!(q.is_empty());
    assert!(q.undo().is_none());
    assert!(q.preview().disk(1).unwrap().partitions.is_empty());

    q.push(create(1, MIB, 8 * MIB, None, None)).unwrap();
    let mut other = base.clone();
    other.disks.retain(|d| d.index != 1);
    q.reset(other);
    assert!(q.is_empty());
    assert!(q.preview().disk(1).is_none());
}

#[test]
fn execute_refuses_system_disk_at_execution_time() {
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![entry(1, MIB, 8 * MIB, gpt_types::BASIC_DATA, 0)])), meta(1));
    // the queue was built when disk 1 was not the system disk ...
    p.set_system_disk(ELSEWHERE);
    let mut q = OpQueue::new(Model::from_platform(&p).unwrap());
    q.push(Operation::WipePartition { disk: 1, number: 1, method: WipeMethod::Zero }).unwrap();
    q.push(create(1, 9 * MIB, 8 * MIB, None, None)).unwrap();
    // ... but now it is
    p.set_system_disk(1);
    let before = p.disk_bytes(1);
    let r = run(&p, q.ops());
    assert_eq!(r.completed, 0);
    let (i, msg) = r.failed.clone().unwrap();
    assert_eq!(i, 0);
    assert!(msg.contains("system disk"), "{msg}");
    assert!(p.write_sessions_log().is_empty());
    assert_eq!(p.disk_bytes(1), before);

    // allow_system_disk relaxes only the non-destructive family
    let allow = ExecOpts { dry_run: false, allow_system_disk: true };
    let r = execute(&p, q.ops(), &allow, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(r.failed.as_ref().map(|f| f.0), Some(0), "wipe stays refused: {:?}", r.log);
    let r = execute(&p, &q.ops()[1..], &allow, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(r.failed, None, "create is allowed with allow_system_disk: {:?}", r.log);
    assert_eq!(table_of(&p, 1).entries.len(), 2);
    assert_sessions_finished(&p);
}

#[test]
fn execute_wipe_partition_and_disk() {
    let p = MockPlatform::new();
    let (start, len) = (2 * MIB, 6 * MIB);
    p.add_disk(1, gpt_dev(DISK_LEN, 0x55, Some(vec![entry(1, start, len, gpt_types::BASIC_DATA, 0)])), meta(1));
    p.set_system_disk(ELSEWHERE);
    let before = p.disk_bytes(1);

    let r = run(&p, &[Operation::WipePartition { disk: 1, number: 1, method: WipeMethod::Zero }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let after = p.disk_bytes(1);
    let range = start as usize..(start + len) as usize;
    assert!(after[range.clone()].iter().all(|&b| b == 0));
    assert_eq!(&after[..start as usize], &before[..start as usize]);
    assert_eq!(&after[range.end..], &before[range.end..]);

    let r = run(&p, &[Operation::WipePartition { disk: 1, number: 1, method: WipeMethod::Dod3 }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let after = p.disk_bytes(1);
    let wiped = &after[range.clone()];
    assert!(wiped.iter().any(|&b| b != 0) && wiped.iter().any(|&b| b != 0xFF), "last pass is random");
    assert_eq!(&after[range.end..], &before[range.end..]);

    let r = run(&p, &[Operation::WipeDisk { disk: 1, method: WipeMethod::Random }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert_eq!(table_of(&p, 1).kind, TableKind::None, "no table left after a disk wipe");
    let all = p.disk_bytes(1);
    assert!(all[..512].iter().all(|&b| b == 0), "sector 0 zeroed");
    assert!(all[DISK_LEN as usize - 33 * 512..].iter().all(|&b| b == 0), "backup GPT zeroed");
    assert!(all[(start + MIB) as usize..(start + 2 * MIB) as usize].iter().any(|&b| b != 0x55));
    assert_sessions_finished(&p);
}

#[test]
fn execute_wipe_cancel_still_finishes_session() {
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0x55, Some(vec![entry(1, MIB, 40 * MIB, gpt_types::BASIC_DATA, 0)])), meta(1));
    p.set_system_disk(ELSEWHERE);
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let seen = Mutex::new(Vec::<String>::new());
    let sink = FnProgress(|r: &ProgressReport| {
        seen.lock().unwrap().push(r.phase.clone());
        c2.cancel();
    });
    let op = Operation::WipePartition { disk: 1, number: 1, method: WipeMethod::Zero };
    let r = execute(&p, &[op], &opts(), &sink, &cancel).unwrap();
    assert_eq!(r.completed, 0);
    let (_, msg) = r.failed.unwrap();
    assert!(msg.contains("cancel"), "{msg}");
    assert_sessions_finished(&p);
    let phases = seen.lock().unwrap();
    assert!(phases.iter().any(|ph| ph.starts_with("[1/1] Wipe partition 1 on Disk 1")), "{phases:?}");
}

#[test]
fn execute_resize_ntfs_shrinks_volume_first_and_grows_table_first() {
    let fixture = || {
        let p = MockPlatform::new();
        p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![entry(1, MIB, 20 * MIB, gpt_types::BASIC_DATA, 0)])), meta(1));
        p.set_system_disk(ELSEWHERE);
        p.add_volume(volume("v1", Some(FsKind::Ntfs), &["F:\\"], 20 * MIB, 15 * MIB), 1, MIB, 20 * MIB);
        p
    };
    let shrink = Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 10 * MIB };
    let grow = Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 30 * MIB };

    // shrink: the filesystem goes first, so a failing shrink leaves the table alone
    let p = fixture();
    p.fail_calls_matching("shrink");
    let r = run(&p, &[shrink.clone()]);
    assert!(r.failed.is_some());
    assert_eq!(table_of(&p, 1).entries[0].len, 20 * MIB);
    assert!(p.write_sessions_log().is_empty());
    let p = fixture();
    let r = run(&p, &[shrink]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert!(p.calls().contains(&format!("shrink v1 {}", 10 * MIB)), "{:?}", p.calls());
    assert_eq!(table_of(&p, 1).entries[0].len, 10 * MIB);

    // grow: the partition goes first, so a failing extend leaves the bigger partition
    let p = fixture();
    p.fail_calls_matching("extend");
    let r = run(&p, &[grow.clone()]);
    assert!(r.failed.is_some());
    assert_eq!(table_of(&p, 1).entries[0].len, 30 * MIB);
    assert_sessions_finished(&p);
    let p = fixture();
    let r = run(&p, &[grow]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert!(p.calls().contains(&format!("extend v1 {}", 30 * MIB)), "{:?}", p.calls());

    // other filesystems cannot be resized
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![entry(1, MIB, 20 * MIB, gpt_types::BASIC_DATA, 0)])), meta(1));
    p.set_system_disk(ELSEWHERE);
    p.add_volume(volume("v1", Some(FsKind::Fat32), &[], 20 * MIB, 15 * MIB), 1, MIB, 20 * MIB);
    let r = run(&p, &[Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 30 * MIB }]);
    let (_, msg) = r.failed.unwrap();
    assert!(msg.contains("resize of FAT32 is not supported"), "{msg}");
    assert!(p.write_sessions_log().is_empty());
}

#[test]
fn execute_move_copies_data_overlap_safe() {
    for (old, new) in [(2 * MIB, 3 * MIB), (4 * MIB, 3 * MIB), (2 * MIB, 20 * MIB)] {
        let len = 6 * MIB;
        let mut dev = gpt_dev(DISK_LEN, 0, Some(vec![entry(1, old, len, gpt_types::BASIC_DATA, 0)]));
        let data = pattern(len as usize, 3);
        dev.write_at(old, &data).unwrap();
        let p = MockPlatform::new();
        p.add_disk(1, dev, meta(1));
        p.set_system_disk(ELSEWHERE);
        let r = run(&p, &[Operation::ResizeMove { disk: 1, number: 1, new_start: new, new_len: len }]);
        assert_eq!(r.failed, None, "{old}->{new}: {:?}", r.log);
        let b = p.disk_bytes(1);
        assert!(b[new as usize..(new + len) as usize] == data[..], "{old}->{new}: data moved intact");
        let e = &table_of(&p, 1).entries[0];
        assert_eq!((e.start, e.len), (new, len));
        assert_sessions_finished(&p);
    }
}

#[test]
fn execute_align_moves_data_before_table() {
    // a legacy partition starting at sector 63
    let (old, len) = (63 * 512, 8 * MIB);
    let mut dev = gpt_dev(DISK_LEN, 0, None);
    let mut t = Table::empty(TableKind::Gpt, DISK_LEN, 512);
    t.gpt_first_usable = 34 * 512;
    t.entries = vec![entry(1, old, len, gpt_types::BASIC_DATA, 0)];
    write_table(&mut dev, &t).unwrap();
    let data = pattern(len as usize, 9);
    dev.write_at(old, &data).unwrap();
    let p = MockPlatform::new();
    p.add_disk(1, dev, meta(1));
    p.set_system_disk(ELSEWHERE);
    // an NTFS volume (a partition of unknown filesystem may not be trimmed)
    p.add_volume(volume("a1", Some(FsKind::Ntfs), &[], len, len - MIB), 1, old, len);
    let r = run(&p, &[Operation::AlignPartitions { disk: 1 }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let e = &table_of(&p, 1).entries[0];
    assert_eq!(e.start, MIB);
    let b = p.disk_bytes(1);
    assert!(b[MIB as usize..(MIB + e.len) as usize] == data[..e.len as usize], "data follows the partition");
    assert_sessions_finished(&p);
}

#[test]
fn execute_image_and_restore_partition() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("part.img");
    let (p, start, len) = clone_fixture();
    // give the target a table with room for the restored partition
    p.add_disk(1, gpt_dev(DISK_LEN, 0xAA, Some(vec![])), meta(1));
    let image = Operation::ImagePartition {
        disk: 0,
        number: 1,
        path: img.clone(),
        format: ImageFormat::Raw,
        mode: CloneMode::Smart,
    };
    let r = run(&p, &[image]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert!(img.exists());
    assert!(!dir.path().join("part.img.partial").exists(), "partial renamed");
    assert_eq!(r.rescue_stats.len(), 1);
    assert!(p.write_sessions_log().is_empty(), "imaging never writes a disk");

    let restore = Operation::RestoreImage { path: img.clone(), dst_disk: 1, dst_start: Some(start) };
    let r = run(&p, &[restore]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let e = &table_of(&p, 1).entries[0];
    assert_eq!((e.start, e.len), (start, len));
    assert_eq!(fs_files(&p, 1, start, len), fs_files(&p, 0, start, len));
    assert_sessions_finished(&p);

    // a failing image leaves no file behind
    let missing = dir.path().join("nope").join("x.img");
    let r = run(&p, &[Operation::ImageDisk { disk: 0, path: missing.clone(), format: ImageFormat::Raw, mode: CloneMode::Sector }]);
    assert!(r.failed.is_some());
    assert!(!missing.exists());
}

#[test]
fn execute_mount_unmount_and_letters() {
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![entry(1, MIB, 8 * MIB, gpt_types::BASIC_DATA, 0)])), meta(1));
    p.set_system_disk(ELSEWHERE);
    p.add_volume(volume("v1", Some(FsKind::Ntfs), &[], 8 * MIB, 4 * MIB), 1, MIB, 8 * MIB);
    let v = VolumeId("v1".into());
    let r = run(
        &p,
        &[
            Operation::Mount { volume: v.clone(), at: MountPoint::Letter('E') },
            Operation::SetLetter { disk: 1, number: 1, letter: Some('G') },
        ],
    );
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert_eq!(p.mounted(), vec![(v.clone(), MountPoint::Letter('G'))]);
    let r = run(&p, &[Operation::Unmount { volume: v.clone(), at: MountPoint::Letter('G') }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert!(p.mounted().is_empty());
    assert!(p.write_sessions_log().is_empty());

    let r = run(&p, &[Operation::SetLabel { disk: 1, number: 1, label: "X".into() }]);
    let (_, msg) = r.failed.unwrap();
    assert!(msg.contains("unsupported"), "{msg}");
}

#[test]
fn execute_table_ops_initialize_convert_active_type_delete() {
    let p = MockPlatform::new();
    p.add_disk(1, MemDevice::new(DISK_LEN), meta(1));
    p.set_system_disk(ELSEWHERE);
    let r = run(
        &p,
        &[
            Operation::InitializeDisk { disk: 1, kind: TableKind::Gpt },
            create(1, MIB, 8 * MIB, None, None),
            create(1, 9 * MIB, 8 * MIB, None, None),
            Operation::ConvertTable { disk: 1, to: TableKind::Mbr },
        ],
    );
    assert_eq!(r.failed, None, "{:?}", r.log);
    let t = table_of(&p, 1);
    assert_eq!(t.kind, TableKind::Mbr);
    let spans: Vec<(u64, u64)> = t.entries.iter().map(|e| (e.start, e.len)).collect();
    assert_eq!(spans, vec![(MIB, 8 * MIB), (9 * MIB, 8 * MIB)]);
    let b = p.disk_bytes(1);
    assert!(b[512..34 * 512].iter().all(|&x| x == 0), "primary GPT cleared");
    assert!(b[DISK_LEN as usize - 33 * 512..].iter().all(|&x| x == 0), "backup GPT cleared");

    let r = run(
        &p,
        &[
            Operation::SetActive { disk: 1, number: 2 },
            Operation::SetType { disk: 1, number: 1, ptype: PartitionType::Mbr(0x0C) },
            Operation::DeletePartition { disk: 1, number: 2 },
            Operation::SetActive { disk: 1, number: 1 },
        ],
    );
    assert_eq!(r.failed, None, "{:?}", r.log);
    let t = table_of(&p, 1);
    assert_eq!(t.entries.len(), 1);
    assert_eq!(t.entries[0].ptype, PartitionType::Mbr(0x0C));
    assert_eq!(t.entries[0].attrs & 1, 1, "partition 1 active");

    let r = run(&p, &[Operation::ConvertTable { disk: 1, to: TableKind::Gpt }, Operation::RebuildMbr { disk: 1 }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let t = table_of(&p, 1);
    assert_eq!(t.kind, TableKind::Gpt);
    assert_eq!((t.entries[0].start, t.entries[0].len), (MIB, 8 * MIB));
    assert_sessions_finished(&p);
    assert_eq!(p.write_sessions_log().len(), 10, "one session per table operation");
}

// ---------------------------------------------------------------- fix round 1

#[test]
fn execute_align_cancel_keeps_table_consistent() {
    let legacy = 63 * 512;
    let (p1, p2, len) = (legacy, 10 * MIB + legacy, 8 * MIB);
    let mut dev = gpt_dev(DISK_LEN, 0, None);
    let mut t = Table::empty(TableKind::Gpt, DISK_LEN, 512);
    t.gpt_first_usable = 34 * 512;
    t.entries = vec![
        entry(1, p1, len, gpt_types::BASIC_DATA, 0),
        entry(2, p2, len, gpt_types::BASIC_DATA, 0),
    ];
    write_table(&mut dev, &t).unwrap();
    let (d1, d2) = (pattern(len as usize, 1), pattern(len as usize, 2));
    dev.write_at(p1, &d1).unwrap();
    dev.write_at(p2, &d2).unwrap();
    let p = MockPlatform::new();
    p.add_disk(1, dev, meta(1));
    p.set_system_disk(ELSEWHERE);
    p.add_volume(volume("a1", Some(FsKind::Ntfs), &[], len, len - MIB), 1, p1, len);
    p.add_volume(volume("a2", Some(FsKind::Ntfs), &[], len, len - MIB), 1, p2, len);

    // cancel once the data of the first partition has been moved
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let sink = FnProgress(|r: &ProgressReport| {
        if r.phase.contains("move the data of partition 1") && r.total > 0 {
            c2.cancel();
        }
    });
    let r = execute(&p, &[Operation::AlignPartitions { disk: 1 }], &opts(), &sink, &cancel).unwrap();
    let (_, msg) = r.failed.expect("cancelled");
    assert!(msg.contains("cancel"), "{msg}");
    let t = table_of(&p, 1);
    let spans: Vec<(u64, u64)> = t.entries.iter().map(|e| (e.start, e.len)).collect();
    assert_eq!(spans, vec![(MIB, 7 * MIB), (p2, len)], "table matches the data on disk");
    let b = p.disk_bytes(1);
    assert!(b[MIB as usize..(8 * MIB) as usize] == d1[..(7 * MIB) as usize], "partition 1 moved");
    assert!(b[p2 as usize..(p2 + len) as usize] == d2[..], "partition 2 untouched");
    assert_sessions_finished(&p);

    // a second run finishes the job
    let r = run(&p, &[Operation::AlignPartitions { disk: 1 }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let t = table_of(&p, 1);
    let spans: Vec<(u64, u64)> = t.entries.iter().map(|e| (e.start, e.len)).collect();
    assert_eq!(spans, vec![(MIB, 7 * MIB), (11 * MIB, 7 * MIB)]);
    let b = p.disk_bytes(1);
    assert!(b[(11 * MIB) as usize..(18 * MIB) as usize] == d2[..(7 * MIB) as usize]);
    assert!(b[MIB as usize..(8 * MIB) as usize] == d1[..(7 * MIB) as usize]);
}

/// Disk 0 (64 MiB): one 56 MiB partition backed by volume `vol`; disk 1: a 32 MiB raw target.
fn shrink_fixture(fs: FsKind, vol: &str) -> (MockPlatform, Vec<u8>) {
    let (start, len) = (MIB, 56 * MIB);
    let mut src = gpt_dev(64 * MIB, 0, Some(vec![entry(1, start, len, gpt_types::BASIC_DATA, 0)]));
    let data = pattern(len as usize, 5);
    src.write_at(start, &data).unwrap();
    if fs == FsKind::Fat32 {
        format_fat(&mut src, start, len, "SRC");
    }
    let p = MockPlatform::new();
    p.add_disk(0, src, meta(0));
    p.add_disk(1, MemDevice::from_vec(vec![0xAA; (32 * MIB) as usize]), meta(1));
    p.set_system_disk(ELSEWHERE);
    p.add_volume(volume(vol, Some(fs), &["F:\\"], len, len - 6 * MIB), 0, start, len);
    (p, data)
}

fn fit_clone() -> Operation {
    Operation::CloneDisk {
        src_disk: 0,
        dst_disk: 1,
        sizing: Sizing::Fit,
        mode: CloneMode::Smart,
        boot_fix: BootFix::None,
    }
}

#[test]
fn execute_clone_to_smaller_disk_shrinks_ntfs_source_first() {
    let (p, data) = shrink_fixture(FsKind::Ntfs, "src-ntfs");
    let model = Model::from_platform(&p).unwrap();
    let warn = fit_clone().shrinks_source(&model);
    assert_eq!(warn.len(), 1, "{warn:?}");
    assert!(spaced(&warn[0]).contains("partition 1"), "{warn:?}");
    assert!(warn[0].contains("will be shrunk from"), "{warn:?}");

    let r = run(&p, &[fit_clone()]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let e = table_of(&p, 1).entries[0].clone();
    assert!(e.len < 56 * MIB);
    assert!(p.calls().contains(&format!("shrink src-ntfs {}", e.len)), "{:?}", p.calls());
    assert!(r.log.iter().any(|l| l.contains("will be shrunk")), "{:?}", r.log);
    assert!(r.log.iter().any(|l| l.contains("shrinking source volume src-ntfs")), "{:?}", r.log);
    let b = p.disk_bytes(1);
    assert!(b[e.start as usize..(e.start + e.len) as usize] == data[..e.len as usize]);
    assert_eq!(p.write_sessions_log(), vec![1]);
    assert_sessions_finished(&p);

    // the shrink happens before anything is written to the target
    let (p, _) = shrink_fixture(FsKind::Ntfs, "src-ntfs");
    p.fail_calls_matching("shrink");
    let before = p.disk_bytes(1);
    let r = run(&p, &[fit_clone()]);
    assert!(r.failed.is_some());
    assert!(p.write_sessions_log().is_empty(), "no session on the target");
    assert_eq!(p.disk_bytes(1), before);
}

#[test]
fn execute_clone_refusal_writes_nothing() {
    let (p, _) = shrink_fixture(FsKind::Fat32, "src-fat");
    let before = p.disk_bytes(1);
    let r = run(&p, &[fit_clone()]);
    let (i, msg) = r.failed.clone().expect("refused");
    assert_eq!(i, 0);
    assert!(spaced(&msg).contains("must be shrunk first"), "{msg}");
    assert!(msg.contains("only NTFS volumes can be shrunk online"), "{msg}");
    assert!(p.write_sessions_log().is_empty());
    assert_eq!(p.disk_bytes(1), before, "the target is untouched");
    assert!(p.calls().is_empty(), "{:?}", p.calls());

    let mut q = OpQueue::new(Model::from_platform(&p).unwrap());
    assert!(q.push(fit_clone()).is_err(), "refused at queue time too");
}

#[test]
fn precheck_rejects_what_the_executor_refuses() {
    let p = MockPlatform::new();
    let legacy = 63 * 512;
    let mut dev = gpt_dev(DISK_LEN, 0, None);
    let mut t = Table::empty(TableKind::Gpt, DISK_LEN, 512);
    t.gpt_first_usable = 34 * 512;
    t.entries = vec![entry(1, legacy, 8 * MIB, gpt_types::BASIC_DATA, 0)];
    write_table(&mut dev, &t).unwrap();
    p.add_disk(1, dev, meta(1));
    p.set_system_disk(ELSEWHERE);
    p.add_volume(volume("v1", Some(FsKind::Fat32), &[], 8 * MIB, 4 * MIB), 1, legacy, 8 * MIB);
    let model = Model::from_platform(&p).unwrap();
    let mut q = OpQueue::new(model.clone());

    let label = Operation::SetLabel { disk: 1, number: 1, label: "X".into() };
    let resize = Operation::ResizeMove { disk: 1, number: 1, new_start: legacy, new_len: 12 * MIB };
    let align = Operation::AlignPartitions { disk: 1 };
    for op in [&label, &resize, &align] {
        let e = pm_ops::precheck(&model, op).expect_err("refused");
        spaced(&e.to_string());
        assert!(q.push(op.clone()).is_err(), "{op:?}");
    }
    let e = pm_ops::precheck(&model, &align).unwrap_err().to_string();
    assert!(e.contains("aligning it would shrink it by"), "{e}");
    assert!(e.contains("only NTFS volumes can be shrunk online"), "{e}");
    assert!(q.is_empty());
    // what the executor can do still passes
    assert!(pm_ops::precheck(&model, &Operation::DeletePartition { disk: 1, number: 1 }).is_ok());

    let dry = ExecOpts { dry_run: true, allow_system_disk: false };
    let r = execute(&p, &[resize], &dry, &NullProgress, &CancelToken::new()).unwrap();
    let (i, msg) = r.failed.expect("dry run refuses a FAT32 resize");
    assert_eq!(i, 0);
    assert!(msg.contains("resize of FAT32 is not supported"), "{msg}");
}

#[test]
fn execute_mbr_over_gpt_clears_stale_gpt() {
    let tail = DISK_LEN as usize - 512;
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![])), meta(1));
    p.set_system_disk(ELSEWHERE);
    let r = run(&p, &[Operation::InitializeDisk { disk: 1, kind: TableKind::Mbr }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let b = p.disk_bytes(1);
    assert_eq!(table_of(&p, 1).kind, TableKind::Mbr);
    assert!(b[512..1024].iter().all(|&x| x == 0), "primary GPT header cleared");
    assert!(b[tail..].iter().all(|&x| x == 0), "backup GPT header cleared");

    // an MBR disk cloned onto a former GPT disk
    let mut src = MemDevice::new(DISK_LEN);
    let mut t = Table::empty(TableKind::Mbr, DISK_LEN, 512);
    t.entries = vec![Entry {
        number: 1,
        start: MIB,
        len: 8 * MIB,
        ptype: PartitionType::Mbr(0x07),
        guid: None,
        name: String::new(),
        attrs: 1,
        logical: false,
    }];
    write_table(&mut src, &t).unwrap();
    let p = MockPlatform::new();
    p.add_disk(0, src, meta(0));
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![])), meta(1));
    p.set_system_disk(ELSEWHERE);
    let op = Operation::CloneDisk {
        src_disk: 0,
        dst_disk: 1,
        sizing: Sizing::Exact,
        mode: CloneMode::Sector,
        boot_fix: BootFix::None,
    };
    let r = run(&p, &[op]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let b = p.disk_bytes(1);
    assert_eq!(table_of(&p, 1).kind, TableKind::Mbr);
    assert!(b[512..1024].iter().all(|&x| x == 0), "primary GPT header cleared");
    assert!(b[tail..].iter().all(|&x| x == 0), "backup GPT header cleared");
}

#[test]
fn execute_move_and_grow_extends_the_relocated_volume() {
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![entry(1, MIB, 8 * MIB, gpt_types::BASIC_DATA, 0)])), meta(1));
    p.set_system_disk(ELSEWHERE);
    p.add_volume(volume("v1", Some(FsKind::Ntfs), &[], 8 * MIB, 6 * MIB), 1, MIB, 8 * MIB);
    // the volume the OS exposes at the new position (MBR volume ids follow the offset)
    p.add_volume(volume("v1-moved", Some(FsKind::Ntfs), &[], 8 * MIB, 6 * MIB), 1, 10 * MIB, 12 * MIB);
    let r = run(&p, &[Operation::ResizeMove { disk: 1, number: 1, new_start: 10 * MIB, new_len: 12 * MIB }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert!(p.calls().contains(&format!("extend v1-moved {}", 12 * MIB)), "{:?}", p.calls());
}

// ---------------------------------------------------------------- fix round 2

#[test]
fn refs_can_grow_but_cannot_be_shrunk_online() {
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![entry(1, MIB, 20 * MIB, gpt_types::BASIC_DATA, 0)])), meta(1));
    p.set_system_disk(ELSEWHERE);
    p.add_volume(volume("r1", Some(FsKind::ReFs), &[], 20 * MIB, 15 * MIB), 1, MIB, 20 * MIB);
    let model = Model::from_platform(&p).unwrap();

    let shrink = Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 10 * MIB };
    let e = pm_ops::precheck(&model, &shrink).unwrap_err().to_string();
    assert!(spaced(&e).contains("only NTFS volumes can be shrunk online"), "{e}");
    let r = run(&p, &[shrink]);
    assert!(r.failed.is_some());
    assert!(p.calls().is_empty() && p.write_sessions_log().is_empty(), "{:?}", p.calls());

    let grow = Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 30 * MIB };
    assert!(pm_ops::precheck(&model, &grow).is_ok());
    let r = run(&p, &[grow]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert!(p.calls().contains(&format!("extend r1 {}", 30 * MIB)), "{:?}", p.calls());

    // a ReFS source larger than its clone target is refused, never shrunk
    let (p, _) = shrink_fixture(FsKind::ReFs, "src-refs");
    let model = Model::from_platform(&p).unwrap();
    assert!(fit_clone().shrinks_source(&model).is_empty());
    let before = p.disk_bytes(1);
    let r = run(&p, &[fit_clone()]);
    let (_, msg) = r.failed.expect("refused");
    assert!(spaced(&msg).contains("(ReFS)") && msg.contains("only NTFS volumes can be shrunk online"), "{msg}");
    assert!(p.calls().is_empty(), "{:?}", p.calls());
    assert!(p.write_sessions_log().is_empty());
    assert_eq!(p.disk_bytes(1), before);
}

// ---------------------------------------------------------------- final review fixes

#[test]
fn shrink_of_a_partition_with_unknown_filesystem_is_refused() {
    // partition 1 carries an NTFS boot sector, but no volume reported it (fs unknown)
    let mut dev = gpt_dev(DISK_LEN, 0, Some(vec![entry(1, MIB, 20 * MIB, gpt_types::BASIC_DATA, 0)]));
    let mut bs = vec![0u8; 512];
    bs[3..11].copy_from_slice(b"NTFS    ");
    bs[510] = 0x55;
    bs[511] = 0xAA;
    dev.write_at(MIB, &bs).unwrap();
    let p = MockPlatform::new();
    p.add_disk(1, dev, meta(1));
    p.set_system_disk(ELSEWHERE);
    let model = Model::from_platform(&p).unwrap();
    assert_eq!(model.partition(1, 1).unwrap().fs, None);

    let shrink = Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 10 * MIB };
    let msg = pm_ops::precheck(&model, &shrink).unwrap_err().to_string();
    assert!(spaced(&msg).contains("filesystem unknown; refresh or check the volume"), "{msg}");
    assert!(OpQueue::new(model.clone()).push(shrink.clone()).is_err());

    let before = p.disk_bytes(1);
    let r = run(&p, &[shrink]);
    let (_, msg) = r.failed.expect("refused");
    assert!(msg.contains("filesystem unknown"), "{msg}");
    assert!(p.write_sessions_log().is_empty(), "no session was opened");
    assert_eq!(p.disk_bytes(1), before);

    // growing it only changes the table
    let r = run(&p, &[Operation::ResizeMove { disk: 1, number: 1, new_start: MIB, new_len: 24 * MIB }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert_eq!(table_of(&p, 1).entries[0].len, 24 * MIB);
}

/// Disk 1 (48 MiB, MBR): extended container 1 holding logicals 5, 6, 7 of 8 MiB each, filled
/// with patterns 5, 6 and 7.
fn mbr_logicals() -> MockPlatform {
    let mut dev = MemDevice::new(DISK_LEN);
    let mut t = Table::empty(TableKind::Mbr, DISK_LEN, 512);
    let e = |number, start, len, ptype, logical| Entry {
        number,
        start,
        len,
        ptype: PartitionType::Mbr(ptype),
        guid: None,
        name: String::new(),
        attrs: 0,
        logical,
    };
    t.entries = vec![
        e(1, MIB, 29 * MIB, 0x0F, false),
        e(5, 2 * MIB, 8 * MIB, 0x07, true),
        e(6, 11 * MIB, 8 * MIB, 0x07, true),
        e(7, 20 * MIB, 8 * MIB, 0x07, true),
    ];
    write_table(&mut dev, &t).unwrap();
    for (seed, start) in [(5u8, 2 * MIB), (6, 11 * MIB), (7, 20 * MIB)] {
        dev.write_at(start + MIB, &pattern(MIB as usize, seed)).unwrap();
    }
    let p = MockPlatform::new();
    p.add_disk(1, dev, DiskInfo { serial: "SER-1".into(), ..meta(1) });
    p.set_system_disk(ELSEWHERE);
    p
}

fn logicals_of(p: &MockPlatform) -> Vec<(u32, u64)> {
    table_of(p, 1).entries.iter().filter(|e| e.logical).map(|e| (e.number, e.start)).collect()
}

#[test]
fn deleting_a_logical_renumbers_the_rest_in_the_preview_and_on_disk() {
    let p = mbr_logicals();
    let model = Model::from_platform(&p).unwrap();
    let mut q = OpQueue::new(model);
    q.push(Operation::DeletePartition { disk: 1, number: 5 }).unwrap();
    // the preview numbers the logicals as the disk will after the delete
    let shown: Vec<(u32, u64)> = q.preview().disk(1).unwrap().partitions.iter()
        .filter(|p| p.logical).map(|p| (p.number, p.start)).collect();
    assert_eq!(shown, vec![(5, 11 * MIB), (6, 20 * MIB)]);
    // the user wipes the partition the preview shows as 5 (the old 6)
    q.push(Operation::WipePartition { disk: 1, number: 5, method: WipeMethod::Zero }).unwrap();
    let r = pm_ops::execute_queued(&p, q.base(), q.ops(), &opts(), &NullProgress, &CancelToken::new())
        .unwrap();
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert_eq!(logicals_of(&p), vec![(5, 11 * MIB), (6, 20 * MIB)]);
    let b = p.disk_bytes(1);
    let at = |start: u64| &b[(start + MIB) as usize..(start + 2 * MIB) as usize];
    assert!(at(11 * MIB).iter().all(|&x| x == 0), "the old 6 was wiped");
    assert!(at(20 * MIB) == &pattern(MIB as usize, 7)[..], "the old 7 is untouched");
    assert_sessions_finished(&p);

    // a list still numbered as before the delete stops instead of hitting the old 7
    let p = mbr_logicals();
    let ops = [
        Operation::DeletePartition { disk: 1, number: 5 },
        Operation::WipePartition { disk: 1, number: 7, method: WipeMethod::Zero },
    ];
    let r = run(&p, &ops);
    let (i, msg) = r.failed.expect("refused");
    assert_eq!(i, 1, "{msg}");
    assert!(msg.contains("partition 7"), "{msg}");
    let b = p.disk_bytes(1);
    assert!(b[(21 * MIB) as usize..(22 * MIB) as usize] == pattern(MIB as usize, 7)[..]);
    assert!(b[(12 * MIB) as usize..(13 * MIB) as usize] == pattern(MIB as usize, 6)[..]);
}

#[test]
fn operations_stop_when_the_disk_changed_since_they_were_queued() {
    let wipe = Operation::WipePartition { disk: 1, number: 6, method: WipeMethod::Zero };
    let queued_on = |p: &MockPlatform| Model::from_platform(p).unwrap();

    // another disk (different size and serial) now has index 1
    let p = mbr_logicals();
    let base = queued_on(&p);
    let mut other = mbr_logicals().disk_bytes(1);
    other.resize((DISK_LEN + 8 * MIB) as usize, 0);
    p.add_disk(1, MemDevice::from_vec(other), DiskInfo { serial: "SER-2".into(), ..meta(1) });
    let before = p.disk_bytes(1);
    for dry_run in [true, false] {
        let o = ExecOpts { dry_run, ..opts() };
        let r = pm_ops::execute_queued(&p, &base, &[wipe.clone()], &o, &NullProgress, &CancelToken::new())
            .unwrap();
        let (_, msg) = r.failed.expect("refused");
        assert!(msg.contains("disk layout changed since the operations were queued"), "{msg}");
    }
    assert!(p.write_sessions_log().is_empty());
    assert_eq!(p.disk_bytes(1), before, "nothing written");

    // same size, only the serial number differs
    let p = mbr_logicals();
    let base = queued_on(&p);
    p.add_disk(1, MemDevice::from_vec(p.disk_bytes(1)), DiskInfo { serial: "SER-9".into(), ..meta(1) });
    let r = pm_ops::execute_queued(&p, &base, &[wipe.clone()], &opts(), &NullProgress, &CancelToken::new())
        .unwrap();
    assert!(r.failed.unwrap().1.contains("serial"));
    assert!(p.write_sessions_log().is_empty());

    // partition 6 was replaced by one at another position
    let p = mbr_logicals();
    let base = queued_on(&p);
    let mut t = table_of(&p, 1);
    t.entries.retain(|e| e.number != 6);
    t.entries.push(Entry { start: 12 * MIB, len: 6 * MIB, number: 6, ..t.entries[1].clone() });
    let mut dev = MemDevice::from_vec(p.disk_bytes(1));
    write_table(&mut dev, &t).unwrap();
    p.add_disk(1, dev, DiskInfo { serial: "SER-1".into(), ..meta(1) });
    let r = pm_ops::execute_queued(&p, &base, &[wipe], &opts(), &NullProgress, &CancelToken::new())
        .unwrap();
    let msg = r.failed.unwrap().1;
    assert!(msg.contains("disk layout changed") && msg.contains("partition 6"), "{msg}");
    assert!(p.write_sessions_log().is_empty());

    // unchanged: runs
    let p = mbr_logicals();
    let base = queued_on(&p);
    let r = pm_ops::execute_queued(
        &p,
        &base,
        &[Operation::WipePartition { disk: 1, number: 6, method: WipeMethod::Zero }],
        &opts(),
        &NullProgress,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(r.failed, None, "{:?}", r.log);
}

#[test]
fn partitions_created_earlier_in_the_run_keep_their_identity() {
    // create (new GUID), then convert the table (new GUIDs and disk id), then wipe what was
    // created: the identifiers the planner invented are matched to those really written
    let p = MockPlatform::new();
    p.add_disk(1, gpt_dev(DISK_LEN, 0, Some(vec![])), meta(1));
    p.set_system_disk(ELSEWHERE);
    let mut q = OpQueue::new(Model::from_platform(&p).unwrap());
    q.push(create(1, MIB, 8 * MIB, None, None)).unwrap();
    q.push(Operation::WipePartition { disk: 1, number: 1, method: WipeMethod::Zero }).unwrap();
    q.push(Operation::ConvertTable { disk: 1, to: TableKind::Mbr }).unwrap();
    q.push(Operation::WipePartition { disk: 1, number: 1, method: WipeMethod::Zero }).unwrap();
    let r = pm_ops::execute_queued(&p, q.base(), q.ops(), &opts(), &NullProgress, &CancelToken::new())
        .unwrap();
    assert_eq!(r.failed, None, "{:?}", r.log);
    assert_eq!(r.completed, 4);
}

#[test]
fn converting_a_4kn_gpt_to_mbr_keeps_data_at_the_end_of_the_disk() {
    const SS: u64 = 4096;
    let len = 64 * MIB;
    let mut dev = MemDevice::new(len).with_sector_size(SS as u32);
    let mut t = Table::empty(TableKind::Gpt, len, SS as u32);
    // as Windows lays out a 4Kn GPT: the backup array (4 sectors) and header end the disk
    t.gpt_last_usable = len - 5 * SS;
    t.entries = vec![entry(1, MIB, t.gpt_last_usable - MIB, gpt_types::BASIC_DATA, 0)];
    write_table(&mut dev, &t).unwrap();
    let tail_start = t.gpt_last_usable - 33 * SS;
    let tail = pattern((33 * SS) as usize, 4);
    dev.write_at(tail_start, &tail).unwrap();
    let p = MockPlatform::new();
    p.add_disk(1, dev, meta(1));
    p.set_system_disk(ELSEWHERE);

    let r = run(&p, &[Operation::ConvertTable { disk: 1, to: TableKind::Mbr }]);
    assert_eq!(r.failed, None, "{:?}", r.log);
    let b = p.disk_bytes(1);
    assert!(
        b[tail_start as usize..(tail_start + 33 * SS) as usize] == tail[..],
        "the end of the partition survives the GPT cleanup"
    );
    // the backup GPT itself is gone, and so is the primary header
    assert!(b[(len - SS) as usize..].iter().all(|&x| x == 0));
    assert!(b[SS as usize..(2 * SS) as usize].iter().all(|&x| x == 0));
    let t = table_of(&p, 1);
    assert_eq!(t.kind, TableKind::Mbr);
    assert_eq!(t.entries.len(), 1);
}
