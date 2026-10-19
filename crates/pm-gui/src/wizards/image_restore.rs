//! Restore Image wizard: Image file (identified with `pm_image::identify`; format and virtual
//! capacity shown) → Target (a whole disk, or unallocated space, that can hold the image; never
//! the running system disk) with a red warning → summary → [`Operation::RestoreImage`].

use super::create::usable_ranges;
use super::{disk_preview, row};
use crate::selection::Selected;
use crate::wizard::{shared, StepResult, Wizard, WizardCtx};
use eframe::egui;
use pm_core::{align_down, align_up, format_size, TableKind, MIB};
use pm_image::ImageFormat;
use pm_ops::{Model, Operation};
use std::path::{Path, PathBuf};

/// Where an image is restored to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreTarget {
    /// The whole disk (its partition table is replaced by the image's content).
    Disk(u32),
    /// A new partition in the unallocated range `(start, len)` of a disk.
    Free(u32, u64, u64),
}

impl RestoreTarget {
    pub fn disk(self) -> u32 {
        match self {
            RestoreTarget::Disk(d) | RestoreTarget::Free(d, _, _) => d,
        }
    }
}

/// The disk of `m` holding the drive letter of `path`, and that letter.
pub fn image_disk(m: &Model, path: &str) -> Option<(u32, char)> {
    let c = crate::tools::path_letter(Path::new(path.trim()))?;
    m.disks.iter().find(|d| d.partitions.iter().any(|p| p.letter() == Some(c))).map(|d| (d.index, c))
}

fn image_on_target(d: u32, c: char) -> String {
    format!("The image file is on drive {c}:, which is on Disk {d}; it cannot be restored onto that disk. Copy it to another disk first.")
}

/// Format and virtual capacity of the image at `path` (reads its header).
pub fn identify(path: &Path) -> Result<(ImageFormat, u64), String> {
    let fmt = pm_image::identify(path).map_err(|e| e.to_string())?;
    let cap = pm_image::open(path, true).map_err(|e| e.to_string())?.len();
    Ok((fmt, cap))
}

/// Targets that can hold `cap` bytes: whole disks at least that large and unallocated ranges
/// of initialized disks with room for a 1 MB-aligned partition of that size; never `system`.
pub fn targets(m: &Model, cap: u64, system: Option<u32>) -> Vec<RestoreTarget> {
    let mut out = Vec::new();
    for d in m.disks.iter().filter(|d| Some(d.index) != system) {
        if d.size >= cap {
            out.push(RestoreTarget::Disk(d.index));
        }
        if matches!(d.table, TableKind::Gpt | TableKind::Mbr) && cap >= MIB {
            for (s, l) in usable_ranges(d) {
                if align_down(s + l, MIB).saturating_sub(align_up(s, MIB)) >= cap {
                    out.push(RestoreTarget::Free(d.index, s, l));
                }
            }
        }
    }
    out
}

pub struct ImageRestoreState {
    pub path: String,
    /// The identified path and its format / capacity (or why it cannot be used).
    pub info: Option<(String, Result<(ImageFormat, u64), String>)>,
    pub target: Option<RestoreTarget>,
    /// The disk holding the image file, with its drive letter: never a target (restoring
    /// locks and overwrites the disk the image is read from). Updated from the model when the
    /// target step is drawn.
    pub image_disk: Option<(u32, char)>,
}

impl ImageRestoreState {
    pub fn initial(_ctx: &WizardCtx) -> Option<ImageRestoreState> {
        Some(ImageRestoreState { path: String::new(), info: None, target: None, image_disk: None })
    }

    /// The capacity of the identified image, when the path is the identified one.
    pub fn capacity(&self) -> Option<u64> {
        match &self.info {
            Some((p, Ok((_, cap)))) if *p == self.path => Some(*cap),
            _ => None,
        }
    }

    /// Identify the image when the path changed since the last time.
    pub fn refresh_info(&mut self) {
        if self.info.as_ref().map(|i| &i.0) != Some(&self.path) {
            let t = self.path.trim();
            let r = if t.is_empty() { Err("Enter the path of an image file.".into()) } else { identify(Path::new(t)) };
            self.info = Some((self.path.clone(), r));
            self.target = None;
        }
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        let t = self.path.trim();
        if t.is_empty() {
            return Err("Enter the path of an image file.".into());
        }
        let target = self.target.ok_or("Choose where to restore the image.")?;
        if let Some((d, c)) = self.image_disk.filter(|(d, _)| *d == target.disk()) {
            return Err(image_on_target(d, c));
        }
        let (dst_disk, dst_start) = match target {
            RestoreTarget::Disk(d) => (d, None),
            RestoreTarget::Free(d, s, _) => (d, Some(align_up(s, MIB))),
        };
        Ok(vec![Operation::RestoreImage { path: PathBuf::from(t), dst_disk, dst_start }])
    }
}

fn file_ui(s: &mut ImageRestoreState, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
    ui.horizontal(|ui| {
        ui.label("Image file:");
        let resp = ui.add(egui::TextEdit::singleline(&mut s.path).desired_width(400.0).hint_text("D:\\Backups\\disk1.vhd"));
        // read the header once the user is done typing, not on every keystroke
        if !resp.has_focus() {
            s.refresh_info();
        }
    });
    ui.add_space(6.0);
    match &s.info {
        Some((p, Ok((fmt, cap)))) if *p == s.path => {
            ui.label(format!("Format: {}", fmt.display()));
            ui.label(format!("Capacity: {}", format_size(*cap)));
            StepResult::Continue
        }
        Some((p, Err(e))) if *p == s.path => StepResult::Invalid(format!("Cannot use this file: {e}")),
        _ => StepResult::Invalid(String::new()),
    }
}

fn target_ui(s: &mut ImageRestoreState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    let m = ctx.model;
    let Some(cap) = s.capacity() else { return StepResult::Invalid("Choose an image file first.".into()) };
    s.image_disk = image_disk(m, &s.path);
    let blocked = s.image_disk.map(|b| b.0);
    let list: Vec<RestoreTarget> = targets(m, cap, ctx.system_disk).into_iter().filter(|t| Some(t.disk()) != blocked).collect();
    if let Some((d, c)) = s.image_disk {
        ui.weak(format!("Disk {d} is not offered: it holds the image file (drive {c}:)."));
        if s.target.is_some_and(|t| t.disk() == d) {
            s.target = None;
        }
    }
    if list.is_empty() {
        return StepResult::Invalid(format!("No disk or unallocated space can hold {}.", format_size(cap)));
    }
    ui.label(format!("Restore the {} image to:", format_size(cap)));
    for t in &list {
        let name = m.disk(t.disk()).map_or(format!("Disk {}", t.disk()), |d| d.display_name());
        let text = match t {
            RestoreTarget::Disk(_) => format!("{name} (whole disk)"),
            RestoreTarget::Free(_, st, l) => format!("{name}: unallocated {} at {}", format_size(*l), format_size(*st)),
        };
        ui.radio_value(&mut s.target, Some(*t), text);
    }
    if let Some(t) = s.target {
        if let Some(d) = m.disk(t.disk()) {
            ui.add_space(8.0);
            let hl = match t {
                RestoreTarget::Free(d, st, l) => Some(Selected::Free(d, st, l)),
                RestoreTarget::Disk(_) => None,
            };
            disk_preview(ui, d, hl);
            ui.add_space(4.0);
            match t {
                RestoreTarget::Disk(d) => super::danger(ui, format!("Everything on Disk {d}, including its partition table, will be overwritten.")),
                RestoreTarget::Free(..) => super::danger(ui, "The image becomes a new partition in this space."),
            }
        }
    }
    StepResult::from_problem(s.target.is_none().then(|| "Choose where to restore the image.".into()))
}

fn file_summary(s: &ImageRestoreState) -> Vec<(String, String)> {
    let mut rows = vec![row("Image file", s.path.trim())];
    if let Some((_, Ok((fmt, cap)))) = &s.info {
        rows.push(row("Format", format!("{} ({})", fmt.display(), format_size(*cap))));
    }
    rows
}

fn target_summary(s: &ImageRestoreState) -> Vec<(String, String)> {
    vec![row(
        "Target",
        match s.target {
            Some(RestoreTarget::Disk(d)) => format!("Disk {d} (whole disk, overwritten)"),
            Some(RestoreTarget::Free(d, st, _)) => format!("Disk {d}, new partition at {}", format_size(align_up(st, MIB))),
            None => "-".into(),
        },
    )]
}

pub fn wizard(state: ImageRestoreState) -> Wizard {
    Wizard::over_state(
        "Restore Image",
        shared(state),
        vec![("Image file", file_ui, file_summary), ("Target", target_ui, target_summary)],
        ImageRestoreState::build,
    )
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    ImageRestoreState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};
    use crate::wizard::check_ops;

    #[test]
    fn build_restore_ops_from_filled_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part.img");
        drop(pm_image::create(&path, &pm_image::CreateOpts { format: ImageFormat::Raw, capacity: 10 * MIB, sparse: false }).unwrap());
        let mut m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        m.system_disk = Some(0);
        let ctx = WizardCtx::new(&m, vec![], Selected::None);
        let mut s = ImageRestoreState::initial(&ctx).unwrap();
        assert!(s.build().is_err());
        s.path = path.display().to_string();
        s.refresh_info();
        assert_eq!(s.capacity(), Some(10 * MIB));
        assert!(matches!(&s.info, Some((_, Ok((ImageFormat::Raw, _))))));
        let list = targets(&m, 10 * MIB, ctx.system_disk);
        assert_eq!(list, [RestoreTarget::Disk(1), RestoreTarget::Free(1, 801 * MIB, 223 * MIB - 33 * 512)], "never the system disk");
        s.target = Some(list[1]);
        let ops = s.build().unwrap();
        assert_eq!(ops, vec![Operation::RestoreImage { path: path.clone(), dst_disk: 1, dst_start: Some(801 * MIB) }]);
        let after = check_ops(&m, &ops).unwrap();
        assert_eq!(after.disk(1).unwrap().partitions.len(), 4);
        s.target = Some(RestoreTarget::Disk(1));
        assert_eq!(wizard(s).build_ops().unwrap(), vec![Operation::RestoreImage { path, dst_disk: 1, dst_start: None }]);
        // an unreadable path is reported
        let mut bad = ImageRestoreState::initial(&ctx).unwrap();
        bad.path = dir.path().join("missing.vhd").display().to_string();
        bad.refresh_info();
        assert!(bad.capacity().is_none());
    }

    #[test]
    fn image_on_the_target_disk_is_refused() {
        let mut m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        m.system_disk = Some(7);
        assert_eq!(image_disk(&m, r"E:\img\disk.vhd"), Some((1, 'E')));
        assert_eq!(image_disk(&m, r"D:\disk.vhd"), None);
        let ctx = WizardCtx::new(&m, vec![], Selected::None);
        let mut s = ImageRestoreState::initial(&ctx).unwrap();
        s.path = r"E:\img\disk.vhd".into();
        s.image_disk = image_disk(&m, &s.path);
        s.target = Some(RestoreTarget::Disk(1));
        let e = s.build().unwrap_err();
        assert!(e.contains("drive E:") && e.contains("Disk 1"), "{e}");
        s.target = Some(RestoreTarget::Disk(0));
        assert!(s.build().is_ok(), "another disk is fine");
    }
}
