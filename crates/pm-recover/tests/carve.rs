mod common;

use common::text_bytes;
use pm_core::{BlockDevice, CancelToken, Error, FnProgress, MemDevice, NullProgress, ProgressReport, ReadOnly};
use pm_recover::{carve, extract_carved, signatures, CarvedFile, Category, LengthRule, Signature};
use std::sync::Mutex;

const SECTOR: u64 = 512;

fn jpeg(body: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
    v.extend(text_bytes(body, 1));
    v.extend([0xFF, 0xD9]);
    v
}

fn png(body: usize) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend(text_bytes(body, 2));
    v.extend([b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]);
    v
}

/// The PDF ends at its first `%%EOF`; the `\n` after it is not part of the carved file.
fn pdf(body: usize) -> Vec<u8> {
    let mut v = b"%PDF-1.4\n".to_vec();
    v.extend(text_bytes(body, 3));
    v.extend(b"\n%%EOF");
    v
}

fn zip_entry_bytes(name: &str, data: &[u8], offset: u32) -> (Vec<u8>, Vec<u8>) {
    let mut local = vec![0x50, 0x4B, 0x03, 0x04, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    local.extend(0u32.to_le_bytes()); // crc (not checked)
    local.extend((data.len() as u32).to_le_bytes());
    local.extend((data.len() as u32).to_le_bytes());
    local.extend((name.len() as u16).to_le_bytes());
    local.extend(0u16.to_le_bytes());
    local.extend(name.as_bytes());
    local.extend(data);
    let mut central = vec![0x50, 0x4B, 0x01, 0x02, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    central.extend(0u32.to_le_bytes());
    central.extend((data.len() as u32).to_le_bytes());
    central.extend((data.len() as u32).to_le_bytes());
    central.extend((name.len() as u16).to_le_bytes());
    central.extend([0u8; 2 + 2 + 2 + 2 + 4]); // extra, comment, disk, internal, external
    central.extend(offset.to_le_bytes());
    central.extend(name.as_bytes());
    (local, central)
}

/// A structurally valid stored zip: local entries, central directory, EOCD with `comment`.
fn zip(files: &[(&str, &[u8])], comment: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cd = Vec::new();
    for (name, data) in files {
        let (l, c) = zip_entry_bytes(name, data, out.len() as u32);
        out.extend(l);
        cd.extend(c);
    }
    let cd_off = out.len() as u32;
    let cd_len = cd.len() as u32;
    out.extend(cd);
    out.extend([0x50, 0x4B, 0x05, 0x06, 0, 0, 0, 0]);
    out.extend((files.len() as u16).to_le_bytes());
    out.extend((files.len() as u16).to_le_bytes());
    out.extend(cd_len.to_le_bytes());
    out.extend(cd_off.to_le_bytes());
    out.extend((comment.len() as u16).to_le_bytes());
    out.extend(comment);
    out
}

fn riff(form: &[u8; 4], total: usize) -> Vec<u8> {
    let mut v = b"RIFF".to_vec();
    v.extend(((total - 8) as u32).to_le_bytes());
    v.extend(form);
    v.extend(text_bytes(total - 12, 5));
    v
}

fn mp4_box(kind: &[u8; 4], total: usize) -> Vec<u8> {
    let mut v = (total as u32).to_be_bytes().to_vec();
    v.extend(kind);
    v.extend(text_bytes(total - 8, 6));
    v
}

fn mp4(mdat: usize) -> Vec<u8> {
    let mut v = Vec::new();
    let mut ftyp = (24u32).to_be_bytes().to_vec();
    ftyp.extend(b"ftypisom");
    ftyp.extend(0u32.to_be_bytes());
    ftyp.extend(b"isom");
    ftyp.extend(b"mp41");
    v.extend(ftyp);
    v.extend(mp4_box(b"moov", 40));
    v.extend(mp4_box(b"mdat", mdat));
    v
}

/// Device filled with lowercase filler so no accidental signature appears.
fn noise_dev(len: usize) -> MemDevice {
    MemDevice::from_vec(text_bytes(len, 9))
}

fn put(dev: &mut MemDevice, off: u64, data: &[u8]) {
    dev.write_at(off, data).unwrap();
}

fn run(dev: &mut dyn BlockDevice, sector: u32) -> Vec<CarvedFile> {
    carve(dev, signatures(), sector, &NullProgress, &CancelToken::new()).unwrap()
}

fn by_sig<'a>(v: &'a [CarvedFile], sig: &str) -> Vec<&'a CarvedFile> {
    v.iter().filter(|f| f.sig == sig).collect()
}

#[test]
fn carve_finds_jpeg_png_pdf_zip_in_synthetic_disk() {
    let mut dev = noise_dev(16 << 20);
    let j = jpeg(3000);
    let p = png(5000);
    let d = pdf(7000);
    let z = zip(&[("a.txt", b"hello zip"), ("dir/b.bin", &text_bytes(1500, 4))], b"hi");
    // odd sector numbers, and each body is followed by filler
    let (oj, op, od, oz) = (1001 * SECTOR, 3331 * SECTOR, 9001 * SECTOR, 12345 * SECTOR);
    put(&mut dev, oj, &j);
    put(&mut dev, op, &p);
    put(&mut dev, od, &d);
    put(&mut dev, oz, &z);

    let found = run(&mut dev, 512);
    assert_eq!(found.len(), 4, "{found:#?}");
    // sorted by offset
    assert!(found.windows(2).all(|w| w[0].offset < w[1].offset));

    let f = &found[0];
    assert_eq!((f.offset, f.len, f.complete), (oj, j.len() as u64, true));
    assert_eq!((f.sig.as_str(), f.ext.as_str(), f.category), ("JPEG", "jpg", Category::Image));
    let f = &found[1];
    assert_eq!((f.offset, f.len, f.complete), (op, p.len() as u64, true));
    assert_eq!((f.sig.as_str(), f.ext.as_str(), f.category), ("PNG", "png", Category::Image));
    let f = &found[2];
    assert_eq!((f.offset, f.len, f.complete), (od, d.len() as u64, true));
    assert_eq!((f.sig.as_str(), f.ext.as_str(), f.category), ("PDF", "pdf", Category::Document));
    let f = &found[3];
    // the EOCD end includes the 2-byte archive comment
    assert_eq!((f.offset, f.len, f.complete), (oz, z.len() as u64, true));
    assert_eq!((f.sig.as_str(), f.ext.as_str(), f.category), ("ZIP", "zip", Category::Archive));

    // extraction reproduces the embedded bytes under <offset hex>.<ext>
    let out = tempfile::tempdir().unwrap();
    for (f, body) in found.iter().zip([&j, &p, &d, &z]) {
        let path = extract_carved(&mut dev, f, out.path()).unwrap();
        assert_eq!(path, out.path().join(format!("{:012x}.{}", f.offset, f.ext)));
        assert_eq!(&std::fs::read(&path).unwrap(), body);
    }
}

#[test]
fn carve_truncates_at_max_len() {
    const HDR: Signature = Signature {
        name: "TEST",
        ext: "tst",
        header: b"HDR1",
        header_offset: 0,
        footer: Some(b"ENDX"),
        max_len: 4096,
        category: Category::Other,
        secondary: None,
        rule: LengthRule::Footer(b"ENDX"),
    };
    let sigs = [HDR];
    // no footer at all
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, 100 * SECTOR, b"HDR1");
    let r = carve(&mut dev, &sigs, 512, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].offset, r[0].len, r[0].complete), (100 * SECTOR, 4096, false));
    // a footer beyond max_len does not count
    put(&mut dev, 100 * SECTOR + 4500, b"ENDX");
    let r = carve(&mut dev, &sigs, 512, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!((r[0].len, r[0].complete), (4096, false));
    // a footer inside max_len does
    put(&mut dev, 100 * SECTOR + 1000, b"ENDX");
    let r = carve(&mut dev, &sigs, 512, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!((r[0].len, r[0].complete), (1004, true));
}

#[test]
fn truncated_jpeg_without_footer_runs_to_max_len_or_device_end() {
    let mut dev = noise_dev(16 << 20);
    let off = 30_001 * SECTOR;
    put(&mut dev, off, &[0xFF, 0xD8, 0xFF, 0xE1]);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 1, "{r:?}");
    let jpeg_max = signatures().iter().find(|s| s.name == "JPEG").unwrap().max_len;
    assert_eq!(r[0].offset, off);
    assert!(!r[0].complete);
    assert_eq!(r[0].len, jpeg_max.min(dev.len() - off));
    assert_eq!(r[0].len, dev.len() - off, "16 MiB device is shorter than the 64 MiB cap");
}

#[test]
fn zip_ignores_nested_eocd_lookalikes() {
    // the stored file contains a fake EOCD whose central directory fields do not add up
    let mut fake = b"PK\x05\x06".to_vec();
    fake.extend([0u8; 18]);
    let z = zip(&[("inner.bin", &fake), ("tail.txt", b"after the fake")], b"");
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, 77 * SECTOR, &z);
    let r = run(&mut dev, 512);
    let zips = by_sig(&r, "ZIP");
    assert_eq!(zips.len(), 1, "{r:?}");
    assert_eq!((zips[0].len, zips[0].complete), (z.len() as u64, true));
}

#[test]
fn riff_and_mp4_lengths_come_from_their_headers() {
    let mut dev = MemDevice::new(2 << 20);
    let wav = riff(b"WAVE", 2000);
    let webp = riff(b"WEBP", 1500);
    let avi = riff(b"AVI ", 3000);
    let mp = mp4(3000);
    put(&mut dev, 10 * SECTOR, &wav);
    put(&mut dev, 50 * SECTOR, &webp);
    put(&mut dev, 100 * SECTOR, &avi);
    put(&mut dev, 200 * SECTOR, &mp);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 4, "{r:#?}");
    let w = &by_sig(&r, "WAV")[0];
    assert_eq!((w.offset, w.len, w.complete, w.ext.as_str()), (10 * SECTOR, 2000, true, "wav"));
    assert_eq!(w.category, Category::Audio);
    let w = &by_sig(&r, "WebP")[0];
    assert_eq!((w.offset, w.len, w.complete), (50 * SECTOR, 1500, true));
    let a = &by_sig(&r, "AVI")[0];
    assert_eq!((a.offset, a.len, a.complete), (100 * SECTOR, 3000, true));
    assert_eq!(a.category, Category::Video);
    let m = &by_sig(&r, "MP4")[0];
    assert_eq!((m.offset, m.len, m.complete), (200 * SECTOR, mp.len() as u64, true));
    assert_eq!(mp.len(), 24 + 40 + 3000);
}

#[test]
fn riff_larger_than_the_device_is_incomplete() {
    let mut dev = MemDevice::new(64 * 1024);
    let mut wav = riff(b"WAVE", 2000);
    wav[4..8].copy_from_slice(&(1_000_000u32).to_le_bytes());
    put(&mut dev, 4 * SECTOR, &wav);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].len, r[0].complete), (64 * 1024 - 4 * SECTOR, false));
}

#[test]
fn mp4_with_overflowing_mdat_is_truncated() {
    let mut dev = MemDevice::new(32 * 1024);
    let mut mp = mp4(3000);
    let n = mp.len();
    mp[64..68].copy_from_slice(&(1_000_000u32).to_be_bytes());
    put(&mut dev, 2 * SECTOR, &mp[..n]);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].len, r[0].complete), (32 * 1024 - 2 * SECTOR, false));
}

#[test]
fn any_offset_mode_finds_unaligned_and_chunk_straddling_headers() {
    let chunk = 4u64 << 20;
    let mut dev = noise_dev(9 << 20);
    let j = jpeg(2000);
    let p = png(3000);
    let oj = chunk - 2; // header straddles the 4 MiB scan boundary
    let op = 6 * chunk / 4 + 12_345; // not sector aligned
    put(&mut dev, oj, &j);
    put(&mut dev, op, &p);

    assert!(run(&mut dev, 512).is_empty());
    let r = run(&mut dev, 1);
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!((r[0].offset, r[0].len, r[0].complete), (oj, j.len() as u64, true));
    assert_eq!((r[1].offset, r[1].len, r[1].complete), (op, p.len() as u64, true));

    // aligned to 4 KiB only
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, 4096 * 3, &j);
    put(&mut dev, 4096 * 5 + 512, &j);
    let r = run(&mut dev, 4096);
    assert_eq!(r.iter().map(|f| f.offset).collect::<Vec<_>>(), vec![4096 * 3]);
}

#[test]
fn different_types_may_overlap_but_nested_same_type_is_suppressed() {
    let mut dev = noise_dev(1 << 20);
    let inner = png(1000);
    let mut outer = b"%PDF-1.7\n".to_vec();
    outer.resize(512, b'x');
    outer.extend(&inner);
    outer.extend(text_bytes(600, 8));
    outer.extend(b"\n%%EOF");
    put(&mut dev, 20 * SECTOR, &outer);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 2, "{r:#?}");
    let pdf = &by_sig(&r, "PDF")[0];
    let png = &by_sig(&r, "PNG")[0];
    assert_eq!((pdf.offset, pdf.len), (20 * SECTOR, outer.len() as u64));
    assert_eq!((png.offset, png.len), (21 * SECTOR, inner.len() as u64));

    // a JPEG header inside a complete JPEG is not reported again
    let mut outer = jpeg(100);
    outer.truncate(outer.len() - 2); // drop the footer: the outer one ends at the inner footer
    outer.resize(512, b'y');
    outer.extend(jpeg(200));
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, 5 * SECTOR, &outer);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].offset, r[0].len), (5 * SECTOR, outer.len() as u64));
}

#[test]
fn headers_cut_off_by_the_device_end_are_ignored() {
    let mut dev = MemDevice::new(1000);
    put(&mut dev, 997, b"%PD");
    assert!(run(&mut dev, 1).is_empty());
    put(&mut dev, 995, b"%PDF-");
    let r = run(&mut dev, 1);
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].offset, r[0].len, r[0].complete), (995, 5, false));
    // empty device
    assert!(run(&mut MemDevice::new(0), 512).is_empty());
}

#[test]
fn carve_never_writes_reports_progress_and_honours_cancel() {
    let mut dev = noise_dev(10 << 20);
    put(&mut dev, 9 * SECTOR, &jpeg(100));
    let mut ro = ReadOnly(dev);
    let reports = Mutex::new(Vec::<ProgressReport>::new());
    let sink = FnProgress(|p: &ProgressReport| reports.lock().unwrap().push(p.clone()));
    let r = carve(&mut ro, signatures(), 512, &sink, &CancelToken::new()).unwrap();
    assert_eq!(r.len(), 1);
    let last = reports.lock().unwrap().last().cloned().expect("a final report");
    assert_eq!(last.done, 10 << 20);
    assert_eq!(last.total, 10 << 20);

    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(carve(&mut ro, signatures(), 512, &NullProgress, &cancel), Err(Error::Cancelled)));
    assert!(matches!(
        carve(&mut ro, signatures(), 0, &NullProgress, &CancelToken::new()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn extract_carved_validates_range_and_extension() {
    let mut dev = noise_dev(64 * 1024);
    let out = tempfile::tempdir().unwrap();
    let mut f = CarvedFile {
        offset: 0x200,
        len: 100,
        sig: "X".into(),
        ext: "bin".into(),
        category: Category::Other,
        complete: false,
    };
    let p = extract_carved(&mut dev, &f, out.path()).unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().len(), 100);
    f.len = 64 * 1024; // runs past the end of the device
    assert!(matches!(extract_carved(&mut dev, &f, out.path()), Err(Error::Invalid(_))));
    // an extension from an untrusted session file cannot escape the directory
    f.len = 10;
    f.ext = "../../evil".into();
    let p = extract_carved(&mut dev, &f, out.path()).unwrap();
    assert_eq!(p.parent().unwrap(), out.path());
    assert!(p.file_name().unwrap().to_str().unwrap().ends_with(".bin"));
}

#[test]
fn signature_table_is_well_formed() {
    let sigs = signatures();
    assert!(sigs.len() >= 25, "{}", sigs.len());
    for s in sigs {
        assert!(!s.name.is_empty() && !s.ext.is_empty() && !s.header.is_empty(), "{}", s.name);
        assert!(s.ext.bytes().all(|b| b.is_ascii_alphanumeric()), "{}", s.name);
        assert!(s.max_len > 0, "{}", s.name);
        if let LengthRule::Footer(f) = s.rule {
            assert_eq!(s.footer, Some(f), "{}", s.name);
        }
    }
    for ext in [
        "jpg", "png", "gif", "bmp", "tif", "webp", "pdf", "zip", "7z", "rar", "gz", "mp3", "flac",
        "wav", "avi", "mp4", "mkv", "doc", "sqlite", "exe", "elf", "ttf", "otf", "psd", "xml", "html",
    ] {
        assert!(sigs.iter().any(|s| s.ext == ext), "missing {ext}");
    }
    // the table survives a JSON round trip of its serializable parts
    let json = serde_json::to_string(&sigs[0]).unwrap();
    assert!(json.contains(sigs[0].name));
}
