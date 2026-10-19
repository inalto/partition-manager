//! Data Recovery wizard (MiniTool screenshot 2): Device (cards grouped "Logical drives" and
//! "Hard disks", plus "Open saved session (*.json)") → Scan type ("Deleted files" through the
//! filesystem, for FAT / NTFS / exFAT / ext volumes; "Deep scan" by file signature, with a
//! category filter) → Scan (a job with progress) → Results (name, size, type, integrity or
//! complete, path or offset; search, category filter, checkboxes; "Save session") →
//! Destination (a folder that must not be on the scanned drive, like the CLI) → Restore (a job;
//! restored / failed counts and "Save session").
//!
//! Scanning only reads; restoring writes only into the destination folder.

use crate::tools::{
    danger, error_text, open_range, progress_panel, wizard_window, Device, Footer, Nav, Task, ToolEnv, ToolWindow,
    WindowKind, WindowOutcome,
};
use eframe::egui::{self, vec2, Align, Layout, RichText, Sense, Stroke};
use egui_extras::{Column, TableBuilder};
use pm_core::{format_size, CancelToken, Error, FsKind, ProgressSink, ProgressTracker};
use pm_ops::Model;
use pm_platform::{OpenMode, Platform};
use pm_recover::{
    carve, deleted_files_at, extract_carved, restore_file, signatures, CarveReport, Category, DeletedFile, DeviceIdentity,
    ScanSession, SessionKind,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PAGES: [&str; 6] = ["Device", "Scan type", "Scan", "Results", "Destination", "Restore"];
/// Errors listed in the restore summary.
const ERROR_LIST: usize = 20;

pub const CATEGORIES: [Category; 8] = [
    Category::Image,
    Category::Document,
    Category::Archive,
    Category::Audio,
    Category::Video,
    Category::Executable,
    Category::Database,
    Category::Other,
];

pub fn category_name(c: Category) -> &'static str {
    match c {
        Category::Image => "Pictures",
        Category::Document => "Documents",
        Category::Archive => "Archives",
        Category::Audio => "Audio",
        Category::Video => "Video",
        Category::Executable => "Programs",
        Category::Database => "Databases",
        Category::Other => "Other",
    }
}

/// The category of a file name by its extension (through the carving signatures).
pub fn category_of(name: &str) -> Category {
    let Some((_, ext)) = name.rsplit_once('.') else { return Category::Other };
    signatures().iter().find(|s| s.ext.eq_ignore_ascii_case(ext)).map_or(Category::Other, |s| s.category)
}

/// Filesystems whose deleted entries can be listed.
pub fn deleted_scan_supported(fs: Option<FsKind>) -> bool {
    matches!(
        fs,
        Some(FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::Ntfs | FsKind::ExFat | FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4)
    )
}

pub use crate::tools::path_letter as dest_letter;

/// Why `dest` cannot receive recovered files from a device holding `source_letters`, if it
/// cannot: recovered data must never be written onto the drive it is recovered from.
pub fn dest_problem(dest: &str, source_letters: &[char]) -> Option<String> {
    let t = dest.trim();
    if t.is_empty() {
        return Some("Choose a destination folder.".into());
    }
    let p = Path::new(t);
    if !p.is_absolute() {
        return Some("Enter a full folder path, e.g. D:\\Recovered.".into());
    }
    match dest_letter(p) {
        Some(c) if source_letters.contains(&c) => Some(format!(
            "'{t}' is on drive {c}:, the drive being recovered from; choose a destination on another drive."
        )),
        _ => None,
    }
}

/// What was scanned.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Device(Device),
    /// A saved session file.
    Session(String),
}

/// A scan's findings.
#[derive(Clone, Debug)]
pub enum Found {
    Deleted(Vec<DeletedFile>),
    Carved(CarveReport),
}

impl Found {
    pub fn len(&self) -> usize {
        match self {
            Found::Deleted(v) => v.len(),
            Found::Carved(r) => r.files.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One row of the results table.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub name: String,
    pub size: u64,
    pub category: Category,
    /// `"80 %"` (integrity estimate) or `"complete"` / `"partial"`.
    pub quality: String,
    /// Path of a deleted file, or the byte offset of a carved one.
    pub location: String,
    /// Directories cannot be restored.
    pub restorable: bool,
}

pub fn rows(found: &Found) -> Vec<Row> {
    match found {
        Found::Deleted(v) => v
            .iter()
            .map(|f| Row {
                name: f.entry.name.trim_start_matches('?').to_string(),
                size: f.entry.size,
                category: if f.entry.is_dir { Category::Other } else { category_of(&f.entry.name) },
                quality: if f.entry.is_dir { "folder".into() } else { format!("{} %", f.integrity) },
                location: f.entry.path.clone(),
                restorable: !f.entry.is_dir,
            })
            .collect(),
        Found::Carved(r) => r
            .files
            .iter()
            .map(|f| Row {
                name: format!("{:012x}.{}", f.offset, f.ext),
                size: f.len,
                category: f.category,
                quality: if f.complete { "complete".into() } else { "partial".into() },
                location: format!("offset {}", f.offset),
                restorable: true,
            })
            .collect(),
    }
}

/// Indices of `rows` matching the search text (name or location, case-insensitive) and the
/// category filter (`None`: all).
pub fn filter(rows: &[Row], search: &str, category: Option<Category>) -> Vec<usize> {
    let needle = search.trim().to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, r)| category.map_or(true, |c| r.category == c))
        .filter(|(_, r)| needle.is_empty() || r.name.to_lowercase().contains(&needle) || r.location.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// What a restore did.
#[derive(Clone, Debug, Default)]
pub struct RestoreSummary {
    pub restored: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

/// Where the files of a scan are read from when they are restored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadFrom {
    pub disk: u32,
    /// The partition (start, len); `None`: the whole disk.
    pub range: Option<(u64, u64)>,
}

fn read_from(dev: Device, m: &Model) -> Option<ReadFrom> {
    match dev {
        Device::Disk(d) => m.disk(d).map(|_| ReadFrom { disk: d, range: None }),
        Device::Partition(..) => dev.range(m).map(|r| ReadFrom { disk: dev.disk(), range: Some(r) }),
    }
}

fn scan_job(
    p: &dyn Platform,
    from: ReadFrom,
    deleted: bool,
    cats: Vec<Category>,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> pm_core::Result<Found> {
    let mut dev = open_range(p, from.disk, from.range, OpenMode::ReadOnly)?;
    if deleted {
        let offset = from.range.map_or(0, |r| r.0);
        let mut fs = pm_fs::open(dev).map_err(|e| Error::Unsupported(format!("no readable filesystem ({e})")))?;
        let mut list = deleted_files_at(&mut *fs, offset, progress, cancel)?;
        list.sort_by(|a, b| a.entry.path.cmp(&b.entry.path));
        Ok(Found::Deleted(list))
    } else {
        let sigs: Vec<_> = signatures().iter().filter(|s| cats.contains(&s.category)).cloned().collect();
        if sigs.is_empty() {
            return Err(Error::Invalid("Choose at least one file type.".into()));
        }
        Ok(Found::Carved(carve(&mut *dev, &sigs, 512, progress, cancel)?))
    }
}

fn restore_job(
    p: &dyn Platform,
    from: ReadFrom,
    found: Found,
    chosen: Vec<usize>,
    dest: PathBuf,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> pm_core::Result<RestoreSummary> {
    let mut dev = open_range(p, from.disk, from.range, OpenMode::ReadOnly)?;
    let mut sum = RestoreSummary::default();
    let mut tracker = ProgressTracker::new(progress, "Restoring files", chosen.len() as u64);
    let note = |sum: &mut RestoreSummary, r: pm_core::Result<PathBuf>, what: &str| match r {
        Ok(_) => sum.restored += 1,
        Err(e) => {
            sum.failed += 1;
            if sum.errors.len() < ERROR_LIST {
                sum.errors.push(format!("{what}: {e}"));
            }
        }
    };
    match found {
        Found::Deleted(list) => {
            let mut fs = pm_fs::open(dev).map_err(|e| Error::Unsupported(format!("no readable filesystem ({e})")))?;
            for i in chosen {
                cancel.check()?;
                if let Some(f) = list.get(i).filter(|f| !f.entry.is_dir) {
                    let r = restore_file(&mut *fs, f, &dest);
                    note(&mut sum, r, &f.entry.path);
                }
                tracker.add(1);
            }
        }
        Found::Carved(rep) => {
            for i in chosen {
                cancel.check()?;
                if let Some(f) = rep.files.get(i) {
                    let r = extract_carved(&mut *dev, f, &dest);
                    note(&mut sum, r, &format!("offset {}", f.offset));
                }
                tracker.add(1);
            }
        }
    }
    tracker.finish();
    Ok(sum)
}

/// The session to save for `found`, scanned on `from` of `disk`: its offsets stay relative
/// to the scanned region, which the session records (`offset`, `scanned_len`).
fn session_for(found: &Found, from: ReadFrom, disk: &pm_core::DiskInfo) -> ScanSession {
    let kind = match found {
        Found::Deleted(v) => SessionKind::Deleted(v.clone()),
        Found::Carved(r) => SessionKind::Carved(r.clone()),
    };
    let (offset, len) = from.range.unwrap_or((0, disk.size));
    ScanSession::new(DeviceIdentity::from_disk(disk), kind).with_region(offset, len)
}

/// Where a loaded session's files are read from on the live disks (`None`: its disk is not
/// connected), and the device that region is (for the destination check). The region is the
/// one the session records; sessions without one (older files) fall back to the filesystem
/// offset of deleted files, or the whole disk. Bounds are checked again when the device is
/// opened.
fn session_origin(s: &ScanSession, m: &Model) -> Option<Origin> {
    let d = m.disks.iter().find(|d| s.matches(d))?;
    let region = if s.offset != 0 || s.scanned_len != 0 {
        s.region(d.size).ok()?
    } else {
        match &s.kind {
            SessionKind::Deleted(v) => match v.first().map_or(0, |f| f.fs_offset) {
                0 => (0, d.size),
                off => d.partitions.iter().find(|p| p.start == off).map(|p| (p.start, p.len))?,
            },
            _ => (0, d.size),
        }
    };
    let dev = d
        .partitions
        .iter()
        .find(|p| (p.start, p.len) == region)
        .map_or(Device::Disk(d.index), |p| Device::Partition(d.index, p.number));
    let range = (region != (0, d.size)).then_some(region);
    Some((dev, ReadFrom { disk: d.index, range }))
}

/// The device a scan's files come from and where exactly they are read.
pub type Origin = (Device, ReadFrom);

pub struct DataRecoveryWizard {
    page: usize,
    pub source: Option<Source>,
    session_path: String,
    pub deleted: bool,
    pub cats: Vec<bool>,
    scan_task: Option<Task<(Found, Option<Origin>)>>,
    /// The findings and where they were scanned, fixed at scan time (`None`: a session whose
    /// disk is not connected; the results can be looked at but not restored).
    pub found: Option<(Found, Option<Origin>)>,
    rows: Vec<Row>,
    pub checked: Vec<bool>,
    search: String,
    category: Option<Category>,
    pub dest: String,
    restore_task: Option<Task<RestoreSummary>>,
    summary: Option<RestoreSummary>,
    save_path: String,
    save_task: Option<Task<()>>,
    save_status: Option<String>,
    error: Option<String>,
}

impl DataRecoveryWizard {
    pub fn new(device: Option<Device>) -> DataRecoveryWizard {
        DataRecoveryWizard {
            page: 0,
            source: device.map(Source::Device),
            session_path: String::new(),
            deleted: false,
            cats: vec![true; CATEGORIES.len()],
            scan_task: None,
            found: None,
            rows: Vec::new(),
            checked: Vec::new(),
            search: String::new(),
            category: None,
            dest: String::new(),
            restore_task: None,
            summary: None,
            save_path: String::new(),
            save_task: None,
            save_status: None,
            error: None,
        }
    }

    fn set_found(&mut self, found: Found, dev: Option<Origin>) {
        self.rows = rows(&found);
        self.checked = self.rows.iter().map(|r| r.restorable).collect();
        self.found = Some((found, dev));
        self.summary = None;
    }

    /// The letters of the drive the files come from (the destination must avoid them).
    fn source_letters(&self, m: &Model) -> Vec<char> {
        match &self.found {
            Some((_, Some((d, _)))) => d.letters(m),
            _ => match &self.source {
                Some(Source::Device(d)) => d.letters(m),
                _ => Vec::new(),
            },
        }
    }

    fn start_scan(&mut self, env: &ToolEnv) {
        self.found = None;
        self.error = None;
        let p = env.platform.clone();
        match self.source.clone() {
            Some(Source::Device(dev)) => {
                let Some(from) = read_from(dev, env.live) else { return };
                let (deleted, cats) = (self.deleted, self.chosen_categories());
                self.scan_task = Some(Task::spawn("Data recovery scan", move |progress, cancel| {
                    scan_job(&*p, from, deleted, cats, progress, cancel).map(|f| (f, Some((dev, from))))
                }));
            }
            Some(Source::Session(path)) => {
                let live = env.live.clone();
                self.scan_task = Some(Task::spawn("Opening session", move |_progress, cancel| {
                    cancel.check()?;
                    let s = ScanSession::load(Path::new(path.trim()))?;
                    let origin = session_origin(&s, &live);
                    if let Some((_, from)) = origin {
                        // the recorded region must fit the device as it is now
                        let len = p.open_disk(from.disk, OpenMode::ReadOnly)?.len();
                        s.region(len)?;
                        if let Some((o, l)) = from.range {
                            if o.checked_add(l).map_or(true, |e| e > len) {
                                return Err(Error::Invalid("The scanned region does not fit the disk any more.".into()));
                            }
                        }
                    }
                    let found = match s.kind {
                        SessionKind::Deleted(v) => Found::Deleted(v),
                        SessionKind::Carved(r) => Found::Carved(r),
                        SessionKind::Partitions(_) => {
                            return Err(Error::Invalid("This is a partition-recovery session; it holds no files.".into()))
                        }
                    };
                    Ok((found, origin))
                }));
            }
            None => {}
        }
    }

    fn chosen_categories(&self) -> Vec<Category> {
        CATEGORIES.iter().zip(&self.cats).filter(|(_, on)| **on).map(|(c, _)| *c).collect()
    }

    fn start_restore(&mut self, env: &ToolEnv) {
        // read exactly where the scan read (a layout change since is caught by the bounds
        // check of the slice when the job opens it)
        let Some((found, Some((_, from)))) = self.found.clone() else { return };
        let chosen: Vec<usize> = self.checked.iter().enumerate().filter(|(i, c)| **c && self.rows[*i].restorable).map(|(i, _)| i).collect();
        let (p, dest) = (env.platform.clone(), PathBuf::from(self.dest.trim()));
        self.error = None;
        self.restore_task = Some(Task::spawn("Restoring files", move |progress, cancel| {
            restore_job(&*p, from, found, chosen, dest, progress, cancel)
        }));
    }

    fn start_save(&mut self, env: &ToolEnv) {
        let Some((found, Some((_, from)))) = &self.found else { return };
        let Some(disk) = env.live.disk(from.disk) else { return };
        let session = session_for(found, *from, disk);
        let path = PathBuf::from(self.save_path.trim());
        self.save_status = None;
        self.save_task = Some(Task::spawn("Saving session", move |_p, _c| session.save(&path)));
    }

    fn save_ui(&mut self, ui: &mut egui::Ui, env: &ToolEnv) {
        if !matches!(self.found, Some((_, Some(_)))) {
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Save session:");
            ui.add(egui::TextEdit::singleline(&mut self.save_path).desired_width(300.0).hint_text("D:\\scan.json"));
            let ok = Path::new(self.save_path.trim()).is_absolute() && self.save_task.is_none();
            if ui.add_enabled(ok, egui::Button::new("Save session")).clicked() {
                self.start_save(env);
            }
        });
        if let Some(s) = &self.save_status {
            ui.label(s);
        }
    }

    fn page_device(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        let m = env.live;
        ui.label(RichText::new("Logical drives").strong());
        ui.horizontal_wrapped(|ui| {
            for d in &m.disks {
                for p in d.partitions.iter().filter(|p| p.fs.is_some()) {
                    let dev = Device::Partition(d.index, p.number);
                    let title = match p.letter() {
                        Some(c) => format!("{}:{}", c, p.label),
                        None => format!("{} (Disk {} #{})", if p.label.is_empty() { "No letter" } else { &p.label }, d.index, p.number),
                    };
                    let used = p.used.map(|u| u.min(p.len));
                    let text = format!(
                        "{}\n{}",
                        p.fs.map_or("", |f| f.display_name()),
                        match used {
                            Some(u) => format!("{} free of {}", format_size(p.len - u), format_size(p.len)),
                            None => format_size(p.len),
                        }
                    );
                    let frac = used.map(|u| u as f32 / p.len.max(1) as f32);
                    if card(ui, &title, &text, frac, self.source == Some(Source::Device(dev))) {
                        self.source = Some(Source::Device(dev));
                    }
                }
            }
        });
        ui.add_space(8.0);
        ui.label(RichText::new("Hard disks").strong());
        ui.horizontal_wrapped(|ui| {
            for d in &m.disks {
                let dev = Device::Disk(d.index);
                if card(ui, &format!("Disk {}", d.index), &format!("{}\n{}", d.model, format_size(d.size)), None, self.source == Some(Source::Device(dev))) {
                    self.source = Some(Source::Device(dev));
                }
            }
        });
        ui.add_space(8.0);
        let is_session = matches!(self.source, Some(Source::Session(_)));
        if card(ui, "Open saved session", "*.json", None, is_session) && !is_session {
            self.source = Some(Source::Session(self.session_path.clone()));
        }
        if is_session {
            ui.horizontal(|ui| {
                ui.label("Session file:");
                if ui.add(egui::TextEdit::singleline(&mut self.session_path).desired_width(360.0).hint_text("D:\\scan.json")).changed() {
                    self.source = Some(Source::Session(self.session_path.clone()));
                }
            });
        }
        let problem = match &self.source {
            None => Some("Choose a drive, a disk or a saved session.".to_string()),
            Some(Source::Session(p)) if p.trim().is_empty() => Some("Enter the session file.".to_string()),
            _ => None,
        };
        let mut f = Footer::step(true, problem);
        if is_session {
            f.next = Some(("Open".into(), f.problem.is_none()));
        }
        f
    }

    fn page_type(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        let fs = match &self.source {
            Some(Source::Device(Device::Partition(d, n))) => env.live.partition(*d, *n).and_then(|p| p.fs),
            _ => None,
        };
        let can_deleted = deleted_scan_supported(fs);
        if !can_deleted {
            self.deleted = false;
        }
        ui.add_enabled_ui(can_deleted, |ui| {
            ui.radio_value(&mut self.deleted, true, "Deleted files (filesystem scan)");
        })
        .response
        .on_disabled_hover_text("Needs a FAT, NTFS, exFAT or ext volume");
        ui.indent("dr_deleted", |ui| ui.weak("Lists the deleted entries of the filesystem with their names and folders. Fast."));
        ui.radio_value(&mut self.deleted, false, "Deep scan (file carving by signature)");
        ui.indent("dr_carve", |ui| {
            ui.weak("Reads every sector and finds files by their content; names and folders are lost.");
            ui.add_enabled_ui(!self.deleted, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (i, c) in CATEGORIES.iter().enumerate() {
                        ui.checkbox(&mut self.cats[i], category_name(*c));
                    }
                });
            });
        });
        let problem = (!self.deleted && !self.cats.iter().any(|c| *c)).then(|| "Choose at least one file type.".to_string());
        Footer { back: true, next: Some(("Scan".into(), problem.is_none())), close: Some("Cancel".into()), problem }
    }

    fn page_scan(&mut self, ui: &mut egui::Ui) -> Footer {
        if let Some(t) = &self.scan_task {
            let p = t.progress();
            let bytes = p.phase == "Carving";
            if progress_panel(ui, &p, t.elapsed(), t.is_cancelled(), bytes) {
                t.cancel();
            }
            if !bytes && p.total > 0 {
                ui.label(format!("{} entries checked", p.done));
            }
            return Footer { back: false, next: None, close: Some("Cancel".into()), problem: None };
        }
        match &self.found {
            Some((f, _)) => {
                ui.label(format!("Found {} file(s).", f.len()));
                Footer::step(false, f.is_empty().then(|| "Nothing was found.".into()))
            }
            None => Footer { back: true, next: None, close: Some("Close".into()), problem: self.error.clone() },
        }
    }

    fn page_results(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        ui.horizontal(|ui| {
            ui.label("Search:");
            ui.add(egui::TextEdit::singleline(&mut self.search).desired_width(200.0));
            ui.label("Type:");
            egui::ComboBox::from_id_salt("dr_cat").selected_text(self.category.map_or("All", category_name)).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.category, None, "All");
                for c in CATEGORIES {
                    ui.selectable_value(&mut self.category, Some(c), category_name(c));
                }
            });
        });
        let shown = filter(&self.rows, &self.search, self.category);
        ui.horizontal(|ui| {
            if ui.small_button("Check shown").clicked() {
                for &i in &shown {
                    self.checked[i] = self.rows[i].restorable;
                }
            }
            if ui.small_button("Uncheck shown").clicked() {
                for &i in &shown {
                    self.checked[i] = false;
                }
            }
            let n = self.checked.iter().filter(|c| **c).count();
            ui.label(format!("{} shown, {n} checked", shown.len()));
        });
        let row_h = 20.0;
        TableBuilder::new(ui)
            .id_salt("dr_results")
            .striped(true)
            .resizable(true)
            .vscroll(true)
            .max_scroll_height(300.0)
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(24.0))
            .column(Column::initial(200.0).at_least(80.0).clip(true))
            .column(Column::initial(80.0).at_least(50.0))
            .column(Column::initial(80.0).at_least(50.0))
            .column(Column::initial(70.0).at_least(50.0))
            .column(Column::remainder().at_least(100.0).clip(true))
            .header(row_h, |mut h| {
                for t in ["", "Name", "Size", "Type", "Integrity", "Path / offset"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, shown.len(), |mut row| {
                    let i = shown[row.index()];
                    let r = &self.rows[i];
                    row.col(|ui| {
                        ui.add_enabled(r.restorable, egui::Checkbox::without_text(&mut self.checked[i]));
                    });
                    row.col(|ui| {
                        ui.label(&r.name);
                    });
                    row.col(|ui| {
                        ui.label(format_size(r.size));
                    });
                    row.col(|ui| {
                        ui.label(category_name(r.category));
                    });
                    row.col(|ui| {
                        ui.label(&r.quality);
                    });
                    row.col(|ui| {
                        ui.label(&r.location);
                    });
                });
            });
        ui.add_space(4.0);
        self.save_ui(ui, env);
        let problem = if !matches!(self.found, Some((_, Some(_)))) {
            Some("The scanned disk is not connected: the files can be listed but not restored.".to_string())
        } else if !self.checked.iter().any(|c| *c) {
            Some("Check the files to restore.".to_string())
        } else {
            None
        };
        Footer::step(false, problem)
    }

    fn page_dest(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        ui.label("Restore the checked files into this folder (created if missing):");
        ui.add(egui::TextEdit::singleline(&mut self.dest).desired_width(420.0).hint_text("D:\\Recovered"));
        let letters = self.source_letters(env.live);
        if !letters.is_empty() {
            ui.weak(format!(
                "Not on {}: writing to the drive being recovered could overwrite the files.",
                letters.iter().map(|c| format!("{c}:")).collect::<Vec<_>>().join(", ")
            ));
        }
        let problem = dest_problem(&self.dest, &letters);
        Footer { back: true, next: Some(("Restore".into(), problem.is_none())), close: Some("Cancel".into()), problem }
    }

    fn page_restore(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        if let Some(t) = &self.restore_task {
            if progress_panel(ui, &t.progress(), t.elapsed(), t.is_cancelled(), false) {
                t.cancel();
            }
            return Footer { back: false, next: None, close: None, problem: None };
        }
        match &self.summary {
            Some(s) => {
                ui.label(RichText::new(format!("Restored {} file(s), {} failed.", s.restored, s.failed)).strong());
                ui.label(format!("Folder: {}", self.dest.trim()));
                for e in &s.errors {
                    ui.colored_label(crate::theme::DANGER, e);
                }
                if s.failed > s.errors.len() {
                    ui.weak(format!("… and {} more", s.failed - s.errors.len()));
                }
            }
            None => {
                if let Some(e) = &self.error {
                    danger(ui, e.as_str());
                }
            }
        }
        ui.add_space(8.0);
        self.save_ui(ui, env);
        Footer { back: true, next: None, close: Some("Close".into()), problem: None }
    }
}

/// A selectable device card; returns `true` when clicked.
fn card(ui: &mut egui::Ui, title: &str, text: &str, used: Option<f32>, selected: bool) -> bool {
    let stroke = if selected {
        Stroke::new(2.0_f32, crate::theme::SELECTED)
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    let resp = egui::Frame::group(ui.style())
        .stroke(stroke)
        .show(ui, |ui| {
            ui.set_width(150.0);
            ui.set_min_height(64.0);
            ui.label(RichText::new(title).strong());
            ui.label(text);
            if let Some(f) = used {
                let (r, _) = ui.allocate_exact_size(vec2(140.0, 6.0), Sense::hover());
                ui.painter().rect_filled(r, 2.0, ui.visuals().widgets.inactive.bg_fill);
                let mut u = r;
                u.set_width(r.width() * f.clamp(0.0, 1.0));
                ui.painter().rect_filled(u, 2.0, crate::theme::ACCENT);
            }
        })
        .response;
    ui.interact(resp.rect, resp.id.with("card"), Sense::click()).clicked()
}

impl ToolWindow for DataRecoveryWizard {
    fn kind(&self) -> WindowKind {
        WindowKind::DataRecovery
    }
    fn modal(&self) -> bool {
        true
    }
    fn writing(&self) -> bool {
        self.restore_task.is_some() || self.save_task.is_some()
    }

    fn show(&mut self, ctx: &egui::Context, env: &ToolEnv) -> WindowOutcome {
        let busy = self.scan_task.is_some() || self.restore_task.is_some() || self.save_task.is_some();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        if let Some(r) = self.scan_task.as_mut().and_then(|t| t.poll()) {
            self.scan_task = None;
            match r {
                Ok((f, dev)) => self.set_found(f, dev),
                Err(e) => self.error = Some(error_text(&e)),
            }
        }
        if let Some(r) = self.restore_task.as_mut().and_then(|t| t.poll()) {
            self.restore_task = None;
            match r {
                Ok(s) => self.summary = Some(s),
                Err(e) => self.error = Some(error_text(&e)),
            }
        }
        if let Some(r) = self.save_task.as_mut().and_then(|t| t.poll()) {
            self.save_task = None;
            self.save_status = Some(match r {
                Ok(()) => format!("Session saved to {}.", self.save_path.trim()),
                Err(e) => format!("Could not save the session: {}", error_text(&e)),
            });
        }
        let page = self.page;
        let nav = wizard_window(ctx, "Data Recovery", &PAGES, page, |ui| match page {
            0 => self.page_device(ui, env),
            1 => self.page_type(ui, env),
            2 => self.page_scan(ui),
            3 => self.page_results(ui, env),
            4 => self.page_dest(ui, env),
            _ => self.page_restore(ui, env),
        });
        let idle = self.scan_task.is_none() && self.restore_task.is_none();
        match nav {
            Nav::Close if !self.writing() => return WindowOutcome::Close,
            Nav::Back if idle => {
                self.error = None;
                // a session skips the scan type page
                self.page = match (page, &self.source) {
                    (2, Some(Source::Session(_))) => 0,
                    _ => page.saturating_sub(1),
                };
            }
            Nav::Next => match page {
                0 if matches!(self.source, Some(Source::Session(_))) => {
                    self.start_scan(env);
                    self.page = 2;
                }
                1 => {
                    self.start_scan(env);
                    self.page = 2;
                }
                4 => {
                    self.summary = None;
                    self.start_restore(env);
                    self.page = 5;
                }
                _ => self.page = (page + 1).min(PAGES.len() - 1),
            },
            _ => {}
        }
        WindowOutcome::Open
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{BlockDevice, TableKind, MIB};
    use pm_recover::CarvedFile;
    use std::sync::Arc;

    #[test]
    fn destination_must_not_be_on_the_source() {
        assert!(dest_problem("", &['E']).is_some());
        assert!(dest_problem("Recovered", &[]).unwrap().contains("full folder path"));
        let e = dest_problem("e:\\Recovered", &['E']).unwrap();
        assert!(e.contains("drive E:"), "{e}");
        assert!(dest_problem("\\\\?\\E:\\Recovered", &['E']).is_some(), "verbatim paths too");
        assert!(dest_problem("D:\\Recovered", &['E']).is_none());
        assert!(dest_problem("\\\\server\\share\\x", &['E']).is_none());
        assert_eq!(dest_letter(Path::new("c:\\x")), Some('C'));
    }

    #[test]
    fn rows_filter_and_categories() {
        let rep = CarveReport {
            files: vec![
                CarvedFile { offset: 4096, len: 1000, sig: "JPEG".into(), ext: "jpg".into(), category: Category::Image, complete: true },
                CarvedFile { offset: 8192, len: 50, sig: "PDF".into(), ext: "pdf".into(), category: Category::Document, complete: false },
            ],
            unreadable: vec![],
            bytes_scanned: 1 << 20,
        };
        let r = rows(&Found::Carved(rep.clone()));
        assert_eq!(r[0].name, "000000001000.jpg");
        assert_eq!(r[1].quality, "partial");
        assert_eq!(filter(&r, "", None), [0, 1]);
        assert_eq!(filter(&r, "", Some(Category::Document)), [1]);
        assert_eq!(filter(&r, "1000", None), [0]);
        assert_eq!(filter(&r, "offset 8192", None), [1]);
        assert_eq!(category_of("holiday.JPG"), Category::Image);
        assert_eq!(category_of("noext"), Category::Other);
        assert!(deleted_scan_supported(Some(FsKind::Ntfs)) && !deleted_scan_supported(Some(FsKind::ReFs)));
        // saved sessions keep region-relative offsets and record the region
        let disk = crate::test_util::gpt_disk(1);
        let s = session_for(&Found::Carved(rep), ReadFrom { disk: 1, range: Some((117 * MIB, 684 * MIB)) }, &disk);
        assert_eq!((s.offset, s.scanned_len), (117 * MIB, 684 * MIB));
        let SessionKind::Carved(r) = &s.kind else { panic!() };
        assert_eq!(r.files[0].offset, 4096);
        let m = crate::test_util::model_of(vec![disk.clone()]);
        assert_eq!(session_origin(&s, &m), Some((Device::Partition(1, 3), ReadFrom { disk: 1, range: Some((117 * MIB, 684 * MIB)) })));
        // a legacy (region-less) carve session: the whole disk
        let legacy = ScanSession { offset: 0, scanned_len: 0, ..s.clone() };
        assert_eq!(session_origin(&legacy, &m), Some((Device::Disk(1), ReadFrom { disk: 1, range: None })));
        // a region past the end of the disk is not offered for restoring
        let mut small = disk;
        small.size = 512 * MIB;
        let s2 = ScanSession { device: DeviceIdentity::from_disk(&small), ..s };
        assert!(session_origin(&s2, &crate::test_util::model_of(vec![small])).is_none());
    }

    /// Deleted files of a FAT32 volume on a disk image are listed and restored into a folder.
    #[test]
    fn scan_and_restore_deleted_files_on_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vol.img");
        let mut dev = pm_image::create(&path, &pm_image::CreateOpts { format: pm_image::ImageFormat::Raw, capacity: 64 * MIB, sparse: false }).unwrap();
        let mut t = pm_parttable::Table::empty(TableKind::Gpt, 64 * MIB, 512);
        t.entries.push(pm_parttable::Entry {
            number: 1,
            start: MIB,
            len: 40 * MIB,
            ptype: pm_core::PartitionType::Gpt(pm_core::gpt_types::BASIC_DATA),
            guid: Some(uuid::Uuid::from_u128(7)),
            name: String::new(),
            attrs: 0,
            logical: false,
        });
        pm_parttable::write_table(&mut *dev, &t).unwrap();
        {
            let mut slice = pm_core::SliceDevice::new(&mut *dev, MIB, 40 * MIB).unwrap();
            pm_fs::format_fat32(&mut slice, &pm_fs::FormatOpts { label: "DATA".into(), cluster_size: None, volume_id: Some(1) }).unwrap();
        }
        // a PDF in the free clusters of the volume, for the deep scan
        let pdf = b"%PDF-1.4
1 0 obj << >> endobj
trailer << >>
%%EOF
";
        dev.write_at(MIB + 30 * MIB, pdf).unwrap();
        dev.flush().unwrap();
        drop(dev);
        let p: Arc<dyn Platform> = Arc::new(pm_platform::FilePlatform::new(vec![path]).unwrap());
        let live = Model::from_platform(&*p).unwrap();
        let disk = live.disks[0].index;
        let from = read_from(Device::Partition(disk, 1), &live).unwrap();
        let (np, c) = (pm_core::NullProgress, CancelToken::new());
        // a freshly formatted volume has no deleted files; the scan still runs through
        let found = scan_job(&*p, from, true, vec![], &np, &c).unwrap();
        assert!(matches!(found, Found::Deleted(_)));
        let carved = scan_job(&*p, from, false, vec![Category::Document], &np, &c).unwrap();
        let Found::Carved(rep) = &carved else { panic!() };
        let i = rep.files.iter().position(|f| f.offset == 30 * MIB && f.ext == "pdf").expect("the PDF is carved");
        let out = dir.path().join("out");
        let sum = restore_job(&*p, from, carved.clone(), vec![i], out.clone(), &np, &c).unwrap();
        assert_eq!((sum.restored, sum.failed), (1, 0), "{:?}", sum.errors);
        let restored = std::fs::read(out.join(format!("{:012x}.pdf", 30 * MIB))).unwrap();
        assert!(restored.starts_with(b"%PDF-1.4") && restored.windows(5).any(|w| w == b"%%EOF"), "{restored:?}");
        assert!(restored.len() <= pdf.len() + 2);
        assert!(scan_job(&*p, from, false, vec![], &np, &c).is_err(), "no file type chosen");
        // a session of the partition scan restores the same bytes from its recorded region
        let sp = dir.path().join("scan.json");
        session_for(&carved, from, live.disk(disk).unwrap()).save(&sp).unwrap();
        let loaded = ScanSession::load(&sp).unwrap();
        assert_eq!((loaded.offset, loaded.scanned_len), (MIB, 40 * MIB));
        let (dev, origin) = session_origin(&loaded, &live).unwrap();
        assert_eq!((dev, origin), (Device::Partition(disk, 1), from));
        let SessionKind::Carved(r) = loaded.kind else { panic!() };
        let out2 = dir.path().join("out2");
        let sum = restore_job(&*p, origin, Found::Carved(r), vec![i], out2.clone(), &np, &c).unwrap();
        assert_eq!(sum.restored, 1);
        assert_eq!(std::fs::read(out2.join(format!("{:012x}.pdf", 30 * MIB))).unwrap(), restored);
    }

    #[test]
    fn every_page_renders() {
        use crate::tools::test_env::{frame, model, platform};
        let (m, p) = (model(), platform());
        let env = ToolEnv { platform: &p, live: &m, pending: &[] };
        let mut w = DataRecoveryWizard::new(Some(Device::Partition(1, 3)));
        let rep = CarveReport {
            files: vec![CarvedFile { offset: 4096, len: 1000, sig: "JPEG".into(), ext: "jpg".into(), category: Category::Image, complete: true }],
            unreadable: vec![],
            bytes_scanned: 1 << 20,
        };
        w.set_found(Found::Carved(rep), Some((Device::Partition(1, 3), ReadFrom { disk: 1, range: Some((117 * MIB, 684 * MIB)) })));
        w.summary = Some(RestoreSummary { restored: 1, failed: 1, errors: vec!["x: failed".into()] });
        for page in 0..PAGES.len() {
            w.page = page;
            assert_eq!(frame(&mut w, &env), WindowOutcome::Open);
        }
        w.source = Some(Source::Session(String::new()));
        w.page = 0;
        frame(&mut w, &env);
        // the destination may not be on E:, where partition 3 is mounted
        assert_eq!(w.source_letters(&m), ['E']);
    }
}
