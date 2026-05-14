//! Minimal PROJ-string parser for ODIM `/where/projdef` values.
//!
//! ODIM_H5 stores the grid projection as a PROJ.4 string (e.g.
//! `+proj=stere +lat_0=90 +lon_0=0 +lat_ts=60 +R=6371228 +x_0=0 +y_0=0`).
//! This module hand-parses the four projections we encounter across
//! European weather radar producers — `stere`, `tmerc`, `laea`, and
//! `longlat` — and maps each into a [`ds_core::geo::Crs`] variant.
//!
//! Out of scope (Phase 1 — see [[project_odim_engine_plan]] for the
//! consolidated plan):
//! - Arbitrary PROJ.4 strings beyond the four supported `+proj=` values
//! - Datum-shift parameters (`+towgs84`, `+nadgrids`, …) — ODIM v2.x
//!   uses sphere-based projections so these don't appear in COMP files
//! - WKT / PROJ-JSON
//!
//! When the input contains an unsupported `+proj=` or a malformed
//! parameter, return an error rather than silently falling back — a
//! quiet projection mismatch would surface as wrong pixel-to-world
//! mappings far downstream.

use ds_core::geo::Crs;

/// Errors from [`parse`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("PROJ string is missing the required `+proj=` token")]
    MissingProj,
    #[error("unsupported `+proj={0}` (Phase 1 supports stere/tmerc/laea/longlat)")]
    UnsupportedProj(String),
    #[error("PROJ parameter `{param}={value}` is not a valid number")]
    InvalidNumber { param: String, value: String },
}

/// Parse a PROJ.4 string from an ODIM `/where/projdef` value into a
/// [`Crs`]. The full implementation lands in the next commit alongside
/// the reader; this stub locks the module signature in place so the
/// rest of the crate can compile against it.
pub fn parse(_projdef: &str) -> Result<Crs, ParseError> {
    Err(ParseError::MissingProj)
}
