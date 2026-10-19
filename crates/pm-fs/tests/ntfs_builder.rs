//! Test-only synthetic NTFS image builder. It writes boot sector, MFT records (with
//! update-sequence fixups), index blocks and the cluster bitmap byte by byte, so the reader
//! under test is never used to construct the images it is tested against.
//!
//! Layout of the default image (4 KiB clusters, 1 KiB records, 16 MiB):
//! - LCN 0-1 `$Boot`, LCN 2 `$MFTMirr`, LCN 16.. `$MFT` (32 records; optionally records
//!   8.. in a second run), LCN 30 `$Bitmap`, LCN 40-41 the root `$INDEX_ALLOCATION`
//!   (two INDX blocks below a root node), then file data (see the `LCN_*` constants).
//! - Records 0..=15 are minimal system files; 16.. are the test files (`REC_*`).
#![allow(dead_code)]

pub const MIB: u64 = 1 << 20;

pub const MFT_RECORDS: u64 = 32;
pub const ROOT: u64 = 5;

pub const REC_HELLO: u64 = 16;
pub const REC_BIG: u64 = 17;
pub const REC_SPLIT: u64 = 18;
pub const REC_SPLIT_EXT1: u64 = 19;
pub const REC_GONE: u64 = 20;
pub const REC_DOCS: u64 = 21;
pub const REC_README: u64 = 22;
pub const REC_LONG: u64 = 23;
pub const REC_SPARSE: u64 = 24;
pub const REC_COMP: u64 = 25;
pub const REC_OLD: u64 = 26;
pub const REC_SPLIT_EXT2: u64 = 27;
pub const REC_GHOST: u64 = 28;
pub const REC_ENC: u64 = 29;
pub const REC_SPLIT_EXT3: u64 = 30;

pub const LCN_MFTMIRR: u64 = 2;
pub const LCN_BITMAP: u64 = 30;
pub const LCN_ROOT_INDX: u64 = 40;
pub const LCN_BIG_A: u64 = 50;
pub const LCN_BIG_B: u64 = 200;
pub const LCN_SPLIT_1: u64 = 300;
pub const LCN_SPLIT_2: u64 = 310;
pub const LCN_SPLIT_3: u64 = 320;
pub const LCN_SPARSE_A: u64 = 400;
pub const LCN_SPARSE_B: u64 = 401;
pub const LCN_COMP: u64 = 500;
pub const LCN_ENC: u64 = 501;
/// Marked used in `$Bitmap` without belonging to any file.
pub const LCN_ORPHAN: u64 = 3000;

pub const HELLO: &[u8; 20] = b"Hello, NTFS world!\r\n";
pub const BIG_SIZE: u64 = 4 * 4096 + 1000;
pub const SPLIT_SIZE: u64 = 2 * 4096 + 1808;
pub const SPARSE_SIZE: u64 = 4 * 4096;
pub const SPARSE_INIT: u64 = 3 * 4096 + 100;
pub const GONE_DATA: &[u8] = b"bye bye";
pub const README_DATA: &[u8] = b"read me\n";
pub const OLD_DATA: &[u8] = b"old\n";
/// `$STANDARD_INFORMATION` modification time of every record (unix seconds).
pub const MODIFIED_UNIX: i64 = 1_700_000_000;
/// The (deliberately different) times stored in `$FILE_NAME` attributes.
pub const FN_TIME_UNIX: i64 = 1_600_000_000;

pub const FILE_ATTR_ARCHIVE: u32 = 0x20;
pub const FILE_NAME_DIR: u32 = 0x1000_0000;

const AT_SI: u32 = 0x10;
const AT_AL: u32 = 0x20;
const AT_FN: u32 = 0x30;
const AT_VOLUME_NAME: u32 = 0x60;
const AT_VOLUME_INFO: u32 = 0x70;
const AT_DATA: u32 = 0x80;
const AT_IR: u32 = 0x90;
const AT_IA: u32 = 0xA0;
const AT_BITMAP: u32 = 0xB0;

pub struct NtfsOpts {
    pub cluster_size: u32,
    pub record_size: u32,
    pub total_size: u64,
    pub mft_lcn: u64,
    /// When set, MFT records 8.. live in a second run starting at this LCN.
    pub mft_second_lcn: Option<u64>,
    /// Override the real / initialized size of the `$Bitmap` `$DATA` (corruption tests).
    pub bitmap_real: Option<u64>,
    pub bitmap_init: Option<u64>,
    /// Override the real / allocated size of the `$MFT` `$DATA` (corruption tests).
    pub mft_real: Option<u64>,
    pub mft_alloc: Option<u64>,
    /// Leave the middle `$DATA` extent of split.dat out of its attribute list.
    pub split_drop_middle_extent: bool,
}

impl Default for NtfsOpts {
    fn default() -> Self {
        NtfsOpts {
            cluster_size: 4096,
            record_size: 1024,
            total_size: 16 * MIB,
            mft_lcn: 16,
            mft_second_lcn: None,
            bitmap_real: None,
            bitmap_init: None,
            mft_real: None,
            mft_alloc: None,
            split_drop_middle_extent: false,
        }
    }
}

pub struct Built {
    pub image: Vec<u8>,
    /// Clusters set in `$Bitmap`, ascending.
    pub used_clusters: Vec<u64>,
    pub total_sectors: u64,
    /// Root directory entries in index (collation) order, including "." and DOS names.
    pub root_index_names: Vec<String>,
}

pub fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
pub fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Deterministic per-file byte pattern; every byte offset differs from its neighbours'
/// clusters so misplaced clusters are detected.
pub fn pattern(seed: u8, i: u64) -> u8 {
    let x = (i as u32).wrapping_mul(2_654_435_761).rotate_right(13) ^ (i >> 12) as u32;
    (x as u8) ^ seed
}

pub fn expected_big() -> Vec<u8> {
    (0..BIG_SIZE).map(|i| pattern(1, i)).collect()
}

pub fn expected_split() -> Vec<u8> {
    (0..SPLIT_SIZE).map(|i| pattern(2, i)).collect()
}

pub fn expected_sparse() -> Vec<u8> {
    (0..SPARSE_SIZE)
        .map(|i| {
            let vcn = i / 4096;
            if i >= SPARSE_INIT || vcn == 1 || vcn == 2 {
                0
            } else {
                pattern(3, i)
            }
        })
        .collect()
}

fn filetime(unix: i64) -> u64 {
    ((unix + 11_644_473_600) as u64) * 10_000_000
}

fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// Minimal little-endian two's-complement encoding of `v`.
fn signed_bytes(v: i64) -> Vec<u8> {
    let b = v.to_le_bytes();
    for n in 1..=8usize {
        let sh = 64 - 8 * n as u32;
        let mut a = [0u8; 8];
        a[..n].copy_from_slice(&b[..n]);
        let x = if sh == 0 { i64::from_le_bytes(a) } else { (i64::from_le_bytes(a) << sh) >> sh };
        if x == v {
            return b[..n].to_vec();
        }
    }
    unreachable!()
}

/// Encode a runlist: `(Some(lcn), len)` for allocated runs, `(None, len)` for sparse runs.
pub fn encode_runs(runs: &[(Option<u64>, u64)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev = 0i64;
    for &(lcn, len) in runs {
        let lb = signed_bytes(len as i64);
        match lcn {
            Some(l) => {
                let ob = signed_bytes(l as i64 - prev);
                out.push(((ob.len() << 4) | lb.len()) as u8);
                out.extend_from_slice(&lb);
                out.extend_from_slice(&ob);
                prev = l as i64;
            }
            None => {
                out.push(lb.len() as u8);
                out.extend_from_slice(&lb);
            }
        }
    }
    out.push(0);
    out
}

/// Write an update sequence number at `usa_off` and move every 512-byte sector's last two
/// bytes into the array.
fn apply_fixups(buf: &mut [u8], usa_off: usize, usn: u16) {
    let n = buf.len() / 512;
    put16(buf, 6, (n + 1) as u16);
    put16(buf, usa_off, usn);
    for i in 0..n {
        let end = i * 512 + 510;
        let orig = [buf[end], buf[end + 1]];
        buf[usa_off + 2 + 2 * i..usa_off + 4 + 2 * i].copy_from_slice(&orig);
        put16(buf, end, usn);
    }
}

fn resident(ty: u32, name: &str, value: &[u8]) -> Vec<u8> {
    let n = utf16(name);
    let voff = align8(0x18 + n.len());
    let len = align8(voff + value.len());
    let mut a = vec![0u8; len];
    put32(&mut a, 0, ty);
    put32(&mut a, 4, len as u32);
    a[8] = 0;
    a[9] = (n.len() / 2) as u8;
    put16(&mut a, 10, 0x18);
    put32(&mut a, 0x10, value.len() as u32);
    put16(&mut a, 0x14, voff as u16);
    a[0x18..0x18 + n.len()].copy_from_slice(&n);
    a[voff..voff + value.len()].copy_from_slice(value);
    a
}

struct NonRes<'a> {
    ty: u32,
    name: &'a str,
    flags: u16,
    start_vcn: u64,
    end_vcn: u64,
    runs: Vec<(Option<u64>, u64)>,
    alloc: u64,
    real: u64,
    init: u64,
    comp_unit: u16,
}

fn nonresident(p: &NonRes) -> Vec<u8> {
    let n = utf16(p.name);
    let hdr = if p.flags & 0x0001 != 0 { 0x48 } else { 0x40 };
    let roff = align8(hdr + n.len());
    let runs = encode_runs(&p.runs);
    let len = align8(roff + runs.len());
    let mut a = vec![0u8; len];
    put32(&mut a, 0, p.ty);
    put32(&mut a, 4, len as u32);
    a[8] = 1;
    a[9] = (n.len() / 2) as u8;
    put16(&mut a, 10, hdr as u16);
    put16(&mut a, 12, p.flags);
    put64(&mut a, 0x10, p.start_vcn);
    put64(&mut a, 0x18, p.end_vcn);
    put16(&mut a, 0x20, roff as u16);
    put16(&mut a, 0x22, p.comp_unit);
    put64(&mut a, 0x28, p.alloc);
    put64(&mut a, 0x30, p.real);
    put64(&mut a, 0x38, p.init);
    if hdr == 0x48 {
        put64(&mut a, 0x40, p.alloc);
    }
    a[hdr..hdr + n.len()].copy_from_slice(&n);
    a[roff..roff + runs.len()].copy_from_slice(&runs);
    a
}

/// A simple contiguous/fragmented non-resident attribute starting at VCN 0.
fn nr_data(name: &str, runs: Vec<(Option<u64>, u64)>, real: u64, init: u64, cs: u64) -> Vec<u8> {
    let clusters: u64 = runs.iter().map(|r| r.1).sum();
    nonresident(&NonRes {
        ty: AT_DATA,
        name,
        flags: 0,
        start_vcn: 0,
        end_vcn: clusters.saturating_sub(1),
        runs,
        alloc: clusters * cs,
        real,
        init,
        comp_unit: 0,
    })
}

fn si_value() -> Vec<u8> {
    let mut v = vec![0u8; 0x48];
    let t = filetime(MODIFIED_UNIX);
    put64(&mut v, 0, t);
    put64(&mut v, 8, t);
    put64(&mut v, 0x10, t);
    put64(&mut v, 0x18, t);
    put32(&mut v, 0x20, FILE_ATTR_ARCHIVE);
    v
}

fn si() -> Vec<u8> {
    resident(AT_SI, "", &si_value())
}

/// `$FILE_NAME` value.
fn fn_value(parent_ref: u64, name: &str, flags: u32, real: u64, namespace: u8) -> Vec<u8> {
    let n = utf16(name);
    let mut v = vec![0u8; 0x42 + n.len()];
    put64(&mut v, 0, parent_ref);
    let t = filetime(FN_TIME_UNIX);
    for o in [8, 0x10, 0x18, 0x20] {
        put64(&mut v, o, t);
    }
    put64(&mut v, 0x28, real.div_ceil(4096) * 4096);
    put64(&mut v, 0x30, real);
    put32(&mut v, 0x38, flags);
    v[0x40] = (n.len() / 2) as u8;
    v[0x41] = namespace;
    v[0x42..].copy_from_slice(&n);
    v
}

fn fname(parent_ref: u64, name: &str, flags: u32, real: u64, namespace: u8) -> Vec<u8> {
    resident(AT_FN, "", &fn_value(parent_ref, name, flags, real, namespace))
}

/// An index entry carrying a `$FILE_NAME` key (`key: None` = the terminating entry).
fn index_entry(file_ref: u64, key: Option<&[u8]>, child: Option<u64>) -> Vec<u8> {
    let klen = key.map_or(0, |k| k.len());
    let mut len = align8(0x10 + klen);
    if child.is_some() {
        len += 8;
    }
    let mut e = vec![0u8; len];
    put64(&mut e, 0, if key.is_some() { file_ref } else { 0 });
    put16(&mut e, 8, len as u16);
    put16(&mut e, 10, klen as u16);
    let mut flags = 0u16;
    if child.is_some() {
        flags |= 1;
    }
    if key.is_none() {
        flags |= 2;
    }
    put16(&mut e, 12, flags);
    if let Some(k) = key {
        e[0x10..0x10 + k.len()].copy_from_slice(k);
    }
    if let Some(vcn) = child {
        put64(&mut e, len - 8, vcn);
    }
    e
}

fn index_root_value(entries: &[u8], large: bool, block_size: u32, cs: u32) -> Vec<u8> {
    let mut v = vec![0u8; 0x20 + entries.len()];
    put32(&mut v, 0, AT_FN);
    put32(&mut v, 4, 1); // COLLATION_FILE_NAME
    put32(&mut v, 8, block_size);
    v[12] = (block_size / cs).max(1) as u8;
    put32(&mut v, 0x10, 0x10);
    put32(&mut v, 0x14, (0x10 + entries.len()) as u32);
    put32(&mut v, 0x18, (0x10 + entries.len()) as u32);
    v[0x1C] = large as u8;
    v[0x20..].copy_from_slice(entries);
    v
}

fn indx_block(vcn: u64, entries: &[u8], size: usize) -> Vec<u8> {
    let mut b = vec![0u8; size];
    b[..4].copy_from_slice(b"INDX");
    let usa_off = 0x28;
    put16(&mut b, 4, usa_off as u16);
    put64(&mut b, 0x10, vcn);
    let usa_end = usa_off + 2 * (size / 512 + 1);
    let eoff = align8(usa_end) - 0x18;
    put32(&mut b, 0x18, eoff as u32);
    put32(&mut b, 0x1C, (eoff + entries.len()) as u32);
    put32(&mut b, 0x20, (size - 0x18) as u32);
    b[0x18 + eoff..0x18 + eoff + entries.len()].copy_from_slice(entries);
    apply_fixups(&mut b, usa_off, 0x2A2A);
    b
}

fn ref_of(rec: u64, seq: u16) -> u64 {
    rec | (seq as u64) << 48
}

fn seq_of(rec: u64) -> u16 {
    if rec < 16 {
        rec.max(1) as u16
    } else {
        1
    }
}

fn fref(rec: u64) -> u64 {
    ref_of(rec, seq_of(rec))
}

/// One MFT record image (fixups applied). Attribute ids are assigned in order.
fn record(rs: usize, num: u64, seq: u16, flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
    let mut r = vec![0u8; rs];
    r[..4].copy_from_slice(b"FILE");
    let usa_off = 0x30;
    put16(&mut r, 4, usa_off as u16);
    put16(&mut r, 0x10, seq);
    put16(&mut r, 0x12, 1);
    let first = align8(usa_off + 2 * (rs / 512 + 1));
    put16(&mut r, 0x14, first as u16);
    put16(&mut r, 0x16, flags);
    put32(&mut r, 0x1C, rs as u32);
    put64(&mut r, 0x20, base);
    put16(&mut r, 0x28, attrs.len() as u16);
    put32(&mut r, 0x2C, num as u32);
    let mut off = first;
    for (id, a) in attrs.iter().enumerate() {
        assert!(off + a.len() + 8 <= rs, "record {num} overflows");
        r[off..off + a.len()].copy_from_slice(a);
        put16(&mut r, off + 14, id as u16);
        off += a.len();
    }
    put32(&mut r, off, 0xFFFF_FFFF);
    put32(&mut r, 0x18, (off + 8) as u32);
    apply_fixups(&mut r, usa_off, 0x0101 + num as u16);
    r
}

fn upcase_key(name: &str) -> Vec<u16> {
    name.to_uppercase().encode_utf16().collect()
}

struct Img {
    data: Vec<u8>,
    cs: u64,
    rs: u64,
    bitmap: Vec<u8>,
    clusters: u64,
    mft_runs: Vec<(Option<u64>, u64)>,
}

impl Img {
    fn write(&mut self, off: u64, b: &[u8]) {
        self.data[off as usize..off as usize + b.len()].copy_from_slice(b);
    }
    fn use_clusters(&mut self, lcn: u64, n: u64) {
        for c in lcn..lcn + n {
            assert!(c < self.clusters);
            self.bitmap[(c / 8) as usize] |= 1 << (c % 8);
        }
    }
    /// Fill clusters with `pattern(seed, file_offset)` where cluster `lcn` holds file VCN `vcn`.
    fn fill(&mut self, lcn: u64, vcn: u64, n: u64, seed: u8) {
        let cs = self.cs;
        for k in 0..n {
            let buf: Vec<u8> = (0..cs).map(|i| pattern(seed, (vcn + k) * cs + i)).collect();
            self.write((lcn + k) * cs, &buf);
        }
    }
    fn record_offset(&self, num: u64) -> u64 {
        let mut vcn_byte = num * self.rs;
        for &(lcn, len) in &self.mft_runs {
            let run_bytes = len * self.cs;
            if vcn_byte < run_bytes {
                return lcn.unwrap() * self.cs + vcn_byte;
            }
            vcn_byte -= run_bytes;
        }
        panic!("record {num} outside the MFT");
    }
    fn put_record(&mut self, num: u64, rec: Vec<u8>) {
        let off = self.record_offset(num);
        self.write(off, &rec);
    }
}

pub fn build(opts: &NtfsOpts) -> Built {
    assert_eq!(opts.cluster_size, 4096, "the LCN layout assumes 4 KiB clusters");
    let cs = opts.cluster_size as u64;
    let rs = opts.record_size as u64;
    let rsz = rs as usize;
    let total_sectors = opts.total_size / 512 - 1;
    let clusters = total_sectors * 512 / cs;
    let mft_clusters = MFT_RECORDS * rs / cs;
    let mft_runs = match opts.mft_second_lcn {
        None => vec![(Some(opts.mft_lcn), mft_clusters)],
        Some(second) => {
            let first = 8 * rs / cs;
            vec![(Some(opts.mft_lcn), first), (Some(second), mft_clusters - first)]
        }
    };
    let mut img = Img {
        data: vec![0u8; opts.total_size as usize],
        cs,
        rs,
        bitmap: vec![0u8; clusters.div_ceil(8) as usize],
        clusters,
        mft_runs: mft_runs.clone(),
    };
    if opts.mft_second_lcn.is_some() {
        // garbage where a contiguous MFT would continue: the reader must follow the runs
        let gap_start = (opts.mft_lcn + mft_runs[0].1) * cs;
        let gap = vec![0xFFu8; ((mft_clusters - mft_runs[0].1) * cs) as usize];
        img.write(gap_start, &gap);
    }

    // ---- boot sector (and its backup in the last sector) ----
    let mut boot = vec![0u8; 512];
    boot[..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    boot[3..11].copy_from_slice(b"NTFS    ");
    put16(&mut boot, 0x0B, 512);
    boot[0x0D] = (cs / 512) as u8;
    boot[0x15] = 0xF8;
    put16(&mut boot, 0x18, 63);
    put16(&mut boot, 0x1A, 255);
    put32(&mut boot, 0x24, 0x0080_0080);
    put64(&mut boot, 0x28, total_sectors);
    put64(&mut boot, 0x30, opts.mft_lcn);
    put64(&mut boot, 0x38, LCN_MFTMIRR);
    let per = |size: u64| -> u8 {
        if size >= cs {
            (size / cs) as u8
        } else {
            (-(size.trailing_zeros() as i8)) as u8
        }
    };
    boot[0x40] = per(rs);
    boot[0x44] = per(4096);
    put64(&mut boot, 0x48, 0x1122_3344_5566_7788);
    boot[0x1FE] = 0x55;
    boot[0x1FF] = 0xAA;
    img.write(0, &boot);
    img.write(total_sectors * 512, &boot);

    // ---- cluster usage ----
    img.use_clusters(0, 2); // $Boot
    img.use_clusters(LCN_MFTMIRR, 1);
    for &(lcn, len) in &mft_runs {
        img.use_clusters(lcn.unwrap(), len);
    }
    img.use_clusters(LCN_BITMAP, 1);
    img.use_clusters(LCN_ROOT_INDX, 2);
    img.use_clusters(LCN_BIG_A, 3);
    img.use_clusters(LCN_BIG_B, 2);
    img.use_clusters(LCN_SPLIT_1, 1);
    img.use_clusters(LCN_SPLIT_2, 1);
    img.use_clusters(LCN_SPLIT_3, 1);
    img.use_clusters(LCN_SPARSE_A, 1);
    img.use_clusters(LCN_SPARSE_B, 1);
    img.use_clusters(LCN_COMP, 1);
    img.use_clusters(LCN_ENC, 1);
    img.use_clusters(LCN_ORPHAN, 1);

    // ---- file data ----
    img.fill(LCN_BIG_A, 0, 3, 1);
    img.fill(LCN_BIG_B, 3, 2, 1);
    img.fill(LCN_SPLIT_1, 0, 1, 2);
    img.fill(LCN_SPLIT_2, 1, 1, 2);
    img.fill(LCN_SPLIT_3, 2, 1, 2);
    img.fill(LCN_SPARSE_A, 0, 1, 3);
    img.fill(LCN_SPARSE_B, 3, 1, 3); // VCN 3; bytes past SPARSE_INIT must still read as zeros
    img.fill(LCN_COMP, 0, 1, 4);
    img.fill(LCN_ENC, 0, 1, 5);

    let root_ref = fref(ROOT);
    let docs_ref = fref(REC_DOCS);
    const SYS: u32 = 0x06; // hidden | system
    const NS_POSIX: u8 = 0;
    const NS_WIN32: u8 = 1;
    const NS_DOS: u8 = 2;
    const NS_BOTH: u8 = 3;
    const IN_USE: u16 = 1;
    const DIR: u16 = 2;

    // ---- root index content: (record, name, flags, real size stored in the index, namespace) ----
    let mut root_entries: Vec<(u64, String, u32, u64, u8)> = vec![
        (0, "$MFT".into(), SYS, MFT_RECORDS * rs, NS_BOTH),
        (1, "$MFTMirr".into(), SYS, cs, NS_BOTH),
        (2, "$LogFile".into(), SYS, 0, NS_BOTH),
        (3, "$Volume".into(), SYS, 0, NS_BOTH),
        (4, "$AttrDef".into(), SYS, 0, NS_BOTH),
        (5, ".".into(), SYS | FILE_NAME_DIR, 0, NS_BOTH),
        (6, "$Bitmap".into(), SYS, clusters.div_ceil(8), NS_BOTH),
        (7, "$Boot".into(), SYS, 8192, NS_BOTH),
        (8, "$BadClus".into(), SYS, 0, NS_BOTH),
        (9, "$Secure".into(), SYS, 0, NS_BOTH),
        (10, "$UpCase".into(), SYS, 0, NS_BOTH),
        (11, "$Extend".into(), SYS | FILE_NAME_DIR, 0, NS_BOTH),
        (REC_HELLO, "hello.txt".into(), FILE_ATTR_ARCHIVE, HELLO.len() as u64, NS_BOTH),
        // stale size in the index: the reader must take the size from the record
        (REC_BIG, "big.bin".into(), FILE_ATTR_ARCHIVE, 0, NS_BOTH),
        (REC_SPLIT, "split.dat".into(), FILE_ATTR_ARCHIVE, SPLIT_SIZE, NS_BOTH),
        (REC_DOCS, "docs".into(), FILE_NAME_DIR, 0, NS_BOTH),
        (REC_LONG, "Document Long Name.txt".into(), FILE_ATTR_ARCHIVE, 5, NS_WIN32),
        (REC_LONG, "DOCUME~1.TXT".into(), FILE_ATTR_ARCHIVE, 5, NS_DOS),
        (REC_SPARSE, "sparse.bin".into(), FILE_ATTR_ARCHIVE | 0x200, SPARSE_SIZE, NS_BOTH),
        (REC_COMP, "comp.bin".into(), FILE_ATTR_ARCHIVE | 0x800, cs, NS_POSIX),
        (REC_ENC, "enc.bin".into(), FILE_ATTR_ARCHIVE | 0x4000, 100, NS_BOTH),
    ];
    root_entries.sort_by_key(|e| upcase_key(&e.1));
    let root_index_names: Vec<String> = root_entries.iter().map(|e| e.1.clone()).collect();
    let key_of = |e: &(u64, String, u32, u64, u8)| fn_value(root_ref, &e.1, e.2, e.3, e.4);

    // B-tree: [lower half] <- root key -> [upper half]
    let mid = root_entries.len() / 2;
    let mut leaf0 = Vec::new();
    for e in &root_entries[..mid] {
        leaf0.extend(index_entry(fref(e.0), Some(&key_of(e)), None));
    }
    leaf0.extend(index_entry(0, None, None));
    let mut leaf1 = Vec::new();
    for e in &root_entries[mid + 1..] {
        leaf1.extend(index_entry(fref(e.0), Some(&key_of(e)), None));
    }
    leaf1.extend(index_entry(0, None, None));
    let mut root_node = index_entry(fref(root_entries[mid].0), Some(&key_of(&root_entries[mid])), Some(0));
    root_node.extend(index_entry(0, None, Some(1)));
    img.write(LCN_ROOT_INDX * cs, &indx_block(0, &leaf0, 4096));
    img.write((LCN_ROOT_INDX + 1) * cs, &indx_block(1, &leaf1, 4096));

    // ---- MFT records ----
    let mut in_use = vec![0u8; 8];
    let mut recs: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut add = |num: u64, seq: u16, flags: u16, base: u64, attrs: Vec<Vec<u8>>, in_use: &mut Vec<u8>| {
        if flags & IN_USE != 0 {
            in_use[(num / 8) as usize] |= 1 << (num % 8);
        }
        recs.push((num, record(rsz, num, seq, flags, base, &attrs)));
    };

    let sys = |name: &str, real: u64| fname(root_ref, name, SYS, real, NS_BOTH);
    let empty_data = || resident(AT_DATA, "", &[]);

    add(
        1,
        1,
        IN_USE,
        0,
        vec![si(), sys("$MFTMirr", cs), nr_data("", vec![(Some(LCN_MFTMIRR), 1)], cs, cs, cs)],
        &mut in_use,
    );
    add(2, 2, IN_USE, 0, vec![si(), sys("$LogFile", 0), empty_data()], &mut in_use);
    let mut volinfo = vec![0u8; 12];
    volinfo[8] = 3;
    volinfo[9] = 1;
    add(
        3,
        3,
        IN_USE,
        0,
        vec![
            si(),
            sys("$Volume", 0),
            resident(AT_VOLUME_NAME, "", &utf16("TESTVOL")),
            resident(AT_VOLUME_INFO, "", &volinfo),
            empty_data(),
        ],
        &mut in_use,
    );
    add(4, 4, IN_USE, 0, vec![si(), sys("$AttrDef", 0), empty_data()], &mut in_use);
    add(
        ROOT,
        5,
        IN_USE | DIR,
        0,
        vec![
            si(),
            fname(root_ref, ".", SYS | FILE_NAME_DIR, 0, NS_BOTH),
            resident(AT_IR, "$I30", &index_root_value(&root_node, true, 4096, cs as u32)),
            nonresident(&NonRes {
                ty: AT_IA,
                name: "$I30",
                flags: 0,
                start_vcn: 0,
                end_vcn: 1,
                runs: vec![(Some(LCN_ROOT_INDX), 2)],
                alloc: 2 * cs,
                real: 2 * cs,
                init: 2 * cs,
                comp_unit: 0,
            }),
            resident(AT_BITMAP, "$I30", &[0x03, 0, 0, 0, 0, 0, 0, 0]),
        ],
        &mut in_use,
    );
    let bm_len = clusters.div_ceil(8);
    add(
        6,
        6,
        IN_USE,
        0,
        vec![
            si(),
            sys("$Bitmap", bm_len),
            nr_data(
                "",
                vec![(Some(LCN_BITMAP), 1)],
                opts.bitmap_real.unwrap_or(bm_len),
                opts.bitmap_init.unwrap_or(opts.bitmap_real.unwrap_or(bm_len)),
                cs,
            ),
        ],
        &mut in_use,
    );
    add(7, 7, IN_USE, 0, vec![si(), sys("$Boot", 8192), nr_data("", vec![(Some(0), 2)], 8192, 8192, cs)], &mut in_use);
    add(
        8,
        8,
        IN_USE,
        0,
        vec![
            si(),
            sys("$BadClus", 0),
            empty_data(),
            // a named sparse stream covering the volume; never confused with the unnamed one
            nonresident(&NonRes {
                ty: AT_DATA,
                name: "$Bad",
                flags: 0x8000,
                start_vcn: 0,
                end_vcn: clusters - 1,
                runs: vec![(None, clusters)],
                alloc: clusters * cs,
                real: clusters * cs,
                init: clusters * cs,
                comp_unit: 0,
            }),
        ],
        &mut in_use,
    );
    add(9, 9, IN_USE, 0, vec![si(), sys("$Secure", 0), empty_data()], &mut in_use);
    add(10, 10, IN_USE, 0, vec![si(), sys("$UpCase", 0), empty_data()], &mut in_use);
    let empty_root = index_root_value(&index_entry(0, None, None), false, 4096, cs as u32);
    add(
        11,
        11,
        IN_USE | DIR,
        0,
        vec![si(), fname(root_ref, "$Extend", SYS | FILE_NAME_DIR, 0, NS_BOTH), resident(AT_IR, "$I30", &empty_root)],
        &mut in_use,
    );
    for n in 12..16 {
        add(n, n as u16, IN_USE, 0, vec![si()], &mut in_use);
    }

    // ---- test files ----
    add(
        REC_HELLO,
        1,
        IN_USE,
        0,
        vec![
            si(),
            fname(root_ref, "hello.txt", FILE_ATTR_ARCHIVE, HELLO.len() as u64, NS_BOTH),
            resident(AT_DATA, "", HELLO),
        ],
        &mut in_use,
    );
    add(
        REC_BIG,
        1,
        IN_USE,
        0,
        vec![
            si(),
            fname(root_ref, "big.bin", FILE_ATTR_ARCHIVE, BIG_SIZE, NS_BOTH),
            nr_data("", vec![(Some(LCN_BIG_A), 3), (Some(LCN_BIG_B), 2)], BIG_SIZE, BIG_SIZE, cs),
        ],
        &mut in_use,
    );

    // split.dat: base record with an attribute list; $DATA split over three extension records
    let split_fn = fname(root_ref, "split.dat", FILE_ATTR_ARCHIVE, SPLIT_SIZE, NS_BOTH);
    let mut al = Vec::new();
    let al_entry = |ty: u32, vcn: u64, rec: u64, id: u16| {
        let mut e = vec![0u8; 0x20];
        put32(&mut e, 0, ty);
        put16(&mut e, 4, 0x20);
        e[6] = 0;
        e[7] = 0x1A;
        put64(&mut e, 8, vcn);
        put64(&mut e, 0x10, fref(rec));
        put16(&mut e, 0x18, id);
        e
    };
    al.extend(al_entry(AT_SI, 0, REC_SPLIT, 0));
    al.extend(al_entry(AT_FN, 0, REC_SPLIT, 2));
    al.extend(al_entry(AT_DATA, 0, REC_SPLIT_EXT1, 0));
    if !opts.split_drop_middle_extent {
        al.extend(al_entry(AT_DATA, 1, REC_SPLIT_EXT2, 0));
    }
    al.extend(al_entry(AT_DATA, 2, REC_SPLIT_EXT3, 0));
    add(REC_SPLIT, 1, IN_USE, 0, vec![si(), resident(AT_AL, "", &al), split_fn], &mut in_use);
    add(
        REC_SPLIT_EXT1,
        1,
        IN_USE,
        fref(REC_SPLIT),
        vec![nonresident(&NonRes {
            ty: AT_DATA,
            name: "",
            flags: 0,
            start_vcn: 0,
            end_vcn: 0,
            runs: vec![(Some(LCN_SPLIT_1), 1)],
            alloc: 3 * cs,
            real: SPLIT_SIZE,
            init: SPLIT_SIZE,
            comp_unit: 0,
        })],
        &mut in_use,
    );
    add(
        REC_SPLIT_EXT2,
        1,
        IN_USE,
        fref(REC_SPLIT),
        vec![nonresident(&NonRes {
            ty: AT_DATA,
            name: "",
            flags: 0,
            start_vcn: 1,
            end_vcn: 1,
            runs: vec![(Some(LCN_SPLIT_2), 1)],
            // sizes are only meaningful in the first extent
            alloc: 0,
            real: 0,
            init: 0,
            comp_unit: 0,
        })],
        &mut in_use,
    );
    add(
        REC_SPLIT_EXT3,
        1,
        IN_USE,
        fref(REC_SPLIT),
        vec![nonresident(&NonRes {
            ty: AT_DATA,
            name: "",
            flags: 0,
            start_vcn: 2,
            end_vcn: 2,
            runs: vec![(Some(LCN_SPLIT_3), 1)],
            alloc: 0,
            real: 0,
            init: 0,
            comp_unit: 0,
        })],
        &mut in_use,
    );

    // deleted: in-use bit clear, still has a $FILE_NAME
    add(
        REC_GONE,
        2,
        0,
        0,
        vec![
            si(),
            fname(root_ref, "gone.txt", FILE_ATTR_ARCHIVE, GONE_DATA.len() as u64, NS_BOTH),
            resident(AT_DATA, "", GONE_DATA),
        ],
        &mut in_use,
    );

    // docs/ with a small resident index
    let readme_key = fn_value(docs_ref, "readme.txt", FILE_ATTR_ARCHIVE, README_DATA.len() as u64, NS_BOTH);
    let mut docs_entries = index_entry(fref(REC_README), Some(&readme_key), None);
    docs_entries.extend(index_entry(0, None, None));
    add(
        REC_DOCS,
        1,
        IN_USE | DIR,
        0,
        vec![
            si(),
            fname(root_ref, "docs", FILE_NAME_DIR, 0, NS_BOTH),
            resident(AT_IR, "$I30", &index_root_value(&docs_entries, false, 4096, cs as u32)),
        ],
        &mut in_use,
    );
    add(
        REC_README,
        1,
        IN_USE,
        0,
        vec![
            si(),
            fname(docs_ref, "readme.txt", FILE_ATTR_ARCHIVE, README_DATA.len() as u64, NS_BOTH),
            resident(AT_DATA, "", README_DATA),
        ],
        &mut in_use,
    );
    // long name + DOS short name (DOS name first in the record)
    add(
        REC_LONG,
        1,
        IN_USE,
        0,
        vec![
            si(),
            fname(root_ref, "DOCUME~1.TXT", FILE_ATTR_ARCHIVE, 5, NS_DOS),
            fname(root_ref, "Document Long Name.txt", FILE_ATTR_ARCHIVE, 5, NS_WIN32),
            resident(AT_DATA, "", b"long\n"),
        ],
        &mut in_use,
    );
    add(
        REC_SPARSE,
        1,
        IN_USE,
        0,
        vec![
            si(),
            fname(root_ref, "sparse.bin", FILE_ATTR_ARCHIVE | 0x200, SPARSE_SIZE, NS_BOTH),
            nonresident(&NonRes {
                ty: AT_DATA,
                name: "",
                flags: 0x8000,
                start_vcn: 0,
                end_vcn: 3,
                runs: vec![(Some(LCN_SPARSE_A), 1), (None, 2), (Some(LCN_SPARSE_B), 1)],
                alloc: SPARSE_SIZE,
                real: SPARSE_SIZE,
                init: SPARSE_INIT,
                comp_unit: 0,
            }),
        ],
        &mut in_use,
    );
    add(
        REC_COMP,
        1,
        IN_USE,
        0,
        vec![
            si(),
            fname(root_ref, "comp.bin", FILE_ATTR_ARCHIVE | 0x800, cs, NS_POSIX),
            nonresident(&NonRes {
                ty: AT_DATA,
                name: "",
                flags: 0x0001,
                start_vcn: 0,
                end_vcn: 15,
                runs: vec![(Some(LCN_COMP), 1), (None, 15)],
                alloc: 16 * cs,
                real: cs,
                init: cs,
                comp_unit: 4,
            }),
        ],
        &mut in_use,
    );
    add(
        REC_ENC,
        1,
        IN_USE,
        0,
        vec![
            si(),
            fname(root_ref, "enc.bin", FILE_ATTR_ARCHIVE | 0x4000, 100, NS_BOTH),
            nonresident(&NonRes {
                ty: AT_DATA,
                name: "",
                flags: 0x4000,
                start_vcn: 0,
                end_vcn: 0,
                runs: vec![(Some(LCN_ENC), 1)],
                alloc: cs,
                real: 100,
                init: 100,
                comp_unit: 0,
            }),
        ],
        &mut in_use,
    );
    // deleted file in docs/, DOS name first: the long name must win
    add(
        REC_OLD,
        2,
        0,
        0,
        vec![
            si(),
            fname(docs_ref, "OLD~1.TXT", FILE_ATTR_ARCHIVE, OLD_DATA.len() as u64, NS_DOS),
            fname(docs_ref, "old file.txt", FILE_ATTR_ARCHIVE, OLD_DATA.len() as u64, NS_WIN32),
            resident(AT_DATA, "", OLD_DATA),
        ],
        &mut in_use,
    );
    // deleted extension record that (oddly) carries a $FILE_NAME: never listed
    add(
        REC_GHOST,
        2,
        0,
        fref(REC_GONE),
        vec![fname(root_ref, "ghost.txt", FILE_ATTR_ARCHIVE, 1, NS_BOTH)],
        &mut in_use,
    );

    // $MFT itself last, so its $BITMAP reflects every record above
    in_use[0] |= 1;
    let mft_real = MFT_RECORDS * rs;
    add(
        0,
        1,
        IN_USE,
        0,
        vec![
            si(),
            sys("$MFT", mft_real),
            nonresident(&NonRes {
                ty: AT_DATA,
                name: "",
                flags: 0,
                start_vcn: 0,
                end_vcn: mft_clusters - 1,
                runs: mft_runs.clone(),
                alloc: opts.mft_alloc.unwrap_or(mft_clusters * cs),
                real: opts.mft_real.unwrap_or(mft_real),
                init: mft_real,
                comp_unit: 0,
            }),
            resident(AT_BITMAP, "", &in_use.clone()),
        ],
        &mut in_use,
    );

    let mut mirror = Vec::new();
    for (num, rec) in recs {
        if num < 4 {
            mirror.push((num, rec.clone()));
        }
        img.put_record(num, rec);
    }
    for (num, rec) in mirror {
        img.write(LCN_MFTMIRR * cs + num * rs, &rec);
    }

    // ---- $Bitmap ----
    let bm = img.bitmap.clone();
    img.write(LCN_BITMAP * cs, &bm);

    let used_clusters = (0..clusters).filter(|&c| bm[(c / 8) as usize] >> (c % 8) & 1 != 0).collect();
    Built { image: img.data, used_clusters, total_sectors, root_index_names }
}

/// Byte offset of MFT record `num` in an image built with `opts`.
pub fn record_offset(opts: &NtfsOpts, num: u64) -> u64 {
    let cs = opts.cluster_size as u64;
    let rs = opts.record_size as u64;
    match opts.mft_second_lcn {
        Some(second) if num >= 8 => second * cs + (num - 8) * rs,
        _ => opts.mft_lcn * cs + num * rs,
    }
}
