//! Admission order of EDR HTML data pages, and the answer limit of
//! `Point`-coverage answers they rest on.
//!
//! - [`parsed_document_bound`] holds for the documents EDR writes: the heap
//!   a parse takes never exceeds it, its transient peak included, nor what
//!   the document holds its retained part.
//! - A page that cannot fit the response budget even idle is a 400 naming
//!   its CoverageJSON, refused before anything is built and not counted
//!   among shed requests; one that cannot fit it now is a 503 to retry,
//!   also before anything is built, so two pages at once never both build a
//!   document; a full grid's page is served on a modest budget, and the
//!   answers that fit are unchanged.
//! - [`MAX_AREA_POINT_COVERAGES`] `Point` coverages write about five grid
//!   answers at [`MAX_AREA_VALUES`] as CoverageJSON.
//!
//! The binary counts its heap through its own allocator, so its tests take
//! one lock and run one at a time. Run them alone, in a debug build or
//! with the server's `preserve_order`:
//!
//! ```sh
//! cargo test -p api-edr --test html_admission_tests
//! cargo test -p api-edr --features serde_json/preserve_order --test html_admission_tests
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use api_edr::handlers::EdrState;
use api_edr::response::{parsed_document_bound, CoverageJson};
use ds_core::config::CollectionConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::error::DataServerError;
use ds_core::feature::{MAX_AREA_POINT_COVERAGES, MAX_AREA_VALUES};
use ds_core::model::*;

// ---------------------------------------------------------------------------
// A heap counter with a high-water mark
// ---------------------------------------------------------------------------

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

struct Counting;

impl Counting {
    fn grow(bytes: usize) {
        let live = LIVE.fetch_add(bytes as isize, Ordering::SeqCst) + bytes as isize;
        PEAK.fetch_max(live, Ordering::SeqCst);
    }

    fn shrink(bytes: usize) {
        LIVE.fetch_sub(bytes as isize, Ordering::SeqCst);
    }
}

// SAFETY: every call forwards to `System` with the caller's arguments; the
// counters are bookkeeping only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::grow(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::grow(layout.size());
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        Self::shrink(layout.size());
        // SAFETY: `ptr` came from this allocator, so from `System`.
        unsafe { System.dealloc(ptr, layout) }
    }

    /// Counted as the worst case, a move: the new block is live before the
    /// old one is freed.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::grow(new_size);
        // SAFETY: as `dealloc`.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if new.is_null() {
            Self::shrink(new_size);
        } else {
            Self::shrink(layout.size());
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One test at a time: every thread's allocations count.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn live() -> isize {
    LIVE.load(Ordering::SeqCst)
}

/// Start a high-water mark at what is live now.
fn reset_peak() -> isize {
    let now = live();
    PEAK.store(now, Ordering::SeqCst);
    now
}

fn peak() -> isize {
    PEAK.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------

fn t0() -> DateTime<Utc> {
    "2026-07-01T12:00:00.123Z".parse().unwrap()
}

fn description(name: &str, unit: &str) -> ParameterDescription {
    ParameterDescription {
        label: name.into(),
        unit: unit.into(),
        observed_property: name.into(),
        standard_name: None,
    }
}

/// `n` events, one `Point` coverage each, as an events collection answers
/// an area: its own domain, one parameter, one value.
fn points(n: usize) -> CoverageResponse {
    let desc = description("peak_current", "kA");
    CoverageResponse::Collection(
        (0..n)
            .map(|i| QueryResult {
                domain: DomainDescription::Point {
                    x: 24.0 + (i % 997) as f64 * 1e-3,
                    y: 60.0 + (i / 997) as f64 * 1e-3,
                    t: Some(t0() + chrono::Duration::milliseconds(i as i64 * 37)),
                    z: None,
                },
                parameters: HashMap::from([("peak_current".to_string(), desc.clone())]),
                ranges: HashMap::from([(
                    "peak_current".to_string(),
                    NdArray {
                        shape: vec![],
                        axis_names: vec![],
                        values: vec![Some(-12.3 - (i % 50) as f64)],
                        precision: ValuePrecision::F64,
                    },
                )]),
            })
            .collect(),
    )
}

/// An `nx` × `ny` grid of one step, `value(i)` at cell `i`.
fn grid(nx: usize, ny: usize, value: impl Fn(usize) -> Option<f64>) -> CoverageResponse {
    CoverageResponse::Single(QueryResult {
        domain: DomainDescription::Grid {
            x: (0..nx).map(|i| 20.0 + i as f64 * 0.01).collect(),
            y: (0..ny).map(|j| 60.0 + j as f64 * 0.01).collect(),
            t: Some(vec![t0()]),
            z: None,
            coarsened: None,
        },
        parameters: HashMap::from([("t".to_string(), description("t", "K"))]),
        ranges: HashMap::from([(
            "t".to_string(),
            NdArray {
                shape: vec![1, ny, nx],
                axis_names: vec!["t".into(), "y".into(), "x".into()],
                values: (0..nx * ny).map(value).collect(),
                precision: ValuePrecision::F32,
            },
        )]),
    })
}

/// A grid of realistic values at the value budget.
fn full_grid() -> CoverageResponse {
    let side = (MAX_AREA_VALUES as f64).sqrt() as usize;
    grid(side, side, |k| Some(250.0 + (k % 977) as f64 * 0.0137))
}

/// `stations` station series of `steps` steps and three parameters.
fn station_series(stations: usize, steps: usize) -> CoverageResponse {
    let names = ["air_temperature", "relative_humidity", "wind_speed"];
    CoverageResponse::Collection(
        (0..stations)
            .map(|s| QueryResult {
                domain: DomainDescription::PointSeries {
                    x: 20.0 + s as f64 * 0.01,
                    y: 60.0,
                    t: (0..steps)
                        .map(|h| t0() + chrono::Duration::hours(h as i64))
                        .collect(),
                    z: None,
                },
                parameters: names
                    .iter()
                    .map(|n| (n.to_string(), description(n, "1")))
                    .collect(),
                ranges: names
                    .iter()
                    .map(|n| {
                        (
                            n.to_string(),
                            NdArray {
                                shape: vec![steps],
                                axis_names: vec!["t".into()],
                                values: (0..steps).map(|h| Some(h as f64 * 0.5)).collect(),
                                precision: ValuePrecision::F64,
                            },
                        )
                    })
                    .collect(),
            })
            .collect(),
    )
}

fn body(result: &CoverageResponse) -> Vec<u8> {
    serde_json::to_vec(&CoverageJson(result)).unwrap()
}

// ---------------------------------------------------------------------------
// A router over one collection whose area answer is fixed
// ---------------------------------------------------------------------------

/// An engine whose area answer is fixed. With `together`, every query
/// waits at the barrier first, so two queries reach their answers at once.
struct Fixed {
    answer: fn() -> CoverageResponse,
    together: Option<Arc<std::sync::Barrier>>,
}

impl EdrEngine for Fixed {
    fn get_locations(&self) -> Result<Arc<LocationSnapshot>, DataServerError> {
        Ok(LocationSnapshot::empty())
    }
    fn query_location(
        &self,
        id: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        Err(DataServerError::LocationNotFound(id.into()))
    }
    fn get_parameters(&self) -> Vec<String> {
        vec!["t".into(), "peak_current".into()]
    }
    fn get_temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        Some((t0(), t0()))
    }
    fn get_spatial_extent(&self) -> Option<[f64; 4]> {
        Some([20.0, 55.0, 30.0, 70.0])
    }
    fn supported_query_types(&self) -> Vec<String> {
        vec!["area".into()]
    }
    fn query_area(
        &self,
        _: &str,
        _: Option<(DateTime<Utc>, DateTime<Utc>)>,
        _: Option<&[String]>,
        _: Option<&[f64]>,
        _: Option<DateTime<Utc>>,
    ) -> Result<CoverageResponse, DataServerError> {
        if let Some(together) = &self.together {
            together.wait();
        }
        Ok((self.answer)())
    }
}

fn app(answer: fn() -> CoverageResponse) -> axum::Router {
    app_with(Fixed {
        answer,
        together: None,
    })
}

fn app_with(engine: Fixed) -> axum::Router {
    let config = CollectionConfig {
        id: "c".into(),
        title: "Answers".into(),
        description: "A collection with a fixed area answer".into(),
        data_path: None,
        apis: vec!["edr".into()],
        engine_type: "postgis".into(),
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
        station_grid: None,
        period: None,
        bufr: None,
        satellite: None,
        netcdf: None,
        csv: None,
        preview: None,
        derive_wind: None,
        derive_geopotential_height: None,
        discovery: None,
        derive_humidity: None,
        expected_interval: None,
    };
    api_edr::router(Arc::new(ArcSwap::from_pointee(EdrState {
        engines: HashMap::from([("c".to_string(), Arc::new(engine) as Arc<dyn EdrEngine>)]),
        feature_engines: HashMap::new(),
        collections: HashMap::from([("c".to_string(), config)]),
        styles: HashMap::new(),
        base_url: String::new(),
        trust_proxy_headers: false,
        discovery: Default::default(),
    })))
}

const AREA: &str = "/collections/c/area?coords=POLYGON((20%2060,21%2060,21%2061,20%2061,20%2060))";

/// The response to `uri`, its body unread: held, it stays charged to the
/// response budget.
async fn respond(app: axum::Router, uri: &str) -> axum::response::Response {
    app.oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// The status, its `Retry-After`, and the body.
async fn get(app: axum::Router, uri: &str) -> (StatusCode, Option<String>, Vec<u8>) {
    let response = respond(app, uri).await;
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, retry_after, bytes.to_vec())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Run `test` in a child process of this binary with `env` set: the
/// response budget is process-wide and read once.
fn in_child(test: &str, env: &[(&str, &str)], body: impl FnOnce()) {
    const CHILD: &str = "MC_TEST_HTML_ADMISSION_CHILD";
    if std::env::var(CHILD).is_ok() {
        return body();
    }
    // The parent allocates too while it waits: one test at a time here as
    // well, or it counts in another test's measurement.
    let _serial = serial();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, "1")
        .envs(env.iter().copied())
        .status()
        .unwrap();
    assert!(status.success(), "{test} child failed");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The heap a parse of each document takes stays within
/// [`parsed_document_bound`] for the shapes EDR answers: what the document
/// holds within its retained part, its high-water mark within the whole. A
/// grid of the shortest values, one with gaps, a full grid, one `Point`
/// coverage per event, station series. The full grid's bound stays close to
/// its peak, so a page the budget can hold is not refused for the bound.
#[test]
fn the_parsed_document_bound_holds_for_every_answer_shape() {
    let _serial = serial();
    let shapes: [(&str, CoverageResponse); 5] = [
        ("grid of zeros", grid(300, 300, |_| Some(0.0))),
        (
            "grid with gaps",
            grid(300, 300, |k| (k % 3 != 0).then_some(273.15)),
        ),
        ("full grid", full_grid()),
        ("points", points(5_000)),
        ("station series", station_series(500, 48)),
    ];
    for (name, answer) in shapes {
        let json = body(&answer);
        drop(answer);
        let bound = parsed_document_bound(&json);
        let before = reset_peak();
        let document: serde_json::Value = serde_json::from_slice(&json).unwrap();
        let held = (live() - before) as usize;
        let peak = (peak() - before) as usize;
        drop(document);
        println!(
            "{name}: body {} B; document {held} B, bound {} B; parse peak {peak} B, bound {} B",
            json.len(),
            bound.retained,
            bound.peak()
        );
        assert!(
            held <= bound.retained,
            "{name}: the document holds {held} B, past its bound {} B",
            bound.retained
        );
        assert!(
            peak <= bound.peak(),
            "{name}: a parse peaked at {peak} B, past its bound {} B",
            bound.peak()
        );
        if name == "full grid" {
            assert!(
                bound.peak() * 4 <= peak * 5,
                "{name}: the bound {} B is far past the peak {peak} B",
                bound.peak()
            );
        }
    }
}

/// With a response budget that cannot hold a page even idle, the HTML page
/// is a 400 naming its CoverageJSON, never a 503 to retry, and not counted
/// among shed requests; it is refused before anything is built, so the
/// request's heap never reaches the document's size. The CoverageJSON and a
/// small page are still served, and every reservation is released.
#[test]
fn an_html_page_over_the_whole_budget_is_a_400_before_its_document_is_built() {
    in_child(
        "an_html_page_over_the_whole_budget_is_a_400_before_its_document_is_built",
        &[("MC_EDR_RESPONSE_MEMORY_MB", "4")],
        || {
            let _serial = serial();
            fn many() -> CoverageResponse {
                points(3_000)
            }
            fn few() -> CoverageResponse {
                points(10)
            }
            // What the document the page would lay out takes once built.
            let json = body(&many());
            let before = live();
            let document: serde_json::Value = serde_json::from_slice(&json).unwrap();
            let document_bytes = (live() - before) as usize;
            drop(document);
            assert!(json.len() < 2 << 20, "the body itself fits the budget");
            assert!(
                parsed_document_bound(&json).peak() > 4 << 20,
                "its document does not"
            );

            let rt = runtime();
            let shed = api_edr::response_budget_metrics().2;
            let before = reset_peak();
            let (status, retry_after, page) =
                rt.block_on(get(app(many), &format!("{AREA}&f=html")));
            let request = (peak() - before) as usize;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(retry_after, None, "no retry can admit it");
            assert_eq!(
                api_edr::response_budget_metrics().2,
                shed,
                "an oversize page is not counted among shed requests"
            );
            let page = String::from_utf8_lossy(&page);
            assert!(page.contains("f=CoverageJSON"), "{page}");
            println!("refused page: request peak {request} B, its document {document_bytes} B");
            assert!(
                request < document_bytes,
                "the refused page built its document: request peak {request} B"
            );
            assert_eq!(api_edr::response_budget_metrics().0, 0);

            let (status, _, _) = rt.block_on(get(app(many), AREA));
            assert_eq!(status, StatusCode::OK, "the CoverageJSON still fits");
            let (status, _, page) = rt.block_on(get(app(few), &format!("{AREA}&f=html")));
            assert_eq!(status, StatusCode::OK, "a small page is served");
            assert!(!page.is_empty());
            assert_eq!(api_edr::response_budget_metrics().0, 0);
        },
    );
}

/// A page whose parse fits the whole budget but whose render, the document
/// beside the page, does not is the 400 too, decided before anything is
/// built: the page's size is estimated from the result up front, never
/// found at the page step after the document exists. The budget sits
/// between the two, worked out from the full grid's own bound.
#[test]
fn a_page_whose_render_cannot_fit_is_refused_before_its_document_is_built() {
    const MIB: usize = 1 << 20;
    let json = body(&full_grid());
    let bound = parsed_document_bound(&json);
    let parse = json.len() + bound.peak();
    let budget = (parse.div_ceil(MIB) + 2).to_string();
    in_child(
        "a_page_whose_render_cannot_fit_is_refused_before_its_document_is_built",
        &[("MC_EDR_RESPONSE_MEMORY_MB", &budget)],
        || {
            let _serial = serial();
            let before = live();
            let document: serde_json::Value = serde_json::from_slice(&json).unwrap();
            let document_bytes = (live() - before) as usize;
            drop(document);
            drop(json);

            let rt = runtime();
            let before = reset_peak();
            let (status, retry_after, page) =
                rt.block_on(get(app(full_grid), &format!("{AREA}&f=html")));
            let request = (peak() - before) as usize;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(retry_after, None);
            assert!(String::from_utf8_lossy(&page).contains("f=CoverageJSON"));
            println!("refused render: request peak {request} B, its document {document_bytes} B");
            assert!(
                request < document_bytes,
                "the refused page built its document: request peak {request} B"
            );
            let (status, _, _) = rt.block_on(get(app(full_grid), AREA));
            assert_eq!(status, StatusCode::OK, "the CoverageJSON still fits");
            assert_eq!(api_edr::response_budget_metrics().0, 0);
        },
    );
}

/// A page that fits an idle budget but not one other bodies hold is the
/// 503 `ServerBusy` to retry, before anything is built, and is served once
/// they are released.
#[test]
fn an_html_page_that_fits_an_idle_budget_is_a_503_while_it_is_busy() {
    in_child(
        "an_html_page_that_fits_an_idle_budget_is_a_503_while_it_is_busy",
        &[("MC_EDR_RESPONSE_MEMORY_MB", "48")],
        || {
            let _serial = serial();
            fn page() -> CoverageResponse {
                points(2_000)
            }
            let html = format!("{AREA}&f=html");
            let rt = runtime();
            let (status, _, _) = rt.block_on(get(app(page), &html));
            assert_eq!(status, StatusCode::OK, "it fits the idle budget");

            // Two full-grid CoverageJSON bodies, unread, hold most of it.
            let held: Vec<_> = (0..2)
                .map(|_| rt.block_on(respond(app(full_grid), AREA)))
                .collect();
            assert!(held.iter().all(|r| r.status() == StatusCode::OK));
            let (status, retry_after, _) = rt.block_on(get(app(page), &html));
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(retry_after.as_deref(), Some("1"));
            // So is a third CoverageJSON body, which an idle budget holds:
            // only one that no idle budget could hold is a 400 (#1500).
            let (status, retry_after, _) = rt.block_on(get(app(full_grid), AREA));
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(retry_after.as_deref(), Some("1"));

            drop(held);
            let (status, _, _) = rt.block_on(get(app(page), &html));
            assert_eq!(status, StatusCode::OK, "served once they are released");
            assert_eq!(api_edr::response_budget_metrics().0, 0);
        },
    );
}

/// The most the response budget held at once since the previous call.
fn response_budget_peak() -> usize {
    // The budget enters the plan when it is first used.
    api_edr::response_budget_metrics();
    ds_cache::plan::take_peaks()
        .into_iter()
        .find(|(entry, _)| entry.id == "edr_response")
        .map(|(_, peak)| peak as usize)
        .expect("the EDR response budget is in the memory plan")
}

/// What a page of `answer` reserves, measured at the default budget, in
/// bytes.
struct PageReservation {
    /// The document's part.
    document: usize,
    /// The body with the parse's transient part.
    parse: usize,
    /// The page's part: its estimate, which its render holds.
    estimate: usize,
    /// What the unread response keeps of it: the page itself (#1500).
    held: usize,
    /// The page's length.
    page: usize,
}

/// The page of `answer`'s reservation. The page's estimate outweighs the
/// parse, so the reservation's peak, the document's part and the larger of
/// the two, names it.
fn page_reservation(answer: fn() -> CoverageResponse) -> PageReservation {
    let _serial = serial();
    let json = body(&answer());
    let bound = parsed_document_bound(&json);
    let parse = json.len() + bound.transient;
    let rt = runtime();
    response_budget_peak();
    let response = rt.block_on(respond(app(answer), &format!("{AREA}&f=html")));
    assert_eq!(response.status(), StatusCode::OK);
    let held = api_edr::response_budget_metrics().0 as usize;
    let whole = response_budget_peak();
    let page = rt
        .block_on(response.into_body().collect())
        .unwrap()
        .to_bytes()
        .len();
    assert_eq!(api_edr::response_budget_metrics().0, 0);
    let estimate = whole - bound.retained;
    assert!(
        estimate > parse,
        "the page's estimate {estimate} B does not outweigh the parse {parse} B"
    );
    PageReservation {
        document: bound.retained,
        parse,
        estimate,
        held,
        page,
    }
}

/// The engine result and the document of a page of `answer`, as the heap
/// counts them.
fn engine_and_document(answer: fn() -> CoverageResponse) -> (usize, usize) {
    let before = live();
    let result = answer();
    let engine = (live() - before) as usize;
    drop(result);
    let json = body(&answer());
    let before = live();
    let document: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let document_bytes = (live() - before) as usize;
    drop(document);
    (engine, document_bytes)
}

fn three_thousand_points() -> CoverageResponse {
    points(3_000)
}

/// A page the budget holds alone, asked for while another page's body is
/// held unread, is the 503 to retry, refused before it builds anything:
/// its heap stays near its engine result. The budget fits one page and not
/// one beside another's page.
#[test]
fn a_page_beside_a_held_page_is_refused_before_it_builds() {
    const MIB: usize = 1 << 20;
    let page = page_reservation(three_thousand_points);
    let whole = page.document + page.estimate;
    assert!(
        page.held > MIB,
        "a held page {} B outweighs the rounding",
        page.held
    );
    let budget = whole.div_ceil(MIB).to_string();
    in_child(
        "a_page_beside_a_held_page_is_refused_before_it_builds",
        &[("MC_EDR_RESPONSE_MEMORY_MB", &budget)],
        || {
            let _serial = serial();
            let (engine, document_bytes) = engine_and_document(three_thousand_points);
            let html = format!("{AREA}&f=html");
            let rt = runtime();
            let first = rt.block_on(respond(app(three_thousand_points), &html));
            assert_eq!(first.status(), StatusCode::OK);
            let before = reset_peak();
            let (status, retry_after, _) = rt.block_on(get(app(three_thousand_points), &html));
            let request = (peak() - before) as usize;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(retry_after.as_deref(), Some("1"));
            println!(
                "refused beside a page: request peak {request} B, engine result {engine} B, \
                 document {document_bytes} B"
            );
            assert!(
                request < engine + document_bytes / 8,
                "the refused page built its document: request peak {request} B"
            );
            drop(first);
            assert_eq!(api_edr::response_budget_metrics().0, 0);
        },
    );
}

/// Two pages at once on a budget that holds both documents with their
/// bodies, but not both documents with a page beside them: each would build
/// its document and then find no room for its page, both a 503 after the
/// work. A page reserves all it holds up front instead, so the one that
/// reserves first is served and the other is the 503 before it builds:
/// one document is built at a time, never two. The engine releases both
/// answers at once, so both pages are in flight together. A built page
/// keeps only its length of the estimate (#1500), which leaves room for
/// the other: one reserving that late, after the first is built, is served
/// too, and still builds its document alone.
#[test]
fn two_pages_at_once_that_fit_only_one_at_a_time_never_both_build() {
    const MIB: usize = 1 << 20;
    let PageReservation {
        document,
        parse,
        estimate,
        ..
    } = page_reservation(three_thousand_points);
    // Both documents and bodies fit; both documents and a page do not.
    let both_parsed = 2 * (document + parse);
    let both_rendering = 2 * document + estimate;
    assert!(
        both_rendering > both_parsed + 2 * MIB,
        "no budget sits between {both_parsed} B and {both_rendering} B"
    );
    let budget = ((both_parsed + both_rendering) / 2)
        .div_ceil(MIB)
        .to_string();
    in_child(
        "two_pages_at_once_that_fit_only_one_at_a_time_never_both_build",
        &[("MC_EDR_RESPONSE_MEMORY_MB", &budget)],
        || {
            let _serial = serial();
            let (_, document_bytes) = engine_and_document(three_thousand_points);
            let html = format!("{AREA}&f=html");
            let app = app_with(Fixed {
                answer: three_thousand_points,
                together: Some(Arc::new(std::sync::Barrier::new(2))),
            });
            let rt = runtime();
            let before = reset_peak();
            let (a, b) = rt.block_on(async {
                tokio::join!(respond(app.clone(), &html), respond(app.clone(), &html))
            });
            let pair = (peak() - before) as usize;
            let mut statuses = [a.status().as_u16(), b.status().as_u16()];
            statuses.sort_unstable();
            assert!(
                matches!(statuses, [200, 503] | [200, 200]),
                "one page is served, and never both refused: {statuses:?}"
            );
            println!(
                "two at once: {statuses:?}, heap peak {pair} B, one document {document_bytes} B"
            );
            assert!(
                pair < 2 * document_bytes,
                "both pages built a document: heap peak {pair} B"
            );
            drop((a, b));
            assert_eq!(api_edr::response_budget_metrics().0, 0);
        },
    );
}

/// A page's response keeps the page it holds, not the estimate it was
/// admitted with (#1500): an events page is a fraction of its estimate, and
/// a body held until delivery keeps only that much of the budget.
#[test]
fn a_held_page_keeps_its_length_not_its_estimate() {
    let page = page_reservation(three_thousand_points);
    println!(
        "events page: {} B, estimate {} B, held {} B",
        page.page, page.estimate, page.held
    );
    assert_eq!(page.held, page.page, "the page, shrunk to its length");
    assert!(
        page.held * 2 < page.estimate,
        "an events page is a fraction of its estimate"
    );
}

/// The HTML page of a full million-value grid is served on the response
/// budget `[memory] mode = "auto"` grants a small host: the bound does not
/// refuse a page the budget can hold.
#[test]
fn a_full_grid_html_page_is_served_on_a_small_host_budget() {
    in_child(
        "a_full_grid_html_page_is_served_on_a_small_host_budget",
        &[("MC_EDR_RESPONSE_MEMORY_MB", "204")],
        || {
            let _serial = serial();
            let rt = runtime();
            let (status, _, page) = rt.block_on(get(app(full_grid), &format!("{AREA}&f=html")));
            assert_eq!(status, StatusCode::OK);
            assert!(!page.is_empty());
            assert_eq!(api_edr::response_budget_metrics().0, 0);
        },
    );
}

/// [`MAX_AREA_POINT_COVERAGES`] `Point` coverages write about five grid
/// answers at [`MAX_AREA_VALUES`] as CoverageJSON: the cost the limit's
/// comment states. Each coverage writes the same layout, so a sample's
/// bytes per coverage scale to the limit.
#[test]
fn point_coverages_at_their_limit_write_about_five_full_grid_answers() {
    let _serial = serial();
    const SAMPLE: usize = 10_000;
    let per_coverage = body(&points(SAMPLE)).len() as f64 / SAMPLE as f64;
    let at_limit = per_coverage * MAX_AREA_POINT_COVERAGES as f64;
    let grid = body(&full_grid()).len() as f64;
    let ratio = at_limit / grid;
    println!("CoverageJSON at the limit: {ratio:.2} full grid answers");
    assert!((4.0..=6.0).contains(&ratio), "{ratio:.2} full grid answers");
}
