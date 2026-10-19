//! Parsing and opening of the `<device>` command line argument.

use pm_core::{BlockDevice, Error, FileDevice, Result, VolumeId};
use pm_platform::{OpenMode, Platform};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceArg {
    /// Physical disk by index (`disk:1`).
    Disk(u32),
    /// Drive letter (`D:`) or `\\?\Volume{..}\` path.
    Volume(String),
    /// Image file (`file:PATH`, or a bare path that exists).
    File(PathBuf),
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

/// Parse `"disk:1"`, `"D:"`, `"\\?\Volume{..}\"`, `"file:PATH"` or a path of an existing file.
pub fn parse_device(s: &str) -> Result<DeviceArg> {
    if let Some(n) = strip_prefix_ci(s, "disk:") {
        return n
            .parse::<u32>()
            .map(DeviceArg::Disk)
            .map_err(|_| Error::Invalid(format!("'{s}': expected disk:<index>, e.g. disk:1")));
    }
    if is_volume_path(s) {
        return Ok(DeviceArg::Volume(normalize_volume_path(s)));
    }
    if let Some(path) = strip_prefix_ci(s, "file:") {
        if path.is_empty() {
            return Err(Error::Invalid("file: needs a path".into()));
        }
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(Error::NotFound(format!("image file '{}' does not exist", path.display())));
        }
        return Ok(DeviceArg::File(path));
    }
    if let Some(c) = drive_letter(s) {
        return Ok(DeviceArg::Volume(format!("{c}:")));
    }
    if Path::new(s).is_file() {
        return Ok(DeviceArg::File(PathBuf::from(s)));
    }
    Err(Error::Invalid(format!(
        "'{s}' is not a device: use disk:N, a drive letter (D:), a \\\\?\\Volume{{..}}\\ path, or an existing image file"
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
    match a {
        DeviceArg::Disk(i) => p.open_disk(*i, mode).map_err(|e| {
            let missing = p.enumerate_disks().map_or(false, |d| !d.iter().any(|x| x.index == *i));
            if missing {
                Error::NotFound(format!("no disk with index {i}"))
            } else {
                e
            }
        }),
        DeviceArg::Volume(v) => {
            let id = resolve_volume(p, v)?;
            p.open_volume(&id, mode).map_err(|e| {
                let missing = p.enumerate_volumes().map_or(false, |vs| !vs.iter().any(|x| x.id == id));
                if missing {
                    Error::NotFound(format!("no volume {}", id.0))
                } else {
                    e
                }
            })
        }
        // Raw image file. Later tasks route image formats (VHD, VMDK, ...) through pm-image here.
        DeviceArg::File(path) => Ok(Box::new(FileDevice::open(path, mode == OpenMode::ReadOnly)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{MemDevice, VolumeInfo};
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
