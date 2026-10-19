//! MFT records: update-sequence fixups, attribute headers, `$FILE_NAME`,
//! `$STANDARD_INFORMATION` and `$ATTRIBUTE_LIST` parsing.

use super::runs::decode_runs;
use crate::util::{le16, le32, le64};
use pm_core::{Error, Result};

pub const AT_STANDARD_INFORMATION: u32 = 0x10;
pub const AT_ATTRIBUTE_LIST: u32 = 0x20;
pub const AT_FILE_NAME: u32 = 0x30;
pub const AT_VOLUME_NAME: u32 = 0x60;
pub const AT_DATA: u32 = 0x80;
pub const AT_INDEX_ROOT: u32 = 0x90;
pub const AT_INDEX_ALLOCATION: u32 = 0xA0;
pub const AT_END: u32 = 0xFFFF_FFFF;

/// Attribute header flags.
pub const ATTR_COMPRESSION_MASK: u16 = 0x00FF;
pub const ATTR_ENCRYPTED: u16 = 0x4000;

/// `$FILE_NAME` flag of directories.
pub const FILE_NAME_DIRECTORY: u32 = 0x1000_0000;
pub const NS_DOS: u8 = 2;
/// The record-number part of a file reference (the top 16 bits are the sequence number).
pub const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Fixups protect every 512-byte stride regardless of the device sector size.
const STRIDE: usize = 512;

fn parse_err(m: impl Into<String>) -> Error {
    Error::Parse(m.into())
}

/// `len` bytes of `b` at `off`, or a parse error when out of bounds.
pub(crate) fn sub(b: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    off.checked_add(len)
        .filter(|&end| end <= b.len())
        .map(|end| &b[off..end])
        .ok_or_else(|| parse_err(format!("field at {off}+{len} outside a {}-byte structure", b.len())))
}

pub(crate) fn utf16(b: &[u8]) -> String {
    let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&u)
}

/// NTFS FILETIME (100 ns ticks since 1601) to unix seconds; 0 means "not set".
pub fn filetime_to_unix(ft: u64) -> Option<i64> {
    (ft != 0).then(|| (ft / 10_000_000) as i64 - 11_644_473_600)
}

/// Verify the signature and undo the update-sequence fixups of a multi-sector structure
/// (MFT record or INDX block) in place.
pub fn apply_fixups(buf: &mut [u8], sig: &[u8; 4]) -> Result<()> {
    if buf.len() < STRIDE || &buf[..4] != sig {
        return Err(parse_err(format!("missing {} signature", String::from_utf8_lossy(sig))));
    }
    let usa_off = le16(buf, 4) as usize;
    let usa_count = le16(buf, 6) as usize;
    let sectors = buf.len() / STRIDE;
    if usa_count != sectors + 1 || usa_off < 8 || usa_off + 2 * usa_count > buf.len() {
        return Err(parse_err("bad update sequence array"));
    }
    let usn = [buf[usa_off], buf[usa_off + 1]];
    for i in 0..sectors {
        let end = i * STRIDE + STRIDE - 2;
        if buf[end..end + 2] != usn {
            return Err(parse_err("bad fixup"));
        }
        let v = usa_off + 2 + 2 * i;
        let orig = [buf[v], buf[v + 1]];
        buf[end..end + 2].copy_from_slice(&orig);
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct NonResident {
    pub start_vcn: u64,
    pub end_vcn: u64,
    pub runs: Vec<(Option<u64>, u64)>,
    pub comp_unit: u16,
    pub alloc: u64,
    pub real: u64,
    pub init: u64,
}

#[derive(Clone, Debug)]
pub enum AttrBody {
    Resident(Vec<u8>),
    NonResident(NonResident),
}

#[derive(Clone, Debug)]
pub struct Attr {
    pub ty: u32,
    pub name: String,
    pub flags: u16,
    pub id: u16,
    pub body: AttrBody,
}

impl Attr {
    fn parse(a: &[u8]) -> Result<Attr> {
        let ty = le32(a, 0);
        let non_resident = a[8] != 0;
        let name_len = a[9] as usize;
        let name_off = le16(a, 10) as usize;
        let flags = le16(a, 12);
        let id = le16(a, 14);
        let name = if name_len == 0 { String::new() } else { utf16(sub(a, name_off, name_len * 2)?) };
        let body = if !non_resident {
            if a.len() < 0x18 {
                return Err(parse_err("resident attribute header truncated"));
            }
            let vlen = le32(a, 0x10) as usize;
            let voff = le16(a, 0x14) as usize;
            AttrBody::Resident(sub(a, voff, vlen)?.to_vec())
        } else {
            if a.len() < 0x40 {
                return Err(parse_err("non-resident attribute header truncated"));
            }
            let roff = le16(a, 0x20) as usize;
            if roff > a.len() {
                return Err(parse_err("runlist offset outside the attribute"));
            }
            AttrBody::NonResident(NonResident {
                start_vcn: le64(a, 0x10),
                end_vcn: le64(a, 0x18),
                runs: decode_runs(&a[roff..])?,
                comp_unit: le16(a, 0x22),
                alloc: le64(a, 0x28),
                real: le64(a, 0x30),
                init: le64(a, 0x38),
            })
        };
        Ok(Attr { ty, name, flags, id, body })
    }

    pub fn resident_value(&self) -> Option<&[u8]> {
        match &self.body {
            AttrBody::Resident(v) => Some(v),
            AttrBody::NonResident(_) => None,
        }
    }

    pub fn start_vcn(&self) -> u64 {
        match &self.body {
            AttrBody::Resident(_) => 0,
            AttrBody::NonResident(n) => n.start_vcn,
        }
    }
}

/// An MFT record with its fixups applied.
#[derive(Clone, Debug)]
pub struct Record {
    pub number: u64,
    data: Vec<u8>,
}

impl Record {
    pub fn parse(number: u64, mut data: Vec<u8>) -> Result<Record> {
        apply_fixups(&mut data, b"FILE")?;
        let first = le16(&data, 0x14) as usize;
        if first < 0x30 || first >= data.len() {
            return Err(parse_err(format!("MFT record {number}: bad first attribute offset")));
        }
        Ok(Record { number, data })
    }

    pub fn sequence(&self) -> u16 {
        le16(&self.data, 0x10)
    }

    pub fn flags(&self) -> u16 {
        le16(&self.data, 0x16)
    }

    pub fn in_use(&self) -> bool {
        self.flags() & 1 != 0
    }

    pub fn is_dir(&self) -> bool {
        self.flags() & 2 != 0
    }

    /// Record number of the base record (0 for base records themselves).
    pub fn base_record(&self) -> u64 {
        le64(&self.data, 0x20) & REF_MASK
    }

    pub fn attributes(&self) -> Result<Vec<Attr>> {
        let b = &self.data;
        let mut off = le16(b, 0x14) as usize;
        let mut out = Vec::new();
        loop {
            if off + 4 > b.len() {
                return Err(parse_err(format!("MFT record {}: no attribute end marker", self.number)));
            }
            if le32(b, off) == AT_END {
                return Ok(out);
            }
            if off + 16 > b.len() {
                return Err(parse_err(format!("MFT record {}: attribute header truncated", self.number)));
            }
            let len = le32(b, off + 4) as usize;
            if len < 16 || len > b.len() - off {
                return Err(parse_err(format!("MFT record {}: bad attribute length", self.number)));
            }
            out.push(Attr::parse(&b[off..off + len])?);
            off += len;
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileName {
    /// Full file reference of the parent directory.
    pub parent: u64,
    pub modified: u64,
    pub alloc: u64,
    pub real: u64,
    pub flags: u32,
    pub namespace: u8,
    pub name: String,
}

impl FileName {
    pub fn parse(v: &[u8]) -> Result<FileName> {
        if v.len() < 0x42 {
            return Err(parse_err("$FILE_NAME truncated"));
        }
        let n = v[0x40] as usize;
        Ok(FileName {
            parent: le64(v, 0),
            modified: le64(v, 0x10),
            alloc: le64(v, 0x28),
            real: le64(v, 0x30),
            flags: le32(v, 0x38),
            namespace: v[0x41],
            name: utf16(sub(v, 0x42, 2 * n)?),
        })
    }

    pub fn is_dir(&self) -> bool {
        self.flags & FILE_NAME_DIRECTORY != 0
    }
}

/// Preference order of name spaces: Win32 / Win32&DOS, then POSIX, then DOS.
fn ns_rank(ns: u8) -> u8 {
    match ns {
        1 | 3 => 0,
        0 => 1,
        _ => 2,
    }
}

/// The preferred resident `$FILE_NAME` among `attrs` (DOS short names lose).
pub fn best_file_name(attrs: &[Attr]) -> Option<FileName> {
    attrs
        .iter()
        .filter(|a| a.ty == AT_FILE_NAME)
        .filter_map(|a| a.resident_value().and_then(|v| FileName::parse(v).ok()))
        .min_by_key(|f| ns_rank(f.namespace))
}

/// Modification time from a `$STANDARD_INFORMATION` value.
pub fn si_modified(v: &[u8]) -> Option<i64> {
    if v.len() < 0x10 {
        return None;
    }
    filetime_to_unix(le64(v, 8))
}

#[derive(Clone, Debug)]
pub struct ListEntry {
    pub ty: u32,
    pub name: String,
    pub start_vcn: u64,
    /// Record number (sequence stripped) holding the attribute.
    pub record: u64,
    pub id: u16,
}

pub fn parse_attr_list(v: &[u8]) -> Result<Vec<ListEntry>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while v.len() - off >= 0x1A {
        let ty = le32(v, off);
        if ty == AT_END {
            break;
        }
        let len = le16(v, off + 4) as usize;
        if len < 0x1A || len > v.len() - off {
            return Err(parse_err("bad $ATTRIBUTE_LIST entry length"));
        }
        let e = &v[off..off + len];
        let name_len = e[6] as usize;
        let name_off = e[7] as usize;
        let name = if name_len == 0 { String::new() } else { utf16(sub(e, name_off, 2 * name_len)?) };
        out.push(ListEntry {
            ty,
            name,
            start_vcn: le64(e, 8),
            record: le64(e, 0x10) & REF_MASK,
            id: le16(e, 0x18),
        });
        off += len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put16(b: &mut [u8], o: usize, v: u16) {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// A 1 KiB record with valid fixups and the given attribute bytes.
    fn record(attrs: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; 1024];
        r[..4].copy_from_slice(b"FILE");
        put16(&mut r, 4, 0x30);
        put16(&mut r, 6, 3);
        put16(&mut r, 0x14, 0x38);
        put16(&mut r, 0x16, 1);
        r[0x38..0x38 + attrs.len()].copy_from_slice(attrs);
        put16(&mut r, 0x30, 7);
        for i in 0..2 {
            let end = i * 512 + 510;
            r[0x32 + 2 * i] = r[end];
            r[0x33 + 2 * i] = r[end + 1];
            put16(&mut r, end, 7);
        }
        r
    }

    #[test]
    fn fixups_restore_sector_ends_and_detect_tears() {
        let mut attrs = vec![0xFFu8; 4];
        attrs.extend([0u8; 4]);
        // the array holds the true sector-end bytes; the image carries the USN there
        let mut img = record(&attrs);
        img[0x32] = 0xAB;
        img[0x33] = 0;
        img[0x34] = 0xCD;
        img[0x35] = 0;
        let rec = Record::parse(3, img.clone()).unwrap();
        assert_eq!(rec.data[510], 0xAB);
        assert_eq!(rec.data[1022], 0xCD);
        assert!(rec.attributes().unwrap().is_empty());
        assert!(rec.in_use());

        let mut torn = img.clone();
        torn[1023] ^= 1;
        assert!(matches!(Record::parse(3, torn), Err(Error::Parse(m)) if m == "bad fixup"));
        let mut wrong_count = img.clone();
        put16(&mut wrong_count, 6, 2);
        assert!(Record::parse(3, wrong_count).is_err());
        let mut usa_out = img;
        put16(&mut usa_out, 4, 1020);
        assert!(Record::parse(3, usa_out).is_err());
        assert!(Record::parse(0, vec![0u8; 1024]).is_err());
    }

    #[test]
    fn corrupt_attributes_are_parse_errors() {
        // length runs past the record
        let mut a = vec![0u8; 0x18];
        a[..4].copy_from_slice(&0x80u32.to_le_bytes());
        a[4..8].copy_from_slice(&0x2000u32.to_le_bytes());
        assert!(Record::parse(1, record(&a)).unwrap().attributes().is_err());
        // resident value outside the attribute
        let mut a = vec![0u8; 0x18];
        a[..4].copy_from_slice(&0x80u32.to_le_bytes());
        a[4..8].copy_from_slice(&0x18u32.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&100u32.to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a.extend(0xFFFF_FFFFu32.to_le_bytes());
        assert!(Record::parse(1, record(&a)).unwrap().attributes().is_err());
        // no end marker at all: attributes of zero length
        assert!(Record::parse(1, record(&[0x80, 0, 0, 0, 0, 0, 0, 0])).unwrap().attributes().is_err());
    }

    #[test]
    fn random_records_never_panic() {
        let mut x = 0xDEAD_BEEFu32;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..2000 {
            let mut attrs = vec![0u8; 600];
            for v in attrs.iter_mut() {
                *v = next() as u8;
            }
            // plausible attribute headers with random lengths/offsets
            let mut off = 0;
            while off + 0x48 < attrs.len() {
                let ty = [0x10u32, 0x20, 0x30, 0x80, 0x90][(next() % 5) as usize];
                attrs[off..off + 4].copy_from_slice(&ty.to_le_bytes());
                let len = (next() % 0x100) as u32 & !7;
                attrs[off + 4..off + 8].copy_from_slice(&len.to_le_bytes());
                attrs[off + 8] = (next() % 2) as u8;
                if len == 0 {
                    break;
                }
                off += len as usize;
            }
            if let Ok(rec) = Record::parse(9, record(&attrs)) {
                if let Ok(list) = rec.attributes() {
                    let _ = best_file_name(&list);
                    for a in &list {
                        if let Some(v) = a.resident_value() {
                            let _ = parse_attr_list(v);
                            let _ = FileName::parse(v);
                            let _ = si_modified(v);
                        }
                    }
                }
            }
        }
    }
}
