//! Partition tables over [`pm_core::BlockDevice`]: the table model plus MBR parse/write.
//!
//! All public sizes and offsets are in bytes; sectors only appear inside the parsers.

pub mod bytes;
pub mod mbr;
pub mod table;

pub use mbr::{chs_from_lba, is_protective_mbr, parse_mbr, write_mbr};
pub use table::{read_table, write_table, Entry, Table};
