//! Native ODIM_H5 weather-radar engine.
//!
//! Reads ODIM `COMP` (composite) reflectivity files from FMI / DMI /
//! SMHI / OPERA via pure-Rust HDF5 parsing — no `gdalwarp`
//! preprocessing, no `libhdf5` system dependency. Implements
//! `MapEngine` (raster output for WMS/Maps/Tiles) and `EdrEngine`
//! (position + area queries) against the same 2D grid the COMP file
//! carries.
//!
//! Phase 1 scope (issue #29 + 2026-05 ODIM-PVOL discussion):
//! - Single-dataset COMP files only
//! - Local directory + S3 sources (DMI STAC and PVOL trajectory
//!   cross-sections land in phases 2/3)
//! - Stereographic / TM / LAEA / longlat projections via the minimal
//!   PROJ-string parser in `proj.rs`
//!
//! See [[project_odim_engine_plan]] for the consolidated multi-phase
//! plan and [[exploration_stac_integration_points]] for the STAC
//! integration map that Phase 2 will draw on.

pub mod catalog;
pub mod edr;
pub mod engine;
pub mod proj;
pub mod pvol;
pub mod reader;
pub mod volume_engine;

pub use engine::{EngineError, OdimEngine};
pub use volume_engine::{PolarVolumeEngine, PolarVolumeSiteView};
