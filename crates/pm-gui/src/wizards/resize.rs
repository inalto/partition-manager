//! Resize/Move wizard: the MiniTool bar. The bar spans the partition's maximal extent (its
//! free neighbours); the left handle moves the start, the right handle the end and the body
//! moves the whole partition. "Unallocated space before / Partition size / Unallocated space
//! after" fields stay in sync with it. The used data is drawn hatched and cannot be shrunk
//! into. A live `Planner::simulate` preview shows the disk before and after, and errors
//! inline. → [`Operation::ResizeMove`].
//!
//! [`BarState`] / [`bar_editor`] are shared with the Create wizard's size step; the pure
//! math lives in [`super::resize_bar`].

use super::resize_bar::{Extent, Handle};
use super::{disk_preview, partition_desc, row, size_edit};
use crate::items::is_container;
use crate::selection::Selected;
use crate::theme::{map_colors, mix, Theme};
use crate::wizard::{check_ops, shared, StepResult, Wizard, WizardCtx};
use eframe::egui::{self, pos2, vec2, Align2, CursorIcon, FontId, Rect, Sense, Stroke};
use pm_core::{format_size, DiskInfo, PartitionInfo, TableKind, MIB};
use pm_ops::Operation;

/// A drag in progress: the handle, the block when it started and the pointer's x then.
#[derive(Clone, Copy, Debug, PartialEq)]
struct BarDrag {
    handle: Handle,
    start: u64,
    len: u64,
    x0: f32,
}

/// The block edited by [`bar_editor`].
#[derive(Clone, Debug, PartialEq)]
pub struct BarState {
    pub ext: Extent,
    pub start: u64,
    pub len: u64,
    /// Used bytes from the start of the block (hatched), when known.
    pub used: Option<u64>,
    drag: Option<BarDrag>,
    texts: [String; 3],
}

impl BarState {
    pub fn new(ext: Extent, start: u64, len: u64, used: Option<u64>) -> BarState {
        BarState { ext, start, len, used, drag: None, texts: Default::default() }
    }

    /// Set the block from the "before / size / after" fields (`None` keeps a field).
    pub fn apply_fields(&mut self, before: Option<u64>, size: Option<u64>, after: Option<u64>) {
        let e = self.ext;
        if let Some(v) = before {
            (self.start, self.len) = e.set_before(self.start, self.len, v);
        }
        if let Some(v) = size {
            (self.start, self.len) = e.set_size(self.start, self.len, v);
        }
        if let Some(v) = after {
            (self.start, self.len) = e.set_after(self.start, self.len, v);
        }
    }
}

const BAR_HEIGHT: f32 = 46.0;
const HANDLE_W: f32 = 9.0;

/// Draw the bar and its three fields; returns `true` when the block changed.
pub fn bar_editor(ui: &mut egui::Ui, b: &mut BarState) -> bool {
    let theme = if ui.visuals().dark_mode { Theme::Dark } else { Theme::Light };
    let c = map_colors(theme);
    let before = (b.start, b.len);
    let width = ui.available_width().max(200.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, BAR_HEIGHT), Sense::hover());
    // keep the handles at the very ends grabbable
    let bar = rect.shrink2(vec2(HANDLE_W / 2.0 + 1.0, 0.0));
    let w = bar.width();
    let e = b.ext;
    let x_at = |v: u64| bar.min.x + e.x_of(v, w);
    let painter = ui.painter_at(rect);

    // free extent
    painter.rect_filled(bar, 3.0, c.label_block);
    painter.rect_stroke(bar, 3.0, Stroke::new(1.0_f32, c.border));

    // the partition
    let (x0, x1) = (x_at(b.start), x_at(b.start.saturating_add(b.len)));
    let block = Rect::from_min_max(pos2(x0, bar.min.y), pos2(x1.max(x0 + 2.0), bar.max.y));
    painter.rect_filled(block, 2.0, c.body);
    if let Some(used) = b.used.filter(|u| *u > 0) {
        let ux = x_at(b.start.saturating_add(used.min(b.len)));
        let r = Rect::from_min_max(pos2(x0, block.min.y + 6.0), pos2(ux, block.max.y));
        painter.rect_filled(r, 0.0, c.used_fill(crate::items::ItemKind::Primary));
        let hatch = painter.with_clip_rect(r);
        let stroke = Stroke::new(1.0_f32, mix(c.primary, c.body, 0.3));
        let mut x = r.min.x - r.height();
        while x < r.max.x {
            hatch.line_segment([pos2(x, r.max.y), pos2(x + r.height(), r.min.y)], stroke);
            x += 6.0;
        }
    }
    painter.rect_filled(Rect::from_min_size(block.min, vec2(block.width(), 6.0)), 2.0, c.primary);
    painter.rect_stroke(block, 2.0, Stroke::new(1.0_f32, c.primary));
    painter.with_clip_rect(block.shrink(2.0)).text(
        block.center() + vec2(0.0, 3.0),
        Align2::CENTER_CENTER,
        format_size(b.len),
        FontId::proportional(12.0),
        c.text,
    );

    // interaction: body first so the handles (added later) win where they overlap
    let id = ui.id().with("resize_bar");
    let hsize = vec2(HANDLE_W, BAR_HEIGHT * 0.7);
    let left = Rect::from_center_size(pos2(x0, bar.center().y), hsize);
    let right = Rect::from_center_size(pos2(x1, bar.center().y), hsize);
    let parts = [
        (ui.interact(block, id.with("body"), Sense::drag()), Handle::Body),
        (ui.interact(left, id.with("start"), Sense::drag()), Handle::Start),
        (ui.interact(right, id.with("end"), Sense::drag()), Handle::End),
    ];
    for r in [left, right] {
        painter.rect_filled(r, 2.0, c.selected);
        painter.rect_stroke(r, 2.0, Stroke::new(1.0_f32, c.border));
    }
    for (resp, handle) in parts {
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(match handle {
                Handle::Body if resp.dragged() => CursorIcon::Grabbing,
                Handle::Body => CursorIcon::Grab,
                _ => CursorIcon::ResizeHorizontal,
            });
        }
        let pointer_x = resp.interact_pointer_pos().map(|p| p.x);
        if resp.drag_started() {
            if let Some(x0) = pointer_x {
                b.drag = Some(BarDrag { handle, start: b.start, len: b.len, x0 });
            }
        }
        if resp.dragged() {
            if let (Some(d), Some(x)) = (b.drag, pointer_x) {
                if d.handle == handle {
                    (b.start, b.len) = e.drag(handle, d.start, d.len, e.delta_bytes(x - d.x0, w));
                }
            }
        }
        if resp.drag_stopped() {
            b.drag = None;
        }
    }

    // the fields
    ui.add_space(6.0);
    let (fb, fs, fa) = e.fields(b.start, b.len);
    let mut edits = [None; 3];
    egui::Grid::new(id.with("fields")).num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
        for (i, (label, value)) in
            [("Unallocated space before:", fb), ("Partition size:", fs), ("Unallocated space after:", fa)]
                .into_iter()
                .enumerate()
        {
            ui.label(label);
            edits[i] = size_edit(ui, &mut b.texts[i], value);
            ui.end_row();
        }
    });
    b.apply_fields(edits[0], edits[1], edits[2]);
    (b.start, b.len) != before
}

/// End of the area partitions may use on `d` (before the GPT backup structures).
pub fn usable_end(d: &DiskInfo) -> u64 {
    match d.table {
        TableKind::Gpt => d.size.saturating_sub(33 * d.sector_size.max(512) as u64),
        _ => d.size,
    }
}

/// The range partition `p` of `d` may occupy: from the end of its left neighbour to the start
/// of its right neighbour (inside its extended container for a logical partition, leaving
/// the 1 MiB EBR slack in front of it). The minimum length is the used data (at least
/// 1 MiB); a filesystem of unknown fill level may not shrink at all.
pub fn extent_for(d: &DiskInfo, p: &PartitionInfo) -> Extent {
    let container = p
        .logical
        .then(|| d.partitions.iter().find(|c| is_container(d, c) && c.start <= p.start && c.end() >= p.end()))
        .flatten();
    let (mut lo, mut hi) = match container {
        Some(c) => (c.start + MIB, c.end()),
        None => (MIB, usable_end(d)),
    };
    for q in d.partitions.iter().filter(|q| q.number != p.number && q.len > 0) {
        let other_level = if p.logical { is_container(d, q) || !q.logical } else { q.logical };
        if other_level {
            continue;
        }
        if q.end() <= p.start {
            lo = lo.max(q.end() + if p.logical { MIB } else { 0 });
        } else if q.start >= p.end() {
            hi = hi.min(q.start);
        }
    }
    let (lo, hi) = (lo.min(p.start), hi.max(p.end()));
    let min_len = match p.used {
        Some(u) => u.max(MIB),
        None if p.fs.is_some() => p.len,
        None => MIB,
    }
    .min(hi - lo);
    Extent { lo, hi, min_len, grid: MIB, keep_start: Some(p.start), keep_end: Some(p.end()), end_at_hi: true }
}

pub struct ResizeState {
    pub disk: u32,
    pub number: u32,
    pub desc: String,
    pub orig_start: u64,
    pub orig_len: u64,
    pub bar: BarState,
}

impl ResizeState {
    pub fn initial(ctx: &WizardCtx) -> Option<ResizeState> {
        let Selected::Partition(disk, number) = ctx.selection else { return None };
        let d = ctx.model.disk(disk)?;
        let p = ctx.model.partition(disk, number).filter(|p| !is_container(d, p))?;
        Some(ResizeState {
            disk,
            number,
            desc: partition_desc(disk, p),
            orig_start: p.start,
            orig_len: p.len,
            bar: BarState::new(extent_for(d, p), p.start, p.len, p.used),
        })
    }

    pub fn op(&self) -> Operation {
        Operation::ResizeMove { disk: self.disk, number: self.number, new_start: self.bar.start, new_len: self.bar.len }
    }

    pub fn build(&self) -> Result<Vec<Operation>, String> {
        if (self.bar.start, self.bar.len) == (self.orig_start, self.orig_len) {
            return Err("Drag the handles or type a new size: the partition is unchanged.".into());
        }
        Ok(vec![self.op()])
    }
}

fn step_ui(s: &mut ResizeState, ui: &mut egui::Ui, ctx: &WizardCtx) -> StepResult {
    ui.label(&s.desc);
    ui.weak("Drag the handles to resize, drag the partition to move it.");
    ui.add_space(6.0);
    bar_editor(ui, &mut s.bar);
    if let Some(u) = s.bar.used {
        ui.weak(format!("Used data (hatched, cannot be shrunk into): {}", format_size(u)));
    } else if s.bar.ext.min_len == s.orig_len {
        ui.weak("The used space of this volume is unknown: it can grow or move, not shrink.");
    }
    ui.add_space(8.0);
    let sel = Some(Selected::Partition(s.disk, s.number));
    if let Some(d) = ctx.model.disk(s.disk) {
        ui.label("Before:");
        disk_preview(ui, d, sel);
    }
    let result = s.build().and_then(|ops| check_ops(ctx.model, &ops));
    if let Ok(m) = &result {
        if let Some(d) = m.disk(s.disk) {
            ui.label("After:");
            disk_preview(ui, d, sel);
        }
    }
    StepResult::from_problem(result.err())
}

fn summary(s: &ResizeState) -> Vec<(String, String)> {
    let (b, size, a) = s.bar.ext.fields(s.bar.start, s.bar.len);
    vec![
        row("Partition", s.desc.clone()),
        row("Size", format!("{} → {}", format_size(s.orig_len), format_size(size))),
        row("Unallocated before", format_size(b)),
        row("Unallocated after", format_size(a)),
    ]
}

pub fn wizard(state: ResizeState) -> Wizard {
    Wizard::over_state("Resize/Move Partition", shared(state), vec![("Size and position", step_ui, summary)], ResizeState::build)
}

pub fn new(ctx: &WizardCtx) -> Option<Wizard> {
    ResizeState::initial(ctx).map(wizard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};
    use pm_core::GIB;

    #[test]
    fn extent_spans_free_neighbours_and_min_is_used() {
        let d = gpt_disk(1);
        let e = extent_for(&d, &d.partitions[2]); // data 117..801 MiB, 342 MiB used
        assert_eq!((e.lo, e.hi), (117 * MIB, 1024 * MIB - 33 * 512));
        assert_eq!(e.min_len, 342 * MIB);
        // MSR (no fs, usage unknown): may shrink to 1 MiB, wedged between EFI and data
        let msr = extent_for(&d, &d.partitions[1]);
        assert_eq!((msr.lo, msr.hi, msr.min_len), (101 * MIB, 117 * MIB, MIB));
        // MBR: the primary sees the container as its neighbour, a logical its siblings
        let m = mbr_disk(0);
        let p1 = extent_for(&m, &m.partitions[0]);
        assert_eq!((p1.lo, p1.hi), (MIB, 300 * MIB));
        assert_eq!(p1.min_len, 200 * MIB, "NTFS of unknown usage cannot shrink");
        let l6 = extent_for(&m, &m.partitions[3]);
        assert_eq!((l6.lo, l6.hi), (402 * MIB, 700 * MIB));
        let l5 = extent_for(&m, &m.partitions[2]);
        assert_eq!((l5.lo, l5.hi), (301 * MIB, 402 * MIB));
    }

    #[test]
    fn build_resize_op_from_state() {
        let m = model_of(vec![gpt_disk(1)]);
        let ctx = WizardCtx::new(&m, vec![], Selected::Partition(1, 3));
        let mut s = ResizeState::initial(&ctx).unwrap();
        assert!(s.build().is_err(), "unchanged");
        s.bar.apply_fields(None, Some(GIB), None); // grow to the end of the free tail
        let ops = s.build().unwrap();
        let want_len = 1024 * MIB - 33 * 512 - 117 * MIB;
        assert_eq!(ops, vec![Operation::ResizeMove { disk: 1, number: 3, new_start: 117 * MIB, new_len: want_len }]);
        assert!(check_ops(&m, &ops).is_ok(), "{:?}", check_ops(&m, &ops));
        // shrinking below the used data is clamped
        s.bar.apply_fields(None, Some(MIB), None);
        assert_eq!(s.bar.len, 342 * MIB);
        // move right by 50 MiB keeping the size: snapped, accepted by the planner
        s.bar.apply_fields(Some(50 * MIB), Some(400 * MIB), None);
        assert_eq!((s.bar.start, s.bar.len), (167 * MIB, 400 * MIB));
        assert!(check_ops(&m, &s.build().unwrap()).is_ok());
        // the wizard over the same state builds the same op
        let w = wizard(s);
        assert_eq!(
            w.build_ops().unwrap(),
            vec![Operation::ResizeMove { disk: 1, number: 3, new_start: 167 * MIB, new_len: 400 * MIB }]
        );
    }

    #[test]
    fn container_and_non_partition_selections_have_no_wizard() {
        let m = model_of(vec![mbr_disk(0)]);
        assert!(ResizeState::initial(&WizardCtx::new(&m, vec![], Selected::Partition(0, 2))).is_none());
        assert!(ResizeState::initial(&WizardCtx::new(&m, vec![], Selected::Disk(0))).is_none());
    }
}
