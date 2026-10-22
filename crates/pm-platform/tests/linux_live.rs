//! Live Linux test on a loop device (needs root; `#[ignore]`d so CI never runs it):
//!
//! ```text
//! sudo -E env PATH=$PATH cargo test -p pm-platform --test linux_live -- --ignored --test-threads=1
//! ```
//!
//! A 64 MiB file is attached with `losetup -fP`, enumerated, given a GPT with two FAT
//! partitions through a write session (`BLKRRPART` must create the partition nodes), formatted
//! with `mkfs.vfat` through `format()`, mounted in temp dirs, and the begin_write rollback is
//! exercised with the second partition busy. The loop device is detached at the end, also when
//! the test fails.
#![cfg(target_os = "linux")]

use pm_core::{gpt_types, DiskBus, FsKind, NullProgress, PartitionType, TableKind, VolumeId, MIB};
use pm_parttable::{Entry, Table};
use pm_platform::linux::{parse_proc_mounts, LinuxPlatform};
use pm_platform::posix::{cmd, is_root};
use pm_platform::{MountPoint, OpenMode, Platform};
use std::path::Path;

/// Detaches the loop device on drop.
struct LoopGuard(String);

impl Drop for LoopGuard {
    fn drop(&mut self) {
        // unmount anything the test may have left behind, then detach
        for e in parse_proc_mounts(&std::fs::read_to_string("/proc/self/mounts").unwrap_or_default()) {
            if e.device.starts_with(&self.0) {
                let _ = cmd::run("umount", &[e.mount_point.as_str()]);
            }
        }
        match cmd::run("losetup", &["-d", self.0.as_str()]) {
            Ok(_) => eprintln!("detached {}", self.0),
            Err(e) => eprintln!("could not detach {}: {e}", self.0),
        }
    }
}

fn mounted_at(dev: &str) -> Vec<String> {
    parse_proc_mounts(&std::fs::read_to_string("/proc/self/mounts").unwrap())
        .into_iter()
        .filter(|e| e.device == dev)
        .map(|e| e.mount_point)
        .collect()
}

fn gpt_entry(number: u32, start: u64, len: u64, name: &str) -> Entry {
    Entry {
        number,
        start,
        len,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        guid: Some(uuid::Uuid::new_v4()),
        name: name.into(),
        attrs: 0,
        logical: false,
    }
}

#[test]
#[ignore = "needs root and loop devices"]
fn loop_device_end_to_end() {
    if !is_root() {
        eprintln!("skipped: run as root (sudo -E ...) to exercise loop devices");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("pm-live.img");
    std::fs::File::create(&img).unwrap().set_len(64 * MIB).unwrap();
    let out = cmd::run("losetup", &["-fP", "--show", img.to_str().unwrap()]).expect("losetup");
    let node = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let _guard = LoopGuard(node.clone());
    eprintln!("attached {} as {node}", img.display());

    let p = LinuxPlatform::new();
    let disks = p.enumerate_disks().unwrap();
    let d = disks.iter().find(|d| d.path == node).expect("the loop device is enumerated");
    assert_eq!(d.bus, DiskBus::Virtual);
    assert_eq!(d.size, 64 * MIB);
    assert_eq!(d.table, TableKind::None, "blank file: no table");
    assert!(!d.flags.system);
    let index = d.index;
    let raw = p.open_disk(index, OpenMode::ReadOnly).unwrap();
    assert_eq!(raw.len(), 64 * MIB);
    drop(raw);

    // GPT: p1 48 MiB at 1 MiB (FAT32 needs >= 65525 clusters: 32 MiB is too small for a
    // valid FAT32 even with 512-byte clusters, and pm-fs rightly rejects one), p2 8 MiB at 50 MiB
    let mut t = Table::empty(TableKind::Gpt, 64 * MIB, 512);
    t.entries.push(gpt_entry(1, MIB, 48 * MIB, "PMTEST"));
    t.entries.push(gpt_entry(2, 50 * MIB, 8 * MIB, "PMTEST2"));
    let mut s = p.begin_write(index).expect("begin_write");
    s.commit_table(&t).expect("commit_table (BLKRRPART)");
    s.finish().expect("finish");

    let p1 = format!("{node}p1");
    let p2 = format!("{node}p2");
    let sys = |n: &str| Path::new("/sys/class/block").join(n.trim_start_matches("/dev/"));
    assert!(sys(&p1).exists() && sys(&p2).exists(), "partition nodes after BLKRRPART");
    let d = p.enumerate_disks().unwrap().into_iter().find(|d| d.path == node).unwrap();
    assert_eq!(d.table, TableKind::Gpt);
    assert_eq!(d.partitions.len(), 2);
    assert_eq!((d.partitions[0].number, d.partitions[0].start, d.partitions[0].len), (1, MIB, 48 * MIB));
    assert_eq!(d.partitions[0].volume, Some(VolumeId(p1.clone())));
    assert_eq!(d.partitions[0].name, "PMTEST");

    // format both through the platform (mkfs.vfat)
    let v1 = VolumeId(p1.clone());
    let v2 = VolumeId(p2.clone());
    p.format(&v1, FsKind::Fat32, "PMTEST", Some(512), true, &NullProgress).expect("format p1");
    p.format(&v2, FsKind::Fat16, "PMTEST2", Some(512), true, &NullProgress).expect("format p2");

    // mount, write a file, locate the path, enumerate the mounted state
    let m1 = tempfile::tempdir().unwrap();
    let m2 = tempfile::tempdir().unwrap();
    let at1 = MountPoint::Folder(m1.path().to_path_buf());
    let at2 = MountPoint::Folder(m2.path().to_path_buf());
    p.mount(&v1, &at1).expect("mount p1");
    p.mount(&v2, &at2).expect("mount p2");
    std::fs::write(m1.path().join("hello.txt"), b"hello from pm").unwrap();
    assert_eq!(p.disk_of_path(&m1.path().join("hello.txt")), Some((index, Some(1))));
    assert_eq!(p.disk_of_path(&m2.path().join("not-yet-created.img")), Some((index, Some(2))));
    let d = p.enumerate_disks().unwrap().into_iter().find(|d| d.path == node).unwrap();
    let part = &d.partitions[0];
    assert_eq!(part.fs, Some(FsKind::Fat32));
    assert_eq!(part.label, "PMTEST");
    assert_eq!(part.mount_points, mounted_at(&p1));
    assert!(part.used.is_some());
    assert_eq!((d.partitions[1].fs, d.partitions[1].label.as_str()), (Some(FsKind::Fat16), "PMTEST2"));
    let vols = p.enumerate_volumes().unwrap();
    let v = vols.iter().find(|v| v.id == v1).unwrap();
    assert_eq!((v.disk, v.start, v.len), (Some(index), MIB, 48 * MIB));
    assert!(v.total > 0 && v.free <= v.total);
    assert!(matches!(p.format(&v1, FsKind::Fat32, "X", None, true, &NullProgress), Err(pm_core::Error::Busy(_))));
    // the whole partitioned disk is never a volume
    let whole = VolumeId(node.clone());
    assert!(matches!(p.format(&whole, FsKind::Fat32, "X", None, true, &NullProgress), Err(pm_core::Error::Invalid(_))));
    assert!(matches!(p.open_volume(&whole, OpenMode::ReadOnly), Err(pm_core::Error::Invalid(_))));

    // Review Focus 1: p2 busy (a file open on it) -> begin_write refuses and p1, already
    // unmounted, is mounted again
    let busy = std::fs::File::create(m2.path().join("busy.txt")).unwrap();
    match p.begin_write(index) {
        Err(pm_core::Error::Busy(msg)) => {
            eprintln!("begin_write refused as expected: {msg}");
            assert!(msg.contains("already unmounted were mounted again"), "{msg}");
        }
        Err(e) => panic!("expected Busy, got {e}"),
        Ok(_) => panic!("begin_write succeeded while p2 is busy"),
    }
    assert_eq!(mounted_at(&p1).len(), 1, "p1 mounted again after the refused session");
    assert_eq!(mounted_at(&p2).len(), 1, "p2 still mounted");
    assert_eq!(std::fs::read(m1.path().join("hello.txt")).unwrap(), b"hello from pm");
    // the original mount options came back too
    let opts = |dev: &str| {
        pm_platform::linux::parse_mountinfo(&std::fs::read_to_string("/proc/self/mountinfo").unwrap())
            .into_iter()
            .find(|e| e.device == dev)
            .map(|e| e.options)
            .unwrap()
    };
    let p1_options = opts(&p1);
    assert!(p1_options.contains("nosuid") && p1_options.contains("nodev"), "{p1_options}");
    drop(busy);

    // a session that writes nothing mounts everything it unmounted again
    let s = p.begin_write(index).expect("begin_write with both mounted");
    assert!(mounted_at(&p1).is_empty() && mounted_at(&p2).is_empty(), "unmounted during the session");
    drop(s);
    assert_eq!(mounted_at(&p1).len(), 1, "p1 mounted again after a session without writes");
    assert_eq!(mounted_at(&p2).len(), 1, "p2 mounted again after a session without writes");
    assert_eq!(opts(&p1), p1_options);
    // a session that rewrites the same layout mounts the unchanged partitions again
    let mut s = p.begin_write(index).expect("begin_write for a rewrite");
    s.commit_table(&t).expect("rewrite the same table");
    s.finish().expect("finish after the rewrite");
    assert_eq!(mounted_at(&p1).len(), 1, "p1 mounted again after an unchanged layout");
    assert_eq!(mounted_at(&p2).len(), 1, "p2 mounted again after an unchanged layout");
    assert_eq!(std::fs::read(m1.path().join("hello.txt")).unwrap(), b"hello from pm");

    p.unmount(&v1, &at1).expect("unmount p1");
    p.unmount(&v2, &at2).expect("unmount p2");
    assert!(mounted_at(&p1).is_empty() && mounted_at(&p2).is_empty());
    assert!(matches!(p.unmount(&v1, &at1), Err(pm_core::Error::NotFound(_))));
    p.rescan(index).expect("rescan");
    let smart = p.smart(index).expect("smart never fails on a missing/unsupported device");
    assert!(smart.is_none(), "a loop device has no S.M.A.R.T. data");

    // a session with everything unmounted succeeds and keeps the data
    let mut s = p.begin_write(index).expect("begin_write when idle");
    let mut sector = vec![0u8; 512];
    s.device().read_at(MIB + 82, &mut sector[..8]).unwrap();
    assert_eq!(&sector[..8], b"FAT32   ", "FAT32 boot sector of p1 seen through the session");
    s.finish().unwrap();
}
