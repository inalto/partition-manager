//! Left sidebar: what the selection is, the actions meant for it, the tools, and the pending
//! operations.

use super::pending;
use crate::actions::{Action, GROUPS};
use crate::app::{AppState, Cmd, Tool};
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

/// Actions that only open a tool window on the selection. The Tools group below already
/// lists them (and a tool opens on the selection too), so the sidebar shows them once, there;
/// the right-click menu, which has no Tools group, keeps them.
pub const IN_TOOLS: [Action; 2] = [Action::PartitionRecovery, Action::SurfaceTest];

pub fn show(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    egui::TopBottomPanel::bottom("pending_operations")
        .resizable(true)
        .default_height(180.0)
        .show_inside(ui, |ui| pending::show(ui, s, out));
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let model = s.preview();
        ui.add_space(4.0);
        let text = heading(&s.selected, model);
        if s.selected == Selected::None {
            ui.add(egui::Label::new(text).wrap());
        } else {
            ui.add(egui::Label::new(RichText::new(text).strong()).truncate());
        }
        ui.add_space(4.0);
        // only the actions meant for this kind of selection; the ones that cannot run stay
        // listed, disabled, and say why
        for g in &GROUPS {
            let shown: Vec<_> = g.actions.iter().filter(|a| !IN_TOOLS.contains(a) && a.shown(&s.selected, model)).collect();
            if shown.is_empty() {
                continue;
            }
            CollapsingHeader::new(RichText::new(g.title).strong()).default_open(true).show(ui, |ui| {
                for a in shown {
                    let blocked = a.blocked(&s.selected, model);
                    let b = Button::new(a.label()).frame(false);
                    let resp = ui.add_enabled(!s.busy() && blocked.is_none(), b);
                    if resp.clicked() {
                        out.push(Cmd::Action(*a, s.selected));
                    }
                    if let Some(why) = blocked {
                        resp.on_disabled_hover_text(why);
                    }
                }
            });
        }
        // the same tools as the toolbar, each on its own row (two of them sit in menus there)
        CollapsingHeader::new(RichText::new("Tools").strong()).default_open(true).show(ui, |ui| {
            for t in Tool::SIDEBAR {
                if ui.add(Button::new(t.label()).frame(false)).clicked() {
                    out.push(Cmd::OpenTool(t));
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, model_of};

    #[test]
    fn actions_left_to_the_tools_group_have_a_tool_of_the_same_name() {
        for a in IN_TOOLS {
            assert!(Tool::SIDEBAR.iter().any(|t| t.label() == a.label()), "{a:?}");
        }
    }

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
