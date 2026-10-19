//! Format Partition wizard: filesystem, label, cluster size and quick format, with a red
//! warning that the partition's data will be erased → [`Operation::FormatPartition`].

use super::{danger, label_problem, partition_desc, row, FAT32_MAX};
use crate::items::is_container;
use crate::selection::Selected;
use crate::wizard::{shared, StepResult, Wizard, WizardCtx};
use eframe::egui;
use pm_core::{FsKind, GIB};
use pm_ops::Operation;

/// Smallest partition offered ReFS.
pub const REFS_MIN: u64 = 2 * GIB;

/// Cluster sizes offered (bytes); `None` is the filesystem's default.
pub const CLUSTERS: [Option<u32>; 9] = [
    None,
    Some(512),
    Some(1024),
    Some(2048),
    Some(4096),
    Some(8192),
    Some(16384),
    Some(32768),
    Some(65536),
];

pub fn cluster_name(c: Option<u32>) -> String {
    match c {
        None => "Default".into(),
        Some(b) if b >= 1024 => format!("{} KB", b / 1024),
        Some(b) => format!("{b} bytes"),
    }
}

/// Filesystems offered for a partition of `len` bytes, each with why it is not available.
pub fn fs_choices(len: u64) -> Vec<(FsKind, Option<&'static str>)> {
    vec![
        (FsKind::Ntfs, None),
        (FsKind::Fat32, (len > FAT32_MAX).then_some("FAT32 volumes are limited to 32 GB")),
        (FsKind::ExFat, None),
        (FsKind::ReFs, (len < REFS_MIN).then_some("ReFS needs at least 2 GB")),
    ]
}

pub struct FormatState {
    pub disk: u32,
    pub number: u32,
    pub desc: String,
    pub len: u64,
    pub fs: FsKind,
    pub label: String,
    pub cluster: Option<u32>,
    pub quick: bool,
}

impl FormatState {
    pub fn initial(ctx: &WizardCtx) -> Option<FormatState> {
        let Selected::Partition(disk, number) = ctx.selection else { return None };
        let d = ctx.model.disk(disk)?;
        let p = ctx.model.partition(disk, number).filter(|p| !is_container(d, p))?;
        let offered = |f: FsKind| fs_choices(p.len).iter().any(|(c, why)| *c == f && why.is_none());
        let fs = p.fs.filter(|f| offered(*f)).unwrap_or(FsKind::Ntfs);
        Some(FormatState {
            disk,
            number,
            desc: partition_desc(disk, p),
            len: p.len,
            fs,
            label: p.label.clone(),
            cluster: None,
            quick: true,
        })
    }

    pub fn problem(&self) -> Option<String> {
        if let Some((_, Some(why))) = fs_choices(self.len).into_iter().find(|(f, _)| *f == self.fs) {
            return Some(format!("{why}."));
        }
        label_problem(Some(self.fs), &self.label)
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        if let Some(p) = self.problem() {
            return Err(p);
        }
        Ok(vec![Operation::FormatPartition {
            disk: self.disk,
            number: self.number,
            fs: self.fs,
            label: self.label.trim().to_string(),
            cluster: self.cluster,
            quick: self.quick,
        }])
    }
}

fn options_ui(s: &mut FormatState, ui: &mut egui::Ui, _ctx: &WizardCtx) -> StepResult {
    ui.label(&s.desc);
    ui.add_space(4.0);
    danger(ui, format!("All data on partition {} of Disk {} will be erased.", s.number, s.disk));
    ui.add_space(8.0);
    egui::Grid::new("format_opts").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label("File system:");
        egui::ComboBox::from_id_salt("format_fs").width(180.0).selected_text(s.fs.display_name()).show_ui(ui, |ui| {
            for (f, why) in fs_choices(s.len) {
                let r = ui.add_enabled_ui(why.is_none(), |ui| ui.selectable_value(&mut s.fs, f, f.display_name()));
                if let Some(why) = why {
                    r.response.on_disabled_hover_text(why);
                }
            }
        });
        ui.end_row();
        ui.label("Label:");
        ui.add(egui::TextEdit::singleline(&mut s.label).desired_width(180.0));
        ui.end_row();
        ui.label("Cluster size:");
        egui::ComboBox::from_id_salt("format_cluster").width(180.0).selected_text(cluster_name(s.cluster)).show_ui(
            ui,
            |ui| {
                for c in CLUSTERS {
                    ui.selectable_value(&mut s.cluster, c, cluster_name(c));
                }
            },
        );
        ui.end_row();
        ui.label("");
        ui.checkbox(&mut s.quick, "Quick format");
        ui.end_row();
    });
    StepResult::from_problem(s.problem())
}

fn summary(s: &FormatState) -> Vec<(String, String)> {
    vec![
        row("Partition", s.desc.clone()),
        row("File system", s.fs.display_name()),
        row("Label", if s.label.trim().is_empty() { "(none)".to_string() } else { s.label.trim().to_string() }),
        row("Cluster size", cluster_name(s.cluster)),
        row("Quick format", if s.quick { "Yes" } else { "No" }),
    ]
}

pub fn wizard(state: FormatState) -> Wizard {
    Wizard::over_state("Format Partition", shared(state), vec![("Format options", options_ui, summary)], FormatState::build)
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    FormatState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, model_of};
    use crate::wizard::check_ops;

    #[test]
    fn build_format_op_from_filled_state() {
        let m = model_of(vec![gpt_disk(1)]);
        let mut s = FormatState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 3))).unwrap();
        assert_eq!((s.fs, s.label.as_str(), s.cluster, s.quick), (FsKind::Ntfs, "Data", None, true));
        s.fs = FsKind::ExFat;
        s.label = "Media".into();
        s.cluster = Some(65536);
        s.quick = false;
        let ops = s.build().unwrap();
        assert_eq!(
            ops,
            vec![Operation::FormatPartition {
                disk: 1,
                number: 3,
                fs: FsKind::ExFat,
                label: "Media".into(),
                cluster: Some(65536),
                quick: false
            }]
        );
        assert!(check_ops(&m, &ops).is_ok());
        assert_eq!(wizard(s).build_ops().unwrap(), ops);
    }

    #[test]
    fn format_validation() {
        let m = model_of(vec![gpt_disk(1)]);
        let mut s = FormatState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 3))).unwrap();
        s.fs = FsKind::ReFs; // 684 MiB: too small
        assert!(s.build().unwrap_err().contains("ReFS"));
        s.fs = FsKind::Fat32;
        s.label = "TWELVE_CHARS".into();
        assert!(s.build().is_err());
        s.len = 64 * GIB;
        s.label.clear();
        assert!(s.build().unwrap_err().contains("FAT32"));
        assert_eq!(cluster_name(Some(4096)), "4 KB");
        assert_eq!(cluster_name(Some(512)), "512 bytes");
        assert!(FormatState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(1))).is_none());
    }
}
