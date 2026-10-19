mod common;

use common::text_bytes;
use pm_core::{BlockDevice, CancelToken, Error, FnProgress, MemDevice, NullProgress, ProgressReport, ReadOnly};
use pm_image::rescue::FaultyDevice;
use pm_recover::{carve, extract_carved, signatures, CarvedFile, Category, LengthRule, Signature};
use std::collections::HashMap;
use std::sync::Mutex;

const SECTOR: u64 = 512;

/// SOS segment, entropy-coded data (with a stuffed FF 00 and an RST marker) and EOI.
fn jpeg_tail(body: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00];
    v.extend(text_bytes(body, 1));
    v.extend([0xFF, 0x00]);
    v.extend(text_bytes(10, 3));
    v.extend([0xFF, 0xD0]);
    v.extend(text_bytes(10, 4));
    v.extend([0xFF, 0xD9]);
    v
}

/// A structurally valid JPEG: SOI, JFIF APP0, SOS + data, EOI.
fn jpeg(body: usize) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    v.extend(b"JFIF\0\x01\x01\x01\x00\x48\x00\x48\x00\x00");
    v.extend(jpeg_tail(body));
    v
}

/// A JPEG whose EXIF APP1 segment embeds a complete thumbnail JPEG (with its own FF D9) that
/// starts 512 bytes into the file, i.e. at a sector boundary when the file does.
fn jpeg_with_thumbnail() -> Vec<u8> {
    let thumb = jpeg(300);
    let payload = (512 - 6) + thumb.len() + 40;
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE1];
    v.extend(((payload + 2) as u16).to_be_bytes());
    v.extend(b"Exif\0\0");
    v.resize(512, b'x');
    v.extend(&thumb);
    v.extend([b'y'; 40]);
    v.extend(jpeg_tail(500));
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
    carve(dev, signatures(), sector, &NullProgress, &CancelToken::new()).unwrap().files
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
    let r = carve(&mut dev, &sigs, 512, &NullProgress, &CancelToken::new()).unwrap().files;
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].offset, r[0].len, r[0].complete), (100 * SECTOR, 4096, false));
    // a footer beyond max_len does not count
    put(&mut dev, 100 * SECTOR + 4500, b"ENDX");
    let r = carve(&mut dev, &sigs, 512, &NullProgress, &CancelToken::new()).unwrap().files;
    assert_eq!((r[0].len, r[0].complete), (4096, false));
    // a footer inside max_len does
    put(&mut dev, 100 * SECTOR + 1000, b"ENDX");
    let r = carve(&mut dev, &sigs, 512, &NullProgress, &CancelToken::new()).unwrap().files;
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

    // the thumbnail inside the EXIF segment neither ends the JPEG early nor is reported again
    let outer = jpeg_with_thumbnail();
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, 5 * SECTOR, &outer);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].offset, r[0].len, r[0].complete), (5 * SECTOR, outer.len() as u64, true));
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
    assert_eq!(r.files.len(), 1);
    assert!(r.unreadable.is_empty());
    assert_eq!(r.bytes_scanned, 10 << 20);
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

// ---- fix round 1: structure-aware ends --------------------------------------------------

fn gif_with_lzw_trailer_lookalike() -> Vec<u8> {
    let mut v = b"GIF89a".to_vec();
    v.extend([1, 0, 1, 0, 0x80, 0, 0]); // 1x1, global color table of 2 entries
    v.extend([0, 0, 0, 255, 255, 255]);
    v.extend([0x21, 0xF9, 0x04, 0, 0, 0, 0, 0]); // graphic control extension
    v.extend([0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0]); // image descriptor
    v.push(2); // LZW minimum code size
    v.extend([5, 0x01, 0x00, 0x3B, 0x02, 0x03]); // sub-block whose data contains 00 3B
    v.extend([3, 0x04, 0x05, 0x06]);
    v.push(0); // block terminator
    v.push(0x3B); // trailer
    v
}

#[test]
fn jpeg_ends_at_the_real_eoi_not_at_an_embedded_thumbnail() {
    let outer = jpeg_with_thumbnail();
    let first_eoi = outer.windows(2).position(|w| w == [0xFF, 0xD9]).unwrap();
    assert!(first_eoi + 2 < outer.len() - 100, "fixture: the thumbnail EOI comes first");
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, 40 * SECTOR, &outer);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].offset, r[0].len, r[0].complete), (40 * SECTOR, outer.len() as u64, true));
}

#[test]
fn jpeg_with_broken_structure_falls_back_to_the_first_footer_as_incomplete() {
    // APP0 claims a length that lands in the middle of plain text
    let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x20];
    j.extend(text_bytes(300, 1));
    j.extend([0xFF, 0xD9]);
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, 3 * SECTOR, &j);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].len, r[0].complete), (j.len() as u64, false));
}

#[test]
fn pdf_ends_at_the_last_eof_before_the_next_pdf() {
    let mut updated = b"%PDF-1.4\n".to_vec();
    updated.extend(text_bytes(900, 1));
    updated.extend(b"\n%%EOF\n"); // first revision / linearization hint
    updated.extend(text_bytes(700, 2));
    updated.extend(b"\n%%EOF"); // incremental update
    let second = pdf(1000);
    let mut dev = noise_dev(1 << 20);
    let o1 = 10 * SECTOR;
    let o2 = 30 * SECTOR;
    put(&mut dev, o1, &updated);
    put(&mut dev, o2, &second);
    let r = run(&mut dev, 512);
    let pdfs = by_sig(&r, "PDF");
    assert_eq!(pdfs.len(), 2, "{r:?}");
    assert_eq!((pdfs[0].offset, pdfs[0].len, pdfs[0].complete), (o1, updated.len() as u64, true));
    assert_eq!((pdfs[1].offset, pdfs[1].len, pdfs[1].complete), (o2, second.len() as u64, true));

    // no %%EOF at all: runs to the next PDF header, incomplete
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, o1, b"%PDF-1.4\n");
    put(&mut dev, o2, b"%PDF-1.7\n");
    let r = run(&mut dev, 512);
    let pdfs = by_sig(&r, "PDF");
    assert_eq!((pdfs[0].len, pdfs[0].complete), (o2 - o1, false));
}

#[test]
fn gif_ends_at_the_trailer_even_when_lzw_data_contains_the_footer_bytes() {
    let g = gif_with_lzw_trailer_lookalike();
    let early = g.windows(2).position(|w| w == [0x00, 0x3B]).unwrap();
    assert!(early + 2 < g.len(), "fixture: 00 3B occurs before the trailer");
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, 9 * SECTOR, &g);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].offset, r[0].len, r[0].complete), (9 * SECTOR, g.len() as u64, true));

    // a broken block structure falls back to the first 00 3B and is incomplete
    let mut bad = g.clone();
    bad[13 + 6] = 0x99; // not a GIF block introducer
    let mut dev = noise_dev(1 << 20);
    put(&mut dev, 9 * SECTOR, &bad);
    let r = run(&mut dev, 512);
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].len, r[0].complete), (early as u64 + 2, false));
}

// ---- fix round 1: footers across the 1 MiB search block ---------------------------------

const BLOCK: usize = 1 << 20;

#[test]
fn png_footer_straddling_the_search_block_boundary_is_found() {
    // The footer search starts right after the 8-byte header, so the footer begins at
    // `body + 8` bytes into the file; put it on both sides of the first and second boundary.
    for body in [BLOCK - 20, BLOCK - 11, BLOCK - 9, BLOCK - 8, BLOCK - 7, BLOCK - 1, BLOCK, BLOCK + 1, 2 * BLOCK - 12, 2 * BLOCK - 7] {
        let p = png(body);
        let mut dev = noise_dev(3 << 20);
        put(&mut dev, 3 * SECTOR, &p);
        let r = run(&mut dev, 512);
        assert_eq!(r.len(), 1, "body {body}: {r:?}");
        assert_eq!((r[0].offset, r[0].len, r[0].complete), (3 * SECTOR, p.len() as u64, true), "body {body}");
    }
}

#[test]
fn zip_eocd_straddling_the_search_block_boundary_is_found() {
    // EOCD offset from the end of the 4-byte local header = 30 + 2 * name + data + 46 - 4 - ...
    let name = "f.bin";
    let fixed = 30 + name.len() + 46 + name.len() - 4;
    for delta in [-6i64, -5, -3, -1, 0, 1, 3] {
        let data = (BLOCK as i64 - fixed as i64 + delta) as usize;
        let z = zip(&[(name, &text_bytes(data, 7))], b"comment");
        let mut dev = noise_dev(3 << 20);
        put(&mut dev, 5 * SECTOR, &z);
        let r = run(&mut dev, 512);
        let zips = by_sig(&r, "ZIP");
        assert_eq!(zips.len(), 1, "delta {delta}: {r:?}");
        assert_eq!((zips[0].len, zips[0].complete), (z.len() as u64, true), "delta {delta}");
    }
}

#[test]
fn zip_larger_than_several_search_blocks_has_the_exact_length() {
    let z = zip(
        &[("a.bin", &text_bytes(BLOCK + 12_345, 1)), ("b.bin", &text_bytes(BLOCK + 99, 2))],
        &[b'c'; 100],
    );
    assert!(z.len() > 2 * BLOCK);
    let mut dev = noise_dev(4 << 20);
    put(&mut dev, 17 * SECTOR, &z);
    let r = run(&mut dev, 512);
    let zips = by_sig(&r, "ZIP");
    assert_eq!(zips.len(), 1, "{r:?}");
    assert_eq!((zips[0].offset, zips[0].len, zips[0].complete), (17 * SECTOR, z.len() as u64, true));
}

#[test]
fn jpeg_entropy_data_longer_than_a_block_is_walked() {
    // the stuffed FF 00 and the RST marker land on both sides of the first block boundary
    for extra in [-14i64, -13, -12, -2, -1, 0, 1, 2, 11, 12, 13] {
        let j = jpeg((BLOCK as i64 + extra) as usize);
        let mut dev = noise_dev(3 << 20);
        put(&mut dev, 2 * SECTOR, &j);
        let r = run(&mut dev, 512);
        assert_eq!(r.len(), 1, "extra {extra}: {r:?}");
        assert_eq!((r[0].len, r[0].complete), (j.len() as u64, true), "extra {extra}");
    }
}

// ---- fix round 1: read errors -----------------------------------------------------------

fn faulty(len: usize, bad: &[u64]) -> FaultyDevice {
    FaultyDevice::new(noise_dev(len), bad.to_vec(), HashMap::new())
}

#[test]
fn carve_survives_a_bad_sector_and_reports_it() {
    let j = jpeg(3000);
    let p = png(5000);
    let bad = 5 * (1u64 << 20) + 7 * SECTOR; // between the two files, in the second chunk
    let (oj, op) = (100 * SECTOR, 14_000 * SECTOR);
    let mut mem = noise_dev(10 << 20);
    put(&mut mem, oj, &j);
    put(&mut mem, op, &p);
    let mut dev = FaultyDevice::new(mem, vec![bad], HashMap::new());
    assert!(op > bad, "fixture: the PNG lies after the bad sector");

    let r = carve(&mut dev, signatures(), 512, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(r.files.len(), 2, "{r:#?}");
    assert_eq!((r.files[0].offset, r.files[0].len, r.files[0].complete), (oj, j.len() as u64, true));
    assert_eq!((r.files[1].offset, r.files[1].len, r.files[1].complete), (op, p.len() as u64, true));
    assert_eq!(r.unreadable, vec![(bad, SECTOR)]);
    assert_eq!(r.bytes_scanned, (10 << 20) - SECTOR);
    assert_eq!(dev.write_count(), 0);
}

#[test]
fn adjacent_bad_sectors_are_merged_and_cover_chunk_overlaps() {
    // bad sectors just before a chunk boundary are read twice (as chunk tail and next head)
    let boundary = 4u64 << 20;
    let bad = [boundary - SECTOR, boundary - 2 * SECTOR, boundary + 5 * SECTOR];
    let mut dev = faulty(9 << 20, &bad);
    let r = carve(&mut dev, signatures(), 512, &NullProgress, &CancelToken::new()).unwrap();
    assert!(r.files.is_empty());
    assert_eq!(r.unreadable, vec![(boundary - 2 * SECTOR, 2 * SECTOR), (boundary + 5 * SECTOR, SECTOR)]);
    assert_eq!(r.bytes_scanned, (9 << 20) - 3 * SECTOR);
}

#[test]
fn a_bad_sector_inside_a_file_makes_it_incomplete_instead_of_aborting() {
    let p = png(200_000);
    let off = 50 * SECTOR;
    let mut mem = noise_dev(2 << 20);
    put(&mut mem, off, &p);
    let bad = off + 100 * SECTOR; // inside the PNG body, hit by the footer search
    let mut dev = FaultyDevice::new(mem, vec![bad], HashMap::new());
    let r = carve(&mut dev, signatures(), 512, &NullProgress, &CancelToken::new()).unwrap();
    assert_eq!(r.files.len(), 1, "{r:#?}");
    assert_eq!(r.files[0].offset, off);
    assert!(!r.files[0].complete);
    assert!(r.unreadable.iter().any(|&(o, l)| o <= bad && bad < o + l), "{:?}", r.unreadable);
}
