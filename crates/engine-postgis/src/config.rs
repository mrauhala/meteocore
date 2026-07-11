//! Validated, engine-internal view of [`ds_core::config::PostgisConfig`].
//!
//! Produced by [`PostgisEngineConfig::resolve`] — resolves `dsn_env`,
//! validates every identifier against the whitelist regex (belt-and-braces
//! with the earlier check in `ds_core::config::validate()`), and expands the
//! schema mapping into typed structures the query builder can emit without
//! re-validating. The public TOML-facing struct stays in `ds-core`; this is
//! the internal form the engine operates on.

use chrono::Duration;
use ds_core::config::PostgisConfig;
use ds_core::datetime::parse_iso8601_duration;
use thiserror::Error;

use crate::schema::{
    check_identifier, EventsShape, LocationSource, ObservationSchema, SchemaError, StationsMapping,
};

/// Default window for deriving the location list from observations when
/// `observations.locations_window` is absent (24h). Keeps the `DISTINCT ON`
/// scan on recent hypertable chunks.
const DEFAULT_LOCATIONS_WINDOW_HOURS: i64 = 24;

/// `events` shape: default query window when a request carries no `datetime`
/// and the config sets no `default_datetime` (1 h). An unqualified events
/// query never scans full history.
const DEFAULT_EVENTS_WINDOW_HOURS: i64 = 1;

/// Default pool size — matches the render-semaphore sizing (`max(4, cores*2)`
/// capped at 16). Override via `[postgis].pool_size`; hard-capped at 32 at
/// config-load time.
fn default_pool_size() -> u32 {
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    (parallelism * 2).clamp(4, 16)
}

/// Default metadata cache refresh cadence, seconds.
const DEFAULT_METADATA_REFRESH_SECS: u64 = 300;

/// Env var that opts in to a literal DSN being present in TOML.
const INLINE_DSN_OPT_IN_ENV: &str = "MC_ALLOW_INLINE_DB_URL";

#[derive(Debug, Error)]
pub enum PostgisConfigError {
    #[error("DSN env var '{0}' is not set")]
    MissingDsnEnv(String),
    #[error("inline DSN rejected (set {INLINE_DSN_OPT_IN_ENV}=1 to opt in)")]
    InlineDsnNotOptedIn,
    #[error("schema mapping error: {0}")]
    Schema(#[from] SchemaError),
    #[error("no [postgis.stations] table and no resolvable observations geometry — set observations.geom_col or add a stations table")]
    NoLocationSource,
    #[error("invalid observations.locations_window: {0}")]
    InvalidLocationsWindow(String),
    #[error("invalid observations.default_datetime: {0}")]
    InvalidDefaultDatetime(String),
    #[error("parameter '{name}' (source_key '{key}') is not mapped in observations.{where_}")]
    ParameterNotMapped {
        name: String,
        key: String,
        where_: &'static str,
    },
}

/// Validated, engine-ready view of [`ds_core::config::PostgisConfig`]. All
/// identifiers in this struct have already passed the SQL whitelist regex;
/// `quote_ident` in `security.rs` still wraps them at emit time for defense
/// in depth.
#[derive(Debug, Clone)]
pub struct PostgisEngineConfig {
    /// Resolved DSN — either the env var's value, or the literal URL (when
    /// `MC_ALLOW_INLINE_DB_URL=1` is set at resolve time).
    pub dsn: String,
    /// Whether the DSN was sourced from a literal TOML value. Callers log a
    /// WARN at startup when this is true.
    pub dsn_was_literal: bool,
    pub pool_size: u32,
    pub pool_label: Option<String>,
    pub metadata_refresh_secs: u64,
    pub location_source: LocationSource,
    pub observations: ObservationSchema,
    pub parameters: Vec<ValidatedParameter>,
    /// Window for deriving the obs-based location list: `Some(d)` ⇒ only
    /// stations seen within `d` of "now" (the default, 24h, keeps the scan on
    /// recent chunks); `None` ⇒ full history (`observations.locations_window =
    /// "all"`). Only consulted when `location_source` derives from observations.
    pub locations_window: Option<Duration>,
    /// `events` shape: window ending "now" applied when a query has no
    /// `datetime` (default 1 h). Unset for station shapes.
    pub events_default_window: Option<Duration>,
    /// `events` shape: advertised spatial extent from config
    /// (`observations.extent_bbox`) — never computed via `ST_Extent`.
    pub events_extent_bbox: Option<[f64; 4]>,
}

/// Parameter descriptor after cross-checking against the observation shape.
/// `source_key` defaults to `name` when not explicitly configured.
#[derive(Debug, Clone)]
pub struct ValidatedParameter {
    pub name: String,
    pub label: String,
    pub unit: String,
    pub observed_property: String,
    pub source_key: String,
}

impl PostgisEngineConfig {
    /// Lower a TOML-parsed [`PostgisConfig`] into engine-internal form.
    ///
    /// - Reads the DSN from `std::env::var(cfg.dsn_env)` — unless `dsn_env`
    ///   itself looks like a literal URL and [`INLINE_DSN_OPT_IN_ENV`] is set,
    ///   in which case the literal is used directly (and `dsn_was_literal` is
    ///   set so the caller can log a WARN).
    /// - Validates every schema/table/column identifier (`^[A-Za-z_][A-Za-z0-9_]{0,62}$`).
    /// - Cross-checks every `[[parameters]]` entry against the chosen shape
    ///   (`wide` ⇒ must be in `columns`; `per_parameter` ⇒ must be in `tables`).
    ///
    /// `ds_core::config::validate()` is expected to have run first; this is
    /// the engine's defense-in-depth pass.
    pub fn resolve(cfg: &PostgisConfig) -> Result<Self, PostgisConfigError> {
        let (dsn, dsn_was_literal) = resolve_dsn(&cfg.dsn_env)?;

        let observations = ObservationSchema::from_config(&cfg.observations)?;

        // Resolve the location source from the presence of a stations table and
        // whether the observations carry a usable geometry (mirrors the core
        // `validate_postgis` rule; this is the engine's defense-in-depth pass).
        // The events shape has no location concept at all.
        let location_source = if matches!(observations, ObservationSchema::Events(_)) {
            LocationSource::None
        } else {
            let obs_geom = cfg.observations.obs_geom_available();
            match (&cfg.stations, obs_geom) {
                (Some(s), true) => {
                    LocationSource::StationsWithOrphans(StationsMapping::from_config(s)?)
                }
                (Some(s), false) => LocationSource::Stations(StationsMapping::from_config(s)?),
                (None, true) => LocationSource::Observations,
                (None, false) => return Err(PostgisConfigError::NoLocationSource),
            }
        };
        let locations_window = resolve_locations_window(&cfg.observations.locations_window)?;
        let events_default_window = match &observations {
            ObservationSchema::Events(_) => {
                Some(resolve_events_window(&cfg.observations.default_datetime)?)
            }
            _ => None,
        };
        let events_extent_bbox = match &observations {
            ObservationSchema::Events(_) => cfg.observations.extent_bbox,
            _ => None,
        };

        let mut parameters = Vec::with_capacity(cfg.parameters.len());
        for p in &cfg.parameters {
            let source_key = p.source_key.clone().unwrap_or_else(|| p.name.clone());
            // Re-validate parameter source_keys — for wide/per_parameter they
            // must match a columns/tables entry; for long they're string
            // literals (stored as `param_col` values), no validation beyond
            // non-empty.
            match &observations {
                ObservationSchema::Wide(w) => {
                    if !w.columns.iter().any(|(k, _)| k == &source_key) {
                        return Err(PostgisConfigError::ParameterNotMapped {
                            name: p.name.clone(),
                            key: source_key,
                            where_: "columns",
                        });
                    }
                }
                ObservationSchema::PerParameter(pp) => {
                    if !pp.tables.iter().any(|t| t.parameter == source_key) {
                        return Err(PostgisConfigError::ParameterNotMapped {
                            name: p.name.clone(),
                            key: source_key,
                            where_: "tables",
                        });
                    }
                }
                ObservationSchema::Long(_) => {}
                ObservationSchema::Events(_) => {
                    // The source_key IS the column name emitted into SQL —
                    // there is no columns/tables mapping to launder it
                    // through, so it must pass the identifier whitelist.
                    check_identifier(&source_key, "parameters[].source_key (events column)")?;
                }
            }
            parameters.push(ValidatedParameter {
                name: check_identifier(&p.name, "parameters[].name")?,
                label: p.label.clone(),
                unit: p.unit.clone(),
                observed_property: p
                    .observed_property
                    .clone()
                    .unwrap_or_else(|| p.name.clone()),
                source_key,
            });
        }

        let pool_size = cfg.pool_size.unwrap_or_else(default_pool_size).min(32);

        Ok(Self {
            dsn,
            dsn_was_literal,
            pool_size,
            pool_label: cfg.pool_label.clone(),
            metadata_refresh_secs: cfg
                .metadata_refresh_secs
                .unwrap_or(DEFAULT_METADATA_REFRESH_SECS),
            location_source,
            observations,
            parameters,
            locations_window,
            events_default_window,
            events_extent_bbox,
        })
    }

    /// The events shape lowered from this config, when it is one.
    pub fn events(&self) -> Option<&EventsShape> {
        match &self.observations {
            ObservationSchema::Events(ev) => Some(ev),
            _ => None,
        }
    }
}

/// Resolve `observations.default_datetime` (events shape) to a duration:
/// absent ⇒ 1 h default. Defense-in-depth — `ds-core`'s `validate_postgis`
/// already rejects a bad string at config load.
fn resolve_events_window(raw: &Option<String>) -> Result<Duration, PostgisConfigError> {
    match raw.as_deref() {
        None => Ok(Duration::hours(DEFAULT_EVENTS_WINDOW_HOURS)),
        Some(s) => parse_iso8601_duration(s)
            .map_err(|e| PostgisConfigError::InvalidDefaultDatetime(e.to_string())),
    }
}

/// Resolve `observations.locations_window` to a `chrono::Duration`:
/// absent ⇒ the 24h default; `"all"` (case-insensitive) ⇒ `None` (full
/// history); otherwise an ISO 8601 duration. Defense-in-depth — `ds-core`'s
/// `validate_postgis` already rejects a bad string at config load.
fn resolve_locations_window(raw: &Option<String>) -> Result<Option<Duration>, PostgisConfigError> {
    match raw.as_deref() {
        None => Ok(Some(Duration::hours(DEFAULT_LOCATIONS_WINDOW_HOURS))),
        Some(s) if s.eq_ignore_ascii_case("all") => Ok(None),
        Some(s) => {
            let d = parse_iso8601_duration(s)
                .map_err(|e| PostgisConfigError::InvalidLocationsWindow(e.to_string()))?;
            Ok(Some(d))
        }
    }
}

fn looks_like_db_url(s: &str) -> bool {
    s.starts_with("postgres://") || s.starts_with("postgresql://")
}

fn resolve_dsn(dsn_env: &str) -> Result<(String, bool), PostgisConfigError> {
    if looks_like_db_url(dsn_env) {
        if std::env::var(INLINE_DSN_OPT_IN_ENV).ok().as_deref() == Some("1") {
            return Ok((dsn_env.to_string(), true));
        }
        return Err(PostgisConfigError::InlineDsnNotOptedIn);
    }
    match std::env::var(dsn_env) {
        Ok(v) => Ok((v, false)),
        Err(_) => Err(PostgisConfigError::MissingDsnEnv(dsn_env.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::config::{
        PostgisObservationColumn, PostgisObservationTable, PostgisObservationsConfig,
        PostgisParameterConfig, PostgisStationsConfig,
    };

    // Guard around MC_ALLOW_INLINE_DB_URL + dsn env vars to serialize process-wide
    // env mutations across tests without adding a `serial_test` dep.
    struct EnvGuard<'a> {
        // Held for the duration of the guard; releases the mutex on Drop.
        #[allow(dead_code)]
        lock: std::sync::MutexGuard<'a, ()>,
    }
    impl EnvGuard<'_> {
        fn set(&self, k: &str, v: Option<&str>) {
            // SAFETY: lock is held for the duration of this guard; nothing
            // else in this test module touches process env without also going
            // through `env_guard()`.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
    impl Drop for EnvGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(INLINE_DSN_OPT_IN_ENV);
                std::env::remove_var("TEST_DSN");
            }
        }
    }
    fn env_guard() -> EnvGuard<'static> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        EnvGuard {
            lock: LOCK.lock().unwrap_or_else(|p| p.into_inner()),
        }
    }

    fn nexus_config() -> PostgisConfig {
        PostgisConfig {
            dsn_env: "TEST_DSN".into(),
            pool_size: None,
            pool_label: None,
            metadata_refresh_secs: None,
            stations: Some(PostgisStationsConfig {
                table: "weather.stations".into(),
                id_col: "wigos_id".into(),
                label_col: "name".into(),
                geom_col: "the_geom".into(),
                property_cols: vec!["territory".into()],
                where_clause: None,
            }),
            observations: PostgisObservationsConfig {
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
                tables: vec![
                    PostgisObservationTable {
                        parameter: "t2m".into(),
                        table: "weather.air_temperature".into(),
                        station_fk_col: None,
                        time_col: None,
                        time_col_tz: None,
                        value_col: None,
                        geom_col: None,
                    },
                    PostgisObservationTable {
                        parameter: "ws_10m".into(),
                        table: "weather.wind_speed".into(),
                        station_fk_col: None,
                        time_col: None,
                        time_col_tz: None,
                        value_col: None,
                        geom_col: None,
                    },
                ],
            },
            parameters: vec![
                PostgisParameterConfig {
                    name: "t2m".into(),
                    label: "2 m air temperature".into(),
                    unit: "°C".into(),
                    observed_property: Some("air_temperature".into()),
                    source_key: None,
                },
                PostgisParameterConfig {
                    name: "ws_10m".into(),
                    label: "10 m wind speed".into(),
                    unit: "m/s".into(),
                    observed_property: Some("wind_speed".into()),
                    source_key: None,
                },
            ],
        }
    }

    #[test]
    fn resolve_reads_dsn_from_env() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        g.set(INLINE_DSN_OPT_IN_ENV, None);

        let resolved = PostgisEngineConfig::resolve(&nexus_config()).unwrap();
        assert_eq!(resolved.dsn, "postgres://from-env/obs");
        assert!(!resolved.dsn_was_literal);
        assert_eq!(resolved.parameters.len(), 2);
        assert_eq!(resolved.parameters[0].source_key, "t2m");
        assert_eq!(resolved.parameters[0].observed_property, "air_temperature");
        match &resolved.observations {
            ObservationSchema::PerParameter(pp) => {
                assert_eq!(pp.tables.len(), 2);
                assert_eq!(pp.tables[0].parameter, "t2m");
                assert_eq!(pp.tables[0].table.table, "air_temperature");
            }
            _ => panic!("expected PerParameter"),
        }
    }

    #[test]
    fn resolve_stations_with_obs_geom_is_orphan_fallback() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        // nexus_config has both a stations table and an observations geom_col.
        let resolved = PostgisEngineConfig::resolve(&nexus_config()).unwrap();
        assert!(matches!(
            resolved.location_source,
            LocationSource::StationsWithOrphans(_)
        ));
    }

    #[test]
    fn resolve_stations_without_obs_geom_is_stations_only() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let mut cfg = nexus_config();
        cfg.observations.geom_col = None; // per-table geoms are already None
        let resolved = PostgisEngineConfig::resolve(&cfg).unwrap();
        assert!(matches!(
            resolved.location_source,
            LocationSource::Stations(_)
        ));
    }

    #[test]
    fn resolve_no_stations_with_obs_geom_is_observations() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let mut cfg = nexus_config();
        cfg.stations = None;
        let resolved = PostgisEngineConfig::resolve(&cfg).unwrap();
        assert!(matches!(
            resolved.location_source,
            LocationSource::Observations
        ));
    }

    #[test]
    fn resolve_no_stations_no_geom_errors() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let mut cfg = nexus_config();
        cfg.stations = None;
        cfg.observations.geom_col = None;
        let err = PostgisEngineConfig::resolve(&cfg).unwrap_err();
        assert!(matches!(err, PostgisConfigError::NoLocationSource));
    }

    #[test]
    fn resolve_locations_window_defaults_to_24h() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let resolved = PostgisEngineConfig::resolve(&nexus_config()).unwrap();
        assert_eq!(resolved.locations_window, Some(Duration::hours(24)));
    }

    #[test]
    fn resolve_locations_window_all_is_full_history() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let mut cfg = nexus_config();
        cfg.observations.locations_window = Some("all".into());
        let resolved = PostgisEngineConfig::resolve(&cfg).unwrap();
        assert_eq!(resolved.locations_window, None);
    }

    #[test]
    fn resolve_locations_window_parses_duration() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let mut cfg = nexus_config();
        cfg.observations.locations_window = Some("PT12H".into());
        let resolved = PostgisEngineConfig::resolve(&cfg).unwrap();
        assert_eq!(resolved.locations_window, Some(Duration::hours(12)));
    }

    #[test]
    fn resolve_locations_window_invalid_errors() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let mut cfg = nexus_config();
        cfg.observations.locations_window = Some("garbage".into());
        let err = PostgisEngineConfig::resolve(&cfg).unwrap_err();
        assert!(matches!(err, PostgisConfigError::InvalidLocationsWindow(_)));
    }

    #[test]
    fn resolve_missing_env_var_errors() {
        let g = env_guard();
        g.set("TEST_DSN", None);
        g.set(INLINE_DSN_OPT_IN_ENV, None);
        let err = PostgisEngineConfig::resolve(&nexus_config()).unwrap_err();
        assert!(matches!(err, PostgisConfigError::MissingDsnEnv(_)));
    }

    #[test]
    fn resolve_inline_dsn_requires_opt_in() {
        let g = env_guard();
        g.set(INLINE_DSN_OPT_IN_ENV, None);

        let mut cfg = nexus_config();
        cfg.dsn_env = "postgres://user:pass@localhost/obs".into();
        let err = PostgisEngineConfig::resolve(&cfg).unwrap_err();
        assert!(matches!(err, PostgisConfigError::InlineDsnNotOptedIn));

        g.set(INLINE_DSN_OPT_IN_ENV, Some("1"));
        let resolved = PostgisEngineConfig::resolve(&cfg).unwrap();
        assert_eq!(resolved.dsn, "postgres://user:pass@localhost/obs");
        assert!(resolved.dsn_was_literal);
    }

    #[test]
    fn resolve_rejects_parameter_missing_from_wide_columns() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));

        let mut cfg = nexus_config();
        cfg.observations = PostgisObservationsConfig {
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
            columns: vec![PostgisObservationColumn {
                parameter: "t2m".into(),
                column: "temperature".into(),
            }],
            tables: vec![],
        };
        // parameters[] still has ws_10m, which has no matching column.
        let err = PostgisEngineConfig::resolve(&cfg).unwrap_err();
        assert!(
            matches!(err, PostgisConfigError::ParameterNotMapped { ref name, .. } if name == "ws_10m"),
            "got: {err:?}"
        );
    }

    #[test]
    fn resolve_long_shape_accepts_any_parameter_source_key() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));

        let mut cfg = nexus_config();
        cfg.observations = PostgisObservationsConfig {
            shape: "long".into(),
            table: Some("public.obs".into()),
            station_fk_col: Some("station_id".into()),
            time_col: Some("time".into()),
            time_col_tz: None,
            param_col: Some("param".into()),
            value_col: Some("value".into()),
            geom_col: None,
            locations_window: None,
            id_col: None,
            default_datetime: None,
            extent_bbox: None,
            columns: vec![],
            tables: vec![],
        };
        // parameters untouched from nexus_config — for long, source_key is
        // just a param_col literal, no cross-ref required.
        let resolved = PostgisEngineConfig::resolve(&cfg).unwrap();
        assert_eq!(resolved.parameters.len(), 2);
    }

    #[test]
    fn resolve_caps_pool_size_at_32() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));

        let mut cfg = nexus_config();
        cfg.pool_size = Some(100);
        let resolved = PostgisEngineConfig::resolve(&cfg).unwrap();
        assert_eq!(resolved.pool_size, 32);
    }

    // ---- events ------------------------------------------------------------

    fn events_config() -> PostgisConfig {
        PostgisConfig {
            dsn_env: "TEST_DSN".into(),
            pool_size: None,
            pool_label: None,
            metadata_refresh_secs: None,
            stations: None,
            observations: PostgisObservationsConfig {
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
                default_datetime: None,
                extent_bbox: Some([4.0, 54.0, 42.0, 72.0]),
            },
            parameters: vec![PostgisParameterConfig {
                name: "peak_current".into(),
                label: "Peak current".into(),
                unit: "kA".into(),
                observed_property: None,
                source_key: None,
            }],
        }
    }

    #[test]
    fn resolve_events_no_location_source_and_defaults() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let resolved = PostgisEngineConfig::resolve(&events_config()).unwrap();
        assert!(matches!(resolved.location_source, LocationSource::None));
        assert!(resolved.events().is_some());
        // default_datetime absent ⇒ the 1 h default; extent from config.
        assert_eq!(resolved.events_default_window, Some(Duration::hours(1)));
        assert_eq!(resolved.events_extent_bbox, Some([4.0, 54.0, 42.0, 72.0]));
    }

    #[test]
    fn resolve_events_parses_default_datetime() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let mut cfg = events_config();
        cfg.observations.default_datetime = Some("PT15M".into());
        let resolved = PostgisEngineConfig::resolve(&cfg).unwrap();
        assert_eq!(resolved.events_default_window, Some(Duration::minutes(15)));
    }

    #[test]
    fn resolve_events_rejects_invalid_source_key() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let mut cfg = events_config();
        cfg.parameters[0].source_key = Some("bad;drop".into());
        let err = PostgisEngineConfig::resolve(&cfg).unwrap_err();
        assert!(matches!(err, PostgisConfigError::Schema(_)));
    }

    #[test]
    fn resolve_station_shapes_have_no_events_window() {
        let g = env_guard();
        g.set("TEST_DSN", Some("postgres://from-env/obs"));
        let resolved = PostgisEngineConfig::resolve(&nexus_config()).unwrap();
        assert_eq!(resolved.events_default_window, None);
        assert_eq!(resolved.events_extent_bbox, None);
    }
}
