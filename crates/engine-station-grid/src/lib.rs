//! Station grid (#1082): a derived collection that interpolates one time
//! slice of a station collection's observations onto a regular CRS84
//! analysis grid, on the fly, and serves it through WMS, Maps, Tiles and
//! EDR.
//!
//! - [`geometry`], [`interpolate`] and the k-d tree are dependency-free pure
//!   functions: the grid, the methods (IDW, ADW, nearest) and the neighbour
//!   search on the sphere.
//! - `source` reads a slice from the station collection's `EdrEngine`.
//! - `compute` holds the grid cache, the admission budget and the pool
//!   grids are computed on.
//! - [`engine`] is the collection: time axis, content version, poll loop,
//!   `MapEngine` and `EdrEngine`.

mod compute;
pub mod engine;
pub mod geometry;
pub mod interpolate;
mod kdtree;
mod source;

pub use compute::{budget_metrics, cache_metrics, pool_workers, AnalysisGrid};
pub use engine::StationGridEngine;
