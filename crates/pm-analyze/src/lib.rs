//! Partition Manager analysis tools: space analyzer, surface test, disk benchmark and SMART
//! summary. Everything reports progress and honours a [`pm_core::CancelToken`].

pub mod bench;
pub mod smart;
pub mod space;
pub mod surface;

pub use bench::{benchmark, BenchMode, BenchOpts, BenchPoint, BenchTarget};
pub use smart::{smart_summary, Health};
pub use space::{scan_space, Node, SpaceReport};
pub use surface::{surface_test, SurfaceReport, DEFAULT_BLOCK};
