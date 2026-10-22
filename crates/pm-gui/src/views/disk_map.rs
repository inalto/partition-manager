//! The disk map: one horizontal bar per disk, a label block followed by one segment per
//! partition / unallocated gap, proportional to its size with a minimum width.

use crate::items::{disk_items, ItemKind};
use crate::selection::Selected;
use super::context_menu;
use crate::app::{AppState, Cmd};
use crate::format_util::{fs_text, partition_name};
use crate::theme::{map_colors, MapColors};
use eframe::egui::{self, pos2, vec2, Align2, FontId, Rect, Sense, Stroke};
use pm_core::{format_size, DiskInfo, TableKind};

/// Narrowest a segment is drawn, in points, as long as the bar is wide enough.
pub const MIN_SEGMENT_WIDTH: f32 = 40.0;

/// One drawn segment of a disk bar; `x` is relative to the start of the bar.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub x: f32,
    pub w: f32,
    pub kind: ItemKind,
    pub label: String,
    /// Used fraction (0..=1) drawn as a fill inside the segment.
    pub used_frac: f32,
    pub selectable: bool,
    pub sel: Selected,
}

/// Widths for items of byte lengths `lens` filling exactly `width`.
///
/// Widths are proportional to the lengths; items that would be narrower than `min_w` are
/// pinned to `min_w` and the others share what is left, again proportionally (repeated until
/// no further item drops below the minimum). When the bar cannot hold `min_w` per item, every
/// item gets the same width.
fn widths(lens: &[u64], width: f32, min_w: f32) -> Vec<f32> {
    let n = lens.len();
    if n == 0 {
        return Vec::new();
    }
    let width = width.max(0.0);
    if min_w * n as f32 >= width {
        return vec![width / n as f32; n];
    }
    let mut pinned = vec![false; n];
    loop {
        let n_pinned = pinned.iter().filter(|p| **p).count();
        let room = (width - min_w * n_pinned as f32) as f64;
        let free_total: f64 = lens
            .iter()
            .zip(&pinned)
            .filter(|(_, p)| !**p)
            .map(|(l, _)| *l as f64)
            .sum();
        let n_free = (n - n_pinned) as f64;
        let share = |len: u64| {
            if free_total > 0.0 {
                room * len as f64 / free_total
            } else {
                room / n_free
            }
        };
        let mut changed = false;
        for i in 0..n {
            if !pinned[i] && share(lens[i]) < min_w as f64 {
                pinned[i] = true;
                changed = true;
            }
        }
        // Terminates: the largest unpinned item always gets at least room / n_free > min_w,
        // so one item always stays unpinned, and every further pass pins one more or stops.
        if !changed {
            return (0..n)
                .map(|i| if pinned[i] { min_w } else { share(lens[i]) as f32 })
                .collect();
        }
    }
}

/// Lay out the segments of `disk` over a bar `width` points wide (see the module docs).
/// The segments are contiguous, start at 0 and end exactly at `width`.
pub fn segments(disk: &DiskInfo, width: f32) -> Vec<Segment> {
    let width = width.max(0.0);
    let items = disk_items(disk);
    if items.is_empty() {
        return vec![Segment {
            x: 0.0,
            w: width,
            kind: ItemKind::Unallocated,
            label: format_size(disk.size),
            used_frac: 0.0,
            selectable: false,
            sel: Selected::Disk(disk.index),
        }];
    }
    let lens: Vec<u64> = items.iter().map(|i| i.len).collect();
    let ws = widths(&lens, width, MIN_SEGMENT_WIDTH);
    let last = items.len() - 1;
    let mut x = 0.0f32;
    let mut out = Vec::with_capacity(items.len());
    for (i, (item, mut w)) in items.iter().zip(ws).enumerate() {
        if i == last {
            // absorb float rounding so the bar ends exactly at `width`
            w = (width - x).max(0.0);
        }
        let label = match item.sel {
            Selected::Partition(_, n) => disk
                .partitions
                .iter()
                .find(|p| p.number == n)
                .map(|p| format!("{}\n{} {}", partition_name(p), format_size(p.len), fs_text(p)))
                .unwrap_or_default(),
            _ => format!("Unallocated\n{}", format_size(item.len)),
        };
        let used_frac = match item.used {
            Some(u) if item.len > 0 => (u as f64 / item.len as f64).clamp(0.0, 1.0) as f32,
            _ => 0.0,
        };
        out.push(Segment { x, w, kind: item.kind, label, used_frac, selectable: true, sel: item.sel });
        x += w;
    }
    out
}

/// Height of one disk bar.
const BAR_HEIGHT: f32 = 56.0;
/// Width of the "Disk N / GPT / size" block at the left of each bar.
const LABEL_WIDTH: f32 = 110.0;
/// Height of the colored strip at the top of each segment.
const STRIP: f32 = 6.0;

/// Draw one bar per disk of the preview model.
pub fn show(ui: &mut egui::Ui, s: &AppState, out: &mut Vec<Cmd>) {
    let colors = map_colors(s.theme);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(4.0);
        for d in &s.preview().disks {
            disk_bar(ui, s, d, &colors, out);
            ui.add_space(8.0);
        }
    });
}

fn table_name(d: &DiskInfo) -> &'static str {
    match d.table {
        TableKind::Gpt => "GPT",
        TableKind::Mbr => "MBR",
        TableKind::None => "Uninitialized",
        TableKind::Unknown => "Unreadable",
    }
}

fn disk_bar(ui: &mut egui::Ui, s: &AppState, d: &DiskInfo, c: &MapColors, out: &mut Vec<Cmd>) {
    let width = ui.available_width().max(LABEL_WIDTH + 2.0 * MIN_SEGMENT_WIDTH);
    let (rect, _) = ui.allocate_exact_size(vec2(width, BAR_HEIGHT), Sense::hover());
    let painter = ui.painter_at(rect);

    // label block
    let label_rect = Rect::from_min_size(rect.min, vec2(LABEL_WIDTH, BAR_HEIGHT));
    let disk_sel = Selected::Disk(d.index);
    let resp = ui.interact(label_rect, ui.id().with(("disk_label", d.index)), Sense::click());
    painter.rect_filled(label_rect, 3.0, c.label_block);
    let lines = [
        (format!("Disk {}", d.index), true),
        (table_name(d).to_string(), false),
        (format_size(d.size), false),
    ];
    for (i, (text, strong)) in lines.iter().enumerate() {
        let font = if *strong { FontId::proportional(13.0) } else { FontId::proportional(11.5) };
        painter.text(
            label_rect.min + vec2(8.0, 6.0 + i as f32 * 15.0),
            Align2::LEFT_TOP,
            text,
            font,
            c.text,
        );
    }
    if s.selected == disk_sel {
        painter.rect_stroke(label_rect.shrink(1.0), 3.0, Stroke::new(2.0_f32, c.selected));
    }
    if resp.clicked() || resp.secondary_clicked() {
        out.push(Cmd::Select(disk_sel));
    }
    resp.on_hover_text(crate::format_util::disk_header(d, s.platform.elevation_hint())).context_menu(|ui| context_menu::show(ui, s, disk_sel, out));

    // segments
    let bar = Rect::from_min_max(pos2(rect.min.x + LABEL_WIDTH + 4.0, rect.min.y), rect.max);
    for (i, seg) in segments(d, bar.width()).iter().enumerate() {
        let r = Rect::from_min_size(pos2(bar.min.x + seg.x, bar.min.y), vec2(seg.w, BAR_HEIGHT));
        let body = r.shrink2(vec2(1.0, 0.0));
        let color = c.of(seg.kind);
        painter.rect_filled(body, 2.0, if seg.kind == ItemKind::Unallocated { c.label_block } else { c.body });
        if seg.used_frac > 0.0 {
            let used = Rect::from_min_max(
                pos2(body.min.x, body.min.y + STRIP),
                pos2(body.min.x + body.width() * seg.used_frac, body.max.y),
            );
            painter.rect_filled(used, 0.0, c.used_fill(seg.kind));
        }
        painter.rect_filled(Rect::from_min_size(body.min, vec2(body.width(), STRIP)), 2.0, color);
        painter.rect_stroke(body, 2.0, Stroke::new(1.0_f32, c.border));
        painter.with_clip_rect(body.shrink(3.0)).text(
            body.min + vec2(4.0, STRIP + 4.0),
            Align2::LEFT_TOP,
            &seg.label,
            FontId::proportional(11.5),
            c.text,
        );
        if !seg.selectable {
            continue;
        }
        let resp = ui.interact(body, ui.id().with(("segment", d.index, i)), Sense::click());
        if s.selected == seg.sel {
            painter.rect_stroke(body.shrink(1.0), 2.0, Stroke::new(2.0_f32, c.selected));
        } else if resp.hovered() {
            painter.rect_stroke(body.shrink(1.0), 2.0, Stroke::new(1.0_f32, color));
        }
        if resp.clicked() || resp.secondary_clicked() {
            out.push(Cmd::Select(seg.sel));
        }
        let sel = seg.sel;
        resp.on_hover_text(&seg.label).context_menu(|ui| context_menu::show(ui, s, sel, out));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, part};
    use pm_core::{TableKind, GIB, MIB};

    fn total(segs: &[Segment]) -> f32 {
        segs.iter().map(|s| s.w).sum()
    }

    fn contiguous(segs: &[Segment]) {
        let mut x = 0.0;
        for s in segs {
            assert!((s.x - x).abs() < 1e-3, "gap before {s:?}");
            x += s.w;
        }
    }

    #[test]
    fn segments_sum_to_width() {
        for width in [200.0, 640.0, 1234.5] {
            for d in [gpt_disk(1), mbr_disk(0)] {
                let segs = segments(&d, width);
                assert_eq!(segs.len(), disk_items(&d).len());
                assert!((total(&segs) - width).abs() < 1e-3, "{width}: {}", total(&segs));
                contiguous(&segs);
            }
        }
    }

    #[test]
    fn min_width_enforced() {
        // a 1 TiB disk with a 16 MiB partition between two big ones: proportionally the small
        // one would be a fraction of a point wide
        let mut d = DiskInfo { index: 2, size: 1024 * GIB, sector_size: 512, table: TableKind::Gpt, ..Default::default() };
        d.partitions = vec![part(1, 1, 16), part(2, 17, 500 * 1024), part(3, 17 + 500 * 1024, 16)];
        let width = 800.0;
        let segs = segments(&d, width);
        assert!(segs.iter().all(|s| s.w >= MIN_SEGMENT_WIDTH - 1e-3), "{segs:?}");
        assert!((segs[0].w - MIN_SEGMENT_WIDTH).abs() < 1e-3);
        // the large partition and the big tail gap shrink to make room, keeping their ratio
        let big = &segs[1];
        let tail = segs.last().unwrap();
        assert!(big.w > MIN_SEGMENT_WIDTH && tail.w > MIN_SEGMENT_WIDTH);
        let tail_len = disk_items(&d).last().unwrap().len as f32;
        let ratio = big.w / tail.w;
        let want = (500.0 * 1024.0 * MIB as f32) / tail_len;
        assert!((ratio - want).abs() / want < 1e-3, "{ratio} vs {want}");
        assert!((total(&segs) - width).abs() < 1e-3);
        contiguous(&segs);
    }

    #[test]
    fn too_narrow_bar_splits_evenly() {
        let d = gpt_disk(1); // 4 items
        let segs = segments(&d, 100.0);
        assert!(segs.iter().all(|s| (s.w - 25.0).abs() < 1e-3));
    }

    #[test]
    fn labels_usage_and_selection() {
        let d = gpt_disk(1);
        let segs = segments(&d, 600.0);
        assert_eq!(segs[2].label, "E:Data\n684.00 MB NTFS");
        assert!((segs[2].used_frac - 0.5).abs() < 1e-6);
        assert_eq!(segs[2].sel, Selected::Partition(1, 3));
        assert!(segs[2].selectable);
        let free = segs.last().unwrap();
        assert_eq!(free.kind, ItemKind::Unallocated);
        assert!(free.label.starts_with("Unallocated\n"));
        assert_eq!(free.used_frac, 0.0);
        assert_eq!(segs[1].used_frac, 0.0); // MSR: usage unknown
    }

    #[test]
    fn empty_disk_gets_one_placeholder_segment() {
        let d = DiskInfo { index: 4, size: 0, ..Default::default() };
        let segs = segments(&d, 300.0);
        assert_eq!(segs.len(), 1);
        assert!(!segs[0].selectable);
        assert_eq!(segs[0].w, 300.0);
        assert_eq!(segs[0].sel, Selected::Disk(4));
        assert!(segments(&gpt_disk(1), 0.0).iter().all(|s| s.w == 0.0));
    }
}
