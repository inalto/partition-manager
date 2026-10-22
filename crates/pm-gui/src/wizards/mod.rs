//! The partition-operation wizards, each a [`crate::wizard::Wizard`] over one state struct.
//!
//! Every module has the same shape: a `…State` struct with a pure `build` (state →
//! operations, unit-tested without egui), an `initial` constructor from the
//! [`WizardCtx`], step functions that draw and validate the state, and a `new` returning the
//! wizard (`None` when the selection does not fit it). [`for_action`] maps sidebar actions to
//! them.

pub mod convert;
pub mod copy_disk;
pub mod copy_partition;
pub mod create;
pub mod data_recovery;
pub mod delete;
pub mod format;
pub mod image_create;
pub mod image_restore;
pub mod initialize;
pub mod label_letter;
pub mod migrate_os;
pub mod mount;
pub mod partition_recovery;
pub mod resize;
pub mod resize_bar;
pub mod rescue;
pub mod wipe;

use crate::actions::Action;
use crate::format_util::{fs_text, partition_name};
use crate::items::ItemKind;
use crate::selection::Selected;
use crate::theme::{map_colors, Theme};
use crate::views::disk_map::segments;
use crate::wizard::{Wizard, WizardCtx};
use eframe::egui::{self, pos2, vec2, Align2, FontId, Rect, Sense, Stroke};
use pm_core::{format_size, parse_size, DiskInfo, FsKind, PartitionInfo, TableKind, GIB, MIB};

/// The actions [`for_action`] has a wizard for.
pub const ACTIONS: [Action; 13] = [
    Action::CreatePartition,
    Action::FormatPartition,
    Action::ResizeMove,
    Action::DeletePartition,
    Action::ChangeLabel,
    Action::ChangeLetter,
    Action::WipeDisk,
    Action::WipePartition,
    Action::ConvertMbrToGpt,
    Action::ConvertGptToMbr,
    Action::InitializeDisk,
    Action::Mount,
    Action::Unmount,
];

/// The wizard for `action` on `ctx.selection`; `None` when the action has no wizard here (or
/// the selection does not fit it).
pub fn for_action(action: Action, ctx: &WizardCtx) -> Option<Wizard> {
    use Action::*;
    match action {
        CreatePartition => create::new(ctx),
        FormatPartition => format::new(ctx),
        ResizeMove => resize::new(ctx),
        DeletePartition => delete::new(ctx),
        ChangeLabel => label_letter::new(ctx, label_letter::Mode::Label),
        ChangeLetter => label_letter::new(ctx, label_letter::Mode::Letter),
        WipeDisk => wipe::new(ctx, false),
        WipePartition => wipe::new(ctx, true),
        ConvertMbrToGpt => convert::new(ctx, TableKind::Gpt),
        ConvertGptToMbr => convert::new(ctx, TableKind::Mbr),
        InitializeDisk => initialize::new(ctx),
        Mount => mount::new(ctx),
        Unmount => mount::new_unmount(ctx),
        _ => None,
    }
}

/// The disk-copy wizards for the sidebar actions [`CLONE_ACTIONS`] (queued like the others).
pub fn clone_for_action(action: Action, ctx: &WizardCtx) -> Option<Wizard> {
    match action {
        Action::MigrateOs => migrate_os::new(ctx),
        Action::CopyDisk => copy_disk::new(ctx),
        Action::CopyPartition => copy_partition::new(ctx),
        _ => None,
    }
}

/// The actions [`clone_for_action`] has a wizard for.
pub const CLONE_ACTIONS: [Action; 3] = [Action::MigrateOs, Action::CopyDisk, Action::CopyPartition];

/// Largest volume Windows formats as FAT32.
pub const FAT32_MAX: u64 = 32 * GIB;

/// Longest volume label `fs` accepts: 11 characters for the FAT family and exFAT, 32 otherwise.
pub fn max_label_len(fs: Option<FsKind>) -> usize {
    match fs {
        Some(FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::ExFat) => 11,
        _ => 32,
    }
}

/// Why `label` cannot be used on `fs`, if it cannot.
pub fn label_problem(fs: Option<FsKind>, label: &str) -> Option<String> {
    let max = max_label_len(fs);
    (label.chars().count() > max).then(|| format!("The label is longer than {max} characters."))
}

/// A size typed by the user: a bare number is MB, otherwise any [`parse_size`] form
/// (`"1.5 GB"`, `"700M"`).
pub fn parse_size_mb(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if t.chars().all(|c| c.is_ascii_digit() || c == '.') {
        let v: f64 = t.parse().ok()?;
        return (v.is_finite() && v >= 0.0 && v < 1e12).then(|| (v * MIB as f64).round() as u64);
    }
    parse_size(t).ok()
}

/// `"Partition 3 (E:Data, NTFS, 684.00 MB) on Disk 1"`.
pub fn partition_desc(disk: u32, p: &PartitionInfo) -> String {
    format!("Partition {} ({}, {}, {}) on Disk {disk}", p.number, partition_name(p), fs_text(p), format_size(p.len))
}

/// `bytes` as MB with enough decimals to round-trip through [`parse_size_mb`] to the byte.
pub fn precise_mb(bytes: u64) -> String {
    let s = format!("{:.7}", bytes as f64 / MIB as f64);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Text of a size field. Unfocused it shows the value rounded ([`format_size`]); on focus
/// it switches to the full-precision MB value, and a value is applied only when the user
/// actually changed the text, so tabbing through a field never moves anything.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SizeText {
    pub text: String,
    /// The text when the field gained focus (`None` while unfocused).
    at_focus: Option<String>,
}

impl SizeText {
    /// One frame of the field showing `value`, given its focus events; returns the new value
    /// when the user leaves the field after changing it.
    pub fn update(&mut self, gained: bool, lost: bool, focused: bool, value: u64) -> Option<u64> {
        let mut out = None;
        if lost {
            if let Some(orig) = self.at_focus.take() {
                if orig != self.text {
                    out = parse_size_mb(&self.text);
                }
            }
        }
        if gained {
            self.text = precise_mb(value);
            self.at_focus = Some(self.text.clone());
        } else if !focused {
            self.text = format_size(value);
            self.at_focus = None;
        }
        out
    }
}

/// A size text field over `st` (see [`SizeText`]); returns the value the user entered, once,
/// when they leave the field (Enter or click elsewhere).
pub fn size_edit(ui: &mut egui::Ui, st: &mut SizeText, value: u64) -> Option<u64> {
    let resp = ui.add(egui::TextEdit::singleline(&mut st.text).desired_width(110.0));
    st.update(resp.gained_focus(), resp.lost_focus(), resp.has_focus(), value)
}

/// Why `fs` cannot be created on `os` (`Platform::os_name`), if it cannot: NTFS needs
/// third-party tools on macOS, ReFS exists on Windows only.
pub fn fs_unavailable(os: &str, fs: FsKind) -> Option<&'static str> {
    match (os, fs) {
        ("macos", FsKind::Ntfs) => Some("NTFS formatting needs third-party tools on macOS"),
        (os, FsKind::ReFs) if os != "windows" => Some("ReFS volumes can only be created on Windows"),
        _ => None,
    }
}

/// Combo box of `letters` plus "None".
pub fn letter_combo(ui: &mut egui::Ui, id: &str, choice: &mut Option<char>, letters: &[char]) {
    let text = choice.map_or("None".to_string(), |c| format!("{c}:"));
    egui::ComboBox::from_id_salt(id).selected_text(text).show_ui(ui, |ui| {
        ui.selectable_value(choice, None, "None");
        for c in letters {
            ui.selectable_value(choice, Some(*c), format!("{c}:"));
        }
    });
}

/// A compact copy of the disk-map bar for `disk` ([`segments`] layout and colors), with
/// `highlight` outlined.
pub fn disk_preview(ui: &mut egui::Ui, disk: &DiskInfo, highlight: Option<Selected>) {
    let theme = if ui.visuals().dark_mode { Theme::Dark } else { Theme::Light };
    let c = map_colors(theme);
    let width = ui.available_width().max(120.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 38.0), Sense::hover());
    let painter = ui.painter_at(rect);
    for seg in segments(disk, width) {
        let body = Rect::from_min_size(pos2(rect.min.x + seg.x, rect.min.y), vec2(seg.w, rect.height())).shrink2(vec2(1.0, 0.0));
        let unalloc = seg.kind == ItemKind::Unallocated;
        painter.rect_filled(body, 2.0, if unalloc { c.label_block } else { c.body });
        if seg.used_frac > 0.0 {
            let used = Rect::from_min_max(
                pos2(body.min.x, body.min.y + 5.0),
                pos2(body.min.x + body.width() * seg.used_frac, body.max.y),
            );
            painter.rect_filled(used, 0.0, c.used_fill(seg.kind));
        }
        painter.rect_filled(Rect::from_min_size(body.min, vec2(body.width(), 5.0)), 2.0, c.of(seg.kind));
        painter.rect_stroke(body, 2.0, Stroke::new(1.0_f32, c.border));
        let label = seg.label.replace('\n', " ");
        painter.with_clip_rect(body.shrink(2.0)).text(
            body.min + vec2(3.0, 8.0),
            Align2::LEFT_TOP,
            label,
            FontId::proportional(11.0),
            c.text,
        );
        if Some(seg.sel) == highlight {
            painter.rect_stroke(body.shrink(1.0), 2.0, Stroke::new(2.0_f32, c.selected));
        }
    }
}

/// A red warning line.
pub fn danger(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.colored_label(crate::theme::DANGER, format!("{} {}", crate::views::icons::WARNING, text.into()));
}

/// `(key, value)` row helper for summaries.
pub fn row(k: &str, v: impl Into<String>) -> (String, String) {
    (k.to_string(), v.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{gpt_disk, mbr_disk, model_of};

    #[test]
    fn sizes_typed_in_mb_or_with_units() {
        assert_eq!(parse_size_mb("100"), Some(100 * MIB));
        assert_eq!(parse_size_mb(" 1.5 "), Some(MIB + MIB / 2));
        assert_eq!(parse_size_mb("2 GB"), Some(2 * GIB));
        assert_eq!(parse_size_mb("700M"), Some(700 * MIB));
        assert_eq!(parse_size_mb(""), None);
        assert_eq!(parse_size_mb("abc"), None);
        assert_eq!(parse_size_mb("1.2.3"), None);
    }

    #[test]
    fn size_field_applies_only_user_changes() {
        let value = 1_234_567_891; // 1.15 GB: not a round number of MB
        let mut st = SizeText::default();
        assert_eq!(st.update(false, false, false, value), None);
        assert_eq!(st.text, format_size(value));
        // tabbing through: focus, then leave without typing → nothing applied
        assert_eq!(st.update(true, false, true, value), None);
        assert_eq!(st.text, precise_mb(value), "full precision while editing");
        assert_eq!(st.update(false, true, false, value), None);
        // retyping the very same text applies nothing either
        st.update(true, false, true, value);
        st.text = precise_mb(value);
        assert_eq!(st.update(false, true, false, value), None);
        assert_eq!(st.text, format_size(value));
        // the precise text round-trips to the byte
        assert_eq!(parse_size_mb(&precise_mb(value)), Some(value));
        // a real edit is applied once
        st.update(true, false, true, value);
        st.text = "500".into();
        assert_eq!(st.update(false, false, true, value), None, "not while still typing");
        assert_eq!(st.update(false, true, false, value), Some(500 * MIB));
        assert_eq!(st.update(false, true, false, value), None, "only once");
    }

    #[test]
    fn label_limits() {
        assert_eq!(max_label_len(Some(FsKind::Fat32)), 11);
        assert!(fs_unavailable("macos", FsKind::Ntfs).is_some());
        assert!(fs_unavailable("linux", FsKind::Ntfs).is_none() && fs_unavailable("windows", FsKind::Ntfs).is_none());
        assert!(fs_unavailable("linux", FsKind::ReFs).is_some() && fs_unavailable("windows", FsKind::ReFs).is_none());
        assert!(fs_unavailable("macos", FsKind::ExFat).is_none());
        assert_eq!(max_label_len(Some(FsKind::ExFat)), 11);
        assert_eq!(max_label_len(Some(FsKind::Ntfs)), 32);
        assert!(label_problem(Some(FsKind::Fat32), "TWELVE_CHARS").is_some());
        assert!(label_problem(Some(FsKind::Ntfs), "TWELVE_CHARS").is_none());
    }

    /// Every action with a wizard opens one on a fitting selection, and every wizard page
    /// renders headless without panicking.
    #[test]
    fn wizards_open_and_render_every_page() {
        let mut free_disk = gpt_disk(2);
        free_disk.partitions.clear();
        free_disk.table = TableKind::None;
        let mut m = model_of(vec![gpt_disk(1), mbr_disk(0), free_disk]);
        m.system_disk = Some(9);
        let free = crate::items::disk_items(m.disk(1).unwrap()).last().unwrap().sel;
        let cases = [
            (Action::CreatePartition, free),
            (Action::FormatPartition, Selected::Partition(1, 3)),
            (Action::ResizeMove, Selected::Partition(1, 3)),
            (Action::DeletePartition, Selected::Partition(1, 3)),
            (Action::ChangeLabel, Selected::Partition(1, 3)),
            (Action::ChangeLetter, Selected::Partition(1, 3)),
            (Action::WipeDisk, Selected::Disk(1)),
            (Action::WipePartition, Selected::Partition(0, 5)),
            (Action::ConvertMbrToGpt, Selected::Disk(0)),
            (Action::ConvertGptToMbr, Selected::Disk(1)),
            (Action::InitializeDisk, Selected::Disk(2)),
            (Action::Mount, Selected::Partition(1, 3)),
            (Action::Unmount, Selected::Partition(1, 3)),
        ];
        assert_eq!(cases.map(|c| c.0), ACTIONS);
        let ctx = egui::Context::default();
        for (action, sel) in cases {
            let wctx = WizardCtx::new(&m, vec!['F', 'G'], sel);
            let mut w = for_action(action, &wctx).unwrap_or_else(|| panic!("{action:?} has no wizard"));
            for page in 0..=w.step_count() {
                w.goto(page);
                for dark in [false, true] {
                    let _ = ctx.run(egui::RawInput::default(), |ctx| {
                        ctx.set_visuals(if dark { egui::Visuals::dark() } else { egui::Visuals::light() });
                        w.show(ctx, &wctx);
                    });
                }
            }
        }
        // actions without a wizard here, and selections that do not fit
        let wctx = WizardCtx::new(&m, vec![], Selected::Disk(1));
        assert!(for_action(Action::CopyDisk, &wctx).is_none());
        assert!(for_action(Action::FormatPartition, &wctx).is_none());
    }
}
