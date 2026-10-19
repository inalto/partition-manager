//! Parsing and opening of the `<device>` command line argument.

use crate::cli::{arg_error, CliError};
use pm_core::{BlockDevice, Error, Result, SliceDevice, VolumeId};
use pm_parttable::read_table;
use pm_platform::{OpenMode, Platform};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceArg {
    /// Physical disk by index (`disk:1`).
    Disk(u32),
    /// Drive letter (`D:`) or `\\?\Volume{..}\` path.
    Volume(String),
    /// Image file (`file:PATH`, or a bare path that exists); any format `pm-image` reads.
    File(PathBuf),
    /// Partition `number` (1-based, as `pm table` prints it) of a disk or an image file:
    /// `disk:N:P`, `file:PATH:P`.
    Partition(Box<DeviceArg>, u32),
}

/// What a device argument is ultimately stored on, for "is this the same device" checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Identity {
    Disk(u32),
    Volume(String),
    /// Canonicalised path of an image file.
    File(PathBuf),
}

/// An opened device together with where it lives, so commands can refuse to copy a device
/// onto itself (or onto an overlapping partition of the same disk).
pub struct Opened {
    pub dev: Box<dyn BlockDevice>,
    /// The disk, volume or file the data is stored on.
    pub identity: Identity,
    /// The byte range of `identity` that `dev` covers (all of it unless a partition).
    pub range: (u64, u64),
}

impl Opened {
    /// Whether writing `len` bytes at `offset` of `self` would touch `other`'s byte range.
    pub fn overlaps(&self, offset: u64, len: u64, other: &Opened) -> bool {
        if self.identity != other.identity {
            return false;
        }
        let start = self.range.0.saturating_add(offset);
        let end = start.saturating_add(len);
        start < other.range.1 && other.range.0 < end
    }
}

/// `D:` / `D:\` -> `Some('D')`.
fn drive_letter(s: &str) -> Option<char> {
    let b = s.as_bytes();
    let ok = b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || (b.len() == 3 && (b[2] == b'\\' || b[2] == b'/')));
    ok.then(|| (b[0] as char).to_ascii_uppercase())
}

/// `prefix` (ASCII, case-insensitive) stripped from the start of `s`.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

fn is_volume_path(s: &str) -> bool {
    strip_prefix_ci(s, r"\\?\Volume{").is_some()
}

/// The volume path with exactly one trailing backslash, as the platform expects it.
fn normalize_volume_path(s: &str) -> String {
    let mut v = s.to_string();
    if !v.ends_with('\\') {
        v.push('\\');
    }
    v
}

/// `"N"` as a 1-based partition number.
fn parse_partition(n: &str, whole: &str) -> Result<u32> {
    match n.parse::<u32>() {
        Ok(0) => Err(Error::Invalid(format!("'{whole}': partition numbers start at 1"))),
        Ok(n) => Ok(n),
        Err(_) => Err(Error::Invalid(format!("'{whole}': '{n}' is not a partition number"))),
    }
}

/// `PATH` or `PATH:P` where `PATH` is an existing file: the whole file, or its partition `P`.
/// `None` when `s` is an existing file in neither form.
fn file_or_partition(s: &str) -> Option<DeviceArg> {
    if Path::new(s).is_file() {
        return Some(DeviceArg::File(PathBuf::from(s)));
    }
    let (base, num) = s.rsplit_once(':')?;
    if base.is_empty() || num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Path::new(base).is_file().then(|| {
        // a number too large for u32 can never name an existing partition (0 is rejected on open)
        DeviceArg::Partition(Box::new(DeviceArg::File(PathBuf::from(base))), num.parse().unwrap_or(0))
    })
}

/// Parse `"disk:1"`, `"disk:1:2"`, `"D:"`, `"\\?\Volume{..}\"`, `"file:PATH"`, `"file:PATH:2"` or
/// a path of an existing file (optionally followed by `:P`).
pub fn parse_device(s: &str) -> Result<DeviceArg> {
    if let Some(rest) = strip_prefix_ci(s, "disk:") {
        let bad = || Error::Invalid(format!("'{s}': expected disk:<index> or disk:<index>:<partition>, e.g. disk:1"));
        let (n, part) = match rest.split_once(':') {
            Some((n, p)) => (n, Some(p)),
            None => (rest, None),
        };
        let disk = DeviceArg::Disk(n.parse::<u32>().map_err(|_| bad())?);
        return match part {
            None => Ok(disk),
            Some(p) => Ok(DeviceArg::Partition(Box::new(disk), parse_partition(p, s)?)),
        };
    }
    if is_volume_path(s) {
        return Ok(DeviceArg::Volume(normalize_volume_path(s)));
    }
    if let Some(path) = strip_prefix_ci(s, "file:") {
        if path.is_empty() {
            return Err(Error::Invalid("file: needs a path".into()));
        }
        return file_or_partition(path)
            .ok_or_else(|| Error::NotFound(format!("image file '{path}' does not exist")));
    }
    if let Some(c) = drive_letter(s) {
        return Ok(DeviceArg::Volume(format!("{c}:")));
    }
    if let Some(a) = file_or_partition(s) {
        return Ok(a);
    }
    Err(Error::Invalid(format!(
        "'{s}' is not a device: use disk:N[:P], a drive letter (D:), a \\\\?\\Volume{{..}}\\ path, or an existing image file[:P]"
    )))
}

/// Resolve `D:` (via the volumes' mount points) or a `\\?\Volume{..}\` path to a [`VolumeId`].
pub fn resolve_volume(platform: &dyn Platform, s: &str) -> Result<VolumeId> {
    if is_volume_path(s) {
        return Ok(VolumeId(normalize_volume_path(s)));
    }
    let letter = drive_letter(s)
        .ok_or_else(|| Error::Invalid(format!("'{s}' is not a drive letter or volume path")))?;
    platform
        .enumerate_volumes()?
        .into_iter()
        .find(|v| v.mount_points.iter().any(|m| drive_letter(m) == Some(letter)))
        .map(|v| v.id)
        .ok_or_else(|| Error::NotFound(format!("no volume is mounted at {letter}:")))
}

/// Open the device described by `a`.
///
/// A device that does not exist is reported as `Error::NotFound` (an argument error for the CLI),
/// whatever the OS says when opening it; genuine failures (access denied, I/O) pass through.
pub fn open_device(p: &dyn Platform, a: &DeviceArg, mode: OpenMode) -> Result<Box<dyn BlockDevice>> {
    open_device_with_slice(p, a, mode).map(|o| o.dev)
}

/// Like [`open_device`], also reporting which disk / volume / file the device is stored on and
/// the byte range it covers (see [`Opened`]). Partitions are bounds-checked slices of their
/// parent, located through `pm_parttable::read_table`.
pub fn open_device_with_slice(p: &dyn Platform, a: &DeviceArg, mode: OpenMode) -> Result<Opened> {
    let whole = |dev: Box<dyn BlockDevice>, identity: Identity| Opened {
        range: (0, dev.len()),
        dev,
        identity,
    };
    match a {
        DeviceArg::Disk(i) => {
            let dev = p.open_disk(*i, mode).map_err(|e| {
                let missing = p.enumerate_disks().map_or(false, |d| !d.iter().any(|x| x.index == *i));
                if missing {
                    Error::NotFound(format!("no disk with index {i}"))
                } else {
                    e
                }
            })?;
            Ok(whole(dev, Identity::Disk(*i)))
        }
        DeviceArg::Volume(v) => {
            let id = resolve_volume(p, v)?;
            let dev = p.open_volume(&id, mode).map_err(|e| {
                let missing = p.enumerate_volumes().map_or(false, |vs| !vs.iter().any(|x| x.id == id));
                if missing {
                    Error::NotFound(format!("no volume {}", id.0))
                } else {
                    e
                }
            })?;
            Ok(whole(dev, Identity::Volume(id.0)))
        }
        // Any image format pm-image understands (raw, VHD, VMDK, qcow2), detected by content.
        DeviceArg::File(path) => {
            let dev = pm_image::open(path, mode == OpenMode::ReadOnly)?;
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            Ok(whole(dev, Identity::File(canonical)))
        }
        DeviceArg::Partition(parent, number) => {
            let mut parent = open_device_with_slice(p, parent, mode)?;
            let table = read_table(&mut *parent.dev)?;
            let entry = table.entries.iter().find(|e| e.number == *number).ok_or_else(|| {
                if table.entries.is_empty() {
                    Error::NotFound(format!("partition {number}: the device has no partition table"))
                } else {
                    let have: Vec<String> = table.entries.iter().map(|e| e.number.to_string()).collect();
                    Error::NotFound(format!("no partition {number} (the table has {})", have.join(", ")))
                }
            })?;
            let (start, len) = (entry.start, entry.len);
            let slice = SliceDevice::new(parent.dev, start, len)?;
            Ok(Opened {
                dev: Box::new(slice),
                identity: parent.identity,
                range: (parent.range.0 + start, parent.range.0 + start + len),
            })
        }
    }
}

/// Parse and open a `<device>` command line argument. A device that does not exist is an
/// argument error (exit 1); anything else that goes wrong while opening it is a runtime error.
pub fn open_spec(platform: &dyn Platform, spec: &str, mode: OpenMode) -> std::result::Result<Opened, CliError> {
    let arg = parse_device(spec).map_err(arg_error)?;
    open_device_with_slice(platform, &arg, mode).map_err(|e| match e {
        Error::NotFound(m) => CliError::Usage(m),
        other => CliError::Runtime(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{FileDevice, MemDevice, VolumeInfo};
    use pm_platform::MockPlatform;

    #[test]
    fn cli_parse_device_variants() {
        assert_eq!(parse_device("disk:1").unwrap(), DeviceArg::Disk(1));
        assert_eq!(parse_device("DISK:12").unwrap(), DeviceArg::Disk(12));
        assert!(parse_device("disk:x").is_err());
        assert!(parse_device("disk:").is_err());
        assert!(parse_device("disk:-1").is_err());

        assert_eq!(parse_device("D:").unwrap(), DeviceArg::Volume("D:".into()));
        assert_eq!(parse_device("d:").unwrap(), DeviceArg::Volume("D:".into()));
        assert_eq!(parse_device(r"E:\").unwrap(), DeviceArg::Volume("E:".into()));

        let v = r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\";
        assert_eq!(parse_device(v).unwrap(), DeviceArg::Volume(v.into()));
        // trailing backslash is normalised
        assert_eq!(parse_device(v.trim_end_matches('\\')).unwrap(), DeviceArg::Volume(v.into()));

        assert!(matches!(parse_device("file:rel/does-not-exist.img"), Err(Error::NotFound(_))));
        assert!(matches!(parse_device(r"file:C:\no\such.img"), Err(Error::NotFound(_))));
        assert!(parse_device("file:").is_err());

        // a bare path is a File only if it exists
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.raw");
        std::fs::write(&img, b"x").unwrap();
        let s = img.to_str().unwrap();
        assert_eq!(parse_device(s).unwrap(), DeviceArg::File(img.clone()));
        assert!(matches!(
            parse_device(dir.path().join("missing.raw").to_str().unwrap()),
            Err(Error::Invalid(_))
        ));
        assert_eq!(parse_device(&format!("file:{s}")).unwrap(), DeviceArg::File(img.clone()));
        assert!(parse_device("").is_err());
        assert!(parse_device("C").is_err());
    }

    #[test]
    fn cli_parse_partition_addressing() {
        assert_eq!(
            parse_device("disk:2:3").unwrap(),
            DeviceArg::Partition(Box::new(DeviceArg::Disk(2)), 3)
        );
        assert!(matches!(parse_device("disk:2:0"), Err(Error::Invalid(_))));
        assert!(parse_device("disk:2:").is_err());
        assert!(parse_device("disk:2:x").is_err());
        assert!(parse_device("disk:2:1:1").is_err());

        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.raw");
        std::fs::write(&img, b"x").unwrap();
        let s = img.to_str().unwrap();
        let part = DeviceArg::Partition(Box::new(DeviceArg::File(img.clone())), 2);
        assert_eq!(parse_device(&format!("file:{s}:2")).unwrap(), part);
        assert_eq!(parse_device(&format!("{s}:2")).unwrap(), part);
        // the base file must exist
        let missing = dir.path().join("missing.raw");
        let m = missing.to_str().unwrap();
        assert!(matches!(parse_device(&format!("file:{m}:2")), Err(Error::NotFound(_))));
        assert!(matches!(parse_device(&format!("{m}:2")), Err(Error::Invalid(_))));
        // a non-numeric suffix is not a partition
        assert!(parse_device(&format!("file:{s}:x")).is_err());
    }

    #[test]
    fn overlap_rules() {
        let mk = |id: Identity, range| Opened { dev: Box::new(MemDevice::new(0)), identity: id, range };
        let f = |s: &str| Identity::File(PathBuf::from(s));
        let whole = mk(f("a"), (0, 100));
        let other = mk(f("b"), (0, 100));
        assert!(!whole.overlaps(0, 100, &other));
        assert!(whole.overlaps(0, 100, &mk(f("a"), (0, 100))));
        // partitions of one disk: [10, 20) and [20, 30)
        let p1 = mk(Identity::Disk(0), (10, 20));
        let p2 = mk(Identity::Disk(0), (20, 30));
        assert!(!p1.overlaps(0, 10, &p2));
        assert!(p1.overlaps(0, 11, &p2));
        assert!(p1.overlaps(5, 10, &p2));
        assert!(!p2.overlaps(0, 10, &p1));
        assert!(!p1.overlaps(u64::MAX, 10, &p2));
    }

    #[test]
    fn open_device_variants() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.raw");
        FileDevice::create(&img, 4096).unwrap();
        let plat = MockPlatform::new();

        let d = open_device(&plat, &DeviceArg::File(img.clone()), OpenMode::ReadOnly).unwrap();
        assert!(d.is_read_only());
        assert_eq!(d.len(), 4096);
        let d = open_device(&plat, &DeviceArg::File(img), OpenMode::ReadWrite).unwrap();
        assert!(!d.is_read_only());

        plat.add_disk(0, MemDevice::new(8192), Default::default());
        let d = open_device(&plat, &DeviceArg::Disk(0), OpenMode::ReadOnly).unwrap();
        assert_eq!(d.len(), 8192);
        assert!(matches!(
            open_device(&plat, &DeviceArg::Disk(7), OpenMode::ReadOnly),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            open_device(&plat, &DeviceArg::Volume("E:".into()), OpenMode::ReadOnly),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            open_device(&plat, &DeviceArg::Volume(r"\\?\Volume{zzzz}\".into()), OpenMode::ReadOnly),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn resolve_letter_through_volumes() {
        let plat = MockPlatform::new();
        plat.add_disk(0, MemDevice::new(1 << 20), Default::default());
        plat.add_volume(
            VolumeInfo {
                id: VolumeId(r"\\?\Volume{aaaa}\".into()),
                mount_points: vec![r"D:\".into()],
                ..Default::default()
            },
            0,
            0,
            4096,
        );
        assert_eq!(resolve_volume(&plat, "d:").unwrap(), VolumeId(r"\\?\Volume{aaaa}\".into()));
        assert!(matches!(resolve_volume(&plat, "E:"), Err(Error::NotFound(_))));
        let raw = r"\\?\Volume{bbbb}\";
        assert_eq!(resolve_volume(&plat, raw).unwrap(), VolumeId(raw.into()));
        // a missing trailing backslash is added here too (mount / unmount call this directly)
        assert_eq!(
            resolve_volume(&plat, raw.trim_end_matches('\\')).unwrap(),
            VolumeId(raw.into())
        );
        assert!(resolve_volume(&plat, "nonsense").is_err());
    }
}
