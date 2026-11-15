//! Self-update for Partition Manager: release discovery, download,
//! verification and install. GUI-free.
#![forbid(unsafe_code)]

mod release;
mod version;

#[cfg(test)]
pub(crate) mod samples;

pub use release::{parse_release, url_allowed, Asset, Release};
pub use version::{Version, TARGET};
