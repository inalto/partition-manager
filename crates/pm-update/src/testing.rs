//! Test doubles shared by the tests of this crate and, through the `testing` feature, by the
//! tests of the application: a [`Fetch`] serving a fixed map of URLs, sample GitHub API
//! answers, and builders of release archives and whole fake releases. Never compiled into a
//! release build.

use crate::fetch::Fetch;
use crate::release::{Asset, Release, DOWNLOAD_PREFIX};
use crate::unpack::DOCUMENTS;
use crate::version::{Version, TARGET};
use pm_core::{Error, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// A [`Fetch`] that serves fixed bodies by exact URL and records every request; any other
/// URL is `Error::NotFound`.
pub struct MapFetch {
    files: std::collections::HashMap<String, Vec<u8>>,
    requested: std::sync::Mutex<Vec<String>>,
    /// When set, every stream fails with an I/O error after this many bytes.
    fail_after: Option<usize>,
}

/// Yields the first `n` bytes of `data`, then an I/O error.
struct BreaksAfter {
    data: std::io::Cursor<Vec<u8>>,
    left: usize,
}

impl Read for BreaksAfter {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "stream broke"));
        }
        let max = buf.len().min(self.left);
        let n = self.data.read(&mut buf[..max])?;
        self.left -= n;
        Ok(n)
    }
}

impl MapFetch {
    pub fn new() -> MapFetch {
        MapFetch { files: Default::default(), requested: Default::default(), fail_after: None }
    }

    pub fn with(mut self, url: &str, bytes: impl AsRef<[u8]>) -> MapFetch {
        self.files.insert(url.to_string(), bytes.as_ref().to_vec());
        self
    }

    /// A copy of this fetcher whose streams fail after `n` bytes.
    pub fn failing_after(&self, n: usize) -> MapFetch {
        MapFetch {
            files: self.files.clone(),
            requested: Default::default(),
            fail_after: Some(n),
        }
    }

    /// URLs asked for so far, in order.
    pub fn requested(&self) -> Vec<String> {
        self.requested.lock().unwrap().clone()
    }
}

impl Default for MapFetch {
    fn default() -> Self {
        MapFetch::new()
    }
}

impl Fetch for MapFetch {
    fn get(&self, url: &str) -> Result<Box<dyn Read + Send>> {
        self.requested.lock().unwrap().push(url.to_string());
        match self.files.get(url) {
            Some(b) => {
                let data = std::io::Cursor::new(b.clone());
                Ok(match self.fail_after {
                    // Only large bodies break: the small checksum file stays readable.
                    Some(left) if b.len() > left => Box::new(BreaksAfter { data, left }),
                    _ => Box::new(data),
                })
            }
            None => Err(Error::NotFound(url.to_string())),
        }
    }
}

// Sample GitHub "latest release" API responses.

pub const SAMPLE: &str = r#"{
  "tag_name": "v0.2.3",
  "name": "Partition Manager 0.2.3",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/v0.2.3",
  "draft": false,
  "prerelease": false,
  "assets": [
    {"name": "partition-manager-0.2.3-windows-x86_64.zip",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-windows-x86_64.zip",
     "size": 11111},
    {"name": "partition-manager-0.2.3-linux-x86_64.tar.gz",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-linux-x86_64.tar.gz",
     "size": 22222},
    {"name": "partition-manager-0.2.3-macos-aarch64.tar.gz",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-macos-aarch64.tar.gz",
     "size": 33333},
    {"name": "partition-manager-0.2.3-macos-x86_64.tar.gz",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-macos-x86_64.tar.gz",
     "size": 44444},
    {"name": "SHA256SUMS.txt",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/SHA256SUMS.txt",
     "size": 400}
  ]
}"#;

pub const SAMPLE_NO_SUMS: &str = r#"{
  "tag_name": "v0.2.3",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/v0.2.3",
  "draft": false,
  "prerelease": false,
  "assets": [
    {"name": "partition-manager-0.2.3-linux-x86_64.tar.gz",
     "browser_download_url": "https://github.com/inalto/partition-manager/releases/download/v0.2.3/partition-manager-0.2.3-linux-x86_64.tar.gz",
     "size": 22222}
  ]
}"#;

pub const SAMPLE_DRAFT: &str = r#"{
  "tag_name": "v0.2.4",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/v0.2.4",
  "draft": true,
  "prerelease": false,
  "assets": []
}"#;

pub const SAMPLE_PRERELEASE: &str = r#"{
  "tag_name": "v0.3.0",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/v0.3.0",
  "draft": false,
  "prerelease": true,
  "assets": []
}"#;

pub const SAMPLE_TAG_NIGHTLY: &str = r#"{
  "tag_name": "nightly",
  "body": "notes",
  "html_url": "https://github.com/inalto/partition-manager/releases/tag/nightly",
  "draft": false,
  "prerelease": false,
  "assets": []
}"#;

// Archive builders.

/// A zip `dir/a.zip` with `entries` (names ending in `/` or `\` are directories).
pub fn zip_with(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
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

/// A Windows release zip: the five files (contents `bin:<name>`) under `top` + `sep`,
/// optionally with a directory entry for `top`.
pub fn zip_fixture(dir: &Path, top: &str, sep: &str, with_dir: bool) -> PathBuf {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    if with_dir {
        entries.push((format!("{top}{sep}"), Vec::new()));
    }
    for n in ["partition-manager.exe", "pm.exe"].iter().chain(DOCUMENTS.iter()) {
        entries.push((format!("{top}{sep}{n}"), format!("bin:{n}").into_bytes()));
    }
    let refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();
    zip_with(dir, &refs)
}

/// One tar entry; the name is written into the header verbatim so that hostile names
/// (`..`, absolute) can be built.
pub struct TarEntry {
    name: String,
    kind: tar::EntryType,
    data: Vec<u8>,
}

/// A regular file entry.
pub fn file(name: &str, data: &[u8]) -> TarEntry {
    TarEntry { name: name.into(), kind: tar::EntryType::Regular, data: data.to_vec() }
}

/// An empty entry of `kind` (directory, link, ...).
pub fn of_kind(name: &str, kind: tar::EntryType) -> TarEntry {
    TarEntry { name: name.into(), kind, data: Vec::new() }
}

/// A `dir/a.tar.gz` with `entries`.
pub fn targz_with(dir: &Path, entries: &[TarEntry]) -> PathBuf {
    let path = dir.join("a.tar.gz");
    let gz = flate2::write::GzEncoder::new(std::fs::File::create(&path).unwrap(), flate2::Compression::default());
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

/// A Unix release tarball: the five files (contents `bin:<name>`) under `top/`.
pub fn targz_fixture(dir: &Path, top: &str) -> PathBuf {
    let mut entries = vec![of_kind(&format!("{top}/"), tar::EntryType::Directory)];
    for n in ["partition-manager", "pm"].iter().chain(DOCUMENTS.iter()) {
        entries.push(file(&format!("{top}/{n}"), format!("bin:{n}").as_bytes()));
    }
    targz_with(dir, &entries)
}

// Whole fake releases.

/// The target name the fixtures are built for: this platform's, or `linux-x86_64` on a
/// platform without release archives.
pub fn fixture_target() -> &'static str {
    TARGET.unwrap_or("linux-x86_64")
}

/// A fake release `version` for this platform (a `.zip` on Windows, a `.tar.gz` elsewhere)
/// whose executables and documents contain `bin:<name>`, and a [`MapFetch`] serving its
/// archive and `SHA256SUMS.txt` (with a wrong checksum unless `good_sum`). The release JSON
/// is not served; add it with [`release_json`] when a check is part of the test.
pub fn release_fixture(version: &str, good_sum: bool) -> (MapFetch, Release) {
    let target = fixture_target();
    let src = tempfile::tempdir().unwrap();
    let (archive, ext) = if target.starts_with("windows-") {
        (zip_fixture(src.path(), &format!("partition-manager-{version}-{target}"), "/", true), "zip")
    } else {
        (targz_fixture(src.path(), target), "tar.gz")
    };
    let bytes = std::fs::read(archive).unwrap();
    let name = format!("partition-manager-{version}-{target}.{ext}");
    let base = format!("{DOWNLOAD_PREFIX}v{version}");
    let digest = if good_sum { format!("{:x}", Sha256::digest(&bytes)) } else { "0".repeat(64) };
    let fetch = MapFetch::new()
        .with(&format!("{base}/SHA256SUMS.txt"), format!("{digest}  {name}\n"))
        .with(&format!("{base}/{name}"), &bytes);
    let release = Release {
        version: Version::parse(version).expect("fixture version is major.minor.patch"),
        notes: format!("What is new in {version}"),
        page_url: format!("https://github.com/inalto/partition-manager/releases/tag/v{version}"),
        archive: Some(Asset { name: name.clone(), url: format!("{base}/{name}"), size: bytes.len() as u64 }),
        sums_url: Some(format!("{base}/SHA256SUMS.txt")),
    };
    (fetch, release)
}

/// The GitHub "latest release" answer describing `r` (its archive and checksums, when it
/// has them, are its only assets).
pub fn release_json(r: &Release) -> String {
    let mut assets = Vec::new();
    if let Some(a) = &r.archive {
        assets.push(serde_json::json!({"name": a.name, "browser_download_url": a.url, "size": a.size}));
    }
    if let Some(url) = &r.sums_url {
        assets.push(serde_json::json!({"name": "SHA256SUMS.txt", "browser_download_url": url, "size": 100}));
    }
    serde_json::json!({
        "tag_name": format!("v{}", r.version),
        "body": r.notes,
        "html_url": r.page_url,
        "draft": false,
        "prerelease": false,
        "assets": assets,
    })
    .to_string()
}
