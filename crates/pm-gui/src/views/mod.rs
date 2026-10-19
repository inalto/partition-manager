//! Panels of the main window. Each `show` function reads the [`crate::app::AppState`] and
//! pushes [`crate::app::Cmd`]s; none of them mutates the state directly.

pub mod context_menu;
pub mod disk_map;
pub mod disk_table;
pub mod legend;
pub mod pending;
pub mod sidebar;
pub mod toolbar;

/// Icons used in button captions; `main_window` tests that the bundled fonts have them.
pub mod icons {
    pub const APPLY: &str = "✔";
    pub const UNDO: &str = "⟲";
    pub const DISCARD: &str = "✖";
    pub const REFRESH: &str = "⟳";
    pub const SETTINGS: &str = "⚙";
    pub const WARNING: &str = "⚠";
    pub const ALL: [&str; 6] = [APPLY, UNDO, DISCARD, REFRESH, SETTINGS, WARNING];
}
