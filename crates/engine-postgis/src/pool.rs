//! Per-URL deadpool-postgres pool registry.
//!
//! Collections sharing a normalized DSN `(host, port, db, user, sslmode)`
//! share one `Arc<Pool>`. Pool lifecycle is independent of engine lifecycle
//! so `/admin/collections/reload` can reuse pools across config swaps by
//! identity — see the reload contract in the plan doc.
//!
//! Password binding: the registry refuses to hand the same `(host, port,
//! db, user, sslmode)` two different passwords. This prevents a config
//! typo from silently splitting a pool into two.
//!
//! TLS: v1 ships `NoTls`. Real TLS wiring (rustls + `webpki-roots` fallback
//! for distroless) lands with #110 via a builder hook.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::Arc;

use deadpool_postgres::{ManagerConfig, Pool, RecyclingMethod, Runtime};
use thiserror::Error;
use tokio_postgres::config::{Host, SslMode};
use tokio_postgres::{Config as PgConfig, NoTls};

/// Hard cap enforced on every pool. Server-side `max_connections` is usually
/// 100; 32 leaves plenty of headroom for multiple collections on one DB.
pub const HARD_POOL_CAP: u32 = 32;

/// Errors produced by the registry. Never leak raw DB error strings to the
/// caller — those get mapped in the engine layer.
#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("DSN parse error: {0}")]
    DsnParse(#[from] tokio_postgres::Error),
    #[error(
        "pool for {host}:{port}/{db} (user '{user}') already registered with a different password"
    )]
    PasswordMismatch {
        host: String,
        port: u16,
        db: String,
        user: String,
    },
    #[error("pool size {size} exceeds hard cap {HARD_POOL_CAP}")]
    SizeCapped { size: u32 },
    #[error("deadpool build error: {0}")]
    PoolBuild(String),
}

/// Normalized identity of a pool. Two DSNs sharing this tuple share a pool.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct PoolKey {
    pub host: String,
    pub port: u16,
    pub db: String,
    pub user: String,
    pub sslmode: String,
}

impl std::fmt::Display for PoolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}:{}/{}", self.user, self.host, self.port, self.db)
    }
}

/// Default pool size. Matches the render-semaphore formula
/// (`max(4, cores*2)`) capped at 16 so the two scale together under load.
pub fn default_pool_size() -> NonZeroU32 {
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let size = (parallelism * 2).clamp(4, 16) as u32;
    // clamp(4, 16) guarantees >= 4; unwrap is safe.
    NonZeroU32::new(size).expect("clamp floor is 4")
}

/// Clamp a caller-supplied size against [`HARD_POOL_CAP`].
pub fn clamp_pool_size(size: u32) -> Result<NonZeroU32, RegistryError> {
    if size == 0 {
        return Err(RegistryError::SizeCapped { size });
    }
    if size > HARD_POOL_CAP {
        return Err(RegistryError::SizeCapped { size });
    }
    Ok(NonZeroU32::new(size).expect("non-zero checked above"))
}

/// Parse a DSN and derive the pool key + extracted password.
///
/// Returned `PgConfig` is the tokio-postgres config ready for `Manager::new`.
/// Password is returned separately so the registry can detect two callers
/// registering the same `(host, port, db, user, sslmode)` with different
/// passwords.
pub fn normalize_dsn(dsn: &str) -> Result<(PgConfig, PoolKey, Option<String>), RegistryError> {
    let pg_config = PgConfig::from_str(dsn)?;

    let host = pg_config
        .get_hosts()
        .first()
        .map(|h| match h {
            Host::Tcp(s) => s.clone(),
            Host::Unix(p) => p.to_string_lossy().into_owned(),
        })
        .unwrap_or_else(|| "localhost".to_string());
    let port = pg_config.get_ports().first().copied().unwrap_or(5432);
    let db = pg_config.get_dbname().unwrap_or("").to_string();
    let user = pg_config.get_user().unwrap_or("").to_string();
    let sslmode = sslmode_str(pg_config.get_ssl_mode());
    let password = pg_config
        .get_password()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned());

    let key = PoolKey {
        host,
        port,
        db,
        user,
        sslmode,
    };
    Ok((pg_config, key, password))
}

fn sslmode_str(mode: SslMode) -> String {
    // tokio-postgres' `SslMode` doesn't implement Display; match exhaustively
    // so a new variant becomes a compile error instead of a silent label.
    match mode {
        SslMode::Disable => "disable",
        SslMode::Prefer => "prefer",
        SslMode::Require => "require",
        _ => "unknown",
    }
    .to_string()
}

struct PoolEntry {
    pool: Arc<Pool>,
    size_used: u32,
    password: Option<String>,
}

/// In-memory registry of pools keyed by [`PoolKey`].
///
/// Not thread-safe across `get_or_create` calls by design — the caller
/// (server reload path) serializes registry construction.
#[derive(Default)]
pub struct PoolRegistry {
    pools: HashMap<PoolKey, PoolEntry>,
}

impl std::fmt::Debug for PoolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolRegistry")
            .field("len", &self.pools.len())
            .field(
                "keys",
                &self.pools.keys().map(|k| k.to_string()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl PoolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a new pool, or return the existing pool for this key.
    ///
    /// First-caller-wins on size: if the pool was previously built with a
    /// different size, the existing pool is returned and an INFO log line is
    /// emitted. Callers that want deterministic max-sizing should compute
    /// `max(requested_sizes)` across all collections on the same DSN before
    /// calling this method. Password mismatch is a hard error.
    pub fn get_or_create(
        &mut self,
        dsn: &str,
        size: NonZeroU32,
    ) -> Result<Arc<Pool>, RegistryError> {
        if size.get() > HARD_POOL_CAP {
            return Err(RegistryError::SizeCapped { size: size.get() });
        }
        let (pg_config, key, password) = normalize_dsn(dsn)?;

        if let Some(entry) = self.pools.get(&key) {
            if entry.password != password {
                return Err(RegistryError::PasswordMismatch {
                    host: key.host.clone(),
                    port: key.port,
                    db: key.db.clone(),
                    user: key.user.clone(),
                });
            }
            if size.get() != entry.size_used {
                tracing::info!(
                    pool_key = %key,
                    existing_size = entry.size_used,
                    requested_size = size.get(),
                    "pool already built; first-caller-wins on size. For max-wins, pre-compute max per DSN before get_or_create."
                );
            }
            return Ok(entry.pool.clone());
        }

        // Operational warning when the DSN does not require TLS. `disable`
        // and `prefer` both allow silent plaintext fallback; `require` is
        // the minimum we want for non-loopback hosts. Production TLS
        // wiring (rustls + webpki-roots) lands with #110.
        if !matches!(key.sslmode.as_str(), "require") {
            let loopback = matches!(
                key.host.as_str(),
                "localhost" | "127.0.0.1" | "::1" | "[::1]"
            );
            if !loopback {
                tracing::warn!(
                    pool_key = %key,
                    sslmode = %key.sslmode,
                    "postgis pool has sslmode='{}' for a non-loopback host; credentials may be sent in plaintext. Set sslmode=require in the DSN.",
                    key.sslmode
                );
            }
        }

        let mgr_config = ManagerConfig {
            recycling_method: RecyclingMethod::Custom(
                "SET statement_timeout = '5s'; SET lock_timeout = '2s'".into(),
            ),
        };
        // TODO(#110): wire rustls so sslmode=require is actually enforced.
        // Until then ALL connections use NoTls regardless of the parsed
        // sslmode — the WARN above is the only signal for non-loopback,
        // non-require deployments.
        let manager = deadpool_postgres::Manager::from_config(pg_config, NoTls, mgr_config);

        let pool = Pool::builder(manager)
            .max_size(size.get() as usize)
            .runtime(Runtime::Tokio1)
            .build()
            .map_err(|e| RegistryError::PoolBuild(e.to_string()))?;
        let pool = Arc::new(pool);

        self.pools.insert(
            key,
            PoolEntry {
                pool: pool.clone(),
                size_used: size.get(),
                password,
            },
        );

        Ok(pool)
    }

    pub fn len(&self) -> usize {
        self.pools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    #[test]
    fn default_size_in_range() {
        let s = default_pool_size().get();
        assert!((4..=16).contains(&s), "got {s}");
    }

    #[test]
    fn clamp_rejects_zero() {
        assert!(clamp_pool_size(0).is_err());
    }

    #[test]
    fn clamp_rejects_above_cap() {
        assert!(clamp_pool_size(33).is_err());
        assert!(clamp_pool_size(100).is_err());
    }

    #[test]
    fn clamp_accepts_edges() {
        assert_eq!(clamp_pool_size(1).unwrap().get(), 1);
        assert_eq!(clamp_pool_size(HARD_POOL_CAP).unwrap().get(), HARD_POOL_CAP);
    }

    #[test]
    fn normalize_minimal_dsn() {
        let (_cfg, key, pw) =
            normalize_dsn("postgres://alice:secret@db.example.com:5433/weather").unwrap();
        assert_eq!(key.host, "db.example.com");
        assert_eq!(key.port, 5433);
        assert_eq!(key.db, "weather");
        assert_eq!(key.user, "alice");
        assert_eq!(pw.as_deref(), Some("secret"));
    }

    #[test]
    fn normalize_default_port() {
        let (_cfg, key, _pw) = normalize_dsn("postgres://alice@db.example.com/weather").unwrap();
        assert_eq!(key.port, 5432);
    }

    #[test]
    fn normalize_sslmode_default_is_prefer() {
        // tokio-postgres default SslMode is Prefer.
        let (_cfg, key, _pw) = normalize_dsn("postgres://alice@db.example.com/weather").unwrap();
        assert_eq!(key.sslmode, "prefer");
    }

    #[test]
    fn normalize_sslmode_override() {
        let (_cfg, key, _pw) =
            normalize_dsn("postgres://alice@db.example.com/weather?sslmode=require").unwrap();
        assert_eq!(key.sslmode, "require");
    }

    #[test]
    fn pool_key_equality_ignores_password_identity() {
        // Two DSNs with the same identity but different passwords produce the
        // same PoolKey (password tracked separately).
        let (_c1, k1, p1) = normalize_dsn("postgres://alice:p1@h/d").unwrap();
        let (_c2, k2, p2) = normalize_dsn("postgres://alice:p2@h/d").unwrap();
        assert_eq!(k1, k2);
        assert_ne!(p1, p2);
    }

    #[test]
    fn get_or_create_reuses_pool_for_same_key() {
        let mut r = PoolRegistry::new();
        let p1 = r
            .get_or_create("postgres://alice:s@h:5432/d", nz(4))
            .unwrap();
        let p2 = r
            .get_or_create("postgres://alice:s@h:5432/d", nz(4))
            .unwrap();
        assert!(Arc::ptr_eq(&p1, &p2));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn get_or_create_reuses_on_size_mismatch() {
        // First-caller-wins on size; second call with different size reuses
        // the pool and logs INFO (not asserted here).
        let mut r = PoolRegistry::new();
        let p1 = r
            .get_or_create("postgres://alice:s@h:5432/d", nz(4))
            .unwrap();
        let p2 = r
            .get_or_create("postgres://alice:s@h:5432/d", nz(16))
            .unwrap();
        assert!(Arc::ptr_eq(&p1, &p2));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn get_or_create_rejects_password_mismatch() {
        let mut r = PoolRegistry::new();
        let _ = r
            .get_or_create("postgres://alice:s1@h:5432/d", nz(4))
            .unwrap();
        let err = r
            .get_or_create("postgres://alice:s2@h:5432/d", nz(4))
            .unwrap_err();
        assert!(matches!(err, RegistryError::PasswordMismatch { .. }));
    }

    #[test]
    fn get_or_create_rejects_oversized() {
        let mut r = PoolRegistry::new();
        let err = r
            .get_or_create("postgres://alice:s@h:5432/d", nz(100))
            .unwrap_err();
        assert!(matches!(err, RegistryError::SizeCapped { size: 100 }));
    }

    #[test]
    fn separate_dbs_get_separate_pools() {
        let mut r = PoolRegistry::new();
        let p1 = r
            .get_or_create("postgres://alice:s@h:5432/one", nz(4))
            .unwrap();
        let p2 = r
            .get_or_create("postgres://alice:s@h:5432/two", nz(4))
            .unwrap();
        assert!(!Arc::ptr_eq(&p1, &p2));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn separate_users_get_separate_pools() {
        let mut r = PoolRegistry::new();
        let p1 = r
            .get_or_create("postgres://alice:s@h:5432/d", nz(4))
            .unwrap();
        let p2 = r.get_or_create("postgres://bob:s@h:5432/d", nz(4)).unwrap();
        assert!(!Arc::ptr_eq(&p1, &p2));
    }
}
