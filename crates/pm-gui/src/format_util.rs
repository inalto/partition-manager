//! Text shown in the disk table, the dialogs and the status bar.

use pm_core::{format_size, DiskInfo, PartitionInfo, PartitionType, TableKind};

/// Column titles of the disk table, in order.
pub const COLUMNS: [&str; 7] = ["Partition", "Capacity", "Used", "Unused", "File system", "Type", "Status"];

/// `"C:Windows"`, `"*:SYSTEM"` (no drive letter) or `"*:"`.
pub fn partition_name(p: &PartitionInfo) -> String {
    let letter = p.letter().unwrap_or('*');
    format!("{letter}:{}", p.label)
}

/// `"GPT (EFI system partition)"` on GPT disks, `"Primary"` / `"Logical"` on MBR disks.
pub fn type_text(p: &PartitionInfo) -> String {
    match p.ptype {
        Some(t @ PartitionType::Gpt(_)) => t.name(),
        _ if p.logical => "Logical".to_string(),
        _ => "Primary".to_string(),
    }
}

/// File system column: the detected file system, `"Other"` for a Microsoft reserved
/// partition and `"Unformatted"` otherwise.
pub fn fs_text(p: &PartitionInfo) -> String {
    match p.fs {
        Some(fs) => fs.display_name().to_string(),
        None if p.ptype.map_or(false, |t| t.is_msr()) => "Other".to_string(),
        None => "Unformatted".to_string(),
    }
}

/// One row of the disk table: Partition | Capacity | Used | Unused | File system | Type | Status.
pub fn partition_row(p: &PartitionInfo) -> [String; 7] {
    let (used, unused) = match p.used {
        Some(u) => {
            let u = u.min(p.len);
            (format_size(u), format_size(p.len - u))
        }
        None => ("-".to_string(), "-".to_string()),
    };
    [
        partition_name(p),
        format_size(p.len),
        used,
        unused,
        fs_text(p),
        type_text(p),
        p.status_text(),
    ]
}

/// Table row for an unallocated gap of `len` bytes.
pub fn free_row(len: u64) -> [String; 7] {
    [
        "(Unallocated)".to_string(),
        format_size(len),
        format_size(0),
        format_size(len),
        "Unallocated".to_string(),
        String::new(),
        "None".to_string(),
    ]
}

/// Header row of a disk group: `"Disk 1 (Model, GPT, 1.82 TB)  /dev/sdb"` (the device path
/// names the disk unambiguously: on Linux and macOS `disk:N` is a position that can change
/// when a disk is plugged in). A disk whose partition table could not be read says that its
/// layout is approximate, with `elevation_hint` (`Platform::elevation_hint`).
pub fn disk_header(d: &DiskInfo, elevation_hint: &str) -> String {
    let mut s = d.display_name();
    if !d.path.is_empty() {
        s.push_str("  ");
        s.push_str(&d.path);
    }
    if d.table == TableKind::Unknown {
        s.push_str(&format!("  [{}]", unreadable_table_note(elevation_hint)));
    }
    s
}

/// What a disk with an unreadable partition table (`TableKind::Unknown`) shows.
pub fn unreadable_table_note(elevation_hint: &str) -> String {
    format!("partition table not readable, offsets approximate: {elevation_hint} for exact offsets")
}

/// An example absolute path for hints and messages: `rel` (with `/` separators) under `D:\`
/// on Windows, under `/mnt/data/` on Linux and macOS (`""`: that folder itself).
pub fn example_path(rel: &str) -> String {
    if cfg!(windows) {
        format!("D:\\{}", rel.replace('/', "\\"))
    } else {
        format!("/mnt/data/{rel}")
    }
}

/// `"12.50 MB/s"`; empty when the rate is unknown (zero).
pub fn format_rate(bps: f64) -> String {
    if bps.is_finite() && bps >= 1.0 {
        format!("{}/s", format_size(bps as u64))
    } else {
        String::new()
    }
}

/// `"1:02:03"` / `"02:03"`, or `"--:--"` when unknown.
pub fn format_eta(secs: Option<u64>) -> String {
    match secs {
        None => "--:--".to_string(),
        Some(s) if s >= 3600 => format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
        Some(s) => format!("{:02}:{:02}", s / 60, s % 60),
    }
}

/// Status bar text for the elevation state: "administrator" wording on Windows, "root" elsewhere.
pub fn elevation_status(os: &str, elevated: bool) -> &'static str {
    match (os == "windows", elevated) {
        (true, true) => "Running as administrator",
        (true, false) => "Not elevated: read-only",
        (false, true) => "Running as root",
        (false, false) => "Not root: read-only",
    }
}

/// Banner advice when the disks cannot be read without the needed rights.
pub fn elevation_advice(os: &str) -> &'static str {
    if os == "windows" {
        " Start Partition Manager as administrator, or open image files with --disk-file."
    } else {
        " Start Partition Manager with sudo, or open image files with --disk-file."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk};
    use pm_core::MIB;

    #[test]
    fn partition_row_gpt_data() {
        let d = gpt_disk(1);
        assert_eq!(
            partition_row(&d.partitions[2]),
            ["E:Data", "684.00 MB", "342.00 MB", "342.00 MB", "NTFS", "GPT (basic data)", "None"]
                .map(String::from)
        );
    }

    #[test]
    fn partition_row_system_and_msr() {
        let d = gpt_disk(1);
        let efi = partition_row(&d.partitions[0]);
        assert_eq!(efi[0], "*:");
        assert_eq!(efi[4], "FAT32");
        assert_eq!(efi[5], "GPT (EFI system partition)");
        assert_eq!(efi[6], "System");
        let msr = partition_row(&d.partitions[1]);
        assert_eq!(msr[2], "-");
        assert_eq!(msr[3], "-");
        assert_eq!(msr[4], "Other");
    }

    #[test]
    fn partition_row_mbr_primary_and_logical() {
        let d = mbr_disk(0);
        let p1 = partition_row(&d.partitions[0]);
        assert_eq!(p1[5], "Primary");
        assert_eq!(p1[6], "Active");
        let l6 = partition_row(&d.partitions[3]);
        assert_eq!(l6[4], "Ext4");
        assert_eq!(l6[5], "Logical");
        let mut raw = d.partitions[0].clone();
        raw.fs = None;
        raw.used = Some(500 * MIB); // more than the partition: clamped
        let r = partition_row(&raw);
        assert_eq!(r[4], "Unformatted");
        assert_eq!(r[2], "200.00 MB");
        assert_eq!(r[3], "0 B");
    }

    #[test]
    fn free_row_and_header() {
        assert_eq!(
            free_row(2 * MIB),
            ["(Unallocated)", "2.00 MB", "0 B", "2.00 MB", "Unallocated", "", "None"].map(String::from)
        );
        let d = gpt_disk(1);
        assert_eq!(disk_header(&d, "HINT"), format!("Disk 1 (Test SSD, GPT, 1.00 GB)  {}", d.path));
        let mut d = DiskInfo { path: "/dev/sdb".into(), ..gpt_disk(1) };
        assert_eq!(disk_header(&d, "HINT"), "Disk 1 (Test SSD, GPT, 1.00 GB)  /dev/sdb");
        d.table = TableKind::Unknown;
        let h = disk_header(&d, "run with sudo (root)");
        assert!(h.ends_with("[partition table not readable, offsets approximate: run with sudo (root) for exact offsets]"), "{h}");
    }

    #[test]
    fn elevation_wording_per_os() {
        assert_eq!(elevation_status("windows", true), "Running as administrator");
        assert_eq!(elevation_status("windows", false), "Not elevated: read-only");
        assert_eq!(elevation_status("linux", true), "Running as root");
        assert_eq!(elevation_status("macos", false), "Not root: read-only");
        assert!(elevation_advice("windows").contains("as administrator"));
        assert!(elevation_advice("linux").contains("with sudo"));
    }

    #[test]
    fn rate_and_eta() {
        assert_eq!(format_rate(0.0), "");
        assert_eq!(format_rate(f64::NAN), "");
        assert_eq!(format_rate(1536.0 * 1024.0), "1.50 MB/s");
        assert_eq!(format_eta(None), "--:--");
        assert_eq!(format_eta(Some(0)), "00:00");
        assert_eq!(format_eta(Some(125)), "02:05");
        assert_eq!(format_eta(Some(3723)), "1:02:03");
    }
}
