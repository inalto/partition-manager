//! Byte-size helpers: human formatting, parsing and alignment.

use crate::error::{Error, Result};

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// Format a byte count with binary (1024) steps, e.g. `1536 MiB` -> `"1.50 GB"`.
///
/// Values below 1 KiB print as an integer (`"512 B"`), everything else with two decimals.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // Avoid printing "1024.00 MB" when rounding would carry into the next unit.
    if (value * 100.0).round() / 100.0 >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

/// Parse a human size such as `"512"`, `"100M"`, `"1.5GiB"`, `"2T"`.
///
/// Suffixes K/M/G/T/P (optionally followed by `iB` or `B`) are case-insensitive and binary.
/// A bare `B` suffix means bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, suffix) = t.split_at(split);
    if num.is_empty() {
        return Err(Error::Parse(format!("invalid size: {s:?}")));
    }
    let shift: u32 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        "t" | "tb" | "tib" => 40,
        "p" | "pb" | "pib" => 50,
        _ => return Err(Error::Parse(format!("unknown size suffix in {s:?}"))),
    };
    let mult = 1u64 << shift;
    if num.contains('.') {
        let v: f64 = num
            .parse()
            .map_err(|_| Error::Parse(format!("invalid number in size: {s:?}")))?;
        let bytes = v * mult as f64;
        // 2^64 as f64; anything at or above it overflows u64.
        if !bytes.is_finite() || bytes >= 18_446_744_073_709_551_616.0 {
            return Err(Error::Parse(format!("size out of range: {s:?}")));
        }
        Ok(bytes.round() as u64)
    } else {
        let v: u64 = num
            .parse()
            .map_err(|_| Error::Parse(format!("invalid number in size: {s:?}")))?;
        v.checked_mul(mult)
            .ok_or_else(|| Error::Parse(format!("size out of range: {s:?}")))
    }
}

/// Round `v` up to the next multiple of `a` (`a == 0` returns `v` unchanged).
/// Saturates to `u64::MAX` if the aligned value would overflow.
pub fn align_up(v: u64, a: u64) -> u64 {
    if a == 0 {
        return v;
    }
    v.div_ceil(a).checked_mul(a).unwrap_or(u64::MAX)
}

/// Round `v` down to a multiple of `a` (`a == 0` returns `v` unchanged).
pub fn align_down(v: u64, a: u64) -> u64 {
    if a == 0 {
        return v;
    }
    v / a * a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_matches_screenshots() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1536 * MIB), "1.50 GB");
        // 1015.63 MB as seen in screenshots
        assert_eq!(format_size(1015 * MIB + 660_000), "1015.63 MB");
        assert_eq!(format_size(1000 * GIB + 194_000_000), "1000.18 GB");
        // 2 TB (decimal) drive as shown in disk names
        assert_eq!(format_size(2_000_398_934_016), "1.82 TB");
        assert_eq!(format_size(2 * (1 << 40)), "2.00 TB");
        assert_eq!(format_size(3 * (1 << 50)), "3.00 PB");
        // rounding never prints 1024.00 of a smaller unit
        assert_eq!(format_size(GIB - 1), "1.00 GB");
    }

    #[test]
    fn parse_size_variants() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("100M").unwrap(), 100 * MIB);
        assert_eq!(parse_size("1.5GiB").unwrap(), 3 * GIB / 2);
        assert_eq!(parse_size("1.5G").unwrap(), 3 * GIB / 2);
        assert_eq!(parse_size("2T").unwrap(), 2 << 40);
        assert_eq!(parse_size("  64 kb ").unwrap(), 64 << 10);
        assert_eq!(parse_size("1p").unwrap(), 1 << 50);
        assert_eq!(parse_size("10B").unwrap(), 10);
        assert_eq!(parse_size("0").unwrap(), 0);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("12X").is_err());
        assert!(parse_size("-5M").is_err());
        assert!(parse_size("99999999999P").is_err());
    }

    #[test]
    fn align_up_saturates_on_overflow() {
        assert_eq!(align_up(u64::MAX, 512), u64::MAX);
        assert_eq!(align_up(u64::MAX - 100, 512), u64::MAX);
        assert_eq!(align_up(u64::MAX - 511, 512), u64::MAX - 511);
    }

    #[test]
    fn align_helpers() {
        assert_eq!(align_up(0, 512), 0);
        assert_eq!(align_up(1, 512), 512);
        assert_eq!(align_up(512, 512), 512);
        assert_eq!(align_up(513, 512), 1024);
        assert_eq!(align_down(1023, 512), 512);
        assert_eq!(align_down(512, 512), 512);
        assert_eq!(align_down(5, 512), 0);
        assert_eq!(align_up(10, 0), 10);
        assert_eq!(align_down(10, 0), 10);
    }
}
