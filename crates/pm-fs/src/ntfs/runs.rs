//! Data run (mapping pairs) decoding.

use pm_core::{Error, Result};

/// Little-endian two's-complement integer of 1..=8 bytes.
fn le_signed(b: &[u8]) -> i64 {
    let mut v = 0i64;
    for (i, &x) in b.iter().enumerate() {
        v |= (x as i64) << (8 * i);
    }
    if b.len() < 8 && b.last().is_some_and(|&x| x & 0x80 != 0) {
        v |= -1i64 << (8 * b.len());
    }
    v
}

/// Decode a runlist into `(lcn, clusters)` pairs; `lcn` is `None` for sparse runs.
///
/// Each run starts with a header byte whose low nibble is the size of the length field and
/// whose high nibble is the size of the offset field (0 = sparse). Offsets are signed and
/// relative to the previous allocated run's LCN. A zero header byte or the end of `data`
/// terminates the list.
pub fn decode_runs(data: &[u8]) -> Result<Vec<(Option<u64>, u64)>> {
    let bad = |m: &str| Error::Parse(format!("data run: {m}"));
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut lcn = 0i64;
    while pos < data.len() {
        let h = data[pos];
        if h == 0 {
            break;
        }
        pos += 1;
        let ls = (h & 0x0F) as usize;
        let os = (h >> 4) as usize;
        if ls == 0 || ls > 8 || os > 8 {
            return Err(bad("invalid header"));
        }
        if data.len() - pos < ls + os {
            return Err(bad("truncated"));
        }
        let len = le_signed(&data[pos..pos + ls]);
        pos += ls;
        if len <= 0 {
            return Err(bad("non-positive length"));
        }
        if os == 0 {
            out.push((None, len as u64));
            continue;
        }
        let delta = le_signed(&data[pos..pos + os]);
        pos += os;
        lcn = lcn.checked_add(delta).ok_or_else(|| bad("LCN overflow"))?;
        if lcn < 0 {
            return Err(bad("negative LCN"));
        }
        out.push((Some(lcn as u64), len as u64));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_fields() {
        assert_eq!(le_signed(&[0xF0]), -16);
        assert_eq!(le_signed(&[0x80, 0x00]), 0x80);
        assert_eq!(le_signed(&[0xFF; 8]), -1);
        assert_eq!(le_signed(&[0x01, 0, 0, 0, 0, 0, 0, 0x80]), i64::MIN + 1);
    }

    #[test]
    fn garbage_never_panics() {
        let mut x = 0x1234_5678u32;
        for _ in 0..20_000 {
            let mut b = [0u8; 24];
            for v in b.iter_mut() {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                *v = x as u8;
            }
            let _ = decode_runs(&b);
        }
    }
}
