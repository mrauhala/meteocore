//! Mapbox Vector Tile (MVT) encoder for `FeatureEngine` outputs.
//!
//! Encodes [`ds_core::feature::Feature`] values into MVT byte streams addressable
//! by tile coordinates. Two tile-matrix sets are supported in v1: `WebMercatorQuad`
//! (EPSG:3857) and `WorldCRS84Quad` (CRS:84). Both consume WGS84 lon/lat input.
//!
//! The crate is intentionally a thin layer over [`mvt`]; it owns:
//!
//! * **Projection** of WGS84 feature coords into a tile's local 4096-grid,
//!   honouring whichever tile-matrix set the caller specified.
//! * **Property filtering** through a [`PropertyAllowlist`].
//! * A small per-collection [`VectorTileCache`] keyed by the encoded inputs.
//!
//! Simplification, clipping, and feature-level retries are out of scope for v1.

mod cache;
mod encode;
mod hash;
mod simplify;

pub use cache::{VectorTileCache, VectorTileKey};
pub use encode::{
    encode_tile, properties_hash, EncodeError, PropertyAllowlist, TileEncodeOptions, TmsKind,
};
