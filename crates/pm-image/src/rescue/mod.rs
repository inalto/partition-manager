//! GNU-ddrescue-style rescue copier: the single copy path for clones, images, restores and
//! rescues of failing drives.
//!
//! [`rescue`] copies a source device to a destination in phases (copy, trim, scrape,
//! retry), recording the state of every byte in a [`MapFile`] that is interchangeable with
//! GNU ddrescue's mapfile, so an interrupted rescue can be resumed by either tool.
//! [`copy_simple`] is the fast abort-on-first-error path used by plain clones.

mod engine;
#[cfg(any(test, feature = "testing"))]
mod faulty;
mod mapfile;

pub use engine::{copy_simple, rescue, RescueOpts, RescueStats};
#[cfg(any(test, feature = "testing"))]
pub use faulty::FaultyDevice;
pub use mapfile::{BlockStatus, MapFile, MapStats};
pub use pm_fs::UsedMap;
