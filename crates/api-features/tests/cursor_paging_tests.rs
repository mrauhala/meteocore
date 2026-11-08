//! `/items` on a collection that pages by cursor (#503, engine-postgis
//! `events`): `next` carries the engine's cursor and following it reaches
//! every feature once; `offset` there, and `cursor` on a collection paged by
//! offset, are 400s; an engine that reads a database per call runs on the
//! shared query runtime; and the collection leaves out the `numberItems` it
//! cannot count.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use api_features::handlers::FeaturesState;
use ds_core::config::CollectionConfig;
use ds_core::error::DataServerError;
use ds_core::feature::{Feature, FeaturePage, FeatureQuery, Geometry, PropertyValue};
use ds_core::feature_engine::FeatureEngine;

/// Five events, newest first, whose ids are their cursors. The ids hold `@`
/// and `:` as engine-postgis event keys do, so the links must encode them.
const IDS: [&str; 5] = [
    "5@2026-01-01T05:50:00Z",
    "4@2026-01-01T05:40:00Z",
    "3@2026-01-01T05:30:00Z",
    "2@2026-01-01T05:20:00Z",
    "1@2026-01-01T05:10:00Z",
];

/// Pages [`IDS`] by cursor (or, with `cursor` false, by offset), and
/// records each call's cursor and thread.
struct Keyed {
    cursor: bool,
    reads_on_request: bool,
    seen: Mutex<Vec<(Option<String>, Option<String>)>>,
}

impl Keyed {
    fn new(cursor: bool, reads_on_request: bool) -> Self {
        Self {
            cursor,
            reads_on_request,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, cursor: Option<String>) {
        let thread = std::thread::current().name().map(str::to_owned);
        self.seen.lock().unwrap().push((cursor, thread));
    }
}

fn feature(id: &str) -> Feature {
    Feature {
        id: id.to_string(),
        geometry: Arc::new(Geometry::Point { x: 25.0, y: 60.0 }),
        properties: Arc::new(HashMap::from([(
            "time".to_string(),
            PropertyValue::String(id.split_once('@').unwrap().1.to_string()),
        )])),
        related: Vec::new(),
    }
}

impl FeatureEngine for Keyed {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        self.record(query.cursor.clone());
        let start = match &query.cursor {
            Some(cursor) => {
                IDS.iter()
                    .position(|id| id == cursor)
                    .ok_or_else(|| DataServerError::InvalidParameter("bad cursor".into()))?
                    + 1
            }
            None => query.offset,
        };
        let end = (start + query.limit).min(IDS.len());
        let features: Vec<Feature> = IDS[start.min(end)..end]
            .iter()
            .map(|id| feature(id))
            .collect();
        let more = end < IDS.len();
        Ok(FeaturePage {
            number_returned: features.len(),
            next_cursor: (self.cursor && more).then(|| features.last().unwrap().id.clone()),
            next_offset: (!self.cursor && more).then_some(end),
            features,
            // As engine-postgis events count: only a first page that holds
            // every match knows how many match.
            number_matched: (!self.cursor || (query.cursor.is_none() && !more))
                .then_some(IDS.len()),
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        self.record(None);
        IDS.iter()
            .find(|id| **id == feature_id)
            .map(|id| feature(id))
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.into()))
    }

    fn feature_count(&self) -> usize {
        IDS.len()
    }

    fn number_items(&self) -> Option<usize> {
        (!self.cursor).then_some(IDS.len())
    }

    fn pages_by_cursor(&self) -> bool {
        self.cursor
    }

    fn reads_on_request(&self) -> bool {
        self.reads_on_request
    }
}

fn collection(id: &str) -> CollectionConfig {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": id, "description": "",
        "engine_type": "postgis", "apis": ["features"],
    }))
    .expect("a collection config")
}

/// `events` pages by cursor and reads per request; `stations` pages by
/// offset from memory.
fn app(events: Arc<Keyed>, stations: Arc<Keyed>) -> axum::Router {
    let state = FeaturesState {
        engines: HashMap::from([
            ("events".to_string(), events as Arc<dyn FeatureEngine>),
            ("stations".to_string(), stations as Arc<dyn FeatureEngine>),
        ]),
        collections: HashMap::from([
            ("events".to_string(), collection("events")),
            ("stations".to_string(), collection("stations")),
        ]),
        discovery: Default::default(),
        base_url: String::new(),
        trust_proxy_headers: false,
        vector_tileset_ids: Default::default(),
        collection_pages: Default::default(),
    };
    api_features::router(Arc::new(ArcSwap::from_pointee(state)))
}

fn engines() -> (Arc<Keyed>, Arc<Keyed>) {
    (
        Arc::new(Keyed::new(true, true)),
        Arc::new(Keyed::new(false, false)),
    )
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let (status, body) = get(app, uri).await;
    (status, serde_json::from_str(&body).unwrap())
}

fn link<'a>(doc: &'a Value, rel: &str) -> Option<&'a str> {
    doc["links"]
        .as_array()?
        .iter()
        .find(|l| l["rel"] == rel)
        .and_then(|l| l["href"].as_str())
}

fn ids(doc: &Value) -> Vec<String> {
    doc["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn following_next_reaches_every_feature_once_with_the_query_kept() {
    let (events, stations) = engines();
    let app = app(events.clone(), stations);
    let mut uri = "/collections/events/items?limit=2&datetime=2026-01-01T05:00:00Z/..".to_string();
    let mut seen = Vec::new();
    let mut pages = 0;
    loop {
        let (status, doc) = get_json(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {doc}");
        // An uncounted page leaves `numberMatched` out, never null.
        assert!(doc.get("numberMatched").is_none(), "{doc}");
        assert_eq!(doc["numberReturned"], ids(&doc).len());
        assert!(
            link(&doc, "prev").is_none(),
            "no prev on a cursor page: {doc}"
        );
        let this = link(&doc, "self").unwrap();
        assert!(this.contains("&datetime=2026-01-01T05:00:00Z/.."), "{this}");
        seen.extend(ids(&doc));
        pages += 1;
        let Some(next) = link(&doc, "next") else {
            break;
        };
        assert!(
            next.contains("&limit=2&datetime=2026-01-01T05:00:00Z/.."),
            "{next}"
        );
        assert!(!next.contains("offset="), "{next}");
        // The cursor is percent-encoded and the page after it repeats it.
        let cursor = seen.last().unwrap();
        let encoded: String = form_urlencoded::byte_serialize(cursor.as_bytes()).collect();
        assert!(next.contains(&format!("?cursor={encoded}&")), "{next}");
        // Links hang off the `/features` mount; the router under test is
        // not nested.
        uri = next.strip_prefix("/features").unwrap().to_string();
        let (_, again) = get_json(&app, &uri).await;
        assert_eq!(link(&again, "self"), Some(next));
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, IDS);
    // One page that holds every match reports the count.
    let (_, doc) = get_json(&app, "/collections/events/items?limit=10").await;
    assert_eq!(doc["numberMatched"], 5);
    assert!(link(&doc, "next").is_none());
    // The engine got back exactly the cursors it wrote.
    let cursors: Vec<Option<String>> = events
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(cursor, _)| cursor.clone())
        .collect();
    assert!(cursors.contains(&Some(IDS[1].to_string())));
    assert!(cursors.contains(&Some(IDS[3].to_string())));
}

#[tokio::test]
async fn each_collection_takes_only_its_own_paging() {
    let (events, stations) = engines();
    let app = app(events, stations);
    let (status, body) = get_json(&app, "/collections/events/items?offset=2").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["description"]
            .as_str()
            .unwrap()
            .contains("pages by cursor: follow the next link"),
        "{body}"
    );
    // A zero offset, as the HTML form sends, is the first page.
    let (status, doc) = get_json(&app, "/collections/events/items?offset=0&limit=2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&doc), IDS[..2]);

    let (status, body) = get_json(&app, "/collections/stations/items?cursor=x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["description"]
            .as_str()
            .unwrap()
            .contains("pages by offset; cursor is not supported"),
        "{body}"
    );
    // The offset collection's links stay what they were.
    let (_, doc) = get_json(&app, "/collections/stations/items?limit=2&offset=2").await;
    assert_eq!(
        link(&doc, "next"),
        Some("/features/collections/stations/items?offset=4&limit=2&f=json")
    );
    assert_eq!(
        link(&doc, "prev"),
        Some("/features/collections/stations/items?offset=0&limit=2&f=json")
    );
    // A cursor the engine did not write is the engine's 400, and a cursor
    // given twice is ambiguous.
    let (status, _) = get_json(&app, "/collections/events/items?cursor=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get_json(&app, "/collections/events/items?cursor=a&cursor=b").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn reads_on_request_run_on_the_query_runtime_and_memory_reads_in_place() {
    let (events, stations) = engines();
    let app = app(events.clone(), stations.clone());
    let item = form_urlencoded::byte_serialize(IDS[2].as_bytes()).collect::<String>();
    for uri in [
        "/collections/events/items".to_string(),
        format!("/collections/events/items/{item}"),
        "/collections/stations/items".to_string(),
        "/collections/stations/items/3@2026-01-01T05:30:00Z".to_string(),
    ] {
        let (status, body) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
    let on_query_runtime = |engine: &Keyed| -> Vec<bool> {
        engine
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, thread)| {
                thread
                    .as_deref()
                    .is_some_and(|t| t.starts_with(ds_executor::QUERY_THREAD_NAME))
            })
            .collect()
    };
    assert_eq!(on_query_runtime(&events), [true, true]);
    assert_eq!(on_query_runtime(&stations), [false, false]);
}

#[tokio::test]
async fn number_items_is_left_out_where_the_engine_cannot_count() {
    let (events, stations) = engines();
    let app = app(events, stations);
    let (_, doc) = get_json(&app, "/collections/events").await;
    assert!(doc.get("numberItems").is_none(), "{doc}");
    let (_, doc) = get_json(&app, "/collections/stations").await;
    assert_eq!(doc["numberItems"], 5);
}

#[tokio::test]
async fn openapi_offers_each_collection_its_paging_parameter() {
    let (events, stations) = engines();
    let app = app(events, stations);
    let (status, api) = get_json(&app, "/api").await;
    assert_eq!(status, StatusCode::OK);
    let refs = |id: &str| -> Vec<String> {
        api["paths"][format!("/features/collections/{id}/items")]["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["$ref"].as_str().map(str::to_owned))
            .collect()
    };
    let events = refs("events");
    assert!(events.contains(&"#/components/parameters/cursor".to_string()));
    assert!(!events.contains(&"#/components/parameters/offset".to_string()));
    let stations = refs("stations");
    assert!(stations.contains(&"#/components/parameters/offset".to_string()));
    assert!(!stations.contains(&"#/components/parameters/cursor".to_string()));
    assert_eq!(api["components"]["parameters"]["cursor"]["in"], "query");
}

#[tokio::test]
async fn the_html_page_of_a_cursor_page_counts_what_it_shows_and_asks_no_offset() {
    let (events, stations) = engines();
    let app = app(events, stations);
    let cursor = form_urlencoded::byte_serialize(IDS[1].as_bytes()).collect::<String>();
    let (status, html) = get(
        &app,
        &format!("/collections/events/items?cursor={cursor}&limit=2&f=html"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("2 shown</span>"), "no count to show: {html}");
    assert!(!html.contains(" matched</span>"), "{html}");
    assert!(!html.contains("name=\"offset\""));
    let (_, html) = get(&app, "/collections/stations/items?limit=2&offset=2&f=html").await;
    assert!(html.contains("3–4 shown · 5 matched"));
    assert!(html.contains("name=\"offset\""));
}

/// Events every six seconds over the hour before `base`, newest first, paged
/// by a cursor that is the event's time. Without `datetime` the engine's own
/// answer is the hour ending its clock, and the clock runs ten minutes on at
/// every read, as a long chain's would; every read also brings a new event at
/// the clock. It names its default window, as engine-postgis events do.
struct Sliding {
    clock: Mutex<chrono::DateTime<chrono::Utc>>,
    events: Mutex<Vec<chrono::DateTime<chrono::Utc>>>,
    windows: Mutex<Vec<Option<ds_core::feature::DatetimeInterval>>>,
}

const HOUR: chrono::TimeDelta = chrono::TimeDelta::hours(1);

impl Sliding {
    fn new(base: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            clock: Mutex::new(base),
            events: Mutex::new(
                (0..590)
                    .map(|k| base - chrono::TimeDelta::seconds(6 * k))
                    .collect(),
            ),
            windows: Mutex::new(Vec::new()),
        }
    }
}

fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

impl FeatureEngine for Sliding {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        self.windows.lock().unwrap().push(query.datetime.clone());
        let mut clock = self.clock.lock().unwrap();
        let (start, end) = match &query.datetime {
            Some(d) => (d.start.unwrap(), d.end.unwrap()),
            None => (*clock - HOUR, *clock),
        };
        *clock += chrono::TimeDelta::minutes(10);
        let mut events = self.events.lock().unwrap();
        events.insert(0, *clock);
        let after: Option<chrono::DateTime<chrono::Utc>> =
            query.cursor.as_deref().map(|c| c.parse().unwrap());
        let selected: Vec<_> = events
            .iter()
            .filter(|t| **t >= start && **t <= end && after.is_none_or(|a| **t < a))
            .take(query.limit + 1)
            .copied()
            .collect();
        let more = selected.len() > query.limit;
        let features: Vec<Feature> = selected
            .iter()
            .take(query.limit)
            .map(|t| Feature {
                id: rfc3339(*t),
                geometry: Arc::new(Geometry::Point { x: 25.0, y: 60.0 }),
                properties: Arc::new(HashMap::new()),
                related: Vec::new(),
            })
            .collect();
        Ok(FeaturePage {
            number_matched: None,
            number_returned: features.len(),
            next_offset: None,
            next_cursor: more.then(|| features.last().unwrap().id.clone()),
            features,
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        Err(DataServerError::FeatureNotFound(feature_id.into()))
    }

    fn pages_by_cursor(&self) -> bool {
        true
    }

    fn default_datetime(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<ds_core::feature::DatetimeInterval> {
        Some(ds_core::feature::DatetimeInterval {
            start: Some(now - HOUR),
            end: Some(now),
        })
    }
}

/// #503: a chain started without `datetime` reads one fixed interval. The
/// API names the engine's default window before the first read, and `next`
/// carries it as an explicit `datetime`, so the engine's sliding clock and
/// the events arriving between pages change nothing: every event of the
/// first page's window is visited exactly once, the oldest included. Without
/// that, each page read the hour ending a later clock, and the chain lost
/// its oldest events. `self` repeats the request, and a page whose window
/// ends now keeps the short `Cache-Control`.
#[tokio::test]
async fn a_default_window_chain_reads_one_fixed_interval() {
    use chrono::SubsecRound;
    let base = chrono::Utc::now().trunc_subsecs(0);
    let engine = Arc::new(Sliding::new(base));
    let state = FeaturesState {
        engines: HashMap::from([(
            "events".to_string(),
            engine.clone() as Arc<dyn FeatureEngine>,
        )]),
        collections: HashMap::from([("events".to_string(), collection("events"))]),
        discovery: Default::default(),
        base_url: String::new(),
        trust_proxy_headers: false,
        vector_tileset_ids: Default::default(),
        collection_pages: Default::default(),
    };
    let app = api_features::router(Arc::new(ArcSwap::from_pointee(state)));

    let mut uri = "/collections/events/items?limit=100".to_string();
    let mut seen = Vec::new();
    let mut first = true;
    loop {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            response.headers()["cache-control"],
            ds_core::http_cache::CACHE_CONTROL_SHORT,
            "{uri}"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let doc: Value = serde_json::from_slice(&body).unwrap();
        if first {
            let this = link(&doc, "self").unwrap();
            assert!(
                !this.contains("datetime"),
                "self repeats the request: {this}"
            );
            first = false;
        }
        seen.extend(ids(&doc));
        let Some(next) = link(&doc, "next") else {
            break;
        };
        assert!(next.contains("&datetime="), "{next}");
        uri = next.strip_prefix("/features").unwrap().to_string();
    }
    let windows = engine.windows.lock().unwrap().clone();
    assert!(windows.len() >= 6, "{} pages", windows.len());
    assert!(windows[0].is_some(), "the first read is the named window");
    assert!(
        windows.iter().all(|w| *w == windows[0]),
        "every page reads it: {windows:?}"
    );
    let window = windows[0].clone().unwrap();
    let expected: Vec<String> = (0..590)
        .map(|k| base - chrono::TimeDelta::seconds(6 * k))
        .filter(|t| *t >= window.start.unwrap() && *t <= window.end.unwrap())
        .map(rfc3339)
        .collect();
    assert_eq!(expected.len(), 590, "the fixture lies in the window");
    assert_eq!(
        seen, expected,
        "each event once, newest first, the oldest kept"
    );
}
