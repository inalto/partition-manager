//! Total and free bytes of a mounted filesystem (`statvfs(3)`).

use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// `(total, free)` bytes of the filesystem holding `path`: `f_blocks` and `f_bfree` (free
/// including the root reserve, so `total - free` is what the files really use) times
/// `f_frsize`.
pub fn usage(path: &Path) -> io::Result<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut st = MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c` is a NUL-terminated path that outlives the call and `st` points to writable
    // memory of the right type; statvfs fills it on success and we read it only then.
    let r = unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs returned 0, so the structure is initialised.
    let st = unsafe { st.assume_init() };
    let unit = if st.f_frsize > 0 { st.f_frsize as u64 } else { st.f_bsize as u64 };
    Ok((st.f_blocks as u64 * unit, st.f_bfree as u64 * unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_of_the_temp_dir() {
        let dir = std::env::temp_dir();
        let (total, free) = usage(&dir).unwrap();
        assert!(total > 0 && free <= total, "{total} {free}");
        assert!(usage(Path::new("/nonexistent/pm-x")).is_err());
    }
}
