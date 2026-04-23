//! Validated, engine-internal view of [`ds_core::config::PostgisConfig`].
//!
//! Produced by [`PostgisEngineConfig::resolve`] — resolves `dsn_env`,
//! validates every identifier against the whitelist regex (belt-and-braces
//! with the earlier check in `ds_core::config::validate()`), and expands the
//! schema mapping into typed structures the query builder can emit without
//! re-validating. The public TOML-facing struct stays in `ds-core`; this is
//! the internal form the engine operates on.

use ds_core::config::PostgisConfig;
use thiserror::Error;

use crate::schema::{check_identifier, ObservationSchema, SchemaError, StationsMapping};

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
    pub stations: StationsMapping,
    pub observations: ObservationSchema,
    pub parameters: Vec<ValidatedParameter>,
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

        let stations = StationsMapping::from_config(&cfg.stations)?;
        let observations = ObservationSchema::from_config(&cfg.observations)?;

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
            stations,
            observations,
            parameters,
        })
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
            stations: PostgisStationsConfig {
                table: "weather.stations".into(),
                id_col: "wigos_id".into(),
                label_col: "name".into(),
                geom_col: "the_geom".into(),
                property_cols: vec!["territory".into()],
                where_clause: None,
            },
            observations: PostgisObservationsConfig {
                shape: "per_parameter".into(),
                table: None,
                station_fk_col: Some("wigos_id".into()),
                time_col: Some("time".into()),
                time_col_tz: Some("UTC".into()),
                param_col: None,
                value_col: Some("value".into()),
                geom_col: Some("the_geom".into()),
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
}
