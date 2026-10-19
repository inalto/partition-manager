//! Recovery on top of `pm-parttable` and `pm-fs`: lost-partition finder, deleted-file recovery,
//! signature-based file carving and saveable scan sessions.
//!
//! Everything here is read-only with respect to the scanned device. All offsets and sizes are
//! bytes.

pub mod carve;
pub mod files;
pub mod partitions;
pub mod session;

pub use carve::{
    carve, extract_carved, signatures, CarveReport, CarvedFile, Category, LengthRule, Signature,
};
pub use files::{deleted_files, deleted_files_at, restore_file, DeletedFile};
pub use partitions::{apply_candidates, find_partitions};
pub use session::{DeviceIdentity, ScanSession, SessionKind};
