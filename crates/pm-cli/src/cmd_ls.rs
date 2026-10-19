//! `pm ls`, `pm cat` and `pm extract`: browse and copy files out of the filesystem on a device
//! (a disk, partition, volume or image file) with the `pm-fs` readers.

use crate::cli::CliError;
use crate::device_arg::open_spec;
use crate::partial::path_err;
use pm_core::{format_size, CancelToken, Error};
use pm_fs::{Entry, FileSystem};
use pm_platform::{OpenMode, Platform};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// Directories nested deeper than this are not descended into (corrupt or hostile images).
const MAX_DEPTH: usize = 64;

fn out_err(e: std::io::Error) -> CliError {
    CliError::Runtime(Error::io(0, 0, e))
}

fn open_fs(platform: &dyn Platform, device: &str) -> Result<Box<dyn FileSystem>, CliError> {
    let dev = open_spec(platform, device, OpenMode::ReadOnly)?.dev;
    pm_fs::open(dev).map_err(|e| match e {
        Error::Unsupported(m) => CliError::Runtime(Error::Unsupported(format!(
            "{m} (to browse a partition address it as disk:N:P or file:PATH:P)"
        ))),
        other => CliError::Runtime(other),
    })
}

/// The entry at the `/`-separated `path` (`\` also separates). Names match exactly first, then
/// case-insensitively, as FAT and NTFS do.
fn resolve(fs: &mut dyn FileSystem, path: &str) -> Result<Entry, CliError> {
    let mut cur = fs.root();
    for comp in path.split(['/', '\\']).filter(|c| !c.is_empty() && *c != ".") {
        if !cur.is_dir {
            return Err(CliError::Usage(format!("'{}' is not a directory", cur.path)));
        }
        let entries = fs.list(&cur)?;
        let live = || entries.iter().filter(|e| !e.deleted);
        let found = live()
            .find(|e| e.name == comp)
            .or_else(|| live().find(|e| e.name.eq_ignore_ascii_case(comp)));
        cur = found
            .cloned()
            .ok_or_else(|| CliError::Usage(format!("no such file or directory: {path}")))?;
    }
    Ok(cur)
}

/// Unix seconds as `YYYY-MM-DD HH:MM:SS` (UTC, proleptic Gregorian).
pub fn format_time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // days since 1970-01-01 -> civil date (H. Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// `type size modified name`; control characters in names are shown as `?`.
fn format_entry(e: &Entry) -> String {
    let size = if e.is_dir { "-".to_string() } else { e.size.to_string() };
    let modified = e.modified.map_or_else(|| "-".to_string(), format_time);
    let name: String = e.name.chars().map(|c| if c.is_control() { '?' } else { c }).collect();
    format!(
        "{} {size:>12} {modified:<19} {name}",
        if e.is_dir { 'd' } else { '-' }
    )
}

pub fn run_ls(platform: &dyn Platform, device: &str, path: Option<&str>, out: &mut dyn Write) -> Result<(), CliError> {
    let mut fs = open_fs(platform, device)?;
    let entry = resolve(&mut *fs, path.unwrap_or("/"))?;
    let mut text = String::new();
    if entry.is_dir {
        let mut entries: Vec<Entry> = fs.list(&entry)?.into_iter().filter(|e| !e.deleted).collect();
        entries.sort_by_key(|e| (!e.is_dir, e.name.to_lowercase()));
        for e in &entries {
            text.push_str(&format_entry(e));
            text.push('\n');
        }
    } else {
        text.push_str(&format_entry(&entry));
        text.push('\n');
    }
    out.write_all(text.as_bytes()).map_err(out_err)
}

pub fn run_cat(platform: &dyn Platform, device: &str, path: &str, out: &mut dyn Write) -> Result<(), CliError> {
    let mut fs = open_fs(platform, device)?;
    let entry = resolve(&mut *fs, path)?;
    if entry.is_dir {
        return Err(CliError::Usage(format!("'{path}' is a directory")));
    }
    fs.extract(&entry, out)?;
    Ok(())
}

/// A safe local file name for the filesystem name `name`, or `None` when it cannot be used
/// (path separators, `.`/`..`, characters or endings Windows would reinterpret). Reserved
/// device names such as `NUL` get a `_` prefix. Images are untrusted input: a name must never
/// be able to leave the destination directory.
fn safe_name(name: &str) -> Option<String> {
    let bad = |c: char| c.is_control() || "/\\:*?\"<>|".contains(c);
    if name.is_empty() || name == "." || name == ".." || name.chars().any(bad) || name.ends_with(['.', ' ']) {
        return None;
    }
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    Some(if reserved { format!("_{name}") } else { name.to_string() })
}

#[derive(Default)]
struct Stats {
    files: u64,
    dirs: u64,
    bytes: u64,
    skipped: u64,
}

struct Extractor<'a> {
    fs: &'a mut dyn FileSystem,
    cancel: &'a CancelToken,
    stats: Stats,
    /// First-cluster / inode ids of the directories being descended into (cycle guard).
    ancestors: Vec<u64>,
}

impl Extractor<'_> {
    fn skip(&mut self, e: &Entry, why: &str) {
        eprintln!("warning: skipping {}: {why}", e.path);
        self.stats.skipped += 1;
    }

    fn entry(&mut self, e: &Entry, parent: &Path) -> Result<(), CliError> {
        self.cancel.check()?;
        if e.deleted {
            return Ok(());
        }
        let Some(name) = safe_name(&e.name) else {
            self.skip(e, "the name cannot be used as a local file name");
            return Ok(());
        };
        let target = parent.join(name);
        if e.is_dir {
            if self.ancestors.len() >= MAX_DEPTH || (e.id != 0 && self.ancestors.contains(&e.id)) {
                self.skip(e, "directory nesting too deep or looping");
                return Ok(());
            }
            std::fs::create_dir_all(&target).map_err(|err| path_err(&target, err))?;
            self.stats.dirs += 1;
            self.ancestors.push(e.id);
            let children = self.fs.list(e)?;
            for c in &children {
                self.entry(c, &target)?;
            }
            self.ancestors.pop();
        } else {
            let file = File::create(&target).map_err(|err| path_err(&target, err))?;
            let mut w = BufWriter::new(file);
            let n = self.fs.extract(e, &mut w)?;
            w.flush().map_err(|err| path_err(&target, err))?;
            self.stats.files += 1;
            self.stats.bytes += n;
        }
        Ok(())
    }
}

pub fn run_extract(
    platform: &dyn Platform,
    device: &str,
    path: &str,
    dest: &Path,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let mut fs = open_fs(platform, device)?;
    let entry = resolve(&mut *fs, path)?;
    std::fs::create_dir_all(dest).map_err(|e| path_err(dest, e))?;
    let is_root = entry.path == "/";
    let mut ex = Extractor {
        fs: &mut *fs,
        cancel,
        stats: Stats::default(),
        ancestors: Vec::new(),
    };
    if is_root {
        let children = ex.fs.list(&entry)?;
        for c in &children {
            ex.entry(c, dest)?;
        }
    } else {
        ex.entry(&entry, dest)?;
    }
    let s = &ex.stats;
    writeln!(
        out,
        "Extracted {} file(s) and {} folder(s), {} ({} bytes), to {}{}",
        s.files,
        s.dirs,
        format_size(s.bytes),
        s.bytes,
        dest.display(),
        if s.skipped > 0 { format!("; {} skipped", s.skipped) } else { String::new() }
    )
    .map_err(out_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formatting() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
        assert_eq!(format_time(1_710_498_030), "2024-03-15 10:20:30");
        assert_eq!(format_time(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(format_time(-1), "1969-12-31 23:59:59");
    }

    #[test]
    fn names_are_sanitised() {
        assert_eq!(safe_name("hello.txt").as_deref(), Some("hello.txt"));
        assert_eq!(safe_name("My File").as_deref(), Some("My File"));
        for bad in ["", ".", "..", "a/b", "a\\b", "..\\x", "c:evil", "x\0y", "dots.", "space ", ".. "] {
            assert_eq!(safe_name(bad), None, "{bad:?}");
        }
        assert_eq!(safe_name("NUL").as_deref(), Some("_NUL"));
        assert_eq!(safe_name("con.txt").as_deref(), Some("_con.txt"));
        assert_eq!(safe_name("COM1").as_deref(), Some("_COM1"));
        assert_eq!(safe_name("COM10").as_deref(), Some("COM10"));
        assert_eq!(safe_name("console").as_deref(), Some("console"));
    }

    #[test]
    fn entry_lines() {
        let mut e = Entry {
            name: "a\tb".into(),
            path: "/a\tb".into(),
            is_dir: false,
            size: 42,
            modified: Some(0),
            id: 3,
            deleted: false,
            attrs: 0,
        };
        let l = format_entry(&e);
        assert!(l.starts_with("- ") && l.contains(" 42 ") && l.contains("1970-01-01 00:00:00") && l.ends_with("a?b"), "{l}");
        e.is_dir = true;
        e.modified = None;
        let l = format_entry(&e);
        assert!(l.starts_with("d ") && l.contains(" - "), "{l}");
    }
}
