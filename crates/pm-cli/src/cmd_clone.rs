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
/// the offset is sector-aligned, the destination is not on the running system disk, it is not
/// (part of) the source, and it is large enough to hold the source behind the offset.
pub fn check_copy(platform: &dyn Platform, src: &Opened, dst: &Opened, dst_offset: u64) -> Result<(), CliError> {
    let len = src.dev.len();
    let sector = dst.dev.sector_size().max(1) as u64;
    if dst_offset % sector != 0 {
        return Err(CliError::Usage(format!(
            "--dst-offset {dst_offset} is not a multiple of the destination sector size ({sector})"
        )));
    }
    if let (Some(disk), Some(system)) = (dst.disk_index(), platform.system_disk()) {
        if disk == system {
            return Err(CliError::Usage(format!(
                "Disk {disk} is the running system disk; refusing to write to it"
            )));
        }
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

/// Open a copy's source and destination. The source is opened read-only. The destination is
/// first opened read-only to learn its size and identity; only when every check in
/// [`check_copy`] passes is it reopened read-write, so a refused copy never takes a write
/// handle (which on a real disk can lock or dismount volumes).
pub fn open_checked(
    platform: &dyn Platform,
    src_spec: &str,
    dst_spec: &str,
    dst_offset: u64,
) -> Result<(Opened, Opened), CliError> {
    let src = open_spec(platform, src_spec, OpenMode::ReadOnly)?;
    let probe = open_spec(platform, dst_spec, OpenMode::ReadOnly)?;
    check_copy(platform, &src, &probe, dst_offset)?;
    drop(probe);
    let dst = open_spec(platform, dst_spec, OpenMode::ReadWrite)?;
    // the device could have changed between the two opens
    check_copy(platform, &src, &dst, dst_offset)?;
    Ok((src, dst))
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

/// Copy `len` bytes of `source` to `dst` at `dst_offset` with [`pm_image::copy_simple`],
/// aborting on the first error; with `used`, unused areas are skipped. Returns bytes copied.
/// The caller has already run [`check_copy`].
pub fn copy_planned(
    source: &mut SharedSource,
    used: Option<&UsedMap>,
    dst: &mut dyn BlockDevice,
    dst_offset: u64,
    cancel: &CancelToken,
) -> Result<u64, CliError> {
    let len = source.len();
    let progress = StderrProgress::new();
    Ok(pm_image::copy_simple(source, dst, 0, dst_offset, len, used, &progress, cancel)?)
}

/// Copy all of `src` onto `dst` (both from [`open_checked`]). Returns the bytes written and
/// whether the copy was smart.
pub fn copy_device(
    src: Opened,
    mut dst: Opened,
    dst_offset: u64,
    mode: CopyMode,
    cancel: &CancelToken,
) -> Result<(u64, bool), CliError> {
    let mut source = SharedSource::new(src.dev);
    let used = plan_used_map(&source, mode)?;
    let copied = copy_planned(&mut source, used.as_ref(), &mut *dst.dev, dst_offset, cancel)?;
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
    let (source, target) = open_checked(platform, src, dst, dst_offset)?;
    let start = Instant::now();
    let (copied, smart) = copy_device(source, target, dst_offset, mode, cancel)?;
    writeln!(out, "{}", summary("Cloned", copied, start.elapsed(), smart))
        .map_err(|e| CliError::Runtime(Error::io(0, 0, e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RescueArgs;
    use pm_core::{gpt_types, FileDevice, MemDevice, PartitionType, TableKind, VolumeId, VolumeInfo, MIB};
    use pm_parttable::{write_table, Entry, Table};
    use pm_platform::MockPlatform;

    /// An 8 MiB disk with a recognisable pattern and a GPT holding `parts` (number, start, len).
    fn disk(seed: u8, parts: &[(u32, u64, u64)]) -> MemDevice {
        let mut d = MemDevice::new(8 * MIB);
        for off in (4 * MIB..8 * MIB - 64 * 1024).step_by(MIB as usize) {
            d.write_at(off, &vec![seed; 4096]).unwrap();
        }
        let mut t = Table::empty(TableKind::Gpt, d.len(), 512);
        for &(number, start, len) in parts {
            t.entries.push(Entry {
                number,
                start,
                len,
                ptype: PartitionType::Gpt(gpt_types::BASIC_DATA),
                guid: Some(uuid::Uuid::from_u128(0x500 + number as u128)),
                name: format!("p{number}"),
                attrs: 0,
                logical: false,
            });
        }
        write_table(&mut d, &t).unwrap();
        d
    }

    fn volume(letter: char, tag: &str) -> VolumeInfo {
        VolumeInfo {
            id: VolumeId(format!(r"\\?\Volume{{{tag}}}\")),
            mount_points: vec![format!(r"{letter}:\")],
            ..Default::default()
        }
    }

    fn clone(p: &MockPlatform, src: &str, dst: &str) -> Result<(), CliError> {
        run(p, src, dst, CopyMode::Sector, 0, &CancelToken::new(), &mut Vec::new())
    }

    fn assert_refused(r: Result<(), CliError>, needle: &str) {
        match r {
            Err(CliError::Usage(m)) => assert!(m.contains(needle), "{m}"),
            other => panic!("expected a usage error containing {needle:?}, got {other:?}"),
        }
    }

    #[test]
    fn system_disk_is_never_a_destination() {
        let p = MockPlatform::new();
        p.add_disk(0, disk(1, &[(1, MIB, 2 * MIB)]), Default::default());
        p.add_disk(1, disk(2, &[]), Default::default());
        p.add_disk(2, disk(3, &[]), Default::default());
        p.set_system_disk(0);
        let system_before = p.disk_bytes(0);

        assert_refused(clone(&p, "disk:1", "disk:0"), "Disk 0 is the running system disk");
        // a partition slice inherits the disk
        assert_refused(clone(&p, "disk:1", "disk:0:1"), "Disk 0 is the running system disk");

        // restore and rescue go through the same helper
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.img");
        FileDevice::create(&img, 8 * MIB).unwrap();
        let spec = format!("file:{}", img.display());
        assert_refused(
            crate::cmd_image::run_restore(&p, &spec, "disk:0", 0, &CancelToken::new(), &mut Vec::new()),
            "Disk 0 is the running system disk",
        );
        let args = RescueArgs {
            src: spec.clone(),
            dst: "disk:0".into(),
            mapfile: dir.path().join("m.map"),
            sector_size: None,
            cluster_size: None,
            skip_size: None,
            retries: 0,
            reverse: false,
            no_scrape: false,
            no_trim: false,
            dst_offset: 0,
            used_map: false,
            ansi: false,
        };
        assert_refused(
            crate::cmd_rescue::run(&p, &args, &CancelToken::new(), &mut Vec::new()),
            "Disk 0 is the running system disk",
        );
        assert!(!args.mapfile.exists());
        assert_eq!(p.disk_bytes(0), system_before);

        // the system disk is fine as a source, and other disks as destinations
        clone(&p, "disk:0", "disk:2").unwrap();
        assert_eq!(p.disk_bytes(2), system_before);
    }

    #[test]
    fn volume_and_disk_aliasing_is_refused() {
        let p = MockPlatform::new();
        p.add_disk(
            1,
            disk(2, &[(1, MIB, 2 * MIB), (2, 3 * MIB, 2 * MIB)]),
            Default::default(),
        );
        p.add_disk(2, disk(3, &[]), Default::default());
        p.add_volume(volume('D', "aaaa"), 1, 2 * MIB, 2 * MIB); // [2M, 4M) of disk 1
        p.add_volume(volume('F', "ffff"), 1, 5 * MIB, MIB / 2); // [5M, 5.5M) of disk 1
        let before = p.disk_bytes(1);

        // a volume onto its own disk, the disk onto its volume, a volume onto a partition
        // that overlaps it
        assert_refused(clone(&p, "D:", "disk:1"), "same device");
        assert_refused(clone(&p, "disk:1", "D:"), "same device");
        assert_refused(clone(&p, "D:", "disk:1:2"), "same device");
        assert_refused(clone(&p, "D:", "disk:1:1"), "same device");
        assert_refused(clone(&p, "disk:1:1", "D:"), "same device");
        assert_eq!(p.disk_bytes(1), before, "refused copies write nothing");

        // adjacent but not overlapping ranges of the same disk, and other disks, are fine
        clone(&p, "F:", "disk:1:2").unwrap();
        let after = p.disk_bytes(1);
        assert_eq!(after[3 * MIB as usize..(3 * MIB + 512 * 1024) as usize], before[5 * MIB as usize..(5 * MIB + 512 * 1024) as usize]);
        clone(&p, "D:", "disk:2").unwrap();
        assert_eq!(p.disk_bytes(2)[..2 * MIB as usize], after[2 * MIB as usize..4 * MIB as usize]);
    }
}
