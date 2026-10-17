//! Advisory privilege diagnostics. Inspect catalogs once per engine build,
//! retry an unchecked audit, and deduplicate warnings across a shared pool.
//! No probe writes, grant changes, or request-time checks.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use deadpool_postgres::{Client, Pool};
use tokio::sync::OnceCell;
use tokio::time::{timeout_at, Instant};

use crate::catalog::{AuditClient, AuditError, AUDIT_TIMEOUT};
use crate::config::PostgisEngineConfig;
use crate::schema::{ObservationSchema, QualifiedTable};

pub(crate) struct PrivilegeAudit {
    relations: Vec<QualifiedTable>,
    completed: OnceCell<()>,
    unchecked_logged: AtomicBool,
    warnings: Arc<Mutex<HashSet<Warning>>>,
}

impl PrivilegeAudit {
    pub(crate) fn new(cfg: &PostgisEngineConfig, pool: &Arc<Pool>) -> Self {
        Self {
            relations: mapped_relations(cfg),
            completed: OnceCell::new(),
            unchecked_logged: AtomicBool::new(false),
            warnings: warnings_for_pool(pool),
        }
    }

    /// Advisory: callers always keep their own metadata/health result. The
    /// successful OnceCell also prevents concurrent refreshes from re-auditing.
    pub(crate) async fn check(&self, collection: &str, pool: &Pool) {
        let result = self.completed.get_or_try_init(|| async {
            let report = inspect(pool, &self.relations).await?;
            self.log_warnings(collection, &report);
            tracing::info!(
                collection = %collection, role = %report.role, relations = report.relations.len(),
                superuser = report.superuser,
                write_privileges_found = report.relations.iter().any(|r| r.writes.any()),
                "postgis: privilege audit completed (mapped relations only; not a read-only guarantee)"
            );
            Ok::<_, AuditError>(())
        }).await;
        if let Err(error) = result {
            if !self.unchecked_logged.swap(true, Ordering::Relaxed) {
                // Never log a DSN: the audit uses the existing pool and only
                // retains catalog identifiers and database/pool errors.
                tracing::warn!(
                    collection = %collection, error = ?error,
                    "postgis: privilege audit unchecked; will retry after recovery (collection availability unchanged)"
                );
            }
        }
    }

    fn log_warnings(&self, collection: &str, report: &Report) {
        let mut seen = self.warnings.lock().expect("privilege warning lock");
        if report.superuser && seen.insert(Warning::Superuser(report.role_oid)) {
            tracing::warn!(
                collection = %collection, role = %report.role,
                "postgis: database role is a superuser; use a dedicated read-only login"
            );
        }
        for relation in &report.relations {
            if relation.writes.any()
                && seen.insert(Warning::Writes(
                    report.role_oid,
                    relation.oid,
                    relation.writes,
                ))
            {
                tracing::warn!(
                    collection = %collection, role = %report.role,
                    schema = %relation.table.schema, relation = %relation.table.table,
                    privileges = ?relation.writes.names(), owns_relation = relation.owner,
                    "postgis: database role has write privileges on a mapped relation (including table or column grants)"
                );
            }
        }
    }
}

/// Key by the actual shared pool, not a log label or a DSN containing a secret.
/// Engines retain the set across incremental reloads; dead pools leave no
/// permanent registry entries. A rebuilt mapping is audited afresh, while an
/// identical warning for a shared role/relation is emitted only once.
fn warnings_for_pool(pool: &Arc<Pool>) -> Arc<Mutex<HashSet<Warning>>> {
    type Warnings = Mutex<HashSet<Warning>>;
    type Entry = (Weak<Pool>, Weak<Warnings>);
    static REGISTRY: OnceLock<Mutex<HashMap<usize, Entry>>> = OnceLock::new();
    let mut registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .expect("privilege registry lock");
    registry.retain(|_, (pool, warnings)| pool.strong_count() != 0 && warnings.strong_count() != 0);
    let key = Arc::as_ptr(pool) as usize;
    if let Some(warnings) = registry
        .get(&key)
        .and_then(|(_, warnings)| warnings.upgrade())
    {
        return warnings;
    }
    let warnings = Arc::new(Mutex::new(HashSet::new()));
    registry.insert(key, (Arc::downgrade(pool), Arc::downgrade(&warnings)));
    warnings
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Writes {
    insert: bool,
    update: bool,
    delete: bool,
}

impl Writes {
    fn any(self) -> bool {
        self.insert || self.update || self.delete
    }

    fn names(self) -> Vec<&'static str> {
        [
            (self.insert, "INSERT"),
            (self.update, "UPDATE"),
            (self.delete, "DELETE"),
        ]
        .into_iter()
        .filter_map(|(held, name)| held.then_some(name))
        .collect()
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum Warning {
    Superuser(u32),
    Writes(u32, u32, Writes),
}

struct Relation {
    table: QualifiedTable,
    oid: u32,
    owner: bool,
    writes: Writes,
}

struct Report {
    role: String,
    role_oid: u32,
    superuser: bool,
    relations: Vec<Relation>,
}

async fn inspect(pool: &Pool, relations: &[QualifiedTable]) -> Result<Report, AuditError> {
    let deadline = Instant::now() + AUDIT_TIMEOUT;
    let client = timeout_at(deadline, pool.get())
        .await
        .map_err(|_| AuditError::Timeout)??;
    let mut client = AuditClient(Some(client));
    let report = timeout_at(
        deadline,
        inspect_on(client.0.as_mut().expect("audit client"), relations),
    )
    .await
    .map_err(|_| AuditError::Timeout)??;
    // inspect_on has explicitly rolled back: session settings are restored.
    drop(client.0.take());
    Ok(report)
}

async fn inspect_on(
    client: &mut Client,
    relations: &[QualifiedTable],
) -> Result<Report, AuditError> {
    let transaction = crate::catalog::read_only(client).await?;
    let role = transaction.query_one(
        "SELECT oid, rolname::text, rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user", &[],
    ).await?;
    let mut report = Report {
        role_oid: role.get(0),
        role: role.get(1),
        superuser: role.get(2),
        relations: Vec::with_capacity(relations.len()),
    };
    for table in relations {
        // Native privilege functions include effective membership, PUBLIC,
        // ownership's default ACL and table-wide or column-level grants.
        // Do not query pg_authid, probe writes, or inspect unmapped columns'
        // SELECT permissions: table-wide SELECT is the recommended setup.
        let row = transaction.query_opt(
            "SELECT c.oid, pg_catalog.pg_has_role(c.relowner, 'USAGE'),
                pg_catalog.has_table_privilege(c.oid, 'INSERT') OR pg_catalog.has_any_column_privilege(c.oid, 'INSERT'),
                pg_catalog.has_table_privilege(c.oid, 'UPDATE') OR pg_catalog.has_any_column_privilege(c.oid, 'UPDATE'),
                pg_catalog.has_table_privilege(c.oid, 'DELETE')
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
            &[&table.schema, &table.table],
        ).await?.ok_or_else(|| AuditError::MissingRelation(table.schema.clone(), table.table.clone()))?;
        report.relations.push(Relation {
            table: table.clone(),
            oid: row.get(0),
            owner: row.get(1),
            writes: Writes {
                insert: row.get(2),
                update: row.get(3),
                delete: row.get(4),
            },
        });
    }
    transaction.rollback().await?;
    Ok(report)
}

fn mapped_relations(cfg: &PostgisEngineConfig) -> Vec<QualifiedTable> {
    let mut tables = Vec::new();
    if let Some(stations) = cfg.location_source.stations() {
        tables.push(&stations.table);
    }
    match &cfg.observations {
        ObservationSchema::Long(s) => tables.push(&s.table),
        ObservationSchema::Wide(s) => tables.push(&s.table),
        ObservationSchema::PerParameter(s) => tables.extend(s.tables.iter().map(|t| &t.table)),
        ObservationSchema::Events(s) => tables.push(&s.table),
    }
    let mut seen = HashSet::new();
    tables
        .into_iter()
        .filter(|t| seen.insert((t.schema.as_str(), t.table.as_str())))
        .cloned()
        .collect()
}
