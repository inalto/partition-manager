//! Read-only ext2/ext3/ext4: superblock and group descriptors (32- and 64-byte), inodes with
//! extent trees or classic indirect block maps, linear directory reads (htree directories are
//! read linearly too), block bitmaps as the [`crate::UsedMap`] and deleted-inode scanning.
//!
//! # Deleted entries
//!
//! ext keeps no names with an inode: unlinking removes the directory entry and the inode only
//! keeps `i_dtime`. [`FileSystem::deleted_entries`](crate::FileSystem::deleted_entries)
//! therefore lists every inode with `i_dtime != 0`, `i_links_count == 0` and a non-zero mode
//! as `inode-N` (path `/inode-N`, id `N`). The kernel also truncates the data mapping on
//! delete, so the size is often 0 and the data is usually unrecoverable.
//!
//! # Used map
//!
//! One unit per block. Bits come from the block bitmaps; groups flagged `BLOCK_UNINIT` (with
//! group descriptor checksums enabled) have no valid bitmap and contribute their metadata
//! (and a possible superblock/descriptor backup) instead. The boot block, the superblock, the
//! descriptor table and every group's bitmaps and inode table are marked regardless of the
//! bitmaps. A filesystem whose journal still has to be replayed (`needs_recovery`) may have
//! stale bitmaps, so every block of it is reported used.
//!
//! # Not supported
//!
//! `BIGALLOC`, `META_BG`, compression and external journal devices fail at open with
//! [`pm_core::Error::Unsupported`]; files with inline data or encryption fail on read.

pub mod dir;
pub mod extent;
pub mod fs;
pub mod inode;
pub mod super_block;

pub use fs::Ext;
