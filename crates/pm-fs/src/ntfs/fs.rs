//! The read-only NTFS [`FileSystem`].

use super::boot::NtfsBoot;
use super::index;
use super::mft::*;
use crate::bitmap::UsedMap;
use crate::traits::{Entry, FileSystem};
use crate::util::read_padded;
use pm_core::{BlockDevice, CancelToken, Error, FsKind, ProgressReport, ProgressSink, Result};
use std::collections::{HashMap, HashSet};

const ROOT_RECORD: u64 = 5;
const MFTMIRR_RECORD: u64 = 1;
const BITMAP_RECORD: u64 = 6;
const VOLUME_RECORD: u64 = 3;
/// Records scanned per progress report / cancellation check.
const SCAN_BATCH: u64 = 1024;
/// Larger attribute lists are treated as corrupt (real ones stay far below this).
const MAX_ATTR_LIST: u64 = 4 << 20;
/// Parent chains longer than this are not resolved.
const MAX_PATH_DEPTH: usize = 64;

#[derive(Clone, Debug)]
struct Run {
    vcn: u64,
    lcn: Option<u64>,
    len: u64,
}

/// The contents of one attribute, all extents merged.
#[derive(Clone, Debug)]
enum Stream {
    Resident { flags: u16, data: Vec<u8> },
    NonResident { flags: u16, runs: Vec<Run>, alloc: u64, real: u64, init: u64 },
}

impl Stream {
    /// Merge the extents of one attribute (any order) into a stream.
    fn from_extents(mut ext: Vec<Attr>) -> Result<Stream> {
        if ext.len() == 1 {
            if let AttrBody::Resident(v) = &ext[0].body {
                return Ok(Stream::Resident { flags: ext[0].flags, data: v.clone() });
            }
        }
        ext.sort_by_key(|a| a.start_vcn());
        let first = match ext.first().map(|a| &a.body) {
            Some(AttrBody::NonResident(n)) if n.start_vcn == 0 => n.clone(),
            Some(AttrBody::NonResident(_)) => return Err(Error::Parse("attribute has no extent at VCN 0".into())),
            Some(AttrBody::Resident(_)) => {
                return Err(Error::Parse("resident attribute split into extents".into()))
            }
            None => return Err(Error::NotFound("attribute".into())),
        };
        let flags = ext[0].flags;
        let mut runs = Vec::new();
        let mut next = 0u64;
        for a in &ext {
            let AttrBody::NonResident(n) = &a.body else {
                return Err(Error::Parse("resident attribute split into extents".into()));
            };
            // extents must tile the VCN space exactly: a gap means a lost extent, which must
            // never read back as zeros
            if n.start_vcn < next {
                return Err(Error::Parse("overlapping attribute extents".into()));
            }
            if n.start_vcn > next {
                return Err(Error::Parse(format!(
                    "attribute extents leave VCNs {next}..{} unmapped",
                    n.start_vcn
                )));
            }
            // `end_vcn` is -1 for an empty attribute, hence the wrapping arithmetic
            let after_end = n.end_vcn.wrapping_add(1);
            if after_end < n.start_vcn {
                return Err(Error::Parse(format!("attribute extent VCN range {}..={} is inverted", n.start_vcn, n.end_vcn)));
            }
            for &(lcn, len) in &n.runs {
                runs.push(Run { vcn: next, lcn, len });
                next = next.checked_add(len).ok_or_else(|| Error::Parse("runlist overflows".into()))?;
            }
            if next != after_end {
                return Err(Error::Parse(format!(
                    "runlist of extent VCN {}..={} ends at VCN {next}",
                    n.start_vcn, n.end_vcn
                )));
            }
        }
        Ok(Stream::NonResident { flags, runs, alloc: first.alloc, real: first.real, init: first.init.min(first.real) })
    }

    fn len(&self) -> u64 {
        match self {
            Stream::Resident { data, .. } => data.len() as u64,
            Stream::NonResident { real, .. } => *real,
        }
    }

    /// Bytes holding defined data; the rest up to `len()` reads as zeros.
    fn init(&self) -> u64 {
        match self {
            Stream::Resident { data, .. } => data.len() as u64,
            Stream::NonResident { init, .. } => *init,
        }
    }

    /// Bytes covered by the runlist (allocated or sparse).
    fn mapped_bytes(&self, cs: u64) -> u64 {
        match self {
            Stream::Resident { data, .. } => data.len() as u64,
            Stream::NonResident { runs, .. } => {
                runs.last().map_or(0, |r| r.vcn.saturating_add(r.len).saturating_mul(cs))
            }
        }
    }

    fn flags(&self) -> u16 {
        match self {
            Stream::Resident { flags, .. } | Stream::NonResident { flags, .. } => *flags,
        }
    }
}

/// Read up to `buf.len()` bytes of `s` at `offset`; returns the count (0 at or past the end).
/// Sparse runs and bytes past the initialized size read as zeros.
fn read_stream(dev: &mut dyn BlockDevice, cs: u64, s: &Stream, offset: u64, buf: &mut [u8]) -> Result<usize> {
    let size = s.len();
    if offset >= size || buf.is_empty() {
        return Ok(0);
    }
    let want = (buf.len() as u64).min(size - offset) as usize;
    let (runs, init) = match s {
        Stream::Resident { data, .. } => {
            let o = offset as usize;
            buf[..want].copy_from_slice(&data[o..o + want]);
            return Ok(want);
        }
        Stream::NonResident { runs, init, .. } => (runs, *init),
    };
    let overflow = || Error::Parse("data run beyond addressable range".into());
    let mut done = 0usize;
    while done < want {
        let pos = offset + done as u64;
        if pos >= init {
            buf[done..want].fill(0);
            break;
        }
        let vcn = pos / cs;
        let i = runs.partition_point(|r| r.vcn.saturating_add(r.len) <= vcn);
        let r = match runs.get(i) {
            Some(r) if r.vcn <= vcn => r,
            _ => return Err(Error::Parse(format!("VCN {vcn} is not mapped by the runlist"))),
        };
        let run_end = r.vcn.checked_add(r.len).and_then(|v| v.checked_mul(cs)).ok_or_else(overflow)?;
        let n = ((want - done) as u64).min(run_end - pos).min(init - pos) as usize;
        match r.lcn {
            None => buf[done..done + n].fill(0),
            Some(lcn) => {
                let at = lcn
                    .checked_add(vcn - r.vcn)
                    .and_then(|c| c.checked_mul(cs))
                    .and_then(|b| b.checked_add(pos % cs))
                    .ok_or_else(overflow)?;
                if at.checked_add(n as u64).map_or(true, |e| e > dev.len()) {
                    return Err(Error::Parse(format!("data run at byte {at} lies beyond the device")));
                }
                dev.read_at(at, &mut buf[done..done + n])?;
            }
        }
        done += n;
    }
    Ok(want)
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" || dir.is_empty() {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// A mounted (read-only) NTFS volume.
pub struct Ntfs {
    dev: Box<dyn BlockDevice>,
    boot: NtfsBoot,
    /// The `$MFT` unnamed `$DATA` stream.
    mft: Stream,
    label: String,
    root_modified: Option<i64>,
    /// Unnamed `$DATA` of the record last read through `read_file`.
    cache: Option<(u64, Stream)>,
}

/// Open the NTFS volume at the start of `dev`.
pub fn open_ntfs(dev: Box<dyn BlockDevice>) -> Result<Ntfs> {
    Ntfs::open(dev)
}

impl Ntfs {
    pub fn open(mut dev: Box<dyn BlockDevice>) -> Result<Ntfs> {
        let mut s0 = [0u8; 512];
        read_padded(&mut *dev, 0, &mut s0)?;
        let boot = NtfsBoot::parse(&s0)?;
        if boot.volume_bytes() > dev.len() {
            return Err(Error::Parse(format!(
                "NTFS volume of {} bytes exceeds the {}-byte device",
                boot.volume_bytes(),
                dev.len()
            )));
        }
        let rec0 = match Self::raw_record(&mut *dev, &boot, boot.mft_lcn) {
            Ok(r) => r,
            // a damaged first record: $MFTMirr holds a copy of records 0..=3
            Err(e) => Self::raw_record(&mut *dev, &boot, boot.mftmirr_lcn).map_err(|_| e)?,
        };
        let mut fs = Ntfs {
            dev,
            boot,
            mft: Stream::Resident { flags: 0, data: Vec::new() },
            label: String::new(),
            root_modified: None,
            cache: None,
        };
        fs.bootstrap_mft(&rec0)?;
        if let Ok(rec) = fs.read_record(VOLUME_RECORD) {
            if let Ok(attrs) = rec.attributes() {
                if let Some(v) = attrs.iter().find(|a| a.ty == AT_VOLUME_NAME).and_then(|a| a.resident_value()) {
                    fs.label = utf16(v);
                }
            }
        }
        fs.root_modified = fs.read_record(ROOT_RECORD).ok().and_then(|r| r.attributes().ok()).and_then(|a| modified_of(&a));
        Ok(fs)
    }

    fn cs(&self) -> u64 {
        self.boot.cluster_size as u64
    }

    /// Record 0 read straight from the cluster at `lcn` (before the MFT runlist is known).
    fn raw_record(dev: &mut dyn BlockDevice, boot: &NtfsBoot, lcn: u64) -> Result<Record> {
        let at = lcn
            .checked_mul(boot.cluster_size as u64)
            .filter(|a| a.saturating_add(boot.record_size as u64) <= dev.len())
            .ok_or_else(|| Error::Parse("MFT lies beyond the device".into()))?;
        let mut buf = vec![0u8; boot.record_size as usize];
        dev.read_at(at, &mut buf)?;
        let rec = Record::parse(0, buf)?;
        if !rec.in_use() {
            return Err(Error::Parse("$MFT record not in use".into()));
        }
        Ok(rec)
    }

    /// Build the `$MFT` data stream from record 0, following its attribute list (heavily
    /// fragmented MFTs) one extension record at a time, in VCN order.
    fn bootstrap_mft(&mut self, rec0: &Record) -> Result<()> {
        let attrs = rec0.attributes()?;
        let mut extents: Vec<Attr> = attrs.iter().filter(|a| a.ty == AT_DATA && a.name.is_empty()).cloned().collect();
        if extents.is_empty() {
            return Err(Error::Parse("$MFT has no $DATA attribute".into()));
        }
        self.mft = Stream::from_extents(extents.clone())?;
        if let Some(al) = attrs.iter().find(|a| a.ty == AT_ATTRIBUTE_LIST) {
            let list = parse_attr_list(&self.attr_value(al)?)?;
            let mut wanted: Vec<ListEntry> =
                list.into_iter().filter(|e| e.ty == AT_DATA && e.name.is_empty() && e.record != 0).collect();
            wanted.sort_by_key(|e| e.start_vcn);
            for le in wanted {
                let rec = self.read_record(le.record)?;
                let a = rec
                    .attributes()?
                    .into_iter()
                    .find(|a| a.ty == AT_DATA && a.name.is_empty() && a.id == le.id)
                    .ok_or_else(|| Error::Parse(format!("$MFT extent missing from record {}", le.record)))?;
                extents.push(a);
                self.mft = Stream::from_extents(extents.clone())?;
            }
        }
        match self.mft {
            Stream::Resident { .. } => Err(Error::Parse("$MFT $DATA is resident".into())),
            Stream::NonResident { real, alloc, .. } if real > alloc => Err(Error::Parse(format!(
                "$MFT real size {real} exceeds its allocated size {alloc}"
            ))),
            _ => Ok(()),
        }
    }

    /// Records in the MFT: its size, bounded by what its runs map and by the volume, so a
    /// corrupt size field cannot make scans run (practically) forever.
    fn record_count(&self) -> u64 {
        let bytes = self.mft.len().min(self.mft.mapped_bytes(self.cs())).min(self.boot.volume_bytes());
        bytes / self.boot.record_size as u64
    }

    fn read_record(&mut self, n: u64) -> Result<Record> {
        let rs = self.boot.record_size as u64;
        if n >= self.record_count() {
            return Err(Error::NotFound(format!("MFT record {n}")));
        }
        let mut buf = vec![0u8; rs as usize];
        let cs = self.cs();
        let got = read_stream(&mut *self.dev, cs, &self.mft, n * rs, &mut buf)?;
        if got != buf.len() {
            return Err(Error::Parse(format!("MFT record {n} truncated")));
        }
        Record::parse(n, buf)
    }

    /// Full value of a (possibly non-resident) attribute such as `$ATTRIBUTE_LIST`.
    fn attr_value(&mut self, a: &Attr) -> Result<Vec<u8>> {
        if let Some(v) = a.resident_value() {
            return Ok(v.to_vec());
        }
        let s = Stream::from_extents(vec![a.clone()])?;
        if s.len() > MAX_ATTR_LIST {
            return Err(Error::Parse("attribute list too large".into()));
        }
        let mut v = vec![0u8; s.len() as usize];
        let cs = self.cs();
        let n = read_stream(&mut *self.dev, cs, &s, 0, &mut v)?;
        v.truncate(n);
        Ok(v)
    }

    /// Every extent of attribute (`ty`, `name`) of the file whose base record is `rec`,
    /// following `$ATTRIBUTE_LIST` into extension records.
    fn extents(&mut self, rec: &Record, ty: u32, name: &str) -> Result<Vec<Attr>> {
        let attrs = rec.attributes()?;
        let Some(al) = attrs.iter().find(|a| a.ty == AT_ATTRIBUTE_LIST) else {
            return Ok(attrs.into_iter().filter(|a| a.ty == ty && a.name == name).collect());
        };
        let list = parse_attr_list(&self.attr_value(al)?)?;
        let mut wanted: Vec<ListEntry> = list.into_iter().filter(|e| e.ty == ty && e.name == name).collect();
        wanted.sort_by_key(|e| e.start_vcn);
        let mut others: HashMap<u64, Vec<Attr>> = HashMap::new();
        let mut out = Vec::new();
        for le in wanted {
            if le.record != rec.number && !others.contains_key(&le.record) {
                let ext = self.read_record(le.record)?;
                if ext.base_record() != rec.number {
                    return Err(Error::Parse(format!(
                        "record {} is not an extension of record {}",
                        le.record, rec.number
                    )));
                }
                others.insert(le.record, ext.attributes()?);
            }
            let pool = if le.record == rec.number { &attrs } else { &others[&le.record] };
            let a = pool
                .iter()
                .find(|a| a.ty == ty && a.name == name && a.id == le.id)
                .ok_or_else(|| Error::Parse(format!("attribute list entry missing from record {}", le.record)))?;
            out.push(a.clone());
        }
        Ok(out)
    }

    /// The unnamed `$DATA` stream of the file whose base record is `rec`, if any.
    fn data_stream(&mut self, rec: &Record) -> Result<Option<Stream>> {
        let ext = self.extents(rec, AT_DATA, "")?;
        if ext.is_empty() {
            return Ok(None);
        }
        Stream::from_extents(ext).map(Some)
    }

    /// (is_dir, size, modified) of the in-use record referenced by `file_ref`.
    fn describe(&mut self, file_ref: u64) -> Result<(bool, u64, Option<i64>)> {
        let rec = self.read_record(file_ref & REF_MASK)?;
        let seq = (file_ref >> 48) as u16;
        if !rec.in_use() || (seq != 0 && seq != rec.sequence()) {
            return Err(Error::Parse("stale index entry".into()));
        }
        let modified = modified_of(&rec.attributes()?);
        if rec.is_dir() {
            return Ok((true, 0, modified));
        }
        let size = self.data_stream(&rec)?.map_or(0, |s| s.len());
        Ok((false, size, modified))
    }

    /// Path of the directory referenced by `parent_ref`, when every ancestor is an in-use
    /// directory up to the root.
    fn dir_path(&mut self, parent_ref: u64, cache: &mut HashMap<u64, Option<String>>) -> Option<String> {
        if let Some(p) = cache.get(&parent_ref) {
            return p.clone();
        }
        let mut names = Vec::new();
        let mut cur = parent_ref;
        let mut resolved = None;
        for _ in 0..MAX_PATH_DEPTH {
            let n = cur & REF_MASK;
            if n == ROOT_RECORD {
                names.reverse();
                resolved = Some(if names.is_empty() { "/".to_string() } else { format!("/{}", names.join("/")) });
                break;
            }
            let Ok(rec) = self.read_record(n) else { break };
            let seq = (cur >> 48) as u16;
            if !rec.in_use() || !rec.is_dir() || (seq != 0 && seq != rec.sequence()) {
                break;
            }
            let Some(f) = rec.attributes().ok().and_then(|a| best_file_name(&a)) else { break };
            names.push(f.name);
            cur = f.parent;
        }
        cache.insert(parent_ref, resolved.clone());
        resolved
    }

    /// A deleted entry for `rec`, or `None` if it is in use, an extension or nameless.
    fn deleted_entry(&mut self, rec: &Record, cache: &mut HashMap<u64, Option<String>>) -> Option<Entry> {
        if rec.in_use() || rec.base_record() != 0 {
            return None;
        }
        let attrs = rec.attributes().ok()?;
        let f = best_file_name(&attrs)?;
        let data = attrs.iter().filter(|a| a.ty == AT_DATA && a.name.is_empty()).find(|a| a.start_vcn() == 0);
        let size = match data.map(|a| &a.body) {
            Some(AttrBody::Resident(v)) => v.len() as u64,
            Some(AttrBody::NonResident(n)) => n.real,
            None => f.real,
        };
        let path = match self.dir_path(f.parent, cache) {
            Some(dir) => join(&dir, &f.name),
            None => join("/", &f.name),
        };
        Some(Entry {
            is_dir: rec.is_dir() || f.is_dir(),
            size: if rec.is_dir() { 0 } else { size },
            modified: modified_of(&attrs),
            id: rec.number,
            deleted: true,
            attrs: f.flags,
            name: f.name,
            path,
        })
    }
}

fn modified_of(attrs: &[Attr]) -> Option<i64> {
    attrs
        .iter()
        .find(|a| a.ty == AT_STANDARD_INFORMATION)
        .and_then(|a| a.resident_value())
        .and_then(si_modified)
}

impl FileSystem for Ntfs {
    fn kind(&self) -> FsKind {
        FsKind::Ntfs
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn total(&self) -> u64 {
        self.boot.volume_bytes()
    }

    fn cluster_size(&self) -> u32 {
        self.boot.cluster_size
    }

    fn root(&self) -> Entry {
        Entry {
            name: String::new(),
            path: "/".into(),
            is_dir: true,
            size: 0,
            modified: self.root_modified,
            id: ROOT_RECORD,
            deleted: false,
            attrs: FILE_NAME_DIRECTORY | 0x06,
        }
    }

    fn list(&mut self, d: &Entry) -> Result<Vec<Entry>> {
        if !d.is_dir {
            return Err(Error::Invalid(format!("{} is not a directory", d.path)));
        }
        let rec = self.read_record(d.id)?;
        if !rec.is_dir() {
            return Err(Error::Invalid(format!("{} is not a directory", d.path)));
        }
        let root = self.extents(&rec, AT_INDEX_ROOT, "$I30")?;
        let value = root
            .first()
            .and_then(|a| a.resident_value())
            .ok_or_else(|| Error::Parse(format!("{}: no resident $INDEX_ROOT", d.path)))?;
        let (block_size, entries) = index::parse_root(value)?;
        let alloc_ext = self.extents(&rec, AT_INDEX_ALLOCATION, "$I30")?;
        let alloc = if alloc_ext.is_empty() { None } else { Some(Stream::from_extents(alloc_ext)?) };

        let cs = self.cs();
        let bs = block_size as u64;
        let dev = &mut *self.dev;
        let mut read_block = |vcn: u64| -> Result<Vec<index::NodeEntry>> {
            let alloc = alloc.as_ref().ok_or_else(|| Error::Parse("index sub-node without $INDEX_ALLOCATION".into()))?;
            if !bs.is_power_of_two() || !(512..=65536).contains(&bs) {
                return Err(Error::Parse(format!("bad index block size {bs}")));
            }
            // VCNs count clusters, or 512-byte units when blocks are smaller than a cluster
            let unit = if bs >= cs { cs } else { 512 };
            let off = vcn.checked_mul(unit).ok_or_else(|| Error::Parse("index VCN overflow".into()))?;
            let mut buf = vec![0u8; bs as usize];
            if read_stream(dev, cs, alloc, off, &mut buf)? != buf.len() {
                return Err(Error::Parse(format!("index block VCN {vcn} beyond $INDEX_ALLOCATION")));
            }
            index::parse_block(&mut buf, vcn)
        };
        let pairs = index::walk(entries, &mut read_block)?;

        let names: Vec<(u64, FileName)> =
            pairs.into_iter().filter_map(|(r, k)| FileName::parse(&k).ok().map(|f| (r, f))).collect();
        let has_long: HashSet<u64> =
            names.iter().filter(|(_, f)| f.namespace != NS_DOS).map(|(r, _)| r & REF_MASK).collect();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (file_ref, f) in names {
            let n = file_ref & REF_MASK;
            if f.name == "." || (f.namespace == NS_DOS && has_long.contains(&n)) || !seen.insert((n, f.name.clone())) {
                continue;
            }
            let (is_dir, size, modified) = match self.describe(file_ref) {
                Ok(x) => x,
                // unreadable record: fall back to the copy of $FILE_NAME in the index
                Err(_) => (f.is_dir(), if f.is_dir() { 0 } else { f.real }, filetime_to_unix(f.modified)),
            };
            out.push(Entry {
                path: join(&d.path, &f.name),
                name: f.name,
                is_dir,
                size,
                modified,
                id: n,
                deleted: false,
                attrs: f.flags,
            });
        }
        Ok(out)
    }

    fn read_file(&mut self, e: &Entry, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if e.is_dir {
            return Err(Error::Invalid(format!("{} is a directory", e.path)));
        }
        let stream = match self.cache.take() {
            Some((id, s)) if id == e.id => s,
            _ => {
                let rec = self.read_record(e.id)?;
                let s = self
                    .data_stream(&rec)?
                    .ok_or_else(|| Error::NotFound(format!("{}: no unnamed $DATA attribute", e.path)))?;
                if s.flags() & ATTR_ENCRYPTED != 0 {
                    return Err(Error::Unsupported("NTFS encrypted".into()));
                }
                // resident data is never stored compressed, whatever the flag says
                if matches!(s, Stream::NonResident { .. }) && s.flags() & ATTR_COMPRESSION_MASK != 0 {
                    return Err(Error::Unsupported("NTFS compressed".into()));
                }
                s
            }
        };
        let cs = self.cs();
        let r = read_stream(&mut *self.dev, cs, &stream, offset, buf);
        self.cache = Some((e.id, stream));
        r
    }

    fn used_map(&mut self) -> Result<UsedMap> {
        let cs = self.cs();
        let bps = self.boot.bytes_per_sector as u64;
        let total_clusters = self.boot.total_clusters();
        // the backup boot sector sits in the sector after the last one of the volume
        // (clipped to the device when the partition ends right after the volume)
        let len = self.boot.volume_bytes().saturating_add(bps).min(self.dev.len());
        let mut map = UsedMap::new(len, cs as u32);

        let rec = self.read_record(BITMAP_RECORD)?;
        let bitmap = self.data_stream(&rec)?.ok_or_else(|| Error::Parse("$Bitmap has no $DATA".into()))?;
        // bit i of $Bitmap (LSB first) is cluster i: the same layout as UsedMap with unit = cluster.
        // Only bytes inside both the real and the initialized size describe clusters.
        let needed = total_clusters.div_ceil(8);
        let bytes = needed.min(bitmap.len()).min(bitmap.init()).min(map.bits.len() as u64) as usize;
        let mut chunk = vec![0u8; 1 << 20];
        let mut done = 0usize;
        while done < bytes {
            let n = chunk.len().min(bytes - done);
            let got = read_stream(&mut *self.dev, cs, &bitmap, done as u64, &mut chunk[..n])?;
            if got != n {
                return Err(Error::Parse("$Bitmap shorter than its size".into()));
            }
            map.bits[done..done + n].copy_from_slice(&chunk[..n]);
            done += n;
        }
        // bits past the last cluster are padding
        if total_clusters % 8 != 0 && (total_clusters / 8) < bytes as u64 {
            map.bits[(total_clusters / 8) as usize] &= (1u8 << (total_clusters % 8)) - 1;
        }
        // a $Bitmap too short for the volume is damaged: clusters it does not describe are
        // conservatively in use, so a copy never drops data
        let described = bytes as u64 * 8;
        if described < total_clusters {
            map.set(described * cs, (total_clusters - described) * cs);
        }

        // belt and braces: boot area, $MFT and $MFTMirr are always in use
        map.set(0, cs.max(8192));
        let mark = |map: &mut UsedMap, s: &Stream| {
            if let Stream::NonResident { runs, .. } = s {
                for r in runs {
                    if let Some(lcn) = r.lcn {
                        map.set(lcn.saturating_mul(cs), r.len.saturating_mul(cs));
                    }
                }
            }
        };
        mark(&mut map, &self.mft);
        if let Ok(Some(mirr)) = self.read_record(MFTMIRR_RECORD).and_then(|r| self.data_stream(&r)) {
            mark(&mut map, &mirr);
        }
        map.set(self.boot.total_sectors * bps, bps);
        Ok(map)
    }

    fn deleted_entries(&mut self, progress: &dyn ProgressSink, cancel: &CancelToken) -> Result<Vec<Entry>> {
        let rs = self.boot.record_size as u64;
        let count = self.record_count();
        let cs = self.cs();
        let mut out = Vec::new();
        let mut paths: HashMap<u64, Option<String>> = HashMap::new();
        let report = |done: u64| {
            progress.report(&ProgressReport {
                phase: "Scanning MFT".into(),
                done,
                total: count,
                ..Default::default()
            })
        };
        let mut first = 0u64;
        while first < count {
            cancel.check()?;
            report(first);
            let n = SCAN_BATCH.min(count - first);
            let mut buf = vec![0u8; (n * rs) as usize];
            // one bad run must not end the scan: fall back to record-by-record reads
            let batch_ok = read_stream(&mut *self.dev, cs, &self.mft, first * rs, &mut buf).is_ok();
            for k in 0..n {
                let num = first + k;
                let raw = if batch_ok {
                    buf[(k * rs) as usize..((k + 1) * rs) as usize].to_vec()
                } else {
                    let mut one = vec![0u8; rs as usize];
                    if read_stream(&mut *self.dev, cs, &self.mft, num * rs, &mut one).is_err() {
                        continue;
                    }
                    one
                };
                if &raw[..4] != b"FILE" {
                    continue;
                }
                let Ok(rec) = Record::parse(num, raw) else { continue };
                if let Some(e) = self.deleted_entry(&rec, &mut paths) {
                    out.push(e);
                }
            }
            first += n;
        }
        report(count);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(start: u64, end: u64, runs: Vec<(Option<u64>, u64)>) -> Attr {
        Attr {
            ty: AT_DATA,
            name: String::new(),
            flags: 0,
            id: 0,
            body: AttrBody::NonResident(NonResident {
                start_vcn: start,
                end_vcn: end,
                runs,
                comp_unit: 0,
                alloc: 4 * 4096,
                real: 4 * 4096,
                init: 4 * 4096,
            }),
        }
    }

    #[test]
    fn extents_must_tile_their_vcn_ranges() {
        // intra-extent sparse runs are fine
        let ok = Stream::from_extents(vec![extent(2, 3, vec![(None, 1), (Some(9), 1)]), extent(0, 1, vec![(Some(5), 2)])]);
        assert_eq!(ok.unwrap().mapped_bytes(4096), 4 * 4096);
        // an empty non-resident attribute has end_vcn = -1 and no runs
        assert!(Stream::from_extents(vec![extent(0, u64::MAX, vec![])]).is_ok());
        let bad = |v: Vec<Attr>| matches!(Stream::from_extents(v), Err(Error::Parse(_)));
        // runlist shorter / longer than the declared range
        assert!(bad(vec![extent(0, 3, vec![(Some(5), 2)])]));
        assert!(bad(vec![extent(0, 0, vec![(Some(5), 2)])]));
        // gap and overlap between extents, inverted range
        assert!(bad(vec![extent(0, 1, vec![(Some(5), 2)]), extent(3, 3, vec![(Some(9), 1)])]));
        assert!(bad(vec![extent(0, 1, vec![(Some(5), 2)]), extent(1, 1, vec![(Some(9), 1)])]));
        assert!(bad(vec![extent(0, 1, vec![(Some(5), 2)]), extent(3, 1, vec![])]));
    }
}
