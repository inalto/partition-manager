//! `/proc/self/mountinfo`, `/proc/mounts` and `/proc/swaps`: which filesystems are mounted
//! where, and how.

use pm_core::FsKind;
use std::collections::HashMap;
use std::path::Path;

/// One mount, from `/proc/self/mountinfo` (or `/proc/mounts`, which lacks the ids, the root
/// and the superblock options).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountEntry {
    /// Mounted device (`/dev/sda1`) or a pseudo source (`proc`, `overlay`, `none`).
    pub device: String,
    pub mount_point: String,
    pub fstype: String,
    /// Per-mount (VFS) options: `rw,nosuid,nodev,relatime`.
    pub options: String,
    /// Superblock (filesystem) options: `rw,fmask=0022,errors=remount-ro`.
    pub super_options: String,
    /// Mount id and parent mount id (0 when unknown).
    pub id: u32,
    pub parent: u32,
    /// Directory of the filesystem mounted here: `/` for an ordinary mount, a subdirectory
    /// for a bind mount, `/@home` for a btrfs subvolume. Empty when unknown.
    pub root: String,
}

impl MountEntry {
    /// Mounted read-only (`ro` among the per-mount options).
    pub fn read_only(&self) -> bool {
        self.options.split(',').any(|o| o == "ro")
    }

    /// Mounts a subtree of its filesystem (bind mount of a directory, btrfs subvolume).
    pub fn is_partial(&self) -> bool {
        !self.root.is_empty() && self.root != "/"
    }
}

/// Decode the octal escapes the kernel uses in `/proc/mounts` fields (`\040` space, `\011`
/// tab, `\012` newline, `\134` backslash).
pub fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' &&i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') as u32 * 64 + (b[i + 2] - b'0') as u32 * 8 + (b[i + 3] - b'0') as u32;
            if let Ok(byte) = u8::try_from(v) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse the text of `/proc/mounts` (`device mount_point fstype options dump pass`). Lines with
/// fewer than three fields are skipped.
pub fn parse_proc_mounts(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let device = unescape(f.next()?);
            let mount_point = unescape(f.next()?);
            let fstype = unescape(f.next()?);
            let options = f.next().map(unescape).unwrap_or_default();
            Some(MountEntry { device, mount_point, fstype, options, ..Default::default() })
        })
        .collect()
}

/// Parse the text of `/proc/self/mountinfo`:
/// `id parent major:minor root mount_point vfs_options [optional fields...] - fstype source super_options`.
/// Malformed lines are skipped.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let mut l = left.split_whitespace();
            let id = l.next()?.parse().ok()?;
            let parent = l.next()?.parse().ok()?;
            let _dev = l.next()?;
            let root = unescape(l.next()?);
            let mount_point = unescape(l.next()?);
            let options = unescape(l.next()?);
            let mut r = right.split_whitespace();
            let fstype = unescape(r.next()?);
            let device = unescape(r.next()?);
            let super_options = r.next().map(unescape).unwrap_or_default();
            Some(MountEntry { device, mount_point, fstype, options, super_options, id, parent, root })
        })
        .collect()
}

/// Why each of `mine` (mounts of the disk about to be written) cannot be unmounted and
/// re-mounted faithfully: a bind mount or subvolume (its root is not `/`), or a mount hidden
/// under a later mount on the same or a parent directory. Empty when all are plain.
/// `all` is the whole mount table in kernel order.
pub fn mount_conflicts(mine: &[MountEntry], all: &[MountEntry]) -> Vec<String> {
    let parent_of: HashMap<u32, u32> = all.iter().filter(|e| e.id != 0).map(|e| (e.id, e.parent)).collect();
    // whether mount `a` is `e` itself or one of its ancestors in the mount tree
    let is_ancestor = |a: u32, e: &MountEntry| {
        let mut cur = e.id;
        for _ in 0..all.len() + 1 {
            if cur == a {
                return true;
            }
            match parent_of.get(&cur) {
                Some(&p) if p != cur => cur = p,
                _ => return false,
            }
        }
        false
    };
    let mut out = Vec::new();
    for e in mine {
        if e.is_partial() {
            out.push(format!(
                "{} on {} mounts only {} of the filesystem (bind mount or subvolume): unmount it yourself first",
                e.device, e.mount_point, e.root
            ));
            continue;
        }
        let pos = all.iter().position(|a| a == e);
        let covered = all.iter().enumerate().any(|(i, f)| {
            f != e
                && pos.is_some_and(|p| i > p)
                && is_under(&e.mount_point, &f.mount_point)
                && !(f.id != 0 && e.id != 0 && is_ancestor(f.id, e))
        });
        if covered {
            out.push(format!(
                "{} on {} is hidden under a later mount: unmount it yourself first",
                e.device, e.mount_point
            ));
        }
    }
    out
}

/// The device node of the block device holding `path`, from its `st_dev`
/// (`/sys/dev/block/M:m`). `None` for filesystems with anonymous device numbers (btrfs
/// subvolumes, overlay, tmpfs, ...).
pub fn node_of_st_dev(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = path.ancestors().find_map(|a| std::fs::metadata(a).ok())?;
    let (major, minor) = super::ioctl::major_minor(meta.dev());
    let sys = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()?;
    Some(format!("/dev/{}", sys.file_name()?.to_string_lossy()))
}

/// Active swap devices and files from the text of `/proc/swaps` (header line skipped).
pub fn parse_proc_swaps(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next().map(unescape))
        .collect()
}

/// Current mounts of this process's mount namespace (`/proc/self/mountinfo`, falling back to
/// `/proc/mounts`), with device symlinks (`/dev/disk/by-uuid/...`) resolved to the device node
/// and `/dev/root` resolved through the `st_dev` of its mount point.
pub fn read_mounts() -> Vec<MountEntry> {
    let mut v = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(t) => parse_mountinfo(&t),
        Err(e) => {
            log::debug!("cannot read /proc/self/mountinfo: {e}");
            let text = std::fs::read_to_string("/proc/mounts").unwrap_or_else(|e| {
                log::warn!("cannot read /proc/mounts: {e}");
                String::new()
            });
            parse_proc_mounts(&text)
        }
    };
    for e in &mut v {
        e.device = if e.device == "/dev/root" {
            node_of_st_dev(Path::new(&e.mount_point)).unwrap_or_else(|| e.device.clone())
        } else {
            canonical_device(&e.device)
        };
    }
    v
}

/// Active swap areas (canonical paths).
pub fn read_swaps() -> Vec<String> {
    std::fs::read_to_string("/proc/swaps")
        .map(|t| parse_proc_swaps(&t).iter().map(|s| canonical_device(s)).collect())
        .unwrap_or_default()
}

/// `/dev/...` symlinks resolved to their target; anything else unchanged.
pub fn canonical_device(dev: &str) -> String {
    if dev.starts_with("/dev/") {
        if let Ok(p) = std::fs::canonicalize(dev) {
            return p.to_string_lossy().into_owned();
        }
    }
    dev.to_string()
}

/// Whether `path` is `mount_point` or lies below it (whole components).
pub fn is_under(path: &str, mount_point: &str) -> bool {
    Path::new(path).starts_with(Path::new(mount_point))
}

/// The entry whose mount point is the longest prefix of the absolute path `path`; for equal
/// mount points the last (top-most) mount wins.
pub fn longest_mount<'a>(entries: &'a [MountEntry], path: &str) -> Option<&'a MountEntry> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| is_under(path, &e.mount_point))
        .max_by_key(|(i, e)| (Path::new(&e.mount_point).components().count(), *i))
        .map(|(_, e)| e)
}

/// Filesystem kind for a `/proc/mounts` type (FAT width unknown: reported as FAT32).
pub fn fs_from_mount_type(fstype: &str) -> Option<FsKind> {
    Some(match fstype {
        "vfat" | "msdos" | "fat" => FsKind::Fat32,
        "exfat" => FsKind::ExFat,
        "ntfs" | "ntfs3" | "ntfs-3g" => FsKind::Ntfs,
        "ext2" => FsKind::Ext2,
        "ext3" => FsKind::Ext3,
        "ext4" => FsKind::Ext4,
        "btrfs" => FsKind::Btrfs,
        "xfs" => FsKind::Xfs,
        "hfsplus" => FsKind::HfsPlus,
        "apfs" => FsKind::Apfs,
        "iso9660" => FsKind::Iso9660,
        "udf" => FsKind::Udf,
        _ => return None,
    })
}

/// Kernel filesystem types to try, in order, when mounting a filesystem of `kind`.
pub fn mount_types(kind: FsKind) -> &'static [&'static str] {
    match kind {
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => &["vfat"],
        FsKind::ExFat => &["exfat"],
        FsKind::Ntfs => &["ntfs3", "ntfs-3g", "ntfs"],
        FsKind::Ext2 => &["ext2", "ext4"],
        FsKind::Ext3 => &["ext3", "ext4"],
        FsKind::Ext4 => &["ext4"],
        FsKind::Xfs => &["xfs"],
        FsKind::Btrfs => &["btrfs"],
        FsKind::HfsPlus => &["hfsplus"],
        FsKind::Iso9660 => &["iso9660"],
        FsKind::Udf => &["udf"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_octal_sequences() {
        assert_eq!(unescape(r"/media/My\040Disk"), "/media/My Disk");
        assert_eq!(unescape(r"a\011b\012c\134d"), "a\tb\nc\\d");
        assert_eq!(unescape(r"trailing\04"), r"trailing\04");
        assert_eq!(unescape(r"not\999octal"), r"not\999octal");
    }

    #[test]
    fn longest_prefix_wins_on_whole_components() {
        let m = |d: &str, mp: &str| MountEntry { device: d.into(), mount_point: mp.into(), ..Default::default() };
        let v = vec![m("/dev/sda2", "/"), m("/dev/sda1", "/boot/efi"), m("/dev/sdb1", "/mnt/data"), m("/dev/sdc1", "/mnt/data")];
        assert_eq!(longest_mount(&v, "/home/x").unwrap().device, "/dev/sda2");
        assert_eq!(longest_mount(&v, "/boot/efi/EFI").unwrap().device, "/dev/sda1");
        assert_eq!(longest_mount(&v, "/boot/efix").unwrap().device, "/dev/sda2");
        assert_eq!(longest_mount(&v, "/mnt/data/f").unwrap().device, "/dev/sdc1", "top-most mount");
        assert!(longest_mount(&v[1..], "/home").is_none());
    }

    #[test]
    fn swaps_and_types() {
        let s = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/dev/sdb3   partition\t8388604\t0\t-2\n/swap\\040file file 1 0 -3\n";
        assert_eq!(parse_proc_swaps(s), vec!["/dev/sdb3".to_string(), "/swap file".to_string()]);
        assert_eq!(fs_from_mount_type("vfat"), Some(FsKind::Fat32));
        assert_eq!(fs_from_mount_type("ntfs3"), Some(FsKind::Ntfs));
        assert_eq!(fs_from_mount_type("tmpfs"), None);
        assert_eq!(mount_types(FsKind::Ntfs)[0], "ntfs3");
        assert!(mount_types(FsKind::LinuxSwap).is_empty());
    }
}
