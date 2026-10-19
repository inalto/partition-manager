//! Convert wizard (MBR → GPT, GPT → MBR): what the conversion means, the constraints the
//! disk must meet (≤ 4 partitions and ≤ 2 TB for MBR; the boot-mode note) and the planner's
//! verdict → [`Operation::ConvertTable`].

use super::{danger, row};
use crate::selection::Selected;
use crate::wizard::{check_ops, shared, StepResult, Wizard, WizardCtx};
use crate::views::icons;
use eframe::egui::{self, RichText};
use pm_core::{format_size, DiskInfo, TableKind};
use pm_ops::Operation;

pub fn table_name(t: TableKind) -> &'static str {
    match t {
        TableKind::Gpt => "GPT",
        TableKind::Mbr => "MBR",
        _ => "uninitialized",
    }
}

/// Whether `d` can address all of its sectors with 32-bit MBR sector numbers (2 TB with
/// 512-byte sectors).
pub fn fits_mbr(d: &DiskInfo) -> bool {
    d.size / d.sector_size.max(512) as u64 <= u32::MAX as u64
}

/// The requirements for converting `d` to `to`, each with whether `d` meets it.
pub fn constraints(d: &DiskInfo, to: TableKind) -> Vec<(String, bool)> {
    let n = d.partitions.len();
    match to {
        TableKind::Mbr => vec![
            (format!("At most 4 partitions (this disk has {n})"), n <= 4),
            (format!("Disk no larger than 2 TB (this disk: {})", format_size(d.size)), fits_mbr(d)),
        ],
        _ => vec![(format!("At most 128 partitions (this disk has {n})"), n <= 128)],
    }
}

pub struct ConvertState {
    pub disk: u32,
    pub from: TableKind,
    pub to: TableKind,
    pub desc: String,
    pub constraints: Vec<(String, bool)>,
    pub has_efi: bool,
}

impl ConvertState {
    pub fn initial(ctx: &WizardCtx, to: TableKind) -> Option<ConvertState> {
        let Selected::Disk(disk) = ctx.selection else { return None };
        let d = ctx.model.disk(disk)?;
        if !matches!(d.table, TableKind::Gpt | TableKind::Mbr) || d.table == to {
            return None;
        }
        Some(ConvertState {
            disk,
            from: d.table,
            to,
            desc: d.display_name(),
            constraints: constraints(d, to),
            has_efi: d.partitions.iter().any(|p| p.ptype.map_or(false, |t| t.is_efi())),
        })
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        if let Some((what, _)) = self.constraints.iter().find(|(_, ok)| !ok) {
            return Err(format!("Not possible: {what}."));
        }
        Ok(vec![Operation::ConvertTable { disk: self.disk, to: self.to }])
    }
}

fn step_ui(s: &mut ConvertState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    ui.label(format!("{}: {} → {}", s.desc, table_name(s.from), table_name(s.to)));
    ui.add_space(6.0);
    let text = match s.to {
        TableKind::Gpt => {
            "GPT supports disks larger than 2 TB and up to 128 partitions. The partitions and \
             their data are kept. A disk that boots Windows in legacy BIOS mode needs UEFI \
             firmware and an EFI system partition after the conversion."
        }
        _ => {
            "MBR allows at most 4 primary partitions and 2 TB. The partitions and their data are \
             kept. Windows on an MBR disk boots in legacy BIOS mode only."
        }
    };
    ui.label(text);
    ui.add_space(8.0);
    ui.label(RichText::new("Requirements").strong());
    for (what, ok) in &s.constraints {
        let mark = if *ok { RichText::new("✔") } else { RichText::new(icons::WARNING).color(crate::theme::DANGER) };
        ui.horizontal(|ui| {
            ui.label(mark);
            ui.label(what);
        });
    }
    if s.to == TableKind::Mbr && s.has_efi {
        ui.add_space(6.0);
        danger(ui, "This disk has an EFI system partition: if it boots in UEFI mode, it will not boot after the conversion.");
    }
    let result = s.build().and_then(|ops| check_ops(ctx.model, &ops));
    StepResult::from_problem(result.err())
}

fn summary(s: &ConvertState) -> Vec<(String, String)> {
    vec![row("Disk", s.desc.clone()), row("Conversion", format!("{} → {}", table_name(s.from), table_name(s.to)))]
}

pub fn wizard(state: ConvertState) -> Wizard {
    let title = format!("Convert {} to {}", table_name(state.from), table_name(state.to));
    Wizard::over_state(title, shared(state), vec![("Conversion", step_ui, summary)], ConvertState::build)
}

pub fn new(ctx: &WizardCtx, to: TableKind) -> Option<Wizard> {
    ConvertState::initial(ctx, to).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of, part};
    use pm_core::GIB;

    #[test]
    fn build_convert_ops_and_constraints() {
        let mut m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        m.system_disk = Some(9);
        let s = ConvertState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(1)), TableKind::Mbr).unwrap();
        assert!(s.has_efi);
        assert!(s.constraints.iter().all(|(_, ok)| *ok));
        let ops = s.build().unwrap();
        assert_eq!(ops, vec![Operation::ConvertTable { disk: 1, to: TableKind::Mbr }]);
        assert!(check_ops(&m, &ops).is_ok(), "{:?}", check_ops(&m, &ops));
        let s = ConvertState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(0)), TableKind::Gpt).unwrap();
        assert_eq!(wizard(s).build_ops().unwrap(), vec![Operation::ConvertTable { disk: 0, to: TableKind::Gpt }]);
        // already the target table, or not a disk selection
        assert!(ConvertState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(1)), TableKind::Gpt).is_none());
        assert!(ConvertState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(1, 3)), TableKind::Mbr).is_none());
    }

    #[test]
    fn mbr_limits() {
        let mut d = gpt_disk(1);
        d.size = 3 * 1024 * GIB;
        d.partitions.extend([part(4, 900, 10), part(5, 910, 10)]);
        let c = constraints(&d, TableKind::Mbr);
        assert_eq!(c.iter().filter(|(_, ok)| !ok).count(), 2, "{c:?}");
        let m = model_of(vec![d]);
        let s = ConvertState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(1)), TableKind::Mbr).unwrap();
        assert!(s.build().unwrap_err().contains("At most 4 partitions"));
    }
}
