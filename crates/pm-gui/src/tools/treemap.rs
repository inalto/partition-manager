//! Squarified treemap layout (Bruls, Huizing, van Wijk 2000): cells whose areas are
//! proportional to their weights, with aspect ratios kept close to 1.
//!
//! Pure geometry, no egui drawing; the Space Analyzer paints the cells.

use eframe::egui::{pos2, Rect};

/// Lay `items` (`(weight, id)`) out inside `rect`. Every item with a positive, finite weight
/// gets one cell; the cells tile `rect` exactly, in decreasing weight order, and each cell's
/// area is `weight / total * rect.area()`. Items with a zero, negative or non-finite weight
/// are left out. Returns `(id, cell)` pairs.
pub fn squarify(items: &[(f64, usize)], rect: Rect) -> Vec<(usize, Rect)> {
    let mut v: Vec<(f64, usize)> = items.iter().copied().filter(|(w, _)| w.is_finite() && *w > 0.0).collect();
    let (w, h) = (rect.width() as f64, rect.height() as f64);
    let total: f64 = v.iter().map(|i| i.0).sum();
    if v.is_empty() || !(w > 0.0 && h > 0.0) || !(total > 0.0) {
        return Vec::new();
    }
    v.sort_by(|a, b| b.0.total_cmp(&a.0));
    let scale = w * h / total;
    let areas: Vec<(f64, usize)> = v.into_iter().map(|(wt, id)| (wt * scale, id)).collect();
    let mut free = Free { x: rect.min.x as f64, y: rect.min.y as f64, w, h };
    let mut out = Vec::with_capacity(areas.len());
    let mut row: Vec<(f64, usize)> = Vec::new();
    let mut i = 0;
    while i < areas.len() {
        let side = free.w.min(free.h);
        let grows_worse = !row.is_empty() && {
            let mut with = row.clone();
            with.push(areas[i]);
            worst(&with, side) > worst(&row, side)
        };
        if grows_worse {
            free.lay_out(&row, &mut out);
            row.clear();
        } else {
            row.push(areas[i]);
            i += 1;
        }
    }
    if !row.is_empty() {
        free.lay_out(&row, &mut out);
    }
    out
}

/// The part of the rectangle not laid out yet.
struct Free {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Free {
    /// Place `row` as a strip along the shorter side of the free area and shrink it.
    fn lay_out(&mut self, row: &[(f64, usize)], out: &mut Vec<(usize, Rect)>) {
        let sum: f64 = row.iter().map(|r| r.0).sum();
        let rect = |x: f64, y: f64, w: f64, h: f64| Rect::from_min_max(pos2(x as f32, y as f32), pos2((x + w) as f32, (y + h) as f32));
        if self.w >= self.h {
            // a column at the left, as tall as the free area
            let cw = if self.h > 0.0 { (sum / self.h).min(self.w) } else { 0.0 };
            let mut y = self.y;
            for &(a, id) in row {
                let ch = if cw > 0.0 { a / cw } else { 0.0 };
                out.push((id, rect(self.x, y, cw, ch)));
                y += ch;
            }
            self.x += cw;
            self.w -= cw;
        } else {
            // a row at the top, as wide as the free area
            let rh = if self.w > 0.0 { (sum / self.w).min(self.h) } else { 0.0 };
            let mut x = self.x;
            for &(a, id) in row {
                let cw = if rh > 0.0 { a / rh } else { 0.0 };
                out.push((id, rect(x, self.y, cw, rh)));
                x += cw;
            }
            self.y += rh;
            self.h -= rh;
        }
    }
}

/// The worst aspect ratio of `row` laid out along a side of length `side`.
fn worst(row: &[(f64, usize)], side: f64) -> f64 {
    let sum: f64 = row.iter().map(|r| r.0).sum();
    let max = row.iter().map(|r| r.0).fold(f64::MIN, f64::max);
    let min = row.iter().map(|r| r.0).fold(f64::MAX, f64::min);
    if sum <= 0.0 || min <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
    let s2 = side * side;
    (s2 * max / (sum * sum)).max(sum * sum / (s2 * min))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::vec2;

    fn area(r: &Rect) -> f64 {
        (r.width() as f64) * (r.height() as f64)
    }

    #[test]
    fn squarify_areas_proportional() {
        let rect = Rect::from_min_size(pos2(10.0, 20.0), vec2(600.0, 400.0));
        let items = [(6.0, 0), (6.0, 1), (4.0, 2), (3.0, 3), (2.0, 4), (2.0, 5), (1.0, 6), (0.0, 7), (-3.0, 8)];
        let cells = squarify(&items, rect);
        let total: f64 = items.iter().map(|i| i.0).filter(|w| *w > 0.0).sum();
        assert_eq!(cells.len(), 7, "zero and negative weights are left out");
        let mut ids: Vec<usize> = cells.iter().map(|c| c.0).collect();
        ids.sort_unstable();
        assert_eq!(ids, [0, 1, 2, 3, 4, 5, 6]);
        let eps = 1e-3;
        for (id, r) in &cells {
            let w = items.iter().find(|i| i.1 == *id).unwrap().0;
            let want = w / total * area(&rect);
            assert!((area(r) - want).abs() / want < 1e-3, "cell {id}: area {} want {want}", area(r));
            assert!(
                r.min.x >= rect.min.x - eps as f32
                    && r.min.y >= rect.min.y - eps as f32
                    && r.max.x <= rect.max.x + eps as f32
                    && r.max.y <= rect.max.y + eps as f32,
                "cell {id} {r:?} leaves {rect:?}"
            );
        }
        for (i, (a, ra)) in cells.iter().enumerate() {
            for (b, rb) in &cells[i + 1..] {
                let inter = ra.intersect(*rb);
                let overlap = if inter.width() > 0.0 && inter.height() > 0.0 { area(&inter) } else { 0.0 };
                assert!(overlap < 0.01, "cells {a} and {b} overlap by {overlap}");
            }
        }
        // the cells tile the rectangle
        let covered: f64 = cells.iter().map(|c| area(&c.1)).sum();
        assert!((covered - area(&rect)).abs() < 0.5);
        // squarified: no sliver cells for these weights
        for (_, r) in &cells {
            let ratio = (r.width() / r.height()).max(r.height() / r.width());
            assert!(ratio < 4.0, "aspect ratio {ratio} of {r:?}");
        }
    }

    #[test]
    fn squarify_degenerate_inputs() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 50.0));
        assert!(squarify(&[], rect).is_empty());
        assert!(squarify(&[(0.0, 1), (f64::NAN, 2)], rect).is_empty());
        let one = squarify(&[(5.0, 9)], rect);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].0, 9);
        assert!((area(&one[0].1) - 5000.0).abs() < 0.01);
        let empty = Rect::from_min_size(pos2(0.0, 0.0), vec2(0.0, 50.0));
        assert!(squarify(&[(1.0, 1)], empty).is_empty());
    }
}
