//! Win32 error codes → `pm_core::Error` with user-friendly messages.

use super::ioctl::{self, Code};
use pm_core::Error;

/// Short, user-facing text for a Win32 error code; falls back to the system message
/// (`FormatMessageW`) and finally to `"Windows error <code>"`.
pub fn friendly_error(code: u32) -> String {
    let known = match code {
        5 => Some("Access denied – run as administrator"),
        32 => Some("Volume in use"),
        1117 => Some("I/O device error"),
        21 => Some("Device not ready"),
        23 => Some("Data error (CRC) – bad sector"),
        2 => Some("Device not found"),
        19 => Some("Media is write protected"),
        112 => Some("Not enough space on the disk"),
        1167 => Some("Device not connected"),
        _ => None,
    };
    match known {
        Some(s) => s.to_string(),
        None => ioctl::system_message(code).unwrap_or_else(|| format!("Windows error {code}")),
    }
}

/// `Error::Platform` for `code`, with `context` (what was being done) prefixed to the message.
pub fn platform_error(code: Code, context: &str) -> Error {
    let friendly = friendly_error(code);
    Error::Platform {
        code,
        msg: if context.is_empty() {
            friendly
        } else {
            format!("{context}: {friendly}")
        },
    }
}

/// `Error::Io` for a failed read/write of `len` bytes at `offset`.
pub fn io_error(code: Code, offset: u64, len: usize) -> Error {
    Error::io(offset, len, std::io::Error::from_raw_os_error(code as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_table_from_brief() {
        assert_eq!(friendly_error(5), "Access denied – run as administrator");
        assert_eq!(friendly_error(32), "Volume in use");
        assert_eq!(friendly_error(1117), "I/O device error");
        assert_eq!(friendly_error(21), "Device not ready");
        assert_eq!(friendly_error(23), "Data error (CRC) – bad sector");
    }

    #[test]
    fn unknown_codes_fall_back_to_system_text() {
        // ERROR_INVALID_FUNCTION has a system message
        let m = friendly_error(1);
        assert!(!m.is_empty());
        assert_ne!(m, "Windows error 1");
        // a code with no message at all
        assert_eq!(friendly_error(0x2FFF_FFFF), format!("Windows error {}", 0x2FFF_FFFFu32));
    }

    #[test]
    fn errors_carry_code_context_and_offsets() {
        match platform_error(5, r"open \\.\PhysicalDrive0") {
            Error::Platform { code, msg } => {
                assert_eq!(code, 5);
                assert_eq!(msg, r"open \\.\PhysicalDrive0: Access denied – run as administrator");
            }
            other => panic!("{other:?}"),
        }
        match io_error(23, 4096, 512) {
            Error::Io { offset, len, source } => {
                assert_eq!((offset, len), (4096, 512));
                assert_eq!(source.raw_os_error(), Some(23));
            }
            other => panic!("{other:?}"),
        }
    }
}
