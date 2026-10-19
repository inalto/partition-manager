//! Thin safe wrappers around every Win32 call the crate makes.
//!
//! This module (together with `device.rs`) is the only place with `unsafe` code. Each wrapper
//! returns the raw Win32 error code on failure (`Err(u32)`), so callers can branch on specific
//! codes (e.g. `ERROR_FILE_NOT_FOUND`) and add context when converting to `pm_core::Error`.
//! Structured IOCTL input/output is encoded/decoded as bytes by `layout.rs`.

use super::layout::{
    encode_property_query, encode_set_disk_attributes, encode_shrink_request, parse_disk_attributes,
    parse_geometry_ex, parse_length_info,
};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use windows_sys::Win32::Foundation::{
    CloseHandle, FreeLibrary, GetLastError, BOOL, BOOLEAN, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
    ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DeleteVolumeMountPointW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    FlushFileBuffers, GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumePathNamesForVolumeNameW, ReadFile, SetVolumeMountPointW, WriteFile, FILE_SHARE_READ,
    FILE_SHARE_WRITE, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
};
use windows_sys::Win32::System::Ioctl::{
    DISK_ATTRIBUTE_OFFLINE, FSCTL_DISMOUNT_VOLUME, FSCTL_EXTEND_VOLUME, FSCTL_LOCK_VOLUME,
    FSCTL_SHRINK_VOLUME, FSCTL_UNLOCK_VOLUME, IOCTL_DISK_GET_DISK_ATTRIBUTES,
    IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_DISK_SET_DISK_ATTRIBUTES, IOCTL_DISK_UPDATE_PROPERTIES, IOCTL_STORAGE_QUERY_PROPERTY,
    SHRINK_VOLUME_REQUEST_TYPES,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetSystemWindowsDirectoryW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};

/// Raw Win32 error code.
pub type Code = u32;

/// Upper bound for growing IOCTL output buffers.
const MAX_IOCTL_BUF: usize = 4 << 20;

/// An owned Win32 handle, closed on drop.
#[derive(Debug)]
pub struct Handle(HANDLE);

// SAFETY: a Win32 kernel-object handle is a process-wide value usable (and closable) from any
// thread; `Handle` only exposes it for synchronous calls.
unsafe impl Send for Handle {}
// SAFETY: see above; the wrapped calls do not rely on thread affinity.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.0` was returned by a successful open and is closed exactly once here.
        unsafe { CloseHandle(self.0) };
    }
}

/// `GetLastError()`.
pub fn last_error() -> Code {
    // SAFETY: no preconditions.
    unsafe { GetLastError() }
}

/// NUL-terminated UTF-16 copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// String up to the first NUL of a UTF-16 buffer.
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// `CreateFileW(path, access, FILE_SHARE_READ | FILE_SHARE_WRITE, OPEN_EXISTING)`.
///
/// `access == 0` opens a query-only handle (allowed without elevation for most query IOCTLs).
pub fn open(path: &str, access: u32) -> Result<Handle, Code> {
    let w = wide(path);
    // SAFETY: `w` is NUL-terminated and outlives the call; null security attributes and
    // template handle are allowed.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        Err(last_error())
    } else {
        Ok(Handle(h))
    }
}

/// `DeviceIoControl` with byte buffers; returns the number of output bytes.
pub fn ioctl(h: &Handle, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, Code> {
    let mut returned = 0u32;
    let in_ptr: *const c_void = if input.is_empty() { null() } else { input.as_ptr().cast() };
    let out_ptr: *mut c_void = if output.is_empty() {
        null_mut()
    } else {
        output.as_mut_ptr().cast()
    };
    // SAFETY: the buffers are valid for the given lengths for the whole synchronous call (no
    // OVERLAPPED is passed) and `returned` is a valid out-pointer.
    let ok = unsafe {
        DeviceIoControl(
            h.0,
            code,
            in_ptr,
            input.len() as u32,
            out_ptr,
            output.len() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok(returned as usize)
    }
}

/// [`ioctl`] with an output buffer that doubles while the driver reports it too small.
fn ioctl_grow(h: &Handle, code: u32, input: &[u8], initial: usize) -> Result<Vec<u8>, Code> {
    let mut size = initial;
    loop {
        let mut buf = vec![0u8; size];
        match ioctl(h, code, input, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                return Ok(buf);
            }
            Err(ERROR_INSUFFICIENT_BUFFER | ERROR_MORE_DATA) if size < MAX_IOCTL_BUF => size *= 2,
            Err(e) => return Err(e),
        }
    }
}

fn ioctl_simple(h: &Handle, code: u32) -> Result<(), Code> {
    ioctl(h, code, &[], &mut []).map(|_| ())
}

/// `IOCTL_DISK_GET_LENGTH_INFO` (needs read access).
pub fn length_info(h: &Handle) -> Result<u64, Code> {
    let mut b = [0u8; 8];
    ioctl(h, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut b)?;
    parse_length_info(&b).ok_or(ERROR_INSUFFICIENT_BUFFER)
}

/// `(BytesPerSector, DiskSize)` via `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` (query-only access).
pub fn geometry(h: &Handle) -> Result<(u32, u64), Code> {
    let buf = ioctl_grow(h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], 256)?;
    parse_geometry_ex(&buf).ok_or(ERROR_INSUFFICIENT_BUFFER)
}

/// `IOCTL_STORAGE_QUERY_PROPERTY` (standard query) for `property_id`; raw descriptor bytes.
pub fn query_property(h: &Handle, property_id: i32) -> Result<Vec<u8>, Code> {
    ioctl_grow(h, IOCTL_STORAGE_QUERY_PROPERTY, &encode_property_query(property_id), 1024)
}

/// Raw `DRIVE_LAYOUT_INFORMATION_EX` bytes.
pub fn drive_layout(h: &Handle) -> Result<Vec<u8>, Code> {
    ioctl_grow(h, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, &[], 48 + 144 * 16)
}

/// `GET_DISK_ATTRIBUTES.Attributes`.
pub fn disk_attributes(h: &Handle) -> Result<u64, Code> {
    let mut b = [0u8; 16];
    ioctl(h, IOCTL_DISK_GET_DISK_ATTRIBUTES, &[], &mut b)?;
    parse_disk_attributes(&b).ok_or(ERROR_INSUFFICIENT_BUFFER)
}

/// Set or clear `DISK_ATTRIBUTE_OFFLINE` (needs a read-write handle).
pub fn set_disk_offline(h: &Handle, offline: bool, persist: bool) -> Result<(), Code> {
    let attrs = if offline { DISK_ATTRIBUTE_OFFLINE } else { 0 };
    let input = encode_set_disk_attributes(attrs, DISK_ATTRIBUTE_OFFLINE, persist);
    ioctl(h, IOCTL_DISK_SET_DISK_ATTRIBUTES, &input, &mut []).map(|_| ())
}

/// `IOCTL_DISK_UPDATE_PROPERTIES`: make the OS re-read the partition table.
pub fn update_properties(h: &Handle) -> Result<(), Code> {
    ioctl_simple(h, IOCTL_DISK_UPDATE_PROPERTIES)
}

/// Raw `VOLUME_DISK_EXTENTS` bytes.
pub fn volume_extents(h: &Handle) -> Result<Vec<u8>, Code> {
    ioctl_grow(h, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], 8 + 24 * 4)
}

pub fn lock_volume(h: &Handle) -> Result<(), Code> {
    ioctl_simple(h, FSCTL_LOCK_VOLUME)
}

pub fn unlock_volume(h: &Handle) -> Result<(), Code> {
    ioctl_simple(h, FSCTL_UNLOCK_VOLUME)
}

pub fn dismount_volume(h: &Handle) -> Result<(), Code> {
    ioctl_simple(h, FSCTL_DISMOUNT_VOLUME)
}

/// `FSCTL_SHRINK_VOLUME` with the given request type (prepare / commit / abort).
pub fn shrink_volume(h: &Handle, request: SHRINK_VOLUME_REQUEST_TYPES, new_sectors: u64) -> Result<(), Code> {
    ioctl(h, FSCTL_SHRINK_VOLUME, &encode_shrink_request(request, new_sectors), &mut []).map(|_| ())
}

/// `FSCTL_EXTEND_VOLUME` to `new_sectors` total sectors.
pub fn extend_volume(h: &Handle, new_sectors: u64) -> Result<(), Code> {
    ioctl(h, FSCTL_EXTEND_VOLUME, &(new_sectors as i64).to_le_bytes(), &mut []).map(|_| ())
}

fn overlapped_at(offset: u64) -> OVERLAPPED {
    OVERLAPPED {
        Internal: 0,
        InternalHigh: 0,
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: offset as u32,
                OffsetHigh: (offset >> 32) as u32,
            },
        },
        hEvent: null_mut(),
    }
}

/// Synchronous `ReadFile` at `offset` (the OVERLAPPED only carries the position).
pub fn read_file_at(h: &Handle, offset: u64, buf: &mut [u8]) -> Result<usize, Code> {
    let mut ov = overlapped_at(offset);
    let mut n = 0u32;
    // SAFETY: `buf` is valid for `buf.len()` bytes; the handle is synchronous (no
    // FILE_FLAG_OVERLAPPED), so `ov` and `n` only need to live for the call.
    let ok = unsafe { ReadFile(h.0, buf.as_mut_ptr(), buf.len() as u32, &mut n, &mut ov) };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok(n as usize)
    }
}

/// Synchronous `WriteFile` at `offset`.
pub fn write_file_at(h: &Handle, offset: u64, buf: &[u8]) -> Result<usize, Code> {
    let mut ov = overlapped_at(offset);
    let mut n = 0u32;
    // SAFETY: as in `read_file_at`; `buf` is only read.
    let ok = unsafe { WriteFile(h.0, buf.as_ptr(), buf.len() as u32, &mut n, &mut ov) };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok(n as usize)
    }
}

pub fn flush(h: &Handle) -> Result<(), Code> {
    // SAFETY: valid handle.
    if unsafe { FlushFileBuffers(h.0) } == 0 {
        Err(last_error())
    } else {
        Ok(())
    }
}

/// All volume GUID paths (`\\?\Volume{...}\`) via `FindFirstVolumeW` / `FindNextVolumeW`.
pub fn list_volumes() -> Result<Vec<String>, Code> {
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is a writable buffer of the given length.
    let find = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let mut out = vec![from_wide(&buf)];
    let result = loop {
        // SAFETY: `find` is a live search handle; `buf` is writable.
        if unsafe { FindNextVolumeW(find, buf.as_mut_ptr(), buf.len() as u32) } == 0 {
            let e = last_error();
            break if e == ERROR_NO_MORE_FILES { Ok(()) } else { Err(e) };
        }
        out.push(from_wide(&buf));
    };
    // SAFETY: closes the search handle opened above, exactly once.
    unsafe { FindVolumeClose(find) };
    result.map(|_| out)
}

/// Mount points of a volume (`GetVolumePathNamesForVolumeNameW`), e.g. `["C:\\"]`.
pub fn volume_path_names(volume: &str) -> Result<Vec<String>, Code> {
    let name = wide(volume);
    let mut len = 512u32;
    loop {
        let mut buf = vec![0u16; len as usize];
        let mut needed = 0u32;
        // SAFETY: `name` is NUL-terminated; `buf` is writable for `len` units; `needed` is valid.
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(name.as_ptr(), buf.as_mut_ptr(), len, &mut needed)
        };
        if ok != 0 {
            // REG_MULTI_SZ-style list terminated by an empty string
            return Ok(buf
                .split(|&c| c == 0)
                .take_while(|s| !s.is_empty())
                .map(String::from_utf16_lossy)
                .collect());
        }
        let e = last_error();
        if e == ERROR_MORE_DATA && needed > len && (needed as usize) < MAX_IOCTL_BUF {
            len = needed;
        } else {
            return Err(e);
        }
    }
}

/// `(label, filesystem name)` from `GetVolumeInformationW(root)`; `root` ends with a backslash.
pub fn volume_information(root: &str) -> Result<(String, String), Code> {
    let r = wide(root);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    // SAFETY: `r` is NUL-terminated; both buffers are writable for their lengths; the optional
    // out-pointers are null.
    let ok = unsafe {
        GetVolumeInformationW(
            r.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            null_mut(),
            null_mut(),
            null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok((from_wide(&label), from_wide(&fs)))
    }
}

/// `(total bytes, free bytes)` from `GetDiskFreeSpaceExW(root)`.
pub fn disk_free_space(root: &str) -> Result<(u64, u64), Code> {
    let r = wide(root);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `r` is NUL-terminated; out-pointers are valid.
    let ok = unsafe { GetDiskFreeSpaceExW(r.as_ptr(), &mut avail, &mut total, &mut free) };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok((total, free))
    }
}

/// `SetVolumeMountPointW(mount_point, volume)`; both end with a backslash.
pub fn set_mount_point(mount_point: &str, volume: &str) -> Result<(), Code> {
    let m = wide(mount_point);
    let v = wide(volume);
    // SAFETY: both strings are NUL-terminated and outlive the call.
    if unsafe { SetVolumeMountPointW(m.as_ptr(), v.as_ptr()) } == 0 {
        Err(last_error())
    } else {
        Ok(())
    }
}

/// `DeleteVolumeMountPointW(mount_point)`; ends with a backslash.
pub fn delete_mount_point(mount_point: &str) -> Result<(), Code> {
    let m = wide(mount_point);
    // SAFETY: NUL-terminated string that outlives the call.
    if unsafe { DeleteVolumeMountPointW(m.as_ptr()) } == 0 {
        Err(last_error())
    } else {
        Ok(())
    }
}

/// `GetLogicalDrives()` bitmask (bit 0 = A:).
pub fn logical_drives() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetLogicalDrives() }
}

/// Interpret the return value `n` of a `Get*DirectoryW(buf, len)` call: 0 → `GetLastError`,
/// `n >= len` → the buffer was too small (`n` is then the required size; no error code is set).
fn directory_result(n: u32, buf: &[u16]) -> Result<String, Code> {
    if n == 0 {
        Err(last_error())
    } else if n as usize >= buf.len() {
        Err(ERROR_INSUFFICIENT_BUFFER)
    } else {
        Ok(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// `GetSystemWindowsDirectoryW()`, e.g. `C:\Windows`. Unlike `%SystemRoot%` this cannot be
/// redirected by the (medium-integrity) user environment.
pub fn windows_directory() -> Result<String, Code> {
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is writable for its length.
    let n = unsafe { GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
    directory_result(n, &buf)
}

/// `GetSystemDirectoryW()`, e.g. `C:\Windows\System32`.
pub fn system_directory() -> Result<String, Code> {
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is writable for its length.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) };
    directory_result(n, &buf)
}

/// Whether the current process token is elevated (`TokenElevation`).
pub fn token_is_elevated() -> bool {
    let mut token: HANDLE = null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle; `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let token = Handle(token);
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: `elevation` is a writable TOKEN_ELEVATION of the size passed.
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// System message text for a Win32 error code (`FormatMessageW`), without trailing period.
pub fn system_message(code: Code) -> Option<String> {
    let mut buf = [0u16; 1024];
    // SAFETY: FROM_SYSTEM | IGNORE_INSERTS reads neither a source nor arguments; `buf` is
    // writable for its length.
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            null(),
        )
    };
    if n == 0 {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..(n as usize).min(buf.len())]);
    let s = s.trim_end().trim_end_matches('.').trim_end();
    (!s.is_empty()).then(|| s.to_string())
}

// ---- fmifs.dll!FormatEx -------------------------------------------------------------------

/// `FMIFS_HARDDISK` media type.
const FMIFS_HARDDISK: u32 = 0xC;
/// Callback command codes (`FILE_SYSTEM_CALLBACK_COMMAND`); not in windows-sys.
const FCC_PROGRESS: u32 = 0;
const FCC_DONE: u32 = 11;

/// `BOOLEAN (WINAPI *)(FILE_SYSTEM_CALLBACK_COMMAND, ULONG, PVOID)`.
type FormatExCallback = unsafe extern "system" fn(command: u32, action: u32, data: *mut c_void) -> BOOLEAN;
/// `VOID FormatEx(PWCHAR DriveRoot, MEDIA_TYPE, PWCHAR FsName, PWCHAR Label, BOOL Quick,
/// ULONG ClusterSize, FILE_SYSTEM_CALLBACK)`; undocumented but stable (used by format.com).
type FormatExFn = unsafe extern "system" fn(
    drive_root: *const u16,
    media: u32,
    fs: *const u16,
    label: *const u16,
    quick: BOOL,
    cluster: u32,
    callback: FormatExCallback,
);

/// Serialises FormatEx calls: its callback has no user pointer, so state lives in statics.
static FORMAT_LOCK: Mutex<()> = Mutex::new(());
static FORMAT_PERCENT: AtomicU32 = AtomicU32::new(0);
/// 0 = no DONE seen, 1 = success, 2 = failure.
static FORMAT_DONE: AtomicU32 = AtomicU32::new(0);
/// Last callback command seen (diagnostics on failure).
static FORMAT_LAST_COMMAND: AtomicU32 = AtomicU32::new(0);

/// Exclusive right to run [`format_ex`]; resets the shared progress state.
pub struct FormatGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

pub fn format_lock() -> FormatGuard {
    let g = FORMAT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    FORMAT_PERCENT.store(0, Ordering::SeqCst);
    FORMAT_DONE.store(0, Ordering::SeqCst);
    FORMAT_LAST_COMMAND.store(0, Ordering::SeqCst);
    FormatGuard(g)
}

impl FormatGuard {
    /// Last percentage reported by the running FormatEx.
    pub fn percent(&self) -> u32 {
        FORMAT_PERCENT.load(Ordering::SeqCst)
    }
    pub fn last_command(&self) -> u32 {
        FORMAT_LAST_COMMAND.load(Ordering::SeqCst)
    }
}

extern "system" fn format_callback(command: u32, _action: u32, data: *mut c_void) -> BOOLEAN {
    if command != FCC_PROGRESS {
        FORMAT_LAST_COMMAND.store(command, Ordering::SeqCst);
    }
    if !data.is_null() {
        match command {
            // SAFETY: for FCC_PROGRESS fmifs passes a pointer to a ULONG percentage.
            FCC_PROGRESS => FORMAT_PERCENT.store(unsafe { *(data as *const u32) }, Ordering::SeqCst),
            FCC_DONE => {
                // SAFETY: for FCC_DONE fmifs passes a pointer to a BOOLEAN success flag (read as a
                // single byte, which is also correct if the callee widened it to a BOOL).
                let ok = unsafe { *(data as *const u8) } != 0;
                FORMAT_DONE.store(if ok { 1 } else { 2 }, Ordering::SeqCst);
            }
            _ => {}
        }
    }
    1 // TRUE: keep going
}

/// Run `fmifs.dll!FormatEx` on `root` (e.g. `E:\`). `Err(code)` means fmifs could not be
/// loaded (caller falls back to format.com); `Ok(success)` reports the DONE callback.
pub fn format_ex(
    _guard: &FormatGuard,
    root: &str,
    fs: &str,
    label: &str,
    quick: bool,
    cluster: u32,
) -> Result<bool, Code> {
    let dll = wide("fmifs.dll");
    // SAFETY: NUL-terminated library name; null file handle is required. The search is
    // restricted to System32 so a planted fmifs.dll next to the (elevated) exe is never loaded.
    let module = unsafe { LoadLibraryExW(dll.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err(last_error());
    }
    // SAFETY: `module` is a loaded library; the name is a NUL-terminated ANSI string.
    let Some(proc) = (unsafe { GetProcAddress(module, b"FormatEx\0".as_ptr()) }) else {
        let e = last_error();
        // SAFETY: releases the reference taken by LoadLibraryExW above.
        unsafe { FreeLibrary(module) };
        return Err(e);
    };
    // SAFETY: `FormatEx` has the `FormatExFn` signature (stable since Windows 2000); both are
    // `extern "system"` function pointers of equal size.
    let format: FormatExFn = unsafe { std::mem::transmute(proc) };
    let (r, f, l) = (wide(root), wide(fs), wide(label));
    // SAFETY: all strings are NUL-terminated and outlive the synchronous call; the callback is
    // a valid `extern "system"` function for the whole process lifetime.
    unsafe {
        format(
            r.as_ptr(),
            FMIFS_HARDDISK,
            f.as_ptr(),
            l.as_ptr(),
            quick as BOOL,
            cluster,
            format_callback,
        )
    };
    // SAFETY: releases the reference taken by LoadLibraryExW above; FormatEx has returned.
    unsafe { FreeLibrary(module) };
    Ok(FORMAT_DONE.load(Ordering::SeqCst) == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_roundtrip() {
        let w = wide("C:\\Windows");
        assert_eq!(*w.last().unwrap(), 0);
        assert_eq!(from_wide(&w), "C:\\Windows");
        assert_eq!(from_wide(&[0x41, 0x42]), "AB");
    }

    #[test]
    fn system_message_for_known_code() {
        // text is localised; just check something comes back and is trimmed
        let m = system_message(5).expect("message for ERROR_ACCESS_DENIED");
        assert!(!m.is_empty());
        assert!(!m.ends_with('.') && !m.ends_with('\n'));
    }

    #[test]
    fn system_directories_come_from_the_api() {
        let sys = system_directory().unwrap();
        let win = windows_directory().unwrap();
        assert!(sys.to_ascii_lowercase().ends_with("\\system32"), "{sys}");
        assert!(sys.to_ascii_lowercase().starts_with(&win.to_ascii_lowercase()), "{sys} / {win}");
        assert!(std::path::Path::new(&sys).join("fmifs.dll").exists());
    }

    #[test]
    fn directory_result_distinguishes_too_small_buffer() {
        let buf = [b'C' as u16, b':' as u16, 0, 0];
        assert_eq!(directory_result(2, &buf), Ok("C:".to_string()));
        assert_eq!(directory_result(4, &buf), Err(ERROR_INSUFFICIENT_BUFFER));
        assert_eq!(directory_result(300, &buf), Err(ERROR_INSUFFICIENT_BUFFER));
    }

    #[test]
    fn open_missing_device_reports_not_found() {
        let e = open("\\\\.\\PhysicalDrive999", 0).unwrap_err();
        assert_eq!(e, windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND);
    }
}
