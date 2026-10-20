//! `pm clone <src> <dst>`: copy one device onto another, plus the helpers `pm image`,
//! `pm restore` and `pm rescue` share (read-only source wrapper, safety checks, used-map planning).

use crate::cli::{CliError, CopyMode};
use crate::cli::arg_error;
use crate::device_arg::{open_spec, parse_device, resolve_volume, volume_extent, DeviceArg, Identity, Opened};
use crate::progress::StderrProgress;
use pm_core::{format_size, BlockDevice, CancelToken, Error};
use pm_fs::UsedMap;
use pm_platform::{OpenMode, Platform, WriteSession};
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
    if let Some(disk) = dst.disk_index() {
        // fail closed: an undeterminable system disk refuses every disk destination
        match system_disk(platform) {
            Some(system) if system != disk => {}
            Some(_) => {
                return Err(CliError::Usage(format!(
                    "Disk {disk} is the running system disk; refusing to write to it"
                )))
            }
            None => {
                return Err(CliError::Usage(format!(
                    "Disk {disk}: cannot determine the running system disk; refusing to write to it"
                )))
            }
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

/// The disk Windows is running from, as the executor determines it
/// ([`pm_ops::Model::effective_system_disk`]); `None` when it cannot be determined.
fn system_disk(platform: &dyn Platform) -> Option<u32> {
    pm_ops::Model::from_platform(platform).ok()?.effective_system_disk()
}

/// A write session shared by the destination device writing through it and the
/// [`SessionGuard`] that finishes it.
#[derive(Clone)]
struct SessionSlot(Arc<Mutex<Option<Box<dyn WriteSession>>>>);

impl SessionSlot {
    fn with<T>(&self, f: impl FnOnce(&mut dyn BlockDevice) -> pm_core::Result<T>) -> pm_core::Result<T> {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_mut() {
            Some(s) => f(s.device()),
            None => Err(Error::Invalid("the write session is already finished".into())),
        }
    }
}

/// `[start, start + len)` of the disk of a write session: how a copy writes to a disk, a
/// partition or a volume (never through a raw handle opened outside a session).
struct SessionSlice {
    slot: SessionSlot,
    start: u64,
    len: u64,
    sector: u32,
}

impl SessionSlice {
    fn at(&self, offset: u64, n: usize) -> pm_core::Result<u64> {
        match offset.checked_add(n as u64) {
            Some(end) if end <= self.len => Ok(self.start + offset),
            _ => Err(Error::Invalid(format!(
                "access at {offset}+{n} outside the {}-byte destination",
                self.len
            ))),
        }
    }
}

impl BlockDevice for SessionSlice {
    fn len(&self) -> u64 {
        self.len
    }
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> pm_core::Result<()> {
        let at = self.at(offset, buf.len())?;
        self.slot.with(|d| d.read_at(at, buf))
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> pm_core::Result<()> {
        let at = self.at(offset, buf.len())?;
        self.slot.with(|d| d.write_at(at, buf))
    }
    fn flush(&mut self) -> pm_core::Result<()> {
        self.slot.with(|d| d.flush())
    }
    fn is_read_only(&self) -> bool {
        false
    }
}

/// Finishes the write session of a copy's destination: through [`SessionGuard::finish`]
/// (whose error is reported) or, on an error, cancellation or panic, when dropped.
pub struct SessionGuard {
    slot: SessionSlot,
}

impl SessionGuard {
    fn take(&self) -> Option<Box<dyn WriteSession>> {
        self.slot.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Finish the session (bring the disk back, re-read its layout).
    pub fn finish(self) -> Result<(), CliError> {
        match self.take() {
            Some(s) => s.finish().map_err(CliError::Runtime),
            None => Ok(()),
        }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(s) = self.take() {
            let disk = s.disk();
            if let Err(e) = s.finish() {
                log::warn!("finishing the write session of Disk {disk}: {e}");
            }
        }
    }
}

fn unplaced_volume(spec: &str) -> CliError {
    CliError::Usage(format!(
        "{spec}: the volume's place on a physical disk is unknown (spanned or virtual volume); \
         refusing to write to it"
    ))
}

/// What a copy keeps of the read-only probe of its destination once the probe handle is
/// closed: its identity, the range it covered and its place on a physical disk.
pub struct Probed {
    identity: Identity,
    range: (u64, u64),
    disk_extent: Option<(u32, u64, u64)>,
}

impl Probed {
    pub fn of(probe: &Opened) -> Probed {
        Probed { identity: probe.identity.clone(), range: probe.range, disk_extent: probe.disk_extent }
    }
}

/// Open the destination `spec` for writing. Disks, their partitions and volumes are written
/// through a write session on the disk (volumes locked and dismounted, the disk offline for
/// the copy); a volume whose place on a physical disk is unknown (spanned, virtual) is
/// refused. Image files are opened directly. `probe` is what the same destination, opened
/// read-only, looked like; its identity is kept. The caller has dropped that read-only
/// handle: a volume cannot be locked while another handle on it is open.
fn open_destination(
    platform: &dyn Platform,
    spec: &str,
    probe: &Probed,
) -> Result<(Opened, Option<SessionGuard>), CliError> {
    let arg = parse_device(spec).map_err(arg_error)?;
    let (disk, part, volume) = match &arg {
        DeviceArg::Disk(i) => (*i, None, None),
        DeviceArg::Partition(parent, n) => match **parent {
            DeviceArg::Disk(i) => (i, Some(*n), None),
            _ => return Ok((open_spec(platform, spec, OpenMode::ReadWrite)?, None)),
        },
        DeviceArg::Volume(_) => match probe.disk_extent {
            Some((d, s, e)) => (d, None, Some((s, e))),
            None => return Err(unplaced_volume(spec)),
        },
        DeviceArg::File(_) => return Ok((open_spec(platform, spec, OpenMode::ReadWrite)?, None)),
    };
    let session = platform.begin_write(disk).map_err(CliError::Runtime)?;
    let guard = SessionGuard { slot: SessionSlot(Arc::new(Mutex::new(Some(session)))) };
    let (disk_len, sector) =
        guard.slot.with(|d| Ok((d.len(), d.sector_size()))).map_err(CliError::Runtime)?;
    let (start, end) = match (volume, part) {
        (Some(extent), _) => extent,
        (None, Some(n)) => {
            let table = guard.slot.with(|d| pm_parttable::read_table(d)).map_err(CliError::Runtime)?;
            let e = table.entries.iter().find(|e| e.number == n).ok_or_else(|| {
                CliError::Usage(format!("Disk {disk} has no partition {n} any more"))
            })?;
            (e.start, e.start + e.len)
        }
        (None, None) => (0, disk_len),
    };
    if start > end || end > disk_len {
        return Err(CliError::Usage(format!("{spec}: lies outside Disk {disk}")));
    }
    let dev = SessionSlice { slot: guard.slot.clone(), start, len: end - start, sector };
    let range = if volume.is_some() { probe.range } else { (start, end) };
    let opened = Opened {
        dev: Box::new(dev),
        identity: probe.identity.clone(),
        range,
        disk_extent: Some((disk, start, end)),
        files: Vec::new(),
    };
    Ok((opened, Some(guard)))
}

/// Open a copy's source and destination. The source is opened read-only. The destination is
/// first opened read-only to learn its size and identity; only when every check in
/// [`check_copy`] passes is it opened for writing — through a write session for disks,
/// partitions and volumes ([`SessionGuard`], which the caller finishes with [`finish`]) — so
/// a refused copy never takes a write handle (which on a real disk can lock or dismount
/// volumes).
pub fn open_checked(
    platform: &dyn Platform,
    src_spec: &str,
    dst_spec: &str,
    dst_offset: u64,
) -> Result<(Opened, Opened, Option<SessionGuard>), CliError> {
    let (src, probe) = probe_checked(platform, src_spec, dst_spec, dst_offset)?;
    let (dst, guard) = open_writable(platform, &src, probe, dst_spec, dst_offset)?;
    Ok((src, dst, guard))
}

/// The read-only half of [`open_checked`]: the source and the destination opened read-only
/// (`probe`), with every check of [`check_copy`] passed. Nothing is written or locked.
pub fn probe_checked(
    platform: &dyn Platform,
    src_spec: &str,
    dst_spec: &str,
    dst_offset: u64,
) -> Result<(Opened, Opened), CliError> {
    let src = open_spec(platform, src_spec, OpenMode::ReadOnly)?;
    if let Ok(DeviceArg::Volume(v)) = parse_device(dst_spec) {
        // refused before anything is opened on the destination
        let id = resolve_volume(platform, &v).map_err(|e| match e {
            Error::NotFound(m) => CliError::Usage(m),
            other => CliError::Runtime(other),
        })?;
        if volume_extent(platform, &id).is_none() {
            return Err(unplaced_volume(dst_spec));
        }
    }
    let probe = open_spec(platform, dst_spec, OpenMode::ReadOnly)?;
    check_copy(platform, &src, &probe, dst_offset)?;
    Ok((src, probe))
}

/// The writing half of [`open_checked`]: open the destination `probe` describes for writing
/// and check it again (the device could have changed between the two opens). The probe's
/// handle is closed first — the write session locks the destination's volumes, which fails
/// while any other handle on them (such as the probe of a volume destination) is open.
pub fn open_writable(
    platform: &dyn Platform,
    src: &Opened,
    probe: Opened,
    dst_spec: &str,
    dst_offset: u64,
) -> Result<(Opened, Option<SessionGuard>), CliError> {
    let probed = Probed::of(&probe);
    drop(probe);
    let (dst, guard) = open_destination(platform, dst_spec, &probed)?;
    check_copy(platform, src, &dst, dst_offset)?;
    Ok((dst, guard))
}

/// Finish the destination's write session, if there is one.
pub fn finish(guard: Option<SessionGuard>) -> Result<(), CliError> {
    guard.map_or(Ok(()), SessionGuard::finish)
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
    let (source, target, session) = open_checked(platform, src, dst, dst_offset)?;
    let start = Instant::now();
    let (copied, smart) = copy_device(source, target, dst_offset, mode, cancel)?;
    finish(session)?;
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
        p.set_system_disk(9);
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
        assert_refused(clone(&p, "F:", "disk:1"), "same device");
        assert_eq!(p.disk_bytes(1), before, "refused copies write nothing");

        // adjacent but not overlapping ranges of the same disk, and other disks, are fine
        clone(&p, "disk:1:1", "disk:1:2").unwrap();
        let after = p.disk_bytes(1);
        assert_eq!(after[3 * MIB as usize..5 * MIB as usize], before[MIB as usize..3 * MIB as usize]);
        // a volume source keeps its handle open for the copy: its own disk cannot be the
        // destination (the session could not lock the volume), another disk can
        match clone(&p, "F:", "disk:1:2") {
            Err(CliError::Runtime(Error::Busy(_))) => {}
            other => panic!("expected Busy, got {other:?}"),
        }
        assert_eq!(p.disk_bytes(1), after);
        clone(&p, "D:", "disk:2").unwrap();
        assert_eq!(p.disk_bytes(2)[..2 * MIB as usize], after[2 * MIB as usize..4 * MIB as usize]);
    }

    #[test]
    fn disk_destinations_are_written_in_a_finished_write_session() {
        let p = MockPlatform::new();
        p.add_disk(1, disk(2, &[(1, MIB, 2 * MIB), (2, 3 * MIB, 2 * MIB)]), Default::default());
        p.add_disk(2, disk(3, &[]), Default::default());
        p.set_system_disk(9);
        let src = p.disk_bytes(1);
        clone(&p, "disk:1", "disk:2").unwrap();
        assert_eq!(p.disk_bytes(2), src);
        assert_eq!(p.write_sessions_log(), vec![2]);
        assert_eq!(p.finished_sessions_log(), vec![2]);

        // a partition is written through the session of its disk, inside its bounds
        clone(&p, "disk:1:1", "disk:2:2").unwrap();
        let b = p.disk_bytes(2);
        assert_eq!(b[3 * MIB as usize..5 * MIB as usize], src[MIB as usize..3 * MIB as usize]);
        assert_eq!(p.write_sessions_log(), vec![2, 2]);
        assert_eq!(p.finished_sessions_log(), vec![2, 2]);

        // a volume through the session of the disk it lives on
        p.add_volume(volume('G', "gggg"), 2, 3 * MIB, 2 * MIB);
        clone(&p, "disk:1:2", "G:").unwrap();
        assert_eq!(p.write_sessions_log(), vec![2, 2, 2]);
        assert_eq!(p.finished_sessions_log(), vec![2, 2, 2]);

        // a cancelled copy still finishes the session it took
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(run(&p, "disk:1", "disk:2", CopyMode::Sector, 0, &cancel, &mut Vec::new()).is_err());
        assert_eq!(p.write_sessions_log(), vec![2, 2, 2, 2]);
        assert_eq!(p.finished_sessions_log(), vec![2, 2, 2, 2]);
    }

    /// The destination is probed read-only before it is opened for writing. For a volume,
    /// that probe handle must be gone before the write session locks the volume (Windows
    /// refuses `FSCTL_LOCK_VOLUME` while another handle is open; the mock does the same).
    #[test]
    fn volume_destination_probe_is_closed_before_the_write_session() {
        let p = MockPlatform::new();
        p.add_disk(1, disk(2, &[(1, MIB, 2 * MIB)]), Default::default());
        p.add_disk(2, disk(3, &[(1, MIB, 2 * MIB)]), Default::default());
        p.set_system_disk(9);
        p.add_volume(volume('G', "gggg"), 2, MIB, 2 * MIB);
        let src = p.disk_bytes(1);

        clone(&p, "disk:1:1", "G:").unwrap();
        assert_eq!(p.write_sessions_log(), vec![2]);
        assert_eq!(p.finished_sessions_log(), vec![2]);
        assert!(p.open_volume_handles().is_empty(), "no handle left open");
        assert_eq!(p.disk_bytes(2)[MIB as usize..3 * MIB as usize], src[MIB as usize..3 * MIB as usize]);

        // the two-phase helpers (used by rescue) behave the same
        let (s, probe) = probe_checked(&p, "disk:1:1", "G:", 0).unwrap();
        assert_eq!(p.open_volume_handles().len(), 1, "the probe holds the volume");
        let (dst, guard) = open_writable(&p, &s, probe, "G:", 0).unwrap();
        assert!(p.open_volume_handles().is_empty(), "the probe is dropped before begin_write");
        assert_eq!(dst.disk_extent, Some((2, MIB, 3 * MIB)));
        assert_eq!(p.write_sessions_log(), vec![2, 2]);
        drop(dst);
        finish(guard).unwrap();
        assert_eq!(p.finished_sessions_log(), vec![2, 2]);
    }

    #[test]
    fn unknown_system_disk_refuses_every_disk_destination() {
        let p = MockPlatform::new();
        p.add_disk(1, disk(2, &[]), Default::default());
        p.add_disk(2, disk(3, &[(1, MIB, 2 * MIB)]), Default::default());
        let before = p.disk_bytes(2);
        for dst in ["disk:2", "disk:2:1"] {
            assert_refused(clone(&p, "disk:1", dst), "cannot determine the running system disk");
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.img");
        FileDevice::create(&img, MIB).unwrap();
        assert_refused(
            crate::cmd_image::run_restore(
                &p,
                &format!("file:{}", img.display()),
                "disk:2",
                0,
                &CancelToken::new(),
                &mut Vec::new(),
            ),
            "cannot determine the running system disk",
        );
        assert_eq!(p.disk_bytes(2), before, "nothing written");
        assert!(p.write_sessions_log().is_empty(), "no session taken");
        // image files are not disks: still fine
        let out = dir.path().join("b.img");
        FileDevice::create(&out, 8 * MIB).unwrap();
        clone(&p, "disk:1", &format!("file:{}", out.display())).unwrap();
    }

    #[test]
    fn spanned_volume_destination_is_refused() {
        let p = MockPlatform::new();
        p.add_disk(1, disk(2, &[]), Default::default());
        p.set_system_disk(9);
        p.add_spanned_volume(volume('S', "span"));
        assert_refused(clone(&p, "disk:1", "S:"), "place on a physical disk is unknown");
        assert!(p.write_sessions_log().is_empty());
    }
}
