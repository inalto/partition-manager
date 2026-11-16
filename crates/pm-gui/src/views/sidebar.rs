//! Left sidebar: what belongs to the tab on screen (for the disks, the selection and the
//! actions meant for it; for a tool tab, its tools and what they do) and the pending
//! operations.

use super::pending;
use crate::actions::GROUPS;
use crate::app::{AppState, Cmd, Tab};
use crate::format_util::partition_row;
use crate::selection::Selected;
use eframe::egui::{self, Button, CollapsingHeader, RichText};
use pm_ops::Model;

/// The heading of the sidebar: the selected item, or what to do when there is none.
pub fn heading(sel: &Selected, model: &Model) -> String {
    match *sel {
        Selected::None => "Select a disk or a partition to see what you can do with it.".into(),
        Selected::Disk(d) => format!("Disk {d}"),
        Selected::Partition(d, n) => match sel.partition_in(model) {
            Some(p) => format!("{} (disk {d})", partition_row(p)[0]),
            None => format!("Partition {n} (disk {d})"),
        },
        Selected::Free(d, ..) => format!("Unallocated space (disk {d})"),
    }
}

pub fn show(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    egui::TopBottomPanel::bottom("pending_operations")
        .resizable(true)
        .default_height(180.0)
        .show_inside(ui, |ui| pending::show(ui, s, out));
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(4.0);
        match s.tab {
            Tab::Disks => disks(ui, s, out),
            tab => tools(ui, s, tab, out),
        }
    });
}

/// The disks tab: what is selected and the actions meant for it.
fn disks(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    let model = s.preview();
    let text = heading(&s.selected, model);
    if s.selected == Selected::None {
        ui.add(egui::Label::new(text).wrap());
    } else {
        ui.add(egui::Label::new(RichText::new(text).strong()).truncate());
    }
    let locked = s.disks_locked();
    if locked {
        ui.add_space(4.0);
        ui.add(egui::Label::new("A tool is writing to a disk. The actions are back when it has finished.").wrap());
    }
    ui.add_space(4.0);
    // only the actions meant for this kind of selection; the ones that cannot run stay
    // listed, disabled, and say why
    for g in &GROUPS {
        let shown: Vec<_> = g.actions.iter().filter(|a| a.shown(&s.selected, model)).collect();
        if shown.is_empty() {
            continue;
        }
        CollapsingHeader::new(RichText::new(g.title).strong()).default_open(true).show(ui, |ui| {
            for a in shown {
                let blocked = a.blocked(&s.selected, model);
                let b = Button::new(a.label()).frame(false);
                let resp = ui.add_enabled(!s.busy() && !locked && blocked.is_none(), b);
                if resp.clicked() {
                    out.push(Cmd::Action(*a, s.selected));
                }
                if let Some(why) = blocked {
                    resp.on_disabled_hover_text(why);
                }
            }
        });
    }
}

/// A tool tab: its tools when it has several (what the toolbar menus used to hold), and
/// what the one on screen does.
fn tools(ui: &mut egui::Ui, s: &AppState, tab: Tab, out: &mut Vec<Cmd>) {
    ui.label(RichText::new(tab.label()).strong());
    ui.add_space(4.0);
    let active = s.active_tool();
    if tab.tools().len() > 1 {
        for t in tab.tools() {
            if ui.selectable_label(active == Some(*t), t.label()).clicked() {
                out.push(Cmd::OpenTool(*t));
            }
        }
        ui.add_space(8.0);
    }
    if let Some(t) = active {
        ui.add(egui::Label::new(t.about()).wrap());
        if s.tool_running(t) {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Running");
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, model_of};

    #[test]
    fn heading_names_the_selection() {
        let m = model_of(vec![gpt_disk(1)]);
        assert!(heading(&Selected::None, &m).starts_with("Select a disk or a partition"));
        assert_eq!(heading(&Selected::Disk(1), &m), "Disk 1");
        assert_eq!(heading(&Selected::Partition(1, 3), &m), "E: Data (disk 1)");
        assert_eq!(heading(&Selected::Partition(1, 99), &m), "Partition 99 (disk 1)");
        assert_eq!(heading(&Selected::Free(1, 0, 0), &m), "Unallocated space (disk 1)");
    }
}
