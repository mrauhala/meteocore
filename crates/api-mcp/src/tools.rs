//! MCP tools over MeteoCore's storm-cell intelligence.
//!
//! Scope is deliberately narrow (#605 follow-up, plan phase 3): the cells
//! surface plus enough collection metadata to discover it. Broad data access
//! (`query_position`, `query_area`) is a separate decision — each engine needs
//! its own bounds and cost controls before a model can reach it.
//!
//! **Why only nowcast collections.** Every tool that touches data resolves the
//! collection and rejects anything that is not `engine_type = "nowcast"`. That
//! is partly semantic (only nowcast serves tracked cells) and partly a
//! runtime-safety rule: nowcast's `FeatureEngine` reads an in-memory `ArcSwap`
//! snapshot, whereas a postgis `FeatureEngine` is a sync bridge over a
//! database. Calling the latter from an MCP handler would park a
//! request-serving worker (root CLAUDE.md rules 6/7). Widening the tool set
//! means solving that first, not just adding a match arm.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};

use ds_core::config::CollectionConfig;
use ds_core::feature::{DatetimeInterval, Feature, FeatureQuery, PropertyValue, SortKey};
use ds_core::feature_engine::FeatureEngine;

/// Collections that serve tracked storm cells.
const CELL_ENGINE_TYPE: &str = "nowcast";

/// Cap on cells returned by one call. A convective day carries ~170 tracked
/// cells; handing all of them to a model wastes context on the ones nobody
/// would look at. The ranking exists precisely so a small K is the right
/// answer.
const MAX_CELLS: usize = 50;
const DEFAULT_CELLS: usize = 10;

/// Cap on snapshots walked when reconstructing one cell's history. The engine
/// retains ~48 (4 h at 5-minute cadence), and each step materializes that
/// snapshot's whole cell set.
const MAX_TRACK_SAMPLES: usize = 48;
/// Hard cap on backward probes, which exceed the sample count when frames are
/// empty. Bounds the loop regardless of how quiet the radar was.
const MAX_TRACK_PROBES: usize = 200;
/// Cap on cells fetched per probed frame.
///
/// The whole frame is needed: `FeatureEngine` has no by-id query at a past
/// instant (`get_feature` serves only the latest snapshot), so finding one
/// cell in an older frame means materializing that frame and searching it.
/// Deliberately unlike `get_storm_cells`, which IS a bounded top-K. A
/// `get_feature(id, datetime)` would remove the asymmetry; until then the cap
/// keeps a pathological frame from being unbounded.
const MAX_CELLS_PER_PROBED_FRAME: usize = 1_000;
const DEFAULT_TRACK_SAMPLES: usize = 24;

#[derive(Clone)]
pub struct McpState {
    pub engines: HashMap<String, Arc<dyn FeatureEngine>>,
    pub collections: HashMap<String, CollectionConfig>,
}

impl McpState {
    /// Resolve a collection that actually serves cells, or explain why not.
    ///
    /// The error text is written for a model: it names the collections that
    /// would work, so a wrong guess self-corrects on the next call instead of
    /// turning into an apology to the user.
    fn cells_engine(&self, id: &str) -> Result<&Arc<dyn FeatureEngine>, ErrorData> {
        let Some(config) = self.collections.get(id) else {
            return Err(ErrorData::invalid_params(
                format!(
                    "Unknown collection '{id}'. Collections serving storm cells: {}. \
                     (A collection is only visible here if its `apis` includes \"features\".)",
                    self.cell_collection_ids().join(", ")
                ),
                None,
            ));
        };
        if config.engine_type != CELL_ENGINE_TYPE {
            return Err(ErrorData::invalid_params(
                format!(
                    "Collection '{id}' does not serve storm cells (engine type '{}'). \
                     Collections that do: {}",
                    config.engine_type,
                    self.cell_collection_ids().join(", ")
                ),
                None,
            ));
        }
        self.engines.get(id).ok_or_else(|| {
            ErrorData::internal_error(format!("Collection '{id}' is not available"), None)
        })
    }

    fn cell_collection_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .collections
            .values()
            .filter(|c| c.engine_type == CELL_ENGINE_TYPE)
            .map(|c| c.id.as_str())
            .collect();
        ids.sort_unstable();
        ids
    }
}

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

// deny_unknown_fields on all three: a model guessing `count` for `limit` or
// `time` for `at` would otherwise silently get the default, which is the
// silently-wrong-answer failure this crate is built to avoid.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CollectionParam {
    /// Collection id, e.g. "fmi-radar-nowcast".
    pub collection: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StormCellsParams {
    /// Collection id serving tracked storm cells.
    pub collection: String,
    /// How many cells to return (default 10, max 50).
    pub limit: Option<usize>,
    /// RFC 3339 instant. Returns the cell situation at the newest analysis
    /// frame at or before this time. Omit for the latest frame.
    pub at: Option<String>,
    /// Property to order by. Omit for significance, which is almost always
    /// what you want. Must be one of the collection's sortable_properties
    /// (get_collection_info lists them); anything else is an error naming the
    /// valid options rather than a silently different ordering.
    pub sort_by: Option<String>,
    /// "desc" (default) or "asc". Ignored unless sort_by is given.
    pub order: Option<String>,
    /// Drop cells below this significance, 0..=1. Applied after ordering, so
    /// it narrows the result rather than changing what ranks first.
    pub min_significance: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CellTrackParams {
    /// Collection id serving tracked storm cells.
    pub collection: String,
    /// Cell track id, as returned by get_storm_cells.
    pub cell_id: String,
    /// How many past analysis frames to walk (default 24, max 48).
    pub samples: Option<usize>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MeteoCoreMcp {
    state: Arc<arc_swap::ArcSwap<McpState>>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[tool_router]
impl MeteoCoreMcp {
    pub fn new(state: Arc<arc_swap::ArcSwap<McpState>>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List MeteoCore collections. Marks which ones serve tracked storm cells, \
                       which are the only ones the storm-cell tools accept."
    )]
    fn list_collections(&self) -> Result<String, ErrorData> {
        let state = self.state.load();
        let mut items: Vec<Value> = state
            .collections
            .values()
            .map(|c| {
                json!({
                    "id": c.id,
                    "title": c.title,
                    "engine_type": c.engine_type,
                    "serves_storm_cells": c.engine_type == CELL_ENGINE_TYPE,
                })
            })
            .collect();
        items.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        Ok(json!({
            "collections": items,
            "storm_cell_collections": state.cell_collection_ids(),
        })
        .to_string())
    }

    #[tool(
        description = "Describe one collection: title, description, and — for storm-cell \
                       collections — how many cells are tracked right now and the time range \
                       of retained analysis frames."
    )]
    fn get_collection_info(
        &self,
        Parameters(CollectionParam { collection }): Parameters<CollectionParam>,
    ) -> Result<String, ErrorData> {
        let state = self.state.load();
        let Some(config) = state.collections.get(&collection) else {
            return Err(ErrorData::invalid_params(
                format!("Unknown collection '{collection}'"),
                None,
            ));
        };
        let mut doc = json!({
            "id": config.id,
            "title": config.title,
            "description": config.description,
            "engine_type": config.engine_type,
            "serves_storm_cells": config.engine_type == CELL_ENGINE_TYPE,
        });
        // Engine methods ONLY for cells collections. `feature_count()` on a
        // postgis engine issues a COUNT against the database — a sync bridge
        // called from a request handler, which parks a worker (Critical Rule
        // 7). The module doc claims every data-touching tool gates on this;
        // it has to be true here too, not just in the cell tools.
        if config.engine_type == CELL_ENGINE_TYPE {
            if let Some(engine) = state.engines.get(&collection) {
                doc["tracked_cells"] = json!(engine.feature_count());
                if let Some((start, end)) = engine.temporal_extent() {
                    doc["retained_frames"] = json!({
                        "from": rfc3339(start),
                        "to": rfc3339(end),
                    });
                }
                if !engine.sortables().is_empty() {
                    doc["sortable_properties"] = json!(engine.sortables());
                }
            }
        }
        Ok(doc.to_string())
    }

    #[tool(
        description = "Tracked storm cells at one analysis frame, most significant first. \
                       Significance combines radar intensity, size, trend, lightning and \
                       impact on populated areas — it is a ranking heuristic, NOT an official \
                       warning. Each cell carries the reasons it ranked where it did."
    )]
    fn get_storm_cells(
        &self,
        Parameters(StormCellsParams {
            collection,
            limit,
            at,
            sort_by,
            order,
            min_significance,
        }): Parameters<StormCellsParams>,
    ) -> Result<String, ErrorData> {
        let state = self.state.load();
        let engine = state.cells_engine(&collection)?;
        let limit = match limit {
            // Coercing 0 to 1 would hand back a cell to a model that asked
            // for none — this crate's whole error style is "say what was
            // wrong so the next call is right".
            Some(0) => {
                return Err(ErrorData::invalid_params(
                    format!("limit must be between 1 and {MAX_CELLS}"),
                    None,
                ))
            }
            Some(n) => n.min(MAX_CELLS),
            None => DEFAULT_CELLS,
        };
        // Validated against what the engine can actually order by, and the
        // error names the alternatives — an unknown key must not degrade to a
        // different-but-plausible ordering (#605, #630).
        let sortby = match sort_by.as_deref() {
            None => vec![SortKey::descending("significance")],
            Some(key) => {
                let sortables = engine.sortables();
                if !sortables.contains(&key) {
                    return Err(ErrorData::invalid_params(
                        format!(
                            "Cannot sort by '{key}' on collection '{collection}'. \
                             Sortable properties: {}",
                            sortables.join(", ")
                        ),
                        None,
                    ));
                }
                match order.as_deref() {
                    None | Some("desc") => vec![SortKey::descending(key)],
                    Some("asc") => vec![SortKey::ascending(key)],
                    Some(other) => {
                        return Err(ErrorData::invalid_params(
                            format!("order must be \"asc\" or \"desc\", got '{other}'"),
                            None,
                        ))
                    }
                }
            }
        };
        if let Some(min) = min_significance {
            if !(0.0..=1.0).contains(&min) {
                return Err(ErrorData::invalid_params(
                    format!("min_significance must be between 0 and 1, got {min}"),
                    None,
                ));
            }
        }
        let requested_at = at.as_deref().map(parse_instant).transpose()?;
        let datetime = requested_at.map(|t| {
            // Newest frame at or before `at` — the same "which frame am I
            // looking at" semantic the Features endpoint uses.
            DatetimeInterval {
                start: None,
                end: Some(t),
            }
        });

        // One bounded call: ranking is server-side now (#605), so top-K does
        // not mean fetching every cell and sorting here.
        let page = engine
            .get_features(&FeatureQuery {
                bbox: None,
                limit,
                offset: 0,
                datetime,
                sortby,
            })
            .map_err(query_failed)?;

        // Applied AFTER the bounded page, so it narrows what was returned
        // rather than reaching deeper into the ranking. The count of what it
        // removed is reported: a model that asked for 10 and got 3 must be
        // able to tell "only 3 cells exist" from "7 were below your floor".
        let (cells, below_floor) = match min_significance {
            None => (page.features.clone(), 0usize),
            Some(min) => {
                let kept: Vec<_> = page
                    .features
                    .iter()
                    .filter(|f| {
                        f.properties
                            .get("significance")
                            .and_then(|v| v.as_f64())
                            .is_some_and(|v| v >= min)
                    })
                    .cloned()
                    .collect();
                let removed = page.features.len() - kept.len();
                (kept, removed)
            }
        };

        let observed = page
            .features
            .first()
            .and_then(|f| f.properties.get("observed"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        // Compare the REQUESTED instant against the retained window. Deriving
        // this from an empty page would be wrong: engine-nowcast retains a
        // snapshot for every generation even when it tracked zero cells, so
        // an empty page means "quiet frame" as often as "no frame at all",
        // and a model reading either as "no storms" states what it does not
        // know.
        let retained = engine.temporal_extent();
        // The retained window is ALWAYS published, not only when a request
        // fell outside it. It was previously part of the out-of-range
        // explanation, which meant a documented field read `null` in every
        // successful response — leaving a client no way to know how far back
        // it may ask without first asking wrongly.
        let retained_frames =
            retained.map(|(start, end)| json!({ "from": rfc3339(start), "to": rfc3339(end) }));
        let outside_retention = matches!(
            (requested_at, retained),
            (Some(t), Some((start, _))) if t < start
        );

        Ok(json!({
            "collection": collection,
            "observed": observed,
            "no_frame_for_requested_time": outside_retention,
            "retained_frames": retained_frames,
            "returned": cells.len(),
            "total_tracked": page.number_matched,
            // Present only when a floor was applied, so its absence cannot be
            // read as "nothing was filtered" on a call that set no floor.
            "below_min_significance": min_significance.map(|_| below_floor),
            "cells": cells.iter().map(cell_json).collect::<Vec<_>>(),
            "note": "Ranking heuristic, not an official warning. Issued warnings come from \
                     the CAP alert collections.",
        })
        .to_string())
    }

    #[tool(
        description = "One cell's history: its properties at each retained analysis frame, \
                       newest first. Shows how it moved and whether it intensified. Cells are \
                       analysis-only — this never returns future positions."
    )]
    fn get_cell_track(
        &self,
        Parameters(CellTrackParams {
            collection,
            cell_id,
            samples,
        }): Parameters<CellTrackParams>,
    ) -> Result<String, ErrorData> {
        let state = self.state.load();
        let engine = state.cells_engine(&collection)?;
        let samples = match samples {
            Some(0) => {
                return Err(ErrorData::invalid_params(
                    format!("samples must be between 1 and {MAX_TRACK_SAMPLES}"),
                    None,
                ))
            }
            Some(n) => n.min(MAX_TRACK_SAMPLES),
            None => DEFAULT_TRACK_SAMPLES,
        };

        let Some((extent_start, extent_end)) = engine.temporal_extent() else {
            return Ok(json!({
                "collection": collection,
                "cell_id": cell_id,
                "history": [],
                // Explicit null, not an omitted key. Both cell tools carry
                // this key on every response so a client can read the field
                // the same way each time; dropping it here would make key
                // presence mean something on one path and nothing on the
                // other.
                "retained_frames": Value::Null,
                "note": "No analysis frames are retained yet.",
            })
            .to_string());
        };

        // Walk snapshots backward by asking for "newest frame at or before
        // t", then stepping to just before whatever that frame's instant was.
        // No cadence is assumed — the engine's own retention decides the
        // steps, so a source that changes interval still walks correctly.
        // A frame with no cells at all (a quiet radar period) carries no
        // `observed` to step from, so the walk needs its own probe budget:
        // stepping back a minute and retrying finds the next older frame
        // instead of stopping and silently reporting a truncated history.
        // `samples` bounds frames WALKED, which is what the parameter says it
        // does. Counting only frames that contained the cell would let a
        // small `samples` be consumed by frames the cell is simply absent
        // from, and report "not tracked" for a cell that is two frames older.
        // Empty frames get their own allowance so they cannot do that either.
        let mut history = Vec::new();
        let mut cursor = extent_end;
        let mut frames = 0;
        let mut empty_probes = 0;
        // Option, so a reason set on the way out survives: `frames += 1`
        // happens before the boundary check, so a walk that reaches retention
        // start on its samples-th frame would otherwise be relabelled.
        let mut stopped: Option<&str> = None;
        while frames < samples && empty_probes < MAX_TRACK_PROBES {
            let page = engine
                .get_features(&FeatureQuery {
                    bbox: None,
                    limit: MAX_CELLS_PER_PROBED_FRAME,
                    offset: 0,
                    datetime: Some(DatetimeInterval {
                        start: None,
                        end: Some(cursor),
                    }),
                    sortby: Vec::new(),
                })
                .map_err(query_failed)?;

            let frame_time = page
                .features
                .first()
                .and_then(|f| f.properties.get("observed"))
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc));

            let Some(frame_time) = frame_time else {
                // Empty frame: probe further back rather than concluding the
                // history ends here. Does not count against `samples`.
                if cursor <= extent_start {
                    stopped = Some("reached_earliest_retained_frame");
                    break;
                }
                empty_probes += 1;
                cursor -= Duration::minutes(1);
                continue;
            };
            frames += 1;
            if let Some(f) = page.features.iter().find(|f| f.id == cell_id) {
                history.push(cell_json(f));
            }
            if frame_time <= extent_start {
                stopped = Some("reached_earliest_retained_frame");
                break;
            }
            cursor = frame_time - Duration::seconds(1);
        }
        let stopped = stopped.unwrap_or(if frames >= samples {
            "samples_reached"
        } else if empty_probes >= MAX_TRACK_PROBES {
            "gave_up_in_empty_gap"
        } else {
            "budget_exhausted"
        });

        Ok(json!({
            "collection": collection,
            "cell_id": cell_id,
            "frames_walked": frames,
            // Which exit happened. "gave_up_in_empty_gap" in particular must
            // not be read as "the cell stopped existing", and
            // "reached_earliest_retained_frame" deliberately does not claim a
            // retention POLICY limit: this layer cannot tell a full buffer
            // from a server that started an hour ago, and the old
            // "reached_retention_start" made short walks early in an
            // archive's life read as a policy boundary. Compare
            // `retained_frames.from` to see which it was.
            "stopped_because": stopped,
            "retained_frames": engine
                .temporal_extent()
                .map(|(start, end)| json!({ "from": rfc3339(start), "to": rfc3339(end) })),
            "history": history,
            "note": if history.is_empty() {
                "This cell id is not present in any retained frame. Track ids restart when the \
                 server reloads, so an id from an earlier session may no longer exist."
            } else {
                "Newest frame first. Analysis only — no forecast positions."
            },
        })
        .to_string())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MeteoCoreMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "MeteoCore weather radar server. Tracked storm cells are segmented from radar \
                 composites every ~5 minutes and ranked by significance, which combines radar \
                 intensity, size, trend, lightning and impact on populated areas.\n\n\
                 Significance is a ranking heuristic, not an official warning — never present \
                 it as one. Report only values present in the response: no rainfall rates, hail \
                 sizes or probabilities, none of which are in this data. A null property means \
                 unknown, not zero. Cells describe observed frames only and never forecast \
                 positions.",
        )
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generic client-facing error for an engine query failure.
///
/// Critical Rule 11: `DataServerError`'s Display carries `Storage(String)` and
/// `Io` detail — filesystem paths, backend messages — which must not reach a
/// client. api-features discards it the same way at the equivalent site; the
/// detail goes to the log instead, where it is actually useful.
fn query_failed(e: ds_core::error::DataServerError) -> ErrorData {
    tracing::error!("MCP feature query failed: {e}");
    ErrorData::internal_error("Query failed", None)
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn parse_instant(s: &str) -> Result<DateTime<Utc>, ErrorData> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| {
            ErrorData::invalid_params(
                format!("'{s}' is not an RFC 3339 instant (e.g. 2026-08-21T14:25:00Z): {e}"),
                None,
            )
        })
}

/// Project a cell feature into compact JSON.
///
/// Properties are passed through as the engine emitted them — including the
/// absent/null distinction, which carries meaning a model must not flatten:
/// absent means the source is not configured, null means it was not measured
/// this frame.
fn cell_json(f: &Feature) -> Value {
    let (lon, lat) = match &*f.geometry {
        ds_core::feature::Geometry::Point { x, y } => (*x, *y),
        other => other.centroid().unwrap_or((f64::NAN, f64::NAN)),
    };
    let mut props: Vec<(&String, &PropertyValue)> = f.properties.iter().collect();
    props.sort_by_key(|(k, _)| *k);
    let mut doc = serde_json::Map::new();
    doc.insert("id".into(), json!(f.id));
    doc.insert("lon".into(), json!(lon));
    doc.insert("lat".into(), json!(lat));
    for (k, v) in props {
        doc.insert(k.clone(), property_json(v));
    }
    Value::Object(doc)
}

fn property_json(v: &PropertyValue) -> Value {
    match v {
        PropertyValue::String(s) => json!(s),
        PropertyValue::Float(f) => json!(f),
        PropertyValue::Integer(i) => json!(i),
        PropertyValue::Bool(b) => json!(b),
        PropertyValue::Null => Value::Null,
        PropertyValue::List(items) => Value::Array(items.iter().map(property_json).collect()),
    }
}
