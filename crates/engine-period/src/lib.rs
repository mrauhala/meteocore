//! Period products (#1379): a derived collection of rolling-window
//! statistics over another collection's frames, served through WMS, Maps,
//! Tiles and EDR. Radar rain accumulation first: reflectivity converted to
//! rain rate with a Z–R relation, times the time each frame counts for,
//! summed over the last hour, three hours, a day.
//!
//! - [`zr`], [`plan`], [`accum`] and [`geometry`] are dependency-free pure
//!   functions: the Z–R relation, which frames a window holds and what it
//!   counts them for, the running grids, and the working grid and its
//!   sampling.
//! - [`rolling`] keeps every window ending at the source's latest frame,
//!   one poll at a time, within a read budget.
//! - [`engine`] is the collection: config validation, the poll loop, the
//!   snapshot, `MapEngine` and `EdrEngine`.

pub mod accum;
pub mod engine;
pub mod geometry;
pub mod plan;
pub mod rolling;
pub mod zr;

pub use engine::PeriodEngine;
