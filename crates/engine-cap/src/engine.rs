//! The CAP engine: implements `FeatureEngine` (one feature per alert area) and
//! `MapEngine` (severity-shaded polygon fills) over a poll-and-swap catalog.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_poll::{FirstTick, Shutdown};

use ds_core::config::{CapConfig, Wis2Config};
use ds_core::datetime::parse_iso8601_duration;
use ds_core::error::DataServerError;
use ds_core::feature::{Bbox, Feature, FeaturePage, FeatureQuery};
use ds_core::health::LiveStatus;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_render::rasterize::{fill_polygon, Combine};
use ds_wis2::{Status as Wis2Status, StatusSnapshot as Wis2StatusSnapshot};

use crate::catalog::{BuildConfig, Catalog, CatalogStore};
use crate::source::Source;
use crate::supersede::resolve_references;
use crate::wis2::{Wis2CapSource, Wis2SourceConfig};

/// The single render parameter advertised by a CAP collection (one layer = the
/// alert set, shaded by severity code 0–4).
pub const CAP_PARAMETER: &str = "severity";

/// Upper bound on areas rasterized into one tile (pathological-input guard).
const MAX_RENDER_RECORDS: usize = 50_000;
/// WIS2 mode: how often the catalog is rebuilt when the accumulator is dirty.
/// Also the floor between two rebuilds — every rebuild advances `as_of`, the
/// TIME-less WMS cache key, so two catalogs must never share one second.
const WIS2_DIRTY_REBUILD: Duration = Duration::from_secs(5);

/// CAP alert engine. Polls a local directory or web feed, parses CAP v1.2
/// documents into a [`Catalog`], and swaps it atomically.
pub struct CapEngine {
    catalog: Arc<CatalogStore>,
    source: Arc<Source>,
    build_cfg: BuildConfig,
    collection_id: String,
    poll_interval: Duration,
    /// Edge-triggered stop signal for `poll_loop` (shared lifecycle, #481);
    /// safe even when `shutdown()` races ahead of the loop's spawn.
    shutdown: Shutdown,
    /// Set once the first `refresh()` succeeds — even with **zero** records. A
    /// healthy CAP source can legitimately have no active alerts, so "loaded"
    /// (not "non-empty") is the readiness signal; see [`Self::is_loaded`].
    loaded: AtomicBool,
    /// WIS2 mode only: subscription config (the pipeline is started from
    /// `poll_loop`, never from the constructor — see `crates/ds-wis2/CLAUDE.md`).
    wis2: Option<Wis2Runtime>,
    /// Alerts withdrawn by an Update/Cancel chain, cumulative over rebuilds
    /// (`cap_alerts_superseded_total`). Counts each identifier once per
    /// withdrawal: `superseded_ids` holds the set as of the last rebuild and
    /// only newly withdrawn identifiers increment the counter — a cancelled
    /// alert that lingers in the source until eviction is not re-counted on
    /// every rebuild.
    superseded: std::sync::atomic::AtomicU64,
    superseded_ids: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// WIS2-mode state shared between `poll_loop` and the health/metrics readers.
struct Wis2Runtime {
    config: Wis2Config,
    /// Broker/pipeline counters; swapped in by `poll_loop` once the pipeline
    /// exists (`None` until then ⇒ "connecting").
    status: ArcSwap<Option<Arc<Wis2Status>>>,
    degrade_after: Duration,
}

impl CapEngine {
    /// Construct the engine and attempt a best-effort initial load. Construction
    /// never fails on an empty/unreachable source — the collection starts empty
    /// (degraded) and the poll loop fills it in, matching the file-backed
    /// raster engines.
    pub fn new(config: &CapConfig, collection_id: &str) -> Result<Self, DataServerError> {
        let default_ttl = match &config.default_ttl {
            Some(s) => Some(parse_iso8601_duration(s)?),
            None => None,
        };
        let (source, wis2) = match &config.wis2 {
            Some(w) => {
                let src = Arc::new(Wis2CapSource::new(Wis2SourceConfig {
                    retention_grace: parse_iso8601_duration(&config.retention_grace)?,
                    max_alerts: config.max_alerts.max(1),
                    geometry_links: config.geometry_links,
                    bbox_fallback: config.bbox_fallback,
                    default_ttl,
                }));
                (
                    Source::Wis2 {
                        source: src,
                        topics: w.topics.clone(),
                    },
                    Some(Wis2Runtime {
                        config: w.clone(),
                        status: ArcSwap::from_pointee(None),
                        degrade_after: Duration::from_secs(w.degrade_after_secs.max(1)),
                    }),
                )
            }
            None => (
                Source::build(
                    config.data_path.as_deref(),
                    config.feed_url.as_deref(),
                    &config.feed_allowlist,
                )?,
                None,
            ),
        };
        // Load the optional geocode → geometry lookup once (static reference
        // data). A misconfigured path is a hard error — it's local config, unlike
        // the pollable source.
        let geocode_lookup = match &config.geocode_geometry {
            Some(path) => {
                let lk = crate::geocode::GeocodeLookup::load(
                    path,
                    &config.geocode_property,
                    config.geocode_value_name.as_deref(),
                )?;
                tracing::info!(
                    "[{collection_id}] cap: loaded {} geocode zone(s) from '{path}'",
                    lk.len()
                );
                Some(Arc::new(lk))
            }
            None => None,
        };
        let build_cfg = BuildConfig {
            language: config.language.clone(),
            status_filter: config
                .status_filter
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .collect(),
            default_ttl,
            circle_segments: config.circle_segments,
            geocode_lookup,
        };

        let catalog = Arc::new(ArcSwap::from_pointee(Catalog::empty(
            CAP_PARAMETER,
            Utc::now(),
        )));
        let engine = CapEngine {
            catalog,
            source: Arc::new(source),
            build_cfg,
            collection_id: collection_id.to_string(),
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            shutdown: Shutdown::new(),
            loaded: AtomicBool::new(false),
            wis2,
            superseded: std::sync::atomic::AtomicU64::new(0),
            superseded_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
        };

        // Best-effort initial load (so local fixtures populate immediately).
        // WIS2 mode has nothing to load until the broker delivers: it stays
        // `Degraded("waiting for broker")` until the first rebuild after
        // subscribing (see `poll_loop`).
        if engine.wis2.is_none() {
            if let Err(e) = engine.refresh() {
                tracing::warn!(
                    "[{collection_id}] cap: initial load from {} failed: {e} (will retry on poll)",
                    engine.source.label()
                );
            }
        }
        Ok(engine)
    }

    /// Whether this collection is fed by a WIS2 subscription.
    pub fn is_wis2(&self) -> bool {
        self.wis2.is_some()
    }

    /// The WIS2 accumulator (WIS2 mode only) — lets tests feed notifications
    /// without a broker.
    pub fn wis2_source(&self) -> Option<&Arc<Wis2CapSource>> {
        self.source.wis2()
    }

    /// Runtime health for the WIS2 mode: `None` for directory/feed sources
    /// (the boot snapshot stands), otherwise whether the broker session is up.
    /// A disconnect shorter than `degrade_after_secs` is not reported — the
    /// last catalog keeps serving and the session resumes with its backlog.
    pub fn live_health(&self) -> Option<LiveStatus> {
        let w = self.wis2.as_ref()?;
        let status = w.status.load();
        let Some(status) = status.as_ref() else {
            return Some(LiveStatus::Degraded {
                reason: "connecting to WIS2 broker",
            });
        };
        let snap = status.snapshot();
        if !snap.connected {
            return Some(match snap.disconnected_for_secs {
                Some(secs) if secs >= w.degrade_after.as_secs() => LiveStatus::Degraded {
                    reason: "WIS2 broker disconnected",
                },
                // Never connected yet, or a short blip inside the grace period.
                Some(_) => {
                    if self.is_loaded() {
                        LiveStatus::Ready
                    } else {
                        LiveStatus::Degraded {
                            reason: "connecting to WIS2 broker",
                        }
                    }
                }
                None => LiveStatus::Degraded {
                    reason: "connecting to WIS2 broker",
                },
            });
        }
        if !snap.subscribed {
            return Some(LiveStatus::Degraded {
                reason: "WIS2 subscription not acknowledged",
            });
        }
        Some(if self.is_loaded() {
            LiveStatus::Ready
        } else {
            LiveStatus::Degraded {
                reason: "waiting for first WIS2 catalog build",
            }
        })
    }

    /// WIS2 broker/pipeline counters for `/metrics` (`None` unless WIS2 mode
    /// and the pipeline has started).
    pub fn wis2_status(&self) -> Option<Wis2StatusSnapshot> {
        let w = self.wis2.as_ref()?;
        let guard = w.status.load();
        guard.as_ref().as_ref().map(|s| s.snapshot())
    }

    /// WIS2 accumulator counters (`None` unless WIS2 mode). Values:
    /// `(documents_ingested, documents_rejected, deletions, hints_attached,
    /// hints_rejected, evicted, alerts_held)`.
    pub fn wis2_source_stats(&self) -> Option<[u64; 7]> {
        let src = self.source.wis2()?;
        let st = &src.stats;
        Some([
            st.documents_ingested.load(Ordering::Relaxed),
            st.documents_rejected.load(Ordering::Relaxed),
            st.deletions.load(Ordering::Relaxed),
            st.hints_attached.load(Ordering::Relaxed),
            st.hints_rejected.load(Ordering::Relaxed),
            st.evicted.load(Ordering::Relaxed),
            src.len() as u64,
        ])
    }

    /// Number of alert areas in the current catalog.
    pub fn record_count(&self) -> usize {
        self.snapshot().records.len()
    }

    /// Collection id (for logging / health).
    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Whether at least one load has succeeded (regardless of record count).
    /// This is the health-readiness signal: a reachable source with **zero**
    /// active alerts is `Ready`, not `Degraded` — only a never-yet-successful
    /// load (e.g. an unreachable feed at startup) is `Degraded`.
    pub fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::Relaxed)
    }

    /// Fetch + parse the source and swap in a fresh catalog (advancing `as_of`
    /// so the TIME-less "now" view tracks expiry). Keeps the previous snapshot
    /// on an I/O failure so a transient outage doesn't blank the alerts.
    pub fn refresh(&self) -> Result<(), DataServerError> {
        self.refresh_at(Utc::now())
    }

    /// [`Self::refresh`] with an explicit "now" (the catalog's `as_of` and, in
    /// WIS2 mode, the accumulator's eviction clock) — lets tests work with
    /// captured documents whose validity has long expired.
    pub fn refresh_at(&self, now: DateTime<Utc>) -> Result<(), DataServerError> {
        let alerts = self.source.load_at(now)?;
        // Apply CAP Update/Cancel chains for every source mode: a directory
        // or feed that keeps an alert next to its cancellation must not
        // render both.
        let (alerts, withdrawn) = resolve_references(alerts);
        let superseded = {
            let mut seen = self
                .superseded_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let new = withdrawn.iter().filter(|id| !seen.contains(*id)).count();
            *seen = withdrawn.into_iter().collect();
            new
        };
        self.superseded
            .fetch_add(superseded as u64, Ordering::Relaxed);
        let as_of = now;
        let catalog = Catalog::build(
            &alerts,
            &self.build_cfg,
            &self.collection_id,
            CAP_PARAMETER,
            as_of,
        );
        tracing::info!(
            "[{}] cap: loaded {} alert area(s) ({} geocode-only, {} newly superseded) from {}",
            self.collection_id,
            catalog.records.len(),
            catalog.geocode_only_count,
            superseded,
            self.source.label()
        );
        self.catalog.store(Arc::new(catalog));
        self.loaded.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Run the poll loop on the background runtime. Exits on [`Self::shutdown`].
    ///
    /// Directory/feed: a fixed-cadence refresh. WIS2: starts the broker
    /// pipeline (must happen here — this is the only entry point guaranteed
    /// to run on `poll_runtime()`), applies every resolved notification to
    /// the accumulator, rebuilds the catalog at most every
    /// [`WIS2_DIRTY_REBUILD`] when something changed, and forces a rebuild
    /// every `poll_interval_secs` so `as_of` advances and expired alerts
    /// drop out of the "now" view while the feed is quiet.
    pub async fn poll_loop(&self) {
        match &self.wis2 {
            None => {
                let mut ticker = self.shutdown.ticker(self.poll_interval, FirstTick::Skip);
                while ticker.tick().await {
                    if let Err(e) = self.refresh() {
                        tracing::warn!("[{}] cap: poll refresh failed: {e}", self.collection_id);
                    }
                }
            }
            Some(w) => self.wis2_loop(w).await,
        }
        tracing::info!("[{}] cap: poll loop shutting down", self.collection_id);
    }

    async fn wis2_loop(&self, w: &Wis2Runtime) {
        let Some(source) = self.source.wis2().cloned() else {
            return;
        };
        let shutdown = Arc::new(Shutdown::new());
        let mut pipeline =
            match ds_wis2::spawn_pipeline(&w.config, &self.collection_id, shutdown.clone()) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(
                        "[{}] cap/wis2: cannot start subscription: {e}",
                        self.collection_id
                    );
                    return;
                }
            };
        w.status.store(Arc::new(Some(pipeline.status.clone())));
        let fetcher = pipeline.fetcher.clone();
        let mut dirty_ticker = self.shutdown.ticker(WIS2_DIRTY_REBUILD, FirstTick::Skip);
        let mut forced_ticker = self.shutdown.ticker(self.poll_interval, FirstTick::Skip);
        let mut first_build_pending = true;
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.wait() => break,
                r = pipeline.receiver.recv() => {
                    match r {
                        Some(r) => source.apply(r, &fetcher, &self.collection_id).await,
                        None => {
                            tracing::warn!("[{}] cap/wis2: pipeline ended", self.collection_id);
                            break;
                        }
                    }
                }
                _ = dirty_ticker.tick() => {
                    let subscribed = pipeline.status.is_subscribed();
                    if source.take_dirty() || (first_build_pending && subscribed) {
                        first_build_pending = false;
                        if let Err(e) = self.refresh() {
                            tracing::warn!("[{}] cap: rebuild failed: {e}", self.collection_id);
                        }
                    }
                }
                _ = forced_ticker.tick() => {
                    if let Err(e) = self.refresh() {
                        tracing::warn!("[{}] cap: periodic rebuild failed: {e}", self.collection_id);
                    }
                }
            }
        }
        // Stop the broker task with the engine (the pipeline's own Shutdown is
        // private to this loop so a reload cannot leave a subscriber behind).
        shutdown.shutdown();
    }

    /// Alerts withdrawn by Update/Cancel chains since boot.
    pub fn superseded_total(&self) -> u64 {
        self.superseded.load(Ordering::Relaxed)
    }

    /// Signal the poll loop to stop.
    pub fn shutdown(&self) {
        self.shutdown.shutdown();
    }

    fn snapshot(&self) -> arc_swap::Guard<Arc<Catalog>> {
        self.catalog.load()
    }
}

// ---------------------------------------------------------------------------
// FeatureEngine
// ---------------------------------------------------------------------------

impl ds_core::feature_engine::FeatureEngine for CapEngine {
    fn get_features(&self, query: &FeatureQuery) -> Result<FeaturePage, DataServerError> {
        let cat = self.snapshot();

        // Candidate indices: spatial index when a bbox is set (excludes
        // null-geometry areas, which can't intersect a bbox), else every area.
        let mut indices: Vec<usize> = match &query.bbox {
            Some(bbox) => cat.query_bbox(bbox),
            None => (0..cat.records.len()).collect(),
        };

        // Active-window (datetime) filter.
        if let Some(dt) = &query.datetime {
            indices.retain(|&i| cat.records[i].window.intersects(dt.start, dt.end));
        }

        let number_matched = indices.len();
        let offset = query.offset.min(number_matched);
        let end = offset.saturating_add(query.limit).min(number_matched);
        let features: Vec<Feature> = indices[offset..end]
            .iter()
            .map(|&i| to_feature(&cat.records[i]))
            .collect();
        let number_returned = features.len();
        let next_offset = (end < number_matched).then_some(end);

        Ok(FeaturePage {
            features,
            number_matched,
            number_returned,
            next_offset,
        })
    }

    fn get_feature(&self, feature_id: &str) -> Result<Feature, DataServerError> {
        let cat = self.snapshot();
        cat.get(feature_id)
            .map(to_feature)
            .ok_or_else(|| DataServerError::FeatureNotFound(feature_id.to_string()))
    }

    fn feature_count(&self) -> usize {
        self.snapshot().records.len()
    }

    fn spatial_extent(&self) -> Option<[f64; 4]> {
        self.snapshot().spatial_extent
    }

    fn temporal_extent(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        self.snapshot().temporal_extent()
    }

    fn data_version(&self) -> u64 {
        self.snapshot().data_version
    }
}

fn to_feature(rec: &crate::catalog::AreaRecord) -> Feature {
    Feature {
        // The catalog key (`rec.id`) is the *decoded* id. The emitted Feature id
        // is URL-path-safe so api-features' verbatim self-link href
        // (`…/items/{feature.id}`) is a single routable segment; axum's `Path`
        // extractor percent-decodes that segment back to `rec.id` on `GET`, so
        // the round-trip is lossless even for CAP identifiers containing `/`.
        id: encode_feature_id(&rec.id),
        geometry: Arc::clone(&rec.geometry),
        properties: Arc::clone(&rec.properties),
    }
}

/// Percent-encode a feature id into a single **URL path segment** for the
/// Features self-link href (which api-features inserts verbatim). Encodes every
/// byte outside RFC 3986 `pchar` (unreserved / sub-delims / `:` / `@`), so `/`
/// `%` `?` `#` `[` `]` space and any non-ASCII byte are escaped — real CAP
/// identifiers (e.g. US-NWS) contain brackets, and bare `[`/`]` are illegal in a
/// path. axum's `Path` extractor decodes the segment back to `rec.id` (the
/// lookup key) on `GET`, so the round-trip is lossless. A no-op for the common
/// dot/colon ids (all `pchar`).
fn encode_feature_id(id: &str) -> String {
    fn is_pchar(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                // unreserved
                b'-' | b'.' | b'_' | b'~'
                // sub-delims
                | b'!' | b'$' | b'&' | b'\'' | b'(' | b')'
                | b'*' | b'+' | b',' | b';' | b'='
                // pchar extras
                | b':' | b'@'
            )
    }
    let mut out = String::with_capacity(id.len());
    for &b in id.as_bytes() {
        if is_pchar(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(
                char::from_digit((b >> 4) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit((b & 0xf) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

// ---------------------------------------------------------------------------
// MapEngine
// ---------------------------------------------------------------------------

impl MapEngine for CapEngine {
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        _parameter: Option<&str>,
        _z: Option<f64>,
        _reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let cat = self.snapshot();
        // None ⇒ "now" (the snapshot's as_of); an explicit TIME selects that instant.
        let t = time.unwrap_or(cat.as_of);

        let (w, h) = (width as usize, height as usize);
        let mut values: Vec<Option<f64>> = vec![None; w.saturating_mul(h)];

        // Prefilter to areas whose bbox intersects the (WGS84) request rectangle.
        // A degenerate request bbox (zero-area / non-finite) asks for no region,
        // so the tile is empty — never a full-catalog render (the API layer
        // already rejects such bboxes; this is the engine's own safety net).
        let candidates: Vec<usize> = match Bbox::new(bbox[0], bbox[1], bbox[2], bbox[3]) {
            Ok(b) => cat.query_bbox(&b),
            Err(_) => {
                return Ok(RasterTile {
                    width,
                    height,
                    values: values.into(),
                })
            }
        };

        let mut rendered = 0usize;
        for &i in &candidates {
            let rec = &cat.records[i];
            if !rec.window.active_at(t) {
                continue;
            }
            let px =
                ds_core::geo::geometry_to_pixels(&rec.geometry, bbox, width, height, output_crs);
            for poly in &px.polygons {
                fill_polygon(
                    &mut values,
                    width,
                    height,
                    &poly.exterior,
                    &poly.holes,
                    rec.severity_code,
                    Combine::Max, // higher severity wins on overlap, order-independent
                );
            }
            rendered += 1;
            if rendered >= MAX_RENDER_RECORDS {
                tracing::warn!(
                    "[{}] cap: render capped at {MAX_RENDER_RECORDS} areas",
                    self.collection_id
                );
                break;
            }
        }

        Ok(RasterTile {
            width,
            height,
            values: values.into(),
        })
    }

    fn raster_info(&self) -> RasterInfo {
        // Cheap clone of the prebuilt snapshot — no recomputation (#211); the
        // cost is O(times), bounded by the 256-entry TIME cap.
        (*self.snapshot().info).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::encode_feature_id;

    #[test]
    fn encode_feature_id_handles_path_unsafe_chars() {
        // Common dot/colon ids are pchar → unchanged.
        assert_eq!(
            encode_feature_id("urn:test:flood-1.0.0"),
            "urn:test:flood-1.0.0"
        );
        // Path-illegal chars are percent-encoded: '/', brackets (real US-NWS ids
        // contain them), space, '%', '?', '#'.
        assert_eq!(encode_feature_id("a/b"), "a%2Fb");
        assert_eq!(encode_feature_id("zone[1]"), "zone%5B1%5D");
        assert_eq!(encode_feature_id("a b"), "a%20b");
        assert_eq!(encode_feature_id("a%b"), "a%25b");
        assert_eq!(encode_feature_id("a?b#c"), "a%3Fb%23c");
        // Non-ASCII is UTF-8 percent-encoded byte-wise.
        assert_eq!(encode_feature_id("é"), "%C3%A9");
    }
}
