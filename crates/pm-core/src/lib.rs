//! Partition Manager core library.

pub mod device;
pub mod error;
pub mod progress;
pub mod units;

pub use device::{copy_range, BlockDevice, FileDevice, MemDevice, ReadOnly, SliceDevice};
pub use error::{Error, Result};
pub use progress::{CancelToken, FnProgress, NullProgress, ProgressReport, ProgressSink, ProgressTracker};
pub use units::{align_down, align_up, format_size, parse_size, GIB, MIB};
