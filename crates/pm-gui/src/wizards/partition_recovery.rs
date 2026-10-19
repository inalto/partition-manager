//! Partition Recovery wizard (MiniTool screenshot 3): Disk (bars and a table of the disks) →
//! Scan mode (Quick: free space and the backup GPT; Full: the whole disk) → Scan (a job with
//! progress) → Results (candidates with start, size, filesystem, label, confidence and source;
//! checked by default from 70 % confidence; overlapping choices flagged; a preview bar of the
//! resulting layout) → Apply (red confirmation: the partition table is rewritten) → the table
//! is written by `pm_recover::apply_candidates` through a platform write session on a worker,
//! and the disks are read again.
//!
//! Like the CLI, existing partitions are never touched: candidates only fill free space, the
//! running system disk is refused (fail closed), and the write is refused when the on-disk
//! table changed since the scan.

use super::disk_preview;
use crate::tools::{
    danger, error_text, progress_panel, wizard_window, Footer, Nav, Task, ToolEnv, ToolWindow, WindowKind, WindowOutcome,
};
use eframe::egui::{self, RichText};
use pm_core::{format_size, DiskInfo, Error, TableKind};
use pm_ops::Model;
use pm_parttable::{read_table, Candidate, CandidateSource, Table};
use pm_platform::{OpenMode, Platform};
use pm_recover::{apply_candidates, find_partitions};
use std::time::Duration;

const PAGES: [&str; 5] = ["Disk", "Scan mode", "Scan", "Results", "Apply"];
/// Candidates at or above this confidence are checked by default.
pub const DEFAULT_CONFIDENCE: u8 = 70;

/// What a scan found, and the table on the disk when it ran.
pub struct ScanResult {
    pub cands: Vec<Candidate>,
    /// The table as read before the scan (`Err`: it could not be read or understood).
    pub table: Result<Table, String>,
}

/// Candidates checked by default: confidence ≥ [`DEFAULT_CONFIDENCE`].
pub fn default_checked(cands: &[Candidate]) -> Vec<bool> {
    cands.iter().map(|c| c.confidence >= DEFAULT_CONFIDENCE).collect()
}

/// For each candidate: checked and overlapping another checked candidate.
pub fn overlap_flags(cands: &[Candidate], checked: &[bool]) -> Vec<bool> {
    let on = |i: usize| checked.get(i).copied().unwrap_or(false);
    (0..cands.len())
        .map(|i| {
            on(i)
                && (0..cands.len()).any(|j| {
                    j != i && on(j) && {
                        let (a, b) = (&cands[i], &cands[j]);
                        a.start < b.start.saturating_add(b.len) && b.start < a.start.saturating_add(a.len)
                    }
                })
        })
        .collect()
}

/// The table the candidates are added to: the disk's own, or a new empty `new_kind` table on
/// a disk without one. A table that cannot be understood is never replaced.
pub fn base_table(read: &Result<Table, String>, new_kind: TableKind) -> Result<Table, String> {
    let t = read.as_ref().map_err(|e| format!("The partition table cannot be read ({e}); refusing to replace it."))?;
    match t.kind {
        TableKind::Gpt | TableKind::Mbr => Ok(t.clone()),
        TableKind::None => Ok(Table::empty(new_kind, t.disk_len, t.sector_size)),
        TableKind::Unknown => Err("The disk has a partition table this tool cannot read; refusing to replace it.".into()),
    }
}

/// The table after adding the checked candidates.
pub fn planned_table(scan: &ScanResult, checked: &[bool], new_kind: TableKind) -> Result<Table, String> {
    let chosen: Vec<Candidate> =
        scan.cands.iter().zip(checked).filter(|(_, c)| **c).map(|(c, _)| c.clone()).collect();
    if chosen.is_empty() {
        return Err("Check at least one partition to recover.".into());
    }
    let base = base_table(&scan.table, new_kind)?;
    apply_candidates(&base, &chosen).map_err(crate::wizard::error_text)
}

fn source_name(s: CandidateSource) -> &'static str {
    match s {
        CandidateSource::BootSector => "Boot sector",
        CandidateSource::BackupBootSector => "Backup boot sector",
        CandidateSource::GptBackupEntry => "Backup GPT",
    }
}

/// `disk` after `table`, with the candidates' filesystems and labels, for the preview bar.
fn preview_disk(disk: &DiskInfo, table: &Table, cands: &[Candidate]) -> DiskInfo {
    let mut d = table.to_disk_info(disk.clone());
    for p in &mut d.partitions {
        if let Some(c) = cands.iter().find(|c| c.start == p.start) {
            p.fs = Some(c.fs);
            p.label = c.label.clone();
        } else if let Some(old) = disk.partitions.iter().find(|o| o.start == p.start) {
            p.fs = old.fs;
            p.label = old.label.clone();
            p.used = old.used;
            p.mount_points = old.mount_points.clone();
        }
    }
    d
}

fn scan_job(p: &dyn Platform, disk: u32, quick: bool, progress: &dyn pm_core::ProgressSink, cancel: &pm_core::CancelToken) -> pm_core::Result<ScanResult> {
    let mut dev = p.open_disk(disk, OpenMode::ReadOnly)?;
    let table = match read_table(&mut *dev) {
        Ok(t) => Ok(t),
        Err(e @ (Error::Io { .. } | Error::Cancelled)) => return Err(e),
        Err(e) => Err(e.to_string()),
    };
    let cands = find_partitions(&mut *dev, quick, progress, cancel)?;
    Ok(ScanResult { cands, table })
}

fn apply_job(p: &dyn Platform, disk: u32, scanned: Result<Table, String>, new: Table) -> pm_core::Result<()> {
    let sys = Model::from_platform(p)?.effective_system_disk();
    if sys.map_or(true, |s| s == disk) {
        return Err(Error::Invalid(format!("Disk {disk} is (or may be) the running system disk; refusing to write to it")));
    }
    let mut session = p.begin_write(disk)?;
    let now = read_table(session.device()).map_err(|e| e.to_string());
    let r = if now != scanned {
        Err(Error::Invalid("The partition table changed since the scan; scan again.".into()))
    } else {
        session.commit_table(&new)
    };
    let fin = session.finish();
    r?;
    fin?;
    p.rescan(disk)
}

pub struct PartitionRecoveryWizard {
    page: usize,
    pub disk: Option<u32>,
    pub quick: bool,
    pub new_kind: TableKind,
    scan_task: Option<Task<ScanResult>>,
    pub scan: Option<ScanResult>,
    pub checked: Vec<bool>,
    apply_task: Option<Task<()>>,
    applied: bool,
    error: Option<String>,
}

impl PartitionRecoveryWizard {
    pub fn new(disk: Option<u32>) -> PartitionRecoveryWizard {
        PartitionRecoveryWizard {
            page: 0,
            disk,
            quick: true,
            new_kind: TableKind::Gpt,
            scan_task: None,
            scan: None,
            checked: Vec::new(),
            apply_task: None,
            applied: false,
            error: None,
        }
    }

    fn start_scan(&mut self, env: &ToolEnv) {
        let Some(disk) = self.disk else { return };
        let (p, quick) = (env.platform.clone(), self.quick);
        self.scan = None;
        self.error = None;
        self.scan_task = Some(Task::spawn("Partition scan", move |progress, cancel| scan_job(&*p, disk, quick, progress, cancel)));
    }

    fn start_apply(&mut self, env: &ToolEnv) {
        let (Some(disk), Some(scan)) = (self.disk, &self.scan) else { return };
        let Ok(new) = planned_table(scan, &self.checked, self.new_kind) else { return };
        let (p, scanned) = (env.platform.clone(), scan.table.clone());
        self.error = None;
        self.apply_task = Some(Task::spawn("Writing the partition table", move |_progress, _cancel| apply_job(&*p, disk, scanned, new)));
    }

    fn page_disk(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        ui.label("Select the disk to search for lost partitions:");
        egui::Grid::new("pr_disks").num_columns(4).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
            for h in ["Disk", "Model", "Size", "Partition table"] {
                ui.strong(h);
            }
            ui.end_row();
            for d in &env.live.disks {
                if ui.radio(self.disk == Some(d.index), format!("Disk {}", d.index)).clicked() && self.disk != Some(d.index) {
                    self.disk = Some(d.index);
                    self.scan = None;
                }
                ui.label(&d.model);
                ui.label(format_size(d.size));
                ui.label(match d.table {
                    TableKind::Gpt => "GPT",
                    TableKind::Mbr => "MBR",
                    TableKind::None => "None",
                    TableKind::Unknown => "Unknown",
                });
                ui.end_row();
            }
        });
        if let Some(d) = self.disk.and_then(|i| env.live.disk(i)) {
            ui.add_space(8.0);
            disk_preview(ui, d, None);
        }
        Footer::step(true, self.disk.is_none().then(|| "Choose a disk.".into()))
    }

    fn page_mode(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        ui.radio_value(&mut self.quick, true, "Quick scan");
        ui.indent("pr_quick", |ui| ui.weak("Looks at the unallocated space and the backup GPT. Fast."));
        ui.radio_value(&mut self.quick, false, "Full scan");
        ui.indent("pr_full", |ui| ui.weak("Probes the whole disk every 1 MB. Slow on large disks."));
        if self.disk.and_then(|i| env.live.disk(i)).is_some_and(|d| d.table == TableKind::None) {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("The disk has no partition table; create:");
                ui.radio_value(&mut self.new_kind, TableKind::Gpt, "GPT");
                ui.radio_value(&mut self.new_kind, TableKind::Mbr, "MBR");
            });
        }
        ui.weak("Scanning only reads the disk.");
        Footer { back: true, next: Some(("Scan".into(), true)), close: Some("Cancel".into()), problem: None }
    }

    fn page_scan(&mut self, ui: &mut egui::Ui) -> Footer {
        if let Some(t) = &self.scan_task {
            if progress_panel(ui, &t.progress(), t.elapsed(), t.is_cancelled(), true) {
                t.cancel();
            }
            return Footer { back: false, next: None, close: Some("Cancel".into()), problem: None };
        }
        match &self.scan {
            Some(s) => {
                ui.label(format!("Scan finished: {} partition(s) found.", s.cands.len()));
                Footer::step(false, s.cands.is_empty().then(|| "No lost partitions were found.".into()))
            }
            None => Footer { back: true, next: None, close: Some("Close".into()), problem: self.error.clone() },
        }
    }

    fn page_results(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        let Some(scan) = &self.scan else { return Footer::step(false, Some("Scan first.".into())) };
        let flags = overlap_flags(&scan.cands, &self.checked);
        ui.horizontal(|ui| {
            if ui.small_button("Check all").clicked() {
                self.checked.iter_mut().for_each(|c| *c = true);
            }
            if ui.small_button("Check none").clicked() {
                self.checked.iter_mut().for_each(|c| *c = false);
            }
        });
        egui::Grid::new("pr_results").num_columns(8).striped(true).spacing([12.0, 3.0]).show(ui, |ui| {
            for h in ["", "Start", "Size", "File system", "Label", "Confidence", "Source", ""] {
                ui.strong(h);
            }
            ui.end_row();
            for (i, c) in scan.cands.iter().enumerate() {
                if let Some(ch) = self.checked.get_mut(i) {
                    ui.checkbox(ch, "");
                }
                ui.label(format_size(c.start));
                ui.label(format_size(c.len));
                ui.label(c.fs.display_name());
                ui.label(&c.label);
                ui.label(format!("{} %", c.confidence));
                ui.label(source_name(c.source));
                if flags[i] {
                    ui.colored_label(crate::theme::DANGER, "overlaps");
                } else {
                    ui.label("");
                }
                ui.end_row();
            }
        });
        let plan = planned_table(scan, &self.checked, self.new_kind);
        if let (Ok(t), Some(d)) = (&plan, self.disk.and_then(|i| env.live.disk(i))) {
            ui.add_space(8.0);
            ui.label(RichText::new("Disk after recovery:").strong());
            disk_preview(ui, &preview_disk(d, t, &scan.cands), None);
        }
        let problem = if flags.iter().any(|f| *f) {
            Some("Some checked partitions overlap; uncheck one of each pair.".to_string())
        } else {
            plan.err()
        };
        Footer::step(false, problem)
    }

    fn page_apply(&mut self, ui: &mut egui::Ui, env: &ToolEnv) -> Footer {
        if self.apply_task.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Writing the partition table…");
            });
            return Footer { back: false, next: None, close: None, problem: None };
        }
        if self.applied {
            ui.label(RichText::new("The partition table was written. The disks are read again.").strong());
            return Footer { back: false, next: None, close: Some("Close".into()), problem: None };
        }
        let Some(disk) = self.disk else { return Footer::step(false, Some("Choose a disk.".into())) };
        let n = self.checked.iter().filter(|c| **c).count();
        ui.label(format!("{n} partition(s) will be added to Disk {disk}. Existing partitions are not changed."));
        ui.add_space(6.0);
        danger(ui, format!("The partition table of Disk {disk} will be rewritten now (this is not a pending operation)."));
        let problem = env.write_problem(disk).or_else(|| self.error.clone());
        Footer { back: true, next: Some(("Recover partitions".into(), problem.is_none())), close: Some("Cancel".into()), problem }
    }
}

impl ToolWindow for PartitionRecoveryWizard {
    fn kind(&self) -> WindowKind {
        WindowKind::PartitionRecovery
    }
    fn modal(&self) -> bool {
        true
    }
    fn writing(&self) -> bool {
        self.apply_task.is_some()
    }

    fn show(&mut self, ctx: &egui::Context, env: &ToolEnv) -> WindowOutcome {
        let mut outcome = WindowOutcome::Open;
        if let Some(t) = self.scan_task.as_mut() {
            ctx.request_repaint_after(Duration::from_millis(150));
            if let Some(r) = t.poll() {
                match r {
                    Ok(s) => {
                        self.checked = default_checked(&s.cands);
                        self.scan = Some(s);
                    }
                    Err(e) => self.error = Some(error_text(&e)),
                }
                self.scan_task = None;
            }
        }
        if let Some(t) = self.apply_task.as_mut() {
            ctx.request_repaint_after(Duration::from_millis(150));
            if let Some(r) = t.poll() {
                match r {
                    Ok(()) => {
                        self.applied = true;
                        outcome = WindowOutcome::Refresh;
                    }
                    Err(e) => self.error = Some(error_text(&e)),
                }
                self.apply_task = None;
            }
        }
        let page = self.page;
        let nav = wizard_window(ctx, "Partition Recovery", &PAGES, page, |ui| match page {
            0 => self.page_disk(ui, env),
            1 => self.page_mode(ui, env),
            2 => self.page_scan(ui),
            3 => self.page_results(ui, env),
            _ => self.page_apply(ui, env),
        });
        match nav {
            Nav::Close if self.apply_task.is_none() => return WindowOutcome::Close,
            Nav::Back if self.scan_task.is_none() && self.apply_task.is_none() => {
                self.error = None;
                self.page = self.page.saturating_sub(1);
            }
            Nav::Next => match page {
                1 => {
                    self.start_scan(env);
                    self.page = 2;
                }
                4 => self.start_apply(env),
                _ => self.page = (page + 1).min(PAGES.len() - 1),
            },
            _ => {}
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{BlockDevice, FsKind, MIB};
    use std::sync::Arc;

    fn cand(start_mib: u64, len_mib: u64, confidence: u8) -> Candidate {
        Candidate {
            start: start_mib * MIB,
            len: len_mib * MIB,
            fs: FsKind::Ntfs,
            label: String::new(),
            source: CandidateSource::BootSector,
            confidence,
        }
    }

    #[test]
    fn checked_by_confidence_and_overlaps_flagged() {
        let c = [cand(1, 100, 90), cand(50, 100, 70), cand(200, 10, 40)];
        let checked = default_checked(&c);
        assert_eq!(checked, [true, true, false]);
        assert_eq!(overlap_flags(&c, &checked), [true, true, false]);
        assert_eq!(overlap_flags(&c, &[true, false, true]), [false, false, false]);
        let empty = Table::empty(TableKind::None, 1024 * MIB, 512);
        assert_eq!(base_table(&Ok(empty), TableKind::Mbr).unwrap().kind, TableKind::Mbr);
        assert!(base_table(&Ok(Table::empty(TableKind::Unknown, MIB, 512)), TableKind::Gpt).is_err());
        assert!(base_table(&Err("x".into()), TableKind::Gpt).is_err());
    }

    /// A FAT32 filesystem whose partition entry is gone is found by a quick scan and written
    /// back through the platform.
    #[test]
    fn scan_and_apply_on_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lost.img");
        let mut dev = pm_image::create(&path, &pm_image::CreateOpts { format: pm_image::ImageFormat::Raw, capacity: 64 * MIB, sparse: false }).unwrap();
        pm_parttable::write_table(&mut *dev, &Table::empty(TableKind::Gpt, 64 * MIB, 512)).unwrap();
        {
            let mut slice = pm_core::SliceDevice::new(&mut *dev, MIB, 40 * MIB).unwrap();
            pm_fs::format_fat32(&mut slice, &pm_fs::FormatOpts { label: "LOST".into(), cluster_size: None, volume_id: Some(1) }).unwrap();
        }
        dev.flush().unwrap();
        drop(dev);
        let p: Arc<dyn Platform> = Arc::new(pm_platform::FilePlatform::new(vec![path.clone()]).unwrap());
        let disk = Model::from_platform(&*p).unwrap().disks[0].index;
        let scan = scan_job(&*p, disk, true, &pm_core::NullProgress, &pm_core::CancelToken::new()).unwrap();
        assert_eq!(scan.cands.len(), 1, "{:?}", scan.cands);
        assert_eq!(scan.cands[0].start, MIB);
        let checked = default_checked(&scan.cands);
        let new = planned_table(&scan, &checked, TableKind::Gpt).unwrap();
        assert_eq!(new.entries.len(), 1);
        apply_job(&*p, disk, scan.table.clone(), new.clone()).unwrap();
        let after = Model::from_platform(&*p).unwrap();
        assert_eq!(after.disks[0].partitions.len(), 1);
        assert_eq!(after.disks[0].partitions[0].start, MIB);
        // the table changed since that scan: a second write is refused
        assert!(apply_job(&*p, disk, scan.table, new).unwrap_err().to_string().contains("changed since the scan"));
    }

    #[test]
    fn every_page_renders() {
        use crate::tools::test_env::{frame, model, platform};
        use crate::tools::ToolEnv;
        let (m, p) = (model(), platform());
        let env = ToolEnv { platform: &p, live: &m, pending: &[] };
        let mut w = PartitionRecoveryWizard::new(Some(1));
        let cands = vec![cand(801, 100, 90), cand(850, 10, 30)];
        w.checked = default_checked(&cands);
        w.scan = Some(ScanResult { cands, table: Ok(pm_ops::table_from_disk(m.disk(1).unwrap())) });
        for page in 0..PAGES.len() {
            w.page = page;
            assert_eq!(frame(&mut w, &env), WindowOutcome::Open);
        }
        assert!(w.modal());
    }
}
