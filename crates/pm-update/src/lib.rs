//! Self-update for Partition Manager: release discovery, download,
//! verification and install. GUI-free.
#![forbid(unsafe_code)]

mod download;
mod fetch;
mod install;
mod release;
mod unpack;
mod version;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use download::{download, sha256_hex, sum_for};
pub use fetch::{check, read_limited, Check, Fetch, HttpFetch, LATEST_URL};
pub use install::{clean_up, install_dir, install_kind, restart, rollback_failed, swap, update, InstallKind, STAGING};
pub use release::{parse_release, release_page, url_allowed, Asset, Release, DOWNLOAD_PREFIX, RELEASES_PAGE};
pub use unpack::{exe_name, unpack, DOCUMENTS, EXECUTABLES};
pub use version::{Version, TARGET};
