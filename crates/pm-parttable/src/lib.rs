//! Partition tables over [`pm_core::BlockDevice`]: the table model, MBR and GPT parse/write,
//! MBR<->GPT conversion, alignment and lost-partition search.
//!
//! All public sizes and offsets are in bytes; sectors only appear inside the parsers.

pub mod bytes;
pub mod convert;
pub mod gpt;
pub mod mbr;
pub mod search;
pub mod table;

pub use convert::{align_table, convert_type, gpt_to_mbr, mbr_to_gpt};
pub use gpt::{
    gpt_health, gpt_repair_backup_from_primary, gpt_repair_primary_from_backup, parse_gpt, write_gpt,
    GptHealth,
};
pub use mbr::{chs_from_lba, is_protective_mbr, parse_mbr, write_mbr};
pub use search::{search_lost, Candidate, CandidateSource};
pub use table::{read_table, write_table, Entry, Table};
