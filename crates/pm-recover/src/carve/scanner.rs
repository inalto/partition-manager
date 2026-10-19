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
/// zip, a thumbnail in a JPEG); incomplete hits never suppress anything. The result is sorted
/// by offset. Progress is reported as phase "Carving" over the device length.
pub fn carve(
    dev: &mut dyn BlockDevice,
    sigs: &[Signature],
    sector: u32,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<Vec<CarvedFile>> {
    if sector == 0 {
        return Err(Error::Invalid("carve sector size must be greater than zero".into()));
    }
    cancel.check()?;
    let dev_len = dev.len();
    let step = sector as u64;
    let mut tracker = ProgressTracker::new(progress, "Carving", dev_len);
    let matcher = Matcher::new(sigs);
    let mut found: Vec<CarvedFile> = Vec::new();
    if matcher.groups.is_empty() || dev_len == 0 {
        tracker.finish();
        return Ok(found);
    }
    let overlap = matcher.max_extent + OVERLAP_SLACK;
    let mut buf = vec![0u8; CHUNK as usize + overlap];
    let mut skip_until = vec![0u64; sigs.len()];

    let mut pos = 0u64;
    while pos < dev_len {
        cancel.check()?;
        let scan_len = CHUNK.min(dev_len - pos);
        let read_len = (CHUNK + overlap as u64).min(dev_len - pos) as usize;
        dev.read_at(pos, &mut buf[..read_len])?;
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
                    if let Some((len, complete)) = determine_len(dev, sig, start, dev_len, cancel)? {
                        if complete {
                            skip_until[si] = start + len;
                        }
                        found.push(CarvedFile {
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
    found.sort_by(|a, b| a.offset.cmp(&b.offset).then_with(|| a.sig.cmp(&b.sig)));
    Ok(found)
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
        LengthRule::Footer(footer) => {
            let from = start + (sig.header_offset + sig.header.len()) as u64;
            let end = start + cap;
            if from >= end {
                return Ok(Some((cap, false)));
            }
            match find_forward(dev, from, end, footer, cancel, &mut |_, _| Ok(true))? {
                Some(p) => Ok(Some((p + footer.len() as u64 - start, true))),
                None => Ok(Some((cap, false))),
            }
        }
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
