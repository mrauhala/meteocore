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
//! - `correction` (with `rasters` and `inventory`) applies the terrain's
//!   lapse-rate correction, the land/sea mask and station exclusions
//!   (#1206).
//! - [`engine`] is the collection: time axis, content version, poll loop,
//!   `MapEngine` and `EdrEngine`.
//! - [`residuals`] is the leave-one-out residual of each station (#1206),
//!   computed with each grid; `features` serves them as the collection's
//!   `FeatureEngine`.

mod compute;
pub mod correction;
pub mod engine;
mod features;
pub mod geometry;
pub mod interpolate;
mod inventory;
mod kdtree;
mod rasters;
pub mod residuals;
mod source;

pub use compute::{budget_metrics, cache_metrics, pool_workers, AnalysisGrid};
pub use correction::Auxiliary;
pub use engine::StationGridEngine;
pub use inventory::HEIGHT_PROPERTY;
