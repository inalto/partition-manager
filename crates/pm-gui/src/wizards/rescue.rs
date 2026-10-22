//! Rescue Copy wizard (GNU ddrescue): Source (disk or partition) → Destination (disk,
//! partition or image file, large enough; a missing image file is created in the chosen
//! format) → Mapfile (default `<dest>.map`; an existing one is
//! resumed) → Options (sector / cluster / skip size, retries, reverse, no-trim, no-scrape,
//! filesystem-aware skipping of unused space) → Rescue (explicit start, a live ddrescue-style
//! status panel, Cancel, final statistics and a bar of the mapfile).
//!
//! There is no `Operation` for a rescue: like the CLI's `pm rescue`, the copy runs at once
//! (after the confirmation on the last page) through `pm_image::rescue` on a worker, reading
//! the source read-only and writing a disk / partition through a platform write session. The
//! CLI's checks are mirrored: no write to the running system disk (fail closed), no copy onto
//! the source itself, and the destination must hold the whole source.

use crate::tools::{
    danger, device_combo, error_text, on_letters_problem, open_range, wizard_window, write_problem, Device, Footer, Nav,
    Task, ToolEnv, ToolWindow, WindowKind, WindowOutcome,
};
use eframe::egui::{self, pos2, vec2, Color32, Rect, RichText, Sense};
use pm_core::{align_up, format_size, parse_size, BlockDevice, CancelToken, Error, ProgressSink};
use pm_image::{rescue, BlockStatus, ImageFormat, MapFile, RescueOpts, RescueStats};
use pm_ops::{Model, Operation};
use pm_platform::{OpenMode, Platform};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PAGES: [&str; 5] = ["Source", "Destination", "Mapfile", "Options", "Rescue"];
/// Largest copy-phase read (cluster x sector), as in the CLI.
const MAX_BLOCK: u64 = 256 << 20;
/// Segments of the mapfile bar.
const BAR_BUCKETS: usize = 400;

/// The ddrescue-style status text of a run: phase and elapsed time, bytes rescued and the
/// average rate, the error size and areas, read errors and retry passes.
pub fn format_rescue_status(stats: &RescueStats, elapsed: Duration, phase: &str) -> String {
    let secs = elapsed.as_secs_f64();
    let rate = if secs > 0.0 { (stats.rescued as f64 / secs) as u64 } else { 0 };
    [
        format!("    phase: {phase:<16}  elapsed: {}", crate::format_util::format_eta(Some(elapsed.as_secs()))),
        format!("  rescued: {:<16}     rate: {}/s", format_size(stats.rescued), format_size(rate)),
        format!("  errsize: {:<16}  bad areas: {}", format_size(stats.bad_bytes), stats.bad_areas),
        format!("read errors: {:<14}  retry passes: {}", stats.read_errors, stats.passes),
    ]
    .join("\n")
}

/// Where the rescue writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dest {
    Device(Device),
    /// An image file (created as an image of the source's size, in the wizard's
    /// `dest_format`, when missing).
    File(String),
}

/// Why `src` cannot be rescued onto the device `dst`, if it cannot: the destination is the
/// running system disk (or that is unknown) or has pending operations, overlaps the source,
/// or is smaller than the source.
pub fn device_dest_problem(live: &Model, pending: &[Operation], src: Device, dst: Device) -> Option<String> {
    let (Some((ss, sl)), Some((ds, dl))) = (src.range(live), dst.range(live)) else {
        return Some("The source or destination no longer exists.".into());
    };
    if src.disk() == dst.disk() && ss < ds + dl && ds < ss + sl {
        return Some("The destination is (part of) the source; refusing to copy a device onto itself.".into());
    }
    if dl < sl {
        return Some(format!("The destination is too small: {} to copy, {} available.", format_size(sl), format_size(dl)));
    }
    write_problem(live, pending, dst.disk())
}

/// Why `path` cannot be the destination image file, if it cannot (the folder is checked).
pub fn file_dest_problem(path: &str) -> Option<String> {
    let t = path.trim();
    if t.is_empty() {
        return Some("Enter the path of the destination image file.".into());
    }
    let p = Path::new(t);
    if !p.is_absolute() || p.file_name().is_none() {
        return Some("Enter a full file path, e.g. D:\\Rescue\\disk1.img.".into());
    }
    match p.parent() {
        Some(d) if d.is_dir() => None,
        _ => Some("The destination folder does not exist.".into()),
    }
}

/// Why the file `path` (described as `what`) may not be used: it lies on a drive of the
/// source disk (it would be read while it changes, or written onto the failing drive) or of a
/// destination disk (which is locked and overwritten during the rescue).
pub fn rescue_path_problem(live: &Model, src: Device, dest: &Dest, path: &str, what: &str) -> Option<String> {
    let src_letters = Device::Disk(src.disk()).letters(live);
    on_letters_problem(path, &src_letters, what, &format!("Disk {} (the source)", src.disk())).or_else(|| match dest {
        Dest::Device(d) => on_letters_problem(
            path,
            &Device::Disk(d.disk()).letters(live),
            what,
            &format!("Disk {} (the destination)", d.disk()),
        ),
        Dest::File(_) => None,
    })
}

/// The default mapfile: `<file>.map` next to an image file; `rescue-disk<N>[-p<M>].map` in the
/// user's profile folder for a device (empty when there is none: the user chooses).
pub fn default_mapfile(dest: &Dest) -> String {
    match dest {
        Dest::File(f) if !f.trim().is_empty() => format!("{}.map", f.trim()),
        Dest::File(_) => String::new(),
        Dest::Device(d) => {
            let name = match d {
                Device::Disk(n) => format!("rescue-disk{n}.map"),
                Device::Partition(n, p) => format!("rescue-disk{n}-p{p}.map"),
            };
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|h| PathBuf::from(h).join(&name).display().to_string())
                .unwrap_or_default()
        }
    }
}

/// What an existing mapfile says (`None`: no file, a new rescue).
#[derive(Clone, Debug, PartialEq)]
pub struct Resume {
    pub len: u64,
    pub rescued: u64,
    pub bad: u64,
}

/// Why `mapfile` cannot be used with `dest`, if it cannot: it exists while the destination
/// image file does not, so resuming would mark areas rescued that are not in the new image
/// (they would silently stay zero).
pub fn missing_image_problem(dest: &Dest, mapfile: &str) -> Option<String> {
    let Dest::File(f) = dest else { return None };
    let (img, map) = (Path::new(f.trim()), Path::new(mapfile.trim()));
    (!f.trim().is_empty() && !mapfile.trim().is_empty() && !img.exists() && map.is_file()).then(|| {
        format!(
            "The mapfile {} was found but the image {} is missing; delete the mapfile to start over.",
            map.display(),
            img.display()
        )
    })
}

/// Read the mapfile at `path` (small text file) for the resume notice.
pub fn probe_mapfile(path: &str) -> Result<Option<Resume>, String> {
    let p = Path::new(path.trim());
    if path.trim().is_empty() {
        return Err("Enter the mapfile path.".into());
    }
    if !p.exists() {
        return match p.parent() {
            Some(d) if d.as_os_str().is_empty() || d.is_dir() => Ok(None),
            _ => Err("The mapfile's folder does not exist.".into()),
        };
    }
    let m = MapFile::load(p).map_err(|e| format!("The mapfile cannot be read ({e}); choose another or delete it."))?;
    let st = m.stats();
    Ok(Some(Resume { len: m.len(), rescued: st.rescued, bad: st.bad_bytes + st.non_trimmed + st.non_scraped }))
}

/// Worst status per bucket of `buckets` equal slices of the map (bad > non-scraped >
/// non-trimmed > non-tried > finished).
pub fn map_bar(map: &MapFile, buckets: usize) -> Vec<BlockStatus> {
    let len = map.len();
    if len == 0 || buckets == 0 {
        return Vec::new();
    }
    let rank = |s: BlockStatus| match s {
        BlockStatus::Finished => 0,
        BlockStatus::NonTried => 1,
        BlockStatus::NonTrimmed => 2,
        BlockStatus::NonScraped => 3,
        BlockStatus::Bad => 4,
    };
    let mut out = vec![BlockStatus::Finished; buckets];
    let mut seen = vec![false; buckets];
    for &(pos, size, st) in &map.blocks {
        if size == 0 {
            continue;
        }
        let first = (pos as u128 * buckets as u128 / len as u128) as usize;
        let last = (((pos + size - 1) as u128 * buckets as u128) / len as u128) as usize;
        for b in first..=last.min(buckets - 1) {
            if !seen[b] || rank(st) > rank(out[b]) {
                out[b] = st;
                seen[b] = true;
            }
        }
    }
    out
}

pub fn status_color(s: BlockStatus) -> Color32 {
    match s {
        BlockStatus::Finished => Color32::from_rgb(0x4C, 0xAF, 0x50),
        BlockStatus::NonTried => Color32::from_gray(150),
        BlockStatus::NonTrimmed => Color32::from_rgb(0xE0, 0xC0, 0x10),
        BlockStatus::NonScraped => Color32::from_rgb(0xE0, 0x80, 0x10),
        BlockStatus::Bad => crate::theme::DANGER,
    }
}

/// The source as the engine sees it: shared (the used map is read through a clone first),
/// read-only, remembering the offset of the latest read (`ipos`).
#[derive(Clone)]
struct SharedSource {
    inner: Arc<Mutex<Box<dyn BlockDevice>>>,
    len: u64,
    sector: u32,
    pos: Arc<AtomicU64>,
}

impl BlockDevice for SharedSource {
    fn len(&self) -> u64 {
        self.len
    }
    fn sector_size(&self) -> u32 {
        self.sector
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> pm_core::Result<()> {
        self.pos.store(offset, Ordering::Relaxed);
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).read_at(offset, buf)
    }
    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> pm_core::Result<()> {
        Err(Error::Invalid("the rescue source is read-only".into()))
    }
    fn flush(&mut self) -> pm_core::Result<()> {
        Ok(())
    }
    fn is_read_only(&self) -> bool {
        true
    }
}

/// Result of a run: the statistics or why it stopped, and the final mapfile.
pub struct RescueOutcome {
    pub stats: Result<RescueStats, String>,
    pub map: Option<MapFile>,
    pub len: u64,
    pub elapsed: Duration,
}

/// Everything the worker needs. The ranges are the ones the user confirmed; the worker
/// re-reads the layout and refuses to run when they changed.
struct Plan {
    src: Device,
    src_range: (u64, u64),
    dest: DestPlan,
    pending: Vec<Operation>,
    opts: RescueOpts,
    used_only: bool,
    mapfile: PathBuf,
}

enum DestPlan {
    /// Write to this disk / partition (confirmed at `range`) through a write session.
    Device { dev: Device, range: (u64, u64) },
    /// Write into this image file; a missing one is created in this format.
    File(PathBuf, ImageFormat),
}

/// Why `path` cannot be the mapfile, if it cannot: it must be an absolute path (a relative one
/// would resolve against the process's working folder, which the drive checks cannot see).
pub fn mapfile_path_problem(path: &str) -> Option<String> {
    let t = path.trim();
    if t.is_empty() {
        return Some("Enter the mapfile path.".into());
    }
    (!Path::new(t).is_absolute()).then(|| "The mapfile needs a full path: enter an absolute path, e.g. D:\\Rescue\\disk1.map.".into())
}

/// Re-check a plan against `fresh` (the layout read on the worker just before writing): the
/// source and destination still exist with the confirmed ranges, and every check of the
/// wizard still passes. Returns the source range to read.
fn recheck(fresh: &Model, plan: &Plan) -> Result<(u64, u64), String> {
    let changed = |d: Device| {
        format!(
            "The layout of Disk {} changed since the rescue was set up ({} moved, changed size or is gone); \
             nothing was written. Check the disks and set the rescue up again.",
            d.disk(),
            match d {
                Device::Disk(n) => format!("Disk {n}"),
                Device::Partition(n, p) => format!("partition {p} of Disk {n}"),
            }
        )
    };
    let src_now = plan.src.range(fresh).ok_or_else(|| changed(plan.src))?;
    if src_now != plan.src_range {
        return Err(changed(plan.src));
    }
    let dest = match &plan.dest {
        DestPlan::Device { dev, range } => {
            if dev.range(fresh) != Some(*range) {
                return Err(changed(*dev));
            }
            if let Some(e) = device_dest_problem(fresh, &plan.pending, plan.src, *dev) {
                return Err(e);
            }
            Dest::Device(*dev)
        }
        DestPlan::File(f, _) => Dest::File(f.display().to_string()),
    };
    if let Dest::File(f) = &dest {
        if let Some(e) = rescue_path_problem(fresh, plan.src, &dest, f, "destination file") {
            return Err(e);
        }
    }
    let mapfile = plan.mapfile.display().to_string();
    if let Some(e) = mapfile_path_problem(&mapfile)
        .or_else(|| rescue_path_problem(fresh, plan.src, &dest, &mapfile, "mapfile"))
        .or_else(|| missing_image_problem(&dest, &mapfile))
    {
        return Err(e);
    }
    Ok(src_now)
}

fn run_rescue(
    p: &dyn Platform,
    mut plan: Plan,
    pos: Arc<AtomicU64>,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> pm_core::Result<RescueOutcome> {
    let started = std::time::Instant::now();
    let fresh = Model::from_platform(p)?;
    let src_range = recheck(&fresh, &plan).map_err(Error::Invalid)?;
    // never the whole disk for a partition source: exactly the re-checked range
    let slice = matches!(plan.src, Device::Partition(..)).then_some(src_range);
    let dev = open_range(p, plan.src.disk(), slice, OpenMode::ReadOnly)?;
    let (len, sector) = (dev.len(), dev.sector_size());
    let mut src = SharedSource { inner: Arc::new(Mutex::new(dev)), len, sector, pos };
    if plan.used_only {
        let used = pm_fs::open(Box::new(src.clone())).and_then(|mut fs| fs.used_map()).map_err(|e| {
            Error::Unsupported(format!("skipping unused space needs a supported filesystem on the source ({e})"))
        })?;
        plan.opts.used_map = Some(used);
        plan.opts.zero_unused = true;
    }
    // once more right before the first write: the used-map scan can take a while
    if recheck(&Model::from_platform(p)?, &plan).map_err(Error::Invalid)? != src_range {
        return Err(Error::Invalid("The source changed while the rescue was starting; nothing was written.".into()));
    }
    let stats = match &plan.dest {
        DestPlan::Device { dev, range: (offset, _) } => {
            let disk = dev.disk();
            let mut session = p.begin_write(disk)?;
            let r = rescue(&mut src, session.device(), *offset, &plan.opts, progress, cancel);
            let fin = session.finish();
            let rescan = p.rescan(disk);
            r.and_then(|s| fin.and(rescan).map(|_| s))
        }
        DestPlan::File(path, format) => {
            let mut out = if path.exists() {
                pm_image::open(path, false)?
            } else {
                let opts = pm_image::CreateOpts { format: *format, capacity: align_up(len.max(512), 512), sparse: true };
                // a new sparse image reads as zeros: its zero blocks need no clusters
                plan.opts.skip_zero_writes = format.is_sparse();
                pm_image::create(path, &opts)?
            };
            let r = rescue(&mut src, &mut *out, 0, &plan.opts, progress, cancel);
            r.and_then(|s| out.flush().map(|_| s))
        }
    };
    let map = MapFile::load(&plan.mapfile).ok();
    Ok(RescueOutcome { stats: stats.map_err(|e| error_text(&e)), map, len, elapsed: started.elapsed() })
}

pub struct RescueWizard {
    page: usize,
    pub src: Option<Device>,
    pub dest: Dest,
    pub mapfile: String,
    /// Format of a destination image file that does not exist yet.
    pub dest_format: ImageFormat,
    /// The mapfile path follows the destination until the user edits it.
    mapfile_edited: bool,
    /// The destination the default mapfile was computed for.
    mapfile_for: Option<Dest>,
    resume: Option<(String, Result<Option<Resume>, String>)>,
    file_check: Option<(String, Option<String>)>,
    pub sector: u32,
    /// Sectors per copy-phase read.
    pub cluster: u64,
    pub skip: String,
    pub retries: u32,
    pub reverse: bool,
    pub no_trim: bool,
    pub no_scrape: bool,
    pub used_only: bool,
    task: Option<Task<RescueOutcome>>,
    pos: Arc<AtomicU64>,
    outcome: Option<RescueOutcome>,
    error: Option<String>,
    wrote_disk: bool,
}

impl RescueWizard {
    pub fn new(src: Option<Device>) -> RescueWizard {
        RescueWizard {
            page: 0,
            src,
            dest: Dest::File(String::new()),
            mapfile: String::new(),
            dest_format: ImageFormat::Raw,
            mapfile_edited: false,
            mapfile_for: None,
            resume: None,
            file_check: None,
            sector: 512,
            cluster: 256,
            skip: "64 KB".into(),
            retries: 0,
            reverse: false,
            no_trim: false,
            no_scrape: false,
            used_only: false,
            task: None,
            pos: Arc::new(AtomicU64::new(0)),
            outcome: None,
            error: None,
            wrote_disk: false,
        }
    }

    /// Choose the format of a new destination image; the file name's extension follows it.
    pub fn set_dest_format(&mut self, fmt: ImageFormat) {
        self.dest_format = fmt;
        if let Dest::File(f) = &mut self.dest {
            *f = super::image_create::fix_extension(f, fmt);
        }
    }

    /// The engine options of the filled-in pages.
    pub fn opts(&self) -> Result<RescueOpts, String> {
        let sector = self.sector.max(512) as u64;
        let block = self.cluster.max(1).checked_mul(sector).filter(|&b| b <= MAX_BLOCK).ok_or_else(|| {
            format!("Cluster size x sector size must be at most {}.", format_size(MAX_BLOCK))
        })?;
        let skip = parse_size(self.skip.trim()).map_err(|_| format!("\"{}\" is not a size.", self.skip.trim()))?;
        Ok(RescueOpts {
            sector: sector as u32,
            block_size: block as usize,
            skip_size: skip.max(sector),
            max_retries: self.retries,
            reverse: self.reverse,
            no_trim: self.no_trim,
            no_scrape: self.no_scrape,
            mapfile: Some(PathBuf::from(self.mapfile.trim())),
            ..RescueOpts::default()
        })
    }

    fn dest_problem(&mut self, env: &ToolEnv) -> Option<String> {
        let Some(src) = self.src else { return Some("Choose the source first.".into()) };
        match &self.dest {
            Dest::Device(d) => device_dest_problem(env.live, env.pending, src, *d),
            Dest::File(f) => {
                if self.file_check.as_ref().map(|c| &c.0) != Some(f) {
                    self.file_check = Some((f.clone(), file_dest_problem(f)));
                }
                let folder = self.file_check.as_ref().and_then(|c| c.1.clone());
                folder.or_else(|| rescue_path_problem(env.live, src, &self.dest, f, "destination file"))
            }
        }
    }

    /// The mapfile check: not on the source or destination disks, readable when it exists.
    fn mapfile_problem(&mut self, env: &ToolEnv) -> Result<Option<Resume>, String> {
        if let Some(e) = mapfile_path_problem(&self.mapfile) {
            return Err(e);
        }
        if let Some(src) = self.src {
            if let Some(e) = rescue_path_problem(env.live, src, &self.dest, &self.mapfile, "mapfile") {
                return Err(e);
            }
        }
        if let Some(e) = missing_image_problem(&self.dest, &self.mapfile) {
            return Err(e);
        }
        self.resume_info()
    }

    fn resume_info(&mut self) -> Result<Option<Resume>, String> {
        if self.resume.as_ref().map(|r| &r.0) != Some(&self.mapfile) {
            let mut r = probe_mapfile(&self.mapfile);
            if let Dest::File(f) = &self.dest {
                if Path::new(f.trim()) == Path::new(self.mapfile.trim()) {
                    r = Err("The mapfile cannot be the destination file.".into());
                }
            }
            self.resume = Some((self.mapfile.clone(), r));
        }
        self.resume.as_ref().map(|r| r.1.clone()).unwrap_or(Ok(None))
    }

    fn start(&mut self, env: &ToolEnv) {
        let (Some(src), Ok(opts)) = (self.src, self.opts()) else { return };
        if let Some(e) = missing_image_problem(&self.dest, &self.mapfile) {
            self.error = Some(e);
            return;
        }
        let gone = || "The source or destination no longer exists; set the rescue up again.".to_string();
        let Some(src_range) = src.range(env.live) else {
            self.error = Some(gone());
            return;
        };
        let dest = match &self.dest {
            Dest::Device(d) => match d.range(env.live) {
                Some(range) => DestPlan::Device { dev: *d, range },
                None => {
                    self.error = Some(gone());
                    return;
                }
            },
            Dest::File(f) => DestPlan::File(PathBuf::from(f.trim()), self.dest_format),
        };
        self.wrote_disk = matches!(dest, DestPlan::Device { .. });
        let plan = Plan {
            src,
            src_range,
            dest,
            pending: env.pending.to_vec(),
            opts,
            used_only: self.used_only,
            mapfile: PathBuf::from(self.mapfile.trim()),
        };
        let p = env.platform.clone();
        let pos = self.pos.clone();
        self.outcome = None;
        self.error = None;
        self.task = Some(Task::spawn("Rescue copy", move |progress, cancel| run_rescue(&*p, plan, pos, progress, cancel)));
    }

    /// Act on a footer button; returns `true` when the window should close.
    fn navigate(&mut self, nav: Nav, env: &ToolEnv) -> bool {
        let idle = self.task.is_none();
        match nav {
            Nav::Close => return idle,
            Nav::Back if idle => {
                self.outcome = None;
                self.error = None;
                self.page = self.page.saturating_sub(1);
            }
            Nav::Next if self.page + 1 < PAGES.len() => self.page += 1,
            // "New run…" on the results: back to the confirmation, never straight to a write
            Nav::Next if idle && self.outcome.is_some() => {
                self.outcome = None;
                self.error = None;
            }
            Nav::Next if idle => self.start(env),
            _ => {}
        }
        false
    }

    fn page_source(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        ui.label("The device to rescue (it is only read):");
        let devices = Device::all(env.live);
        device_combo(ui, "rescue_src", &mut self.src, &devices, env.live);
        if let Some(len) = self.src.and_then(|s| s.len(env.live)) {
            ui.label(format!("Size: {}", format_size(len)));
        }
        Footer::step(true, self.src.is_none().then(|| "Choose the source.".into()))
    }

    fn page_dest(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        let mut to_file = matches!(self.dest, Dest::File(_));
        ui.horizontal(|ui| {
            ui.radio_value(&mut to_file, true, "Image file");
            ui.radio_value(&mut to_file, false, "Disk or partition");
        });
        if to_file != matches!(self.dest, Dest::File(_)) {
            self.dest = if to_file { Dest::File(String::new()) } else { Dest::Device(Device::Disk(u32::MAX)) };
        }
        let mut new_format = None;
        match &mut self.dest {
            Dest::File(f) => {
                ui.add(egui::TextEdit::singleline(f).desired_width(420.0).hint_text("D:\\Rescue\\disk1.img"));
                ui.horizontal(|ui| {
                    ui.label("Format of a new image:");
                    let mut fmt = self.dest_format;
                    egui::ComboBox::from_id_salt("rescue_dst_fmt").selected_text(fmt.display()).show_ui(ui, |ui| {
                        for x in super::image_create::FORMATS {
                            ui.selectable_value(&mut fmt, x, x.display());
                        }
                    });
                    if fmt != self.dest_format {
                        new_format = Some(fmt);
                    }
                });
                ui.weak("A missing file is created as an image of the source's size in this format; an existing image is written into (its format is detected).");
            }
            Dest::Device(d) => {
                let devices: Vec<Device> = Device::all(env.live).into_iter().filter(|x| Some(*x) != self.src).collect();
                let mut choice = devices.contains(d).then_some(*d);
                device_combo(ui, "rescue_dst", &mut choice, &devices, env.live);
                if let Some(c) = choice {
                    *d = c;
                    danger(ui, format!("Everything on {} will be overwritten.", c.describe(env.live)));
                }
            }
        }
        if let Some(fmt) = new_format {
            self.set_dest_format(fmt);
        }
        // the default follows the destination (computed when it changes, not every frame)
        if !self.mapfile_edited && self.mapfile_for.as_ref() != Some(&self.dest) {
            self.mapfile = match &self.dest {
                Dest::Device(Device::Disk(u32::MAX)) => String::new(),
                d => default_mapfile(d),
            };
            self.mapfile_for = Some(self.dest.clone());
        }
        let problem = match &self.dest {
            Dest::Device(Device::Disk(u32::MAX)) => Some("Choose the destination.".into()),
            _ => self.dest_problem(env),
        };
        Footer::step(false, problem)
    }

    fn page_mapfile(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        ui.label("The mapfile records which areas are rescued; with it an interrupted rescue can be resumed (by this tool or GNU ddrescue).");
        if ui.add(egui::TextEdit::singleline(&mut self.mapfile).desired_width(460.0)).changed() {
            self.mapfile_edited = true;
        }
        let r = self.mapfile_problem(env);
        match &r {
            Ok(Some(res)) => {
                ui.colored_label(
                    crate::tools::WARN,
                    format!(
                        "This mapfile exists: the rescue resumes where it stopped ({} of {} rescued, {} still in error).",
                        format_size(res.rescued),
                        format_size(res.len),
                        format_size(res.bad)
                    ),
                );
            }
            Ok(None) => {
                ui.weak("A new mapfile is created.");
            }
            Err(_) => {}
        }
        Footer::step(false, r.err())
    }

    fn page_options(&mut self, ui: &mut egui::Ui) -> Footer {
        egui::Grid::new("rescue_opts").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label("Sector size:");
            egui::ComboBox::from_id_salt("rescue_sector").selected_text(format!("{} bytes", self.sector)).show_ui(ui, |ui| {
                for s in [512u32, 4096] {
                    ui.selectable_value(&mut self.sector, s, format!("{s} bytes"));
                }
            });
            ui.end_row();
            ui.label("Cluster size (sectors per read):");
            ui.add(egui::DragValue::new(&mut self.cluster).range(1..=65536));
            ui.end_row();
            ui.label("Skip size after an error:");
            ui.add(egui::TextEdit::singleline(&mut self.skip).desired_width(100.0));
            ui.end_row();
            ui.label("Retry passes:");
            ui.add(egui::DragValue::new(&mut self.retries).range(0..=20));
            ui.end_row();
        });
        ui.checkbox(&mut self.reverse, "Reverse (copy from the end towards the start)");
        ui.checkbox(&mut self.no_trim, "No trim (skip the trimming phase)");
        ui.checkbox(&mut self.no_scrape, "No scrape (skip the scraping phase)");
        ui.checkbox(&mut self.used_only, "Skip unused space (filesystem-aware; unused areas are zero-filled)");
        Footer::step(false, self.opts().err())
    }

    fn page_run(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        if let Some(t) = &self.task {
            let p = t.progress();
            let detail = |k: &str| p.detail.split(", ").find_map(|s| s.strip_prefix(k).map(|v| v.trim().to_string()));
            let live = RescueStats {
                rescued: p.done,
                bad_bytes: detail("errsize").and_then(|v| parse_size(&v).ok()).unwrap_or(0),
                bad_areas: detail("errors").and_then(|v| v.parse().ok()).unwrap_or(0),
                read_errors: detail("read errors").and_then(|v| v.parse().ok()).unwrap_or(0),
                elapsed_secs: t.elapsed().as_secs_f64(),
                ..Default::default()
            };
            ui.label(RichText::new(format!("ipos: {}", format_size(self.pos.load(Ordering::Relaxed)))).monospace());
            ui.label(RichText::new(format_rescue_status(&live, t.elapsed(), &p.phase)).monospace());
            let frac = if p.total > 0 { (p.done as f64 / p.total as f64).clamp(0.0, 1.0) as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).show_percentage());
            if t.is_cancelled() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Stopping; the mapfile is saved so the rescue can be resumed…");
                });
            } else if ui.button("Cancel").clicked() {
                t.cancel();
            }
            return Footer { back: false, next: None, close: None, problem: None };
        }
        if let Some(o) = &self.outcome {
            match &o.stats {
                Ok(s) => {
                    ui.label(RichText::new("Rescue finished.").strong());
                    ui.label(RichText::new(format_rescue_status(s, o.elapsed, "finished")).monospace());
                    let skipped = o.len.saturating_sub(s.rescued).saturating_sub(s.bad_bytes);
                    if skipped > 0 {
                        ui.label(format!("Unused areas skipped: {}", format_size(skipped)));
                    }
                    if s.bad_bytes > 0 {
                        ui.colored_label(crate::tools::WARN, "Unrescued areas remain: run again with the same mapfile (and more retries) to continue.");
                    }
                }
                Err(e) => danger(ui, format!("The rescue stopped: {e}")),
            }
            if let Some(m) = &o.map {
                ui.add_space(6.0);
                map_bar_ui(ui, m);
            }
            ui.label(format!("Mapfile: {}", self.mapfile.trim()));
            // a new run goes through the confirmation (and its checks) again
            return Footer { back: true, next: Some(("New run…".into(), true)), close: Some("Close".into()), problem: None };
        }
        // the confirmation
        let src = self.src.map_or("-".into(), |s| s.describe(env.live));
        let dst = match &self.dest {
            Dest::Device(d) => d.describe(env.live),
            Dest::File(f) if Path::new(f.trim()).exists() => f.trim().to_string(),
            Dest::File(f) => format!("{} (new {} image)", f.trim(), self.dest_format.display()),
        };
        egui::Grid::new("rescue_summary").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            for (k, v) in [("Source", src), ("Destination", dst.clone()), ("Mapfile", self.mapfile.trim().to_string())] {
                ui.strong(k);
                ui.label(v);
                ui.end_row();
            }
        });
        ui.add_space(8.0);
        let problem = self
            .dest_problem(env)
            .or_else(|| self.mapfile_problem(env).err())
            .or_else(|| self.opts().err())
            .or(self.error.clone());
        danger(ui, format!("The rescue starts immediately (it is not a pending operation) and overwrites {dst}."));
        Footer { back: true, next: Some(("Start rescue".into(), problem.is_none())), close: Some("Cancel".into()), problem }
    }
}

fn map_bar_ui(ui: &mut egui::Ui, m: &MapFile) {
    let width = ui.available_width().max(200.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
    let painter = ui.painter_at(rect);
    let bar = map_bar(m, BAR_BUCKETS);
    let w = width / bar.len().max(1) as f32;
    for (i, s) in bar.iter().enumerate() {
        let r = Rect::from_min_size(pos2(rect.min.x + i as f32 * w, rect.min.y), vec2(w + 0.5, rect.height()));
        painter.rect_filled(r, 0.0, status_color(*s));
    }
    ui.horizontal(|ui| {
        for (s, name) in [
            (BlockStatus::Finished, "rescued"),
            (BlockStatus::NonTried, "not tried"),
            (BlockStatus::NonTrimmed, "not trimmed"),
            (BlockStatus::NonScraped, "not scraped"),
            (BlockStatus::Bad, "bad"),
        ] {
            let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            ui.painter().rect_filled(r, 1.0, status_color(s));
            ui.label(name);
        }
    });
}

impl ToolWindow for RescueWizard {
    fn kind(&self) -> WindowKind {
        WindowKind::RescueCopy
    }
    fn modal(&self) -> bool {
        true
    }
    fn writing(&self) -> bool {
        self.task.is_some()
    }

    fn show(&mut self, ctx: &egui::Context, env: &ToolEnv) -> WindowOutcome {
        let mut refresh = false;
        if let Some(t) = self.task.as_mut() {
            ctx.request_repaint_after(Duration::from_millis(200));
            if let Some(r) = t.poll() {
                match r {
                    Ok(o) => self.outcome = Some(o),
                    Err(e) => self.error = Some(error_text(&e)),
                }
                self.task = None;
                refresh = self.wrote_disk;
            }
        }
        let page = self.page;
        let nav = wizard_window(ctx, "Rescue Copy", &PAGES, page, |ui| match page {
            0 => self.page_source(ui, env),
            1 => self.page_dest(ui, env),
            2 => self.page_mapfile(ui, env),
            3 => self.page_options(ui),
            _ => self.page_run(ui, env),
        });
        if self.navigate(nav, env) {
            return WindowOutcome::Close;
        }
        if refresh {
            WindowOutcome::Refresh
        } else {
            WindowOutcome::Open
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};
    use pm_core::MIB;

    #[test]
    fn rescue_status_text_formatting() {
        let s = RescueStats {
            rescued: 3 * 1024 * MIB,
            bad_bytes: 8192,
            bad_areas: 2,
            read_errors: 5,
            elapsed_secs: 0.0,
            passes: 1,
            mapfile: None,
        };
        let text = format_rescue_status(&s, Duration::from_secs(125), "Scraping");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].contains("phase: Scraping") && lines[0].contains("elapsed: 02:05"), "{text}");
        assert!(lines[1].contains("rescued: 3.00 GB") && lines[1].contains("rate: 24.58 MB/s"), "{text}");
        assert!(lines[2].contains("errsize: 8.00 KB") && lines[2].contains("bad areas: 2"), "{text}");
        assert!(lines[3].contains("read errors: 5") && lines[3].contains("retry passes: 1"), "{text}");
        // no time elapsed yet: no division by zero
        let t = format_rescue_status(&RescueStats::default(), Duration::ZERO, "Copying");
        assert!(t.contains("rate: 0 B/s") || t.contains("rate: 0.00 B/s"), "{t}");
    }

    #[test]
    fn rescue_destination_checks() {
        let mut m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        m.system_disk = Some(7);
        let (data, disk0) = (Device::Partition(1, 3), Device::Disk(0));
        assert!(device_dest_problem(&m, &[], data, disk0).is_none());
        assert!(device_dest_problem(&m, &[], Device::Disk(1), data).unwrap().contains("itself"));
        assert!(device_dest_problem(&m, &[], data, Device::Partition(1, 1)).unwrap().contains("too small"));
        assert!(device_dest_problem(&m, &[], disk0, Device::Disk(1)).is_none());
        m.system_disk = Some(0);
        assert!(device_dest_problem(&m, &[], data, disk0).unwrap().contains("system disk"));
        assert!(file_dest_problem("").is_some());
        assert!(file_dest_problem("rel.img").is_some());
        let dir = tempfile::tempdir().unwrap();
        assert!(file_dest_problem(&dir.path().join("x.img").display().to_string()).is_none());
        assert_eq!(default_mapfile(&Dest::File(" D:\\r\\a.img ".into())), "D:\\r\\a.img.map");
        assert!(default_mapfile(&Dest::Device(Device::Partition(2, 1))).ends_with("rescue-disk2-p1.map"));
    }

    #[test]
    fn rescue_options_and_map_bar() {
        let mut w = RescueWizard::new(Some(Device::Disk(1)));
        w.mapfile = "D:\\x.map".into();
        w.sector = 4096;
        w.cluster = 16;
        w.retries = 3;
        w.reverse = true;
        let o = w.opts().unwrap();
        assert_eq!((o.sector, o.block_size, o.skip_size, o.max_retries, o.reverse), (4096, 65536, 65536, 3, true));
        assert_eq!(o.mapfile.as_deref(), Some(Path::new("D:\\x.map")));
        w.cluster = 1 << 20;
        assert!(w.opts().is_err(), "read size capped");
        w.cluster = 16;
        w.skip = "lots".into();
        assert!(w.opts().is_err());

        let mut map = MapFile::new(1000);
        map.mark(0, 500, BlockStatus::Finished);
        map.mark(600, 10, BlockStatus::Bad);
        let bar = map_bar(&map, 10);
        assert_eq!(bar[0], BlockStatus::Finished);
        assert_eq!(bar[6], BlockStatus::Bad, "the worst status of the bucket");
        assert_eq!(bar[9], BlockStatus::NonTried);
    }

    // drive-letter / Windows-path logic: the mount-point equivalent arrives with the Linux and
    // macOS platform layers (multi-platform Task D)
    #[cfg(windows)]
    #[test]
    fn files_on_the_source_or_destination_disk_are_refused() {
        let mut m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        m.system_disk = Some(7);
        // E: is partition 3 of Disk 1
        let src = Device::Partition(1, 1);
        let to_file = Dest::File("E:\\x.img".into());
        let e = rescue_path_problem(&m, src, &to_file, "E:\\x.img", "destination file").unwrap();
        assert!(e.contains("drive E:") && e.contains("Disk 1 (the source)"), "{e}");
        assert!(rescue_path_problem(&m, Device::Disk(0), &to_file, "E:\\x.img", "destination file").is_none());
        // the mapfile neither on the source nor on a destination disk
        let to_disk = Dest::Device(Device::Disk(1));
        let e = rescue_path_problem(&m, Device::Disk(0), &to_disk, "e:\\r.map", "mapfile").unwrap();
        assert!(e.contains("the destination"), "{e}");
        assert!(rescue_path_problem(&m, Device::Disk(0), &to_disk, "D:\\r.map", "mapfile").is_none());
        // device destinations: the default mapfile is in the profile folder, never the cwd
        let d = default_mapfile(&Dest::Device(Device::Disk(3)));
        if let Some(h) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            assert_eq!(d, PathBuf::from(h).join("rescue-disk3.map").display().to_string());
        }
    }

    /// The wizard was set up on one layout and the disks changed before the job ran: the
    /// worker re-reads the layout and refuses without writing anything.
    #[test]
    fn a_layout_change_before_the_run_is_refused() {
        use pm_core::MemDevice;
        let mock = pm_platform::MockPlatform::new();
        for d in [gpt_disk(1), mbr_disk(0)] {
            let mut dev = MemDevice::new(d.size);
            pm_parttable::write_table(&mut dev, &pm_ops::table_from_disk(&d)).unwrap();
            mock.add_disk(d.index, dev, d);
        }
        mock.set_system_disk(7);
        let mock = Arc::new(mock);
        let p: Arc<dyn Platform> = mock.clone();
        let run = |stale: &Model, src: Device, dst: Device| -> String {
            let env = ToolEnv { platform: &p, live: stale, pending: &[] };
            let mut w = RescueWizard::new(Some(src));
            w.dest = Dest::Device(dst);
            w.mapfile = String::new();
            w.start(&env);
            let Some(t) = w.task.as_mut() else { return w.error.clone().unwrap() };
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(r) = t.poll() {
                    return match r {
                        Err(e) => error_text(&e),
                        Ok(o) => o.stats.err().unwrap_or_else(|| "it ran".into()),
                    };
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let live = Model::from_platform(&*p).unwrap();
        // the source partition moved
        let mut moved = live.clone();
        moved.partition_mut(1, 3).unwrap().start += MIB;
        let e = run(&moved, Device::Partition(1, 3), Device::Disk(0));
        assert!(e.contains("changed since the rescue was set up") && e.contains("partition 3 of Disk 1"), "{e}");
        // the source partition is gone on the disk (only the stale model has it)
        let mut extra = live.clone();
        let mut ghost = extra.partition(1, 3).unwrap().clone();
        ghost.number = 9;
        ghost.start = 801 * MIB;
        ghost.len = 100 * MIB;
        extra.disk_mut(1).unwrap().partitions.push(ghost);
        let e = run(&extra, Device::Partition(1, 9), Device::Disk(0));
        assert!(e.contains("changed since"), "{e}");
        // the destination partition changed size
        let mut dst_changed = live.clone();
        dst_changed.partition_mut(0, 1).unwrap().len += MIB;
        let e = run(&dst_changed, Device::Partition(1, 5 - 4), Device::Partition(0, 1));
        assert!(e.contains("partition 1 of Disk 0"), "{e}");
        // a partition missing from the UI model is never read as the whole disk
        let e = run(&live, Device::Partition(1, 42), Device::Disk(0));
        assert!(e.contains("no longer exists"), "{e}");
        assert!(mock.write_sessions_log().is_empty(), "nothing was written");
    }

    #[test]
    fn mapfile_must_be_an_absolute_path() {
        use crate::tools::test_env::{model, platform};
        assert!(mapfile_path_problem("rescue.map").unwrap().contains("absolute path"));
        assert!(mapfile_path_problem(r"..\rescue.map").is_some());
        assert!(mapfile_path_problem("").is_some());
        assert!(mapfile_path_problem(&std::env::temp_dir().join("disk1.map").display().to_string()).is_none());
        if cfg!(windows) {
            assert!(mapfile_path_problem(r"D:\Rescue\disk1.map").is_none());
        }
        // in the wizard: relative refused, absolute on another drive accepted
        let (m, p) = (model(), platform());
        let env = ToolEnv { platform: &p, live: &m, pending: &[] };
        let dir = tempfile::tempdir().unwrap();
        let mut w = RescueWizard::new(Some(Device::Partition(1, 3)));
        w.dest = Dest::Device(Device::Disk(0));
        w.mapfile = "rescue.map".into();
        assert!(w.mapfile_problem(&env).unwrap_err().contains("absolute path"));
        w.mapfile = dir.path().join("disk1.map").display().to_string();
        assert_eq!(w.mapfile_problem(&env), Ok(None), "a new mapfile on a drive of neither disk");
        // the worker applies the same rule
        let plan = Plan {
            src: Device::Partition(1, 3),
            src_range: Device::Partition(1, 3).range(&m).unwrap(),
            dest: DestPlan::Device { dev: Device::Disk(0), range: Device::Disk(0).range(&m).unwrap() },
            pending: vec![],
            opts: RescueOpts::default(),
            used_only: false,
            mapfile: PathBuf::from("rescue.map"),
        };
        assert!(recheck(&m, &plan).unwrap_err().contains("absolute path"));
        let plan = Plan { mapfile: dir.path().join("disk1.map"), ..plan };
        assert!(recheck(&m, &plan).is_ok());
    }

    #[test]
    fn new_run_goes_back_to_the_confirmation() {
        use crate::tools::test_env::{model, platform};
        let (m, p) = (model(), platform());
        let env = ToolEnv { platform: &p, live: &m, pending: &[] };
        let mut w = RescueWizard::new(Some(Device::Partition(1, 3)));
        w.page = PAGES.len() - 1;
        w.outcome = Some(RescueOutcome { stats: Ok(RescueStats::default()), map: None, len: MIB, elapsed: Duration::ZERO });
        w.error = Some("old".into());
        // "New run…" shows the confirmation again instead of writing
        assert!(!w.navigate(Nav::Next, &env));
        assert!(w.outcome.is_none() && w.error.is_none());
        assert!(w.task.is_none(), "no write started");
        assert_eq!(w.page, PAGES.len() - 1);
    }

    #[test]
    fn rescue_copies_an_image_file_through_a_job() {
        // a file-backed platform disk as the source, an image file as the destination
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.img");
        let mut dev = pm_image::create(&src_path, &pm_image::CreateOpts { format: pm_image::ImageFormat::Raw, capacity: 4 * MIB, sparse: false }).unwrap();
        dev.write_at(MIB, &[0xAB; 4096]).unwrap();
        dev.flush().unwrap();
        drop(dev);
        let p: Arc<dyn Platform> = Arc::new(pm_platform::FilePlatform::new(vec![src_path]).unwrap());
        let live = Model::from_platform(&*p).unwrap();
        let env = ToolEnv { platform: &p, live: &live, pending: &[] };
        let mut w = RescueWizard::new(Some(Device::Disk(live.disks[0].index)));
        let out = dir.path().join("out.img");
        w.dest = Dest::File(out.display().to_string());
        w.mapfile = default_mapfile(&w.dest);
        assert!(w.dest_problem(&env).is_none());
        w.start(&env);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let o = loop {
            if let Some(r) = w.task.as_mut().unwrap().poll() {
                break r.unwrap();
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        let s = o.stats.unwrap();
        assert_eq!(s.rescued, 4 * MIB);
        assert!(o.map.is_some());
        let mut copy = pm_image::open(&out, true).unwrap();
        let mut buf = [0u8; 4096];
        copy.read_at(MIB, &mut buf).unwrap();
        assert_eq!(buf, [0xAB; 4096]);
    }

    #[test]
    fn a_leftover_mapfile_without_its_image_is_refused() {
        use crate::tools::test_env::{model, platform};
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("gone.img");
        let map = dir.path().join("gone.img.map");
        std::fs::write(&map, MapFile::new(MIB).render(&[])).unwrap();
        let dest = Dest::File(img.display().to_string());
        let map_s = map.display().to_string();
        let e = missing_image_problem(&dest, &map_s).unwrap();
        assert!(e.contains("missing") && e.contains("delete the mapfile to start over"), "{e}");
        // no mapfile: a new rescue; an existing image: a resume; a device: not a file
        assert!(missing_image_problem(&dest, &dir.path().join("new.map").display().to_string()).is_none());
        assert!(missing_image_problem(&Dest::Device(Device::Disk(0)), &map_s).is_none());
        std::fs::write(&img, vec![0u8; 4096]).unwrap();
        assert!(missing_image_problem(&dest, &map_s).is_none());
        std::fs::remove_file(&img).unwrap();

        // in the wizard: the mapfile page and the start both refuse, nothing is created
        let (m, p) = (model(), platform());
        let env = ToolEnv { platform: &p, live: &m, pending: &[] };
        let mut w = RescueWizard::new(Some(Device::Partition(1, 3)));
        w.dest = dest;
        w.mapfile = map_s;
        assert!(w.mapfile_problem(&env).unwrap_err().contains("missing"));
        w.start(&env);
        assert!(w.task.is_none(), "no rescue started");
        assert!(w.error.as_deref().is_some_and(|e| e.contains("missing")), "{:?}", w.error);
        assert!(!img.exists());
    }

    #[test]
    fn rescue_creates_a_new_destination_image_in_the_chosen_format() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.img");
        let mut dev = pm_image::create(&src_path, &pm_image::CreateOpts { format: ImageFormat::Raw, capacity: 4 * MIB, sparse: false }).unwrap();
        dev.write_at(2 * MIB, &[0xCD; 4096]).unwrap();
        dev.flush().unwrap();
        drop(dev);
        let p: Arc<dyn Platform> = Arc::new(pm_platform::FilePlatform::new(vec![src_path]).unwrap());
        let live = Model::from_platform(&*p).unwrap();
        let env = ToolEnv { platform: &p, live: &live, pending: &[] };
        let mut w = RescueWizard::new(Some(Device::Disk(live.disks[0].index)));
        w.dest = Dest::File(dir.path().join("out.img").display().to_string());
        // the extension follows the chosen format
        w.set_dest_format(ImageFormat::Qcow2);
        let out = dir.path().join("out.qcow2");
        assert_eq!(w.dest, Dest::File(out.display().to_string()));
        w.mapfile = default_mapfile(&w.dest);
        assert!(w.dest_problem(&env).is_none());
        w.start(&env);
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let o = loop {
            if let Some(r) = w.task.as_mut().unwrap().poll() {
                break r.unwrap();
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(o.stats.unwrap().rescued, 4 * MIB);
        assert_eq!(pm_image::identify(&out).unwrap(), ImageFormat::Qcow2);
        let mut copy = pm_image::open(&out, true).unwrap();
        assert_eq!(copy.len(), 4 * MIB);
        let mut buf = [0u8; 4096];
        copy.read_at(2 * MIB, &mut buf).unwrap();
        assert_eq!(buf, [0xCD; 4096]);
    }

    #[test]
    fn every_page_renders() {
        use crate::tools::test_env::{frame, model, platform};
        let (m, p) = (model(), platform());
        let env = ToolEnv { platform: &p, live: &m, pending: &[] };
        let mut w = RescueWizard::new(Some(Device::Partition(1, 3)));
        w.dest = Dest::Device(Device::Disk(0));
        for page in 0..PAGES.len() {
            w.page = page;
            assert_eq!(frame(&mut w, &env), WindowOutcome::Open);
        }
        w.outcome = Some(RescueOutcome { stats: Ok(RescueStats::default()), map: Some(MapFile::new(MIB)), len: MIB, elapsed: Duration::from_secs(1) });
        frame(&mut w, &env);
        assert!(w.modal() && !w.writing());
    }
}
