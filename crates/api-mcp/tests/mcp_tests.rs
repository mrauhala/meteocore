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
        // The signed breakdown (#650): a negative entry is a reason the cell
        // ranked LOWER, and must reach the model as a number, not a name.
        let record = |term: &str, value: f64| {
            PropertyValue::Object(vec![
                ("term".into(), PropertyValue::String(term.into())),
                ("value".into(), PropertyValue::Float(value)),
            ])
        };
        m.insert(
            "significance_contributions".into(),
            PropertyValue::List(vec![record("impact", 0.5), record("clutter", -0.25)]),
        );
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

    fn get_feature_at(
        &self,
        id: &str,
        datetime: &DatetimeInterval,
    ) -> Result<Feature, DataServerError> {
        feature_at(self, id, datetime)
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

    fn available_times(&self) -> Vec<chrono::DateTime<chrono::Utc>> {
        vec![
            "2026-08-21T14:20:00Z".parse().unwrap(),
            "2026-08-21T14:25:00Z".parse().unwrap(),
        ]
    }
}

/// A mock's `get_feature_at`: the frame its `get_features` selects for
/// `datetime`, searched for the id.
fn feature_at(
    engine: &dyn FeatureEngine,
    id: &str,
    datetime: &DatetimeInterval,
) -> Result<Feature, DataServerError> {
    engine
        .get_features(&FeatureQuery {
            limit: usize::MAX,
            datetime: Some(datetime.clone()),
            ..Default::default()
        })?
        .features
        .into_iter()
        .find(|f| f.id == id)
        .ok_or_else(|| DataServerError::FeatureNotFound(id.into()))
}

/// A cells engine with nothing retained yet — the state right after a boot or
/// reload, before the first generation lands.
struct EmptyCellEngine;

impl FeatureEngine for EmptyCellEngine {
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

    fn feature_count(&self) -> usize {
        0
    }

    fn temporal_extent(
        &self,
    ) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
        None
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
        bufr: None,
        satellite: None,
        preview: None,
        derive_wind: None,
    }
}

fn app() -> axum::Router {
    let mut engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    engines.insert("cells".into(), Arc::new(CellEngine));
    engines.insert("places".into(), Arc::new(CellEngine));
    engines.insert("empty".into(), Arc::new(EmptyCellEngine));
    let mut collections = HashMap::new();
    collections.insert("cells".to_string(), collection("cells", "nowcast"));
    collections.insert("places".to_string(), collection("places", "geojson"));
    collections.insert("empty".to_string(), collection("empty", "nowcast"));

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

/// Call a tool expecting a JSON-RPC error, returning its message.
///
/// Argument errors surface as `invalid_params` on the error channel rather
/// than as `isError` tool content, so they need their own unwrapping.
async fn call_tool_expect_error(app: &axum::Router, sid: &str, name: &str, args: Value) -> String {
    let (status, _, body) = call(
        app,
        Some(TOKEN),
        Some(sid),
        json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
               "params": {"name": name, "arguments": args}}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "transport should still be 200: {body}"
    );
    let doc: Value = parse_rpc(&body);
    doc["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a JSON-RPC error, got {doc}"))
        .to_string()
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
    // Records survive as objects, sign included.
    assert_eq!(
        cells[0]["significance_contributions"],
        json!([
            {"term": "impact", "value": 0.5},
            {"term": "clutter", "value": -0.25}
        ])
    );

    // The response carries its own disclaimer, so a model summarizing one
    // cell in isolation still sees it.
    assert!(out["note"]
        .as_str()
        .unwrap_or_default()
        .contains("not an official warning"));

    // limit is honoured.
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

const LEAKY_DETAIL: &str = "/meteo/data/secret-path/db.sqlite: connection refused from 10.0.0.7";

impl FeatureEngine for FailingEngine {
    fn get_features(&self, _q: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        Err(DataServerError::Storage(LEAKY_DETAIL.into()))
    }
    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        Err(DataServerError::FeatureNotFound(id.into()))
    }
    fn get_feature_at(
        &self,
        _id: &str,
        _datetime: &DatetimeInterval,
    ) -> Result<Feature, DataServerError> {
        Err(DataServerError::Storage(LEAKY_DETAIL.into()))
    }
    fn available_times(&self) -> Vec<chrono::DateTime<chrono::Utc>> {
        vec!["2026-08-21T14:25:00Z".parse().unwrap()]
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

    // Both data paths: a frame query, and the track walk's by-id lookups.
    for (tool, args) in [
        ("get_storm_cells", json!({"collection": "cells"})),
        (
            "get_cell_track",
            json!({"collection": "cells", "cell_id": "42"}),
        ),
    ] {
        let (status, _, body) = call(
            &app,
            Some(TOKEN),
            Some(&sid),
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                   "params": {"name": tool, "arguments": args}}),
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
                "{tool}: internal detail {leaked:?} reached the client: {body}"
            );
        }
        assert!(
            body.contains("Query failed"),
            "{tool}: the client should still learn the query failed: {body}"
        );
    }
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
async fn out_of_range_limits_are_rejected_rather_than_coerced() {
    // A model asking for 0 means none; handing back 1 is a silently-wrong
    // answer, which is the failure mode this crate is built to avoid. Above
    // the maximum likewise: a silent clamp returns a page nobody asked for
    // (#652).
    let app = app();
    let sid = handshake(&app).await;
    for (tool, args) in [
        (
            "get_storm_cells",
            json!({"collection": "cells", "limit": 0}),
        ),
        (
            "get_storm_cells",
            json!({"collection": "cells", "limit": 51}),
        ),
        (
            "get_cell_track",
            json!({"collection": "cells", "cell_id": "42", "samples": 0}),
        ),
        (
            "get_cell_track",
            json!({"collection": "cells", "cell_id": "42", "samples": 49}),
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
            "{tool} should reject {args} with a range: {body}"
        );
    }
    // The maxima themselves are accepted.
    let max = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "limit": 50}),
    )
    .await;
    assert!(max["cells"].is_array(), "{max}");
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
    // "samples_reached" must never be mistaken for "the cell stopped
    // existing", so the reason is always reported.
    assert_eq!(out["stopped_because"], "samples_reached");
}

/// An empty history's note follows how the walk ended (#646): only a walk
/// that read every retained frame may say the id is not retained; one that
/// stopped at `samples` says to look further back.
#[tokio::test]
async fn an_empty_track_note_follows_why_the_walk_stopped() {
    let app = app();
    let sid = handshake(&app).await;
    let track = |samples: u64| {
        let (app, sid) = (&app, &sid);
        async move {
            call_tool(
                app,
                sid,
                "get_cell_track",
                json!({"collection": "cells", "cell_id": "no-such-cell", "samples": samples}),
            )
            .await
        }
    };
    let short = track(1).await;
    assert_eq!(short["stopped_because"], "samples_reached");
    let note = short["note"].as_str().unwrap();
    assert!(
        !note.contains("not present in any retained frame"),
        "{note}"
    );
    assert!(note.contains("Raise `samples`"), "{note}");

    let full = track(2).await;
    assert_eq!(full["stopped_because"], "reached_earliest_retained_frame");
    let note = full["note"].as_str().unwrap();
    assert!(note.contains("not present in any retained frame"), "{note}");
}

/// Retained frames at 5-minute cadence, 14:00 through 17:55 (48 of them,
/// the engine's full retention), where only the OLDEST holds a cell: 230
/// minutes of quiet frames separate it from the newest.
struct GappyEngine;

impl GappyEngine {
    fn frames() -> Vec<chrono::DateTime<chrono::Utc>> {
        let first: chrono::DateTime<chrono::Utc> = "2026-08-21T14:00:00Z".parse().unwrap();
        (0..48)
            .map(|i| first + chrono::Duration::minutes(5 * i))
            .collect()
    }
}

impl FeatureEngine for GappyEngine {
    fn get_features(&self, q: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        // Newest frame inside the interval, as engine-nowcast selects.
        let frame = Self::frames().into_iter().rev().find(|t| {
            let dt = q.datetime.as_ref();
            dt.and_then(|d| d.start).is_none_or(|s| *t >= s)
                && dt.and_then(|d| d.end).is_none_or(|e| *t <= e)
        });
        let features = match frame {
            Some(t) if t == Self::frames()[0] => {
                vec![CellEngine::cell("early", 0.5, 50.0, "2026-08-21T14:00:00Z")]
            }
            _ => vec![],
        };
        Ok(FeaturePage {
            number_matched: features.len(),
            number_returned: features.len(),
            features,
            next_offset: None,
        })
    }
    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        Err(DataServerError::FeatureNotFound(id.into()))
    }
    fn get_feature_at(
        &self,
        id: &str,
        datetime: &DatetimeInterval,
    ) -> Result<Feature, DataServerError> {
        feature_at(self, id, datetime)
    }
    fn temporal_extent(
        &self,
    ) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
        let frames = Self::frames();
        Some((frames[0], frames[frames.len() - 1]))
    }
    fn available_times(&self) -> Vec<chrono::DateTime<chrono::Utc>> {
        Self::frames()
    }
}

/// #646: the walk probed one minute at a time with a 200-probe budget for
/// quiet frames, so a cell behind more than ~199 minutes of them was
/// unreachable although retention is ~240. Stepping frame to frame reaches
/// it, and quiet frames count as frames walked.
#[tokio::test]
async fn a_long_quiet_stretch_does_not_hide_an_older_cell() {
    let app = cells_app(Arc::new(GappyEngine));
    let sid = handshake(&app).await;

    let full = call_tool(
        &app,
        &sid,
        "get_cell_track",
        json!({"collection": "cells", "cell_id": "early", "samples": 48}),
    )
    .await;
    let history = full["history"].as_array().unwrap();
    assert_eq!(history.len(), 1, "{full}");
    assert_eq!(history[0]["observed"], "2026-08-21T14:00:00Z");
    assert_eq!(full["frames_walked"], 48, "{full}");
    assert_eq!(full["stopped_because"], "reached_earliest_retained_frame");
    assert_eq!(full["retained_frames"]["from"], "2026-08-21T14:00:00Z");

    // Quiet frames spend `samples` like any other, and the note says the
    // older frames were left unread rather than that the id is gone.
    let half = call_tool(
        &app,
        &sid,
        "get_cell_track",
        json!({"collection": "cells", "cell_id": "early", "samples": 24}),
    )
    .await;
    assert!(half["history"].as_array().unwrap().is_empty(), "{half}");
    assert_eq!(half["frames_walked"], 24);
    assert_eq!(half["stopped_because"], "samples_reached");
    assert!(
        half["note"].as_str().unwrap().contains("Raise `samples`"),
        "{half}"
    );
}

/// #646: each frame was read as one page of at most 1000 cells, in id order,
/// so a cell past the page was reported missing from a frame it was in. A
/// by-id lookup has no page to fall off.
#[tokio::test]
async fn a_crowded_frame_does_not_hide_the_cell() {
    let app = cells_app(Arc::new(ManyCellEngine(1_005)));
    let sid = handshake(&app).await;
    let track = call_tool(
        &app,
        &sid,
        "get_cell_track",
        json!({"collection": "cells", "cell_id": "c1004"}),
    )
    .await;
    assert_eq!(track["history"].as_array().unwrap().len(), 1, "{track}");
    assert_eq!(track["history"][0]["id"], "c1004");
    assert_eq!(track["stopped_because"], "reached_earliest_retained_frame");
    assert!(
        track.get("frames_truncated").is_none(),
        "no frame is read in part any more: {track}"
    );
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
        out["stopped_because"], "reached_earliest_retained_frame",
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

/// The retained window is published on every response, not only when the
/// caller asked for a time outside it.
///
/// It was previously part of the out-of-range explanation, so a documented
/// field read `null` in every successful response and a client had no way to
/// learn how far back it could ask without first asking wrongly. Reported by
/// a model consuming the live endpoint, 2026-08-24.
#[tokio::test]
async fn the_retained_window_is_published_on_successful_responses_too() {
    let app = app();
    let sid = handshake(&app).await;

    let cells = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells"}),
    )
    .await;
    assert_eq!(
        cells["no_frame_for_requested_time"], false,
        "this request is inside retention"
    );
    assert!(
        cells["retained_frames"]["from"].is_string() && cells["retained_frames"]["to"].is_string(),
        "the window must be present anyway: {cells}"
    );

    let track = call_tool(
        &app,
        &sid,
        "get_cell_track",
        json!({"collection": "cells", "cell_id": "42"}),
    )
    .await;
    assert!(
        track["retained_frames"]["from"].is_string(),
        "a track walk must say how far back it could have gone: {track}"
    );
}

/// Both cell tools carry `retained_frames` on EVERY response, including the
/// paths where there is nothing to report.
///
/// Found in review on #626: `get_cell_track`'s early return for an engine with
/// no retained frames omitted the key entirely, while `get_storm_cells`
/// emitted an explicit null. A client testing key presence would have read the
/// same situation two different ways depending on which tool it called.
#[tokio::test]
async fn retained_frames_is_present_even_when_nothing_is_retained() {
    let app = app();
    let sid = handshake(&app).await;

    for (tool, args) in [
        ("get_storm_cells", json!({"collection": "empty"})),
        (
            "get_cell_track",
            json!({"collection": "empty", "cell_id": "42"}),
        ),
    ] {
        let out = call_tool(&app, &sid, tool, args).await;
        let obj = out.as_object().expect("object response");
        assert!(
            obj.contains_key("retained_frames"),
            "{tool} dropped the key entirely: {out}"
        );
        assert!(
            out["retained_frames"].is_null(),
            "{tool} must report null, not a window: {out}"
        );
    }
}

/// #630: `sortable_properties` was advertised while `get_storm_cells` had no
/// way to use it — a capability announced and withheld.
#[tokio::test]
async fn storm_cells_can_be_ordered_by_an_advertised_property() {
    let app = app();
    let sid = handshake(&app).await;

    // Default is unchanged: most significant first.
    let out = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells"}),
    )
    .await;
    let sig: Vec<f64> = out["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["significance"].as_f64().unwrap())
        .collect();
    assert!(
        sig.windows(2).all(|w| w[0] >= w[1]),
        "default must stay significance-desc: {sig:?}"
    );

    // Ascending by an advertised key actually reorders.
    let out = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "sort_by": "max_dbz", "order": "asc"}),
    )
    .await;
    let dbz: Vec<f64> = out["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["max_dbz"].as_f64().unwrap())
        .collect();
    assert!(
        dbz.windows(2).all(|w| w[0] <= w[1]),
        "ascending max_dbz was ignored: {dbz:?}"
    );
}

/// An unsortable key is an error naming the alternatives, never a silently
/// different ordering.
#[tokio::test]
async fn an_unknown_sort_key_is_rejected_with_the_valid_ones() {
    let app = app();
    let sid = handshake(&app).await;
    let err = call_tool_expect_error(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "sort_by": "not_a_property"}),
    )
    .await;
    assert!(err.contains("not_a_property"), "{err}");
    assert!(
        err.contains("significance") && err.contains("max_dbz"),
        "the error must name what WOULD work: {err}"
    );

    let err = call_tool_expect_error(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "sort_by": "max_dbz", "order": "sideways"}),
    )
    .await;
    assert!(err.contains("sideways"), "{err}");

    // `order` alone has nothing to apply to. Accepting it and returning the
    // default ordering is the silent no-op this parameter set exists to
    // prevent — the caller asked for ascending and would get descending.
    let err = call_tool_expect_error(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "order": "asc"}),
    )
    .await;
    assert!(
        err.contains("sort_by"),
        "the error must name what is missing: {err}"
    );
}

/// A significance floor narrows the result, and says how much it removed —
/// "3 cells exist" and "7 were below your floor" are different answers.
#[tokio::test]
async fn a_significance_floor_reports_what_it_removed() {
    let app = app();
    let sid = handshake(&app).await;

    let out = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells"}),
    )
    .await;
    assert!(
        out["below_min_significance"].is_null() && out["matching_min_significance"].is_null(),
        "null unless a floor was set, so it cannot read as 'nothing filtered'"
    );

    // The mock's cells are 0.88 / 0.55 / 0.31.
    let out = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "min_significance": 0.5}),
    )
    .await;
    assert_eq!(out["returned"], 2);
    assert_eq!(out["matching_min_significance"], 2);
    assert_eq!(out["below_min_significance"], 1);
    assert_eq!(
        out["total_tracked"], 3,
        "the frame still had three cells; the floor did not delete them"
    );

    let err = call_tool_expect_error(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "min_significance": 1.5}),
    )
    .await;
    assert!(err.contains("between 0 and 1"), "{err}");
}

/// The advertised input schema must match the parameters actually accepted.
///
/// Reported 2026-08-25: `get_storm_cells` was seen advertising only
/// `collection`, `at` and `limit` with `additionalProperties: false`, while
/// the server accepted `sort_by`, `order` and `min_significance`. A
/// schema-conforming client can then only reach them by guessing a name its
/// schema says is forbidden — and a NUMBER cannot survive that path at all,
/// because an undeclared numeric gets serialised as a string and rejected.
#[tokio::test]
async fn the_storm_cells_schema_declares_every_parameter_it_accepts() {
    let app = app();
    let sid = handshake(&app).await;
    let (_, _, body) = call(
        &app,
        Some(TOKEN),
        Some(&sid),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let doc: Value = parse_rpc(&body);
    let tool = doc["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .find(|t| t["name"] == "get_storm_cells")
        .expect("get_storm_cells advertised");
    let props = tool["inputSchema"]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("no properties in {tool}"));

    for p in [
        "collection",
        "at",
        "limit",
        "sort_by",
        "order",
        "min_significance",
    ] {
        assert!(
            props.contains_key(p),
            "`{p}` is accepted but not advertised: {}",
            serde_json::to_string(&props).unwrap()
        );
    }

    // The numeric one is the case that fails silently in the wild: an
    // undeclared number is serialised as a string by the client and rejected
    // by serde, so the filter reads as broken rather than undiscovered.
    // An Option<f64> renders as `type: ["number", "null"]`, which is valid.
    // What matters is that "number" appears at all: a client that cannot see
    // a numeric type sends the value as a string, and serde then rejects it —
    // the filter reads as broken rather than as undiscovered.
    let ty = &props["min_significance"];
    let mentions_number = ty["type"]
        .as_str()
        .map(|t| t == "number")
        .or_else(|| {
            ty["type"]
                .as_array()
                .map(|v| v.iter().any(|t| t == "number"))
        })
        .unwrap_or(false);
    assert!(
        mentions_number,
        "min_significance must advertise a numeric type: {ty}"
    );
}

/// An app whose only collection, "cells", is served by `engine`.
fn cells_app(engine: Arc<dyn FeatureEngine>) -> axum::Router {
    let mut engines: HashMap<String, Arc<dyn FeatureEngine>> = HashMap::new();
    engines.insert("cells".into(), engine);
    let mut collections = HashMap::new();
    collections.insert("cells".to_string(), collection("cells", "nowcast"));
    api_mcp::router(
        Arc::new(ArcSwap::from_pointee(McpState {
            engines,
            collections,
        })),
        Arc::new(McpAuth::new(TOKEN.to_string(), 0)),
        api_mcp::allowed_hosts(BASE_URL, &[]),
    )
}

/// #652: an `at` after the newest frame resolved to that frame with nothing
/// saying so, so a model asking for 09:00 could present 08:25 as 09:00.
#[tokio::test]
async fn a_future_at_is_flagged_and_names_the_frame_served() {
    let app = app();
    let sid = handshake(&app).await;
    let at = |t: &'static str| json!({"collection": "cells", "at": t});

    let future = call_tool(&app, &sid, "get_storm_cells", at("2026-08-21T15:00:00Z")).await;
    assert_eq!(
        future["requested_time_after_newest_frame"], true,
        "{future}"
    );
    assert_eq!(future["observed"], "2026-08-21T14:25:00Z", "{future}");
    assert_eq!(
        future["no_frame_for_requested_time"], false,
        "a frame WAS served — the newest one"
    );
    let note = future["note"].as_str().unwrap();
    assert!(note.contains("after the newest analysis frame"), "{note}");
    assert!(note.contains("2026-08-21T14:25:00Z"), "{note}");
    assert!(
        note.contains("not an official warning"),
        "the disclaimer survives: {note}"
    );

    // Exactly the newest frame, an older one, and no `at`: nothing to flag.
    for args in [
        at("2026-08-21T14:25:00Z"),
        at("2026-08-21T14:22:00Z"),
        json!({"collection": "cells"}),
    ] {
        let out = call_tool(&app, &sid, "get_storm_cells", args.clone()).await;
        assert_eq!(
            out["requested_time_after_newest_frame"], false,
            "{args}: {out}"
        );
        assert!(
            !out["note"]
                .as_str()
                .unwrap()
                .contains("after the newest analysis frame"),
            "{args}: {out}"
        );
    }

    // A quiet newest frame has no cell to carry `observed`; the frame served
    // is still named, or the flag would point at nothing.
    let app = cells_app(Arc::new(QuietEngine));
    let sid = handshake(&app).await;
    let quiet = call_tool(&app, &sid, "get_storm_cells", at("2026-08-21T15:00:00Z")).await;
    assert_eq!(quiet["returned"], 0);
    assert_eq!(quiet["requested_time_after_newest_frame"], true, "{quiet}");
    assert_eq!(quiet["observed"], "2026-08-21T14:25:00Z", "{quiet}");
}

/// `n` cells in one frame, significance rising with the index and `max_dbz`
/// with it, so ascending `max_dbz` is ascending significance.
struct ManyCellEngine(usize);

impl FeatureEngine for ManyCellEngine {
    fn sortables(&self) -> &[&'static str] {
        &["significance", "max_dbz"]
    }

    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let n = self.0;
        let mut all: Vec<Feature> = (0..n)
            .map(|i| {
                CellEngine::cell(
                    &format!("c{i:04}"),
                    (i as f64 + 0.5) / n as f64,
                    30.0 + i as f64 / 100.0,
                    "2026-08-21T14:25:00Z",
                )
            })
            .collect();
        sort_features(&mut all, &query.sortby);
        let page: Vec<Feature> = all
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .collect();
        Ok(FeaturePage {
            number_returned: page.len(),
            features: page,
            number_matched: n,
            next_offset: None,
        })
    }

    fn get_feature(&self, id: &str) -> Result<Feature, DataServerError> {
        Err(DataServerError::FeatureNotFound(id.into()))
    }

    fn get_feature_at(
        &self,
        id: &str,
        datetime: &DatetimeInterval,
    ) -> Result<Feature, DataServerError> {
        feature_at(self, id, datetime)
    }

    fn temporal_extent(
        &self,
    ) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
        let t = "2026-08-21T14:25:00Z".parse().unwrap();
        Some((t, t))
    }

    fn available_times(&self) -> Vec<chrono::DateTime<chrono::Utc>> {
        vec!["2026-08-21T14:25:00Z".parse().unwrap()]
    }
}

/// #652: the floor filtered the page AFTER it was cut to `limit`, so "every
/// cell at or above 0.3" was unanswerable once more than `limit` qualified —
/// and under `sort_by` the page could hold few or none of the qualifying
/// cells at all.
#[tokio::test]
async fn min_significance_filters_the_frame_before_limit() {
    let app = cells_app(Arc::new(ManyCellEngine(90)));
    let sid = handshake(&app).await;
    // (i + 0.5) / 90 >= 0.3 for i >= 27: 63 of 90 cells qualify, more than
    // the largest page.
    let significances = |out: &Value| -> Vec<f64> {
        out["cells"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["significance"].as_f64().unwrap())
            .collect()
    };

    for args in [
        json!({"collection": "cells", "limit": 50, "min_significance": 0.3}),
        // Ascending: the first 50 cells in this order hold only 23 that
        // qualify, so filtering the page returned 23, not 50.
        json!({"collection": "cells", "limit": 50, "min_significance": 0.3,
               "sort_by": "max_dbz", "order": "asc"}),
    ] {
        let out = call_tool(&app, &sid, "get_storm_cells", args.clone()).await;
        let sig = significances(&out);
        assert_eq!(out["returned"], 50, "{args}");
        assert_eq!(sig.len(), 50, "{args}");
        assert!(sig.iter().all(|&s| s >= 0.3), "{args}: {sig:?}");
        assert_eq!(out["matching_min_significance"], 63, "{args}");
        assert_eq!(out["below_min_significance"], 27, "{args}");
        assert_eq!(out["total_tracked"], 90, "{args}");
    }

    // The requested order still holds within the qualifying set.
    let asc = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "limit": 3, "min_significance": 0.3,
               "sort_by": "max_dbz", "order": "asc"}),
    )
    .await;
    let ids: Vec<&str> = asc["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["c0027", "c0028", "c0029"]);
}

/// Past the read cap the counts are partial, and the note says so rather
/// than leaving a model to notice they do not add up to `total_tracked`.
#[tokio::test]
async fn a_floor_over_an_oversized_frame_says_its_counts_are_partial() {
    let app = cells_app(Arc::new(ManyCellEngine(1_005)));
    let sid = handshake(&app).await;
    let out = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells", "min_significance": 0.5}),
    )
    .await;
    assert_eq!(out["total_tracked"], 1_005);
    let counted = out["matching_min_significance"].as_u64().unwrap()
        + out["below_min_significance"].as_u64().unwrap();
    assert_eq!(counted, 1_000, "{out}");
    let note = out["note"].as_str().unwrap();
    assert!(note.contains("first 1000 of 1005 cells"), "{note}");

    // Without a floor nothing is checked, so nothing is partial.
    let out = call_tool(
        &app,
        &sid,
        "get_storm_cells",
        json!({"collection": "cells"}),
    )
    .await;
    assert!(!out["note"].as_str().unwrap().contains("checked"), "{out}");
}
