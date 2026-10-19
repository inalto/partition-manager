//! End-to-end tests for `pm apply`, the operation subcommands, `pm recover` and `pm analyze`:
//! run the `pm` binary on image files through `--disk-file` (no admin, no real disks).

mod common;

use common::*;
use pm_core::{gpt_types, BlockDevice, FileDevice, PartitionType, TableKind, MIB};
use pm_ops::Operation;
use pm_parttable::{read_table, Table};
use std::path::Path;

fn blank(path: &Path, len: u64) {
    FileDevice::create(path, len).unwrap();
}

fn table_of(path: &Path) -> Table {
    let mut dev = FileDevice::open(path, true).unwrap();
    read_table(&mut dev).unwrap()
}

fn write_ops(path: &Path, ops: &[Operation]) {
    std::fs::write(path, serde_json::to_string_pretty(ops).unwrap()).unwrap();
}

fn create_op(start: u64, len: u64, name: &str) -> Operation {
    Operation::CreatePartition {
        disk: 0,
        start,
        len,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        fs: None,
        label: String::new(),
        letter: None,
        name: name.into(),
    }
}

/// Zero LBA 0..34: the protective MBR and the primary GPT.
fn wipe_primary_gpt(img: &Path) {
    let mut dev = FileDevice::open(img, false).unwrap();
    dev.write_at(0, &vec![0u8; 34 * 512]).unwrap();
    dev.flush().unwrap();
}

#[test]
fn cli_apply_json_creates_partitions_on_image() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    blank(&img, 64 * MIB);
    let ops = dir.path().join("ops.json");
    write_ops(
        &ops,
        &[
            Operation::InitializeDisk { disk: 0, kind: TableKind::Gpt },
            create_op(MIB, 20 * MIB, "First"),
            create_op(22 * MIB, 30 * MIB, "Second"),
        ],
    );

    let out = pm(&["--disk-file", s(&img), "apply", s(&ops), "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Initialize Disk 0 as GPT"), "{text}");
    assert!(text.contains("Create partition on Disk 0"), "{text}");

    let out = pm(&["table", s(&img)]);
    let text = stdout(&out);
    assert!(text.contains("GPT") && text.contains("First") && text.contains("Second"), "{text}");
    assert!(text.contains(&MIB.to_string()) && text.contains(&(22 * MIB).to_string()), "{text}");
    let t = table_of(&img);
    assert_eq!(t.entries.len(), 2);
    assert_eq!((t.entries[0].start, t.entries[0].len), (MIB, 20 * MIB));
    assert_eq!((t.entries[1].start, t.entries[1].len), (22 * MIB, 30 * MIB));

    // `pm list` sees the same disk through the file platform
    let out = pm(&["--disk-file", s(&img), "list", "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let disks: Vec<pm_core::DiskInfo> = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(disks.len(), 1);
    assert_eq!(disks[0].partitions.len(), 2);
}

#[test]
fn cli_apply_accepts_hand_written_json_and_a_single_object() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    blank(&img, 16 * MIB);
    let ops = dir.path().join("ops.json");
    std::fs::write(&ops, r#"{"InitializeDisk": {"disk": 0, "kind": "Mbr"}}"#).unwrap();
    // the global option may follow the subcommand
    let out = pm(&["apply", s(&ops), "--yes", "--disk-file", s(&img)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(table_of(&img).kind, TableKind::Mbr);
}

#[test]
fn cli_apply_dry_run_leaves_image_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    blank(&img, 64 * MIB);
    let ops = dir.path().join("ops.json");
    write_ops(&ops, &[Operation::InitializeDisk { disk: 0, kind: TableKind::Gpt }, create_op(MIB, 20 * MIB, "A")]);
    let before = read_all(&img);

    let out = pm(&["--disk-file", s(&img), "apply", s(&ops), "--dry-run"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("would: Initialize Disk 0 as GPT"), "{}", stdout(&out));
    assert_eq!(read_all(&img), before);

    // --dry-run wins over --yes
    let out = pm(&["--disk-file", s(&img), "apply", s(&ops), "--dry-run", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(read_all(&img), before);

    // without --yes nothing is written either, and the user is told how to proceed
    let out = pm(&["--disk-file", s(&img), "apply", s(&ops)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Re-run with --yes to apply"), "{}", stdout(&out));
    assert_eq!(read_all(&img), before);
}

#[test]
fn cli_apply_planning_error_exits_1_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    blank(&img, 64 * MIB);
    let ops = dir.path().join("ops.json");
    // the second partition overlaps the first
    write_ops(
        &ops,
        &[
            Operation::InitializeDisk { disk: 0, kind: TableKind::Gpt },
            create_op(MIB, 20 * MIB, "A"),
            create_op(10 * MIB, 20 * MIB, "B"),
        ],
    );
    let before = read_all(&img);
    let out = pm(&["--disk-file", s(&img), "apply", s(&ops), "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("overlaps"), "{}", stderr(&out));
    assert_eq!(read_all(&img), before);

    // unparsable file
    std::fs::write(&ops, "not json").unwrap();
    let out = pm(&["--disk-file", s(&img), "apply", s(&ops), "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    // a missing --disk-file image
    let out = pm(&["--disk-file", s(&dir.path().join("nope.img")), "list"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn cli_ops_subcommands_build_and_run() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    blank(&img, 64 * MIB);
    let run = |extra: &[&str]| {
        let mut a = vec!["--disk-file", s(&img)];
        a.extend_from_slice(extra);
        pm(&a)
    };

    let out = run(&["init-disk", "0", "--gpt", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let out = run(&["create", "disk:0", "--start", "1M", "--len", "20M", "--name", "Alpha", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let out = run(&["create", "0", "--start", "22M", "--len", "10M", "--type", "linux", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let t = table_of(&img);
    assert_eq!(t.entries.len(), 2);
    assert_eq!(t.entries[0].name, "Alpha");
    assert_eq!(t.entries[1].ptype, PartitionType::Gpt(gpt_types::LINUX_FS));

    // no --yes: nothing happens
    let before = read_all(&img);
    let out = run(&["delete", "0", "2"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Re-run with --yes"), "{}", stdout(&out));
    assert_eq!(read_all(&img), before);

    let out = run(&["set-type", "0", "2", "--type", "basic", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(table_of(&img).entries[1].ptype, PartitionType::Gpt(gpt_types::BASIC_DATA));

    let out = run(&["resize", "0", "1", "--start", "1M", "--len", "21M", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(table_of(&img).entries[0].len, 21 * MIB);

    let out = run(&["delete", "0", "2", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(table_of(&img).entries.len(), 1);

    // --emit-json prints the operation and touches nothing
    let before = read_all(&img);
    let out = run(&["wipe-disk", "0", "--method", "dod3", "--emit-json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let op: Operation = serde_json::from_str(&stdout(&out)).expect("operation JSON");
    assert!(matches!(op, Operation::WipeDisk { disk: 0, method: pm_ops::WipeMethod::Dod3 }), "{op:?}");
    assert_eq!(read_all(&img), before);

    // operations that need a volume layer are refused up front with a clear message
    let out = run(&["format", "0", "1", "--fs", "ntfs", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("image files"), "{}", stderr(&out));
    let out = run(&["create", "0", "--start", "40M", "--len", "10M", "--fs", "ntfs", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(read_all(&img), before);

    // bad arguments are usage errors
    assert_eq!(run(&["init-disk", "0", "--yes"]).status.code(), Some(1));
    assert_eq!(run(&["create", "0", "--start", "1M", "--len", "x"]).status.code(), Some(1));
    assert_eq!(run(&["delete", "7", "1", "--yes"]).status.code(), Some(1));
}

#[test]
fn cli_clone_disk_image_to_image_proportional() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("dst.img");
    gpt_fat32_image(&src, 64 * MIB, MIB, 40 * MIB);
    blank(&dst, 128 * MIB);
    let ls_src = pm(&["ls", &format!("{}:1", s(&src))]);
    assert!(ls_src.status.success(), "stderr: {}", stderr(&ls_src));
    assert!(stdout(&ls_src).contains("HELLO.TXT"), "{}", stdout(&ls_src));

    let out = pm(&[
        "--disk-file", s(&src), "--disk-file", s(&dst),
        "clone-disk", "0", "1", "--sizing", "proportional", "--yes",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Clone Disk 0"), "{}", stdout(&out));

    let t = table_of(&dst);
    assert_eq!(t.kind, TableKind::Gpt);
    assert_eq!(t.entries.len(), 1);
    assert!(t.entries[0].len >= 40 * MIB, "{:?}", t.entries[0]);
    assert_eq!(t.entries[0].name, "Data");

    let ls_dst = pm(&["ls", &format!("{}:1", s(&dst))]);
    assert!(ls_dst.status.success(), "stderr: {}", stderr(&ls_dst));
    assert_eq!(stdout(&ls_dst), stdout(&ls_src));
    let cat = pm(&["cat", &format!("{}:1", s(&dst)), "/DATA.BIN"]);
    assert_eq!(cat.stdout, data_bin());
    // the source is untouched
    assert_eq!(table_of(&src).entries.len(), 1);
}

#[test]
fn cli_clone_disk_dry_run_writes_nothing_and_self_clone_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("dst.img");
    gpt_fat32_image(&src, 64 * MIB, MIB, 40 * MIB);
    blank(&dst, 128 * MIB);
    let before = read_all(&dst);
    let out = pm(&["--disk-file", s(&src), "--disk-file", s(&dst), "clone-disk", "0", "1", "--dry-run"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("would: Clone Disk 0"), "{}", stdout(&out));
    assert_eq!(read_all(&dst), before);

    let out = pm(&["--disk-file", s(&src), "clone-disk", "0", "0", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn cli_image_and_restore_ops_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("dst.img");
    let saved = dir.path().join("saved.img");
    gpt_fat32_image(&src, 64 * MIB, MIB, 40 * MIB);
    blank(&dst, 64 * MIB);

    let out = pm(&["--disk-file", s(&src), "image-disk", "0", s(&saved), "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(saved.exists());
    let out = pm(&["--disk-file", s(&dst), "restore-image", s(&saved), "0", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let out = pm(&["cat", &format!("{}:1", s(&dst)), "/HELLO.TXT"]);
    assert_eq!(out.stdout, HELLO);
}

#[test]
fn cli_recover_partitions_on_wiped_image() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    gpt_fat32_image(&img, 64 * MIB, MIB, 40 * MIB);
    wipe_primary_gpt(&img);
    assert!(table_of(&img).entries.is_empty());

    // listing never writes
    let before = read_all(&img);
    let out = pm(&["recover", "partitions", s(&img)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("FAT32") && text.contains(&MIB.to_string()), "{text}");
    assert_eq!(read_all(&img), before);

    // --apply without --yes only says what it would do
    let out = pm(&["recover", "partitions", s(&img), "--apply"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("Re-run with --yes"), "{}", stdout(&out));
    assert_eq!(read_all(&img), before);

    let session = dir.path().join("found.json");
    let out = pm(&["recover", "partitions", s(&img), "--apply", "--yes", "--session", s(&session)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let t = table_of(&img);
    assert_eq!(t.kind, TableKind::Gpt);
    assert_eq!(t.entries.len(), 1);
    assert_eq!((t.entries[0].start, t.entries[0].len), (MIB, 40 * MIB));
    let out = pm(&["table", s(&img)]);
    assert!(stdout(&out).contains(&MIB.to_string()), "{}", stdout(&out));
    // the filesystem is reachable again through the recovered table
    let out = pm(&["ls", &format!("{}:1", s(&img))]);
    assert!(stdout(&out).contains("HELLO.TXT"), "{}", stdout(&out));

    let out = pm(&["recover", "open-session", s(&session)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("FAT32"), "{}", stdout(&out));
}

#[test]
fn cli_recover_partitions_through_the_platform_disk() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    gpt_fat32_image(&img, 64 * MIB, MIB, 40 * MIB);
    wipe_primary_gpt(&img);
    let out = pm(&["--disk-file", s(&img), "recover", "partitions", "disk:0", "--quick", "--apply", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(table_of(&img).entries.len(), 1);
}

#[test]
fn cli_recover_files_lists_and_restores_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    gpt_fat32_image_with(&img, 64 * MIB, MIB, 40 * MIB, |b| {
        b.add_file(2, "GONE.BIN", &pattern(20_000, 5));
        b.delete(2, "GONE.BIN");
    });
    let spec = format!("{}:1", s(&img));
    let out = pm(&["recover", "files", &spec]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // FAT replaces the first character of a deleted name
    assert!(stdout(&out).to_ascii_lowercase().contains("one.bin"), "{}", stdout(&out));

    let dest = dir.path().join("out");
    let out = pm(&["recover", "files", &spec, "--dest", s(&dest), "--min-integrity", "50"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let restored: Vec<_> = std::fs::read_dir(&dest).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(restored.len(), 1, "{restored:?} / {}", stdout(&out));
    assert_eq!(std::fs::read(&restored[0]).unwrap(), pattern(20_000, 5));
}

#[test]
fn cli_recover_carve_finds_embedded_jpeg() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("raw.img");
    let mut dev = FileDevice::create(&img, 4 * MIB).unwrap();
    let j = jpeg(3000);
    let off = 701 * 512;
    dev.write_at(off, &j).unwrap();
    dev.flush().unwrap();
    drop(dev);

    let out = pm(&["recover", "carve", s(&img)]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Image") && text.contains("JPEG"), "{text}");
    assert!(text.contains(&off.to_string()), "{text}");

    // --types filters
    let out = pm(&["recover", "carve", s(&img), "--types", "document"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(!stdout(&out).contains("JPEG"), "{}", stdout(&out));

    // --extract writes the file and a session can be saved
    let dest = dir.path().join("carved");
    let session = dir.path().join("carve.json");
    let out = pm(&[
        "recover", "carve", s(&img), "--types", "image", "--extract", "--dest", s(&dest),
        "--session", s(&session),
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let files: Vec<_> = std::fs::read_dir(&dest).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(std::fs::read(&files[0]).unwrap(), j);
    assert!(session.exists());
    let out = pm(&["recover", "open-session", s(&session)]);
    assert!(stdout(&out).contains("JPEG"), "{}", stdout(&out));

    // --extract needs somewhere to write; an unknown type is a usage error
    assert_eq!(pm(&["recover", "carve", s(&img), "--extract"]).status.code(), Some(1));
    assert_eq!(pm(&["recover", "carve", s(&img), "--types", "bogus"]).status.code(), Some(1));
}

#[test]
fn cli_analyze_space_on_temp_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("big")).unwrap();
    std::fs::create_dir_all(root.join("small/inner")).unwrap();
    std::fs::write(root.join("big/a.bin"), vec![1u8; 100_000]).unwrap();
    std::fs::write(root.join("big/b.txt"), vec![2u8; 20_000]).unwrap();
    std::fs::write(root.join("small/inner/c.txt"), vec![3u8; 5_000]).unwrap();

    let out = pm(&["analyze", "space", s(root), "--threads", "2"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let big = text.find("big").expect(&text);
    let small = text.find("small").expect(&text);
    assert!(big < small, "largest directory first: {text}");
    assert!(text.contains("bin") && text.contains("txt"), "{text}");
    assert!(text.contains("a.bin"), "{text}");

    let out = pm(&["analyze", "space", s(root), "--json", "--top", "2"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("JSON");
    assert_eq!(v["total"], 125_000);

    // a file is not a directory
    let out = pm(&["analyze", "space", s(&root.join("big/a.bin"))]);
    assert!(!out.status.success());
}

#[test]
fn cli_analyze_surface_on_image() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    blank(&img, 8 * MIB + 4096);
    let out = pm(&["analyze", "surface", s(&img), "--block", "1M"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Bad blocks: 0"), "{text}");
    assert!(text.contains("Slow blocks:"), "{text}");
    assert!(text.contains("........."), "map of ok blocks: {text}");
    assert_eq!(pm(&["analyze", "surface", s(&img), "--block", "0"]).status.code(), Some(1));
}

#[test]
fn cli_analyze_smart_reports_none_for_images() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    blank(&img, MIB);
    let out = pm(&["--disk-file", s(&img), "analyze", "smart", "0"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("No SMART data"), "{}", stdout(&out));
    let out = pm(&["--disk-file", s(&img), "analyze", "smart", "0", "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "null");
}

#[test]
fn cli_analyze_bench_runs_on_a_temp_directory() {
    let dir = tempfile::tempdir().unwrap();
    let out = pm(&[
        "analyze", "bench", s(dir.path()), "--sizes", "4K,8K", "--total", "1M", "--threads", "1",
        "--queue", "2", "--cooldown", "0",
    ]);
    if cfg!(windows) {
        assert!(out.status.success(), "stderr: {}", stderr(&out));
        let text = stdout(&out);
        assert!(text.contains("4.00 KB") && text.contains("8.00 KB"), "{text}");
        assert!(text.contains("MB/s") && text.contains("IOPS"), "{text}");
        // the scratch file is gone
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    } else {
        assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    }
}

#[test]
fn cli_image_filesystems_are_protected_from_resize_and_align() {
    let dir = tempfile::tempdir().unwrap();
    let img = dir.path().join("disk.img");
    gpt_fat32_image(&img, 128 * MIB, MIB, 40 * MIB);
    let run = |extra: &[&str]| {
        let mut a = vec!["--disk-file", s(&img)];
        a.extend_from_slice(extra);
        pm(&a)
    };

    // the file system kind and label are read from the image
    let out = run(&["list"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("FAT32"), "{}", stdout(&out));
    assert!(stdout(&out).contains("PMTEST"), "{}", stdout(&out));

    // a shrink (or any length change of a FAT volume) would truncate the file system: refused
    let before = read_all(&img);
    for len in ["20M", "60M"] {
        let out = run(&["resize", "0", "1", "--start", "1M", "--len", len, "--yes"]);
        assert_eq!(out.status.code(), Some(1), "{len}: {}", stderr(&out));
        assert_eq!(read_all(&img), before, "{len}");
    }
    let out = run(&["align", "0", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("image files"), "{}", stderr(&out));
    assert_eq!(read_all(&img), before);

    // moving it (same length) is fine and the files follow
    let out = run(&["resize", "0", "1", "--start", "60M", "--len", "40M", "--yes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(table_of(&img).entries[0].start, 60 * MIB);
    let out = pm(&["cat", &format!("{}:1", s(&img)), "/HELLO.TXT"]);
    assert_eq!(out.stdout, HELLO);
}

#[test]
fn cli_disk_file_must_be_a_raw_image_and_unique() {
    let dir = tempfile::tempdir().unwrap();
    let vhd = dir.path().join("disk.vhd");
    pm_image::create(
        &vhd,
        &pm_image::CreateOpts { format: pm_image::ImageFormat::VhdFixed, capacity: MIB, sparse: false },
    )
    .unwrap();
    let before = read_all(&vhd);
    let out = pm(&["--disk-file", s(&vhd), "list"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("only raw images"), "{}", stderr(&out));
    assert!(stderr(&out).contains("pm convert"), "{}", stderr(&out));
    assert_eq!(read_all(&vhd), before);

    let img = dir.path().join("a.img");
    blank(&img, MIB);
    let out = pm(&["--disk-file", s(&img), "--disk-file", s(&img), "list"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("more than once"), "{}", stderr(&out));
}
