//! Little helpers shared by the filesystem parsers.

use pm_core::{BlockDevice, Result};

pub(crate) fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub(crate) fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub(crate) fn le64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

pub(crate) fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub(crate) fn be64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_be_bytes(a)
}

/// Read up to `buf.len()` bytes at `offset`, zero-filling whatever lies beyond the end of
/// the device. Only genuine I/O errors are returned.
pub(crate) fn read_padded(dev: &mut dyn BlockDevice, offset: u64, buf: &mut [u8]) -> Result<()> {
    buf.fill(0);
    let avail = dev.len().saturating_sub(offset).min(buf.len() as u64) as usize;
    if avail > 0 {
        dev.read_at(offset, &mut buf[..avail])?;
    }
    Ok(())
}

/// Text of a fixed-width, space- or NUL-padded field (single-byte code page, mapped as Latin-1).
pub(crate) fn trim_padded_latin1(b: &[u8]) -> String {
    let end = b.iter().rposition(|&c| c != b' ' && c != 0).map_or(0, |i| i + 1);
    b[..end].iter().map(|&c| c as char).collect()
}

/// UTF-8 text of a NUL-terminated field.
pub(crate) fn cstr_utf8(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim_end().to_string()
}

/// 16 raw bytes as a lower-case hyphenated UUID.
pub(crate) fn uuid_string(b: &[u8]) -> String {
    let h: String = b[..16].iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// A 32-bit FAT volume id as `XXXX-XXXX`.
pub(crate) fn volume_id_string(id: u32) -> String {
    format!("{:04X}-{:04X}", id >> 16, id & 0xFFFF)
}
