//! Partition Manager core library: block devices, progress/cancellation, size units and
//! the disk/partition domain model.

pub mod device;
pub mod error;
pub mod fs_kind;
pub mod model;
pub mod progress;
pub mod ptype;
pub mod units;

pub use device::{copy_range, BlockDevice, FileDevice, MemDevice, ReadOnly, SliceDevice};
pub use error::{Error, Result};
pub use fs_kind::FsKind;
pub use model::*;
pub use progress::{CancelToken, FnProgress, NullProgress, ProgressReport, ProgressSink, ProgressTracker};
pub use ptype::*;
pub use units::{align_down, align_up, format_size, parse_size, GIB, MIB};
