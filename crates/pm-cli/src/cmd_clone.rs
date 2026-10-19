//! `pm clone <src> <dst>`: copy one device onto another, plus the helpers `pm image`,
//! `pm restore` and `pm rescue` share (read-only source wrapper, safety checks, used-map planning).

use crate::cli::{CliError, CopyMode};
use crate::device_arg::{open_spec, Opened};
use crate::progress::StderrProgress;
use pm_core::{format_size, BlockDevice, CancelToken, Error};
use pm_fs::UsedMap;
use pm_platform::{OpenMode, Platform};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A cloneable, read-only view of a source device. The filesystem reader gets one handle (it
/// needs an owned `Box<dyn BlockDevice>`) and the copier another, so the source is opened once.
/// Writes are rejected, so no command can modify its own source through it.
#[derive(Clone)]
pub struct SharedSource {
    inner: Arc<Mutex<Box<dyn BlockDevice>>>,
    len: u64,
    sector_size: u32,
}

impl SharedSource {
    pub fn new(dev: Box<dyn BlockDevice>) -> Self {
        SharedSource {
            len: dev.len(),
            sector_size: dev.sector_size(),
            inner: Arc::new(Mutex::new(dev)),
        }
    }
}

impl BlockDevice for SharedSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn sector_size(&self) -> u32 {
        self.sector_size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> pm_core::Result<()> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).read_at(offset, buf)
    }
    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> pm_core::Result<()> {
        Err(Error::Invalid("source devices are read-only".into()))
    }
    fn flush(&mut self) -> pm_core::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        true
    }
}

/// The canonical path of `p` when it exists.
pub fn existing_canonical(p: &std::path::Path) -> Option<std::path::PathBuf> {
    if p.exists() {
        std::fs::canonicalize(p).ok()
    } else {
        None
    }
}

/// The checks every copy runs after opening both devices and before writing anything:
/// the offset is sector-aligned, the destination is not (part of) the source, and it is large
/// enough to hold the source behind the offset.
pub fn check_copy(src: &Opened, dst: &Opened, dst_offset: u64) -> Result<(), CliError> {
    let len = src.dev.len();
    let sector = dst.dev.sector_size().max(1) as u64;
    if dst_offset % sector != 0 {
        return Err(CliError::Usage(format!(
            "--dst-offset {dst_offset} is not a multiple of the destination sector size ({sector})"
        )));
    }
    if dst.overlaps(dst_offset, len, src) {
        return Err(CliError::Usage(
            "source and destination are the same device (or overlap); refusing to copy a device onto itself"
                .into(),
        ));
    }
    let room = dst.dev.len().saturating_sub(dst_offset);
    if room < len {
        return Err(CliError::Usage(format!(
            "destination too small: {} ({len} bytes) to copy but only {} ({room} bytes) available{}",
            format_size(len),
            format_size(room),
            if dst_offset > 0 { format!(" at offset {dst_offset}") } else { String::new() }
        )));
    }
    Ok(())
}

/// The used-block map of the filesystem on `src`, or the reason there is none.
pub fn fs_used_map(src: &SharedSource) -> Result<UsedMap, Error> {
    let mut fs = pm_fs::open(Box::new(src.clone()))?;
    fs.used_map()
}

/// Decide what to copy: `Some(map)` for a smart copy, `None` for every sector. `Auto` is smart
/// exactly when a filesystem reader opens `src`; an explicit `--smart` that cannot be honoured
/// is an error rather than a silent full copy.
pub fn plan_used_map(src: &SharedSource, mode: CopyMode) -> Result<Option<UsedMap>, CliError> {
    match mode {
        CopyMode::Sector => Ok(None),
        CopyMode::Auto => Ok(fs_used_map(src).ok()),
        CopyMode::Smart => fs_used_map(src).map(Some).map_err(|e| {
            CliError::Runtime(Error::Unsupported(format!(
                "smart mode needs a supported filesystem at the start of the source ({e}); use --sector to copy every sector"
            )))
        }),
    }
}

/// `"{verb} {size} ({bytes} bytes) in {secs}s, {rate}/s ({mode} copy)"`.
pub fn summary(verb: &str, copied: u64, elapsed: Duration, smart: bool) -> String {
    let secs = elapsed.as_secs_f64();
    let rate = if secs > 0.0 { (copied as f64 / secs) as u64 } else { copied };
    format!(
        "{verb} {} ({copied} bytes) in {secs:.1}s, {}/s ({} copy)",
        format_size(copied),
        format_size(rate),
        if smart { "smart" } else { "sector" }
    )
}

/// Copy all of `src` onto `dst` at `dst_offset`, aborting on the first error. Returns the
/// bytes written and whether the copy was smart. Nothing is written unless [`check_copy`]
/// passes.
pub fn copy_device(
    src: Opened,
    dst: Opened,
    dst_offset: u64,
    mode: CopyMode,
    cancel: &CancelToken,
) -> Result<(u64, bool), CliError> {
    check_copy(&src, &dst, dst_offset)?;
    let len = src.dev.len();
    let mut source = SharedSource::new(src.dev);
    let used = plan_used_map(&source, mode)?;
    let mut dst_dev = dst.dev;
    let progress = StderrProgress::new();
    let copied = pm_image::copy_simple(
        &mut source,
        &mut *dst_dev,
        0,
        dst_offset,
        len,
        used.as_ref(),
        &progress,
        cancel,
    )?;
    Ok((copied, used.is_some()))
}

pub fn run(
    platform: &dyn Platform,
    src: &str,
    dst: &str,
    mode: CopyMode,
    dst_offset: u64,
    cancel: &CancelToken,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let source = open_spec(platform, src, OpenMode::ReadOnly)?;
    let target = open_spec(platform, dst, OpenMode::ReadWrite)?;
    let start = Instant::now();
    let (copied, smart) = copy_device(source, target, dst_offset, mode, cancel)?;
    writeln!(out, "{}", summary("Cloned", copied, start.elapsed(), smart))
        .map_err(|e| CliError::Runtime(Error::io(0, 0, e)))
}
