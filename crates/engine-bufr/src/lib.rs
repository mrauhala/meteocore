//! WMO BUFR surface-observation engine.
//!
//! Decodes station reports (SYNOP / SHIP BUFR templates) into an in-memory,
//! time-windowed store and serves them as OGC API - EDR (`locations`,
//! `position`, `area`, `radius`) and OGC API - Features (one Point feature
//! per station). Sources: a polled directory / object-store prefix of BUFR
//! files (`data_path`). See `CLAUDE.md` in this crate.

pub mod decode;
mod engine;
pub mod health;
mod metadata;
pub mod params;
mod source;
pub mod store;

pub use decode::{Decoder, ObsReport};
pub use engine::{BufrEngine, MAX_RESPONSE_VALUES, MAX_STATIONS_IN_POLYGON};
pub use params::ParameterTable;
