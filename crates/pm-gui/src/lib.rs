//! Partition Manager desktop application: an eframe/egui shell over `pm-ops`.
//!
//! The pure parts (row formatting, disk-map layout, action enablement, the job system) live
//! in their own modules and are unit-tested; [`app::AppState`] holds the state and applies
//! [`app::Cmd`]s, while `views` and `dialogs` only draw and emit commands.

#![forbid(unsafe_code)]

pub mod actions;
pub mod app;
pub mod args;
pub mod dialogs;
pub mod format_util;
pub mod items;
pub mod jobs;
pub mod logging;
pub mod main_window;
pub mod selection;
pub mod theme;
pub mod tools;
pub mod views;
pub mod wizard;
pub mod wizards;

#[cfg(test)]
mod test_util;
