//! Partition Manager operations: the in-memory [`Model`] of disks and volumes, the
//! [`Operation`] list queued by the UI, the [`Planner`] that validates and simulates
//! operations in memory, and the disk clone / OS migration sizing planner.
//!
//! The [`OpQueue`] holds the pending operations and their preview; [`execute`] applies them
//! to the disks, only ever through [`pm_platform::Platform`] and its write sessions.

pub mod executor;
pub mod model;
pub mod operation;
pub mod planner;
pub mod queue;
pub mod sizing;
mod steps;

pub use model::{table_from_disk, Model};
pub use operation::{BootFix, CloneMode, Operation, Sizing, WipeMethod};
pub use planner::Planner;
pub use sizing::{plan_disk_clone, windows_partition, TargetLayout};
pub use executor::{execute, ExecOpts, ExecReport, StepEvent};
pub use queue::OpQueue;
