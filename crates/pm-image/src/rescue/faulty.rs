//! [`FaultyDevice`]: an in-memory device that injects read errors, for tests.

use pm_core::{BlockDevice, Error, MemDevice, Result};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const SECTOR: u64 = 512;

/// Wraps a [`MemDevice`] and fails every `read_at` whose range touches a bad 512-byte
/// sector, as a whole (like a real drive failing a multi-sector read). Transient sectors
/// fail a given number of times and then read normally. Writes pass through.
pub struct FaultyDevice {
    inner: MemDevice,
    bad: BTreeSet<u64>,
    transient: HashMap<u64, u32>,
    reads: Arc<AtomicU64>,
    writes: Arc<AtomicU64>,
    log: Arc<Mutex<Vec<(u64, usize)>>>,
}

impl FaultyDevice {
    /// `bad`: byte offsets of permanently bad sectors; `transient`: byte offset of a sector
    /// -> number of reads touching it that fail before it starts reading fine. Offsets are
    /// rounded down to a 512-byte boundary.
    pub fn new(inner: MemDevice, bad: Vec<u64>, transient: HashMap<u64, u32>) -> Self {
        FaultyDevice {
            inner,
            bad: bad.into_iter().map(|o| o / SECTOR * SECTOR).collect(),
            transient: transient
                .into_iter()
                .map(|(o, n)| (o / SECTOR * SECTOR, n))
                .collect(),
            reads: Arc::new(AtomicU64::new(0)),
            writes: Arc::new(AtomicU64::new(0)),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Number of `read_at` calls so far (successful or not).
    pub fn read_count(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }

    /// Shared handle to the read counter.
    pub fn reads(&self) -> Arc<AtomicU64> {
        self.reads.clone()
    }

    /// Number of `write_at` calls so far.
    pub fn write_count(&self) -> u64 {
        self.writes.load(Ordering::SeqCst)
    }

    /// Every `read_at` call as `(offset, len)`, in call order.
    pub fn read_log(&self) -> Vec<(u64, usize)> {
        self.log.lock().unwrap().clone()
    }

    pub fn inner(&self) -> &MemDevice {
        &self.inner
    }

    pub fn into_inner(self) -> MemDevice {
        self.inner
    }

    /// Whether a read of `[offset, offset + len)` fails; consumes transient failures.
    fn fails(&mut self, offset: u64, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let first = offset / SECTOR * SECTOR;
        let end = offset.saturating_add(len as u64);
        let mut fail = self.bad.range(first..end).next().is_some();
        for (&sec, left) in self.transient.iter_mut() {
            if sec >= first && sec < end && *left > 0 {
                *left -= 1;
                fail = true;
            }
        }
        fail
    }
}

impl BlockDevice for FaultyDevice {
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn sector_size(&self) -> u32 {
        SECTOR as u32
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap().push((offset, buf.len()));
        if self.fails(offset, buf.len()) {
            return Err(Error::io(
                offset,
                buf.len(),
                io::Error::new(io::ErrorKind::Other, "simulated bad sector"),
            ));
        }
        self.inner.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write_at(offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faulty_fails_whole_reads_touching_bad_sectors() {
        let mut t = HashMap::new();
        t.insert(4096 + 100, 2);
        let mut d = FaultyDevice::new(MemDevice::new(8192), vec![1024], t);
        let mut b = vec![0u8; 2048];
        assert!(matches!(d.read_at(0, &mut b), Err(Error::Io { offset: 0, len: 2048, .. })));
        assert!(d.read_at(1536, &mut b[..512]).is_ok());
        assert!(d.read_at(1023, &mut b[..1]).is_ok());
        assert!(d.read_at(1024, &mut b[..1]).is_err());
        // transient sector at 4096: fails twice, then fine
        assert!(d.read_at(4096, &mut b[..512]).is_err());
        assert!(d.read_at(3584, &mut b[..1024]).is_err());
        assert!(d.read_at(4096, &mut b[..512]).is_ok());
        assert_eq!(d.read_count(), 7);
        assert_eq!(d.read_log()[0], (0, 2048));
        d.write_at(1024, b"x").unwrap();
        assert_eq!(d.write_count(), 1);
        assert_eq!(d.inner().as_slice()[1024], b'x');
    }
}
