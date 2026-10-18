pub mod config;
pub mod engine;
pub mod feature;
pub mod health;
pub mod maps;
pub mod metadata;
pub mod pool;
pub mod query;
pub mod schema;
pub mod security;
pub mod station_map;

pub use engine::PostgisEngine;
pub use health::{Health, HealthSnapshot, HealthStatus, LiveHealth};
pub use maps::strike_window_cache_metrics;
pub use metadata::LocationsCut;
pub use station_map::{station_frame_cache_metrics, StationSymbols, NODATA_MARKER};

pub use config::{PostgisConfigError, PostgisEngineConfig, StationMapSettings, ValidatedParameter};
pub use schema::{
    LongShape, ObservationSchema, PerParameterShape, PerParameterTable, QualifiedTable,
    SchemaError, StationsMapping, WideShape,
};

mod catalog;
mod indexes;
mod privileges;
mod validation;
