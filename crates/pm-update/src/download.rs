//! Download of a release archive with SHA-256 verification.
//!
//! This runs with administrator rights: the archive is only handed on after
//! its digest matched the published `SHA256SUMS.txt`, and a failed download
//! never leaves a file behind.

use crate::fetch::{read_limited, Fetch};
use crate::release::{Asset, Release};
use pm_core::{CancelToken, Error, ProgressSink, ProgressTracker, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Upper bound for `SHA256SUMS.txt`.
const MAX_SUMS: u64 = 64 * 1024;
/// Streaming chunk size.
const CHUNK: usize = 64 * 1024;

/// The lower-case hex digest listed for exactly `name` in a `sha256sum`
/// style file (`<hex>  name` or `<hex> *name`).
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let line = line.trim();
        let (hex, rest) = line.split_once(char::is_whitespace)?;
        let file = rest.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_ascii_lowercase())
    })
}

/// SHA-256 of the file at `path`, as lower-case hex.
pub fn sha256_hex(path: &Path) -> Result<String> {
    let mut f = File::open(path)
        .map_err(|e| Error::Invalid(format!("cannot open {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| Error::Invalid(format!("cannot read {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Downloads the release archive into `staging` (which must exist) and
/// verifies it against the release's `SHA256SUMS.txt`. Returns the path of
/// the verified archive. On any error the partial archive is removed.
pub fn download(
    fetch: &dyn Fetch,
    release: &Release,
    staging: &Path,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<PathBuf> {
    let asset = release
        .archive
        .as_ref()
        .ok_or_else(|| Error::Unsupported("this release has no archive for this system".into()))?;
    download_asset(fetch, release, asset, staging, progress, cancel)
}

/// [`download`] for the release's installer (`...-setup.exe`): what an installed copy
/// updates with. Verified against `SHA256SUMS.txt` in the same way.
pub fn download_installer(
    fetch: &dyn Fetch,
    release: &Release,
    staging: &Path,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<PathBuf> {
    let asset = release.installer.as_ref().ok_or_else(|| {
        Error::Unsupported("this release has no installer; download it from the release page".into())
    })?;
    download_asset(fetch, release, asset, staging, progress, cancel)
}

fn download_asset(
    fetch: &dyn Fetch,
    release: &Release,
    asset: &Asset,
    staging: &Path,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<PathBuf> {
    let sums_url = release
        .sums_url
        .as_deref()
        .ok_or_else(|| Error::Unsupported("this release has no SHA256SUMS.txt".into()))?;
    // The name becomes a path inside `staging`; it must be a plain file name.
    let plain = Path::new(&asset.name).file_name().map(|n| n == asset.name.as_str());
    if asset.name.is_empty() || plain != Some(true) {
        return Err(Error::Invalid(format!("unsafe archive name {:?}", asset.name)));
    }
    cancel.check()?;

    let sums = read_limited(fetch.get(sums_url)?, MAX_SUMS)?;
    let sums = std::str::from_utf8(&sums)
        .map_err(|_| Error::Invalid("SHA256SUMS.txt is not UTF-8".into()))?;
    let expected = sum_for(sums, &asset.name)
        .ok_or_else(|| Error::Invalid(format!("SHA256SUMS.txt has no entry for {}", asset.name)))?;

    cancel.check()?;
    let mut body = fetch.get(&asset.url)?;
    let path = staging.join(&asset.name);
    // Fails when the file exists: a file we did not create is never removed.
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| Error::Invalid(format!("cannot create {}: {e}", path.display())))?;

    let streamed = stream(&mut body, file, asset.size, progress, cancel);
    let verified = streamed.and_then(|actual| {
        if actual == expected {
            Ok(())
        } else {
            Err(Error::Invalid(format!("checksum mismatch for {}", asset.name)))
        }
    });
    match verified {
        Ok(()) => Ok(path),
        Err(e) => {
            let _ = fs::remove_file(&path);
            Err(e)
        }
    }
}

/// Copies `body` into `file` in chunks and returns the SHA-256 hex of what
/// was written. Fails on cancellation, on a read or write error, and when the
/// body is longer or shorter than `size`. Closes the file before returning.
fn stream(
    body: &mut dyn Read,
    mut file: File,
    size: u64,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<String> {
    let mut tracker = ProgressTracker::new(progress, "Downloading", size);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    loop {
        cancel.check()?;
        let n = body.read(&mut buf).map_err(|e| Error::Invalid(format!("download failed: {e}")))?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > size {
            return Err(Error::Invalid(format!("download is larger than the announced {size} bytes")));
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| Error::Invalid(format!("cannot write the archive: {e}")))?;
        tracker.add(n as u64);
    }
    if done != size {
        return Err(Error::Invalid(format!("download ended after {done} of {size} bytes")));
    }
    file.sync_all().map_err(|e| Error::Invalid(format!("cannot write the archive: {e}")))?;
    tracker.finish();
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MapFetch;
    use crate::release::Asset;
    use crate::version::Version;
    use pm_core::{NullProgress, ProgressReport};
    use std::sync::Mutex;

    const NAME: &str = "partition-manager-0.3.0-linux-x86_64.tar.gz";
    const ARCHIVE_URL: &str = "https://github.com/inalto/partition-manager/releases/download/v0.3.0/partition-manager-0.3.0-linux-x86_64.tar.gz";
    const SUMS_URL: &str =
        "https://github.com/inalto/partition-manager/releases/download/v0.3.0/SHA256SUMS.txt";

    #[derive(Default)]
    struct RecordingProgress(Mutex<Vec<ProgressReport>>);

    impl RecordingProgress {
        fn last(&self) -> ProgressReport {
            self.0.lock().unwrap().last().cloned().expect("at least one report")
        }
    }

    impl ProgressSink for RecordingProgress {
        fn report(&self, p: &ProgressReport) {
            self.0.lock().unwrap().push(p.clone());
        }
    }

    fn payload() -> Vec<u8> {
        (0..300 * 1024u32).map(|i| (i % 251) as u8).collect()
    }

    fn hex_of(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// A release whose archive announces `announced` bytes.
    fn release(announced: u64) -> Release {
        Release {
            version: Version::parse("0.3.0").unwrap(),
            notes: String::new(),
            page_url: String::new(),
            archive: Some(Asset { name: NAME.into(), url: ARCHIVE_URL.into(), size: announced }),
            installer: None,
            sums_url: Some(SUMS_URL.into()),
        }
    }

    fn fixture() -> (MapFetch, Release, Vec<u8>) {
        let bytes = payload();
        let sums = format!("{}  {NAME}\n", hex_of(&bytes));
        let f = MapFetch::new().with(SUMS_URL, sums).with(ARCHIVE_URL, &bytes);
        (f, release(bytes.len() as u64), bytes)
    }

    fn fixture_bad_sum() -> (MapFetch, Release) {
        let (_, rel, bytes) = fixture();
        let sums = format!("{}  {NAME}\n", hex_of(b"something else"));
        (MapFetch::new().with(SUMS_URL, sums).with(ARCHIVE_URL, &bytes), rel)
    }

    fn fixture_no_line() -> (MapFetch, Release) {
        let (_, rel, bytes) = fixture();
        let sums = format!("{}  other-file.zip\n", hex_of(&bytes));
        (MapFetch::new().with(SUMS_URL, sums).with(ARCHIVE_URL, &bytes), rel)
    }

    fn fixture_longer_than_announced() -> (MapFetch, Release) {
        let (f, _, bytes) = fixture();
        (f, release(bytes.len() as u64 - 1000))
    }

    fn fixture_shorter_than_announced() -> (MapFetch, Release) {
        let (f, _, bytes) = fixture();
        (f, release(bytes.len() as u64 + 1000))
    }

    fn entries(dir: &Path) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn sum_lines_in_both_sha256sum_styles() {
        let s = "aa11  a.zip\nBB22 *b.tar.gz\n";
        assert_eq!(sum_for(s, "a.zip").as_deref(), Some("aa11"));
        assert_eq!(sum_for(s, "b.tar.gz").as_deref(), Some("bb22"));
        assert_eq!(sum_for(s, "c.zip"), None);
        assert_eq!(sum_for(s, "zip"), None); // whole name, not a suffix
        assert_eq!(sum_for("aa11  a.zip\r\n", "a.zip").as_deref(), Some("aa11")); // CRLF file
        assert_eq!(sum_for("not-hex  a.zip\n", "a.zip"), None);
    }

    #[test]
    fn sha256_hex_matches_a_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc");
        fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_hex(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(sha256_hex(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn download_verifies_and_reports_progress() {
        let (f, rel, bytes) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let seen = RecordingProgress::default();
        let p = download(&f, &rel, dir.path(), &seen, &CancelToken::new()).unwrap();
        assert_eq!(p, dir.path().join(NAME));
        assert_eq!(fs::read(&p).unwrap(), bytes);
        let last = seen.last();
        assert_eq!((last.done, last.total), (bytes.len() as u64, bytes.len() as u64));
        assert_eq!(last.phase, "Downloading");
        assert_eq!(f.requested()[0], rel.sums_url.clone().unwrap(), "checksums first");
    }

    #[test]
    fn wrong_checksum_missing_line_and_wrong_size_fail_and_leave_nothing() {
        for broken in [
            fixture_bad_sum(),
            fixture_no_line(),
            fixture_longer_than_announced(),
            fixture_shorter_than_announced(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            assert!(download(&broken.0, &broken.1, dir.path(), &NullProgress, &CancelToken::new())
                .is_err());
            assert_eq!(entries(dir.path()), 0);
        }
    }

    #[test]
    fn cancel_and_broken_stream_leave_nothing() {
        let (f, rel, _) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let c = CancelToken::new();
        c.cancel();
        assert!(matches!(download(&f, &rel, dir.path(), &NullProgress, &c), Err(Error::Cancelled)));
        assert!(download(&f.failing_after(1000), &rel, dir.path(), &NullProgress, &CancelToken::new())
            .is_err());
        assert_eq!(entries(dir.path()), 0);
    }

    #[test]
    fn cancel_in_the_middle_of_the_stream_leaves_nothing() {
        /// Serves the archive but cancels the token after the first read.
        struct CancelsWhileReading(MapFetch, CancelToken);
        struct Reader(Box<dyn Read + Send>, CancelToken);
        impl Read for Reader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.read(buf)?;
                self.1.cancel();
                Ok(n)
            }
        }
        impl Fetch for CancelsWhileReading {
            fn get(&self, url: &str) -> Result<Box<dyn Read + Send>> {
                let r = self.0.get(url)?;
                Ok(if url == ARCHIVE_URL { Box::new(Reader(r, self.1.clone())) } else { r })
            }
        }
        let (f, rel, _) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let c = CancelToken::new();
        let f = CancelsWhileReading(f, c.clone());
        assert!(matches!(download(&f, &rel, dir.path(), &NullProgress, &c), Err(Error::Cancelled)));
        assert_eq!(entries(dir.path()), 0);
    }

    #[test]
    fn no_archive_or_no_sums_is_unsupported_and_requests_nothing() {
        let (f, rel, _) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let no_archive = Release { archive: None, ..rel.clone() };
        let no_sums = Release { sums_url: None, ..rel };
        for r in [no_archive, no_sums] {
            let e = download(&f, &r, dir.path(), &NullProgress, &CancelToken::new());
            assert!(matches!(e, Err(Error::Unsupported(_))), "{e:?}");
        }
        assert!(f.requested().is_empty());
        assert_eq!(entries(dir.path()), 0);
    }

    #[test]
    fn an_archive_name_with_a_path_is_refused_without_requests() {
        let (f, mut rel, _) = fixture();
        for bad in ["../evil.tar.gz", "sub/evil.tar.gz", "..", ""] {
            rel.archive.as_mut().unwrap().name = bad.into();
            let dir = tempfile::tempdir().unwrap();
            assert!(download(&f, &rel, dir.path(), &NullProgress, &CancelToken::new()).is_err());
            assert_eq!(entries(dir.path()), 0);
        }
        assert!(f.requested().is_empty());
    }

    #[test]
    fn an_existing_file_is_neither_overwritten_nor_removed() {
        let (f, rel, _) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join(NAME);
        fs::write(&existing, b"precious").unwrap();
        assert!(download(&f, &rel, dir.path(), &NullProgress, &CancelToken::new()).is_err());
        assert_eq!(fs::read(&existing).unwrap(), b"precious");
    }
}
