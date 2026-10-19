//! Process elevation and the disk holding the running Windows installation.

use super::ioctl;
use super::layout::parse_disk_extents;

/// Whether the process runs elevated (administrator token).
pub fn is_elevated() -> bool {
    ioctl::token_is_elevated()
}

/// `\\.\X:` for a path such as `C:\Windows`.
pub fn drive_device_of(path: &str) -> Option<String> {
    let b = path.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        .then(|| format!(r"\\.\{}:", (b[0] as char).to_ascii_uppercase()))
}

/// Disk number of the (first extent of the) volume holding `%SystemRoot%`.
pub fn find_system_disk() -> Option<u32> {
    let windir = ioctl::windows_directory().ok()?;
    let dev = drive_device_of(&windir)?;
    let h = ioctl::open(&dev, 0).ok()?;
    let extents = parse_disk_extents(&ioctl::volume_extents(&h).ok()?).ok()?;
    extents.first().map(|e| e.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_device_from_windows_dir() {
        assert_eq!(drive_device_of(r"C:\Windows").as_deref(), Some(r"\\.\C:"));
        assert_eq!(drive_device_of(r"d:\WINNT").as_deref(), Some(r"\\.\D:"));
        assert_eq!(drive_device_of(r"\\server\share"), None);
        assert_eq!(drive_device_of(""), None);
    }

    #[test]
    fn system_disk_is_found_without_elevation() {
        // works with a query-only handle, so it must succeed on any Windows machine
        assert!(find_system_disk().is_some());
    }
}
