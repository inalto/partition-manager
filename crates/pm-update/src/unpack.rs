//! Unpacking of a verified release archive (`.zip` or `.tar.gz`).
//!
//! This runs with administrator rights and writes files that are installed
//! as executables, so it takes only the five expected files by exact name
//! and ignores everything else. Entries never choose their own destination:
//! the data of each wanted entry is read here and written to
//! `into/<name>`, created new.

use pm_core::{Error, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;

/// The executables of a release, without the platform suffix.
pub const EXECUTABLES: [&str; 2] = ["partition-manager", "pm"];
/// The documents of a release.
pub const DOCUMENTS: [&str; 3] = ["README.md", "LICENSE", "CHANGELOG.md"];

/// `base` with the platform's executable suffix.
pub fn exe_name(base: &str, windows: bool) -> String {
    if windows {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// Upper bound for one extracted file.
const MAX_FILE: u64 = 512 * 1024 * 1024;

/// What an archive entry is, as far as unpacking cares.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    File,
    Dir,
    /// Symlink, hardlink, device, ...: never taken.
    Other,
}

/// Decides which entries are taken and writes them into `into`.
struct Sink<'a> {
    into: &'a Path,
    /// Names wanted from this archive's platform.
    wanted: Vec<String>,
    /// The archive's single top-level folder, fixed by its first entry.
    top: Option<String>,
    written: Vec<String>,
    /// A zip is a Windows archive and may use `\` as separator (PowerShell
    /// `Compress-Archive`).
    windows: bool,
}

impl<'a> Sink<'a> {
    fn new(into: &'a Path, windows: bool) -> Self {
        let wanted = EXECUTABLES
            .iter()
            .map(|n| exe_name(n, windows))
            .chain(DOCUMENTS.iter().map(|n| n.to_string()))
            .collect();
        Sink { into, wanted, top: None, written: Vec::new(), windows }
    }

    /// The wanted file name `raw` denotes, if the entry is a regular file at
    /// exactly `<top>/<wanted name>`.
    fn select(&mut self, raw: &str, kind: Kind) -> Result<Option<String>> {
        if kind == Kind::Other {
            return Ok(None);
        }
        let name = if self.windows { raw.replace('\\', "/") } else { raw.to_string() };
        let mut path = name.as_str();
        while let Some(rest) = path.strip_prefix("./") {
            path = rest;
        }
        let path = path.strip_suffix('/').unwrap_or(path);
        let parts: Vec<&str> = path.split('/').collect();
        // Empty parts cover absolute paths and `//`; `.` and `..` cover
        // traversal.
        if parts.iter().any(|p| p.is_empty() || *p == "." || *p == "..") {
            return Ok(None);
        }
        let first = parts[0];
        // AppleDouble companions and anything that is not a plain folder name.
        if first.starts_with("._") || first.contains(['\\', ':']) {
            return Ok(None);
        }
        if self.top.is_none() && (kind == Kind::Dir || parts.len() > 1) {
            self.top = Some(first.to_string());
        }
        if self.top.as_deref() != Some(first) || kind != Kind::File || parts.len() != 2 {
            return Ok(None);
        }
        let file = parts[1];
        if !self.wanted.iter().any(|w| w == file) {
            return Ok(None);
        }
        if self.written.iter().any(|w| w == file) {
            return Err(Error::Invalid(format!("archive contains {file} twice")));
        }
        Ok(Some(file.to_string()))
    }

    /// Writes `data` to `into/<name>` (created new, never overwriting).
    fn store(&mut self, name: &str, data: &mut dyn Read) -> Result<()> {
        let dest = self.into.join(name);
        #[cfg(unix)]
        let mode = if EXECUTABLES.contains(&name) { 0o755 } else { 0o644 };
        let io = |e: std::io::Error| Error::Invalid(format!("cannot write {}: {e}", dest.display()));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        let mut out = opts.open(&dest).map_err(io)?;
        self.written.push(name.to_string());
        let copied = std::io::copy(&mut data.take(MAX_FILE + 1), &mut out).map_err(io)?;
        if copied > MAX_FILE {
            return Err(Error::Invalid(format!("{name} in the archive is too large")));
        }
        out.sync_all().map_err(io)?;
        drop(out);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The umask may have narrowed the mode given at creation.
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode)).map_err(io)?;
        }
        Ok(())
    }

    fn check_complete(&self) -> Result<()> {
        let gui = exe_name("partition-manager", self.windows);
        if !self.written.contains(&gui) {
            return Err(Error::Invalid(format!("the archive does not contain {gui}")));
        }
        Ok(())
    }
}

fn unreadable(archive: &Path, e: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("cannot read archive {}: {e}", archive.display()))
}

/// Extracts the wanted files of the archive flat into `into` (which must
/// exist) and returns their names. The format follows the extension (`.zip`
/// or `.tar.gz`); a `.zip` is a Windows archive (`.exe` executables).
/// Anything else in the archive is ignored. On error the files written so
/// far are removed.
pub fn unpack(archive: &Path, into: &Path) -> Result<Vec<String>> {
    let name = archive.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase());
    let name = name.unwrap_or_default();
    let is_zip = name.ends_with(".zip");
    if !is_zip && !name.ends_with(".tar.gz") {
        return Err(Error::Invalid(format!("unsupported archive {}", archive.display())));
    }
    let mut sink = Sink::new(into, is_zip);
    let result = if is_zip { unpack_zip(archive, &mut sink) } else { unpack_targz(archive, &mut sink) }
        .and_then(|()| sink.check_complete());
    match result {
        Ok(()) => Ok(sink.written),
        Err(e) => {
            for name in &sink.written {
                let _ = fs::remove_file(into.join(name));
            }
            Err(e)
        }
    }
}

fn unpack_zip(archive: &Path, sink: &mut Sink) -> Result<()> {
    let file = File::open(archive).map_err(|e| unreadable(archive, e))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| unreadable(archive, e))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| unreadable(archive, e))?;
        let named_dir = entry.name().ends_with(['/', '\\']);
        let kind = match entry.unix_mode().map(|m| m & 0o170000) {
            Some(0o040000) => Kind::Dir,
            Some(0o100000) | Some(0) | None if named_dir => Kind::Dir,
            Some(0o100000) | Some(0) | None => Kind::File,
            Some(_) => Kind::Other,
        };
        if let Some(name) = sink.select(entry.name(), kind)? {
            sink.store(&name, &mut entry)?;
        }
    }
    Ok(())
}

fn unpack_targz(archive: &Path, sink: &mut Sink) -> Result<()> {
    let file = File::open(archive).map_err(|e| unreadable(archive, e))?;
    let mut gz = flate2::read::GzDecoder::new(file);
    {
        let mut tar = tar::Archive::new(&mut gz);
        for entry in tar.entries().map_err(|e| unreadable(archive, e))? {
            let mut entry = entry.map_err(|e| unreadable(archive, e))?;
            let kind = match entry.header().entry_type() {
                t if t.is_file() => Kind::File,
                t if t.is_dir() => Kind::Dir,
                _ => Kind::Other,
            };
            let Ok(raw) = std::str::from_utf8(&entry.path_bytes()).map(str::to_string) else {
                continue;
            };
            if let Some(name) = sink.select(&raw, kind)? {
                sink.store(&name, &mut entry)?;
            }
        }
    }
    // The tar reader stops at the end-of-archive marker: read on so that a
    // truncated or corrupt gzip stream (checked at its end) is an error.
    std::io::copy(&mut gz, &mut std::io::sink()).map_err(|e| unreadable(archive, e))?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    const DOC_NAMES: [&str; 3] = DOCUMENTS;

    fn zip_with(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join("a.zip");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            if name.ends_with('/') || name.ends_with('\\') {
                w.add_directory(*name, opts).unwrap();
            } else {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
        }
        w.finish().unwrap();
        path
    }

    pub(crate) fn zip_fixture(dir: &Path, top: &str, sep: &str, with_dir: bool) -> PathBuf {
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        if with_dir {
            entries.push((format!("{top}{sep}"), Vec::new()));
        }
        for n in ["partition-manager.exe", "pm.exe"].iter().chain(DOC_NAMES.iter()) {
            entries.push((format!("{top}{sep}{n}"), format!("bin:{n}").into_bytes()));
        }
        let refs: Vec<(&str, &[u8])> =
            entries.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();
        zip_with(dir, &refs)
    }

    /// One tar entry; the name is written into the header verbatim so that
    /// hostile names (`..`, absolute) can be built.
    struct TarEntry {
        name: String,
        kind: tar::EntryType,
        data: Vec<u8>,
    }

    fn file(name: &str, data: &[u8]) -> TarEntry {
        TarEntry { name: name.into(), kind: tar::EntryType::Regular, data: data.to_vec() }
    }
    fn of_kind(name: &str, kind: tar::EntryType) -> TarEntry {
        TarEntry { name: name.into(), kind, data: Vec::new() }
    }

    fn targz_with(dir: &Path, entries: &[TarEntry]) -> PathBuf {
        let path = dir.join("a.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut b = tar::Builder::new(gz);
        for e in entries {
            let mut h = tar::Header::new_gnu();
            let raw = h.as_old_mut();
            raw.name[..e.name.len()].copy_from_slice(e.name.as_bytes());
            h.set_entry_type(e.kind);
            h.set_mode(0o644);
            h.set_size(e.data.len() as u64);
            h.set_cksum();
            b.append(&h, e.data.as_slice()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        path
    }

    pub(crate) fn targz_fixture(dir: &Path, top: &str) -> PathBuf {
        let mut entries = vec![of_kind(&format!("{top}/"), tar::EntryType::Directory)];
        for n in ["partition-manager", "pm"].iter().chain(DOC_NAMES.iter()) {
            entries.push(file(&format!("{top}/{n}"), format!("bin:{n}").as_bytes()));
        }
        targz_with(dir, &entries)
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn exe_name_adds_the_suffix_only_on_windows() {
        assert_eq!(exe_name("pm", true), "pm.exe");
        assert_eq!(exe_name("pm", false), "pm");
    }

    #[test]
    fn zip_and_targz_yield_the_five_files_flat() {
        let src = tempfile::tempdir().unwrap();
        let zip = zip_fixture(src.path(), "partition-manager-0.2.3-windows-x86_64", "/", true);
        let tgz = targz_fixture(src.path(), "linux-x86_64");
        for (archive, exes) in [
            (zip, ["partition-manager.exe", "pm.exe"]),
            (tgz, ["partition-manager", "pm"]),
        ] {
            let out = tempfile::tempdir().unwrap();
            let names = sorted(unpack(&archive, out.path()).unwrap());
            assert_eq!(names.len(), 5, "{names:?}");
            for e in exes {
                assert_eq!(std::fs::read(out.path().join(e)).unwrap(), format!("bin:{e}").as_bytes());
            }
            for d in DOCUMENTS {
                assert_eq!(std::fs::read(out.path().join(d)).unwrap(), format!("bin:{d}").as_bytes());
            }
            assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 5);
        }
    }

    #[test]
    fn zip_variants_with_backslashes_and_without_directory_entries() {
        let top = "partition-manager-0.2.3-windows-x86_64";
        for (sep, with_dir) in [("\\", true), ("\\", false), ("/", false)] {
            let src = tempfile::tempdir().unwrap();
            let zip = zip_fixture(src.path(), top, sep, with_dir);
            let out = tempfile::tempdir().unwrap();
            let names = unpack(&zip, out.path()).unwrap();
            assert_eq!(names.len(), 5, "sep {sep:?} dir {with_dir}: {names:?}");
            assert_eq!(std::fs::read(out.path().join("pm.exe")).unwrap(), b"bin:pm.exe");
        }
    }

    #[test]
    fn targz_with_dot_prefix_appledouble_and_directory_entries() {
        let src = tempfile::tempdir().unwrap();
        let tgz = targz_with(
            src.path(),
            &[
                file("./._macos-aarch64", b"apple"),
                of_kind("./macos-aarch64/", tar::EntryType::Directory),
                file("./macos-aarch64/._pm", b"apple"),
                file("./macos-aarch64/partition-manager", b"bin:partition-manager"),
                file("./macos-aarch64/pm", b"bin:pm"),
                file("./macos-aarch64/README.md", b"r"),
            ],
        );
        let out = tempfile::tempdir().unwrap();
        let names = sorted(unpack(&tgz, out.path()).unwrap());
        assert_eq!(names, ["README.md", "partition-manager", "pm"]);
        assert_eq!(std::fs::read(out.path().join("pm")).unwrap(), b"bin:pm");
    }

    #[test]
    fn extra_nested_and_escaping_entries_are_ignored() {
        let src = tempfile::tempdir().unwrap();
        let hostile = targz_with(
            src.path(),
            &[
                of_kind("top/", tar::EntryType::Directory),
                file("top/partition-manager", b"good"),
                file("top/evil.exe", b"evil"),
                file("top/sub/pm", b"nested"),
                file("../pm", b"escape"),
                file("/abs/pm", b"absolute"),
                file("top/../../pm", b"traversal"),
                file("other/pm", b"second top-level folder"),
                of_kind("top/pm", tar::EntryType::Symlink),
                of_kind("top/LICENSE", tar::EntryType::Link),
            ],
        );
        let work = tempfile::tempdir().unwrap();
        let into = work.path().join("out");
        std::fs::create_dir(&into).unwrap();
        let names = unpack(&hostile, &into).unwrap();
        assert_eq!(names, ["partition-manager"]);
        assert_eq!(std::fs::read(into.join("partition-manager")).unwrap(), b"good");
        assert_eq!(std::fs::read_dir(&into).unwrap().count(), 1);
        assert!(!work.path().join("pm").exists());
        assert!(!work.path().parent().unwrap().join("pm").exists());
    }

    #[test]
    fn hostile_zip_entries_are_ignored() {
        let src = tempfile::tempdir().unwrap();
        let zip = zip_with(
            src.path(),
            &[
                ("top/partition-manager.exe", b"good"),
                ("top/pm", b"wrong platform name"),
                ("top/sub/pm.exe", b"nested"),
                ("../pm.exe", b"escape"),
                ("/abs/pm.exe", b"absolute"),
                ("top\\..\\..\\pm.exe", b"traversal"),
                ("C:\\pm.exe", b"drive"),
                ("other\\pm.exe", b"second top-level folder"),
            ],
        );
        let out = tempfile::tempdir().unwrap();
        let names = unpack(&zip, out.path()).unwrap();
        assert_eq!(names, ["partition-manager.exe"]);
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1);
    }

    #[test]
    fn duplicate_wanted_entry_is_an_error() {
        let src = tempfile::tempdir().unwrap();
        let tgz = targz_with(
            src.path(),
            &[file("t/partition-manager", b"one"), file("t/partition-manager", b"two")],
        );
        let out = tempfile::tempdir().unwrap();
        assert!(unpack(&tgz, out.path()).is_err());
    }

    #[test]
    fn archive_without_the_gui_executable_is_an_error() {
        let src = tempfile::tempdir().unwrap();
        let tgz = targz_with(
            src.path(),
            &[file("t/pm", b"x"), file("t/README.md", b"x"), file("t/LICENSE", b"x")],
        );
        let out = tempfile::tempdir().unwrap();
        assert!(matches!(unpack(&tgz, out.path()), Err(Error::Invalid(m)) if m.contains("partition-manager")));
        let zip = zip_with(src.path(), &[("t/pm.exe", b"x"), ("t/partition-manager", b"x")]);
        assert!(matches!(unpack(&zip, out.path()), Err(Error::Invalid(_))));
    }

    #[test]
    fn truncated_or_garbage_archives_are_errors() {
        let src = tempfile::tempdir().unwrap();
        let big = vec![b'x'; 256 * 1024];
        let zip = zip_with(src.path(), &[("t/partition-manager.exe", &big), ("t/pm.exe", &big)]);
        let tgz = targz_with(src.path(), &[file("t/partition-manager", &big), file("t/pm", &big)]);
        for archive in [zip, tgz] {
            let bytes = std::fs::read(&archive).unwrap();
            let cut = src.path().join(format!("cut-{}", archive.file_name().unwrap().to_str().unwrap()));
            std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
            let out = tempfile::tempdir().unwrap();
            assert!(unpack(&cut, out.path()).is_err(), "{}", cut.display());
        }
        let zeros = src.path().join("zeros.zip");
        std::fs::write(&zeros, vec![0u8; 1024]).unwrap();
        let out = tempfile::tempdir().unwrap();
        assert!(unpack(&zeros, out.path()).is_err());
        let zeros = src.path().join("zeros.tar.gz");
        std::fs::write(&zeros, vec![0u8; 1024]).unwrap();
        assert!(unpack(&zeros, out.path()).is_err());
        let other = src.path().join("a.rar");
        std::fs::write(&other, b"x").unwrap();
        assert!(unpack(&other, out.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn executables_are_executable_documents_are_not() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        let tgz = targz_fixture(src.path(), "linux-x86_64");
        let out = tempfile::tempdir().unwrap();
        unpack(&tgz, out.path()).unwrap();
        for e in EXECUTABLES {
            let mode = std::fs::metadata(out.path().join(e)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "{e}");
        }
        for d in DOCUMENTS {
            let mode = std::fs::metadata(out.path().join(d)).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0, "{d}");
        }
    }
}
