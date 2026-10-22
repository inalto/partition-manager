//! End-to-end tests: run the `pm` binary as a child process.

use pm_core::{gpt_types, BlockDevice, FileDevice, PartitionType, TableKind, MIB};
use pm_parttable::{write_table, Entry, Table};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn pm(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pm"))
        .args(args)
        .output()
        .expect("spawn pm")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn gpt_entry(number: u32, start: u64, len: u64, name: &str) -> Entry {
    Entry {
        number,
        start,
        len,
        ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
        guid: Some(uuid_for(number)),
        name: name.into(),
        attrs: 0,
        logical: false,
    }
}

fn uuid_for(n: u32) -> uuid::Uuid {
    uuid::Uuid::from_u128(0x1000 + n as u128)
}

/// 64 MiB raw image holding a GPT with two entries; returns (path, table written).
fn make_gpt_image(dir: &Path) -> (PathBuf, Table) {
    let path = dir.join("disk.img");
    let mut dev = FileDevice::create(&path, 64 * MIB).unwrap();
    let mut t = Table::empty(TableKind::Gpt, dev.len(), 512);
    t.entries.push(gpt_entry(1, 2 * MIB, 10 * MIB, "First"));
    t.entries.push(gpt_entry(2, 20 * MIB, 16 * MIB, "Second"));
    write_table(&mut dev, &t).unwrap();
    dev.flush().unwrap();
    (path, t)
}

#[test]
fn cli_table_on_image_file_prints_gpt() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = make_gpt_image(dir.path());
    let p = path.to_str().unwrap();

    let out = pm(&["table", p]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("GPT"), "{text}");
    assert!(text.contains(&(2 * MIB).to_string()), "{text}");
    assert!(text.contains(&(20 * MIB).to_string()), "{text}");
    assert!(text.contains("First") && text.contains("Second"), "{text}");

    let out = pm(&["table", p, "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let t: Table = serde_json::from_str(&stdout(&out)).expect("valid Table JSON");
    assert_eq!(t.kind, TableKind::Gpt);
    assert_eq!(t.entries.len(), 2);
    assert_eq!(t.entries[0].start, 2 * MIB);
    assert_eq!(t.entries[1].start, 20 * MIB);

    // the explicit file: prefix selects the same device
    let out = pm(&["table", &format!("file:{p}"), "--json"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
}

#[test]
fn cli_table_on_missing_file_is_usage_error() {
    let out = pm(&["table", "definitely-not-a-device.img"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn cli_list_json_runs() {
    let out = pm(&["list", "--json"]);
    match out.status.code() {
        Some(0) => {
            let disks: Vec<pm_core::DiskInfo> =
                serde_json::from_str(&stdout(&out)).expect("valid DiskInfo JSON");
            let _ = disks;
        }
        Some(2) => {
            let err = stderr(&out);
            assert!(err.starts_with("error:"), "{err}");
            // access denied (with the elevation hint) or an OS without a platform layer yet
            assert!(
                err.contains("elevated") || err.contains("sudo") || err.contains("not supported"),
                "{err}"
            );
        }
        other => panic!("unexpected exit {other:?}: {}", stderr(&out)),
    }
}

#[test]
fn cli_doctor_runs_and_reports() {
    let out = pm(&["doctor"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    for key in ["Elevated", "Platform", "Toolchain", "Disks", "System disk", "Free letters", "Tools:"] {
        assert!(text.contains(key), "missing {key}: {text}");
    }
}

#[test]
fn cli_usage_errors_exit_1() {
    assert_eq!(pm(&[]).status.code(), Some(1));
    assert_eq!(pm(&["bogus"]).status.code(), Some(1));
    // mount needs exactly one of --letter / --folder
    assert_eq!(pm(&["mount", "D:"]).status.code(), Some(1));
    assert_eq!(pm(&["mount", "D:", "--letter", "E", "--folder", "x"]).status.code(), Some(1));
    assert_eq!(pm(&["unmount", "D:", "--letter", "1"]).status.code(), Some(1));
    assert_eq!(pm(&["--help"]).status.code(), Some(0));
}

/// A drive letter that no volume on this machine uses (`None` if `pm list` itself fails).
fn unused_letter() -> Option<char> {
    let out = pm(&["list", "--json"]);
    if out.status.code() != Some(0) {
        return None;
    }
    let disks: Vec<pm_core::DiskInfo> = serde_json::from_str(&stdout(&out)).ok()?;
    let used: Vec<char> = disks
        .iter()
        .flat_map(|d| d.partitions.iter().filter_map(|p| p.letter()))
        .collect();
    ('D'..='Z').rev().find(|c| !used.contains(c))
}

#[test]
fn cli_unknown_volume_exits_1() {
    // an unknown disk index is an argument error, not a platform failure
    let out = pm(&["table", "disk:99999"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).starts_with("error:"), "{}", stderr(&out));

    // so is a device node that does not exist (Linux, macOS syntax; parsed on every OS)
    let out = pm(&["table", "/dev/pm-no-such-node"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).starts_with("error:"), "{}", stderr(&out));

    // so is a drive letter nothing is mounted at, for every command that takes a volume
    // (drive letters exist on Windows only)
    if !cfg!(windows) {
        return;
    }
    let Some(c) = unused_letter() else { return };
    let letter = format!("{c}:");
    let out = pm(&["table", &letter]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let out = pm(&["mount", &letter, "--letter", "Y"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let out = pm(&["unmount", &letter, "--letter", "Y"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn cli_missing_file_exits_1_for_both_forms() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.img");
    let m = missing.to_str().unwrap();
    for arg in [m.to_string(), format!("file:{m}")] {
        let out = pm(&["table", &arg]);
        assert_eq!(out.status.code(), Some(1), "{arg}: {}", stderr(&out));
        assert!(stderr(&out).starts_with("error:"), "{}", stderr(&out));
    }
}
