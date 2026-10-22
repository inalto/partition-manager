//! Helpers shared by the Unix platforms (Linux, macOS).
//!
//! * [`FdDevice`]: a [`pm_core::BlockDevice`] over a raw device node (`pread`/`pwrite`).
//! * [`cmd`]: running system tools (`mkfs.*`, `smartctl`, `diskutil`, ...) without a shell.
//! * [`smartctl`]: parsing `smartctl -j -a` JSON into [`crate::SmartData`].
//! * [`statvfs`]: total / free bytes of a mounted filesystem.
//! * [`os_error`]: `std::io::Error` → [`pm_core::Error::Platform`] with the errno as code.

pub mod bench;
pub mod cmd;
pub mod fd_device;
pub mod smartctl;
pub mod statvfs;

pub use bench::{BenchFile, FileBenchTarget};
pub use fd_device::FdDevice;

use pm_core::Error;

/// Effective user id of this process.
pub fn euid() -> u32 {
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory of ours.
    unsafe { libc::geteuid() }
}

/// Whether this process runs as root (`geteuid() == 0`): the requirement for raw disk writes.
pub fn is_root() -> bool {
    euid() == 0
}

/// `Error::Platform` for an OS error, with the errno as code and `context` (what was being
/// done) in front. Permission errors say "access denied" so callers can show the sudo hint.
pub fn os_error(e: &std::io::Error, context: &str) -> Error {
    let code = e.raw_os_error().unwrap_or(0);
    let what = if matches!(code, libc::EACCES | libc::EPERM) {
        format!("access denied ({e})")
    } else {
        e.to_string()
    };
    Error::Platform {
        code: code as u32,
        msg: if context.is_empty() { what } else { format!("{context}: {what}") },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_errors_carry_errno_and_context() {
        let e = std::io::Error::from_raw_os_error(libc::EACCES);
        match os_error(&e, "open /dev/sda") {
            Error::Platform { code, msg } => {
                assert_eq!(code, libc::EACCES as u32);
                assert!(msg.starts_with("open /dev/sda: access denied"), "{msg}");
            }
            other => panic!("{other:?}"),
        }
        let e = std::io::Error::from_raw_os_error(libc::EBUSY);
        match os_error(&e, "") {
            Error::Platform { code, msg } => {
                assert_eq!(code, libc::EBUSY as u32);
                assert!(!msg.contains("access denied"), "{msg}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn euid_matches_root_flag() {
        assert_eq!(is_root(), euid() == 0);
    }
}
