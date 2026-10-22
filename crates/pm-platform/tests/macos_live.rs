//! Live test of [`MacPlatform`] on a file-backed disk image (a `.dmg` in a temp dir). Ignored
//! by default (needs root and macOS); run on a Mac with:
//!
//! ```text
//! sudo -E cargo test -p pm-platform --test macos_live -- --ignored --nocapture
//! ```
//!
//! Procedure (what the test does, also usable by hand):
//! 1. `hdiutil create -size 64m -layout NONE -o <tmp>/pm-live.dmg` (a blank image, no table);
//! 2. `hdiutil attach -nomount <tmp>/pm-live.dmg` → `/dev/diskN`;
//! 3. `enumerate_disks()` lists disk N (bus `Virtual`, 64 MiB, no table);
//! 4. `begin_write(N)` (`diskutil unmountDisk diskN`), `commit_table` of a GPT with one Basic
//!    Data partition at 1 MiB, `finish` (close → the kernel re-reads the map);
//! 5. `read_table` through `open_disk(N)` shows the partition and `diskutil list` shows
//!    `diskNs1`;
//! 6. `hdiutil detach /dev/diskN` (also on failure).
#![cfg(target_os = "macos")]

use pm_core::{gpt_types, PartitionType, TableKind, MIB};
use pm_parttable::{read_table, Entry, Table};
use pm_platform::{MacPlatform, OpenMode, Platform};
use std::process::Command;

/// Detaches the image when dropped.
struct Attached(String);

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil").args(["detach", "-force", &self.0]).status();
    }
}

fn hdiutil(args: &[&str]) -> String {
    let out = Command::new("hdiutil").args(args).output().expect("run hdiutil");
    assert!(out.status.success(), "hdiutil {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "needs root on macOS: attaches a disk image and writes a partition table to it"]
fn gpt_on_an_attached_image() {
    let p = MacPlatform::new();
    assert!(p.is_elevated(), "run with sudo");
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("pm-live.dmg");
    hdiutil(&["create", "-size", "64m", "-layout", "NONE", "-o", image.to_str().unwrap()]);
    let out = hdiutil(&["attach", "-nomount", image.to_str().unwrap()]);
    let node = out.split_whitespace().next().expect("device node").to_string();
    let _guard = Attached(node.clone());
    let index: u32 = node.trim_start_matches("/dev/disk").parse().expect("/dev/diskN");

    let disks = p.enumerate_disks().unwrap();
    let d = disks.iter().find(|d| d.index == index).expect("attached image listed");
    assert_eq!(d.size, 64 * MIB);
    assert_eq!(d.path, format!("/dev/rdisk{index}"));
    assert!(d.partitions.is_empty());
    assert_ne!(p.system_disk(), Some(index));

    let mut t = Table::empty(TableKind::Gpt, d.size, d.sector_size);
    let start = t.gpt_first_usable;
    t.entries.push(Entry {
        number: 1,
        start,
        len: 32 * MIB,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        guid: Some(uuid::Uuid::new_v4()),
        name: "pm-live".into(),
        attrs: 0,
        logical: false,
    });
    let mut s = p.begin_write(index).unwrap();
    s.commit_table(&t).unwrap();
    s.finish().unwrap();

    let mut dev = p.open_disk(index, OpenMode::ReadOnly).unwrap();
    let back = read_table(dev.as_mut()).unwrap();
    assert_eq!(back.kind, TableKind::Gpt);
    assert_eq!(back.entries.len(), 1);
    assert_eq!((back.entries[0].start, back.entries[0].len), (start, 32 * MIB));
    drop(dev);

    // the kernel re-read the map: diskutil lists the new slice, at the right offset
    let d = p.enumerate_disks().unwrap().into_iter().find(|d| d.index == index).unwrap();
    assert_eq!(d.table, TableKind::Gpt);
    assert_eq!(d.partitions.len(), 1);
    assert_eq!(d.partitions[0].start, start);
    assert_eq!(d.partitions[0].volume.as_ref().map(|v| v.0.clone()), Some(format!("/dev/disk{index}s1")));
}
