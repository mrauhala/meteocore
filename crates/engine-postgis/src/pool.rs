//! Per-URL deadpool-postgres pool registry.
//!
//! Collections that share a normalized DSN `(host, port, db, user, sslmode)`
//! share one `Arc<Pool>`. Pool lifecycle is independent of engine lifecycle
//! so hot-reload can reuse pools across config swaps — see #102.
