//! `pm mount` / `pm unmount`: add or remove a drive letter or folder mount point.

use crate::cli::{arg_error, CliError, MountTarget};
use crate::device_arg::resolve_volume;
use pm_platform::{MountPoint, Platform};
use std::io::Write;

fn mount_point(t: &MountTarget) -> Result<MountPoint, CliError> {
    match (&t.letter, &t.folder) {
        (Some(c), None) => Ok(MountPoint::Letter(*c)),
        (None, Some(p)) => Ok(MountPoint::Folder(p.clone())),
        _ => Err(CliError::Usage("specify exactly one of --letter or --folder".into())),
    }
}

pub fn run_mount(
    platform: &dyn Platform,
    volume: &str,
    at: &MountTarget,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let at = mount_point(at)?;
    let id = resolve_volume(platform, volume).map_err(arg_error)?;
    platform.mount(&id, &at)?;
    writeln!(out, "Mounted {} at {}", id.0, at.to_path_string()).ok();
    Ok(())
}

pub fn run_unmount(
    platform: &dyn Platform,
    volume: &str,
    at: &MountTarget,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let at = mount_point(at)?;
    let id = resolve_volume(platform, volume).map_err(arg_error)?;
    platform.unmount(&id, &at)?;
    writeln!(out, "Unmounted {} from {}", id.0, at.to_path_string()).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{MemDevice, VolumeId, VolumeInfo};
    use pm_platform::MockPlatform;
    use std::path::PathBuf;

    fn plat() -> MockPlatform {
        let p = MockPlatform::new();
        p.add_disk(0, MemDevice::new(1 << 20), Default::default());
        p.add_volume(
            VolumeInfo {
                id: VolumeId(r"\\?\Volume{aaaa}\".into()),
                mount_points: vec![r"D:\".into()],
                ..Default::default()
            },
            0,
            0,
            4096,
        );
        p
    }

    #[test]
    fn mount_by_volume_path_and_unmount_by_letter() {
        let p = plat();
        let mut out = Vec::new();
        let at = MountTarget {
            letter: Some('X'),
            folder: None,
        };
        run_mount(&p, r"\\?\Volume{aaaa}\", &at, &mut out).unwrap();
        assert!(String::from_utf8(out.clone()).unwrap().contains(r"X:\"));
        assert_eq!(
            p.mounted(),
            vec![(VolumeId(r"\\?\Volume{aaaa}\".into()), MountPoint::Letter('X'))]
        );
        // `D:` resolves through the volume's mount points
        run_unmount(&p, "D:", &at, &mut out).unwrap();
        assert!(p.mounted().is_empty());
    }

    #[test]
    fn folder_mount_and_errors() {
        let p = plat();
        let mut out = Vec::new();
        let folder = MountTarget {
            letter: None,
            folder: Some(PathBuf::from(r"C:\mnt\x")),
        };
        run_mount(&p, "D:", &folder, &mut out).unwrap();
        assert_eq!(p.mounted()[0].1, MountPoint::Folder(PathBuf::from(r"C:\mnt\x")));

        let letter = MountTarget {
            letter: Some('Q'),
            folder: None,
        };
        // unknown drive letter / garbage are argument errors
        assert!(matches!(run_mount(&p, "Z:", &letter, &mut out), Err(CliError::Usage(_))));
        assert!(matches!(run_mount(&p, "junk", &letter, &mut out), Err(CliError::Usage(_))));
        let none = MountTarget {
            letter: None,
            folder: None,
        };
        assert!(matches!(run_mount(&p, "D:", &none, &mut out), Err(CliError::Usage(_))));
    }
}
