//! Read-only NTFS: boot sector, MFT records with fixups, attributes and attribute lists,
//! data runs, `$I30` index B-trees, the `$Bitmap` used map and deleted-record scanning.

pub mod boot;
pub mod fs;
pub mod index;
pub mod mft;
pub mod runs;

pub use fs::{open_ntfs, Ntfs};
pub use runs::decode_runs;
