//! Text shown in the disk table, the dialogs and the status bar.

use pm_core::{format_size, DiskInfo, PartitionInfo, PartitionType};

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

/// Header row of a disk group: `"Disk 1 (Model, GPT, 1.82 TB)"`.
pub fn disk_header(d: &DiskInfo) -> String {
    d.display_name()
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
        assert_eq!(disk_header(&gpt_disk(1)), "Disk 1 (Test SSD, GPT, 1.00 GB)");
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
