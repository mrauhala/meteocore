//! Radar nowcasting: motion estimation + semi-Lagrangian extrapolation, and
//! the derived-collection engine serving the result (epic #519).
//!
//! The algorithm modules stay dependency-free pure functions: [`motion`]
//! estimates a block-level motion field from two consecutive composite
//! frames, [`advect`] extrapolates the latest frame along that field, and
//! [`skill`] scores a hindcast against what actually happened (the phase-0
//! gate: extrapolation must beat persistence, or the motion estimator is
//! wrong — see `examples/skill_spike.rs`).
//!
//! [`engine::NowcastEngine`] (phase 1, #522) wraps another collection's
//! `MapEngine` and turns generations of extrapolated frames into an ordinary
//! raster collection with TIME values in the future.
//!
//! Conventions shared by every module:
//! - A [`Grid`] is a row-major raster, row 0 at the top, `f32` physical values
//!   (dBZ for radar), `NaN` = nodata.
//! - Motion vectors are in **pixels per frame interval**, `+u` = rightward
//!   (+x), `+v` = downward (+y, image convention).
//! - Lead times are in frame intervals; the engine maps intervals to
//!   timestamps.

pub mod advect;
pub mod engine;
pub mod motion;
pub mod objects;
pub mod skill;

pub use engine::NowcastEngine;

/// A row-major 2-D raster of physical values; `NaN` = nodata.
#[derive(Debug, Clone)]
pub struct Grid {
    pub width: usize,
    pub height: usize,
    /// `width * height` values, row-major, row 0 = top.
    pub data: Vec<f32>,
}

impl Grid {
    /// Build a grid, checking the buffer length.
    pub fn new(width: usize, height: usize, data: Vec<f32>) -> Self {
        assert_eq!(
            data.len(),
            width * height,
            "Grid buffer length must be width*height"
        );
        Self {
            width,
            height,
            data,
        }
    }

    /// A grid filled with nodata.
    pub fn filled_nodata(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![f32::NAN; width * height],
        }
    }

    /// Value at (x, y); out-of-bounds returns `NaN`.
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> f32 {
        if x < self.width && y < self.height {
            self.data[y * self.width + x]
        } else {
            f32::NAN
        }
    }
}
