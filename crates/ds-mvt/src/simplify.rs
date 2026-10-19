//! Geometry simplification hooks.
//!
//! Vector tiles are not simplified yet: the plan calls it optional, and the
//! encode-time CPU is deferred until a deployment shows it's needed. When it
//! is, use `ds_core::simplify`, the workspace's one Douglas–Peucker (#1135),
//! rather than adding a second one or a dependency on `geo`.
//!
//! Polygons/MultiPolygons currently flow through unchanged; the projection step
//! in [`super::encode`] handles the only mandatory transform (lon/lat → tile pixels).

#[allow(dead_code)] // Reserved for future zoom-derived tolerance.
pub(crate) fn tolerance_for_zoom(_zoom: u32) -> f64 {
    0.0
}
