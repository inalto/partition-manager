//! Bitmap of used regions of a filesystem, used to copy only the occupied clusters.

/// One bit per `unit` bytes of a filesystem of `len` bytes; bit `i` of byte `i / 8`
/// (least significant bit first) describes the unit starting at byte `i * unit`.
#[derive(Clone, Debug)]
pub struct UsedMap {
    /// Bytes per bit.
    pub unit: u32,
    pub bits: Vec<u8>,
    /// Filesystem size in bytes.
    pub len: u64,
}

impl UsedMap {
    /// An empty (all free) map. A zero `unit` is treated as 1.
    pub fn new(len: u64, unit: u32) -> UsedMap {
        let unit = unit.max(1);
        let units = len.div_ceil(unit as u64);
        UsedMap { unit, bits: vec![0u8; units.div_ceil(8) as usize], len }
    }

    /// A map with every byte of `len` marked used (4 KiB units).
    pub fn all_used(len: u64) -> UsedMap {
        let mut m = UsedMap::new(len, 4096);
        m.set(0, len);
        m
    }

    fn units(&self) -> u64 {
        self.len.div_ceil(self.unit as u64)
    }

    fn bit(&self, unit_idx: u64) -> bool {
        (self.bits[(unit_idx / 8) as usize] >> (unit_idx % 8)) & 1 != 0
    }

    /// Mark every unit touched by `[byte_off, byte_off + byte_len)` as used.
    /// The range is clipped to `len`.
    pub fn set(&mut self, byte_off: u64, byte_len: u64) {
        if byte_len == 0 || byte_off >= self.len {
            return;
        }
        let end = byte_off.saturating_add(byte_len).min(self.len);
        let first = byte_off / self.unit as u64;
        let last = (end - 1) / self.unit as u64;
        for i in first..=last {
            self.bits[(i / 8) as usize] |= 1 << (i % 8);
        }
    }

    /// Whether the unit containing `byte_off` is used; offsets past `len` are not.
    pub fn is_used(&self, byte_off: u64) -> bool {
        byte_off < self.len && self.bit(byte_off / self.unit as u64)
    }

    /// Total bytes covered by used units (the last unit is clipped to `len`).
    pub fn used_bytes(&self) -> u64 {
        self.ranges().map(|(_, l)| l).sum()
    }

    /// Coalesced used ranges as `(start_byte, len_bytes)`, in ascending order, clipped to `len`.
    pub fn ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        Ranges { map: self, pos: 0, units: self.units() }
    }
}

struct Ranges<'a> {
    map: &'a UsedMap,
    pos: u64,
    units: u64,
}

impl Iterator for Ranges<'_> {
    type Item = (u64, u64);

    fn next(&mut self) -> Option<(u64, u64)> {
        let m = self.map;
        // skip free units, a whole zero byte at a time when aligned
        while self.pos < self.units && !m.bit(self.pos) {
            if self.pos % 8 == 0 && m.bits[(self.pos / 8) as usize] == 0 {
                self.pos += 8;
            } else {
                self.pos += 1;
            }
        }
        if self.pos >= self.units {
            return None;
        }
        let start = self.pos;
        while self.pos < self.units && m.bit(self.pos) {
            if self.pos % 8 == 0 && m.bits[(self.pos / 8) as usize] == 0xFF {
                self.pos += 8;
            } else {
                self.pos += 1;
            }
        }
        let end = self.pos.min(self.units);
        self.pos = end;
        let unit = m.unit as u64;
        let start_byte = start * unit;
        let end_byte = (end * unit).min(m.len);
        Some((start_byte, end_byte - start_byte))
    }
}
