//! Geometry simplification hooks.
//!
//! Douglas-Peucker / Visvalingam-Whyatt are intentionally not implemented in v1.
//! The plan calls simplification optional; the trade-off (extra dep on `geo`,
//! moderate CPU at encode time) is deferred until a deployment shows it's needed.
//!
//! Polygons/MultiPolygons currently flow through unchanged; the projection step
//! in [`super::encode`] handles the only mandatory transform (lon/lat → tile pixels).

#[allow(dead_code)] // Reserved for future zoom-derived tolerance.
pub(crate) fn tolerance_for_zoom(_zoom: u32) -> f64 {
    0.0
}
