//! Shared building blocks for the sparse formats (VHD dynamic, VMDK sparse, qcow2):
//! a growable container file with positioned I/O, and a generic "allocate-on-write
//! block map" engine that translates virtual offsets to file offsets.

use pm_core::{BlockDevice, Error, FileDevice, Result};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

/// Error for an access outside `[0, len)`, shaped like `pm_core`'s own bounds errors.
pub(crate) fn check_range(offset: u64, len: usize, dev_len: u64) -> Result<()> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= dev_len => Ok(()),
        _ => Err(Error::io(
            offset,
            len,
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("range out of bounds (device length {dev_len})"),
            ),
        )),
    }
}

pub(crate) fn read_only_error() -> Error {
    Error::Invalid("read-only".into())
}

/// The container file of an image. Wraps [`FileDevice`] for positioned I/O and lets
/// the file grow: the inner device gets an unbounded window and the real file length is
/// tracked here, so writes past the current end extend the file.
pub(crate) struct ImageFile {
    dev: FileDevice,
    len: u64,
}

impl ImageFile {
    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        let dev = FileDevice::open(path, read_only)?;
        let len = dev.len();
        Ok(ImageFile {
            dev: dev.with_len(u64::MAX),
            len,
        })
    }

    /// Create (truncating) an empty file opened read-write.
    pub fn create(path: &Path) -> Result<Self> {
        Ok(ImageFile {
            dev: FileDevice::create(path, 0)?.with_len(u64::MAX),
            len: 0,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_read_only(&self) -> bool {
        self.dev.is_read_only()
    }

    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        self.dev.read_at(offset, buf)
    }

    pub fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| Error::Invalid("image offset overflow".into()))?;
        self.dev.write_at(offset, buf)?;
        self.len = self.len.max(end);
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.dev.flush()
    }
}

/// Sparse virtual-block -> file-offset table. Stored as fixed-size pages in an ordered
/// map; a page is only materialised once one of its entries is set, so memory is
/// proportional to the allocated data, never to the (header-supplied) block count.
pub(crate) struct BlockMap {
    pub block_size: u64,
    count: u64,
    pages: BTreeMap<u64, Box<[u64]>>,
}

const PAGE: u64 = 4096;

impl BlockMap {
    /// `block_size` must be non-zero; callers validate header values before this.
    pub fn new(block_size: u64, count: u64) -> Self {
        assert!(block_size > 0, "block size must be non-zero");
        BlockMap {
            block_size,
            count,
            pages: BTreeMap::new(),
        }
    }

    /// File offset of virtual block `idx`, or `None` when unallocated.
    pub fn get(&self, idx: u64) -> Option<u64> {
        if idx >= self.count {
            return None;
        }
        match self.pages.get(&(idx / PAGE))?[(idx % PAGE) as usize] {
            0 => None,
            off => Some(off),
        }
    }

    /// Record the file offset of block `idx` (offset 0 is reserved for "unallocated";
    /// no format places data at file offset 0).
    pub fn set(&mut self, idx: u64, file_off: u64) {
        assert!(idx < self.count && file_off != 0, "invalid block mapping");
        let page = self
            .pages
            .entry(idx / PAGE)
            .or_insert_with(|| vec![0u64; PAGE as usize].into_boxed_slice());
        page[(idx % PAGE) as usize] = file_off;
    }

    /// Read `buf` at virtual `offset`; unallocated blocks read as zeros.
    pub fn read(&self, file: &mut ImageFile, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let idx = pos / self.block_size;
            let within = pos % self.block_size;
            let n = ((self.block_size - within) as usize).min(buf.len() - done);
            match self.get(idx) {
                Some(f) => file.read_at(f + within, &mut buf[done..done + n])?,
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }

    /// Write `buf` at virtual `offset`. Unallocated blocks are allocated through `alloc`:
    /// it chooses the file offset, the whole block (zero padded) is written there, then
    /// `alloc.on_allocate` persists the mapping.
    pub fn write<A: BlockAllocator>(&mut self, alloc: &mut A, offset: u64, buf: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let idx = pos / self.block_size;
            let within = pos % self.block_size;
            let n = ((self.block_size - within) as usize).min(buf.len() - done);
            let chunk = &buf[done..done + n];
            match self.get(idx) {
                Some(f) => alloc.file().write_at(f + within, chunk)?,
                None => {
                    let f = alloc.place_block(idx)?;
                    let mut block = vec![0u8; self.block_size as usize];
                    block[within as usize..within as usize + n].copy_from_slice(chunk);
                    alloc.file().write_at(f, &block)?;
                    alloc.on_allocate(idx, f)?;
                    self.set(idx, f);
                }
            }
            done += n;
        }
        Ok(())
    }
}

/// Format-specific half of the allocate-on-write engine.
pub(crate) trait BlockAllocator {
    fn file(&mut self) -> &mut ImageFile;
    fn is_read_only(&self) -> bool;
    /// Reserve space for a new data block and return its file offset (anything the
    /// format stores in front of the block, e.g. a VHD sector bitmap, is written here).
    fn place_block(&mut self, idx: u64) -> Result<u64>;
    /// Persist the mapping `idx -> file_off` in the format's own tables.
    fn on_allocate(&mut self, idx: u64, file_off: u64) -> Result<()>;
}

/// A sparse image as a [`BlockDevice`]: virtual capacity, block map and the format's
/// allocator (which owns the container file).
pub(crate) struct SparseDevice<A: BlockAllocator> {
    pub capacity: u64,
    pub map: BlockMap,
    pub alloc: A,
}

impl<A: BlockAllocator + Send> BlockDevice for SparseDevice<A> {
    fn len(&self) -> u64 {
        self.capacity
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.capacity)?;
        self.map.read(self.alloc.file(), offset, buf)
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.alloc.file().is_read_only() {
            return Err(read_only_error());
        }
        check_range(offset, buf.len(), self.capacity)?;
        self.map.write(&mut self.alloc, offset, buf)
    }
    fn flush(&mut self) -> Result<()> {
        self.alloc.file().flush()
    }
    fn is_read_only(&self) -> bool {
        self.alloc.is_read_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Appender {
        file: ImageFile,
        log: Vec<(u64, u64)>,
    }

    impl BlockAllocator for Appender {
        fn file(&mut self) -> &mut ImageFile {
            &mut self.file
        }
        fn is_read_only(&self) -> bool {
            false
        }
        fn place_block(&mut self, _idx: u64) -> Result<u64> {
            Ok(self.file.len().max(512))
        }
        fn on_allocate(&mut self, idx: u64, file_off: u64) -> Result<()> {
            self.log.push((idx, file_off));
            Ok(())
        }
    }

    #[test]
    fn block_map_allocates_on_write_and_reads_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Appender {
            file: ImageFile::create(&dir.path().join("f")).unwrap(),
            log: vec![],
        };
        let mut m = BlockMap::new(1024, 10_000);
        assert_eq!(m.get(9_999), None);
        assert_eq!(m.get(10_000), None);
        m.write(&mut a, 1000, &[7u8; 100]).unwrap(); // spans blocks 0 and 1
        assert_eq!(a.log, vec![(0, 512), (1, 1536)]);
        assert_eq!(a.file.len(), 2560);
        m.write(&mut a, 1020, &[9u8; 2]).unwrap(); // already mapped
        assert_eq!(a.log.len(), 2);
        let mut b = vec![1u8; 3000];
        m.read(&mut a.file, 0, &mut b).unwrap();
        assert!(b[..1000].iter().all(|&x| x == 0));
        assert!(b[1000..1020].iter().all(|&x| x == 7));
        assert_eq!(&b[1020..1022], &[9, 9]);
        assert!(b[1022..1100].iter().all(|&x| x == 7));
        assert!(b[1100..].iter().all(|&x| x == 0));
        m.set(8_000, 4096);
        assert_eq!(m.get(8_000), Some(4096));
    }
}
