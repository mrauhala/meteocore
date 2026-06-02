//! Native ODIM_H5 weather-radar engine.
//!
//! Reads ODIM_H5 radar data with a pure-Rust HDF5 parser — no `gdalwarp`
//! preprocessing, no `libhdf5` system dependency. Two engines share the
//! reader:
//!
//! - `OdimEngine` — 2-D `COMP` composites (FMI / DMI / SMHI / OPERA), one
//!   pre-projected reflectivity grid per timestep. Implements `MapEngine`
//!   (WMS / Maps / Tiles) and `EdrEngine` (position + area). Source: local
//!   directory or S3.
//! - `PolarVolumeEngine` — native polar volumes (PVOL). Owns the scan,
//!   parse cache, and poll loop for a whole radar *network* and
//!   auto-expands into one collection per radar site, each served by a
//!   cheap `PolarVolumeSiteView` (`MapEngine` + `EdrEngine`) whose
//!   parameters are the bare ODIM quantities. EDR covers position,
//!   locations, area, and trajectory vertical cross-sections; the vertical
//!   dimension is elevation angle. Source: local directory or S3.
//!
//! Composite projections (Stereographic / TM / LAEA / longlat) use the
//! minimal PROJ-string parser in `proj.rs`.

pub mod catalog;
pub mod edr;
pub mod engine;
pub mod proj;
pub mod pvol;
pub mod reader;
pub mod volume_engine;

pub use engine::{EngineError, OdimEngine};
pub use volume_engine::{PolarVolumeEngine, PolarVolumeSiteView};
