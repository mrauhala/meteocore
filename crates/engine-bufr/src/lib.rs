//! WMO BUFR surface-observation engine.
//!
//! Decodes station reports (SYNOP / SHIP BUFR templates) into an in-memory,
//! time-windowed store and serves them as OGC API - EDR (`locations`,
//! `position`, `area`, `radius`) and OGC API - Features (one Point feature
//! per station). Sources: a polled directory / object-store prefix of BUFR
//! files (`data_path`) or a WIS2 subscription, whose store is snapshotted
//! across restarts when the server has a state store (`persist`). See
//! `CLAUDE.md` in this crate.

pub mod decode;
mod engine;
pub mod health;
mod metadata;
pub mod params;
mod persist;
mod source;
pub mod store;
mod wis2;

pub use decode::{Decoder, ObsReport};
pub use engine::{BufrEngine, IngestOutcome, MAX_RESPONSE_VALUES, MAX_STATIONS_IN_POLYGON};
pub use params::ParameterTable;
