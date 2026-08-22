//! `/mcp` end to end: the auth boundary first, then the tools.
//!
//! The auth tests matter more than the tool tests. A broken tool returns a
//! confusing answer; a broken guard publishes every collection to anyone who
//! finds the URL.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use api_mcp::{McpAuth, McpState};
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::feature::*;
use ds_core::feature_engine::FeatureEngine;

const TOKEN: &str = "test-token-abc123";
/// Deliberately not loopback. rmcp's default allowlist is localhost-only, so
/// testing through 127.0.0.1 passes while every request behind a real proxy
/// 403s — which is exactly how that shipped.
const HOST: &str = "meteocore.example.fi";
const BASE_URL: &str = "https://meteocore.example.fi";

struct CellEngine;

impl CellEngine {
    fn cell(id: &str, significance: f64, dbz: f64, observed: &str) -> Feature {
        let mut m = HashMap::new();
        m.insert("significance".into(), PropertyValue::Float(significance));
        m.insert("max_dbz".into(), PropertyValue::Float(dbz));
        m.insert("severity".into(), PropertyValue::String("severe".into()));
        m.insert(
            "observed".into(),
            PropertyValue::String(observed.to_string()),
        );
        // Present-but-null: "configured, not measured this frame". A client
        // that flattens this to false would state something untrue.
        m.insert("lightning_jump".into(), PropertyValue::Null);
        Feature {
            id: id.into(),
            geometry: Arc::new(Geometry::Point { x: 24.9, y: 60.2 }),
            properties: Arc::new(m),
        }
    }
}

impl FeatureEngine for CellEngine {
    fn sortables(&self) -> &[&'static str] {
        &["significance", "max_dbz"]
    }

    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        // Two frames; `datetime` end selects the newest at or before it.
        let newest = "2026-08-21T14:25:00Z";
        let older = "2026-08-21T14:20:00Z";
        let cutoff = query.datetime.as_ref().and_then(|d| d.end);
        let oldest = older.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
        // Like the real engine: an instant before the retained window matches
        // no snapshot and returns nothing at all.
        if cutoff.is_some_and(|t| t < oldest) {
            return Ok(FeaturePage {
                features: vec![],
                number_matched: 0,
                number_returned: 0,
                next_offset: None,
            });
        }
        let frame = match cutoff {
            Some(t) if t < newest.parse::<chrono::DateTime<chrono::Utc>>().unwrap() => older,
            _ => newest,
        };
        let mut all = vec![
            Self::cell("7", 0.31, 47.0, frame),
            Self::cell("42", 0.88, 58.0, frame),
            Self::cell("13", 0.55, 51.0, frame),
        ];
        // Cell "old-only" exists solely in the older frame, so a walk that
        // stops too early misses it.
        if frame == older {
            all.push(Self::cell("old-only", 0.42, 49.0, frame));
        }
        sort_features(&mut all, &query.sortby);
        let matched = all.len();
        let end = query.offset.saturating_add(query.limit).min(matched);
        let page = all[query.offset.min(matched)..end].to_vec();
        Ok(FeaturePage {
            number_returned: page.len(),
            features: page,
            number_matched: matched,
            next_offset: None,
        })
    }

    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        Err(DataServerError::FeatureNotFound(id.into()))
    }

    fn feature_count(&self) -> usize {
        3
    }

    fn temporal_extent(
        &self,
    ) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
        Some((
            "2026-08-21T14:20:00Z".parse().unwrap(),
            "2026-08-21T14:25:00Z".parse().unwrap(),
        ))
    }
}

fn collection(id: &str, engine_type: &str) -> CollectionConfig {
    CollectionConfig {
        id: id.to_string(),
        title: format!("{id} title"),
        description: "desc".into(),
        data_path: None,
        apis: vec!["features".to_string()],
        engine_type: engine_type.to_string(),
        keywords: Vec::new(),
        license: None,
        geotiff: None,
        querydata: None,
        wms: None,
        grib: None,
        zarr: None,
        odim: None,
        cap: None,
        postgis: None,
        nowcast: None,
        preview: None,
    }
}

fn app() -> axum::Router {
    let mut engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    engines.insert("cells".into(), Arc::new(CellEngine));
    engines.insert("places".into(), Arc::new(CellEngine));
    let mut collections = HashMap::new();
    collections.insert("cells".to_string(), collection("cells", "nowcast"));
    collections.insert("places".to_string(), collection("places", "geojson"));

    api_mcp::router(
        Arc::new(ArcSwap::from_pointee(McpState {
            engines,
            collections,
        })),
        Arc::new(McpAuth::new(TOKEN.to_string(), 0)),
        api_mcp::allowed_hosts(BASE_URL, &[]),
    )
}

/// One JSON-RPC call against a given app instance.
///
/// Takes the app rather than building one, so a session survives across calls
/// — MCP requires an `initialize` handshake before any other method, and a
/// fresh app each time would lose it.
async fn call(
    app: &axum::Router,
    token: Option<&str>,
    session: Option<&str>,
    body: Value,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json")
        // The transport requires Host (DNS-rebinding protection) — a request
        // without it is refused before reaching a tool.
        .header("host", HOST)
        .header("accept", "application/json, text/event-stream");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    if let Some(sid) = session {
        req = req.header("mcp-session-id", sid);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

/// Shorthand for the auth tests, which never get past the guard.
async fn rpc(token: Option<&str>, body: Value) -> (StatusCode, String) {
    let (s, _, b) = call(&app(), token, None, body).await;
    (s, b)
}

/// Complete the handshake and return the session id.
async fn handshake(app: &axum::Router) -> String {
    let (status, headers, body) = call(app, Some(TOKEN), None, initialize()).await;
    assert_eq!(status, StatusCode::OK, "initialize failed: {body}");
    let sid = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("initialize must return a session id")
        .to_string();
    // The spec requires the initialized notification before other methods.
    let (status, _, _) = call(
        app,
        Some(TOKEN),
        Some(&sid),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert!(status.is_success(), "initialized notification rejected");
    sid
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    })
}

// ---------------------------------------------------------------------------
// Auth boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_token_is_rejected() {
    let (status, body) = rpc(None, initialize()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        !body.contains("tools") && !body.contains("collection"),
        "an unauthenticated response must not leak anything about the server: {body}"
    );
}

#[tokio::test]
async fn a_wrong_token_is_rejected() {
    for wrong in [
        "",
        "test-token",         // prefix of the real one
        "test-token-abc1234", // real one plus a char
        "TEST-TOKEN-ABC123",  // case differs
        "completely-different",
    ] {
        let (status, _) = rpc(Some(wrong), initialize()).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "token {wrong:?} must not be accepted"
        );
    }
}

#[tokio::test]
async fn the_error_does_not_distinguish_missing_from_wrong() {
    // Distinguishable messages are a (small) oracle; a client that can't tell
    // still knows to check its credential.
    let (_, missing) = rpc(None, initialize()).await;
    let (_, wrong) = rpc(Some("nope"), initialize()).await;
    assert_eq!(missing, wrong);
}

#[tokio::test]
async fn the_rate_limit_refuses_beyond_the_window() {
    let auth = McpAuth::new(TOKEN.into(), 2);
    assert!(auth.limiter.allow());
    assert!(auth.limiter.allow());
    assert!(!auth.limiter.allow());
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_token_reaches_the_protocol() {
    let (status, body) = rpc(Some(TOKEN), initialize()).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        body.contains("protocolVersion"),
        "expected an initialize result: {body}"
    );
    // The instructions carry the hallucination guards, so their absence is a
    // real regression rather than cosmetic.
    assert!(
        body.contains("not an official warning"),
        "server instructions must warn against presenting significance as a warning: {body}"
    );
}

#[tokio::test]
async fn tools_are_advertised() {
    let app = app();
    let sid = handshake(&app).await;
    let (_, _, body) = call(
        &app,
        Some(TOKEN),
        Some(&sid),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    for tool in [
        "list_collections",
        "get_collection_info",
        "get_storm_cells",
        "get_cell_track",
    ] {
        assert!(body.contains(tool), "{tool} must be advertised: {body}");
    }
}

/// Unwrap a JSON-RPC response, which the transport frames as SSE when the
/// client accepts `text/event-stream` (as MCP clients must).
fn parse_rpc(body: &str) -> Value {
    let payload = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or(body);
    serde_json::from_str(payload).unwrap_or_else(|e| panic!("not JSON-RPC: {e}: {body}"))
}

async fn call_tool(app: &axum::Router, sid: &str, name: &str, args: Value) -> Value {
    let (status, _, body) = call(
        app,
        Some(TOKEN),
        Some(sid),
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
               "params": {"name": name, "arguments": args}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{name} failed: {body}");
    let doc: Value = parse_rpc(&body);
    let text = doc["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in {doc}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool output is not JSON: {e}: {text}"))
}

#[tokio::test]
async fn storm_cells_come_back_ranked_and_bounded() {
    let app = app();
    let sid = handshake(&app).await;
    let out = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells"}),
    )
    .await;

    let cells = out["cells"].as_array().expect("cells array");
    let scores: Vec<f64> = cells
        .iter()
        .map(|c| c["significance"].as_f64().unwrap_or_default())
        .collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "most significant first, got {scores:?}"
    );
    assert_eq!(cells[0]["id"], "42");
    assert_eq!(out["total_tracked"], 3);

    // A null property must survive as null. Flattening it to false would let
    // a model state "no lightning jump" about a frame where the join was
    // skipped.
    assert!(
        cells[0]["lightning_jump"].is_null(),
        "null must not be flattened: {}",
        cells[0]
    );

    // The response carries its own disclaimer, so a model summarizing one
    // cell in isolation still sees it.
    assert!(out["note"]
        .as_str()
        .unwrap_or_default()
        .contains("not an official warning"));

    // limit is honoured and clamped.
    let two = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "limit": 2}),
    )
    .await;
    assert_eq!(two["cells"].as_array().unwrap().len(), 2);
    assert_eq!(
        two["total_tracked"], 3,
        "total is the full set, not the page"
    );
}

#[tokio::test]
async fn a_non_cell_collection_is_refused_with_a_usable_message() {
    let app = app();
    let sid = handshake(&app).await;
    let (status, _, body) = call(
        &app,
        Some(TOKEN),
        Some(&sid),
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
               "params": {"name": "get_storm_cells", "arguments": {"collection": "places"}}}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "protocol-level OK, tool-level error"
    );
    // The error must name the collections that WOULD work, so a wrong guess
    // self-corrects instead of becoming an apology to the user.
    assert!(
        body.contains("does not serve storm cells") && body.contains("cells"),
        "error should redirect to a valid collection: {body}"
    );
}

#[tokio::test]
async fn cell_track_walks_frames_and_says_so_when_the_id_is_gone() {
    let app = app();
    let sid = handshake(&app).await;
    let out = call_tool(
        &app,
        &sid,
        "get_cell_track",
        json!({"collection": "cells", "cell_id": "42"}),
    )
    .await;
    let history = out["history"].as_array().expect("history array");
    assert!(!history.is_empty(), "cell 42 exists in the frames: {out}");

    let missing = call_tool(
        &app,
        &sid,
        "get_cell_track",
        json!({"collection": "cells", "cell_id": "99999"}),
    )
    .await;
    assert!(missing["history"].as_array().unwrap().is_empty());
    // Track ids restart on reload — the note has to say so, or a model will
    // report a storm as having vanished.
    assert!(missing["note"]
        .as_str()
        .unwrap_or_default()
        .contains("restart when the server reloads"));
}

#[tokio::test]
async fn a_disabled_endpoint_looks_absent() {
    // Reload can flip this; the route stays nested from boot, so without the
    // flag an operator turning MCP off would leave it live and reachable.
    let auth = Arc::new(McpAuth::new(TOKEN.to_string(), 0));
    auth.set_enabled(false);
    let mut engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    engines.insert("cells".into(), Arc::new(CellEngine));
    let mut collections = HashMap::new();
    collections.insert("cells".to_string(), collection("cells", "nowcast"));
    let app = api_mcp::router(
        Arc::new(ArcSwap::from_pointee(McpState {
            engines,
            collections,
        })),
        auth,
        api_mcp::allowed_hosts(BASE_URL, &[]),
    );

    // 404, not 401: a disabled endpoint should look absent rather than
    // advertise that a credential would help.
    let (status, _, _) = call(&app, Some(TOKEN), None, initialize()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn collection_info_does_not_touch_a_non_cells_engine() {
    // feature_count()/temporal_extent() on a postgis engine hit the database
    // — a sync bridge from a request handler (Critical Rule 7). The tool must
    // return config metadata only for anything that isn't a cells collection.
    let app = app();
    let sid = handshake(&app).await;
    let out = call_tool(
        &app,
        &sid,
        "get_collection_info",
        json!({"collection": "places"}),
    )
    .await;
    assert_eq!(out["serves_storm_cells"], false);
    assert!(
        out.get("tracked_cells").is_none() && out.get("retained_frames").is_none(),
        "engine-derived fields must be absent for a non-cells collection: {out}"
    );
    // A cells collection still gets them.
    let cells = call_tool(
        &app,
        &sid,
        "get_collection_info",
        json!({"collection": "cells"}),
    )
    .await;
    assert_eq!(cells["tracked_cells"], 3);
}

/// An engine whose queries fail with a message carrying internal detail.
struct FailingEngine;

impl FeatureEngine for FailingEngine {
    fn get_features(&self, _q: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        Err(DataServerError::Storage(
            "/meteo/data/secret-path/db.sqlite: connection refused from 10.0.0.7".into(),
        ))
    }
    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        Err(DataServerError::FeatureNotFound(id.into()))
    }
}

/// Critical Rule 11: engine errors must not reach the client. The mock above
/// carries a filesystem path and an internal host, which is exactly the shape
/// of `DataServerError::Storage` in production.
#[tokio::test]
async fn an_engine_failure_does_not_leak_internal_detail() {
    let mut engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    engines.insert("cells".into(), Arc::new(FailingEngine));
    let mut collections = HashMap::new();
    collections.insert("cells".to_string(), collection("cells", "nowcast"));
    let app = api_mcp::router(
        Arc::new(ArcSwap::from_pointee(McpState {
            engines,
            collections,
        })),
        Arc::new(McpAuth::new(TOKEN.to_string(), 0)),
        api_mcp::allowed_hosts(BASE_URL, &[]),
    );
    let sid = handshake(&app).await;

    let (status, _, body) = call(
        &app,
        Some(TOKEN),
        Some(&sid),
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
               "params": {"name": "get_storm_cells", "arguments": {"collection": "cells"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for leaked in [
        "/meteo/data",
        "secret-path",
        "db.sqlite",
        "10.0.0.7",
        "connection refused",
    ] {
        assert!(
            !body.contains(leaked),
            "internal detail {leaked:?} reached the client: {body}"
        );
    }
    assert!(
        body.contains("Query failed"),
        "the client should still learn the query failed: {body}"
    );
}

/// `samples` bounds frames WALKED, not frames containing the cell — a cell
/// present only in an older frame must still be found when the budget covers
/// that many frames.
#[tokio::test]
async fn samples_counts_frames_walked_not_matches() {
    let app = app();
    let sid = handshake(&app).await;
    let out = call_tool(
        &app,
        &sid,
        "get_cell_track",
        json!({"collection": "cells", "cell_id": "old-only", "samples": 2}),
    )
    .await;
    assert_eq!(
        out["history"].as_array().unwrap().len(),
        1,
        "a cell absent from the newest frame must still be found in the second: {out}"
    );
    assert_eq!(out["frames_walked"], 2, "both frames were walked");
}

#[tokio::test]
async fn zero_limits_are_rejected_rather_than_coerced() {
    // A model asking for 0 means none; handing back 1 is a silently-wrong
    // answer, which is the failure mode this crate is built to avoid.
    let app = app();
    let sid = handshake(&app).await;
    for (tool, args) in [
        (
            "get_storm_cells",
            json!({"collection": "cells", "limit": 0}),
        ),
        (
            "get_cell_track",
            json!({"collection": "cells", "cell_id": "42", "samples": 0}),
        ),
    ] {
        let (_, _, body) = call(
            &app,
            Some(TOKEN),
            Some(&sid),
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                   "params": {"name": tool, "arguments": args}}),
        )
        .await;
        assert!(
            body.contains("must be between"),
            "{tool} should reject 0 with a range: {body}"
        );
    }
}

#[tokio::test]
async fn a_time_outside_retention_is_distinguishable_from_a_quiet_frame() {
    // Both return zero cells. A model must not read the first as "no storms".
    let app = app();
    let sid = handshake(&app).await;
    let out = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "at": "2020-01-01T00:00:00Z"}),
    )
    .await;
    assert_eq!(out["no_frame_for_requested_time"], true);
    assert!(
        out["retained_frames"]["from"].is_string(),
        "and it should say what window IS available: {out}"
    );

    // The latest frame is a real answer, so the flag stays false.
    let now = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells"}),
    )
    .await;
    assert_eq!(now["no_frame_for_requested_time"], false);
}

#[tokio::test]
async fn the_track_walk_says_why_it_stopped() {
    let app = app();
    let sid = handshake(&app).await;
    let out = call_tool(
        &app,
        &sid,
        "get_cell_track",
        json!({"collection": "cells", "cell_id": "42", "samples": 1}),
    )
    .await;
    // "gave_up_in_empty_gap" must never be mistaken for "the cell stopped
    // existing", so the reason is always reported.
    assert_eq!(out["stopped_because"], "samples_reached");
}

/// An engine whose frames are retained but contain no cells — engine-nowcast
/// pushes a snapshot every generation regardless of cell count.
struct QuietEngine;

impl FeatureEngine for QuietEngine {
    fn get_features(&self, _q: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        Ok(FeaturePage {
            features: vec![],
            number_matched: 0,
            number_returned: 0,
            next_offset: None,
        })
    }
    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        Err(DataServerError::FeatureNotFound(id.into()))
    }
    fn temporal_extent(
        &self,
    ) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
        Some((
            "2026-08-21T14:20:00Z".parse().unwrap(),
            "2026-08-21T14:25:00Z".parse().unwrap(),
        ))
    }
}

/// The distinction the flag exists for. Deriving it from an empty page — as
/// the first version did — labels a genuinely quiet frame "no frame for that
/// time", which is a different and false statement.
#[tokio::test]
async fn a_quiet_frame_inside_retention_is_not_reported_as_missing() {
    let mut engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    engines.insert("cells".into(), Arc::new(QuietEngine));
    let mut collections = HashMap::new();
    collections.insert("cells".to_string(), collection("cells", "nowcast"));
    let app = api_mcp::router(
        Arc::new(ArcSwap::from_pointee(McpState {
            engines,
            collections,
        })),
        Arc::new(McpAuth::new(TOKEN.to_string(), 0)),
        api_mcp::allowed_hosts(BASE_URL, &[]),
    );
    let sid = handshake(&app).await;

    // Inside the retained window, zero cells: quiet, not missing.
    let quiet = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "at": "2026-08-21T14:22:00Z"}),
    )
    .await;
    assert_eq!(quiet["returned"], 0);
    assert_eq!(
        quiet["no_frame_for_requested_time"], false,
        "a quiet frame is an answer, not an absence: {quiet}"
    );

    // Before the window: genuinely missing.
    let missing = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "at": "2020-01-01T00:00:00Z"}),
    )
    .await;
    assert_eq!(missing["no_frame_for_requested_time"], true);
}

/// `frames += 1` runs before the boundary check, so a walk that reaches
/// retention start on its samples-th frame was relabelled "samples_reached".
#[tokio::test]
async fn reaching_retention_start_is_not_relabelled_as_samples_reached() {
    let app = app();
    let sid = handshake(&app).await;
    // The mock retains exactly two frames; asking for two walks both and hits
    // the boundary on the second.
    let out = call_tool(
        &app,
        &sid,
        "get_cell_track",
        json!({"collection": "cells", "cell_id": "42", "samples": 2}),
    )
    .await;
    assert_eq!(
        out["stopped_because"], "reached_retention_start",
        "the real reason must survive the post-loop default: {out}"
    );
}

/// A model guessing a wrong argument name must be told, not silently given
/// the default.
#[tokio::test]
async fn unknown_arguments_are_rejected() {
    let app = app();
    let sid = handshake(&app).await;
    let (_, _, body) = call(
        &app,
        Some(TOKEN),
        Some(&sid),
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
               "params": {"name": "get_storm_cells",
                          "arguments": {"collection": "cells", "count": 5}}}),
    )
    .await;
    assert!(
        body.contains("count") || body.contains("unknown field"),
        "a misspelled argument should be named, not ignored: {body}"
    );
}

/// The bug this fixes: rmcp's Host allowlist defaults to loopback, so a
/// deployment behind any reverse proxy 403s every request. Every other test
/// here now speaks to a public hostname for exactly this reason.
#[tokio::test]
async fn a_public_host_is_accepted_and_an_unknown_one_is_not() {
    let app = app();
    // The configured host works — the whole point.
    let (status, _, _) = call(&app, Some(TOKEN), None, initialize()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the deployment's own host must work"
    );

    // An unrecognised Host is still refused: the protection stays on.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header("content-type", "application/json")
                .header("host", "evil.example.com")
                .header("accept", "application/json, text/event-stream")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from(initialize().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::FORBIDDEN,
        "DNS-rebinding protection must stay on for unknown hosts"
    );
}

#[test]
fn allowed_hosts_derives_from_base_url() {
    let hosts = api_mcp::allowed_hosts("https://meteocore.app.meteo.fi", &[]);
    assert!(hosts.contains(&"meteocore.app.meteo.fi".to_string()));
    // Loopback stays, so a local smoke test still works.
    assert!(hosts.contains(&"localhost".to_string()));
    assert!(hosts.contains(&"127.0.0.1".to_string()));

    // A non-default port appears both with and without it, since the Host
    // header carries the port only when it is non-default for the scheme.
    let hosts = api_mcp::allowed_hosts("http://example.org:8000", &[]);
    assert!(hosts.contains(&"example.org:8000".to_string()));
    assert!(hosts.contains(&"example.org".to_string()));

    // Explicit extras are added, for a proxy presenting another name.
    let hosts = api_mcp::allowed_hosts("https://a.test", &["b.test".to_string()]);
    assert!(hosts.contains(&"a.test".to_string()) && hosts.contains(&"b.test".to_string()));

    // A malformed base_url degrades to loopback rather than panicking.
    let hosts = api_mcp::allowed_hosts("not-a-url", &[]);
    assert!(hosts.contains(&"localhost".to_string()));
}
