//! Benchmark scratch files: unbuffered, write-through temp files that delete themselves.
//!
//! [`FileBenchTarget`] hands out [`BenchFile`] devices. They are opened with
//! `FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH | FILE_FLAG_DELETE_ON_CLOSE`, so the numbers
//! measure the volume rather than the file cache, and the file disappears when the device is
//! dropped (even if the process dies). Unbuffered I/O needs sector-aligned offsets, lengths *and*
//! memory addresses; `BenchFile` therefore copies through a page-aligned buffer, and handles
//! unaligned requests with read-modify-write. The benchmark itself uses multiples of 4096, which
//! take the fast path.
//!
//! The file is only sized, not pre-filled: its valid data length starts at 0 and NTFS zero-fills
//! lazily, so `pm_analyze::benchmark` writes the whole file once, untimed, before measuring.
//!
//! The `pm_analyze::BenchTarget` trait lives in `pm-analyze` (which depends on this crate), so
//! the trait impl for [`FileBenchTarget`] is provided there.

use pm_core::{BlockDevice, Error, Result};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH,
};

/// Offset/length/address granularity of unbuffered I/O (covers 512e and 4Kn volumes).
const SECTOR: u64 = 4096;
/// Largest single transfer through the bounce buffer.
const MAX_CHUNK: usize = 8 << 20;

static NEXT_FILE: AtomicU32 = AtomicU32::new(0);

/// Creates unbuffered scratch files in `dir` (a directory on the volume under test).
#[derive(Clone, Debug)]
pub struct FileBenchTarget {
    dir: PathBuf,
}

impl FileBenchTarget {
    pub fn new(dir: PathBuf) -> Self {
        FileBenchTarget { dir }
    }

    /// Create `<dir>/pm-bench-<pid>-<n>.tmp` sized to `len` (rounded up to 4096 bytes) and
    /// return it as a device. The file is deleted when the device is dropped.
    pub fn open_for_bench(&self, len: u64) -> Result<Box<dyn BlockDevice>> {
        let n = NEXT_FILE.fetch_add(1, Ordering::SeqCst);
        let path = self.dir.join(format!("pm-bench-{}-{n}.tmp", std::process::id()));
        let len = len.div_ceil(SECTOR).max(1) * SECTOR;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH | FILE_FLAG_DELETE_ON_CLOSE)
            .open(&path)
            .map_err(|e| Error::io(0, 0, e))?;
        file.set_len(len).map_err(|e| Error::io(0, 0, e))?;
        Ok(Box::new(BenchFile { file, len, buf: AlignedBuf::default() }))
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

/// An unbuffered temp file as a [`BlockDevice`].
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
            match file.seek_read(&mut tmp[done..], pos + done as u64) {
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
            match file.seek_write(&tmp[done..], pos + done as u64) {
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
        // FILE_FLAG_WRITE_THROUGH: every write is already on stable storage when it returns.
        Ok(())
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
    fn unbuffered_file_roundtrip_aligned_and_unaligned() {
        let dir = scratch_dir("roundtrip");
        let target = FileBenchTarget::new(dir.clone());
        let mut dev = target.open_for_bench(100_000).unwrap();
        assert_eq!(dev.len(), 102_400); // rounded up to 4096
        assert_eq!(dev.sector_size(), 4096);
        // aligned
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
        // the temp file exists while open and is deleted on drop
        let count = |d: &PathBuf| std::fs::read_dir(d).unwrap().count();
        assert_eq!(count(&dir), 1);
        drop(dev);
        assert_eq!(count(&dir), 0);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn each_open_creates_a_distinct_file() {
        let dir = scratch_dir("distinct");
        std::fs::create_dir_all(&dir).unwrap();
        let t = FileBenchTarget::new(dir.clone());
        let a = t.open_for_bench(4096).unwrap();
        let b = t.open_for_bench(4096).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        drop((a, b));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn missing_directory_is_an_error() {
        let t = FileBenchTarget::new(scratch_dir("missing").join("does-not-exist"));
        assert!(t.open_for_bench(4096).is_err());
    }
}
