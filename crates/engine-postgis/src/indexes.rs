//! Conservative catalog diagnostics for the access paths emitted by query.rs.
//! These recognize common indexes, not optimal plans or all usable expressions.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use deadpool_postgres::{Client, Pool};
use tokio::sync::OnceCell;
use tokio::time::{timeout_at, Instant};

use crate::catalog::{AuditClient, AuditError, AUDIT_TIMEOUT};
use crate::config::PostgisEngineConfig;
use crate::schema::{LocationSource, ObservationSchema, QualifiedTable};
use crate::security::quote_ident;

const MAX_INDEX_KEYS: i64 = 4096;

pub(crate) struct IndexAudit {
    mappings: Vec<Mapping>,
    completed: OnceCell<()>,
    unchecked_logged: AtomicBool,
    warnings: Arc<Mutex<HashSet<Diagnostic>>>,
}

impl IndexAudit {
    pub(crate) fn new(cfg: &PostgisEngineConfig, pool: &Arc<Pool>) -> Self {
        Self {
            mappings: mappings(cfg),
            completed: OnceCell::new(),
            unchecked_logged: AtomicBool::new(false),
            warnings: warnings_for_pool(pool),
        }
    }

    pub(crate) async fn check(&self, collection: &str, pool: &Pool) {
        let result = self
            .completed
            .get_or_try_init(|| async {
                let findings = inspect(pool, &self.mappings).await?;
                let mut seen = self.warnings.lock().expect("index warning lock");
                for finding in findings {
                    let diagnostic = finding.diagnostic;
                    if diagnostic.verdict == Verdict::Found || !seen.insert(diagnostic.clone()) {
                        continue;
                    }
                    let suggestion = diagnostic
                        .path
                        .suggestion(&finding.table, diagnostic.verdict == Verdict::Missing);
                    match diagnostic.verdict {
                        Verdict::Missing => tracing::warn!(
                            collection = %collection, schema = %finding.table.schema,
                            relation = %finding.table.table, query_path = diagnostic.path.name(),
                            suggestion = %suggestion,
                            "postgis: missing query index (no recognized usable access path)"
                        ),
                        Verdict::NotAssessed(reason) => tracing::info!(
                            collection = %collection, schema = %finding.table.schema,
                            relation = %finding.table.table, query_path = diagnostic.path.name(),
                            reason, suggestion = %suggestion,
                            "postgis: query index not assessed; investigate query plans"
                        ),
                        Verdict::Found => unreachable!(),
                    }
                }
                tracing::info!(collection = %collection,
                "postgis: index audit completed (catalog heuristics, not a query-plan guarantee)");
                Ok::<_, AuditError>(())
            })
            .await;
        if let Err(error) = result {
            if !self.unchecked_logged.swap(true, Ordering::Relaxed) {
                tracing::warn!(collection = %collection, error = ?error,
                    "postgis: index audit unchecked; will retry after recovery (collection availability unchanged)");
            }
        }
    }
}

// Keep warning identity across incremental reloads without retaining pools or
// caching catalog results. A changed mapping always performs a fresh audit.
fn warnings_for_pool(pool: &Arc<Pool>) -> Arc<Mutex<HashSet<Diagnostic>>> {
    type Warnings = Mutex<HashSet<Diagnostic>>;
    type Entry = (Weak<Pool>, Weak<Warnings>);
    static REGISTRY: OnceLock<Mutex<HashMap<usize, Entry>>> = OnceLock::new();
    let mut registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .expect("index registry lock");
    registry.retain(|_, (p, w)| p.strong_count() != 0 && w.strong_count() != 0);
    let key = Arc::as_ptr(pool) as usize;
    if let Some(warnings) = registry.get(&key).and_then(|(_, w)| w.upgrade()) {
        return warnings;
    }
    let warnings = Arc::new(Mutex::new(HashSet::new()));
    registry.insert(key, (Arc::downgrade(pool), Arc::downgrade(&warnings)));
    warnings
}

struct Mapping {
    table: QualifiedTable,
    paths: Vec<AccessPath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum AccessPath {
    StationTime {
        station: String,
        time: String,
        parameter: Option<String>,
    },
    Time(String),
    Geometry(String),
    Geography(String),
}

impl AccessPath {
    fn name(&self) -> &'static str {
        match self {
            Self::StationTime { .. } => "station/time",
            Self::Time(_) => "event time",
            Self::Geometry(_) => "spatial geometry",
            Self::Geography(_) => "nearest-station geography",
        }
    }

    fn columns(&self) -> Vec<&str> {
        match self {
            Self::StationTime {
                station,
                time,
                parameter,
            } => {
                let mut columns = vec![station.as_str(), time.as_str()];
                columns.extend(parameter.as_deref());
                columns
            }
            Self::Time(c) | Self::Geometry(c) | Self::Geography(c) => vec![c],
        }
    }

    fn suggestion(&self, table: &QualifiedTable, missing: bool) -> String {
        // Suggestions are logs only. Apply the same identifier quoting as SQL
        // builders, including mixed-case/reserved identifiers; never execute DDL.
        let quoted = |s: &str| quote_ident(s).expect("validated mapping identifier");
        let relation = format!("{}.{}", quoted(&table.schema), quoted(&table.table));
        if !missing {
            return format!("Inspect query plans and underlying relation indexes for {relation}");
        }
        let candidate = match self {
            Self::StationTime { station, time, .. } => {
                format!("B-tree ({}, {})", quoted(station), quoted(time))
            }
            Self::Time(time) => format!("B-tree ({})", quoted(time)),
            Self::Geometry(geom) => format!("2D GiST/SP-GiST ({})", quoted(geom)),
            Self::Geography(geom) => format!("GiST (({}::geography))", quoted(geom)),
        };
        format!("Inspect query plans for {relation}; consider {candidate} if appropriate")
    }
}

fn mappings(cfg: &PostgisEngineConfig) -> Vec<Mapping> {
    let mut mappings: Vec<Mapping> = Vec::new();
    let mut add = |table: &QualifiedTable, path| {
        if let Some(mapping) = mappings.iter_mut().find(|m| m.table == *table) {
            if !mapping.paths.contains(&path) {
                mapping.paths.push(path);
            }
        } else {
            mappings.push(Mapping {
                table: table.clone(),
                paths: vec![path],
            });
        }
    };
    // Modes A/B select cached points in memory; no spatial index is needed for
    // that selection. Stations-only has both geometry and geography SQL paths.
    if let LocationSource::Stations(s) = &cfg.location_source {
        add(&s.table, AccessPath::Geometry(s.geom_col.clone()));
        add(&s.table, AccessPath::Geography(s.geom_col.clone()));
    }
    match &cfg.observations {
        ObservationSchema::Long(s) => add(
            &s.table,
            AccessPath::StationTime {
                station: s.station_fk_col.clone(),
                time: s.time_col.clone(),
                parameter: Some(s.param_col.clone()),
            },
        ),
        ObservationSchema::Wide(s) => add(
            &s.table,
            AccessPath::StationTime {
                station: s.station_fk_col.clone(),
                time: s.time_col.clone(),
                parameter: None,
            },
        ),
        ObservationSchema::PerParameter(s) => {
            for t in &s.tables {
                add(
                    &t.table,
                    AccessPath::StationTime {
                        station: t.station_fk_col.clone(),
                        time: t.time_col.clone(),
                        parameter: None,
                    },
                );
            }
        }
        ObservationSchema::Events(s) => {
            add(&s.table, AccessPath::Time(s.time_col.clone()));
            add(&s.table, AccessPath::Geometry(s.geom_col.clone()));
        }
    }
    mappings
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Verdict {
    Found,
    Missing,
    NotAssessed(&'static str),
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Diagnostic {
    oid: u32,
    path: AccessPath,
    verdict: Verdict,
}

struct Finding {
    table: QualifiedTable,
    diagnostic: Diagnostic,
}

struct Key {
    column: Option<String>, // None = expression; INCLUDE columns are never fetched.
    default_ops: bool,
    same_collation: bool,
    bbox_ops: bool,
}

struct Index {
    oid: u32,
    method: String,
    usable: bool,
    conditional: bool, // partial predicate or indcheckxmin horizon unknown
    keys: Vec<Key>,
}

async fn inspect(pool: &Pool, mappings: &[Mapping]) -> Result<Vec<Finding>, AuditError> {
    let deadline = Instant::now() + AUDIT_TIMEOUT;
    let client = timeout_at(deadline, pool.get())
        .await
        .map_err(|_| AuditError::Timeout)??;
    let mut client = AuditClient(Some(client));
    let findings = timeout_at(
        deadline,
        inspect_on(client.0.as_mut().expect("audit client"), mappings),
    )
    .await
    .map_err(|_| AuditError::Timeout)??;
    drop(client.0.take()); // explicit rollback succeeded, safe to recycle
    Ok(findings)
}

async fn inspect_on(client: &mut Client, mappings: &[Mapping]) -> Result<Vec<Finding>, AuditError> {
    let transaction = crate::catalog::read_only(client).await?;
    let mut findings = Vec::new();
    for mapping in mappings {
        let table = &mapping.table;
        let row = transaction
            .query_opt(
                "SELECT c.oid, c.relkind::text, c.relhassubclass,
                ARRAY(SELECT a.attname::text FROM pg_catalog.pg_attribute a
                      WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped)
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relname = $2",
                &[&table.schema, &table.table],
            )
            .await?
            .ok_or_else(|| {
                AuditError::MissingRelation(table.schema.clone(), table.table.clone())
            })?;
        let oid: u32 = row.get(0);
        let kind: String = row.get(1);
        let inherited: bool = row.get(2);
        let columns: Vec<String> = row.get(3);
        let mut indexes: Vec<Index> = Vec::new();
        let mut unassessed = None;
        if !matches!(kind.as_str(), "r" | "p" | "m") {
            unassessed =
                Some("view/foreign or unsupported relation; underlying indexes are not traced");
        } else {
            // Inspect logical parent indexes directly, never scan data/chunks.
            // pg_index.indisvalid on a partitioned parent covers its attached
            // partition indexes. indnkeyatts excludes INCLUDE-only columns.
            let rows = transaction.query(
                "SELECT i.indexrelid, am.amname::text,
                    i.indisvalid AND i.indisready AND i.indislive,
                    i.indpred IS NOT NULL OR i.indcheckxmin,
                    a.attname::text, COALESCE(opc.opcdefault AND ns.nspname = 'pg_catalog', false),
                    COALESCE(i.indcollation[k.pos] = a.attcollation, false),
                    EXISTS(SELECT 1 FROM pg_catalog.pg_amop ao
                           JOIN pg_catalog.pg_operator op ON op.oid = ao.amopopr
                           WHERE ao.amopfamily = opc.opcfamily AND ao.amoppurpose = 's'
                             AND op.oprname = '&&' AND op.oprleft = a.atttypid AND op.oprright = a.atttypid)
                 FROM pg_catalog.pg_index i
                 JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
                 JOIN pg_catalog.pg_am am ON am.oid = c.relam
                 CROSS JOIN LATERAL pg_catalog.generate_series(0, i.indnkeyatts - 1) AS k(pos)
                 LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = i.indkey[k.pos]
                 LEFT JOIN pg_catalog.pg_opclass opc ON opc.oid = i.indclass[k.pos]
                 LEFT JOIN pg_catalog.pg_namespace ns ON ns.oid = opc.opcnamespace
                 WHERE i.indrelid = $1 ORDER BY i.indexrelid, k.pos LIMIT $2",
                &[&oid, &(MAX_INDEX_KEYS + 1)],
            ).await?;
            if rows.len() > MAX_INDEX_KEYS as usize {
                unassessed = Some("catalog index-key limit exceeded");
            } else {
                for row in rows {
                    let index_oid: u32 = row.get(0);
                    if indexes.last().is_none_or(|i| i.oid != index_oid) {
                        indexes.push(Index {
                            oid: index_oid,
                            method: row.get(1),
                            usable: row.get(2),
                            conditional: row.get(3),
                            keys: Vec::new(),
                        });
                    }
                    indexes.last_mut().expect("index row").keys.push(Key {
                        column: row.get(4),
                        default_ops: row.get(5),
                        same_collation: row.get(6),
                        bbox_ops: row.get(7),
                    });
                }
            }
        }
        for path in &mapping.paths {
            let verdict = if let Some(reason) = unassessed {
                Verdict::NotAssessed(reason)
            } else if path
                .columns()
                .iter()
                .any(|c| !columns.iter().any(|name| name == c))
            {
                Verdict::NotAssessed("mapped column is absent from the catalog")
            } else {
                let verdict = assess(path, &indexes);
                if verdict == Verdict::Missing && (kind == "p" || inherited) {
                    Verdict::NotAssessed(
                        "no matching valid parent index; investigate partition/chunk indexes",
                    )
                } else {
                    verdict
                }
            };
            findings.push(Finding {
                table: table.clone(),
                diagnostic: Diagnostic {
                    oid,
                    path: path.clone(),
                    verdict,
                },
            });
        }
    }
    transaction.rollback().await?;
    Ok(findings)
}

fn assess(path: &AccessPath, indexes: &[Index]) -> Verdict {
    let mut uncertain = None;
    for index in indexes.iter().filter(|i| i.usable) {
        let candidate = match path {
            AccessPath::StationTime {
                station,
                time,
                parameter,
            } if index.method == "btree" => {
                // Both ASC and DESC support equality/range access. This does
                // not certify that every ORDER BY avoids a sort. Long shape
                // also constrains parameter (= ANY), so allow that key before
                // station/time, but not an arbitrary unconstrained prefix.
                let mut station_seen = false;
                let mut candidate = Verdict::Missing;
                let mut compatible = true;
                for key in &index.keys {
                    compatible &= key.default_ops && key.same_collation;
                    match key.column.as_deref() {
                        Some(c) if c == station => station_seen = true,
                        Some(c) if Some(c) == parameter.as_deref() => {}
                        Some(c) if c == time && station_seen => {
                            candidate = if compatible {
                                Verdict::Found
                            } else {
                                Verdict::NotAssessed("operator class/collation")
                            };
                            break;
                        }
                        None => {
                            candidate = Verdict::NotAssessed("expression key");
                            break;
                        }
                        _ => break,
                    }
                }
                candidate
            }
            AccessPath::Time(time) if index.method == "btree" => match index.keys.first() {
                Some(k) if k.column.as_deref() == Some(time) => {
                    if k.default_ops && k.same_collation {
                        Verdict::Found
                    } else {
                        Verdict::NotAssessed("operator class/collation")
                    }
                }
                Some(k) if k.column.is_none() => Verdict::NotAssessed("expression key"),
                _ => Verdict::Missing,
            },
            AccessPath::Geometry(geom) if matches!(index.method.as_str(), "gist" | "spgist") => {
                if index
                    .keys
                    .iter()
                    .any(|k| k.column.as_deref() == Some(geom) && k.bbox_ops)
                {
                    Verdict::Found
                } else if index
                    .keys
                    .iter()
                    .any(|k| k.column.is_none() || k.column.as_deref() == Some(geom))
                {
                    Verdict::NotAssessed("spatial expression/operator class")
                } else {
                    Verdict::Missing
                }
            }
            AccessPath::Geography(_) if matches!(index.method.as_str(), "gist" | "spgist") => {
                if index.keys.iter().any(|k| k.column.is_none()) {
                    Verdict::NotAssessed("geography expression index")
                } else {
                    Verdict::Missing
                }
            }
            _ => {
                if !matches!(index.method.as_str(), "btree" | "hash" | "gist" | "spgist")
                    && index.keys.iter().any(|k| {
                        k.column.is_none()
                            || path.columns().contains(&k.column.as_deref().unwrap_or(""))
                    })
                {
                    Verdict::NotAssessed("alternative access method")
                } else {
                    Verdict::Missing
                }
            }
        };
        match candidate {
            Verdict::Found if !index.conditional => return Verdict::Found,
            Verdict::Found => uncertain = Some("partial predicate or snapshot horizon"),
            Verdict::NotAssessed(reason) => uncertain = Some(reason),
            Verdict::Missing => {}
        }
    }
    if let AccessPath::StationTime { station, time, .. } = path {
        let leading = |column: &str| {
            indexes.iter().any(|i| {
                i.usable
                    && i.method == "btree"
                    && i.keys
                        .first()
                        .is_some_and(|k| k.column.as_deref() == Some(column))
            })
        };
        let time_first = indexes.iter().any(|i| {
            i.usable
                && i.method == "btree"
                && i.keys
                    .first()
                    .is_some_and(|k| k.column.as_deref() == Some(time))
                && i.keys.iter().any(|k| k.column.as_deref() == Some(station))
        });
        // Reversed or separate keys can help bounded queries/bitmap plans,
        // but do not establish the station-first path for unbounded reads.
        if time_first || (leading(station) && leading(time)) {
            uncertain = Some("separate or time-first keys; investigate bitmap/range plans");
        }
    }
    uncertain
        .map(Verdict::NotAssessed)
        .unwrap_or(Verdict::Missing)
}
