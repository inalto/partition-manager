//! Create Image wizard: Source (a disk or partition, from the selection) → Image file (a
//! full path whose folder exists, the format, the extension fixed to match it) → Copy mode →
//! summary → [`Operation::ImageDisk`] / [`Operation::ImagePartition`].

use super::{disk_preview, row};
use crate::selection::Selected;
use crate::tools::{on_letters_problem, Device};
use crate::wizard::{shared, StepResult, Wizard, WizardCtx};
use eframe::egui;
use pm_core::format_size;
use pm_image::ImageFormat;
use pm_ops::{CloneMode, Model, Operation};
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
        if let Some(e) = self.on_source_problem() {
            return Err(e);
        }
        let path = PathBuf::from(t);
        let (format, mode) = (self.format, self.mode);
        Ok(vec![match self.source {
            Device::Disk(disk) => Operation::ImageDisk { disk, path, format, mode, rescue: None },
            Device::Partition(disk, number) => Operation::ImagePartition { disk, number, path, format, mode, rescue: None },
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

pub fn wizard(state: ImageCreateState) -> Wizard {
    Wizard::over_state(
        "Create Image",
        shared(state),
        vec![("Source", source_ui, source_summary), ("Image file", file_ui, file_summary), ("Copy mode", mode_ui, mode_summary)],
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
