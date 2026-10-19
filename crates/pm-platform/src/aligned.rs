//! Byte-granular reads/writes on top of a device that only accepts sector-aligned I/O.
//!
//! Raw disk handles (`\\.\PhysicalDriveN`, `\\.\X:`) reject I/O whose offset or length is not a
//! multiple of the sector size (`ERROR_INVALID_PARAMETER`). [`read_unaligned`] and
//! [`write_unaligned`] bounce such requests through a sector-aligned buffer, doing
//! read-modify-write of the edge sectors for writes.

use pm_core::{Error, Result};
use std::io;

/// Largest single aligned transfer issued to the device.
pub const MAX_CHUNK: usize = 8 << 20;
/// Memory alignment of the bounce buffer (covers 512e and 4Kn devices).
const BUF_ALIGN: usize = 4096;
/// `ERROR_INVALID_PARAMETER`, returned by Windows for misaligned raw I/O.
const ERROR_INVALID_PARAMETER: i32 = 87;

/// A device whose primitive I/O must be sector-aligned in offset and length.
pub trait RawSectorIo {
    fn sector_size(&self) -> u32;
    fn len(&self) -> u64;
    /// `offset` and `buf.len()` are multiples of [`RawSectorIo::sector_size`].
    fn read_aligned(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
    /// `offset` and `buf.len()` are multiples of [`RawSectorIo::sector_size`].
    fn write_aligned(&mut self, offset: u64, buf: &[u8]) -> Result<()>;
}

/// Heap buffer whose usable slice starts at a [`BUF_ALIGN`]-aligned address.
struct AlignedBuf {
    raw: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let raw = vec![0u8; len + BUF_ALIGN];
        let misalign = raw.as_ptr() as usize % BUF_ALIGN;
        let start = (BUF_ALIGN - misalign) % BUF_ALIGN;
        AlignedBuf { raw, start, len }
    }
    fn slice(&mut self, n: usize) -> &mut [u8] {
        debug_assert!(n <= self.len);
        &mut self.raw[self.start..self.start + n]
    }
}

fn check_range(offset: u64, len: usize, dev_len: u64) -> Result<()> {
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

fn is_invalid_param(e: &Error) -> bool {
    matches!(e, Error::Io { source, .. } if source.raw_os_error() == Some(ERROR_INVALID_PARAMETER))
}

/// Chunk size: [`MAX_CHUNK`] rounded down to a whole number of sectors (at least one sector).
fn chunk_size(ss: u64) -> u64 {
    ((MAX_CHUNK as u64 / ss) * ss).max(ss)
}

/// Read `buf.len()` bytes at any `offset`.
pub fn read_unaligned<R: RawSectorIo + ?Sized>(dev: &mut R, offset: u64, buf: &mut [u8]) -> Result<()> {
    check_range(offset, buf.len(), dev.len())?;
    if buf.is_empty() {
        return Ok(());
    }
    let ss = dev.sector_size().max(1) as u64;
    if offset % ss == 0 && buf.len() as u64 % ss == 0 {
        match read_direct(dev, offset, buf, ss) {
            Err(e) if is_invalid_param(&e) => {} // e.g. buffer address alignment: bounce below
            other => return other,
        }
    }
    read_bounced(dev, offset, buf, ss)
}

fn read_direct<R: RawSectorIo + ?Sized>(dev: &mut R, offset: u64, buf: &mut [u8], ss: u64) -> Result<()> {
    let chunk = chunk_size(ss) as usize;
    for (i, piece) in buf.chunks_mut(chunk).enumerate() {
        dev.read_aligned(offset + (i * chunk) as u64, piece)?;
    }
    Ok(())
}

fn read_bounced<R: RawSectorIo + ?Sized>(dev: &mut R, offset: u64, buf: &mut [u8], ss: u64) -> Result<()> {
    let end = offset + buf.len() as u64;
    let span_start = offset / ss * ss;
    let span_end = end.div_ceil(ss) * ss;
    let chunk = chunk_size(ss);
    let mut bounce = AlignedBuf::new(chunk.min(span_end - span_start) as usize);
    let mut pos = span_start;
    while pos < span_end {
        let n = chunk.min(span_end - pos);
        let tmp = bounce.slice(n as usize);
        dev.read_aligned(pos, tmp)?;
        let lo = pos.max(offset);
        let hi = (pos + n).min(end);
        buf[(lo - offset) as usize..(hi - offset) as usize]
            .copy_from_slice(&tmp[(lo - pos) as usize..(hi - pos) as usize]);
        pos += n;
    }
    Ok(())
}

/// Write all of `buf` at any `offset`; partially covered edge sectors are read first.
pub fn write_unaligned<R: RawSectorIo + ?Sized>(dev: &mut R, offset: u64, buf: &[u8]) -> Result<()> {
    check_range(offset, buf.len(), dev.len())?;
    if buf.is_empty() {
        return Ok(());
    }
    let ss = dev.sector_size().max(1) as u64;
    if offset % ss == 0 && buf.len() as u64 % ss == 0 {
        match write_direct(dev, offset, buf, ss) {
            Err(e) if is_invalid_param(&e) => {}
            other => return other,
        }
    }
    write_bounced(dev, offset, buf, ss)
}

fn write_direct<R: RawSectorIo + ?Sized>(dev: &mut R, offset: u64, buf: &[u8], ss: u64) -> Result<()> {
    let chunk = chunk_size(ss) as usize;
    for (i, piece) in buf.chunks(chunk).enumerate() {
        dev.write_aligned(offset + (i * chunk) as u64, piece)?;
    }
    Ok(())
}

fn write_bounced<R: RawSectorIo + ?Sized>(dev: &mut R, offset: u64, buf: &[u8], ss: u64) -> Result<()> {
    let end = offset + buf.len() as u64;
    let span_start = offset / ss * ss;
    let span_end = end.div_ceil(ss) * ss;
    let chunk = chunk_size(ss);
    let mut bounce = AlignedBuf::new(chunk.min(span_end - span_start) as usize);
    let mut pos = span_start;
    while pos < span_end {
        let n = chunk.min(span_end - pos);
        let tmp = bounce.slice(n as usize);
        let lo = pos.max(offset);
        let hi = (pos + n).min(end);
        // read-modify-write of the partially covered first / last sector of this piece
        if lo > pos {
            dev.read_aligned(pos, &mut tmp[..ss as usize])?;
        }
        if hi < pos + n {
            let last = n - ss;
            if !(lo > pos && last == 0) {
                dev.read_aligned(pos + last, &mut tmp[last as usize..])?;
            }
        }
        tmp[(lo - pos) as usize..(hi - pos) as usize]
            .copy_from_slice(&buf[(lo - offset) as usize..(hi - offset) as usize]);
        dev.write_aligned(pos, tmp)?;
        pos += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory device that, like a raw disk, rejects misaligned I/O with error 87.
    struct Strict {
        data: Vec<u8>,
        ss: u32,
        /// Also require the buffer address to be sector-aligned.
        strict_buffers: bool,
        ops: Vec<(char, u64, usize)>,
    }

    impl Strict {
        fn new(len: usize, ss: u32) -> Self {
            Strict {
                data: (0..len).map(|i| (i % 251) as u8).collect(),
                ss,
                strict_buffers: false,
                ops: Vec::new(),
            }
        }
        fn check(&self, offset: u64, buf: *const u8, len: usize) -> Result<()> {
            let ss = self.ss as u64;
            let misaligned = offset % ss != 0
                || len as u64 % ss != 0
                || (self.strict_buffers && buf as usize % self.ss as usize != 0);
            if misaligned {
                return Err(Error::io(offset, len, io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER)));
            }
            check_range(offset, len, self.data.len() as u64)
        }
    }

    impl RawSectorIo for Strict {
        fn sector_size(&self) -> u32 {
            self.ss
        }
        fn len(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_aligned(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.check(offset, buf.as_ptr(), buf.len())?;
            self.ops.push(('r', offset, buf.len()));
            let o = offset as usize;
            buf.copy_from_slice(&self.data[o..o + buf.len()]);
            Ok(())
        }
        fn write_aligned(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
            self.check(offset, buf.as_ptr(), buf.len())?;
            self.ops.push(('w', offset, buf.len()));
            let o = offset as usize;
            self.data[o..o + buf.len()].copy_from_slice(buf);
            Ok(())
        }
    }

    #[test]
    fn unaligned_read_returns_exact_bytes() {
        let mut d = Strict::new(8192, 512);
        let expect = d.data.clone();
        let mut b = vec![0u8; 1000];
        read_unaligned(&mut d, 300, &mut b).unwrap();
        assert_eq!(b, expect[300..1300]);
        // covers sectors 0..3 in one bounced read
        assert_eq!(d.ops, vec![('r', 0, 1536)]);
        let mut one = [0u8; 1];
        read_unaligned(&mut d, 8191, &mut one).unwrap();
        assert_eq!(one[0], expect[8191]);
    }

    #[test]
    fn aligned_read_goes_direct() {
        let mut d = Strict::new(8192, 512);
        let mut b = vec![0u8; 1024];
        read_unaligned(&mut d, 512, &mut b).unwrap();
        assert_eq!(b, d.data[512..1536]);
        assert_eq!(d.ops, vec![('r', 512, 1024)]);
    }

    #[test]
    fn unaligned_write_preserves_neighbours() {
        let mut d = Strict::new(8192, 512);
        let mut expect = d.data.clone();
        let payload = vec![0xEEu8; 700];
        write_unaligned(&mut d, 100, &payload).unwrap();
        expect[100..800].copy_from_slice(&payload);
        assert_eq!(d.data, expect);
        // inside one sector
        write_unaligned(&mut d, 2050, b"xy").unwrap();
        expect[2050..2052].copy_from_slice(b"xy");
        assert_eq!(d.data, expect);
        // starts aligned, ends unaligned
        write_unaligned(&mut d, 4096, &[7u8; 513]).unwrap();
        expect[4096..4609].copy_from_slice(&[7u8; 513]);
        assert_eq!(d.data, expect);
        // 4Kn device
        let mut d = Strict::new(32768, 4096);
        let mut expect = d.data.clone();
        write_unaligned(&mut d, 4000, &[1u8; 200]).unwrap();
        expect[4000..4200].copy_from_slice(&[1u8; 200]);
        assert_eq!(d.data, expect);
    }

    #[test]
    fn aligned_write_goes_direct() {
        let mut d = Strict::new(8192, 512);
        write_unaligned(&mut d, 1024, &[9u8; 512]).unwrap();
        assert_eq!(d.ops, vec![('w', 1024, 512)]);
        assert!(d.data[1024..1536].iter().all(|&b| b == 9));
    }

    #[test]
    fn large_unaligned_io_spans_chunks() {
        let len = MAX_CHUNK * 2 + 4096;
        let mut d = Strict::new(len, 512);
        let mut expect = d.data.clone();
        let payload: Vec<u8> = (0..MAX_CHUNK + 1000).map(|i| (i % 7) as u8).collect();
        write_unaligned(&mut d, 777, &payload).unwrap();
        expect[777..777 + payload.len()].copy_from_slice(&payload);
        assert_eq!(d.data, expect);
        let mut back = vec![0u8; payload.len()];
        read_unaligned(&mut d, 777, &mut back).unwrap();
        assert_eq!(back, payload);
        assert!(d.ops.iter().all(|&(_, _, n)| n <= MAX_CHUNK));
    }

    #[test]
    fn misaligned_buffer_address_falls_back_to_bounce() {
        let mut d = Strict::new(8192, 512);
        d.strict_buffers = true;
        let expect = d.data.clone();
        let mut backing = vec![0u8; 1024 + 1];
        // force a buffer address that is not 512-aligned
        let off = if backing.as_ptr() as usize % 512 == 0 { 1 } else { 0 };
        let b = &mut backing[off..off + 1024];
        read_unaligned(&mut d, 1024, b).unwrap();
        assert_eq!(b, &expect[1024..2048]);
        write_unaligned(&mut d, 0, &backing[off..off + 512]).unwrap();
        assert_eq!(&d.data[..512], &expect[1024..1536]);
    }

    #[test]
    fn out_of_range_and_empty() {
        let mut d = Strict::new(4096, 512);
        assert!(read_unaligned(&mut d, 4000, &mut [0u8; 200]).is_err());
        assert!(write_unaligned(&mut d, u64::MAX, &[0u8; 2]).is_err());
        read_unaligned(&mut d, 4096, &mut []).unwrap();
        write_unaligned(&mut d, 10, &[]).unwrap();
        assert!(d.ops.is_empty());
    }
}
