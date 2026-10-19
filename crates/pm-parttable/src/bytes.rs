//! Little-endian helpers for reading and writing fixed-offset on-disk fields.
//!
//! All functions panic if `off + width` exceeds the buffer; callers hold sector-sized buffers.

pub fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}

pub fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

pub fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

pub fn wr_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

pub fn wr_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn wr_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_roundtrip() {
        let mut b = [0u8; 16];
        wr_u16(&mut b, 1, 0xBEEF);
        wr_u32(&mut b, 4, 0xDEAD_BEEF);
        wr_u64(&mut b, 8, 0x0102_0304_0506_0708);
        assert_eq!(b[1], 0xEF);
        assert_eq!(b[2], 0xBE);
        assert_eq!(rd_u16(&b, 1), 0xBEEF);
        assert_eq!(rd_u32(&b, 4), 0xDEAD_BEEF);
        assert_eq!(rd_u64(&b, 8), 0x0102_0304_0506_0708);
        assert_eq!(b[8], 0x08);
    }
}
