//! Disk and partition images: create (write-then-rename) and restore.

use super::{entry_of, fs_facts, with_session, Ctx};
use crate::model::is_container;
use crate::operation::CloneMode;
use pm_core::units::align_up;
use pm_core::{BlockDevice, DiskInfo, Error, Result, SliceDevice};
use pm_fs::UsedMap;
use pm_image::{CreateOpts, ImageFormat, RescueOpts, RescueStats};
use pm_platform::{OpenMode, Platform};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Unit of the disk-wide used map of a smart disk image.
const DISK_MAP_UNIT: u32 = 64 << 10;
const IMAGE_BLOCK: usize = 4 << 20;

/// An image written to `<path>.partial` and renamed to `<path>` once complete (a flat VMDK's
/// `-flat.vmdk` extent too, with the descriptor pointed at the final extent name).
struct Partial {
    target: PathBuf,
    partial: PathBuf,
    format: ImageFormat,
}

/// `<stem>-flat.vmdk` next to descriptor `p`: the extent pm-image writes for `VmdkFlat`.
fn flat_extent(p: &Path) -> Option<PathBuf> {
    let stem = p.file_stem()?.to_str()?;
    Some(p.with_file_name(format!("{stem}-flat.vmdk")))
}

fn io_err(p: &Path, e: std::io::Error) -> Error {
    Error::Platform {
        code: e.raw_os_error().unwrap_or(0) as u32,
        msg: format!("{}: {e}", p.display()),
    }
}

impl Partial {
    fn new(target: &Path, format: ImageFormat) -> Self {
        let mut s: OsString = target.as_os_str().to_owned();
        s.push(".partial");
        Partial { target: target.to_path_buf(), partial: PathBuf::from(s), format }
    }

    fn commit(&self) -> Result<()> {
        let r = self.try_commit();
        if r.is_err() {
            self.discard();
        }
        r
    }

    fn try_commit(&self) -> Result<()> {
        if self.format == ImageFormat::VmdkFlat {
            let (Some(from), Some(to)) = (flat_extent(&self.partial), flat_extent(&self.target))
            else {
                return Err(Error::Invalid(format!("bad image path {}", self.target.display())));
            };
            let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
            let text =
                std::fs::read_to_string(&self.partial).map_err(|e| io_err(&self.partial, e))?;
            let patched = text.replace(
                &format!("\"{}\"", name(&from).unwrap_or_default()),
                &format!("\"{}\"", name(&to).unwrap_or_default()),
            );
            std::fs::write(&self.partial, patched).map_err(|e| io_err(&self.partial, e))?;
            std::fs::rename(&from, &to).map_err(|e| io_err(&to, e))?;
        }
        std::fs::rename(&self.partial, &self.target).map_err(|e| io_err(&self.target, e))
    }

    /// Remove the partial files (best effort).
    fn discard(&self) {
        let _ = std::fs::remove_file(&self.partial);
        if self.format == ImageFormat::VmdkFlat {
            if let Some(e) = flat_extent(&self.partial) {
                let _ = std::fs::remove_file(e);
            }
        }
    }
}

/// Used map of a whole disk for a smart image: everything outside the partitions (tables,
/// EBRs, gaps) plus what each partition's filesystem uses (all of it when unreadable).
fn disk_used_map(p: &dyn Platform, d: &DiskInfo) -> UsedMap {
    let mut map = UsedMap::new(d.size, DISK_MAP_UNIT);
    let mut parts: Vec<_> = d.partitions.iter().filter(|q| !is_container(d.table, q)).collect();
    parts.sort_by_key(|q| q.start);
    let mut cursor = 0;
    for q in parts {
        if q.start > cursor {
            map.set(cursor, q.start - cursor);
        }
        cursor = cursor.max(q.end());
        match fs_facts(p, d.index, q.start, q.len) {
            Some(f) => {
                for (o, l) in f.used.ranges() {
                    map.set(q.start + o, l);
                }
                if f.used.len < q.len {
                    map.set(q.start + f.used.len, q.len - f.used.len);
                }
            }
            None => map.set(q.start, q.len),
        }
    }
    if cursor < d.size {
        map.set(cursor, d.size - cursor);
    }
    map
}

/// Image disk `disk` (or its partition `number`) into `path`. Reads go through the rescue
/// engine in clone mode (the first read error fails the operation) so the read statistics
/// are reported; smart mode reads only used areas, the rest stays unallocated/zero.
pub(crate) fn image(
    ctx: &mut Ctx,
    disk: u32,
    number: Option<u32>,
    path: &Path,
    format: ImageFormat,
    mode: CloneMode,
) -> Result<()> {
    let d = ctx.live_disk(disk)?;
    let (start, len, used) = match number {
        Some(n) => {
            let q = ctx.live_partition(disk, n)?;
            let used = match mode {
                CloneMode::Smart => fs_facts(ctx.platform, disk, q.start, q.len).map(|f| f.used),
                CloneMode::Sector => None,
            };
            (q.start, q.len, used)
        }
        None => {
            let used = (mode == CloneMode::Smart).then(|| disk_used_map(ctx.platform, d));
            (0, d.size, used)
        }
    };
    let sink = ctx.step(&format!("write image {}", path.display()));
    let partial = Partial::new(path, format);
    let r = (|| -> Result<RescueStats> {
        let opts = CreateOpts { format, capacity: align_up(len, 512), sparse: true };
        let mut img = pm_image::create(&partial.partial, &opts)?;
        let dev = ctx.platform.open_disk(disk, OpenMode::ReadOnly)?;
        let mut src = SliceDevice::new(dev, start, len)?;
        let ropts = RescueOpts {
            block_size: IMAGE_BLOCK,
            used_map: used,
            abort_on_error: true,
            ..Default::default()
        };
        let stats = pm_image::rescue(&mut src, &mut *img, 0, &ropts, &sink, ctx.cancel)?;
        img.flush()?;
        Ok(stats)
    })();
    match r {
        Ok(stats) => {
            partial.commit()?;
            ctx.rescue_stats.push(stats);
            Ok(())
        }
        Err(e) => {
            partial.discard();
            Err(e)
        }
    }
}

/// Restore image `path` over the whole of `dst_disk` (`dst_start == None`), into the
/// partition starting at `dst_start`, or into a new partition there (the planner's numbers).
pub(crate) fn restore(
    ctx: &mut Ctx,
    path: &Path,
    dst_disk: u32,
    dst_start: Option<u64>,
) -> Result<()> {
    let mut img = pm_image::open(path, true)?;
    let cap = img.len();
    let existing = match dst_start {
        Some(s) => ctx.live_disk(dst_disk)?.partitions.iter().find(|p| p.start == s),
        None => None,
    };
    let new = match (dst_start, existing) {
        (Some(_), None) => Some(ctx.new_partition(dst_disk)?),
        _ => None,
    };
    let platform = ctx.platform;
    with_session(platform, dst_disk, |s| {
        let (at, room) = match (dst_start, existing, new) {
            (None, ..) => (0, s.device().len()),
            (Some(_), Some(p), _) | (Some(_), None, Some(p)) => (p.start, p.len),
            (Some(_), None, None) => {
                return Err(Error::Invalid(format!("no target partition on Disk {dst_disk}")))
            }
        };
        if cap > room {
            return Err(Error::Invalid(format!(
                "image {} ({cap} bytes) does not fit its {room}-byte target on Disk {dst_disk}",
                path.display()
            )));
        }
        let mut t = new.map(|_| s.read_table()).transpose()?;
        if let (Some(t), Some(p)) = (&t, new) {
            if t.entries.iter().any(|e| e.start < p.end() && p.start < e.end()) {
                return Err(Error::Invalid(format!(
                    "the target range on Disk {dst_disk} is no longer free"
                )));
            }
        }
        let sink = ctx.step(&format!("restore {}", path.display()));
        pm_image::copy_simple(&mut *img, s.device(), 0, at, cap, None, &sink, ctx.cancel)?;
        if let (Some(t), Some(p)) = (t.as_mut(), new) {
            ctx.step("write partition table");
            let e = entry_of(t.kind, p);
            t.entries.push(e);
            s.commit_table(t)?;
        }
        Ok(())
    })?;
    ctx.rescan(dst_disk)
}
