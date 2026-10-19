//! Read-only exFAT: boot sector, FAT chains and contiguous (NoFatChain) streams, directory
//! entry sets (0x85 / 0xC0 / 0xC1), the allocation bitmap as the [`crate::UsedMap`] and
//! deleted-entry-set scanning.
//!
//! # Entry ids
//!
//! [`crate::Entry::id`] packs everything needed to find the data of a file or directory
//! without re-reading its directory entry:
//!
//! - bits 0..32: first cluster,
//! - bit 32: the stream's `NoFatChain` flag (the clusters are contiguous and the FAT holds
//!   no chain for them),
//! - bits 33..64: for `NoFatChain` streams, the number of clusters
//!   (`ceil(data_length / cluster_size)`); 0 otherwise.
//!
//! The root directory is always a FAT chain, so its id is simply the first cluster.
//!
//! # ValidDataLength
//!
//! Bytes at or beyond a file's `ValidDataLength` read as zeros. `Entry` has no field for it,
//! so the reader remembers it per `(first cluster, deleted)` for the entries returned by
//! `list` and `deleted_entries` of the same [`ExFat`] instance; an `Entry` built by hand reads
//! up to its `size`.
//!
//! # Deleted entries
//!
//! A deleted entry set has bit 7 of its primary entry type cleared (`0x05`). Deleting clears
//! the FAT chain of a fragmented file, so recovery of a deleted file is best effort: when
//! the first FAT entry of a deleted, chained stream is free it is read as one contiguous run.

pub mod boot;
pub mod dir;
pub mod fs;

pub use fs::ExFat;
