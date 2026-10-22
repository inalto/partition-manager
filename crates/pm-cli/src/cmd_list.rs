//! `pm list [--json]`: disks and partitions in a MiniTool-style table.

use crate::cli::CliError;
use pm_core::{format_size, DiskInfo, PartitionInfo, TableKind, MIB};
use pm_platform::Platform;
use std::io::Write;

const HEADERS: [&str; 7] = [
    "Partition",
    "Capacity",
    "Used",
    "Unused",
    "File system",
    "Type",
    "Status",
];

pub fn run(platform: &dyn Platform, json: bool, out: &mut dyn Write) -> Result<(), CliError> {
    let disks = platform.enumerate_disks()?;
    let text = if json {
        let mut s = serde_json::to_string_pretty(&disks)
            .map_err(|e| CliError::Usage(format!("cannot serialize disks: {e}")))?;
        s.push('\n');
        s
    } else {
        render_disks(&disks, platform.elevation_hint())
    };
    out.write_all(text.as_bytes())
        .map_err(|e| CliError::Runtime(pm_core::Error::io(0, 0, e)))
}

fn partition_label(p: &PartitionInfo) -> String {
    let letter = p.letter().map_or("*".to_string(), |c| c.to_string());
    format!("{letter}:{}", p.label)
}

fn partition_row(p: &PartitionInfo) -> [String; 7] {
    let (used, unused) = match p.used {
        Some(u) => (format_size(u), format_size(p.len.saturating_sub(u))),
        None => (String::new(), String::new()),
    };
    [
        partition_label(p),
        format_size(p.len),
        used,
        unused,
        p.fs.map_or("Other".to_string(), |f| f.display_name().to_string()),
        p.ptype.map_or(String::new(), |t| t.name()),
        p.status_text(),
    ]
}

fn unallocated_row(len: u64) -> [String; 7] {
    [
        "(Unallocated)".to_string(),
        format_size(len),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ]
}

/// Rows of one disk in start order: partitions interleaved with unallocated gaps >= 1 MiB.
fn disk_rows(d: &DiskInfo) -> Vec<[String; 7]> {
    let mut items: Vec<(u64, [String; 7])> = d.partitions.iter().map(|p| (p.start, partition_row(p))).collect();
    items.extend(d.free_ranges(MIB).into_iter().map(|(s, l)| (s, unallocated_row(l))));
    // stable: a partition and a gap never share a start, logicals keep table order
    items.sort_by_key(|(s, _)| *s);
    items.into_iter().map(|(_, r)| r).collect()
}

/// Header line of a disk: `Disk 1 (Model, GPT, 1.82 TB)  /dev/sdb`. The device path names the
/// disk for good: on Linux and macOS `disk:N` is a position that can change when a disk is
/// plugged in, so commands also take the path (`pm table /dev/sdb`).
fn disk_header(d: &DiskInfo) -> String {
    if d.path.is_empty() {
        d.display_name()
    } else {
        format!("{}  {}", d.display_name(), d.path)
    }
}

/// The full text output for `disks`, columns aligned across all disks; a disk whose table
/// could not be read advises `elevation_hint` (`Platform::elevation_hint`).
pub fn render_disks(disks: &[DiskInfo], elevation_hint: &str) -> String {
    if disks.is_empty() {
        return "No disks found.\n".to_string();
    }
    let all: Vec<Vec<[String; 7]>> = disks.iter().map(disk_rows).collect();
    let mut widths = HEADERS.map(|h| h.chars().count());
    for row in all.iter().flatten() {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| -> String {
        let mut s = String::new();
        for (i, (cell, w)) in cells.iter().zip(&widths).enumerate() {
            if i > 0 {
                s.push_str("  ");
            }
            s.push_str(cell);
            s.push_str(&" ".repeat(w - cell.chars().count()));
        }
        s.trim_end().to_string()
    };
    let header_cells: Vec<String> = HEADERS.iter().map(|h| h.to_string()).collect();
    let rule = widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("  ");

    let mut out = String::new();
    for (i, (d, rows)) in disks.iter().zip(&all).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&disk_header(d));
        if d.table == TableKind::Unknown {
            out.push_str(&format!(
                "  [partition table not readable, offsets approximate: {elevation_hint} for exact offsets]"
            ));
        }
        out.push('\n');
        out.push_str(&line(&header_cells));
        out.push('\n');
        out.push_str(&rule);
        out.push('\n');
        for r in rows {
            out.push_str(&line(r));
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{gpt_types, FsKind, PartFlags, PartitionType, GIB};

    fn sample() -> DiskInfo {
        DiskInfo {
            index: 1,
            model: "Test Disk".into(),
            size: 10 * GIB,
            sector_size: 512,
            table: TableKind::Gpt,
            partitions: vec![
                PartitionInfo {
                    number: 1,
                    start: MIB,
                    len: 100 * MIB,
                    ptype: Some(PartitionType::Gpt(gpt_types::EFI_SYSTEM)),
                    fs: Some(FsKind::Fat32),
                    mount_points: vec![],
                    flags: PartFlags::default(),
                    ..Default::default()
                },
                PartitionInfo {
                    number: 2,
                    start: GIB,
                    len: 4 * GIB,
                    ptype: Some(PartitionType::Gpt(gpt_types::BASIC_DATA)),
                    fs: Some(FsKind::Ntfs),
                    label: "Data".into(),
                    mount_points: vec![r"D:\".into()],
                    used: Some(GIB),
                    flags: PartFlags {
                        boot: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn renders_header_rows_and_gaps() {
        let text = render_disks(&[sample()], "HINT");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Disk 1 (Test Disk, GPT, 10.00 GB)");
        for h in HEADERS {
            assert!(lines[1].contains(h), "{}", lines[1]);
        }
        let efi = lines.iter().find(|l| l.starts_with("*:")).unwrap();
        assert!(efi.contains("100.00 MB") && efi.contains("FAT32") && efi.contains("System"), "{efi}");
        let data = lines.iter().find(|l| l.starts_with("D:Data")).unwrap();
        assert!(data.contains("4.00 GB") && data.contains("1.00 GB") && data.contains("3.00 GB"), "{data}");
        assert!(data.contains("NTFS") && data.contains("Boot"), "{data}");
        // gap between the partitions (101 MiB..1 GiB) and the tail after partition 2
        assert_eq!(lines.iter().filter(|l| l.starts_with("(Unallocated)")).count(), 2);
        // columns line up: every row's Capacity starts at the same offset as the header's
        let col = lines[1].find("Capacity").unwrap();
        assert!(efi[col..].starts_with("100.00 MB"), "{efi}");
        assert!(data[col..].starts_with("4.00 GB"), "{data}");
        // rows are in start order
        let pos = |needle: &str| lines.iter().position(|l| l.starts_with(needle)).unwrap();
        assert!(pos("*:") < pos("(Unallocated)") && pos("(Unallocated)") < pos("D:Data"));
    }

    #[test]
    fn empty_and_unknown_cases() {
        assert_eq!(render_disks(&[], "HINT"), "No disks found.\n");
        let blank = DiskInfo {
            index: 0,
            model: "Blank".into(),
            size: 2 * GIB,
            table: TableKind::None,
            ..Default::default()
        };
        let text = render_disks(&[blank], "HINT");
        assert!(text.starts_with("Disk 0 (Blank, Uninitialized, 2.00 GB)"));
        assert!(text.contains("(Unallocated)"));
    }

    #[test]
    fn json_roundtrips() {
        let disks = vec![sample()];
        let mut buf = Vec::new();
        let plat = pm_platform::MockPlatform::new();
        run(&plat, true, &mut buf).unwrap();
        let back: Vec<DiskInfo> = serde_json::from_slice(&buf).unwrap();
        assert!(back.is_empty());
        let s = serde_json::to_string_pretty(&disks).unwrap();
        let back: Vec<DiskInfo> = serde_json::from_str(&s).unwrap();
        assert_eq!(back, disks);
    }

    #[test]
    fn disks_listed_by_index_with_their_paths() {
        use pm_core::MemDevice;
        use pm_platform::MockPlatform;
        // Linux keeps a removed disk's index: 0, 2 and 5 are listed, in index order
        let p = MockPlatform::new();
        for (i, path) in [(5, "/dev/sdf"), (0, "/dev/sda"), (2, "/dev/sdc")] {
            p.add_disk(i, MemDevice::new(4 * MIB), DiskInfo { model: "M".into(), path: path.into(), ..Default::default() });
        }
        let mut out = Vec::new();
        run(&p, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let headers: Vec<&str> = text.lines().filter(|l| l.starts_with("Disk ")).collect();
        assert_eq!(headers.len(), 3, "{text}");
        assert!(headers[0].starts_with("Disk 0 (M,") && headers[0].ends_with("  /dev/sda"), "{text}");
        assert!(headers[1].starts_with("Disk 2 (") && headers[1].ends_with("/dev/sdc"), "{text}");
        assert!(headers[2].starts_with("Disk 5 (") && headers[2].ends_with("/dev/sdf"), "{text}");
        // an unreadable table names the remedy
        let unknown = DiskInfo { index: 3, table: TableKind::Unknown, path: "/dev/rdisk3".into(), ..Default::default() };
        let t = render_disks(&[unknown], "run with sudo (root)");
        assert!(t.lines().next().unwrap().ends_with("/dev/rdisk3  [partition table not readable, offsets approximate: run with sudo (root) for exact offsets]"), "{t}");
    }
}
