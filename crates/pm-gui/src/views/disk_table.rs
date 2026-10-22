//! The partition table: Partition | Capacity | Used | Unused | File system | Type | Status,
//! grouped by disk, built from the preview model so queued operations show up immediately.

use super::context_menu;
use crate::app::{AppState, Cmd};
use crate::format_util::{disk_header, free_row, partition_row, COLUMNS};
use crate::items::disk_items;
use crate::selection::Selected;
use eframe::egui::{self, Align, Layout, RichText, Sense};
use egui_extras::{Column, TableBuilder};
use pm_ops::Model;

/// One table row.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    /// Disk group header: `Disk N (Model, GPT, size)`.
    Disk { sel: Selected, title: String },
    Item { sel: Selected, cells: [String; 7] },
}

impl Row {
    pub fn sel(&self) -> Selected {
        match self {
            Row::Disk { sel, .. } | Row::Item { sel, .. } => *sel,
        }
    }
}

/// The rows for `model`: each disk header followed by its partitions and unallocated gaps
/// (`elevation_hint`: what an unreadable table's header advises).
pub fn rows(model: &Model, elevation_hint: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for d in &model.disks {
        out.push(Row::Disk { sel: Selected::Disk(d.index), title: disk_header(d, elevation_hint) });
        for item in disk_items(d) {
            let cells = match item.sel {
                Selected::Partition(_, n) => match d.partitions.iter().find(|p| p.number == n) {
                    Some(p) => partition_row(p),
                    None => continue,
                },
                _ => free_row(item.len),
            };
            out.push(Row::Item { sel: item.sel, cells });
        }
    }
    out
}

pub fn show(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    if !s.loaded {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Reading disks…");
        });
        return;
    }
    let rows = rows(s.preview(), s.platform.elevation_hint());
    if rows.is_empty() {
        ui.weak("No disks to show.");
        return;
    }
    let row_h = 22.0;
    TableBuilder::new(ui)
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::initial(300.0).at_least(120.0).clip(true))
        .columns(Column::initial(90.0).at_least(60.0).clip(true), 3)
        .column(Column::initial(95.0).at_least(60.0).clip(true))
        .column(Column::initial(190.0).at_least(60.0).clip(true))
        .column(Column::remainder().at_least(80.0).clip(true))
        .auto_shrink([false, false])
        .header(row_h, |mut header| {
            for c in COLUMNS {
                header.col(|ui| {
                    ui.strong(c);
                });
            }
        })
        .body(|body| {
            body.rows(row_h, rows.len(), |mut row| {
                let r = &rows[row.index()];
                let sel = r.sel();
                row.set_selected(s.selected == sel);
                match r {
                    Row::Disk { title, .. } => {
                        row.col(|ui| {
                            ui.label(RichText::new(title).strong());
                        });
                        for _ in 1..COLUMNS.len() {
                            row.col(|_| {});
                        }
                    }
                    Row::Item { cells, .. } => {
                        for (i, c) in cells.iter().enumerate() {
                            row.col(|ui| {
                                if i == 0 {
                                    ui.add_space(14.0);
                                }
                                ui.label(c);
                            });
                        }
                    }
                }
                let resp = row.response();
                if resp.clicked() || resp.secondary_clicked() {
                    out.push(Cmd::Select(sel));
                }
                resp.context_menu(|ui| context_menu::show(ui, s, sel, out));
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};

    #[test]
    fn rows_grouped_by_disk_from_the_model() {
        let m = model_of(vec![gpt_disk(1), mbr_disk(0)]);
        let rows = rows(&m, "HINT");
        // disk 1: header + 3 partitions + free tail; disk 0: header + 6 items
        assert_eq!(rows.len(), 1 + 4 + 1 + 6);
        let title = format!("Disk 1 (Test SSD, GPT, 1.00 GB)  {}", m.disks[0].path);
        assert_eq!(rows[0], Row::Disk { sel: Selected::Disk(1), title });
        match &rows[3] {
            Row::Item { sel, cells } => {
                assert_eq!(*sel, Selected::Partition(1, 3));
                assert_eq!(cells[0], "E:Data");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&rows[4], Row::Item { cells, .. } if cells[0] == "(Unallocated)"));
        assert_eq!(rows[5].sel(), Selected::Disk(0));
    }

    #[test]
    fn disk_indexes_need_not_be_contiguous() {
        // Linux keeps a removed disk's index: disks 0, 2 and 5
        let disks = [0, 2, 5].map(|i| pm_core::DiskInfo { path: format!("/dev/sd{}", (b'a' + i as u8) as char), ..gpt_disk(i) });
        let m = model_of(disks.to_vec());
        let rows = rows(&m, "HINT");
        let headers: Vec<_> = rows.iter().filter_map(|r| match r {
            Row::Disk { sel, title } => Some((*sel, title.clone())),
            _ => None,
        }).collect();
        assert_eq!(headers.iter().map(|h| h.0).collect::<Vec<_>>(), [Selected::Disk(0), Selected::Disk(2), Selected::Disk(5)]);
        assert!(headers[2].1.starts_with("Disk 5 (") && headers[2].1.ends_with("/dev/sdf"), "{:?}", headers[2]);
        // every partition row belongs to its own disk, looked up by index
        assert!(rows.iter().any(|r| r.sel() == Selected::Partition(5, 3)));
        assert_eq!(m.disk(5).map(|d| d.index), Some(5));
        assert!(m.disk(1).is_none() && m.partition(2, 3).is_some());
    }

    #[test]
    fn queued_delete_shows_as_unallocated() {
        let mut q = pm_ops::OpQueue::new(model_of(vec![gpt_disk(1)]));
        q.push(pm_ops::Operation::DeletePartition { disk: 1, number: 3 }).unwrap();
        let rows = rows(q.preview(), "HINT");
        assert!(rows.iter().all(|r| r.sel() != Selected::Partition(1, 3)));
        assert!(matches!(rows.last(), Some(Row::Item { cells, .. }) if cells[0] == "(Unallocated)"));
    }
}
