//! The GetCapabilities document, built once per base URL and served for
//! [`CAPABILITIES_TTL`], gzipped once (#1482).
//!
//! The document grows with collections × layers × time values: 837 KB for
//! 40 collections and 443 named layers, 2–5 ms to build and about 5.5 ms more
//! to gzip at level 6. Built per request on the handler's task, and gzipped
//! there by the server's `CompressionLayer`, it held a request worker for
//! both. Now a miss builds and compresses on the blocking pool, in a task of
//! its own that no client disconnecting can cancel, concurrent misses share
//! that one build, and every request inside the window is served the same
//! bytes, the gzipped ones to a client that takes gzip.
//!
//! - **Key.** The base URL the request's links are written with
//!   ([`crate::handlers::WmsState`]'s `base_url`, or the forwarding headers
//!   with `trust_proxy_headers`), so a document never carries another host's
//!   links. Nothing else in the request changes the document: the handler
//!   reads no other parameter for GetCapabilities.
//! - **Reload.** Each document remembers the state it was built from. A
//!   reload swaps in a new state, and the first request that loads it drops
//!   every document built from the old one, whatever their age.
//! - **New data.** Engines change their time steps without a reload, so a
//!   document is served for at most [`CAPABILITIES_TTL`] after its slot was
//!   made, which is when its build started or earlier.
//! - **Bound.** At most [`MAX_BASE_URLS`] documents, the oldest dropped
//!   first, and at most [`BUILD_PERMITS`] builds at once. A deployment has
//!   one base URL, or a few behind proxies; forged forwarding headers can
//!   cycle the slots and queue for the permits, never hold more documents
//!   or run more builds. The bound is on documents, not bytes: every link
//!   repeats the base URL, so a long forged host makes a long document.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tokio::sync::{OnceCell, Semaphore};
// Tokio's clock, which a test can pause and advance.
use tokio::time::Instant;

use crate::error::WmsError;
use crate::handlers::WmsState;

/// How long a document is served after its build started: 5 s.
///
/// New time steps reach GetCapabilities at most this late. That is far
/// inside the shortest data cadence served (5-minute radar frames) and
/// shorter than clients poll capabilities, while a burst of clients opening
/// a map shares one build. A reload shows at once ([`CapabilitiesCache`]).
pub const CAPABILITIES_TTL: Duration = Duration::from_secs(5);

/// The most base URLs whose documents are held at once.
pub const MAX_BASE_URLS: usize = 4;

/// The most documents built at once, on the blocking pool. Builds are rare,
/// one per base URL per window; two let two hosts' builds overlap, and
/// forged forwarding headers wait for a permit instead of filling the pool.
pub const BUILD_PERMITS: usize = 2;

/// The documents of one WMS router, which outlives the states reloads swap
/// in.
pub struct CapabilitiesCache {
    ttl: Duration,
    slots: Mutex<Vec<Slot>>,
    /// Shared with the fill tasks, which outlive the requests that started
    /// them.
    builds: Arc<AtomicU64>,
    permits: Arc<Semaphore>,
}

/// One base URL's document, built or being built.
struct Slot {
    base_url: String,
    /// The state the document is built from. A `Weak` keeps its allocation,
    /// so no later state can have its address while the slot lives, and
    /// keeps none of its engines.
    state: Weak<WmsState>,
    /// When the slot was made. Its build starts then, or later when it
    /// waits for a permit, so its document is never older than this.
    since: Instant,
    /// Filled by the fill task of the first request that misses; requests
    /// arriving while it builds wait for it, asynchronously.
    document: Arc<OnceCell<Arc<Document>>>,
}

/// A built document: its bytes, gzipped once, and their ETag.
pub(crate) struct Document {
    xml: Bytes,
    /// `None` only when compressing failed: the document is then served
    /// as is.
    gzip: Option<Bytes>,
    /// Weak, `W/"…"`: the gzipped and the plain bytes are one document.
    etag: HeaderValue,
}

impl CapabilitiesCache {
    /// A cache serving each document for `ttl` after its build started.
    /// `Duration::ZERO` builds every request's document.
    pub fn new(ttl: Duration) -> Self {
        CapabilitiesCache {
            ttl,
            slots: Mutex::new(Vec::new()),
            builds: Arc::new(AtomicU64::new(0)),
            permits: Arc::new(Semaphore::new(BUILD_PERMITS)),
        }
    }

    /// How many documents this cache has built.
    pub fn builds(&self) -> u64 {
        self.builds.load(Ordering::Relaxed)
    }

    /// The document `state` serves to requests whose links start with
    /// `base_url`: one built within the window, else a new one, built on
    /// the blocking pool once however many requests ask for it meanwhile.
    ///
    /// A miss fills the slot in a task of its own and awaits its
    /// `JoinHandle`, so a client that disconnects drops only its wait: the
    /// build goes on, and the requests waiting for it, or arriving later,
    /// get its document instead of building another.
    pub(crate) async fn document(
        &self,
        state: &Arc<WmsState>,
        base_url: String,
    ) -> Result<Arc<Document>, WmsError> {
        let cell = self.slot(state, &base_url);
        if let Some(document) = cell.get() {
            return Ok(Arc::clone(document));
        }
        let (builds, permits) = (Arc::clone(&self.builds), Arc::clone(&self.permits));
        let state = Arc::clone(state);
        let fill = tokio::spawn(async move {
            cell.get_or_try_init(|| async move {
                let _permit = permits.acquire().await.map_err(|_| {
                    WmsError::Internal("The capabilities build permits are closed".into())
                })?;
                builds.fetch_add(1, Ordering::Relaxed);
                tokio::task::spawn_blocking(move || Arc::new(Document::build(&state, &base_url)))
                    .await
                    .map_err(|e| {
                        WmsError::Internal(format!(
                            "Building the capabilities document failed: {e}"
                        ))
                    })
            })
            .await
            .cloned()
        });
        fill.await
            .map_err(|e| WmsError::Internal(format!("The capabilities fill task failed: {e}")))?
    }

    /// The slot of `base_url` for `state`, made if there is none. The lock
    /// is held for a scan of at most [`MAX_BASE_URLS`] slots, never across
    /// a build or an await.
    fn slot(&self, state: &Arc<WmsState>, base_url: &str) -> Arc<OnceCell<Arc<Document>>> {
        let now = Instant::now();
        let current = Arc::as_ptr(state);
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        // Documents of a state a reload replaced, or past the window, are
        // never served again.
        slots.retain(|slot| {
            std::ptr::eq(slot.state.as_ptr(), current) && now.duration_since(slot.since) < self.ttl
        });
        if let Some(slot) = slots.iter().find(|slot| slot.base_url == base_url) {
            return Arc::clone(&slot.document);
        }
        if slots.len() >= MAX_BASE_URLS {
            let oldest = slots
                .iter()
                .enumerate()
                .min_by_key(|(_, slot)| slot.since)
                .map(|(i, _)| i);
            if let Some(oldest) = oldest {
                slots.swap_remove(oldest);
            }
        }
        let document = Arc::new(OnceCell::new());
        slots.push(Slot {
            base_url: base_url.to_owned(),
            state: Arc::downgrade(state),
            since: now,
            document: Arc::clone(&document),
        });
        document
    }
}

/// Sizes, not bytes: a document is up to megabytes.
impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document")
            .field("xml_bytes", &self.xml.len())
            .field("gzip_bytes", &self.gzip.as_ref().map(Bytes::len))
            .field("etag", &self.etag)
            .finish()
    }
}

impl Document {
    /// Build `state`'s document with links below `base_url`, gzip it and
    /// tag it. CPU work: run it on the blocking pool.
    fn build(state: &WmsState, base_url: &str) -> Document {
        let xml = crate::capabilities::get_capabilities_xml(
            &state.engines,
            &state.collections,
            &state.styles,
            &state.point_engines,
            base_url,
        );
        let etag = HeaderValue::try_from(ds_core::http_cache::weak_etag_of(&xml))
            .expect("a weak quoted-hex ETag is a header value");
        let gzip = match gzip(&xml) {
            Ok(gzip) => Some(Bytes::from(gzip)),
            Err(e) => {
                tracing::warn!("Compressing the capabilities document failed: {e}");
                None
            }
        };
        Document {
            xml: Bytes::from(xml),
            gzip,
            etag,
        }
    }

    /// The response to a request with `headers`: 304 when its
    /// `If-None-Match` names the document, else the document, gzipped when
    /// the client takes gzip. Every answer carries `Vary: Accept-Encoding`.
    /// The server's `CompressionLayer` adds it only to a response its
    /// predicate would compress, so never to one that already has a
    /// `Content-Encoding` nor to a 304's empty body, and the router must
    /// answer right without the layer too.
    pub(crate) fn response(&self, headers: &HeaderMap) -> Response {
        let vary = HeaderValue::from_static("accept-encoding");
        let not_modified = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|value| value.to_str().ok())
            .zip(self.etag.to_str().ok())
            .is_some_and(|(if_none_match, etag)| {
                ds_core::http_cache::if_none_match_matches(if_none_match, etag)
            });
        if not_modified {
            return (
                StatusCode::NOT_MODIFIED,
                [(header::ETAG, self.etag.clone()), (header::VARY, vary)],
            )
                .into_response();
        }
        let gzip = self.gzip.as_ref().filter(|_| accepts_gzip(headers));
        let mut response = (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("text/xml")),
                (
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                ),
                (header::VARY, vary),
                (header::ETAG, self.etag.clone()),
            ],
            gzip.unwrap_or(&self.xml).clone(),
        )
            .into_response();
        if gzip.is_some() {
            // The `CompressionLayer` never compresses a response that has a
            // `Content-Encoding`, so these bytes pass it as they are.
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        }
        response
    }
}

/// `bytes` gzipped at level 6, the server's `CompressionLayer` default.
fn gzip(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(bytes.len() / 8),
        flate2::Compression::default(),
    );
    encoder.write_all(bytes)?;
    encoder.finish()
}

/// A coding the server's `CompressionLayer` can answer in. The order is
/// its preference between equal weights: gzip over identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Coding {
    Identity,
    Gzip,
}

/// Whether the client takes gzip, by its `Accept-Encoding` (RFC 9110
/// §12.5.3), as tower-http 0.7.1's `CompressionLayer`, gzip only, decides
/// it. A listing with a malformed weight is skipped. Without `*`, the
/// heaviest listing of gzip (or `x-gzip`) and `identity` with a weight
/// above zero wins, gzip on a tie, and nothing listed means identity. With
/// `*`, each coding weighs as its first listing, or as the first `*`, and
/// gzip wins when it weighs above zero and no less than identity.
fn accepts_gzip(headers: &HeaderMap) -> bool {
    let mut listed: Vec<(Coding, u16)> = Vec::new();
    let mut any = None;
    let codings = headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','));
    for coding in codings {
        let mut parts = coding.splitn(2, ';');
        let name = parts.next().unwrap_or_default().trim();
        let weight = match parts.next() {
            Some(weight) => match parse_weight(weight.trim()) {
                Some(weight) => weight,
                None => continue,
            },
            None => 1000,
        };
        if name.eq_ignore_ascii_case("gzip") || name.eq_ignore_ascii_case("x-gzip") {
            listed.push((Coding::Gzip, weight));
        } else if name.eq_ignore_ascii_case("identity") {
            listed.push((Coding::Identity, weight));
        } else if name == "*" {
            any.get_or_insert(weight);
        }
    }
    match any {
        None => listed
            .iter()
            .filter(|(_, weight)| *weight > 0)
            .max_by_key(|&&(coding, weight)| (weight, coding))
            .is_some_and(|&(coding, _)| coding == Coding::Gzip),
        Some(any) => {
            let weight = |coding: Coding| {
                listed
                    .iter()
                    .find(|(listed, _)| *listed == coding)
                    .map_or(any, |&(_, weight)| weight)
            };
            let gzip = weight(Coding::Gzip);
            gzip > 0 && gzip >= weight(Coding::Identity)
        }
    }
}

/// A `q=` weight in thousandths: `q=0`, `q=1`, `q=0.5`, up to three
/// decimals and no space around `=` (RFC 9110 §12.4.2). `None` when
/// malformed.
fn parse_weight(weight: &str) -> Option<u16> {
    let value = weight.strip_prefix(['q', 'Q'])?.strip_prefix('=')?;
    let (whole, decimals) = value.split_once('.').unwrap_or((value, ""));
    let whole = match whole {
        "0" => 0,
        "1" => 1000,
        _ => return None,
    };
    if decimals.len() > 3 || !decimals.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut fraction = 0;
    for (digit, scale) in decimals.bytes().zip([100, 10, 1]) {
        fraction += u16::from(digit - b'0') * scale;
    }
    let weight = whole + fraction;
    (weight <= 1000).then_some(weight)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};

    /// An engine whose metadata reads take `delay` and are counted: a
    /// build reads them, a cached document does not. `reading` counts the
    /// reads under way and `most` the most at once: one build reads in turn.
    struct Slow {
        delay: Duration,
        reads: AtomicU64,
        reading: AtomicU64,
        most: AtomicU64,
    }

    impl MapEngine for Slow {
        fn get_raster_tile(
            &self,
            _bbox: [f64; 4],
            _width: u32,
            _height: u32,
            _time: Option<chrono::DateTime<chrono::Utc>>,
            _output_crs: &OutputCrs,
            _parameter: Option<&str>,
            _z: Option<f64>,
            _reference_time: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<RasterTile, ds_core::error::DataServerError> {
            unreachable!("capabilities draw nothing")
        }

        fn raster_info(&self) -> RasterInfo {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let reading = self.reading.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(reading, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.reading.fetch_sub(1, Ordering::SeqCst);
            RasterInfo {
                native_crs: "EPSG:4326".into(),
                spatial_extent: Some([10.0, 55.0, 30.0, 70.0]),
                times: Vec::new(),
                parameter: "reflectivity".into(),
                unit: "dBZ".into(),
                parameters: vec![],
                vertical: None,
                grid_size: None,
                layer_subtitle: None,
                reference_times: Vec::new(),
                native_grid: None,
            }
        }
    }

    fn state(engine: Arc<Slow>) -> Arc<WmsState> {
        let collection = serde_json::from_value(serde_json::json!({
            "id": "radar",
            "title": "Radar",
            "description": "A slow engine",
            "apis": ["wms"],
        }))
        .unwrap();
        Arc::new(WmsState {
            engines: HashMap::from([("radar".to_string(), engine as Arc<dyn MapEngine>)]),
            point_engines: HashMap::new(),
            collections: HashMap::from([("radar".to_string(), collection)]),
            styles: HashMap::new(),
            render_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            rendered_cache: Arc::new(ds_render::RenderedCache::new(1)),
            tile_cache: Arc::new(ds_render::TilePixelCache::new(1)),
            base_url: String::new(),
            trust_proxy_headers: false,
        })
    }

    fn slow(delay: Duration) -> Arc<Slow> {
        Arc::new(Slow {
            delay,
            reads: AtomicU64::new(0),
            reading: AtomicU64::new(0),
            most: AtomicU64::new(0),
        })
    }

    const BASE: &str = "http://localhost:8000";

    /// Requests that arrive while a document builds wait for that build:
    /// eight concurrent misses read the engine once, on one build.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_misses_build_once() {
        let engine = slow(Duration::from_millis(200));
        let state = state(engine.clone());
        let cache = Arc::new(CapabilitiesCache::new(Duration::from_secs(60)));
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let (cache, state) = (cache.clone(), state.clone());
                tokio::spawn(async move { cache.document(&state, BASE.into()).await })
            })
            .collect();
        let mut documents = Vec::new();
        for task in tasks {
            documents.push(task.await.unwrap().unwrap());
        }
        assert_eq!(cache.builds(), 1);
        assert!(documents.iter().all(|d| Arc::ptr_eq(d, &documents[0])));
        let reads = engine.reads.load(Ordering::Relaxed);
        assert!(reads > 0);
        // A further request inside the window reads nothing.
        cache.document(&state, BASE.into()).await.unwrap();
        assert_eq!(engine.reads.load(Ordering::Relaxed), reads);
    }

    /// A document is served until the window ends, then built again: on
    /// tokio's paused clock, to the millisecond.
    #[tokio::test(start_paused = true)]
    async fn a_document_is_built_again_after_the_window() {
        let state = state(slow(Duration::ZERO));
        let cache = CapabilitiesCache::new(CAPABILITIES_TTL);
        let first = cache.document(&state, BASE.into()).await.unwrap();
        tokio::time::advance(CAPABILITIES_TTL - Duration::from_millis(1)).await;
        let again = cache.document(&state, BASE.into()).await.unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(cache.builds(), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        let later = cache.document(&state, BASE.into()).await.unwrap();
        assert!(!Arc::ptr_eq(&first, &later));
        assert_eq!(cache.builds(), 2);
    }

    /// A client that disconnects mid-build cancels only its own wait: the
    /// request waiting with it gets that build's document, and so does one
    /// arriving after every requester is gone. One build each time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_request_leaves_its_build_to_the_others() {
        let engine = slow(Duration::from_millis(100));
        let state = state(engine.clone());
        let cache = Arc::new(CapabilitiesCache::new(Duration::from_secs(60)));
        let request = |url: &'static str| {
            let (cache, state) = (cache.clone(), state.clone());
            tokio::spawn(async move { cache.document(&state, url.into()).await })
        };
        let started = |cache: Arc<CapabilitiesCache>, builds: u64| async move {
            while cache.builds() < builds {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };

        let first = request(BASE);
        started(cache.clone(), 1).await;
        let waiter = request(BASE);
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        waiter.await.unwrap().unwrap();
        assert_eq!(cache.builds(), 1);

        // Every requester gone: the build still fills the slot.
        let other = "http://other:8000";
        let alone = request(other);
        started(cache.clone(), 2).await;
        alone.abort();
        assert!(alone.await.unwrap_err().is_cancelled());
        let later = request(other).await.unwrap().unwrap();
        assert_eq!(cache.builds(), 2);
        let xml = String::from_utf8(later.xml.to_vec()).unwrap();
        assert!(xml.contains("http://other:8000/wms"));
    }

    /// Builds for many base URLs at once run `BUILD_PERMITS` at a time on
    /// the blocking pool, and every request is answered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn builds_run_at_most_build_permits_at_once() {
        let engine = slow(Duration::from_millis(20));
        let state = state(engine.clone());
        let cache = Arc::new(CapabilitiesCache::new(Duration::from_secs(60)));
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let (cache, state) = (cache.clone(), state.clone());
                tokio::spawn(async move {
                    cache
                        .document(&state, format!("http://host{i}.example"))
                        .await
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(cache.builds(), 8);
        let most = engine.most.load(Ordering::SeqCst);
        assert!(
            (1..=BUILD_PERMITS as u64).contains(&most),
            "{most} builds at once"
        );
    }

    /// The build runs off the handler's task: on a current-thread runtime,
    /// where a build on the task would stop every other task, a ticker
    /// keeps ticking while a slow build runs.
    #[tokio::test]
    async fn the_build_never_runs_on_the_requests_task() {
        let state = state(slow(Duration::from_millis(100)));
        let cache = CapabilitiesCache::new(Duration::from_secs(60));
        let ticks = Arc::new(AtomicU64::new(0));
        let ticker = tokio::spawn({
            let ticks = ticks.clone();
            async move {
                loop {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        });
        tokio::task::yield_now().await;
        let before = ticks.load(Ordering::SeqCst);
        cache.document(&state, BASE.into()).await.unwrap();
        let during = ticks.load(Ordering::SeqCst) - before;
        ticker.abort();
        assert_eq!(cache.builds(), 1);
        assert!(
            during >= 10,
            "{during} ticks during a build of 100 ms or more"
        );
    }

    /// A state a reload swaps in is built at once, inside the window, and
    /// the replaced state's documents are dropped.
    #[tokio::test]
    async fn a_new_state_is_built_at_once() {
        let old = state(slow(Duration::ZERO));
        let cache = CapabilitiesCache::new(Duration::from_secs(60));
        cache.document(&old, BASE.into()).await.unwrap();
        cache
            .document(&old, "http://other:8000".into())
            .await
            .unwrap();
        let new = state(slow(Duration::ZERO));
        cache.document(&new, BASE.into()).await.unwrap();
        assert_eq!(cache.builds(), 3);
        let slots = cache.slots.lock().unwrap();
        assert_eq!(slots.len(), 1);
        assert!(std::ptr::eq(slots[0].state.as_ptr(), Arc::as_ptr(&new)));
    }

    /// Each base URL has its own document, and at most `MAX_BASE_URLS` are
    /// held: the oldest goes first.
    #[tokio::test]
    async fn base_urls_have_their_own_documents_and_a_bound() {
        let state = state(slow(Duration::ZERO));
        let cache = CapabilitiesCache::new(Duration::from_secs(60));
        let urls: Vec<String> = (0..=MAX_BASE_URLS)
            .map(|i| format!("http://host{i}.example"))
            .collect();
        for url in &urls {
            let document = cache.document(&state, url.clone()).await.unwrap();
            let xml = String::from_utf8(document.xml.to_vec()).unwrap();
            assert!(xml.contains(&format!("{url}/wms")), "{url}");
            for other in urls.iter().filter(|other| *other != url) {
                assert!(!xml.contains(&format!("{other}/")), "{other} in {url}'s");
            }
        }
        assert_eq!(cache.builds(), urls.len() as u64);
        let held: Vec<String> = cache
            .slots
            .lock()
            .unwrap()
            .iter()
            .map(|slot| slot.base_url.clone())
            .collect();
        assert_eq!(held.len(), MAX_BASE_URLS);
        assert!(!held.contains(&urls[0]), "{held:?}");
        // The newest ones are still served without a build.
        cache.document(&state, urls[1].clone()).await.unwrap();
        assert_eq!(cache.builds(), urls.len() as u64);
    }

    /// The window of zero builds every request's document.
    #[tokio::test]
    async fn a_zero_window_builds_every_request() {
        let state = state(slow(Duration::ZERO));
        let cache = CapabilitiesCache::new(Duration::ZERO);
        for _ in 0..3 {
            cache.document(&state, BASE.into()).await.unwrap();
        }
        assert_eq!(cache.builds(), 3);
    }

    fn accept(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                header::ACCEPT_ENCODING,
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn accept_encoding_is_read_as_the_compression_layer_reads_it() {
        for (values, gzip) in [
            (&[][..], false),
            (&["gzip"][..], true),
            (&["GZIP"][..], true),
            (&["x-gzip"][..], true),
            (&["deflate, gzip;q=0.5"][..], true),
            (&["gzip;q=0.5"][..], true),
            (&["gzip;q=0"][..], false),
            (&["gzip; q=0.000"][..], false),
            (&["identity"][..], false),
            (&["br, deflate"][..], false),
            (&["identity;q=1, gzip;q=0.5"][..], false),
            (&["identity;q=0.5, gzip"][..], true),
            (&["gzip, identity"][..], true),
            (&["*"][..], true),
            (&["*;q=0"][..], false),
            (&["*, gzip;q=0"][..], false),
            (&["*;q=0.5, identity"][..], false),
            (&["br", "gzip"][..], true),
            (&["gzip;q=2"][..], false),
            (&["gzip;q=abc"][..], false),
            (&["gzip;q=0.5000"][..], false),
            (&["gzip;q = 0.5"][..], false),
            (&["gzip;q=0, gzip"][..], true),
            (&["gzip;q=0.5, identity;q=0, identity"][..], false),
            (&["gzip;q=0.5", "identity;q=0.8"][..], false),
            (&["gzip;q=0.8", "identity;q=0.8"][..], true),
            (&["*;q=0.5, gzip;q=0, gzip"][..], false),
            (&["identity;q=0"][..], false),
            (&["gzip;Q=1.000"][..], true),
        ] {
            assert_eq!(accepts_gzip(&accept(values)), gzip, "{values:?}");
        }
    }
}
