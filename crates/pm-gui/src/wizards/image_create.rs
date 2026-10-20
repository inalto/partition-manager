//! Create Image wizard: Source (a disk or partition, from the selection) → Image file (a
//! full path whose folder exists, the format, the extension fixed to match it) → Copy mode →
//! Damaged disk (optional ddrescue mode: retries, skip size, phases, mapfile) → summary →
//! [`Operation::ImageDisk`] / [`Operation::ImagePartition`].
//!
//! The Damaged disk page exists here only: Restore Image reads an image file, it has no
//! failing source to rescue.

use super::{disk_preview, row};
use crate::selection::Selected;
use crate::tools::{on_letters_problem, Device};
use crate::wizard::{shared, StepResult, Wizard, WizardCtx};
use eframe::egui;
use pm_core::format_size;
use pm_image::ImageFormat;
use pm_ops::{CloneMode, Model, Operation, RescueSettings};
use std::path::{Path, PathBuf};

/// Formats offered, in menu order.
pub const FORMATS: [ImageFormat; 6] = [
    ImageFormat::Raw,
    ImageFormat::VhdDynamic,
    ImageFormat::VhdFixed,
    ImageFormat::VmdkSparse,
    ImageFormat::VmdkFlat,
    ImageFormat::Qcow2,
];

const IMAGE_EXTS: [&str; 6] = ["img", "raw", "dd", "vhd", "vmdk", "qcow2"];

/// Initial skip sizes offered on the Damaged disk page (the first is the default).
pub const SKIP_SIZES: [u64; 5] = [64 << 10, 256 << 10, 1 << 20, 4 << 20, 16 << 20];
/// Most retry passes offered.
pub const MAX_RETRIES: u32 = 10;

/// Why `map` cannot be the mapfile of the rescue-mode image `image`, if it cannot: the same
/// rules as the Rescue wizard (a full path whose folder exists, not on a drive of the source),
/// and not the image itself.
pub fn mapfile_problem(map: &str, image: &str, format: ImageFormat, source_letters: &[char]) -> Option<String> {
    let t = map.trim();
    if t.is_empty() {
        return Some("Enter the mapfile path.".into());
    }
    let p = Path::new(t);
    if !p.is_absolute() {
        return Some("The mapfile needs a full path: enter an absolute path, e.g. D:\\Backups\\disk1.vhd.map.".into());
    }
    if let Some(e) = on_letters_problem(t, source_letters, "mapfile", "the device being imaged") {
        return Some(e);
    }
    if p.file_name().is_none() || t.ends_with(['\\', '/']) {
        return Some("The mapfile path names a folder, not a file.".into());
    }
    match p.parent() {
        Some(dir) if dir.is_dir() => {}
        Some(dir) => return Some(format!("Folder {} for the mapfile does not exist.", dir.display())),
        None => return Some("Enter a full path for the mapfile.".into()),
    }
    let image = image.trim();
    let partial = format!("{image}.partial");
    let mut files = vec![image.to_string(), partial.clone()];
    if format == ImageFormat::VmdkFlat {
        // pm-image writes the data of a flat VMDK to `<stem>-flat.vmdk` next to the descriptor
        for d in [image, partial.as_str()] {
            let d = Path::new(d);
            if let Some(stem) = d.file_stem().and_then(|s| s.to_str()) {
                files.push(d.with_file_name(format!("{stem}-flat.vmdk")).display().to_string());
            }
        }
    }
    if files.iter().any(|f| t.eq_ignore_ascii_case(f)) {
        return Some("The mapfile cannot be the image file itself.".into());
    }
    None
}

/// `path` with the extension of `fmt`: a missing or image extension is replaced
/// (`disk.vhd` → `disk.qcow2`), any other is kept and the image extension appended
/// (`backup.2024` → `backup.2024.img`). An empty path stays empty.
pub fn fix_extension(path: &str, fmt: ImageFormat) -> String {
    let t = path.trim();
    if t.is_empty() || t.ends_with(['\\', '/']) {
        return t.to_string();
    }
    let p = Path::new(t);
    let want = fmt.extension();
    match p.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case(want) => t.to_string(),
        Some(e) if IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()) => p.with_extension(want).display().to_string(),
        Some(_) => format!("{t}.{want}"),
        None => p.with_extension(want).display().to_string(),
    }
}

/// Why `path` cannot be the new image file, if it cannot: empty, not a full path, a folder,
/// or its folder does not exist.
pub fn path_problem(path: &str) -> Option<String> {
    let t = path.trim();
    if t.is_empty() {
        return Some("Enter the path of the image file.".into());
    }
    let p = Path::new(t);
    if !p.is_absolute() {
        return Some("Enter a full path, e.g. D:\\Backups\\disk1.vhd.".into());
    }
    if p.file_name().is_none() || t.ends_with(['\\', '/']) {
        return Some("The path names a folder, not a file.".into());
    }
    match p.parent() {
        Some(dir) if dir.is_dir() => None,
        Some(dir) => Some(format!("Folder {} does not exist.", dir.display())),
        None => Some("Enter a full path.".into()),
    }
}

pub struct ImageCreateState {
    pub source: Device,
    pub path: String,
    pub format: ImageFormat,
    pub mode: CloneMode,
    /// [`path_problem`] of the last checked path (checked when the text changes).
    checked: Option<(String, Option<String>)>,
    /// Drive letters on the source (the image may not be written there: it would read what
    /// it writes). Updated from the model whenever a step is drawn.
    pub source_letters: Vec<char>,
    /// Damaged disk page: continue on read errors (ddrescue mode).
    pub rescue: bool,
    /// Retry passes over the bad sectors (0..=[`MAX_RETRIES`]).
    pub retries: u32,
    /// Initial skip after a read error (one of [`SKIP_SIZES`]).
    pub skip_size: u64,
    pub no_trim: bool,
    pub no_scrape: bool,
    pub reverse: bool,
    /// The mapfile typed by the user; `None` while it follows the image path
    /// (`<image>.map`, see [`ImageCreateState::mapfile_path`]).
    mapfile: Option<String>,
}

impl ImageCreateState {
    pub fn initial(ctx: &WizardCtx) -> Option<ImageCreateState> {
        let all = Device::all(ctx.model);
        let source = Device::from_selection(ctx.selection).filter(|d| all.contains(d)).or_else(|| all.first().copied())?;
        Some(ImageCreateState {
            source,
            path: String::new(),
            format: ImageFormat::VhdDynamic,
            mode: CloneMode::Smart,
            checked: None,
            source_letters: source.letters(ctx.model),
            rescue: false,
            retries: 0,
            skip_size: SKIP_SIZES[0],
            no_trim: false,
            no_scrape: false,
            reverse: false,
            mapfile: None,
        })
    }

    /// The mapfile of rescue mode: the one typed, else `<image>.map`.
    pub fn mapfile_path(&self) -> String {
        match &self.mapfile {
            Some(m) => m.clone(),
            None if self.path.trim().is_empty() => String::new(),
            None => format!("{}.map", self.path.trim()),
        }
    }

    /// Use `path` as the mapfile (it stops following the image path).
    pub fn set_mapfile(&mut self, path: String) {
        self.mapfile = Some(path);
    }

    /// Why the Damaged disk page cannot be left, if it cannot (only in rescue mode).
    pub fn damaged_problem(&self) -> Option<String> {
        if !self.rescue {
            return None;
        }
        mapfile_problem(&self.mapfile_path(), &self.path, self.format, &self.source_letters)
    }

    /// The rescue settings of the operation (`None` unless rescue mode is on).
    pub fn rescue_settings(&self) -> Option<RescueSettings> {
        self.rescue.then(|| RescueSettings {
            retries: self.retries.min(MAX_RETRIES),
            skip_size: (self.skip_size != SKIP_SIZES[0]).then_some(self.skip_size),
            cluster_size: None,
            no_trim: self.no_trim,
            no_scrape: self.no_scrape,
            reverse: self.reverse,
            mapfile: self.mapfile.as_ref().map(|m| PathBuf::from(m.trim())),
        })
    }

    fn path_check(&mut self) -> Option<String> {
        if self.checked.as_ref().map(|c| &c.0) != Some(&self.path) {
            self.checked = Some((self.path.clone(), path_problem(&self.path)));
        }
        self.checked.as_ref().and_then(|c| c.1.clone())
    }

    /// Why the image may not be written where `path` says: on the device being imaged.
    pub fn on_source_problem(&self) -> Option<String> {
        on_letters_problem(&self.path, &self.source_letters, "image file", "the device being imaged")
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        let t = self.path.trim();
        if t.is_empty() {
            return Err("Enter the path of the image file.".into());
        }
        if let Some(e) = self.on_source_problem().or_else(|| self.damaged_problem()) {
            return Err(e);
        }
        let path = PathBuf::from(t);
        let (format, mode, rescue) = (self.format, self.mode, self.rescue_settings());
        Ok(vec![match self.source {
            Device::Disk(disk) => Operation::ImageDisk { disk, path, format, mode, rescue },
            Device::Partition(disk, number) => Operation::ImagePartition { disk, number, path, format, mode, rescue },
        }])
    }
}

fn source_ui(s: &mut ImageCreateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    s.source_letters = s.source.letters(ctx.model);
    let m: &Model = ctx.model;
    ui.label("Select the disk or partition to image:");
    let mut choice = Some(s.source);
    crate::tools::device_combo(ui, "image_src", &mut choice, &Device::all(m), m);
    if let Some(c) = choice {
        s.source = c;
    }
    if let Some(d) = m.disk(s.source.disk()) {
        ui.add_space(8.0);
        let hl = match s.source {
            Device::Partition(d, n) => Some(Selected::Partition(d, n)),
            Device::Disk(_) => None,
        };
        disk_preview(ui, d, hl);
        if let Some(len) = s.source.len(m) {
            ui.label(format!("Size: {}", format_size(len)));
        }
    }
    StepResult::Continue
}

fn file_ui(s: &mut ImageCreateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    s.source_letters = s.source.letters(ctx.model);
    egui::Grid::new("image_file").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Image file:");
        let resp = ui.add(egui::TextEdit::singleline(&mut s.path).desired_width(380.0).hint_text("D:\\Backups\\disk1.vhd"));
        if resp.lost_focus() {
            s.path = fix_extension(&s.path, s.format);
        }
        ui.end_row();
        ui.label("Format:");
        let before = s.format;
        egui::ComboBox::from_id_salt("image_fmt").selected_text(s.format.display()).show_ui(ui, |ui| {
            for f in FORMATS {
                ui.selectable_value(&mut s.format, f, f.display());
            }
        });
        if s.format != before {
            s.path = fix_extension(&s.path, s.format);
        }
        ui.end_row();
    });
    if s.format.is_sparse() {
        ui.weak("Sparse format: unused areas take no space in the file.");
    }
    ui.weak("An existing file of that name is replaced.");
    StepResult::from_problem(s.path_check().or_else(|| s.on_source_problem()))
}

fn mode_ui(s: &mut ImageCreateState, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
    ui.radio_value(&mut s.mode, CloneMode::Smart, super::copy_disk::mode_name(CloneMode::Smart));
    ui.indent("img_smart", |ui| ui.weak("Only the blocks the filesystems use are read; the rest of the image stays empty."));
    ui.radio_value(&mut s.mode, CloneMode::Sector, super::copy_disk::mode_name(CloneMode::Sector));
    ui.indent("img_sector", |ui| ui.weak("Every sector is copied (also unknown filesystems and deleted data)."));
    StepResult::Continue
}

fn damaged_ui(s: &mut ImageCreateState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    s.source_letters = s.source.letters(ctx.model);
    ui.checkbox(&mut s.rescue, "Continue on read errors (ddrescue mode)");
    ui.indent("img_rescue_why", |ui| {
        ui.weak(
            "Unreadable blocks are retried, then written as zeros; a mapfile next to the image records them \
             so the export can be resumed (run the same operation again).",
        );
    });
    if !s.rescue {
        ui.weak("Without it the first unreadable sector stops the export and no image is kept.");
        return StepResult::Continue;
    }
    ui.add_space(6.0);
    egui::Grid::new("img_rescue").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("Retry passes:");
        ui.add(egui::DragValue::new(&mut s.retries).range(0..=MAX_RETRIES));
        ui.end_row();
        ui.label("Skip after an error:");
        egui::ComboBox::from_id_salt("img_rescue_skip").selected_text(format_size(s.skip_size)).show_ui(ui, |ui| {
            for k in SKIP_SIZES {
                ui.selectable_value(&mut s.skip_size, k, format_size(k));
            }
        });
        ui.end_row();
        ui.label("Mapfile:");
        let mut text = s.mapfile_path();
        if ui.add(egui::TextEdit::singleline(&mut text).desired_width(340.0)).changed() {
            s.set_mapfile(text);
        }
        ui.end_row();
    });
    let mut trim = !s.no_trim;
    let mut scrape = !s.no_scrape;
    ui.checkbox(&mut trim, "Trim (read the edges of failed blocks sector by sector)");
    ui.checkbox(&mut scrape, "Scrape (read the rest of failed blocks sector by sector)");
    ui.checkbox(&mut s.reverse, "Reverse (copy from the end towards the start)");
    (s.no_trim, s.no_scrape) = (!trim, !scrape);
    StepResult::from_problem(s.damaged_problem())
}

fn source_summary(s: &ImageCreateState) -> Vec<(String, String)> {
    vec![row(
        "Source",
        match s.source {
            Device::Disk(d) => format!("Disk {d}"),
            Device::Partition(d, n) => format!("Partition {n} on Disk {d}"),
        },
    )]
}

fn file_summary(s: &ImageCreateState) -> Vec<(String, String)> {
    vec![row("Image file", s.path.trim()), row("Format", s.format.display())]
}

fn mode_summary(s: &ImageCreateState) -> Vec<(String, String)> {
    vec![row("Copy mode", super::copy_disk::mode_name(s.mode))]
}

fn damaged_summary(s: &ImageCreateState) -> Vec<(String, String)> {
    if !s.rescue {
        return vec![row("Read errors", "Stop at the first unreadable sector")];
    }
    let mut phases = vec!["copy"];
    if !s.no_trim {
        phases.push("trim");
    }
    if !s.no_scrape {
        phases.push("scrape");
    }
    if s.retries > 0 {
        phases.push("retry");
    }
    let mut phases = phases.join(", ");
    if s.reverse {
        phases.push_str(" (reverse)");
    }
    vec![
        row("Read errors", format!("Continue (ddrescue mode), {} retries", s.retries)),
        row("Skip size", format_size(s.skip_size)),
        row("Phases", phases),
        row("Mapfile", s.mapfile_path()),
    ]
}

pub fn wizard(state: ImageCreateState) -> Wizard {
    Wizard::over_state(
        "Create Image",
        shared(state),
        vec![
            ("Source", source_ui, source_summary),
            ("Image file", file_ui, file_summary),
            ("Copy mode", mode_ui, mode_summary),
            ("Damaged disk", damaged_ui, damaged_summary),
        ],
        ImageCreateState::build,
    )
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    ImageCreateState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, model_of};
    use crate::wizard::check_ops;

    #[test]
    fn extension_follows_the_format() {
        assert_eq!(fix_extension("C:\\b\\disk", ImageFormat::Raw), "C:\\b\\disk.img");
        assert_eq!(fix_extension("C:\\b\\disk.vhd", ImageFormat::Qcow2), "C:\\b\\disk.qcow2");
        assert_eq!(fix_extension("C:\\b\\disk.VMDK", ImageFormat::VmdkFlat), "C:\\b\\disk.VMDK");
        assert_eq!(fix_extension("C:\\b\\backup.2024", ImageFormat::Raw), "C:\\b\\backup.2024.img");
        assert_eq!(fix_extension("  ", ImageFormat::Raw), "");
    }

    #[test]
    fn image_path_validation() {
        let dir = tempfile::tempdir().unwrap();
        assert!(path_problem("").is_some());
        assert!(path_problem("disk.img").unwrap().contains("full path"));
        assert!(path_problem(&dir.path().join("x").join("disk.img").display().to_string()).unwrap().contains("does not exist"));
        assert!(path_problem(&dir.path().join("disk.img").display().to_string()).is_none());
    }

    #[test]
    fn build_image_ops_from_filled_state() {
        let dir = tempfile::tempdir().unwrap();
        let m = model_of(vec![gpt_disk(1)]);
        let ctx = WizardCtx::new(&m, vec![], Selected::Partition(1, 3));
        let mut s = ImageCreateState::initial(&ctx).unwrap();
        assert_eq!(s.source, Device::Partition(1, 3));
        assert!(s.build().is_err(), "no path yet");
        let path = dir.path().join("data.qcow2");
        s.path = path.display().to_string();
        s.format = ImageFormat::Qcow2;
        let ops = s.build().unwrap();
        assert_eq!(
            ops,
            vec![Operation::ImagePartition { disk: 1, number: 3, path: path.clone(), format: ImageFormat::Qcow2, mode: CloneMode::Smart, rescue: None }]
        );
        assert!(check_ops(&m, &ops).is_ok());
        s.source = Device::Disk(1);
        s.mode = CloneMode::Sector;
        s.format = ImageFormat::VhdFixed;
        let ops = wizard(s).build_ops().unwrap();
        assert_eq!(ops, vec![Operation::ImageDisk { disk: 1, path, format: ImageFormat::VhdFixed, mode: CloneMode::Sector, rescue: None }]);
    }

    #[test]
    fn build_image_op_with_rescue_settings() {
        let dir = tempfile::tempdir().unwrap();
        let m = model_of(vec![gpt_disk(1)]);
        let ctx = WizardCtx::new(&m, vec![], Selected::Partition(1, 3));
        let mut s = ImageCreateState::initial(&ctx).unwrap();
        let path = dir.path().join("data.vhd");
        s.path = path.display().to_string();
        // off by default: the first read error stops the export
        assert!(!s.rescue);
        assert!(matches!(&s.build().unwrap()[0], Operation::ImagePartition { rescue: None, .. }));
        assert_eq!(damaged_summary(&s), vec![row("Read errors", "Stop at the first unreadable sector")]);

        s.rescue = true;
        s.retries = 3;
        s.skip_size = 1 << 20;
        s.no_scrape = true;
        s.reverse = true;
        let ops = s.build().unwrap();
        let settings = RescueSettings {
            retries: 3,
            skip_size: Some(1 << 20),
            cluster_size: None,
            no_trim: false,
            no_scrape: true,
            reverse: true,
            mapfile: None,
        };
        assert_eq!(
            ops,
            vec![Operation::ImagePartition {
                disk: 1,
                number: 3,
                path: path.clone(),
                format: ImageFormat::VhdDynamic,
                mode: CloneMode::Smart,
                rescue: Some(settings.clone()),
            }]
        );
        assert!(check_ops(&m, &ops).is_ok());
        assert!(ops[0].summary().contains("rescue mode (3 retries)"), "{}", ops[0].summary());
        // the default mapfile follows the image path
        assert_eq!(s.mapfile_path(), format!("{}.map", path.display()));
        let rows = damaged_summary(&s);
        assert!(rows.contains(&row("Read errors", "Continue (ddrescue mode), 3 retries")), "{rows:?}");
        assert!(rows.contains(&row("Mapfile", format!("{}.map", path.display()))), "{rows:?}");
        assert!(rows.contains(&row("Phases", "copy, trim, retry (reverse)")), "{rows:?}");

        // an edited mapfile is carried into the operation
        let map = dir.path().join("elsewhere.map");
        s.set_mapfile(map.display().to_string());
        s.path = dir.path().join("renamed.vhd").display().to_string();
        assert_eq!(s.mapfile_path(), map.display().to_string(), "an edited mapfile no longer follows the image");
        let ops = wizard(s).build_ops().unwrap();
        let Operation::ImagePartition { rescue: Some(r), .. } = &ops[0] else { panic!("{ops:?}") };
        assert_eq!(*r, RescueSettings { mapfile: Some(map), ..settings });
    }

    #[test]
    fn damaged_page_requires_absolute_mapfile_off_source() {
        let dir = tempfile::tempdir().unwrap();
        let m = model_of(vec![gpt_disk(1)]);
        let ctx = WizardCtx::new(&m, vec![], Selected::Partition(1, 3));
        let mut s = ImageCreateState::initial(&ctx).unwrap();
        assert_eq!(s.source_letters, ['E']);
        s.path = dir.path().join("data.vhd").display().to_string();
        s.rescue = true;
        assert_eq!(s.damaged_problem(), None, "the default mapfile next to the image");

        let problem = |s: &mut ImageCreateState, map: String| {
            s.set_mapfile(map);
            let p = s.damaged_problem();
            assert_eq!(s.build().err(), p, "build refuses what the page refuses");
            p.unwrap_or_default()
        };
        assert!(problem(&mut s, "data.map".into()).contains("full path"));
        assert!(problem(&mut s, String::new()).contains("Enter the mapfile"));
        assert!(problem(&mut s, r"e:\maps\data.map".into()).contains("drive E:"));
        assert!(problem(&mut s, dir.path().join("missing").join("x.map").display().to_string()).contains("does not exist"));
        let image = s.path.clone();
        assert!(problem(&mut s, image).contains("cannot be the image"));
        // a flat VMDK's extent is part of the image too
        s.format = ImageFormat::VmdkFlat;
        s.path = dir.path().join("data.vmdk").display().to_string();
        assert!(problem(&mut s, dir.path().join("data-flat.vmdk").display().to_string()).contains("cannot be the image"));
        assert!(problem(&mut s, dir.path().join("data.vmdk.partial").display().to_string()).contains("cannot be the image"));
        s.format = ImageFormat::VhdDynamic;
        s.path = dir.path().join("data.vhd").display().to_string();
        assert!(problem(&mut s, dir.path().join("ok.map").display().to_string()).is_empty());

        // rescue mode off: the mapfile is not used, so it is not checked
        s.set_mapfile("relative.map".into());
        s.rescue = false;
        assert_eq!(s.damaged_problem(), None);
        assert!(s.build().is_ok());

        // the page renders with rescue mode on (and stays inside a short viewport)
        s.rescue = true;
        s.set_mapfile(dir.path().join("ok.map").display().to_string());
        let mut w = wizard(s);
        assert_eq!(w.page_titles()[3], "Damaged disk");
        w.goto(3);
        let egui_ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 600.0));
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let _ = egui_ctx.run(input, |c| {
                w.show(c, &ctx);
            });
        }
        let rect = egui_ctx.memory(|mem| mem.area_rect(egui::Id::new("Create Image"))).expect("laid out");
        assert!(rect.max.y <= screen.max.y && rect.min.y >= 0.0, "{rect:?}");
        assert!(w.checked_ops(&m).is_ok());

        // the page belongs to Create Image only: restoring reads an image, there is no damaged source
        let restore = crate::wizards::image_restore::new(&ctx).unwrap();
        assert!(!restore.page_titles().iter().any(|t| t.contains("Damaged")), "{:?}", restore.page_titles());
    }

    #[test]
    fn image_file_on_the_source_is_refused() {
        let m = model_of(vec![gpt_disk(1)]);
        let ctx = WizardCtx::new(&m, vec![], Selected::Partition(1, 3));
        let mut s = ImageCreateState::initial(&ctx).unwrap();
        assert_eq!(s.source_letters, ['E']);
        s.path = r"e:\backup\data.vhd".into();
        let e = s.build().unwrap_err();
        assert!(e.contains("drive E:") && e.contains("being imaged"), "{e}");
        s.path = r"D:\backup\data.vhd".into();
        assert!(s.on_source_problem().is_none());
        assert!(s.build().is_ok());
    }
}
