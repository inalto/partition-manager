//! The file-type signature table used by the carver.

use pm_core::units::{GIB, MIB};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category {
    Image,
    Document,
    Archive,
    Audio,
    Video,
    Executable,
    Database,
    Other,
}

/// How the length of a carved file is determined once its header has matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LengthRule {
    /// The file ends with the first occurrence of these bytes (inclusive), searched within
    /// `max_len` of the start.
    Footer(&'static [u8]),
    /// No reliable end marker: take `max_len` (capped at the device end); never "complete".
    MaxOnly,
    /// RIFF container (WAV, AVI, WebP): length is the little-endian u32 at offset 4, plus 8.
    Riff,
    /// ISO base media (MP4, MOV, M4A): walk the top-level atom chain.
    Mp4Atoms,
    /// ZIP and its derivatives: find the end-of-central-directory record that belongs to this
    /// archive; the file ends after its comment.
    ZipEocd,
}

/// One file type. `header` must appear at `header_offset` bytes into the file; when `secondary`
/// is set its bytes must also appear at the given offset from the start of the file (used by
/// RIFF flavours). Adding a plain type is a one-line entry in [`signatures`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub name: &'static str,
    /// File extension without the dot (ASCII alphanumerics).
    pub ext: &'static str,
    pub header: &'static [u8],
    pub header_offset: usize,
    /// Footer bytes, when the type has one (informational for the non-`Footer` rules).
    pub footer: Option<&'static [u8]>,
    /// Longest file of this type that will be carved.
    pub max_len: u64,
    pub category: Category,
    /// Extra `(offset from file start, bytes)` check after the header matched.
    pub secondary: Option<(usize, &'static [u8])>,
    pub rule: LengthRule,
}

impl Signature {
    /// Bytes from the file start that must be readable to check the header and secondary
    /// match.
    pub fn match_extent(&self) -> usize {
        let h = self.header_offset + self.header.len();
        let s = self.secondary.map_or(0, |(o, b)| o + b.len());
        h.max(s)
    }
}

const fn plain(
    name: &'static str,
    ext: &'static str,
    header: &'static [u8],
    max_len: u64,
    category: Category,
) -> Signature {
    Signature {
        name,
        ext,
        header,
        header_offset: 0,
        footer: None,
        max_len,
        category,
        secondary: None,
        rule: LengthRule::MaxOnly,
    }
}

const fn with_footer(
    name: &'static str,
    ext: &'static str,
    header: &'static [u8],
    footer: &'static [u8],
    max_len: u64,
    category: Category,
) -> Signature {
    Signature {
        name,
        ext,
        header,
        header_offset: 0,
        footer: Some(footer),
        max_len,
        category,
        secondary: None,
        rule: LengthRule::Footer(footer),
    }
}

const fn riff(
    name: &'static str,
    ext: &'static str,
    form: &'static [u8],
    max_len: u64,
    category: Category,
) -> Signature {
    Signature {
        name,
        ext,
        header: b"RIFF",
        header_offset: 0,
        footer: None,
        max_len,
        category,
        secondary: Some((8, form)),
        rule: LengthRule::Riff,
    }
}

const EOCD: &[u8] = b"PK\x05\x06";

static SIGNATURES: &[Signature] = &[
    with_footer("JPEG", "jpg", &[0xFF, 0xD8, 0xFF], &[0xFF, 0xD9], 64 * MIB, Category::Image),
    with_footer(
        "PNG",
        "png",
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        &[b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82],
        64 * MIB,
        Category::Image,
    ),
    with_footer("GIF87a", "gif", b"GIF87a", &[0x00, 0x3B], 32 * MIB, Category::Image),
    with_footer("GIF89a", "gif", b"GIF89a", &[0x00, 0x3B], 32 * MIB, Category::Image),
    plain("BMP", "bmp", b"BM", 64 * MIB, Category::Image),
    plain("TIFF (LE)", "tif", &[0x49, 0x49, 0x2A, 0x00], 256 * MIB, Category::Image),
    plain("TIFF (BE)", "tif", &[0x4D, 0x4D, 0x00, 0x2A], 256 * MIB, Category::Image),
    riff("WebP", "webp", b"WEBP", 256 * MIB, Category::Image),
    plain("PSD", "psd", b"8BPS", 256 * MIB, Category::Image),
    with_footer("PDF", "pdf", b"%PDF-", b"%%EOF", 256 * MIB, Category::Document),
    Signature {
        name: "ZIP",
        ext: "zip",
        header: &[0x50, 0x4B, 0x03, 0x04],
        header_offset: 0,
        footer: Some(EOCD),
        max_len: 2 * GIB,
        category: Category::Archive,
        secondary: None,
        rule: LengthRule::ZipEocd,
    },
    plain("7-Zip", "7z", &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C], 2 * GIB, Category::Archive),
    plain("RAR", "rar", &[0x52, 0x61, 0x72, 0x21, 0x1A, 0x07], 2 * GIB, Category::Archive),
    plain("GZIP", "gz", &[0x1F, 0x8B, 0x08], 2 * GIB, Category::Archive),
    plain("MP3 (ID3)", "mp3", b"ID3", 64 * MIB, Category::Audio),
    plain("MP3", "mp3", &[0xFF, 0xFB], 64 * MIB, Category::Audio),
    plain("FLAC", "flac", b"fLaC", 256 * MIB, Category::Audio),
    riff("WAV", "wav", b"WAVE", 4 * GIB, Category::Audio),
    riff("AVI", "avi", b"AVI ", 4 * GIB, Category::Video),
    Signature {
        name: "MP4",
        ext: "mp4",
        header: b"ftyp",
        header_offset: 4,
        footer: None,
        max_len: 4 * GIB,
        category: Category::Video,
        secondary: None,
        rule: LengthRule::Mp4Atoms,
    },
    plain("MKV/WebM", "mkv", &[0x1A, 0x45, 0xDF, 0xA3], 4 * GIB, Category::Video),
    plain(
        "OLE2",
        "doc",
        &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1],
        64 * MIB,
        Category::Document,
    ),
    plain("SQLite", "sqlite", b"SQLite format 3\0", GIB, Category::Database),
    plain("EXE/DLL", "exe", b"MZ", 256 * MIB, Category::Executable),
    plain("ELF", "elf", &[0x7F, b'E', b'L', b'F'], 256 * MIB, Category::Executable),
    plain("TTF", "ttf", &[0x00, 0x01, 0x00, 0x00], 32 * MIB, Category::Other),
    plain("OTF", "otf", b"OTTO", 32 * MIB, Category::Other),
    plain("XML", "xml", b"<?xml", 16 * MIB, Category::Other),
    with_footer("HTML (doctype)", "html", b"<!DOCTYPE html", b"</html>", 16 * MIB, Category::Other),
    with_footer("HTML", "html", b"<html", b"</html>", 16 * MIB, Category::Other),
];

/// All built-in signatures.
pub fn signatures() -> &'static [Signature] {
    SIGNATURES
}
