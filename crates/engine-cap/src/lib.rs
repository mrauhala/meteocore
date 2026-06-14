//! `engine-cap` — an OASIS Common Alerting Protocol (CAP) v1.2 engine.
//!
//! Ingests emergency/weather alerts from a local directory of CAP `.xml` files
//! or an Atom/RSS feed, and exposes each alert **area** as:
//!
//! - **OGC API – Features** — one GeoJSON Feature per `(alert, info, area)`,
//!   with the alert/info metadata as properties; filterable by bbox, datetime
//!   (active-at-time), and limit/offset.
//! - **WMS 1.3.0 / OGC API – Maps / Tiles** — alert zones rendered as a
//!   severity-shaded polygon fill (the `cap_severity` colormap), with the
//!   standard CRS/TIME machinery.
//!
//! The single engine implements both `FeatureEngine` and `MapEngine` over one
//! poll-and-swap [`catalog::Catalog`]. The map fill reuses the shared
//! `ds_render::rasterize::fill_polygon` primitive (#397) fed by
//! `ds_core::geo::geometry_to_pixels` — projecting polygon *vertices*, never per
//! output pixel.
//!
//! See the "CAP Engine Notes" section of `CLAUDE.md` for the full design.

mod catalog;
mod engine;
mod parser;
mod source;

pub use catalog::severity_code;
pub use engine::{CapEngine, CAP_PARAMETER};
