//! Self-update for Partition Manager: release discovery, download,
//! verification and install. GUI-free.
#![forbid(unsafe_code)]

mod download;
mod fetch;
mod release;
mod unpack;
mod version;

#[cfg(test)]
pub(crate) mod samples;

pub use download::{download, sha256_hex, sum_for};
pub use fetch::{check, read_limited, Check, Fetch, HttpFetch, LATEST_URL};
pub use release::{parse_release, url_allowed, Asset, Release};
pub use unpack::{exe_name, unpack, DOCUMENTS, EXECUTABLES};
pub use version::{Version, TARGET};
