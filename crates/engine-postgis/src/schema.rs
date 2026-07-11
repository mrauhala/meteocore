//! Schema-mapping DSL and identifier whitelist helpers.
//!
//! Three shapes — `long` (EAV), `wide` (column-per-param), `per_parameter`
//! (table-per-param) — are expressed in TOML as [`ds_core::config::PostgisConfig`]
//! and lowered here into typed, validated mapping structs the query builder
//! uses directly.
//!
//! Identifier validation is `^[A-Za-z_][A-Za-z0-9_]{0,62}$` enforced as a
//! byte-level check (no regex dep). The same check runs earlier in
//! `ds_core::config::validate()`; we repeat it here as defense-in-depth for
//! any code path that constructs a mapping bypassing the TOML loader.

use ds_core::config::{
    is_valid_sql_identifier, validate_qualified_table, PostgisObservationColumn,
    PostgisObservationTable, PostgisObservationsConfig, PostgisStationsConfig,
};
use thiserror::Error;

/// Errors produced while lowering a [`ds_core::config::PostgisConfig`] into a
/// validated schema mapping. All are hard fails at engine construction.
#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("invalid SQL identifier '{ident}' ({context})")]
    InvalidIdentifier { ident: String, context: String },
    #[error("invalid qualified table '{name}' ({reason})")]
    InvalidQualifiedTable { name: String, reason: String },
    #[error("shape inconsistency: {0}")]
    ShapeMismatch(String),
}

/// Parsed, validated `schema.table` pair. Schema defaults to `"public"` when
/// the name was unqualified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedTable {
    pub schema: String,
    pub table: String,
}

impl QualifiedTable {
    pub fn parse(name: &str) -> Result<Self, SchemaError> {
        match validate_qualified_table(name) {
            Ok((schema, table)) => Ok(Self {
                schema: schema.to_string(),
                table: table.to_string(),
            }),
            Err(reason) => Err(SchemaError::InvalidQualifiedTable {
                name: name.to_string(),
                reason,
            }),
        }
    }
}

/// Validated identifier used in SQL emission. Construction enforces the
/// whitelist regex so later `quote_ident` has nothing pathological to escape.
pub fn check_identifier(ident: &str, context: &str) -> Result<String, SchemaError> {
    if is_valid_sql_identifier(ident) {
        Ok(ident.to_string())
    } else {
        Err(SchemaError::InvalidIdentifier {
            ident: ident.to_string(),
            context: context.to_string(),
        })
    }
}

/// Stations mapping after validation.
#[derive(Debug, Clone)]
pub struct StationsMapping {
    pub table: QualifiedTable,
    pub id_col: String,
    pub label_col: String,
    pub geom_col: String,
    pub property_cols: Vec<String>,
    /// Config-time-constant WHERE fragment; not re-parsed from user input.
    pub where_clause: Option<String>,
}

impl StationsMapping {
    pub fn from_config(cfg: &PostgisStationsConfig) -> Result<Self, SchemaError> {
        let mut property_cols = Vec::with_capacity(cfg.property_cols.len());
        for col in &cfg.property_cols {
            property_cols.push(check_identifier(col, "stations.property_cols")?);
        }
        Ok(Self {
            table: QualifiedTable::parse(&cfg.table)?,
            id_col: check_identifier(&cfg.id_col, "stations.id_col")?,
            label_col: check_identifier(&cfg.label_col, "stations.label_col")?,
            geom_col: check_identifier(&cfg.geom_col, "stations.geom_col")?,
            property_cols,
            where_clause: cfg.where_clause.clone(),
        })
    }
}

/// Where the engine sources its locations — resolved once at config lowering
/// from the presence of a `[postgis.stations]` block and a usable observations
/// geometry column. Gates the metadata refresh and the position/area paths;
/// the per-request observation fetch is identical in every variant.
#[derive(Debug, Clone)]
pub enum LocationSource {
    /// Stations table is the sole source of locations (the original behavior).
    Stations(StationsMapping),
    /// No stations table — every location is derived from the observations
    /// table's geometry (mode A). Orphan id = the `station_fk` value, label = id,
    /// properties empty.
    Observations,
    /// Stations table supplies label/properties for registered stations;
    /// observations whose `station_fk` has no stations row are filled from the
    /// observations geometry (mode B, orphan fallback).
    StationsWithOrphans(StationsMapping),
    /// The collection has no station/location concept at all (`events`
    /// shape) — the location list is empty and position/locations queries
    /// are rejected at the engine layer.
    None,
}

impl LocationSource {
    /// The stations mapping, if this source has one (`Stations` /
    /// `StationsWithOrphans`); `None` for the observations-only mode.
    pub fn stations(&self) -> Option<&StationsMapping> {
        match self {
            LocationSource::Stations(s) | LocationSource::StationsWithOrphans(s) => Some(s),
            LocationSource::Observations | LocationSource::None => None,
        }
    }

    /// Whether observation-derived locations participate (modes A and B) — i.e.
    /// the refresh must derive locations from the observations table and
    /// position/area are answered in-memory from the cached set.
    pub fn uses_observations(&self) -> bool {
        matches!(
            self,
            LocationSource::Observations | LocationSource::StationsWithOrphans(_)
        )
    }
}

/// Observation-schema shapes. One per `observations.shape` TOML value.
#[derive(Debug, Clone)]
pub enum ObservationSchema {
    Long(LongShape),
    Wide(WideShape),
    PerParameter(PerParameterShape),
    Events(EventsShape),
}

#[derive(Debug, Clone)]
pub struct LongShape {
    pub table: QualifiedTable,
    pub station_fk_col: String,
    pub time_col: String,
    pub time_col_tz: Option<String>,
    pub param_col: String,
    pub value_col: String,
    pub geom_col: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WideShape {
    pub table: QualifiedTable,
    pub station_fk_col: String,
    pub time_col: String,
    pub time_col_tz: Option<String>,
    pub geom_col: Option<String>,
    /// parameter key → column name.
    pub columns: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct PerParameterShape {
    pub tables: Vec<PerParameterTable>,
}

/// `events` shape (#113): non-station event data — each row is an
/// independent event with its own time and point geometry (e.g. a lightning
/// strike). Parameter `source_key`s name columns on this table directly
/// (validated as identifiers at config load and again at engine resolve).
#[derive(Debug, Clone)]
pub struct EventsShape {
    pub table: QualifiedTable,
    pub time_col: String,
    pub time_col_tz: Option<String>,
    pub geom_col: String,
    /// Unique/tiebreak column for deterministic `ORDER BY time DESC, id`.
    pub id_col: String,
}

#[derive(Debug, Clone)]
pub struct PerParameterTable {
    pub parameter: String,
    pub table: QualifiedTable,
    pub station_fk_col: String,
    pub time_col: String,
    pub time_col_tz: Option<String>,
    pub value_col: String,
    pub geom_col: Option<String>,
}

impl ObservationSchema {
    pub fn from_config(cfg: &PostgisObservationsConfig) -> Result<Self, SchemaError> {
        match cfg.shape.as_str() {
            "long" => Ok(Self::Long(LongShape::from_config(cfg)?)),
            "wide" => Ok(Self::Wide(WideShape::from_config(cfg)?)),
            "per_parameter" => Ok(Self::PerParameter(PerParameterShape::from_config(cfg)?)),
            "events" => Ok(Self::Events(EventsShape::from_config(cfg)?)),
            other => Err(SchemaError::ShapeMismatch(format!(
                "unknown shape '{other}'"
            ))),
        }
    }
}

impl LongShape {
    fn from_config(cfg: &PostgisObservationsConfig) -> Result<Self, SchemaError> {
        let table = cfg.table.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'long' requires observations.table".into())
        })?;
        let station_fk_col = cfg.station_fk_col.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'long' requires observations.station_fk_col".into())
        })?;
        let time_col = cfg.time_col.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'long' requires observations.time_col".into())
        })?;
        let param_col = cfg.param_col.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'long' requires observations.param_col".into())
        })?;
        let value_col = cfg.value_col.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'long' requires observations.value_col".into())
        })?;
        if !cfg.columns.is_empty() {
            return Err(SchemaError::ShapeMismatch(
                "'long' does not allow [[observations.columns]]".into(),
            ));
        }
        if !cfg.tables.is_empty() {
            return Err(SchemaError::ShapeMismatch(
                "'long' does not allow [[observations.tables]]".into(),
            ));
        }

        Ok(Self {
            table: QualifiedTable::parse(table)?,
            station_fk_col: check_identifier(station_fk_col, "observations.station_fk_col")?,
            time_col: check_identifier(time_col, "observations.time_col")?,
            time_col_tz: cfg.time_col_tz.clone(),
            param_col: check_identifier(param_col, "observations.param_col")?,
            value_col: check_identifier(value_col, "observations.value_col")?,
            geom_col: cfg
                .geom_col
                .as_deref()
                .map(|g| check_identifier(g, "observations.geom_col"))
                .transpose()?,
        })
    }
}

impl WideShape {
    fn from_config(cfg: &PostgisObservationsConfig) -> Result<Self, SchemaError> {
        let table = cfg.table.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'wide' requires observations.table".into())
        })?;
        let station_fk_col = cfg.station_fk_col.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'wide' requires observations.station_fk_col".into())
        })?;
        let time_col = cfg.time_col.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'wide' requires observations.time_col".into())
        })?;
        if cfg.columns.is_empty() {
            return Err(SchemaError::ShapeMismatch(
                "'wide' requires at least one [[observations.columns]] entry".into(),
            ));
        }
        if cfg.param_col.is_some() || cfg.value_col.is_some() {
            return Err(SchemaError::ShapeMismatch(
                "'wide' does not allow observations.param_col / observations.value_col".into(),
            ));
        }
        if !cfg.tables.is_empty() {
            return Err(SchemaError::ShapeMismatch(
                "'wide' does not allow [[observations.tables]]".into(),
            ));
        }

        let mut columns = Vec::with_capacity(cfg.columns.len());
        for PostgisObservationColumn { parameter, column } in &cfg.columns {
            columns.push((
                parameter.clone(),
                check_identifier(column, "observations.columns[].column")?,
            ));
        }

        Ok(Self {
            table: QualifiedTable::parse(table)?,
            station_fk_col: check_identifier(station_fk_col, "observations.station_fk_col")?,
            time_col: check_identifier(time_col, "observations.time_col")?,
            time_col_tz: cfg.time_col_tz.clone(),
            geom_col: cfg
                .geom_col
                .as_deref()
                .map(|g| check_identifier(g, "observations.geom_col"))
                .transpose()?,
            columns,
        })
    }
}

impl PerParameterShape {
    fn from_config(cfg: &PostgisObservationsConfig) -> Result<Self, SchemaError> {
        if cfg.tables.is_empty() {
            return Err(SchemaError::ShapeMismatch(
                "'per_parameter' requires at least one [[observations.tables]] entry".into(),
            ));
        }
        if cfg.table.is_some() || cfg.param_col.is_some() {
            return Err(SchemaError::ShapeMismatch(
                "'per_parameter' does not allow observations.table / observations.param_col".into(),
            ));
        }
        if !cfg.columns.is_empty() {
            return Err(SchemaError::ShapeMismatch(
                "'per_parameter' does not allow [[observations.columns]]".into(),
            ));
        }

        let mut tables = Vec::with_capacity(cfg.tables.len());
        for entry in &cfg.tables {
            tables.push(PerParameterTable::from_config(cfg, entry)?);
        }
        Ok(Self { tables })
    }
}

impl EventsShape {
    fn from_config(cfg: &PostgisObservationsConfig) -> Result<Self, SchemaError> {
        let table = cfg.table.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'events' requires observations.table".into())
        })?;
        let time_col = cfg.time_col.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'events' requires observations.time_col".into())
        })?;
        let geom_col = cfg.geom_col.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'events' requires observations.geom_col".into())
        })?;
        let id_col = cfg.id_col.as_deref().ok_or_else(|| {
            SchemaError::ShapeMismatch("'events' requires observations.id_col".into())
        })?;
        if cfg.station_fk_col.is_some() || cfg.param_col.is_some() || cfg.value_col.is_some() {
            return Err(SchemaError::ShapeMismatch(
                "'events' does not allow station_fk_col / param_col / value_col".into(),
            ));
        }
        if !cfg.columns.is_empty() || !cfg.tables.is_empty() {
            return Err(SchemaError::ShapeMismatch(
                "'events' does not allow [[observations.columns]] / [[observations.tables]]".into(),
            ));
        }

        Ok(Self {
            table: QualifiedTable::parse(table)?,
            time_col: check_identifier(time_col, "observations.time_col")?,
            time_col_tz: cfg.time_col_tz.clone(),
            geom_col: check_identifier(geom_col, "observations.geom_col")?,
            id_col: check_identifier(id_col, "observations.id_col")?,
        })
    }
}

impl PerParameterTable {
    fn from_config(
        defaults: &PostgisObservationsConfig,
        entry: &PostgisObservationTable,
    ) -> Result<Self, SchemaError> {
        let station_fk_col = entry
            .station_fk_col
            .as_deref()
            .or(defaults.station_fk_col.as_deref())
            .ok_or_else(|| {
                SchemaError::ShapeMismatch(format!(
                    "observations.tables['{}'] missing station_fk_col (no default)",
                    entry.parameter
                ))
            })?;
        let time_col = entry
            .time_col
            .as_deref()
            .or(defaults.time_col.as_deref())
            .ok_or_else(|| {
                SchemaError::ShapeMismatch(format!(
                    "observations.tables['{}'] missing time_col (no default)",
                    entry.parameter
                ))
            })?;
        let value_col = entry
            .value_col
            .as_deref()
            .or(defaults.value_col.as_deref())
            .ok_or_else(|| {
                SchemaError::ShapeMismatch(format!(
                    "observations.tables['{}'] missing value_col (no default)",
                    entry.parameter
                ))
            })?;
        let time_col_tz = entry
            .time_col_tz
            .clone()
            .or_else(|| defaults.time_col_tz.clone());
        let geom_col = entry.geom_col.clone().or_else(|| defaults.geom_col.clone());

        Ok(Self {
            parameter: entry.parameter.clone(),
            table: QualifiedTable::parse(&entry.table)?,
            station_fk_col: check_identifier(
                station_fk_col,
                "observations.tables[].station_fk_col",
            )?,
            time_col: check_identifier(time_col, "observations.tables[].time_col")?,
            time_col_tz,
            value_col: check_identifier(value_col, "observations.tables[].value_col")?,
            geom_col: geom_col
                .as_deref()
                .map(|g| check_identifier(g, "observations.tables[].geom_col"))
                .transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_identifier_accepts_regular_names() {
        assert_eq!(check_identifier("wigos_id", "t").unwrap(), "wigos_id");
        assert_eq!(check_identifier("_", "t").unwrap(), "_");
        assert_eq!(check_identifier("a1", "t").unwrap(), "a1");
    }

    #[test]
    fn check_identifier_rejects_sql_injection_payload() {
        let err = check_identifier("\"; DROP TABLE x;--", "ctx").unwrap_err();
        match err {
            SchemaError::InvalidIdentifier { ident, context } => {
                assert_eq!(ident, "\"; DROP TABLE x;--");
                assert_eq!(context, "ctx");
            }
            _ => panic!("wrong error: {err:?}"),
        }
    }

    #[test]
    fn check_identifier_rejects_embedded_quote() {
        assert!(check_identifier("a\"b", "t").is_err());
    }

    #[test]
    fn check_identifier_rejects_embedded_nul() {
        assert!(check_identifier("a\0b", "t").is_err());
    }

    #[test]
    fn check_identifier_rejects_empty() {
        assert!(check_identifier("", "t").is_err());
    }

    #[test]
    fn check_identifier_rejects_oversize() {
        let too_long = "a".repeat(64);
        assert!(check_identifier(&too_long, "t").is_err());
        // 63 is the upper bound — must be accepted.
        let max = "a".repeat(63);
        assert!(check_identifier(&max, "t").is_ok());
    }

    #[test]
    fn qualified_table_parses_schema_and_bare() {
        assert_eq!(
            QualifiedTable::parse("weather.stations").unwrap(),
            QualifiedTable {
                schema: "weather".into(),
                table: "stations".into(),
            }
        );
        assert_eq!(
            QualifiedTable::parse("stations").unwrap(),
            QualifiedTable {
                schema: "public".into(),
                table: "stations".into(),
            }
        );
    }

    #[test]
    fn qualified_table_rejects_three_parts() {
        assert!(QualifiedTable::parse("a.b.c").is_err());
    }

    #[test]
    fn qualified_table_rejects_bad_parts() {
        assert!(QualifiedTable::parse("1bad.tbl").is_err());
        assert!(QualifiedTable::parse("ok.\"nope\"").is_err());
    }

    fn minimal_long() -> PostgisObservationsConfig {
        PostgisObservationsConfig {
            shape: "long".into(),
            table: Some("public.obs".into()),
            station_fk_col: Some("station_id".into()),
            time_col: Some("time".into()),
            time_col_tz: Some("UTC".into()),
            param_col: Some("param".into()),
            value_col: Some("value".into()),
            geom_col: None,
            locations_window: None,
            id_col: None,
            default_datetime: None,
            extent_bbox: None,
            columns: vec![],
            tables: vec![],
        }
    }

    #[test]
    fn observation_schema_from_config_long() {
        let cfg = minimal_long();
        let schema = ObservationSchema::from_config(&cfg).unwrap();
        match schema {
            ObservationSchema::Long(s) => {
                assert_eq!(s.table.schema, "public");
                assert_eq!(s.table.table, "obs");
                assert_eq!(s.param_col, "param");
                assert_eq!(s.time_col_tz.as_deref(), Some("UTC"));
            }
            _ => panic!("expected Long"),
        }
    }

    #[test]
    fn observation_schema_wide_carries_column_map() {
        let cfg = PostgisObservationsConfig {
            shape: "wide".into(),
            table: Some("public.obs".into()),
            station_fk_col: Some("station_id".into()),
            time_col: Some("time".into()),
            time_col_tz: None,
            param_col: None,
            value_col: None,
            geom_col: None,
            locations_window: None,
            id_col: None,
            default_datetime: None,
            extent_bbox: None,
            columns: vec![
                PostgisObservationColumn {
                    parameter: "t2m".into(),
                    column: "temperature".into(),
                },
                PostgisObservationColumn {
                    parameter: "ws_10m".into(),
                    column: "wind_speed".into(),
                },
            ],
            tables: vec![],
        };
        let schema = ObservationSchema::from_config(&cfg).unwrap();
        match schema {
            ObservationSchema::Wide(w) => {
                assert_eq!(w.columns.len(), 2);
                assert_eq!(w.columns[0], ("t2m".into(), "temperature".into()));
            }
            _ => panic!("expected Wide"),
        }
    }

    #[test]
    fn observation_schema_per_parameter_inherits_defaults() {
        let cfg = PostgisObservationsConfig {
            shape: "per_parameter".into(),
            table: None,
            station_fk_col: Some("wigos_id".into()),
            time_col: Some("time".into()),
            time_col_tz: Some("UTC".into()),
            param_col: None,
            value_col: Some("value".into()),
            geom_col: Some("the_geom".into()),
            locations_window: None,
            id_col: None,
            default_datetime: None,
            extent_bbox: None,
            columns: vec![],
            tables: vec![PostgisObservationTable {
                parameter: "t2m".into(),
                table: "weather.air_temperature".into(),
                station_fk_col: None,
                time_col: None,
                time_col_tz: None,
                value_col: None,
                geom_col: None,
            }],
        };
        let schema = ObservationSchema::from_config(&cfg).unwrap();
        match schema {
            ObservationSchema::PerParameter(pp) => {
                assert_eq!(pp.tables.len(), 1);
                let t = &pp.tables[0];
                assert_eq!(t.parameter, "t2m");
                assert_eq!(t.table.schema, "weather");
                assert_eq!(t.table.table, "air_temperature");
                assert_eq!(t.station_fk_col, "wigos_id");
                assert_eq!(t.time_col, "time");
                assert_eq!(t.time_col_tz.as_deref(), Some("UTC"));
                assert_eq!(t.value_col, "value");
                assert_eq!(t.geom_col.as_deref(), Some("the_geom"));
            }
            _ => panic!("expected PerParameter"),
        }
    }

    #[test]
    fn observation_schema_unknown_shape_rejected() {
        let mut cfg = minimal_long();
        cfg.shape = "weird".into();
        let err = ObservationSchema::from_config(&cfg).unwrap_err();
        assert!(matches!(err, SchemaError::ShapeMismatch(_)));
    }

    #[test]
    fn per_parameter_without_defaults_errors_on_missing_columns() {
        let cfg = PostgisObservationsConfig {
            shape: "per_parameter".into(),
            table: None,
            station_fk_col: None, // no default
            time_col: Some("time".into()),
            time_col_tz: None,
            param_col: None,
            value_col: Some("value".into()),
            geom_col: None,
            locations_window: None,
            id_col: None,
            default_datetime: None,
            extent_bbox: None,
            columns: vec![],
            tables: vec![PostgisObservationTable {
                parameter: "t2m".into(),
                table: "weather.air_temperature".into(),
                station_fk_col: None, // no override either → error
                time_col: None,
                time_col_tz: None,
                value_col: None,
                geom_col: None,
            }],
        };
        let err = ObservationSchema::from_config(&cfg).unwrap_err();
        assert!(matches!(err, SchemaError::ShapeMismatch(msg) if msg.contains("station_fk_col")));
    }

    fn minimal_events() -> PostgisObservationsConfig {
        PostgisObservationsConfig {
            shape: "events".into(),
            table: Some("public.lightning".into()),
            station_fk_col: None,
            time_col: Some("time".into()),
            time_col_tz: Some("UTC".into()),
            param_col: None,
            value_col: None,
            geom_col: Some("the_geom".into()),
            locations_window: None,
            columns: vec![],
            tables: vec![],
            id_col: Some("id".into()),
            default_datetime: Some("PT1H".into()),
            extent_bbox: Some([4.0, 54.0, 42.0, 72.0]),
        }
    }

    #[test]
    fn observation_schema_events_from_config() {
        let schema = ObservationSchema::from_config(&minimal_events()).unwrap();
        match schema {
            ObservationSchema::Events(ev) => {
                assert_eq!(ev.table.schema, "public");
                assert_eq!(ev.table.table, "lightning");
                assert_eq!(ev.time_col, "time");
                assert_eq!(ev.time_col_tz.as_deref(), Some("UTC"));
                assert_eq!(ev.geom_col, "the_geom");
                assert_eq!(ev.id_col, "id");
            }
            _ => panic!("expected Events"),
        }
    }

    #[test]
    fn events_requires_id_col() {
        let mut cfg = minimal_events();
        cfg.id_col = None;
        let err = ObservationSchema::from_config(&cfg).unwrap_err();
        assert!(matches!(err, SchemaError::ShapeMismatch(msg) if msg.contains("id_col")));
    }

    #[test]
    fn events_requires_geom_col() {
        let mut cfg = minimal_events();
        cfg.geom_col = None;
        let err = ObservationSchema::from_config(&cfg).unwrap_err();
        assert!(matches!(err, SchemaError::ShapeMismatch(msg) if msg.contains("geom_col")));
    }

    #[test]
    fn events_rejects_station_machinery() {
        let mut cfg = minimal_events();
        cfg.station_fk_col = Some("station_id".into());
        let err = ObservationSchema::from_config(&cfg).unwrap_err();
        assert!(matches!(err, SchemaError::ShapeMismatch(_)));

        let mut cfg = minimal_events();
        cfg.columns = vec![PostgisObservationColumn {
            parameter: "x".into(),
            column: "x".into(),
        }];
        let err = ObservationSchema::from_config(&cfg).unwrap_err();
        assert!(matches!(err, SchemaError::ShapeMismatch(_)));
    }
}
