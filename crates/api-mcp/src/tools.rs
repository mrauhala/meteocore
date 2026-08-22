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
                    "Unknown collection '{id}'. Collections serving storm cells: {}",
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CollectionParam {
    /// Collection id, e.g. "fmi-radar-nowcast".
    pub collection: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StormCellsParams {
    /// Collection id serving tracked storm cells.
    pub collection: String,
    /// How many cells to return, most significant first (default 10, max 50).
    pub limit: Option<usize>,
    /// RFC 3339 instant. Returns the cell situation at the newest analysis
    /// frame at or before this time. Omit for the latest frame.
    pub at: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
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
        }): Parameters<StormCellsParams>,
    ) -> Result<String, ErrorData> {
        let state = self.state.load();
        let engine = state.cells_engine(&collection)?;
        let limit = limit.unwrap_or(DEFAULT_CELLS).clamp(1, MAX_CELLS);
        let datetime = at.as_deref().map(parse_instant).transpose()?.map(|t| {
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
                sortby: vec![SortKey::descending("significance")],
            })
            .map_err(|e| ErrorData::internal_error(format!("Query failed: {e}"), None))?;

        let observed = page
            .features
            .first()
            .and_then(|f| f.properties.get("observed"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        Ok(json!({
            "collection": collection,
            "observed": observed,
            "returned": page.number_returned,
            "total_tracked": page.number_matched,
            "cells": page.features.iter().map(cell_json).collect::<Vec<_>>(),
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
        let samples = samples
            .unwrap_or(DEFAULT_TRACK_SAMPLES)
            .clamp(1, MAX_TRACK_SAMPLES);

        let Some((extent_start, extent_end)) = engine.temporal_extent() else {
            return Ok(json!({
                "collection": collection,
                "cell_id": cell_id,
                "history": [],
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
        let mut history = Vec::new();
        let mut cursor = extent_end;
        let max_probes = samples.saturating_mul(4).min(MAX_TRACK_PROBES);
        let mut probes = 0;
        while history.len() < samples && probes < max_probes {
            probes += 1;
            let page = engine
                .get_features(&FeatureQuery {
                    bbox: None,
                    limit: usize::MAX,
                    offset: 0,
                    datetime: Some(DatetimeInterval {
                        start: None,
                        end: Some(cursor),
                    }),
                    sortby: Vec::new(),
                })
                .map_err(|e| ErrorData::internal_error(format!("Query failed: {e}"), None))?;

            let frame_time = page
                .features
                .first()
                .and_then(|f| f.properties.get("observed"))
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc));

            let Some(frame_time) = frame_time else {
                // Empty frame: probe further back rather than concluding the
                // history ends here.
                if cursor <= extent_start {
                    break;
                }
                cursor -= Duration::minutes(1);
                continue;
            };
            if let Some(f) = page.features.iter().find(|f| f.id == cell_id) {
                history.push(cell_json(f));
            }
            if frame_time <= extent_start {
                break;
            }
            cursor = frame_time - Duration::seconds(1);
        }

        Ok(json!({
            "collection": collection,
            "cell_id": cell_id,
            "frames_walked": history.len(),
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
