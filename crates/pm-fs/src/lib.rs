//! Partition Manager filesystems: detection, the read-only [`FileSystem`] trait, the
//! [`UsedMap`] bitmap, a FAT12/16/32 reader and a FAT32 formatter.

pub mod bitmap;
pub mod detect;
pub mod fat;
pub mod traits;
mod util;

pub use bitmap::UsedMap;
pub use detect::{detect, probe_for_search, FsProbe};
pub use fat::format::{format_fat32, FormatOpts};
pub use fat::FatFs;
pub use traits::{open, Entry, FileSystem};
