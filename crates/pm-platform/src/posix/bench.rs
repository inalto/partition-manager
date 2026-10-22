//! Benchmark scratch files for Unix: uncached temp files that cannot outlive the process.
//!
//! [`FileBenchTarget`] hands out [`BenchFile`] devices. On Linux they are reopened with
//! `O_DIRECT` (falling back to ordinary buffered I/O when the filesystem refuses it, e.g. tmpfs),
//! on macOS `F_NOCACHE` is switched on, so the numbers measure the volume rather than the page
//! cache. The file is unlinked as soon as it is open, so it disappears when the device is
//! dropped, even if the process dies. Direct I/O needs aligned offsets, lengths *and* memory
//! addresses; `BenchFile` therefore copies through a page-aligned buffer and handles unaligned
//! requests with read-modify-write. The benchmark itself uses multiples of 4096 (fast path).
//!
//! The `pm_analyze::BenchTarget` trait lives in `pm-analyze` (which depends on this crate), so
//! the trait impl for [`FileBenchTarget`] is provided there.

use pm_core::{BlockDevice, Error, Result};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Offset/length/address granularity of direct I/O (covers 512e and 4Kn volumes).
const SECTOR: u64 = 4096;
/// Largest single transfer through the bounce buffer.
const MAX_CHUNK: usize = 8 << 20;

static NEXT_FILE: AtomicU32 = AtomicU32::new(0);

/// Creates uncached scratch files in `dir` (a directory on the volume under test).
#[derive(Clone, Debug)]
pub struct FileBenchTarget {
    dir: PathBuf,
}

impl FileBenchTarget {
    pub fn new(dir: PathBuf) -> Self {
        FileBenchTarget { dir }
    }

    /// Create a scratch file in the directory sized to `len` (rounded up to 4096 bytes) and
    /// return it as a device. The file is already unlinked: it vanishes when the device drops.
    pub fn open_for_bench(&self, len: u64) -> Result<Box<dyn BlockDevice>> {
        let n = NEXT_FILE.fetch_add(1, Ordering::SeqCst);
        let path = self.dir.join(format!("pm-bench-{}-{n}.tmp", std::process::id()));
        let len = len.div_ceil(SECTOR).max(1) * SECTOR;
        let created = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| Error::io(0, 0, e))?;
        let file = open_uncached(created, &path);
        // unlink right away: the open handle keeps the data, nothing is left behind on a crash
        let unlinked = std::fs::remove_file(&path);
        unlinked.map_err(|e| Error::io(0, 0, e))?;
        file.set_len(len).map_err(|e| Error::io(0, 0, e))?;
        Ok(Box::new(BenchFile { file, len, buf: AlignedBuf::default() }))
    }
}

/// Turn `created` (a buffered handle on `path`) into an uncached one where the OS allows it;
/// otherwise keep the buffered handle.
fn open_uncached(created: File, path: &Path) -> File {
    #[cfg(target_os = "linux")]
    {
        match OpenOptions::new().read(true).write(true).custom_flags(libc::O_DIRECT).open(path) {
            Ok(direct) => return direct,
            Err(e) => log::debug!("O_DIRECT is not available for {}: {e}; using buffered I/O", path.display()),
        }
        created
    }
    #[cfg(target_os = "macos")]
    {
        let _ = path;
        if let Err(e) = set_nocache(&created) {
            log::debug!("F_NOCACHE failed: {e}; using cached I/O");
        }
        created
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = path;
        created
    }
}

/// Ask macOS not to keep this file's data in the unified buffer cache.
#[cfg(target_os = "macos")]
fn set_nocache(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: fcntl(F_NOCACHE) takes the descriptor of a live `File` and an int flag by value
    // and touches no caller memory; a failure is reported through the return value, and errno
    // is read immediately by `last_os_error`.
    let r = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
    if r == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Growable byte buffer whose usable slice starts at a [`SECTOR`]-aligned address.
#[derive(Default)]
struct AlignedBuf {
    raw: Vec<u8>,
    start: usize,
}

impl AlignedBuf {
    fn slice(&mut self, n: usize) -> &mut [u8] {
        if self.raw.len() < n + SECTOR as usize {
            self.raw = vec![0u8; n + SECTOR as usize];
            let misalign = self.raw.as_ptr() as usize % SECTOR as usize;
            self.start = (SECTOR as usize - misalign) % SECTOR as usize;
        }
        &mut self.raw[self.start..self.start + n]
    }
}

/// An uncached temp file as a [`BlockDevice`].
pub struct BenchFile {
    file: File,
    len: u64,
    buf: AlignedBuf,
}

fn check_range(offset: u64, len: usize, dev_len: u64) -> Result<()> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= dev_len => Ok(()),
        _ => Err(Error::io(
            offset,
            len,
            io::Error::new(io::ErrorKind::UnexpectedEof, format!("range out of bounds (device length {dev_len})")),
        )),
    }
}

impl BenchFile {
    /// Read the aligned span `[pos, pos + tmp.len())`.
    fn read_span(file: &File, pos: u64, tmp: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < tmp.len() {
            match file.read_at(&mut tmp[done..], pos + done as u64) {
                Ok(0) => return Err(Error::io(pos, tmp.len(), io::Error::from(io::ErrorKind::UnexpectedEof))),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::io(pos, tmp.len(), e)),
            }
        }
        Ok(())
    }

    fn write_span(file: &File, pos: u64, tmp: &[u8]) -> Result<()> {
        let mut done = 0;
        while done < tmp.len() {
            match file.write_at(&tmp[done..], pos + done as u64) {
                Ok(0) => return Err(Error::io(pos, tmp.len(), io::Error::from(io::ErrorKind::WriteZero))),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::io(pos, tmp.len(), e)),
            }
        }
        Ok(())
    }
}

impl BlockDevice for BenchFile {
    fn len(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> u32 {
        SECTOR as u32
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset + buf.len() as u64;
        let mut pos = offset / SECTOR * SECTOR;
        let span_end = end.div_ceil(SECTOR) * SECTOR;
        while pos < span_end {
            let n = (span_end - pos).min(MAX_CHUNK as u64) as usize;
            let tmp = self.buf.slice(n);
            Self::read_span(&self.file, pos, tmp)?;
            let lo = pos.max(offset);
            let hi = (pos + n as u64).min(end);
            buf[(lo - offset) as usize..(hi - offset) as usize]
                .copy_from_slice(&tmp[(lo - pos) as usize..(hi - pos) as usize]);
            pos += n as u64;
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset + buf.len() as u64;
        let mut pos = offset / SECTOR * SECTOR;
        let span_end = end.div_ceil(SECTOR) * SECTOR;
        while pos < span_end {
            let n = (span_end - pos).min(MAX_CHUNK as u64) as usize;
            let tmp = self.buf.slice(n);
            let lo = pos.max(offset);
            let hi = (pos + n as u64).min(end);
            if lo != pos || hi != pos + n as u64 {
                // partially covered edge sector(s): read-modify-write
                Self::read_span(&self.file, pos, tmp)?;
            }
            tmp[(lo - pos) as usize..(hi - pos) as usize]
                .copy_from_slice(&buf[(lo - offset) as usize..(hi - offset) as usize]);
            Self::write_span(&self.file, pos, tmp)?;
            pos += n as u64;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.file.sync_data().map_err(|e| Error::io(0, 0, e))
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pm-bench-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn uncached_file_roundtrip_aligned_and_unaligned() {
        let dir = scratch_dir("roundtrip");
        let target = FileBenchTarget::new(dir.clone());
        let mut dev = target.open_for_bench(100_000).unwrap();
        assert_eq!(dev.len(), 102_400); // rounded up to 4096
        assert_eq!(dev.sector_size(), 4096);
        // the file is unlinked while open
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let data: Vec<u8> = (0..8192u32).map(|i| (i % 253) as u8).collect();
        dev.write_at(4096, &data).unwrap();
        let mut back = vec![0u8; 8192];
        dev.read_at(4096, &mut back).unwrap();
        assert_eq!(back, data);
        // unaligned inside the written area: read-modify-write keeps the neighbours
        dev.write_at(5000, b"hello").unwrap();
        let mut b = vec![0u8; 8192];
        dev.read_at(4096, &mut b).unwrap();
        assert_eq!(&b[904..909], b"hello");
        assert_eq!(&b[..904], &data[..904]);
        assert_eq!(&b[909..], &data[909..]);
        // bounds
        assert!(dev.read_at(102_400 - 10, &mut [0u8; 20]).is_err());
        assert!(dev.write_at(102_400, b"x").is_err());
        dev.flush().unwrap();
        drop(dev);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn each_open_is_an_independent_file() {
        let dir = scratch_dir("distinct");
        let t = FileBenchTarget::new(dir.clone());
        let mut a = t.open_for_bench(4096).unwrap();
        let mut b = t.open_for_bench(4096).unwrap();
        a.write_at(0, &[1u8; 4096]).unwrap();
        b.write_at(0, &[2u8; 4096]).unwrap();
        let mut x = [0u8; 4096];
        a.read_at(0, &mut x).unwrap();
        assert_eq!(x[0], 1);
        b.read_at(0, &mut x).unwrap();
        assert_eq!(x[0], 2);
        drop((a, b));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn missing_directory_is_an_error() {
        let t = FileBenchTarget::new(scratch_dir("missing").join("does-not-exist"));
        assert!(t.open_for_bench(4096).is_err());
    }
}
