//! Read-only, bounded catalog work shared by advisory diagnostics.
use deadpool_postgres::Client;
use std::time::Duration;

pub(crate) const AUDIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub(crate) enum AuditError {
    #[error("pool acquisition failed: {0}")]
    Pool(#[from] deadpool_postgres::PoolError),
    #[error("catalog query failed: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("mapped relation not found: {0}.{1}")]
    MissingRelation(String, String),
    #[error("catalog audit exceeded its two-second deadline")]
    Timeout,
}

/// On timeout/error/cancellation discard the connection, never recycle an
/// unfinished transaction. ClientWrapper::drop aborts its connection driver.
pub(crate) struct AuditClient(pub(crate) Option<Client>);

impl Drop for AuditClient {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            drop(Client::take(client));
        }
    }
}

pub(crate) async fn read_only(
    client: &mut Client,
) -> Result<deadpool_postgres::Transaction<'_>, AuditError> {
    let transaction = client
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::ReadCommitted)
        .read_only(true)
        .start()
        .await?;
    // Transaction-local and no weaker than the role's existing timeout. The
    // server-side bound also stops work after a disconnected client times out.
    transaction.query_one(
        "SELECT pg_catalog.set_config('statement_timeout', LEAST(COALESCE(NULLIF(EXTRACT(EPOCH FROM current_setting('statement_timeout')::interval) * 1000, 0), 2000), 2000)::bigint::text, true)", &[],
    ).await?;
    Ok(transaction)
}
