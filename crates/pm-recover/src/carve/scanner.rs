//! The carving scan: find signature headers at sector-aligned offsets and work out how long
//! each file is. The device is only ever read.

use super::{Category, LengthRule, Signature};
use pm_core::{BlockDevice, CancelToken, Error, ProgressSink, ProgressTracker, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bytes of header start offsets examined per read. The cancel token is checked once per chunk.
const CHUNK: u64 = 4 << 20;
/// Block size for forward footer searches and for extraction.
const BLOCK: usize = 1 << 20;
/// Slack added to the chunk overlap on top of the longest header check.
const OVERLAP_SLACK: usize = 16;
/// Upper bound on the atoms walked for one MP4 file.
const MAX_MP4_BOXES: u32 = 1_000_000;
/// Size of the sub-reads used to work around a failing chunk read.
const SUB_READ: usize = 64 << 10;
/// Upper bound on the segments/blocks walked for one JPEG or GIF.
const MAX_STRUCTURE_STEPS: u32 = 4_000_000;
/// The largest plausible `ftyp` atom.
const MAX_FTYP: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarvedFile {
    /// Byte offset of the file on the device.
    pub offset: u64,
    pub len: u64,
    /// Name of the matching [`Signature`].
    pub sig: String,
    pub ext: String,
    pub category: Category,
    /// The end was found (footer, container length or atom chain) rather than assumed from
    /// `max_len` or the device end.
    pub complete: bool,
}

/// Outcome of a carving scan.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarveReport {
    /// Carved files sorted by offset.
    pub files: Vec<CarvedFile>,
    /// Byte ranges `(offset, len)` that could not be read, sorted and merged. They were
    /// scanned as zeros, so files overlapping them may be missing or incomplete.
    pub unreadable: Vec<(u64, u64)>,
    /// Bytes of the device that were read and examined.
    pub bytes_scanned: u64,
}

/// Header lookup: signatures grouped by header offset, indexed by the first header byte.
struct Matcher<'a> {
    sigs: &'a [Signature],
    groups: Vec<Group>,
    /// Longest `header_offset + header.len()` / secondary extent over all signatures.
    max_extent: usize,
}

struct Group {
    offset: usize,
    by_first: Vec<Vec<usize>>,
}

impl<'a> Matcher<'a> {
    fn new(sigs: &'a [Signature]) -> Matcher<'a> {
        let mut groups: Vec<Group> = Vec::new();
        let mut max_extent = 0;
        for (i, s) in sigs.iter().enumerate() {
            if s.header.is_empty() {
                continue;
            }
            max_extent = max_extent.max(s.match_extent());
            let g = match groups.iter().position(|g| g.offset == s.header_offset) {
                Some(g) => g,
                None => {
                    groups.push(Group { offset: s.header_offset, by_first: vec![Vec::new(); 256] });
                    groups.len() - 1
                }
            };
            groups[g].by_first[s.header[0] as usize].push(i);
        }
        Matcher { sigs, groups, max_extent }
    }

    /// Call `hit(sig_index)` for every signature whose header (and secondary check) matches
    /// the file starting at `data[start..]`.
    #[inline]
    fn matches_at(&self, data: &[u8], start: usize, mut hit: impl FnMut(usize)) {
        for g in &self.groups {
            let Some(&first) = data.get(start + g.offset) else { continue };
            for &si in &g.by_first[first as usize] {
                let s = &self.sigs[si];
                if data.len() < start + s.match_extent() {
                    continue;
                }
                if &data[start + g.offset..start + g.offset + s.header.len()] != s.header {
                    continue;
                }
                if let Some((o, b)) = s.secondary {
                    if &data[start + o..start + o + b.len()] != b {
                        continue;
                    }
                }
                hit(si);
            }
        }
    }
}

/// Scan the whole device for files described by `sigs`.
///
/// `sector` is the alignment of file starts (512 for ordinary disks, 1 for "any offset" mode,
/// which is much slower). The device is read in 4 MiB chunks that overlap by the longest header
/// check, so headers straddling a chunk boundary are found exactly once. The end of each file
/// comes from the signature's [`LengthRule`]; without a verifiable end the file is reported as
/// `max_len` long (capped at the device end) and `complete == false`.
///
/// Hits of different types may overlap and are all reported. Once a type has produced a
/// *complete* file, further headers of that same type inside it are ignored (a zip nested in a
/// zip, a thumbnail in a JPEG); incomplete hits never suppress anything.
///
/// A read error does not abort the scan: the failing chunk is re-read in 64 KiB pieces and then
/// sector by sector, unreadable sectors are scanned as zeros and listed in
/// [`CarveReport::unreadable`]. A read error while measuring a file makes that file incomplete.
/// Progress is reported as phase "Carving" over the device length.
pub fn carve(
    dev: &mut dyn BlockDevice,
    sigs: &[Signature],
    sector: u32,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<CarveReport> {
    if sector == 0 {
        return Err(Error::Invalid("carve sector size must be greater than zero".into()));
    }
    cancel.check()?;
    let dev_len = dev.len();
    let step = sector as u64;
    let mut tracker = ProgressTracker::new(progress, "Carving", dev_len);
    let matcher = Matcher::new(sigs);
    let mut report = CarveReport::default();
    if matcher.groups.is_empty() || dev_len == 0 {
        tracker.finish();
        return Ok(report);
    }
    let overlap = matcher.max_extent + OVERLAP_SLACK;
    let mut buf = vec![0u8; CHUNK as usize + overlap];
    let mut skip_until = vec![0u64; sigs.len()];
    let mut chunk_bad: Vec<(u64, u64)> = Vec::new();
    let mut measure_bad: Vec<(u64, u64)> = Vec::new();

    let mut pos = 0u64;
    while pos < dev_len {
        cancel.check()?;
        let scan_len = CHUNK.min(dev_len - pos);
        let read_len = (CHUNK + overlap as u64).min(dev_len - pos) as usize;
        read_resilient(dev, pos, &mut buf[..read_len], cancel, &mut chunk_bad)?;
        let data = &buf[..read_len];

        let mut hits: Vec<usize> = Vec::new();
        // first sector-aligned start (absolute alignment) inside this chunk
        let mut rel = (step - pos % step) % step;
        while rel < scan_len {
            matcher.matches_at(data, rel as usize, |si| hits.push(si));
            if !hits.is_empty() {
                let start = pos + rel;
                for si in hits.drain(..) {
                    if start < skip_until[si] {
                        continue;
                    }
                    let sig = &sigs[si];
                    let measured = match determine_len(dev, sig, start, dev_len, cancel) {
                        Ok(m) => m,
                        Err(Error::Io { offset, len, .. }) => {
                            measure_bad.push((offset, len as u64));
                            Some((sig.max_len.min(dev_len - start), false))
                        }
                        Err(e) => return Err(e),
                    };
                    if let Some((len, complete)) = measured {
                        if complete {
                            skip_until[si] = start + len;
                        }
                        report.files.push(CarvedFile {
                            offset: start,
                            len,
                            sig: sig.name.to_string(),
                            ext: sig.ext.to_string(),
                            category: sig.category,
                            complete,
                        });
                    }
                }
            }
            rel += step;
        }
        tracker.add(scan_len);
        pos += scan_len;
    }
    tracker.finish();
    report.files.sort_by(|a, b| a.offset.cmp(&b.offset).then_with(|| a.sig.cmp(&b.sig)));
    let chunk_bad = coalesce(chunk_bad);
    report.bytes_scanned = dev_len.saturating_sub(chunk_bad.iter().map(|r| r.1).sum());
    let mut all = chunk_bad;
    all.extend(measure_bad);
    report.unreadable = coalesce(all);
    Ok(report)
}

/// Read `buf` at `off`; if that fails retry in 64 KiB pieces, then sector by sector. Sectors
/// that still fail are zero-filled and appended to `bad`.
fn read_resilient(
    dev: &mut dyn BlockDevice,
    off: u64,
    buf: &mut [u8],
    cancel: &CancelToken,
    bad: &mut Vec<(u64, u64)>,
) -> Result<()> {
    if dev.read_at(off, buf).is_ok() {
        return Ok(());
    }
    let sector = (dev.sector_size() as usize).max(1);
    for (i, piece) in buf.chunks_mut(SUB_READ).enumerate() {
        cancel.check()?;
        let piece_off = off + (i * SUB_READ) as u64;
        if dev.read_at(piece_off, piece).is_ok() {
            continue;
        }
        for (j, s) in piece.chunks_mut(sector).enumerate() {
            let o = piece_off + (j * sector) as u64;
            if dev.read_at(o, s).is_err() {
                s.fill(0);
                bad.push((o, s.len() as u64));
            }
        }
    }
    Ok(())
}

/// Sort ranges and merge those that overlap or touch.
fn coalesce(mut v: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    v.retain(|r| r.1 > 0);
    v.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (o, l) in v {
        match out.last_mut() {
            Some(last) if o <= last.0 + last.1 => last.1 = last.1.max(o + l - last.0),
            _ => out.push((o, l)),
        }
    }
    out
}

/// Length of the file of type `sig` starting at `start`, as `(len, complete)`; `None` rejects
/// the hit as a false positive. Every parsed length is bounded by `max_len` and the device end.
fn determine_len(
    dev: &mut dyn BlockDevice,
    sig: &Signature,
    start: u64,
    dev_len: u64,
    cancel: &CancelToken,
) -> Result<Option<(u64, bool)>> {
    let avail = dev_len - start;
    let cap = sig.max_len.min(avail);
    match sig.rule {
        LengthRule::MaxOnly => Ok(Some((cap, false))),
        LengthRule::Footer(footer) => footer_scan(dev, sig, start, cap, footer, true, cancel),
        LengthRule::JpegMarkers => match jpeg_walk(dev, start, cap, cancel)? {
            Walk::End(len) => Ok(Some((len, true))),
            Walk::Truncated => Ok(Some((cap, false))),
            Walk::Broken => footer_scan(dev, sig, start, cap, sig.footer.unwrap_or(&[]), false, cancel),
        },
        LengthRule::GifBlocks => match gif_walk(dev, start, cap, cancel)? {
            Walk::End(len) => Ok(Some((len, true))),
            Walk::Truncated => Ok(Some((cap, false))),
            Walk::Broken => footer_scan(dev, sig, start, cap, sig.footer.unwrap_or(&[]), false, cancel),
        },
        LengthRule::PdfLastEof => pdf_len(dev, sig, start, cap, cancel),
        LengthRule::Riff => {
            let mut h = [0u8; 8];
            if cap < 12 {
                return Ok(None);
            }
            dev.read_at(start, &mut h)?;
            let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64;
            if size < 4 {
                return Ok(None);
            }
            let total = size + 8;
            if total > cap {
                Ok(Some((cap, false)))
            } else {
                Ok(Some((total, true)))
            }
        }
        LengthRule::Mp4Atoms => mp4_len(dev, sig, start, avail, cap, cancel),
        LengthRule::ZipEocd => zip_len(dev, sig, start, dev_len, cap, cancel),
    }
}

/// Search for `footer` after the header; `found_complete` is the `complete` flag to report
/// when it is found (false for fallbacks from a failed structural walk).
fn footer_scan(
    dev: &mut dyn BlockDevice,
    sig: &Signature,
    start: u64,
    cap: u64,
    footer: &[u8],
    found_complete: bool,
    cancel: &CancelToken,
) -> Result<Option<(u64, bool)>> {
    let from = start + (sig.header_offset + sig.header.len()) as u64;
    let end = start + cap;
    if footer.is_empty() || from >= end {
        return Ok(Some((cap, false)));
    }
    match find_forward(dev, from, end, footer, cancel, &mut |_, _| Ok(true))? {
        Some(p) => Ok(Some((p + footer.len() as u64 - start, found_complete))),
        None => Ok(Some((cap, false))),
    }
}

/// Result of walking a file's internal structure.
enum Walk {
    /// The file ends here (length in bytes).
    End(u64),
    /// The structure was fine up to the cap or device end.
    Truncated,
    /// The structure does not parse.
    Broken,
}

fn read_u8(dev: &mut dyn BlockDevice, off: u64) -> Result<u8> {
    let mut b = [0u8; 1];
    dev.read_at(off, &mut b)?;
    Ok(b[0])
}

/// Walk JPEG marker segments from SOI. Segments (APPn with an embedded thumbnail, DQT, ...) are
/// skipped by their length, so only a top-level EOI ends the file; after SOS the entropy-coded
/// data is scanned for the next real marker (stuffed `FF 00`, RSTn and fill bytes are data).
fn jpeg_walk(dev: &mut dyn BlockDevice, start: u64, cap: u64, cancel: &CancelToken) -> Result<Walk> {
    let mut pos = 2u64;
    let mut steps = 0u32;
    loop {
        if steps >= MAX_STRUCTURE_STEPS {
            return Ok(Walk::Broken);
        }
        if steps % 256 == 0 {
            cancel.check()?;
        }
        steps += 1;
        if pos + 2 > cap {
            return Ok(Walk::Truncated);
        }
        let mut m = [0u8; 2];
        dev.read_at(start + pos, &mut m)?;
        if m[0] != 0xFF {
            return Ok(Walk::Broken);
        }
        match m[1] {
            0xFF => pos += 1, // fill byte
            0xD9 => return Ok(Walk::End(pos + 2)),
            0x01 | 0xD0..=0xD7 => pos += 2, // standalone markers
            0x00 | 0xD8 => return Ok(Walk::Broken),
            marker => {
                if pos + 4 > cap {
                    return Ok(Walk::Truncated);
                }
                let mut l = [0u8; 2];
                dev.read_at(start + pos + 2, &mut l)?;
                let len = u16::from_be_bytes(l) as u64;
                if len < 2 {
                    return Ok(Walk::Broken);
                }
                pos += 2 + len;
                if pos > cap {
                    return Ok(Walk::Truncated);
                }
                if marker == 0xDA {
                    match scan_entropy(dev, start, pos, cap, cancel)? {
                        Some(q) => pos = q,
                        None => return Ok(Walk::Truncated),
                    }
                }
            }
        }
    }
}

/// Position (relative to `start`) of the first marker in entropy-coded data beginning at
/// `from`, or `None` when the data runs to `cap` without one.
fn scan_entropy(
    dev: &mut dyn BlockDevice,
    start: u64,
    from: u64,
    cap: u64,
    cancel: &CancelToken,
) -> Result<Option<u64>> {
    if from >= cap {
        return Ok(None);
    }
    let mut buf = vec![0u8; (BLOCK + 1).min((cap - from) as usize)];
    let mut p = from;
    loop {
        cancel.check()?;
        let n = (buf.len() as u64).min(cap - p) as usize;
        dev.read_at(start + p, &mut buf[..n])?;
        let last = p + n as u64 == cap;
        // offsets handled by this block; the next one starts BLOCK bytes further
        let limit = if last { n } else { BLOCK };
        let mut i = 0;
        while i < limit {
            match buf[i..limit].iter().position(|&b| b == 0xFF) {
                None => break,
                Some(k) => {
                    i += k;
                    if i + 1 >= n {
                        return Ok(None); // an FF as the very last byte
                    }
                    let next = buf[i + 1];
                    if next == 0x00 || next == 0xFF || (0xD0..=0xD7).contains(&next) {
                        i += 1;
                        continue;
                    }
                    return Ok(Some(p + i as u64));
                }
            }
        }
        if last {
            return Ok(None);
        }
        p += BLOCK as u64;
    }
}

/// Skip the sub-blocks starting at `*pos`; false when they run past `cap`.
fn gif_skip_sub_blocks(
    dev: &mut dyn BlockDevice,
    start: u64,
    pos: &mut u64,
    cap: u64,
    steps: &mut u32,
    cancel: &CancelToken,
) -> Result<bool> {
    loop {
        if *steps % 4096 == 0 {
            cancel.check()?;
        }
        *steps += 1;
        if *pos >= cap {
            return Ok(false);
        }
        let size = read_u8(dev, start + *pos)? as u64;
        *pos += 1 + size;
        if size == 0 {
            return Ok(true);
        }
    }
}

/// Walk GIF blocks: header, logical screen (+ global color table), then image descriptors,
/// extensions and finally the trailer `3B`.
fn gif_walk(dev: &mut dyn BlockDevice, start: u64, cap: u64, cancel: &CancelToken) -> Result<Walk> {
    if cap < 13 {
        return Ok(Walk::Truncated);
    }
    let mut h = [0u8; 13];
    dev.read_at(start, &mut h)?;
    let mut pos = 13u64;
    if h[10] & 0x80 != 0 {
        pos += 3u64 << ((h[10] & 7) + 1);
    }
    let mut steps = 0u32;
    loop {
        if steps >= MAX_STRUCTURE_STEPS {
            return Ok(Walk::Broken);
        }
        if pos >= cap {
            return Ok(Walk::Truncated);
        }
        match read_u8(dev, start + pos)? {
            0x3B => return Ok(Walk::End(pos + 1)),
            0x2C => {
                if pos + 11 > cap {
                    return Ok(Walk::Truncated);
                }
                let packed = read_u8(dev, start + pos + 9)?;
                pos += 10;
                if packed & 0x80 != 0 {
                    pos += 3u64 << ((packed & 7) + 1);
                }
                pos += 1; // LZW minimum code size
            }
            0x21 => pos += 2, // introducer + label
            _ => return Ok(Walk::Broken),
        }
        if !gif_skip_sub_blocks(dev, start, &mut pos, cap, &mut steps, cancel)? {
            return Ok(Walk::Truncated);
        }
    }
}

/// A PDF ends at its last `%%EOF` before the next `%PDF-` header (or the cap).
fn pdf_len(
    dev: &mut dyn BlockDevice,
    sig: &Signature,
    start: u64,
    cap: u64,
    cancel: &CancelToken,
) -> Result<Option<(u64, bool)>> {
    let from = start + sig.header.len() as u64;
    let end = start + cap;
    if from >= end {
        return Ok(Some((cap, false)));
    }
    let limit = find_forward(dev, from, end, sig.header, cancel, &mut |_, _| Ok(true))?.unwrap_or(end);
    let eof = sig.footer.unwrap_or(b"%%EOF");
    let mut last: Option<u64> = None;
    find_forward(dev, from, limit, eof, cancel, &mut |_, p| {
        last = Some(p);
        Ok(false)
    })?;
    Ok(Some(match last {
        Some(p) => (p + eof.len() as u64 - start, true),
        None => (limit - start, false),
    }))
}

/// Walk the top-level atom chain of an ISO base media file.
fn mp4_len(
    dev: &mut dyn BlockDevice,
    sig: &Signature,
    start: u64,
    avail: u64,
    cap: u64,
    cancel: &CancelToken,
) -> Result<Option<(u64, bool)>> {
    const KNOWN_TOP: [&[u8; 4]; 13] = [
        b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"moof", b"mfra", b"sidx",
        b"styp", b"meta", b"pdin",
    ];
    let reached_cap = avail <= sig.max_len;
    let mut pos = 0u64;
    let mut boxes = 0u32;
    loop {
        if boxes % 1024 == 0 {
            cancel.check()?;
        }
        if boxes >= MAX_MP4_BOXES {
            return Ok(Some((pos, true)));
        }
        if pos + 8 > cap {
            break;
        }
        let mut h = [0u8; 16];
        dev.read_at(start + pos, &mut h[..8])?;
        let kind = [h[4], h[5], h[6], h[7]];
        if !kind.iter().all(|b| (0x20..0x7F).contains(b)) {
            if boxes < 2 {
                return Ok(None);
            }
            return Ok(Some((pos, true)));
        }
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        if size == 0 {
            // the atom runs to the end of the file, which we cannot know
            return Ok(if boxes == 0 { None } else { Some((cap, false)) });
        }
        if size == 1 {
            if pos + 16 > cap {
                break;
            }
            dev.read_at(start + pos + 8, &mut h[8..16])?;
            size = u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
            if size < 16 {
                return Ok(if boxes < 2 { None } else { Some((pos, true)) });
            }
        } else if size < 8 {
            return Ok(if boxes < 2 { None } else { Some((pos, true)) });
        }
        if boxes == 0 && (&kind != b"ftyp" || !(12..=MAX_FTYP).contains(&size)) {
            return Ok(None);
        }
        if size > cap - pos {
            // the atom does not fit: truncated file, or whatever follows a complete one
            if boxes >= 1 && KNOWN_TOP.iter().any(|k| **k == kind) {
                return Ok(Some((cap, false)));
            }
            return Ok(if boxes < 2 { None } else { Some((pos, true)) });
        }
        pos += size;
        boxes += 1;
    }
    // ran out of room for another atom header
    if boxes < 2 {
        return Ok(None);
    }
    Ok(Some((pos, pos < cap || reached_cap)))
}

/// Find the end-of-central-directory record that closes the archive starting at `start`.
fn zip_len(
    dev: &mut dyn BlockDevice,
    sig: &Signature,
    start: u64,
    dev_len: u64,
    cap: u64,
    cancel: &CancelToken,
) -> Result<Option<(u64, bool)>> {
    const EOCD: &[u8] = b"PK\x05\x06";
    let from = start + sig.header.len() as u64;
    let limit = start + cap;
    if from >= limit {
        return Ok(Some((cap, false)));
    }
    let mut loose: Option<u64> = None;
    let mut strict_end: Option<u64> = None;
    let found = find_forward(dev, from, limit, EOCD, cancel, &mut |dev, p| {
        if p + 22 > dev_len {
            return Ok(false);
        }
        let mut r = [0u8; 22];
        dev.read_at(p, &mut r)?;
        let comment = u16::from_le_bytes([r[20], r[21]]) as u64;
        let end = p + 22 + comment;
        if end > limit {
            return Ok(false);
        }
        let entries = u16::from_le_bytes([r[10], r[11]]);
        let cd_size = u32::from_le_bytes([r[12], r[13], r[14], r[15]]);
        let cd_off = u32::from_le_bytes([r[16], r[17], r[18], r[19]]);
        let zip64 = entries == 0xFFFF || cd_size == u32::MAX || cd_off == u32::MAX;
        // a record of this archive: its central directory ends exactly where the record begins
        if zip64 || cd_size as u64 + cd_off as u64 == p - start {
            strict_end = Some(end);
            return Ok(true);
        }
        if loose.is_none() {
            loose = Some(end);
        }
        Ok(false)
    })?;
    let _ = found;
    match strict_end.or(loose) {
        Some(end) => Ok(Some((end - start, true))),
        None => Ok(Some((cap, false))),
    }
}

/// Position of the first occurrence of `needle` in `[from, end)` of the device that `accept`
/// approves. Reads in 1 MiB blocks overlapping by `needle.len() - 1`; checks `cancel` per block.
fn find_forward(
    dev: &mut dyn BlockDevice,
    from: u64,
    end: u64,
    needle: &[u8],
    cancel: &CancelToken,
    accept: &mut dyn FnMut(&mut dyn BlockDevice, u64) -> Result<bool>,
) -> Result<Option<u64>> {
    let nl = needle.len();
    if nl == 0 || from >= end || end - from < nl as u64 {
        return Ok(None);
    }
    let mut buf = vec![0u8; (BLOCK + nl - 1).min((end - from) as usize)];
    let mut p = from;
    loop {
        cancel.check()?;
        let n = (buf.len() as u64).min(end - p) as usize;
        dev.read_at(p, &mut buf[..n])?;
        let last_block = p + n as u64 == end;
        // match starts handled by this block; the next block re-reads the tail
        let limit = if last_block { n } else { BLOCK };
        let hay = &buf[..n];
        let mut i = 0;
        while i + nl <= n && i < limit {
            match hay[i..n - nl + 1].iter().position(|&b| b == needle[0]) {
                None => break,
                Some(k) => {
                    i += k;
                    if i >= limit {
                        break;
                    }
                    if &hay[i..i + nl] == needle && accept(dev, p + i as u64)? {
                        return Ok(Some(p + i as u64));
                    }
                    i += 1;
                }
            }
        }
        if last_block {
            return Ok(None);
        }
        p += BLOCK as u64;
    }
}

/// Keep an extension from an untrusted source (a session file) to ASCII alphanumerics.
fn safe_ext(ext: &str) -> &str {
    if (1..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        ext
    } else {
        "bin"
    }
}

/// Copy a carved file to `dest/<offset in hex>.<ext>` and return the path. `dest` is created
/// if needed; an existing file of that name is replaced. On failure no partial file is left.
pub fn extract_carved(dev: &mut dyn BlockDevice, f: &CarvedFile, dest: &Path) -> Result<PathBuf> {
    match f.offset.checked_add(f.len) {
        Some(end) if end <= dev.len() => {}
        _ => {
            return Err(Error::Invalid(format!(
                "carved range {}+{} lies outside the device (length {})",
                f.offset,
                f.len,
                dev.len()
            )))
        }
    }
    std::fs::create_dir_all(dest).map_err(|e| Error::io(0, 0, e))?;
    let path = dest.join(format!("{:012x}.{}", f.offset, safe_ext(&f.ext)));
    let result = write_range(dev, f, &path);
    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    result.map(|_| path)
}

fn write_range(dev: &mut dyn BlockDevice, f: &CarvedFile, path: &Path) -> Result<()> {
    let file = std::fs::File::create(path).map_err(|e| Error::io(f.offset, 0, e))?;
    let mut out = std::io::BufWriter::new(file);
    let mut buf = vec![0u8; BLOCK.min(f.len as usize).max(1)];
    let mut done = 0u64;
    while done < f.len {
        let n = (buf.len() as u64).min(f.len - done) as usize;
        dev.read_at(f.offset + done, &mut buf[..n])?;
        out.write_all(&buf[..n]).map_err(|e| Error::io(f.offset + done, n, e))?;
        done += n as u64;
    }
    out.flush().map_err(|e| Error::io(f.offset, 0, e))
}
