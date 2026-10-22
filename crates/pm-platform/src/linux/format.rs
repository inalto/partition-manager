//! Creating filesystems with the system's `mkfs.*` tools.

use super::mount::node;
use super::mounts::{canonical_device, read_mounts};
use crate::posix::cmd;
use pm_core::{Error, FsKind, ProgressReport, ProgressSink, Result, VolumeId};
use std::path::Path;

/// Where the partition lies, for tools that write it into the boot sector (`mkfs.ntfs -p`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    /// Logical sector size of the device.
    pub sector_size: u32,
    /// Partition start in logical sectors (0 for a whole disk or when unknown).
    pub start_sector: u64,
}

/// `(tool, args)` that format `dev` as `fs`. `cluster` (bytes) maps to the tool's cluster or
/// block size option where it has one and is ignored (with a log line) otherwise; `quick`
/// only changes NTFS (`-Q`: no zeroing of the volume).
pub fn mkfs_command(
    fs: FsKind,
    label: &str,
    cluster: Option<u32>,
    quick: bool,
    dev: &str,
    at: Placement,
) -> Result<(&'static str, Vec<String>)> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |s: &[&str]| a.extend(s.iter().map(|s| s.to_string()));
    let ignored = |tool: &str| {
        if let Some(c) = cluster {
            log::info!("{tool}: cluster size {c} is not supported here; using the default");
        }
    };
    let tool = match fs {
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => {
            let bits = match fs {
                FsKind::Fat12 => "12",
                FsKind::Fat16 => "16",
                _ => "32",
            };
            push(&["-F", bits]);
            if !label.is_empty() {
                push(&["-n", label]);
            }
            let ss = at.sector_size.max(512);
            match cluster {
                Some(c) if c >= ss && c % ss == 0 && (c / ss).is_power_of_two() && c / ss <= 128 => {
                    push(&["-s", &(c / ss).to_string()]);
                }
                _ => ignored("mkfs.vfat"),
            }
            "mkfs.vfat"
        }
        FsKind::ExFat => {
            if !label.is_empty() {
                push(&["-L", label]);
            }
            if let Some(c) = cluster {
                push(&["-c", &c.to_string()]);
            }
            "mkfs.exfat"
        }
        FsKind::Ntfs => {
            if quick {
                push(&["-Q"]);
            }
            if !label.is_empty() {
                push(&["-L", label]);
            }
            if let Some(c) = cluster {
                push(&["-c", &c.to_string()]);
            }
            if at.sector_size != 0 {
                push(&["-s", &at.sector_size.to_string()]);
            }
            // the hidden-sectors field of the boot sector: Windows refuses to boot / mount
            // some volumes when it is wrong, and mkfs.ntfs cannot always find it itself
            push(&["-p", &at.start_sector.to_string()]);
            "mkfs.ntfs"
        }
        FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4 => {
            push(&["-F"]);
            if !label.is_empty() {
                push(&["-L", label]);
            }
            match cluster {
                Some(c @ 1024..=65536) if c.is_power_of_two() => push(&["-b", &c.to_string()]),
                _ => ignored("mkfs.ext*"),
            }
            match fs {
                FsKind::Ext2 => "mkfs.ext2",
                FsKind::Ext3 => "mkfs.ext3",
                _ => "mkfs.ext4",
            }
        }
        FsKind::Xfs => {
            push(&["-f"]);
            if !label.is_empty() {
                push(&["-L", label]);
            }
            if let Some(c) = cluster {
                push(&["-b", &format!("size={c}")]);
            }
            "mkfs.xfs"
        }
        FsKind::Btrfs => {
            push(&["-f"]);
            if !label.is_empty() {
                push(&["-L", label]);
            }
            ignored("mkfs.btrfs");
            "mkfs.btrfs"
        }
        other => {
            return Err(Error::Unsupported(format!("Linux cannot create {} filesystems", other.display_name())))
        }
    };
    a.push(dev.to_string());
    Ok((tool, a))
}

/// Logical sector size and start sector of partition node `dev` from sysfs.
pub fn placement(dev: &str) -> Placement {
    let name = dev.trim_start_matches("/dev/");
    let class = Path::new("/sys/class/block").join(name);
    let read = |p: &Path| std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse::<u64>().ok());
    // a partition's queue/ lives in its parent disk's directory
    let disk_dir = std::fs::canonicalize(&class).ok().and_then(|c| {
        if c.join("partition").exists() {
            c.parent().map(Path::to_path_buf)
        } else {
            Some(c)
        }
    });
    let sector_size = disk_dir
        .and_then(|d| read(&d.join("queue/logical_block_size")))
        .unwrap_or(512) as u32;
    let start512 = read(&class.join("start")).unwrap_or(0);
    Placement {
        sector_size,
        start_sector: start512 * super::sysfs::SYSFS_SECTOR / sector_size.max(1) as u64,
    }
}

/// Refuse when `dev` is mounted.
pub fn ensure_unmounted(dev: &str) -> Result<()> {
    let c = canonical_device(dev);
    let at: Vec<String> = read_mounts().into_iter().filter(|e| e.device == c).map(|e| e.mount_point).collect();
    if at.is_empty() {
        Ok(())
    } else {
        Err(Error::Busy(format!("{dev} is mounted at {}: unmount it first", at.join(", "))))
    }
}

fn report(progress: &dyn ProgressSink, done: u64) {
    progress.report(&ProgressReport { phase: "format".into(), done, total: 100, ..Default::default() });
}

/// Format volume `id` (an unmounted partition node).
pub fn format(
    id: &VolumeId,
    fs: FsKind,
    label: &str,
    cluster: Option<u32>,
    quick: bool,
    progress: &dyn ProgressSink,
) -> Result<()> {
    let dev = node(id)?;
    ensure_unmounted(dev)?;
    let (tool, args) = mkfs_command(fs, label, cluster, quick, dev, placement(dev))?;
    report(progress, 0);
    cmd::run(tool, &args)?;
    report(progress, 100);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(fs: FsKind, label: &str, cluster: Option<u32>, quick: bool) -> String {
        let at = Placement { sector_size: 512, start_sector: 2048 };
        let (tool, args) = mkfs_command(fs, label, cluster, quick, "/dev/sdb1", at).unwrap();
        format!("{tool} {}", args.join(" "))
    }

    #[test]
    fn command_lines() {
        assert_eq!(line(FsKind::Fat32, "PMTEST", None, true), "mkfs.vfat -F 32 -n PMTEST /dev/sdb1");
        assert_eq!(line(FsKind::Fat32, "", Some(4096), true), "mkfs.vfat -F 32 -s 8 /dev/sdb1");
        assert_eq!(line(FsKind::Fat16, "A", Some(1000), true), "mkfs.vfat -F 16 -n A /dev/sdb1", "odd cluster ignored");
        assert_eq!(line(FsKind::ExFat, "Data", Some(131072), true), "mkfs.exfat -L Data -c 131072 /dev/sdb1");
        assert_eq!(line(FsKind::Ntfs, "Win", None, true), "mkfs.ntfs -Q -L Win -s 512 -p 2048 /dev/sdb1");
        assert_eq!(line(FsKind::Ntfs, "", Some(4096), false), "mkfs.ntfs -c 4096 -s 512 -p 2048 /dev/sdb1");
        assert_eq!(line(FsKind::Ext4, "root", Some(4096), true), "mkfs.ext4 -F -L root -b 4096 /dev/sdb1");
        assert_eq!(line(FsKind::Ext2, "", Some(512), true), "mkfs.ext2 -F /dev/sdb1");
        assert_eq!(line(FsKind::Xfs, "x", None, true), "mkfs.xfs -f -L x /dev/sdb1");
        assert!(matches!(
            mkfs_command(FsKind::ReFs, "", None, true, "/dev/sdb1", Placement::default()),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn fat_cluster_in_4k_sectors() {
        let at = Placement { sector_size: 4096, start_sector: 256 };
        let (_, a) = mkfs_command(FsKind::Fat32, "", Some(32768), true, "/dev/nvme0n1p1", at).unwrap();
        assert_eq!(a, ["-F", "32", "-s", "8", "/dev/nvme0n1p1"]);
    }
}
