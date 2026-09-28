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

use chrono::{DateTime, Utc};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorData, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};

use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
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

/// Cap on retained frames walked when reconstructing one cell's history —
/// the engine retains 48 (4 h at 5-minute cadence), so the maximum walks all
/// of them. Each step is one by-id lookup in that frame, so the cap bounds
/// the response rather than the work.
const MAX_TRACK_SAMPLES: usize = 48;
const DEFAULT_TRACK_SAMPLES: usize = 24;
/// Cap on cells read from one frame when `get_storm_cells` has a
/// `min_significance` floor.
///
/// The engine's property filters are exact-match only, so a numeric floor
/// cannot ride in the query. It is applied in the handler before the page is
/// cut to `limit` (#652), which means reading past the page — no extra engine
/// work, since engine-nowcast builds every cell of a frame before paging. A
/// convective day carries ~170 cells, so this guards a pathological frame
/// rather than limiting a real one; past it, the note says the counts are
/// partial.
const MAX_CELLS_CHECKED_FOR_FLOOR: usize = 1_000;

const DISCLAIMER: &str = "Ranking heuristic, not an official warning. Issued warnings come from \
                          the CAP alert collections.";

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
    /// frame at or before this time. Omit for the latest frame. A time after
    /// the newest frame returns that frame and sets
    /// requested_time_after_newest_frame; cells are never extrapolated.
    pub at: Option<String>,
    /// Property to order by. Omit for significance, which is almost always
    /// what you want. Must be one of the collection's sortable_properties
    /// (get_collection_info lists them); anything else is an error naming the
    /// valid options rather than a silently different ordering.
    pub sort_by: Option<String>,
    /// "desc" (default) or "asc". Requires sort_by — setting it alone is an
    /// error, not a silent no-op.
    pub order: Option<String>,
    /// Only cells at or above this significance, 0..=1. Filters the whole
    /// frame before limit, so the page is the first `limit` qualifying cells
    /// in the requested order; matching_min_significance counts all of them.
    pub min_significance: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CellTrackParams {
    /// Collection id serving tracked storm cells.
    pub collection: String,
    /// Cell track id, as returned by get_storm_cells.
    pub cell_id: String,
    /// How many retained analysis frames to walk, newest first, counting
    /// frames with no cells too (default 24, max 48 — the whole ~4 h
    /// retention at 5-minute cadence).
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
                       warning. Each cell carries the reasons it ranked where it did, and \
                       `significance_contributions` gives every term's signed share of the \
                       score: a negative value (`clutter`, `weakening`) is a reason it ranked \
                       LOWER."
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
            Some(n) if (1..=MAX_CELLS).contains(&n) => n,
            // Coercing 0 to 1 would hand back a cell to a model that asked
            // for none, and clamping 200 to 50 would hand back a page it did
            // not ask for with nothing saying so (#652) — this crate's whole
            // error style is "say what was wrong so the next call is right".
            Some(_) => {
                return Err(ErrorData::invalid_params(
                    format!("limit must be between 1 and {MAX_CELLS}"),
                    None,
                ))
            }
            None => DEFAULT_CELLS,
        };
        // Validated against what the engine can actually order by, and the
        // error names the alternatives — an unknown key must not degrade to a
        // different-but-plausible ordering (#605, #630).
        let sortby = match sort_by.as_deref() {
            // `order` alone cannot be honoured: there is nothing to order by
            // except the default, and silently returning that default is the
            // "plausible-but-different ordering with no way to notice"
            // failure this whole parameter set exists to prevent. The caller
            // asked for ascending and would have received descending.
            None if order.is_some() => {
                return Err(ErrorData::invalid_params(
                    "order requires sort_by — on its own it has nothing to apply to. \
                     Pass sort_by with one of the collection's sortable_properties, \
                     or omit both for most-significant-first."
                        .to_string(),
                    None,
                ))
            }
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
        // not mean fetching every cell and sorting here. A significance floor
        // reads the whole frame instead (bounded), because it is applied
        // below rather than in the query.
        let page = engine
            .get_features(&FeatureQuery {
                bbox: None,
                limit: if min_significance.is_some() {
                    MAX_CELLS_CHECKED_FOR_FLOOR
                } else {
                    limit
                },
                offset: 0,
                datetime,
                sortby,
                property_filters: Vec::new(),
            })
            .map_err(query_failed)?;

        // The floor applies BEFORE the page is cut to `limit`, so "every cell
        // at or above 0.3" is answerable when more than `limit` qualify
        // (#652). Filtering the page instead — as this first did — returned
        // only the qualifying cells that happened to land in it, and with
        // `sort_by` could return none while qualifying cells existed. Both
        // counts cover the frame, not the page: how many qualify in all, even
        // past `limit`, and how many did not.
        let (cells, floor_counts): (Vec<&Feature>, _) = match min_significance {
            None => (page.features.iter().collect(), None),
            Some(min) => {
                let qualifying: Vec<&Feature> = page
                    .features
                    .iter()
                    .filter(|f| {
                        f.properties
                            .get("significance")
                            .and_then(|v| v.as_f64())
                            .is_some_and(|v| v >= min)
                    })
                    .collect();
                let matching = qualifying.len();
                let below = page.features.len() - matching;
                let page_of_qualifying = qualifying.into_iter().take(limit).collect();
                (page_of_qualifying, Some((matching, below)))
            }
        };
        // Cells past the read cap were never checked against the floor.
        let floor_unchecked = match min_significance {
            Some(_) => page.number_matched.saturating_sub(page.features.len()),
            None => 0,
        };

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
        // The engine resolves a future `at` to the newest frame, silently
        // (#652): a model asking for 09:00 got 08:25 with only `observed` <
        // `at` as a hint, and could present it as the 09:00 situation.
        let after_newest_frame = matches!(
            (requested_at, retained),
            (Some(t), Some((_, end))) if t > end
        );

        // The frame served. A cell carries it; a quiet frame has no cell to
        // ask, but when the request resolved to the newest frame (no `at`, or
        // one at or after it) that frame is the retained window's end, so
        // even an empty answer names the frame it describes. (Two snapshot
        // loads: a generation landing between them could skew this by one
        // frame, which a ~5-minute cadence makes negligible.)
        let observed = page
            .features
            .first()
            .and_then(|f| f.properties.get("observed"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| match retained {
                Some((_, end)) if requested_at.is_none_or(|t| t >= end) => Some(rfc3339(end)),
                _ => None,
            });

        Ok(json!({
            "collection": collection,
            "observed": observed,
            "no_frame_for_requested_time": outside_retention,
            "requested_time_after_newest_frame": after_newest_frame,
            "retained_frames": retained_frames,
            "returned": cells.len(),
            "total_tracked": page.number_matched,
            // Both null unless a floor was applied, so a call that set no
            // floor cannot read as "nothing was filtered".
            "matching_min_significance": floor_counts.map(|(matching, _)| matching),
            "below_min_significance": floor_counts.map(|(_, below)| below),
            "cells": cells.into_iter().map(cell_json).collect::<Vec<_>>(),
            "note": storm_cells_note(
                observed.as_deref().filter(|_| after_newest_frame),
                floor_unchecked,
                page.number_matched,
            ),
        })
        .to_string())
    }

    #[tool(
        description = "One cell's history: its properties at each retained analysis frame, \
                       newest first. Shows how it moved and whether it intensified. Walks the \
                       newest `samples` retained frames, quiet ones included, looking the id up \
                       in each; `stopped_because` says whether older retained frames were left \
                       unread. Cells are analysis-only — this never returns future positions."
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
            Some(n) if (1..=MAX_TRACK_SAMPLES).contains(&n) => n,
            // Out of range either way is an error, never a silent clamp (#652).
            Some(_) => {
                return Err(ErrorData::invalid_params(
                    format!("samples must be between 1 and {MAX_TRACK_SAMPLES}"),
                    None,
                ))
            }
            None => DEFAULT_TRACK_SAMPLES,
        };

        // The engine's own frame instants, oldest first: the walk steps from
        // one retained frame to the next (#646). No cadence is assumed, and
        // a frame with no cells is an ordinary step. The one-minute probing
        // this replaces could not see a quiet frame at all, so its probe
        // budget gave up behind ~200 minutes of them — short of retention.
        let frame_times = engine.available_times();
        let (Some(&first), Some(&last)) = (frame_times.first(), frame_times.last()) else {
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

        // `samples` bounds frames WALKED, which is what the parameter says it
        // does. Counting only frames that contained the cell would let a
        // small `samples` be consumed by frames the cell is simply absent
        // from, and report "not tracked" for a cell that is two frames older.
        // Each frame is a by-id lookup rather than a page of its cells, so a
        // crowded frame cannot hide the cell past a page cap.
        let mut history = Vec::new();
        let mut frames = 0;
        for &frame_time in frame_times.iter().rev().take(samples) {
            frames += 1;
            let frame = DatetimeInterval {
                start: Some(frame_time),
                end: Some(frame_time),
            };
            match engine.get_feature_at(&cell_id, &frame) {
                Ok(f) => history.push(cell_json(&f)),
                // Absent from this frame (or the frame aged out since the
                // list was read — equally a frame without the cell).
                Err(DataServerError::FeatureNotFound(_)) => {}
                Err(e) => return Err(query_failed(e)),
            }
        }
        // "reached_earliest_retained_frame" deliberately does not claim a
        // retention POLICY limit: this layer cannot tell a full buffer from a
        // server that started an hour ago, and the old
        // "reached_retention_start" made short walks early in an archive's
        // life read as a policy boundary. Compare `retained_frames.from` to
        // see which it was.
        let stopped = if frames == frame_times.len() {
            "reached_earliest_retained_frame"
        } else {
            "samples_reached"
        };

        Ok(json!({
            "collection": collection,
            "cell_id": cell_id,
            "frames_walked": frames,
            "stopped_because": stopped,
            // From the same frame list the walk used, so `from` is exactly
            // the frame a "reached_earliest_retained_frame" walk ended on.
            "retained_frames": { "from": rfc3339(first), "to": rfc3339(last) },
            "note": track_note(history.is_empty(), stopped, frames),
            "history": history,
        })
        .to_string())
    }
}

/// `get_storm_cells`' note: anything about this answer a model could misread,
/// then the disclaimer, which every response repeats.
fn storm_cells_note(
    newest_frame_for_future_at: Option<&str>,
    floor_unchecked: usize,
    total: usize,
) -> String {
    let mut note = String::new();
    if let Some(observed) = newest_frame_for_future_at {
        note.push_str(&format!(
            "The requested time is after the newest analysis frame, so this is that frame, \
             observed {observed}, not the situation at the requested time. Cells are never \
             extrapolated forward. "
        ));
    }
    if floor_unchecked > 0 {
        note.push_str(&format!(
            "Only the first {} of {total} cells in this order were checked against \
             min_significance; its counts cover those. ",
            total - floor_unchecked
        ));
    }
    note.push_str(DISCLAIMER);
    note
}

/// What an empty `get_cell_track` walk means, from how it ended (#646). Only
/// a walk that reached the earliest retained frame may say the id is not
/// retained at all.
fn track_note(empty: bool, stopped: &str, frames: usize) -> String {
    if !empty {
        return "Newest frame first. Analysis only — no forecast positions.".to_string();
    }
    match stopped {
        "reached_earliest_retained_frame" => {
            "This cell id is not present in any retained frame. Track ids restart when the \
             server reloads, so an id from an earlier session may no longer exist."
                .to_string()
        }
        _ => format!(
            "The cell id is not in the newest {frames} frames walked; older retained frames \
             were not read. Raise `samples` to look further back."
        ),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MeteoCoreMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
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
        PropertyValue::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), property_json(v)))
                .collect(),
        ),
    }
}
