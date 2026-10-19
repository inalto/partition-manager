//! Deleted-file recovery on top of [`pm_fs::FileSystem`].

use pm_core::{CancelToken, Error, ProgressSink, ProgressTracker, Result};
use pm_fs::{Entry, FileSystem};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bytes read from each end of a file when judging its integrity.
const PROBE_LEN: u64 = 4096;
/// Longest file name written by [`restore_file`], in characters.
const MAX_NAME_CHARS: usize = 200;
/// Collision suffixes tried before giving up.
const MAX_SUFFIX: u32 = 10_000;

/// A deleted directory entry together with an estimate of how recoverable it is.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeletedFile {
    pub entry: Entry,
    /// 0-100 estimate of how likely the content is intact, see [`deleted_files`].
    pub integrity: u8,
    /// Byte offset of the filesystem on the device it was found on (0 when unknown).
    pub fs_offset: u64,
}

/// List the deleted files of `fs` with an integrity estimate; `fs_offset` is recorded as 0.
/// See [`deleted_files_at`].
pub fn deleted_files(
    fs: &mut dyn FileSystem,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Vec<DeletedFile>> {
    deleted_files_at(fs, 0, progress, cancel)
}

/// Like [`deleted_files`], recording `fs_offset` (where the filesystem starts on the device)
/// in every result.
///
/// The integrity estimate is deterministic and cheap:
///
/// * directories: 0 (they cannot be restored as files);
/// * empty files: 100;
/// * otherwise the first and the last `min(4 KiB, size, cluster size)` bytes are read through
///   the filesystem: both readable gives 80, a readable start but unreadable end 40 (typically
///   a FAT chain freed by the delete), an unreadable start 10.
///
/// Not considered: whether the file's clusters have since been reused by another file. The
/// generic [`Entry`] does not say where a file's data lives, so the used-block map cannot be
/// consulted here.
pub fn deleted_files_at(
    fs: &mut dyn FileSystem,
    fs_offset: u64,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Vec<DeletedFile>> {
    cancel.check()?;
    let entries = fs.deleted_entries(progress, cancel)?;
    let cluster = fs.cluster_size() as u64;
    let mut tracker = ProgressTracker::new(progress, "Checking deleted files", entries.len() as u64);
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        cancel.check()?;
        let integrity = estimate_integrity(fs, &entry, cluster);
        out.push(DeletedFile { entry, integrity, fs_offset });
        tracker.add(1);
    }
    tracker.finish();
    Ok(out)
}

fn estimate_integrity(fs: &mut dyn FileSystem, e: &Entry, cluster: u64) -> u8 {
    if e.is_dir {
        return 0;
    }
    if e.size == 0 {
        return 100;
    }
    let probe = PROBE_LEN.min(e.size).min(if cluster == 0 { PROBE_LEN } else { cluster });
    if !readable(fs, e, 0, probe) {
        return 10;
    }
    if readable(fs, e, e.size - probe, probe) {
        80
    } else {
        40
    }
}

fn readable(fs: &mut dyn FileSystem, e: &Entry, offset: u64, len: u64) -> bool {
    let mut buf = vec![0u8; len as usize];
    matches!(fs.read_file(e, offset, &mut buf), Ok(n) if n as u64 == len)
}

fn is_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// A file name that is safe to create inside a chosen directory on any platform.
fn sanitize_name(e: &Entry) -> String {
    // FAT deleted entries are named "?" + the rest of the 8.3 name
    let raw = e.name.trim_start_matches('?');
    let mut s: String = raw
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .take(MAX_NAME_CHARS)
        .collect();
    s = s.trim_matches(' ').trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() {
        s = format!("recovered_{}", e.id);
    }
    if is_reserved_device_name(&s) {
        s.insert(0, '_');
    }
    s
}

/// `name` with `_<n>` inserted before the extension.
fn with_suffix(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{}_{}{}", &name[..i], n, &name[i..]),
        _ => format!("{name}_{n}"),
    }
}

/// Copy a deleted file's content to `dest_dir/<sanitized name>` and return the path.
///
/// Path separators and `: * ? " < > |` become `_`, the leading `?` of FAT deleted names is
/// dropped, and a name already taken gets a numeric suffix (`name_1.ext`). `dest_dir` is created
/// when missing. If reading fails part-way the partial file is removed and the error returned.
pub fn restore_file(fs: &mut dyn FileSystem, f: &DeletedFile, dest_dir: &Path) -> Result<PathBuf> {
    if f.entry.is_dir {
        return Err(Error::Invalid(format!("{} is a directory", f.entry.path)));
    }
    std::fs::create_dir_all(dest_dir).map_err(|e| Error::io(0, 0, e))?;
    let base = sanitize_name(&f.entry);
    let mut n = 0u32;
    let (path, file) = loop {
        let name = if n == 0 { base.clone() } else { with_suffix(&base, n) };
        let path = dest_dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break (path, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < MAX_SUFFIX => n += 1,
            Err(e) => return Err(Error::io(0, 0, e)),
        }
    };
    let mut out = std::io::BufWriter::new(file);
    let result = fs
        .extract(&f.entry, &mut out)
        .and_then(|_| out.flush().map_err(|e| Error::io(0, 0, e)));
    drop(out);
    match result {
        Ok(()) => Ok(path),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.into(),
            path: format!("/{name}"),
            is_dir: false,
            size: 0,
            modified: None,
            id: 9,
            deleted: true,
            attrs: 0,
        }
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name(&entry("?gone.txt")), "gone.txt");
        assert_eq!(sanitize_name(&entry("a/b\\c:d")), "a_b_c_d");
        assert_eq!(sanitize_name(&entry("trailing. . ")), "trailing");
        assert_eq!(sanitize_name(&entry("..")), "recovered_9");
        assert_eq!(sanitize_name(&entry("")), "recovered_9");
        assert_eq!(sanitize_name(&entry("nul.txt")), "_nul.txt");
        assert_eq!(sanitize_name(&entry("COM1")), "_COM1");
        assert_eq!(sanitize_name(&entry("COM0")), "COM0");
        assert_eq!(sanitize_name(&entry("tab\tname")), "tab_name");
        assert_eq!(sanitize_name(&entry(&"x".repeat(500))).chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn suffix_goes_before_the_extension() {
        assert_eq!(with_suffix("a.txt", 2), "a_2.txt");
        assert_eq!(with_suffix("a.tar.gz", 1), "a.tar_1.gz");
        assert_eq!(with_suffix("noext", 3), "noext_3");
        assert_eq!(with_suffix(".hidden", 1), ".hidden_1");
    }
}
