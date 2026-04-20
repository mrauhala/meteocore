//! Validated, engine-internal view of [`ds_core::config::PostgisConfig`].
//!
//! Produced by `PostgisConfig::resolve()` — resolves `dsn_env`, validates
//! every identifier against the whitelist regex, and expands the schema
//! mapping into typed structures the query builder can emit without
//! re-validating. Public TOML-facing struct stays in `ds-core`; this is the
//! internal form the engine operates on.

#[derive(Debug, Clone)]
pub struct PostgisEngineConfig {
    // Populated in #101 (config + DSL parser).
}
