//! Partition Manager operations: the in-memory [`Model`] of disks and volumes, the
//! [`Operation`] list queued by the UI, the [`Planner`] that validates and simulates
//! operations in memory, and the disk clone / OS migration sizing planner.
//!
//! Nothing in this crate touches a device: the executor (built on top of these types)
//! applies the operations.

pub mod model;
pub mod operation;
pub mod planner;
pub mod sizing;

pub use model::{table_from_disk, Model};
pub use operation::{BootFix, CloneMode, Operation, Sizing, WipeMethod};
pub use planner::Planner;
pub use sizing::{plan_disk_clone, windows_partition, TargetLayout};
