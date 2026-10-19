//! Light (MiniTool-like: light grey panels, blue accents) and dark themes, plus the colors
//! of the disk map and its legend.

use crate::items::ItemKind;
use eframe::egui::{self, Color32, Rounding, Stroke, Visuals};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Light,
    Dark,
}

impl Theme {
    pub fn toggled(self) -> Theme {
        match self {
            Theme::Light => Theme::Dark,
            Theme::Dark => Theme::Light,
        }
    }
}

pub const ACCENT: Color32 = Color32::from_rgb(0x3C, 0x78, 0xC8);
pub const SELECTED: Color32 = Color32::from_rgb(0xE5, 0xA8, 0x00);
pub const DANGER: Color32 = Color32::from_rgb(0xD0, 0x30, 0x30);

/// Colors of the disk map.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapColors {
    pub primary: Color32,
    pub logical: Color32,
    pub unallocated: Color32,
    pub system: Color32,
    pub recovery: Color32,
    pub selected: Color32,
    /// Segment body behind the usage fill.
    pub body: Color32,
    pub border: Color32,
    pub text: Color32,
    /// Background of the "Disk N" block at the left of each bar.
    pub label_block: Color32,
}

impl MapColors {
    pub fn of(&self, kind: ItemKind) -> Color32 {
        match kind {
            ItemKind::Primary => self.primary,
            ItemKind::Logical => self.logical,
            ItemKind::Unallocated => self.unallocated,
            ItemKind::System => self.system,
            ItemKind::Recovery => self.recovery,
        }
    }

    /// The usage fill drawn inside a segment of `kind`: its color blended into the body.
    pub fn used_fill(&self, kind: ItemKind) -> Color32 {
        mix(self.body, self.of(kind), 0.35)
    }
}

pub fn map_colors(theme: Theme) -> MapColors {
    let common = MapColors {
        primary: ACCENT,
        logical: Color32::from_rgb(0x2C, 0xB5, 0xC8),
        unallocated: Color32::from_rgb(0xA0, 0xA0, 0xA0),
        system: Color32::from_rgb(0x1E, 0x4E, 0x8C),
        recovery: Color32::from_rgb(0x4B, 0x5A, 0xA8),
        selected: SELECTED,
        body: Color32::WHITE,
        border: Color32::from_rgb(0xB4, 0xB9, 0xC2),
        text: Color32::from_rgb(0x20, 0x20, 0x20),
        label_block: Color32::from_rgb(0xE4, 0xEA, 0xF3),
    };
    match theme {
        Theme::Light => common,
        Theme::Dark => MapColors {
            unallocated: Color32::from_rgb(0x70, 0x70, 0x70),
            body: Color32::from_rgb(0x2B, 0x2D, 0x31),
            border: Color32::from_rgb(0x50, 0x54, 0x5C),
            text: Color32::from_rgb(0xE6, 0xE6, 0xE6),
            label_block: Color32::from_rgb(0x34, 0x3A, 0x45),
            ..common
        },
    }
}

/// Linear blend of `a` towards `b` by `t` (0 = `a`, 1 = `b`), opaque.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(ch(a.r(), b.r()), ch(a.g(), b.g()), ch(a.b(), b.b()))
}

pub fn visuals(theme: Theme) -> Visuals {
    match theme {
        Theme::Light => {
            let mut v = Visuals::light();
            v.panel_fill = Color32::from_rgb(0xF0, 0xF2, 0xF5);
            v.window_fill = Color32::from_rgb(0xFA, 0xFB, 0xFC);
            v.extreme_bg_color = Color32::WHITE;
            v.faint_bg_color = Color32::from_rgb(0xE8, 0xEE, 0xF6);
            v.selection.bg_fill = Color32::from_rgb(0xC6, 0xDB, 0xF5);
            v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
            v.hyperlink_color = ACCENT;
            v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0xDD, 0xE8, 0xF6);
            v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
            v.window_rounding = Rounding::same(4.0);
            v
        }
        Theme::Dark => {
            let mut v = Visuals::dark();
            v.selection.bg_fill = Color32::from_rgb(0x2D, 0x4F, 0x7C);
            v.selection.stroke = Stroke::new(1.0_f32, Color32::from_rgb(0x8C, 0xB4, 0xE6));
            v.hyperlink_color = Color32::from_rgb(0x8C, 0xB4, 0xE6);
            v.window_rounding = Rounding::same(4.0);
            v
        }
    }
}

pub fn apply(ctx: &egui::Context, theme: Theme) {
    ctx.set_visuals(visuals(theme));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_endpoints_and_midpoint() {
        let a = Color32::from_rgb(0, 100, 200);
        let b = Color32::from_rgb(200, 100, 0);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, 0.5), Color32::from_rgb(100, 100, 100));
        assert_eq!(mix(a, b, 7.0), b, "clamped");
    }

    #[test]
    fn kinds_have_distinct_colors_in_both_themes() {
        for t in [Theme::Light, Theme::Dark] {
            let c = map_colors(t);
            let kinds = [ItemKind::Primary, ItemKind::Logical, ItemKind::Unallocated, ItemKind::System, ItemKind::Recovery];
            let colors: std::collections::HashSet<_> = kinds.iter().map(|k| c.of(*k)).collect();
            assert_eq!(colors.len(), kinds.len());
            assert_ne!(c.body, c.text);
        }
        assert_eq!(Theme::Light.toggled(), Theme::Dark);
        assert!(!visuals(Theme::Light).dark_mode && visuals(Theme::Dark).dark_mode);
    }
}
