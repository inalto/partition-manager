//! Space Analyzer window (MiniTool screenshot 6): a folder or drive and Scan; tabs "Tree view"
//! (collapsible directory tree with size, % and file count), "File view" (largest files) and
//! "Folder view" (largest folders), next to a squarified treemap of the current folder
//! ([`super::treemap::squarify`]). Hovering a cell shows its path and size; clicking a folder
//! cell drills into it.

use super::treemap::squarify;
use super::{error_text, progress_panel, tool_frame, Task, ToolEnv, ToolWindow, WindowKind, WindowOutcome};
use eframe::egui::{self, vec2, Align2, Color32, FontId, Rect, Sense, Stroke};
use pm_analyze::{scan_space, Node, SpaceReport};
use pm_core::format_size;
use std::path::PathBuf;
use std::time::Duration;

/// Children of a tree node drawn before the rest is summarised.
const TREE_LIMIT: usize = 300;
/// Cells of the treemap (the rest is merged into one "other" cell).
const MAP_LIMIT: usize = 250;
/// Rows of the folder view.
const FOLDER_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Tree,
    Files,
    Folders,
}

/// A finished scan and what is derived from it (computed on the worker).
pub struct Analysis {
    pub report: SpaceReport,
    /// `(path, size, files)` of the largest folders, largest first.
    pub folders: Vec<(PathBuf, u64, u64)>,
}

/// The `limit` largest folders of the tree below (and including) `root`.
pub fn largest_folders(root: &Node, limit: usize) -> Vec<(PathBuf, u64, u64)> {
    let mut all = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        all.push((n.path.clone(), n.size, n.files));
        stack.extend(n.children.iter());
    }
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    all.truncate(limit);
    all
}

/// The node reached from `root` through the child indices `path` (as far as they exist).
pub fn node_at<'a>(root: &'a Node, path: &[usize]) -> &'a Node {
    let mut n = root;
    for &i in path {
        match n.children.get(i) {
            Some(c) => n = c,
            None => break,
        }
    }
    n
}

/// Treemap items of `n`: its largest sub-folders (ids = child indices), the files directly in
/// it (id `usize::MAX`) and the remaining folders merged (id `usize::MAX - 1`).
pub fn map_items(n: &Node) -> Vec<(f64, usize)> {
    let mut items: Vec<(f64, usize)> =
        n.children.iter().enumerate().take(MAP_LIMIT).map(|(i, c)| (c.size as f64, i)).collect();
    let rest: u64 = n.children.iter().skip(MAP_LIMIT).map(|c| c.size).sum();
    let direct = n.size.saturating_sub(n.children.iter().map(|c| c.size).sum());
    if rest > 0 {
        items.push((rest as f64, usize::MAX - 1));
    }
    if direct > 0 {
        items.push((direct as f64, usize::MAX));
    }
    items
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

pub struct SpaceAnalyzerWindow {
    pub path: String,
    task: Option<Task<Analysis>>,
    analysis: Option<Analysis>,
    tab: Tab,
    /// Treemap drill-down: child indices from the root.
    drill: Vec<usize>,
    error: Option<String>,
}

impl SpaceAnalyzerWindow {
    pub fn new(path: Option<String>) -> SpaceAnalyzerWindow {
        SpaceAnalyzerWindow { path: path.unwrap_or_default(), task: None, analysis: None, tab: Tab::Tree, drill: Vec::new(), error: None }
    }

    fn start(&mut self) {
        let root = PathBuf::from(self.path.trim());
        self.error = None;
        self.task = Some(Task::spawn("Space analysis", move |progress, cancel| {
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
            let report = scan_space(&root, threads, progress, cancel)?;
            let folders = largest_folders(&report.root, FOLDER_LIMIT);
            Ok(Analysis { report, folders })
        }));
    }
}

impl ToolWindow for SpaceAnalyzerWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::SpaceAnalyzer
    }

    fn show(&mut self, ctx: &egui::Context, env: &ToolEnv) -> WindowOutcome {
        if let Some(t) = self.task.as_mut() {
            ctx.request_repaint_after(Duration::from_millis(150));
            if let Some(r) = t.poll() {
                match r {
                    Ok(a) => {
                        self.analysis = Some(a);
                        self.drill.clear();
                    }
                    Err(e) => self.error = Some(error_text(&e)),
                }
                self.task = None;
            }
        }
        if tool_frame(ctx, "Space Analyzer", [1000.0, 640.0], |ui| self.ui(ui, env)) {
            WindowOutcome::Open
        } else {
            WindowOutcome::Close
        }
    }
}

impl SpaceAnalyzerWindow {
    fn ui(&mut self, ui: &mut egui::Ui, env: &ToolEnv) {
        let running = self.task.is_some();
        ui.horizontal(|ui| {
            ui.label("Path:");
            ui.add_enabled_ui(!running, |ui| {
                let roots = super::benchmark::drive_roots(env);
                egui::ComboBox::from_id_salt("space_drive").width(90.0).selected_text("Drive").show_ui(ui, |ui| {
                    for (root, desc) in &roots {
                        ui.selectable_value(&mut self.path, root.clone(), desc);
                    }
                });
                ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(320.0).hint_text("C:\\Users"));
            });
            if running {
                if ui.button("Stop").clicked() {
                    if let Some(t) = &self.task {
                        t.cancel();
                    }
                }
            } else if ui.add_enabled(!self.path.trim().is_empty(), egui::Button::new("Scan")).clicked() {
                self.start();
            }
        });
        if let Some(t) = &self.task {
            if progress_panel(ui, &t.progress(), t.elapsed(), t.is_cancelled(), false) {
                t.cancel();
            }
        }
        if let Some(e) = &self.error {
            super::danger(ui, e.as_str());
        }
        let Some(a) = &self.analysis else {
            if self.task.is_none() {
                ui.weak("Choose a drive or folder and press Scan.");
            }
            return;
        };
        ui.label(format!(
            "{}: {} in {} files, {} folders{}",
            a.report.root.path.display(),
            format_size(a.report.total),
            a.report.root.files,
            a.report.root.dirs,
            if a.report.errors > 0 { format!(" ({} entries could not be read)", a.report.errors) } else { String::new() }
        ));
        ui.separator();
        let mut drill_to: Option<Vec<usize>> = None;
        ui.columns(2, |cols| {
            let ui = &mut cols[0];
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Tree, "Tree view");
                ui.selectable_value(&mut self.tab, Tab::Files, "File view");
                ui.selectable_value(&mut self.tab, Tab::Folders, "Folder view");
            });
            ui.separator();
            egui::ScrollArea::both().id_salt("space_tab").max_height(440.0).auto_shrink([false, false]).show(ui, |ui| {
                match self.tab {
                    Tab::Tree => tree(ui, &a.report.root, a.report.total, &mut Vec::new(), &mut drill_to),
                    Tab::Files => files(ui, &a.report),
                    Tab::Folders => folders(ui, &a.folders, a.report.total),
                }
            });
            let ui = &mut cols[1];
            let node = node_at(&a.report.root, &self.drill);
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.drill.is_empty(), egui::Button::new("Up")).clicked() {
                    let mut d = self.drill.clone();
                    d.pop();
                    drill_to = Some(d);
                }
                ui.label(format!("{} ({})", node.path.display(), format_size(node.size)));
            });
            if let Some(i) = treemap(ui, node) {
                let mut d = self.drill.clone();
                d.push(i);
                drill_to = Some(d);
            }
        });
        if let Some(d) = drill_to {
            self.drill = d;
        }
    }
}

/// One collapsible tree level; a "Show in treemap" click sets `drill_to`.
fn tree(ui: &mut egui::Ui, n: &Node, total: u64, path: &mut Vec<usize>, drill_to: &mut Option<Vec<usize>>) {
    let text = format!(
        "{}   {}   {:.1}%   {} files",
        if n.name.is_empty() { n.path.display().to_string() } else { n.name.clone() },
        format_size(n.size),
        pct(n.size, total),
        n.files
    );
    if n.children.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(18.0);
            ui.label(text);
        });
        return;
    }
    let resp = egui::CollapsingHeader::new(text)
        .id_salt(("space_tree", n.path.as_os_str()))
        .default_open(path.is_empty())
        .show(ui, |ui| {
            for (i, c) in n.children.iter().enumerate().take(TREE_LIMIT) {
                path.push(i);
                tree(ui, c, total, path, drill_to);
                path.pop();
            }
            if n.children.len() > TREE_LIMIT {
                ui.weak(format!("… and {} more folders", n.children.len() - TREE_LIMIT));
            }
        });
    resp.header_response.context_menu(|ui| {
        if ui.button("Show in treemap").clicked() {
            *drill_to = Some(path.clone());
            ui.close_menu();
        }
    });
}

fn files(ui: &mut egui::Ui, r: &SpaceReport) {
    egui::Grid::new("space_files").num_columns(3).striped(true).show(ui, |ui| {
        ui.strong("File");
        ui.strong("Size");
        ui.strong("%");
        ui.end_row();
        for (p, size) in &r.largest {
            ui.label(p.display().to_string());
            ui.label(format_size(*size));
            ui.label(format!("{:.1}", pct(*size, r.total)));
            ui.end_row();
        }
    });
}

fn folders(ui: &mut egui::Ui, f: &[(PathBuf, u64, u64)], total: u64) {
    egui::Grid::new("space_folders").num_columns(4).striped(true).show(ui, |ui| {
        for h in ["Folder", "Size", "%", "Files"] {
            ui.strong(h);
        }
        ui.end_row();
        for (p, size, files) in f {
            ui.label(p.display().to_string());
            ui.label(format_size(*size));
            ui.label(format!("{:.1}", pct(*size, total)));
            ui.label(files.to_string());
            ui.end_row();
        }
    });
}

/// Color of cell `i` (stable per index).
fn cell_color(i: usize, dark: bool) -> Color32 {
    const HUES: [(u8, u8, u8); 8] = [
        (0x3C, 0x78, 0xC8),
        (0x2C, 0xB5, 0xC8),
        (0x5A, 0xA0, 0x46),
        (0xE0, 0x80, 0x10),
        (0x9B, 0x59, 0xB6),
        (0xC8, 0x50, 0x50),
        (0x4B, 0x5A, 0xA8),
        (0xB0, 0x90, 0x30),
    ];
    let (r, g, b) = HUES[i % HUES.len()];
    let c = Color32::from_rgb(r, g, b);
    if dark {
        crate::theme::mix(c, Color32::BLACK, 0.25)
    } else {
        crate::theme::mix(c, Color32::WHITE, 0.15)
    }
}

/// Draw the treemap of `n`; returns the child index of a clicked folder cell.
fn treemap(ui: &mut egui::Ui, n: &Node) -> Option<usize> {
    let size = vec2(ui.available_width().max(200.0), 440.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let painter = ui.painter_at(rect);
    let dark = ui.visuals().dark_mode;
    let border = ui.visuals().window_fill;
    let cells = squarify(&map_items(n), rect);
    let mut hovered: Option<(usize, Rect)> = None;
    let pointer = resp.hover_pos();
    for (id, r) in &cells {
        let color = match *id {
            usize::MAX => Color32::from_gray(if dark { 90 } else { 170 }),
            x if x == usize::MAX - 1 => Color32::from_gray(if dark { 70 } else { 200 }),
            i => cell_color(i, dark),
        };
        painter.rect_filled(*r, 0.0, color);
        painter.rect_stroke(*r, 0.0, Stroke::new(1.0_f32, border));
        if r.width() > 50.0 && r.height() > 16.0 {
            let name = match *id {
                usize::MAX => "(files)".to_string(),
                x if x == usize::MAX - 1 => "(other folders)".to_string(),
                i => n.children[i].name.clone(),
            };
            painter.with_clip_rect(r.shrink(2.0)).text(
                r.min + vec2(3.0, 2.0),
                Align2::LEFT_TOP,
                name,
                FontId::proportional(11.0),
                Color32::WHITE,
            );
        }
        if pointer.is_some_and(|p| r.contains(p)) {
            hovered = Some((*id, *r));
        }
    }
    if cells.is_empty() {
        painter.text(rect.center(), Align2::CENTER_CENTER, "(empty)", FontId::proportional(12.0), ui.visuals().weak_text_color());
    }
    let (id, r) = hovered?;
    painter.rect_stroke(r, 0.0, Stroke::new(2.0_f32, crate::theme::SELECTED));
    let (path, bytes) = match id {
        usize::MAX => (format!("{} (files in this folder)", n.path.display()), n.size - n.children.iter().map(|c| c.size).sum::<u64>()),
        x if x == usize::MAX - 1 => ("(other folders)".to_string(), n.children.iter().skip(MAP_LIMIT).map(|c| c.size).sum()),
        i => (n.children[i].path.display().to_string(), n.children[i].size),
    };
    let clicked = resp.clicked();
    resp.on_hover_text_at_pointer(format!("{path}\n{}", format_size(bytes)));
    (clicked && id < n.children.len() && !n.children[id].children.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, size: u64, files: u64, children: Vec<Node>) -> Node {
        Node { name: name.into(), path: PathBuf::from(name), size, files, dirs: 0, children, is_dir: true }
    }

    #[test]
    fn folders_and_treemap_items() {
        let root = node("root", 100, 10, vec![node("a", 60, 6, vec![node("a1", 50, 5, vec![])]), node("b", 30, 3, vec![])]);
        let f = largest_folders(&root, 3);
        assert_eq!(f.iter().map(|x| x.1).collect::<Vec<_>>(), [100, 60, 50]);
        assert_eq!(node_at(&root, &[0, 0]).name, "a1");
        assert_eq!(node_at(&root, &[5]).name, "root", "missing index stops");
        // a, b, and 10 bytes of files directly in root
        assert_eq!(map_items(&root), [(60.0, 0), (30.0, 1), (10.0, usize::MAX)]);
        assert_eq!(pct(1, 4), 25.0);
        assert_eq!(pct(1, 0), 0.0);
    }

    #[test]
    fn window_renders_a_report() {
        let mut w = SpaceAnalyzerWindow::new(Some("C:\\".into()));
        let root = node("root", 100, 10, vec![node("a", 60, 6, vec![node("a1", 50, 5, vec![])]), node("b", 30, 3, vec![])]);
        let folders = largest_folders(&root, 10);
        w.analysis = Some(Analysis {
            report: SpaceReport { root, total: 100, by_ext: vec![], largest: vec![(PathBuf::from("x.bin"), 40)], errors: 0 },
            folders,
        });
        let m = pm_ops::Model::default();
        let p: std::sync::Arc<dyn pm_platform::Platform> = std::sync::Arc::new(pm_platform::NullPlatform);
        let env = ToolEnv { platform: &p, live: &m, pending: &[] };
        let ctx = egui::Context::default();
        for tab in [Tab::Tree, Tab::Files, Tab::Folders] {
            w.tab = tab;
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                assert_eq!(w.show(ctx, &env), WindowOutcome::Open);
            });
        }
    }
}
