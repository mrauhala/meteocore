//! `MapEngine` impl backed by an ODIM_H5 composite catalog.
//!
//! At construction time, [`OdimEngine::new`] scans the configured
//! source — a local directory or an S3/HTTP object-store prefix — for
//! ODIM files matching the `filename_template` pattern and pre-loads
//! the most recent one so `raster_info()` can answer with non-empty
//! `spatial_extent` and `times`. After that, every `get_raster_tile`
//! call:
//!
//! 1. Picks the catalog entry matching the requested time (or the
//!    most recent if `time` is `None`).
//! 2. Loads the composite from a process-global path-keyed LRU cache
//!    (decoded once per file and shared across collections and concurrent
//!    requests; subsequent same-file reads avoid disk + HDF5 reparse).
//! 3. Walks the output grid pixel-by-pixel, projecting the WGS84
//!    or Web-Mercator output coords into the composite's native
//!    CRS, and samples via nearest-neighbour.
//!
//! Scope is still narrowed from the format's full capabilities:
//! - Single-dataset COMP composites (no PVOL polar volumes yet)
//! - Single-parameter (one composite quantity per collection)
//! - STAC sources land in a later phase
//!
//! A background `poll_loop` re-scans the source every
//! `poll_interval_secs` and atomically swaps the catalog
//! (`ArcSwap<Vec<CatalogEntry>>`) when the file set changes, so
//! new ODIM files appear in the temporal extent without needing
//! an admin reload.
//!
//! See [[project_odim_engine_plan]] for the full multi-phase plan.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::error::DataServerError;
use ds_core::geo::Crs;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_core::resample::ProjectionGrid;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use ds_storage::discovery::{expand_prefix_for_dates, expand_prefix_pattern, TimeWindow};

use crate::catalog::{scan_local_directory, scan_remote, CatalogEntry, FilenameMatcher, Location};
use crate::reader::{read_composite, OdimComposite};

/// Days of date-partitioned prefixes to scan when an S3 source has no
/// `time_window`. Two days covers the just-after-midnight case where
/// the recent tail still straddles yesterday's partition.
const DEFAULT_SCAN_DAYS: u32 = 2;

/// Default decoded-composite cache size (MB) when `MC_ODIM_COMPOSITE_CACHE_MB`
/// is unset. A decoded `OdimComposite` is essentially its raw pixel array:
/// national COMP grids are a few MB, but the OPERA pan-European composite is a
/// 134 MB `f64` array whose HDF5 decode is ~111 ms (measured). 1024 MB holds
/// ≈7 OPERA-class grids resident — enough for a typical concurrent
/// full-viewport WMS animation working set, where N distinct-time render tasks
/// each fire ~40 [`OdimEngine::load_composite`] calls through the meta-tile
/// loop. A single-slot cache ping-pongs across those N tasks and re-decodes
/// the same file many times (#212); a multi-entry LRU keeps every active
/// timestep resident instead.
///
/// Sizing note: a heavy OPERA animation of ~13 concurrent frames is ≈1.7 GB
/// resident, so it can still thrash a 1 GB cap — raise this knob (or bound the
/// client's preload concurrency) for that workload. `0` disables the cache,
/// so every load decodes (same convention as the PVOL pixel / voxel-grid
/// caches in `volume_engine.rs`).
const DEFAULT_COMPOSITE_CACHE_MB: u64 = 1024;

/// Byte-weights each cached composite by its decoded pixel array (plus the
/// key string and `Arc`/control overhead). Mirrors the PVOL pixel / voxel-grid
/// weighters.
#[derive(Clone)]
struct CompositeWeighter;

impl quick_cache::Weighter<Arc<str>, Arc<OdimComposite>> for CompositeWeighter {
    fn weight(&self, key: &Arc<str>, val: &Arc<OdimComposite>) -> u64 {
        val.size_bytes() as u64 + key.len() as u64 + 64
    }
}

/// Process-global byte-bounded LRU of decoded ODIM composites, shared across
/// every COMP collection. Keyed by the composite's location id
/// ([`Location::id`] — a globally-unique local path or S3 object key), which
/// fully determines the immutable decode, so the key carries no data-version
/// (same as the PVOL pixel / voxel-grid caches). Sized once from the
/// environment on first use; `0` disables (`capacity_bytes.max(1)` keeps the
/// cache valid but unable to retain anything, so every load decodes).
static COMPOSITE_CACHE: std::sync::LazyLock<
    quick_cache::sync::Cache<Arc<str>, Arc<OdimComposite>, CompositeWeighter>,
> = std::sync::LazyLock::new(|| {
    let capacity_bytes = std::env::var("MC_ODIM_COMPOSITE_CACHE_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_COMPOSITE_CACHE_MB)
        .saturating_mul(1024 * 1024);
    // Estimate item slots at one OPERA-class (134 MB) grid each; `max(4)`
    // keeps a small/zero capacity valid (a near-disabled cache holds nothing).
    let estimated_items = ((capacity_bytes / (134 * 1024 * 1024)).max(4)) as usize;
    quick_cache::sync::Cache::with_weighter(
        estimated_items,
        capacity_bytes.max(1),
        CompositeWeighter,
    )
});

/// Cumulative `(hits, misses)` of [`COMPOSITE_CACHE`], for `/metrics`.
static COMPOSITE_CACHE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static COMPOSITE_CACHE_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Snapshot of the process-global composite cache for `/metrics`:
/// `(hits, misses, resident_bytes, capacity_bytes)`. Mirrors
/// `volume_engine::voxel_grid_cache_metrics`.
pub fn composite_cache_metrics() -> (u64, u64, u64, u64) {
    (
        COMPOSITE_CACHE_HITS.load(std::sync::atomic::Ordering::Relaxed),
        COMPOSITE_CACHE_MISSES.load(std::sync::atomic::Ordering::Relaxed),
        COMPOSITE_CACHE.weight(),
        COMPOSITE_CACHE.capacity(),
    )
}

/// Where an [`OdimEngine`] reads ODIM files from.
#[derive(Clone)]
enum Source {
    /// A local filesystem directory, scanned with `read_dir`.
    Local { data_dir: PathBuf },
    /// An S3 or HTTP(S) object store. `prefix_pattern` may carry
    /// strftime codes (e.g. `%Y/%m/%d/OPERA/COMP/`); it is expanded
    /// per UTC date on every scan so the listing stays current across
    /// day boundaries. `time_window`, when set, bounds both the dates
    /// expanded and the timestamps kept. `origin` is retained purely
    /// for diagnostics (the `store` already targets it) so log and
    /// error messages can name the store.
    Remote {
        store: ds_storage::DataStore,
        origin: RemoteOrigin,
        prefix_pattern: String,
        time_window: Option<TimeWindow>,
    },
    /// A non-listable HTTP(S) directory discovered by **template probe**
    /// (#287): instead of `list`, candidate filenames are built from
    /// `template` (the strftime `filename_template`) for timestamps walked
    /// back from now over `cadence`, and each is `HEAD`-probed. For
    /// autoindex servers (DWD opendata) where `list` (WebDAV `PROPFIND`)
    /// returns nothing. `time_window` bounds how far back to walk;
    /// `base_url` is diagnostics only.
    TemplateHttp {
        store: ds_storage::DataStore,
        base_url: String,
        base_prefix: String,
        template: String,
        cadence: Duration,
        time_window: Option<TimeWindow>,
    },
}

/// Hard cap on candidate timestamps probed per template scan — a backstop
/// against an unbounded walk when neither `time_window` nor `max_files`
/// bounds it. 288 = 24 h at a 5-minute cadence. The probes are concurrent
/// `HEAD`s (cheap), so this is generous; `time_window`/`max_files` clamp it
/// far lower in practice (DWD `-PT2H` @ 300 s → 25 probes).
const HARD_MAX_PROBES: usize = 288;

/// Max concurrent `HEAD` probes in a template scan.
const PROBE_CONCURRENCY: usize = 16;

/// Diagnostic descriptor for a [`Source::Remote`] store — names the
/// backend in logs and error messages. The `store` does the real work;
/// this only carries enough to identify *which* store an operator is
/// looking at.
#[derive(Clone)]
enum RemoteOrigin {
    /// An S3 bucket reached via `endpoint` + `bucket`.
    S3 { endpoint: String, bucket: String },
    /// A plain HTTP(S) directory reached via an `http(s)://` `data_path`.
    Http { base_url: String },
}

/// Optional per-file scaling overrides (`physical = raw * gain + offset`,
/// plus a nodata sentinel). Each takes precedence over the value read
/// from the composite. Grouped so the engine constructors don't carry
/// three loose `Option<f64>` arguments.
#[derive(Clone, Copy, Default)]
struct Overrides {
    gain: Option<f64>,
    offset: Option<f64>,
    nodata: Option<f64>,
}

/// Scan `source` for ODIM files, returning catalog entries sorted by
/// timestamp ascending. Blocking — remote scans bridge async object-
/// store I/O internally (see [`ds_storage::DataStore`]).
fn scan_source(
    source: &Source,
    matcher: &FilenameMatcher,
    max_files: Option<usize>,
    // The current catalog, used only by the template-probe source to skip
    // re-probing already-known slots. Empty at construction; the poll
    // passes the live catalog. List-based sources ignore it (one `list`
    // call already returns the whole set cheaply).
    known: &[CatalogEntry],
) -> Result<Vec<CatalogEntry>, EngineError> {
    match source {
        Source::Local { data_dir } => Ok(scan_local_directory(data_dir, matcher, max_files)?),
        Source::Remote {
            store,
            prefix_pattern,
            time_window,
            ..
        } => {
            let now = Utc::now();
            let (prefixes, time_filter) = match time_window {
                Some(tw) => (
                    expand_prefix_for_dates(prefix_pattern, &tw.scan_dates(now)),
                    Some(tw.to_range(now)),
                ),
                None => (
                    expand_prefix_pattern(prefix_pattern, DEFAULT_SCAN_DAYS),
                    None,
                ),
            };
            Ok(scan_remote(
                store,
                &prefixes,
                matcher,
                time_filter,
                max_files,
            )?)
        }
        Source::TemplateHttp {
            store,
            base_prefix,
            template,
            cadence,
            time_window,
            ..
        } => Ok(discover_template_at(
            Utc::now(),
            store,
            base_prefix,
            template,
            *cadence,
            time_window.as_ref(),
            max_files,
            known,
        )?),
    }
}

/// Discover ODIM files on a non-listable HTTP source by **probing**
/// candidate filenames instead of listing the directory.
///
/// Builds one candidate per timestamp from `now` walked back by `cadence`
/// — `N = min(window/cadence, max_files, HARD_MAX_PROBES)` steps — and
/// renders each filename with `time.format(template)` (the strftime
/// inverse of the catalog matcher) joined under `base_prefix`.
///
/// **Incremental.** A candidate already present in `known` (the current
/// catalog) is carried forward unchanged — radar files are immutable once
/// published, so re-`HEAD`-ing them is pure overhead. Only the candidates
/// *not* in `known` are `HEAD`-probed (concurrently): the freshly-arrived
/// newest slot, plus any still-missing slots (a gap or a late upload),
/// which keep being retried until they arrive or age out of the window.
/// So the first scan probes the whole window, but each subsequent poll
/// probes only ~one slot. Survivors (carried + newly-found) become
/// [`CatalogEntry`] values whose timestamp is the one we generated (no
/// parsing); bytes are fetched lazily on render via [`fetch_bytes`].
/// Sorted ascending, deduped by timestamp.
///
/// `now` is a parameter (not `Utc::now()` inside) so the probe is
/// deterministically testable against a controlled clock.
#[allow(clippy::too_many_arguments)]
fn discover_template_at(
    now: DateTime<Utc>,
    store: &ds_storage::DataStore,
    base_prefix: &str,
    template: &str,
    cadence: Duration,
    time_window: Option<&TimeWindow>,
    max_files: Option<usize>,
    known: &[CatalogEntry],
) -> Result<Vec<CatalogEntry>, EngineError> {
    use ds_storage::object_store::path::Path as ObjectPath;
    use std::collections::HashMap;

    let cadence_secs = (cadence.as_secs().max(1)) as i64;

    // How many timestamp slots back to probe.
    let window_steps = match time_window {
        Some(tw) => {
            let (start, end) = tw.to_range(now);
            // +1 so the walk includes `now`'s own (aligned) slot.
            ((end - start).num_seconds().abs() / cadence_secs) as usize + 1
        }
        None => HARD_MAX_PROBES,
    };
    let bounded = window_steps
        .min(max_files.unwrap_or(usize::MAX))
        .min(HARD_MAX_PROBES);
    let n = bounded.max(1);

    // Align `now` down to the cadence grid (epoch-aligned), then walk back.
    let now_ts = now.timestamp();
    let aligned = now_ts - now_ts.rem_euclid(cadence_secs);
    let stamps: Vec<DateTime<Utc>> = (0..n)
        .filter_map(|i| DateTime::<Utc>::from_timestamp(aligned - (i as i64) * cadence_secs, 0))
        .collect();

    // Carry forward already-known in-window entries (no re-probe); collect
    // the rest as the slots to actually HEAD.
    let known_by_time: HashMap<DateTime<Utc>, &CatalogEntry> =
        known.iter().map(|e| (e.time, e)).collect();
    let mut entries: Vec<CatalogEntry> = Vec::new();
    let mut probe_stamps: Vec<DateTime<Utc>> = Vec::new();
    for stamp in &stamps {
        match known_by_time.get(stamp) {
            Some(entry) => entries.push((*entry).clone()),
            None => probe_stamps.push(*stamp),
        }
    }

    // Render + probe only the unknown candidates' filenames concurrently.
    let probe_keys: Vec<String> = probe_stamps
        .iter()
        .map(|t| join_prefix(base_prefix, &t.format(template).to_string()))
        .collect();
    let paths: Vec<ObjectPath> = probe_keys
        .iter()
        .map(|k| ObjectPath::from(k.as_str()))
        .collect();
    let probed = store.head_many(&paths, PROBE_CONCURRENCY)?;

    for ((time, key), result) in probe_stamps
        .iter()
        .zip(probe_keys.iter())
        .zip(probed.iter())
    {
        match result {
            Ok(Some(meta)) => {
                if meta.size as u64 > crate::catalog::MAX_REMOTE_FILE_SIZE {
                    tracing::warn!(
                        "[odim-template] skipping oversized object `{key}` ({} bytes)",
                        meta.size
                    );
                    continue;
                }
                entries.push(CatalogEntry {
                    time: *time,
                    location: Location::Remote {
                        store: store.clone(),
                        key: key.clone(),
                    },
                });
            }
            Ok(None) => {} // absent — expected for most candidate slots
            Err(e) => tracing::warn!("[odim-template] HEAD `{key}` failed: {e}"),
        }
    }

    entries.sort_by_key(|e| e.time);
    entries.dedup_by(|a, b| a.time == b.time);
    Ok(entries)
}

/// Join a base prefix and a filename with exactly one `/` (and none when
/// the base is empty — an `http(s)://host/` URL pointing at the root).
fn join_prefix(base_prefix: &str, filename: &str) -> String {
    if base_prefix.is_empty() {
        filename.to_string()
    } else {
        format!("{}/{}", base_prefix.trim_end_matches('/'), filename)
    }
}

/// Fetch the raw bytes of one ODIM file from its catalog location.
///
/// Remote fetches re-check the object size against
/// [`crate::catalog::MAX_REMOTE_FILE_SIZE`] via `head` before `get`.
/// `scan_remote` already filters by size at list time, but an object
/// can grow between listing and fetching — the `head` closes that gap
/// so a runaway object can't be pulled unbounded into memory.
fn fetch_bytes(location: &Location) -> Result<Vec<u8>, DataServerError> {
    match location {
        Location::Local(path) => std::fs::read(path).map_err(|e| {
            DataServerError::Engine(format!(
                "failed to read ODIM file `{}`: {e}",
                path.display()
            ))
        }),
        Location::Remote { store, key } => {
            let object = ds_storage::object_store::path::Path::from(key.as_str());
            let meta = store.head(&object)?;
            if meta.size as u64 > crate::catalog::MAX_REMOTE_FILE_SIZE {
                return Err(DataServerError::Engine(format!(
                    "ODIM object `{key}` is {} bytes — exceeds the {}-byte limit",
                    meta.size,
                    crate::catalog::MAX_REMOTE_FILE_SIZE
                )));
            }
            store.get(&object).map(|b| b.to_vec())
        }
    }
}

/// `MapEngine` implementation backed by an ODIM_H5 composite catalog.
///
/// Holds an [`ArcSwap`]-protected catalog so the async poll loop can
/// publish refreshed entries (new files, removed files) without
/// disturbing in-flight reads.
pub struct OdimEngine {
    // Fields used by both the `MapEngine` impl (this file) and the
    // `EdrEngine` impl (`edr.rs`) are `pub(crate)`.
    pub(crate) catalog: Arc<ArcSwap<Vec<CatalogEntry>>>,
    pub(crate) collection_id: String,
    pub(crate) parameter: String,
    pub(crate) unit: String,
    pub(crate) gain_override: Option<f64>,
    pub(crate) offset_override: Option<f64>,
    pub(crate) nodata_override: Option<f64>,
    /// Native-CRS label, WGS84 corner envelope, and grid dimensions
    /// captured from the seed composite at construction. Every
    /// timestep of a given ODIM collection shares the same grid, so
    /// these are stable for the engine's lifetime. Holding them as
    /// plain fields lets `raster_info()` (MapEngine) and the EDR
    /// metadata / area-grid-sizing paths answer without touching the
    /// process-global composite cache — and, crucially, without
    /// depending on whether a `get_raster_tile` call has warmed that
    /// cache yet (an `apis = ["edr"]`-only collection never issues one).
    pub(crate) seed_native_crs: String,
    pub(crate) seed_spatial_extent: [f64; 4],
    pub(crate) seed_xsize: u32,
    pub(crate) seed_ysize: u32,
    /// Local directory or S3/HTTP object store the poll loop re-scans.
    source: Source,
    matcher: FilenameMatcher,
    max_files: Option<usize>,
    poll_interval: Duration,
    /// Shutdown coordination. `shutdown` is the authoritative flag —
    /// every poll-loop iteration checks it at the top. `notify` is
    /// a wake-up signal so `shutdown()` doesn't have to wait for
    /// the next `interval.tick()` to take effect.
    ///
    /// **Why not `watch::Sender<()>`** (the original design): if
    /// `shutdown()` fires before `poll_loop()` calls `subscribe()`,
    /// `watch::send` returns `Err` (no receivers) and silently
    /// drops the signal — the version doesn't bump, so a later
    /// `subscribe()`-then-`changed().await` would block forever.
    /// `AtomicBool` is edge-triggered: once set, every future
    /// iteration sees it regardless of timing.
    shutdown: AtomicBool,
    shutdown_notify: Notify,
}

/// Errors from [`OdimEngine::new`]. Per-tile errors are mapped to
/// [`DataServerError`] inside the `MapEngine` impl.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("either `filename_template` or `filename_pattern`+`timestamp_format` must be set")]
    NoFilenamePattern,
    #[error("filename pattern build failed: {0}")]
    BadPattern(#[from] crate::catalog::CatalogError),
    #[error(
        "ODIM collection has no source — set a local `data_path`, an \
         `http(s)://` `data_path`, or an S3 `endpoint` + `bucket`"
    )]
    NoSource,
    #[error(
        "ODIM S3 config is incomplete — `endpoint` and `bucket` must both be \
         set (or both omitted for a local `data_path` source)"
    )]
    IncompleteS3Config,
    #[error(
        "ODIM S3 source is missing `prefix_pattern` — set it to the bucket's \
         date-partitioned key prefix (e.g. `%Y/%m/%d/OPERA/COMP/`), or to an \
         empty string to scan the bucket root"
    )]
    MissingPrefixPattern,
    #[error(
        "no ODIM files found at `{location}` matching the configured filename \
         pattern — verify the source exists and the template matches the \
         producer's layout"
    )]
    NoFiles { location: String },
    #[error("ODIM source error: {0}")]
    Storage(#[from] DataServerError),
    #[error("failed to parse seed composite `{location}`: {source}")]
    SeedParseFailed {
        location: String,
        #[source]
        source: crate::reader::ReadError,
    },
    #[error(
        "ODIM COMP collection is missing `[odim].{field}` — single-parameter \
         `engine_type = \"odim\"` collections must set both `parameter` and \
         `unit` (only `engine_type = \"odim-volume\"` may omit them)"
    )]
    MissingCompField { field: &'static str },
    #[error(
        "ODIM `discovery = \"{value}\"` is not a known mode — use `\"list\"` \
         (default, WebDAV PROPFIND) or `\"template\"` (HEAD-probe candidate \
         filenames for non-listable autoindex servers)"
    )]
    UnknownDiscovery { value: String },
    #[error(
        "ODIM `discovery = \"template\"` requires `filename_template` — the \
         `filename_pattern` + `timestamp_format` form can't be inverted to a \
         candidate filename to probe"
    )]
    TemplateNeedsFilenameTemplate,
    #[error(
        "ODIM `discovery = \"template\"` requires `cadence_secs` > 0 — the \
         spacing of candidate timestamps to probe (e.g. 300 for a 5-min feed)"
    )]
    TemplateNeedsCadence,
    #[error("ODIM `discovery = \"template\"` only applies to an `http(s)://` `data_path` source")]
    TemplateNeedsHttp,
}

impl OdimEngine {
    /// Build an engine by scanning its configured source for files
    /// matching the filename pattern. The source is a local directory
    /// (`data_path`), an S3 bucket (`endpoint`/`bucket`/`prefix_pattern`),
    /// or an HTTP(S) directory (`http(s)://` `data_path`). Loads the most
    /// recent file synchronously to populate metadata; raises
    /// [`EngineError`] when the source yields no files.
    pub fn new(
        collection_id: &str,
        data_path: Option<&str>,
        config: &ds_core::config::OdimConfig,
    ) -> Result<Self, EngineError> {
        // COMP is single-parameter — `parameter`/`unit` are mandatory.
        // (The shared `OdimConfig` makes both `Option` so the
        // multi-parameter `odim-volume` engine can omit them.)
        let parameter = config
            .parameter
            .clone()
            .ok_or(EngineError::MissingCompField { field: "parameter" })?;
        let unit = config
            .unit
            .clone()
            .ok_or(EngineError::MissingCompField { field: "unit" })?;

        let matcher = build_matcher(config)?;
        let source = build_source(collection_id, data_path, config)?;

        // `time_window` only constrains a *remote* (S3/HTTP) source's
        // prefix expansion + timestamp filtering. A local `data_path`
        // source ignores it — warn so a misplaced setting doesn't
        // silently do nothing. (An `http(s)://` `data_path` resolves to
        // `Source::Remote`, so it is correctly exempt.)
        if config.time_window.is_some() && matches!(source, Source::Local { .. }) {
            tracing::warn!(
                "[{collection_id}] `time_window` is set but has no effect on a \
                 local `data_path` ODIM source — it only applies to S3/HTTP sources"
            );
        }

        // `resampling` only applies to the PVOL (`odim-volume`) Cartesian
        // render. The COMP composite render is always nearest-neighbour, so a
        // non-default value here is silently ignored — warn rather than swallow.
        if config.resampling != ds_core::config::ResamplingMethod::default() {
            tracing::warn!(
                "[{collection_id}] `resampling` is set but has no effect on an \
                 `odim` (COMP) collection — it only applies to the `odim-volume` \
                 (PVOL) render; the composite render is always nearest-neighbour"
            );
        }

        // `prewarm_sweeps` only drives the PVOL (`odim-volume`) engine's
        // poll-time pixel pre-warm — COMP has no per-moment lazy pixel cache to
        // warm, so a non-default value here is silently ignored (#461). `0` is a
        // deliberate "disable" that's already a no-op on COMP, so don't warn on
        // it (it would be misleading in a base config shared across engine
        // types — #462 review); only warn for a value set expecting an effect.
        if config.prewarm_sweeps != ds_core::config::DEFAULT_PREWARM_SWEEPS
            && config.prewarm_sweeps != 0
        {
            tracing::warn!(
                "[{collection_id}] `prewarm_sweeps` is set but has no effect on an \
                 `odim` (COMP) collection — it only applies to the `odim-volume` \
                 (PVOL) engine's poll-time pixel pre-warm"
            );
        }

        Self::assemble(
            collection_id,
            parameter,
            unit,
            Overrides {
                gain: config.gain,
                offset: config.offset,
                nodata: config.nodata,
            },
            source,
            matcher,
            config.max_files,
            config.poll_interval_secs,
        )
    }

    /// Scan `source`, pre-load the most recent file to seed metadata,
    /// and assemble the engine. Shared by [`OdimEngine::new`] and the
    /// test-only remote constructor — everything past source resolution
    /// is identical regardless of how the source was obtained.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        collection_id: &str,
        parameter: String,
        unit: String,
        overrides: Overrides,
        source: Source,
        matcher: FilenameMatcher,
        max_files: Option<usize>,
        poll_interval_secs: u64,
    ) -> Result<Self, EngineError> {
        // Construction: no prior catalog, so the template probe scans the
        // whole window (`known` is empty).
        let catalog = scan_source(&source, &matcher, max_files, &[])?;
        if catalog.is_empty() {
            return Err(EngineError::NoFiles {
                location: source_label(&source),
            });
        }

        // Pre-load the most recent file so `raster_info()` can
        // populate `spatial_extent` and `times` immediately, and so a
        // misconfigured filename pattern surfaces at engine
        // construction rather than at first request.
        let seed_location = catalog.last().expect("catalog non-empty").location.clone();
        let bytes = fetch_bytes(&seed_location)?;
        let composite =
            Arc::new(
                read_composite(&bytes).map_err(|e| EngineError::SeedParseFailed {
                    location: seed_location.id(),
                    source: e,
                })?,
            );

        let seed_native_crs = crs_label(&composite.crs);
        let seed_spatial_extent = composite.wgs84_bbox;
        let seed_xsize = composite.xsize;
        let seed_ysize = composite.ysize;

        // Seed the process-global composite cache so the first render of the
        // most recent file is a hit (we just decoded it for the metadata
        // seed). `insert` rather than the old per-engine slot — every COMP
        // collection shares the one byte-bounded LRU (#212).
        COMPOSITE_CACHE.insert(Arc::from(seed_location.id().as_str()), composite);

        Ok(Self {
            catalog: Arc::new(ArcSwap::from_pointee(catalog)),
            collection_id: collection_id.to_string(),
            parameter,
            unit,
            gain_override: overrides.gain,
            offset_override: overrides.offset,
            nodata_override: overrides.nodata,
            seed_native_crs,
            seed_spatial_extent,
            seed_xsize,
            seed_ysize,
            source,
            matcher,
            max_files,
            poll_interval: Duration::from_secs(poll_interval_secs.max(1)),
            shutdown: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
        })
    }

    /// Build an engine over a pre-constructed remote [`ds_storage::DataStore`]
    /// — the `LocalFileSystem`-as-remote test trick (PR #182) used to drive
    /// the S3/HTTP `scan_remote` → render path without a live endpoint.
    /// `prefix` is the (already date-expanded or flat) key prefix to list.
    #[doc(hidden)]
    pub fn new_remote_for_test(
        collection_id: &str,
        store: ds_storage::DataStore,
        prefix: &str,
        parameter: &str,
        unit: &str,
        config: &ds_core::config::OdimConfig,
    ) -> Result<Self, EngineError> {
        let matcher = build_matcher(config)?;
        let source = Source::Remote {
            store,
            origin: RemoteOrigin::Http {
                base_url: "test://remote".to_string(),
            },
            prefix_pattern: prefix.to_string(),
            time_window: None,
        };
        Self::assemble(
            collection_id,
            parameter.to_string(),
            unit.to_string(),
            Overrides {
                gain: config.gain,
                offset: config.offset,
                nodata: config.nodata,
            },
            source,
            matcher,
            config.max_files,
            config.poll_interval_secs,
        )
    }

    /// Drive the #287 template-discovery probe against a controlled clock —
    /// the seam the integration suite uses to test the listing-free HTTP
    /// discovery deterministically (over a `LocalFileSystem`-backed
    /// `DataStore`, whose `head` returns `NotFound` for absent candidates).
    /// `time_window` is an ISO 8601 duration string (e.g. `"-PT1H"`).
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn discover_template_for_test(
        now: DateTime<Utc>,
        store: ds_storage::DataStore,
        base_prefix: &str,
        template: &str,
        cadence_secs: u64,
        time_window: Option<&str>,
        max_files: Option<usize>,
        known: &[CatalogEntry],
    ) -> Result<Vec<CatalogEntry>, EngineError> {
        let tw = match time_window {
            Some(s) => Some(TimeWindow::parse(s)?),
            None => None,
        };
        discover_template_at(
            now,
            &store,
            base_prefix,
            template,
            Duration::from_secs(cadence_secs),
            tw.as_ref(),
            max_files,
            known,
        )
    }

    /// Run the source poll loop. Exits when [`OdimEngine::shutdown`]
    /// is called. Each tick re-scans the source, atomically swaps the
    /// catalog `ArcSwap` if the file set changed, and logs at INFO
    /// when new files appear so operators can confirm the polling is
    /// alive.
    ///
    /// The scan runs **directly on the background poll runtime worker**,
    /// NOT via `spawn_blocking`: a remote scan reaches `ds-storage`, whose
    /// `block_in_place` is valid on a multi-thread-runtime worker but
    /// *panics* on a `spawn_blocking` pool thread. Wrapping it in
    /// `spawn_blocking` silently fails every remote (S3 / HTTP / template)
    /// refresh — the `JoinError` is caught, but the catalog never updates.
    /// Mirrors the GRIB / GeoTIFF / QueryData / PVOL poll loops, which also
    /// call their blocking scan directly on the poll runtime.
    ///
    /// Errors from the scan (source temporarily unavailable, S3
    /// timeout) are logged at WARN and otherwise ignored — the
    /// previous catalog stays in place so live requests keep working.
    pub async fn poll_loop(&self) {
        let mut interval = tokio::time::interval(self.poll_interval);
        interval.tick().await; // skip immediate first tick (already loaded at boot)

        loop {
            // Authoritative check — catches a `shutdown()` that fired
            // before `poll_loop()` started, or between two ticks.
            if self.shutdown.load(Ordering::Acquire) {
                tracing::info!("[{}] ODIM poll loop shutting down", self.collection_id);
                break;
            }
            tokio::select! {
                _ = interval.tick() => {
                    // Recheck inside the branch: `shutdown()` may have
                    // fired between the load above and the tick
                    // returning, and we don't want to do extra I/O on
                    // the way out.
                    if !self.shutdown.load(Ordering::Acquire) {
                        self.poll_once();
                    }
                }
                _ = self.shutdown_notify.notified() => {
                    // `shutdown()` was just called. The flag is set;
                    // top-of-loop check on the next iteration handles
                    // the actual exit.
                }
            }
        }
    }

    /// Signal the polling loop to stop. Idempotent — the first call
    /// transitions the flag and wakes any waiter; subsequent calls are
    /// no-ops. Safe to call before `poll_loop()` has started: the flag
    /// persists and the next `poll_loop()` invocation will exit on its
    /// first iteration.
    pub fn shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::Release) {
            // We just transitioned false → true. Wake any pending
            // `notified()` waiter. If no waiter exists, the
            // top-of-loop flag check on the next iteration covers it.
            self.shutdown_notify.notify_waiters();
        }
    }

    fn poll_once(&self) {
        // Snapshot the current catalog *before* scanning so the template
        // probe can carry forward already-known slots and HEAD only the new
        // ones (list-based sources ignore it). Held across the scan — a
        // cheap `ArcSwap` guard on the background poll path.
        let prev = self.catalog.load_full();
        // Direct (not `spawn_blocking`) so a remote scan's `ds-storage`
        // `block_in_place` runs on the multi-thread poll-runtime worker —
        // valid there, panics on a `spawn_blocking` thread. See `poll_loop`.
        let scan = match scan_source(&self.source, &self.matcher, self.max_files, &prev) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "[{}] ODIM catalog refresh failed: {}",
                    self.collection_id,
                    e
                );
                return;
            }
        };
        // Diff against the previous catalog using count + most-recent
        // timestamp. Producing-side renames are rare; this is cheap
        // and accurate for the append-only / rolling-window case ODIM
        // producers actually use.
        let prev_count = prev.len();
        let prev_latest = prev.last().map(|e| e.time);
        let new_latest = scan.last().map(|e| e.time);
        if prev_count != scan.len() || prev_latest != new_latest {
            tracing::info!(
                "[{}] ODIM catalog refreshed: {} → {} files, latest {} → {}",
                self.collection_id,
                prev_count,
                scan.len(),
                prev_latest
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "<none>".into()),
                new_latest
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| "<none>".into()),
            );
            self.catalog.store(Arc::new(scan));
        }
    }

    /// Pick the catalog entry whose timestamp is closest to `time`
    /// (exact match preferred). If `time` is `None`, return the most
    /// recent entry. Returns an owned `CatalogEntry` clone because
    /// the catalog lives behind `ArcSwap` and the snapshot guard
    /// must drop before the per-request work proceeds.
    ///
    /// Logs a WARN when the nearest match is more than 2× the poll
    /// interval from the requested time — operators should treat
    /// this as a signal that the data feed has stalled or that the
    /// client is querying outside the available window.
    fn select_entry(&self, time: Option<DateTime<Utc>>) -> Option<CatalogEntry> {
        let snapshot = self.catalog.load();
        if let Some(target) = time {
            // The catalog is sorted by time ascending — pick the
            // smallest-abs-difference entry. With only ~24 entries in
            // practice (24h * 5-min ODIM cadence is the upper end
            // before `max_files` trims), a linear scan is faster than
            // a binary search through the noise of branch prediction.
            let pick = snapshot
                .iter()
                .min_by_key(|e| (e.time - target).num_seconds().abs())?;
            let gap = (pick.time - target).num_seconds().abs();
            // Stale-match threshold: 2× the poll interval, with a 600s
            // floor. The floor accommodates real radar feeds which
            // typically arrive at 5-minute cadence — without it, the
            // default 30-second poll_interval would trip the warning
            // on healthy feeds whenever the requested time happened
            // to fall in the middle of a 5-min window. 600s is a
            // conservative envelope around the 5-min cadence + clock
            // skew + late arrivals.
            let stale_threshold = (self.poll_interval.as_secs() * 2).max(600) as i64;
            if gap > stale_threshold {
                tracing::warn!(
                    "[{}] ODIM request at {} matched stale entry {} (gap {}s > {}s threshold)",
                    self.collection_id,
                    target.to_rfc3339(),
                    pick.time.to_rfc3339(),
                    gap,
                    stale_threshold
                );
            }
            Some(pick.clone())
        } else {
            snapshot.last().cloned()
        }
    }

    /// Read + parse the ODIM file at `location` and return a shared
    /// `Arc<OdimComposite>` snapshot, served from the process-global
    /// byte-bounded composite LRU ([`COMPOSITE_CACHE`]) keyed by
    /// [`Location::id`].
    ///
    /// **Blocking call** — reads the file (local `read` or S3 `get`)
    /// and parses HDF5 directly. Callers from async contexts must wrap
    /// in `tokio::task::spawn_blocking`. Two call paths reach here:
    ///
    /// - `MapEngine::get_raster_tile` — already runs inside
    ///   `spawn_blocking` (the WMS / Maps / Tiles handlers do this).
    /// - `EdrEngine::query_position` / `query_area` (see `edr.rs`) —
    ///   the api-edr handlers currently call these directly from an
    ///   `async fn` *without* `spawn_blocking`, so this blocking
    ///   work lands on a Tokio worker. That is a pre-existing
    ///   api-edr-level gap affecting every EDR engine (GeoTIFF,
    ///   QueryData, GRIB all do blocking I/O in `query_position`
    ///   too) — tracked in issue #178, to be fixed in the api-edr
    ///   handlers rather than per-engine.
    pub(crate) fn load_composite(
        &self,
        location: &Location,
    ) -> Result<Arc<OdimComposite>, DataServerError> {
        // Composites are immutable, so the location id (local path or S3 key)
        // fully determines the decode — no data-version in the key (same as
        // the PVOL pixel / voxel-grid caches). `get_or_insert_with` is the
        // single-flight: quick_cache's placeholder guard runs the fetch+decode
        // closure for exactly one caller per key and blocks concurrent callers
        // for the SAME key until it finishes, so a burst of cold misses on one
        // file decodes once. Crucially, distinct timesteps stay resident
        // side-by-side, so a concurrent full-viewport WMS animation — N
        // distinct-time render tasks, each firing ~40 `load_composite` calls
        // through the meta-tile loop — no longer ping-pongs a single slot and
        // re-decodes the same 134 MB OPERA grid many times (#212).
        let key: Arc<str> = Arc::from(location.id().as_str());
        let mut computed = false;
        // The fallible form returns a fetch/decode error to *this* caller
        // without inserting (the placeholder is dropped), so a transient read
        // failure does NOT poison the key — the next request retries it. The
        // miss is counted at the TOP of the closure, before the fallible
        // fetch/decode, so a failed read still registers as a miss rather than
        // a silent gap in the metric (the `?` below would otherwise skip the
        // post-call accounting on error).
        let composite = COMPOSITE_CACHE.get_or_insert_with(&key, || {
            computed = true;
            COMPOSITE_CACHE_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let bytes = fetch_bytes(location)
                .map_err(|e| DataServerError::Engine(format!("[{}] {e}", self.collection_id)))?;
            let composite = Arc::new(read_composite(&bytes).map_err(|e| {
                DataServerError::Engine(format!(
                    "[{}] failed to parse ODIM file `{}`: {e}",
                    self.collection_id, key
                ))
            })?);
            Ok::<_, DataServerError>(composite)
        })?;
        // The closure ran (miss, counted inside) iff `computed`; otherwise the
        // value came from the cache, so count a hit here. Mirrors
        // `voxel_grid_cached`'s hit/miss accounting.
        if !computed {
            COMPOSITE_CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(composite)
    }
}

/// Resolve the engine's `FilenameMatcher` from config: prefer
/// `filename_template` (strftime), fall back to the explicit
/// `filename_pattern` + `timestamp_format` pair.
fn build_matcher(config: &ds_core::config::OdimConfig) -> Result<FilenameMatcher, EngineError> {
    if let Some(template) = &config.filename_template {
        return Ok(FilenameMatcher::from_template(template)?);
    }
    if let (Some(pattern), Some(format)) = (&config.filename_pattern, &config.timestamp_format) {
        return Ok(FilenameMatcher::from_pattern(pattern, format)?);
    }
    Err(EngineError::NoFilenamePattern)
}

/// Resolve the engine's [`Source`] from config:
///
/// - `endpoint` + `bucket` → an **S3** source (date-partitioned via
///   `prefix_pattern`). Setting exactly one is a configuration error.
/// - an `http(s)://` `data_path` → an **HTTP(S)** object store, built
///   with [`ds_storage::build_store`] (the same dispatch the GeoTIFF
///   engine uses, so an `amazonaws.com`/`cloudferro.com` URL still
///   resolves to S3). The URL path becomes the base prefix; any
///   `prefix_pattern` is appended under it. `discovery = "template"`
///   switches it to the listing-free [`Source::TemplateHttp`] probe
///   (#287, for non-listable autoindex servers); otherwise it lists.
/// - a plain `data_path` → a **local** directory.
fn build_source(
    collection_id: &str,
    data_path: Option<&str>,
    config: &ds_core::config::OdimConfig,
) -> Result<Source, EngineError> {
    // Validate `discovery` up front (catches an unknown value for any
    // source type); template mode is only valid on HTTP, rejected here for S3.
    if discovery_mode(config)? == DiscoveryMode::Template && config.endpoint.is_some() {
        return Err(EngineError::TemplateNeedsHttp);
    }

    match (config.endpoint.as_deref(), config.bucket.as_deref()) {
        (Some(endpoint), Some(bucket)) => {
            // A missing `prefix_pattern` is almost always a config
            // mistake: it would list the whole bucket on every poll.
            // An explicit empty string is a deliberate flat-bucket
            // opt-in and is allowed.
            let prefix_pattern = config
                .prefix_pattern
                .clone()
                .ok_or(EngineError::MissingPrefixPattern)?;
            let store = ds_storage::build_s3_store_from_parts(endpoint, bucket)?;
            let time_window = match &config.time_window {
                Some(s) => Some(TimeWindow::parse(s)?),
                None => None,
            };
            tracing::info!(
                "[{}] ODIM S3 source: endpoint={endpoint} bucket={bucket} prefix='{prefix_pattern}'",
                collection_id
            );
            Ok(Source::Remote {
                store,
                origin: RemoteOrigin::S3 {
                    endpoint: endpoint.to_string(),
                    bucket: bucket.to_string(),
                },
                prefix_pattern,
                time_window,
            })
        }
        (None, None) => {
            let data_path = data_path.ok_or(EngineError::NoSource)?;
            let is_http = ds_storage::has_scheme(data_path, "http://")
                || ds_storage::has_scheme(data_path, "https://");

            // `discovery = "template"` only makes sense for an HTTP source —
            // reject it on a local `data_path` so a misconfig fails loudly.
            if discovery_mode(config)? == DiscoveryMode::Template && !is_http {
                return Err(EngineError::TemplateNeedsHttp);
            }

            if is_http {
                // `build_store` dispatches the URL (plain HTTP → `HttpStore`;
                // amazonaws/cloudferro → S3). The returned base path is the
                // directory; any `prefix_pattern` is appended under it.
                let (store, base) = ds_storage::build_store(data_path)?;
                let base_prefix =
                    combine_http_prefix(base.as_ref(), config.prefix_pattern.as_deref());
                let time_window = match &config.time_window {
                    Some(s) => Some(TimeWindow::parse(s)?),
                    None => None,
                };

                match discovery_mode(config)? {
                    // Template probe — for non-listable autoindex servers
                    // (DWD opendata): no `list`, HEAD candidate filenames
                    // built from `filename_template` + `cadence_secs`.
                    DiscoveryMode::Template => {
                        let template = config
                            .filename_template
                            .clone()
                            .ok_or(EngineError::TemplateNeedsFilenameTemplate)?;
                        let cadence_secs = config
                            .cadence_secs
                            .filter(|c| *c > 0)
                            .ok_or(EngineError::TemplateNeedsCadence)?;
                        tracing::info!(
                            "[{}] ODIM HTTP template source: url={data_path} prefix='{base_prefix}' \
                             cadence={cadence_secs}s",
                            collection_id
                        );
                        Ok(Source::TemplateHttp {
                            store,
                            base_url: data_path.to_string(),
                            base_prefix,
                            template,
                            cadence: Duration::from_secs(cadence_secs),
                            time_window,
                        })
                    }
                    // List mode (default) — WebDAV `PROPFIND`. NOTE: a plain
                    // Apache/nginx autoindex is *not* listable; use
                    // `discovery = "template"` for those.
                    DiscoveryMode::List => {
                        tracing::info!(
                            "[{}] ODIM HTTP source: url={data_path} prefix='{base_prefix}'",
                            collection_id
                        );
                        Ok(Source::Remote {
                            store,
                            origin: RemoteOrigin::Http {
                                base_url: data_path.to_string(),
                            },
                            prefix_pattern: base_prefix,
                            time_window,
                        })
                    }
                }
            } else {
                Ok(Source::Local {
                    data_dir: PathBuf::from(data_path),
                })
            }
        }
        _ => Err(EngineError::IncompleteS3Config),
    }
}

/// Parsed `[odim].discovery` mode.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DiscoveryMode {
    List,
    Template,
}

/// Resolve the discovery mode from config: absent / `"list"` → list,
/// `"template"` → template, anything else → an explicit error.
fn discovery_mode(config: &ds_core::config::OdimConfig) -> Result<DiscoveryMode, EngineError> {
    match config.discovery.as_deref() {
        None | Some("list") => Ok(DiscoveryMode::List),
        Some("template") => Ok(DiscoveryMode::Template),
        Some(other) => Err(EngineError::UnknownDiscovery {
            value: other.to_string(),
        }),
    }
}

/// Join an HTTP store's base path (the URL path from
/// [`ds_storage::build_store`]) with an optional `prefix_pattern`.
///
/// The base is a literal directory (no strftime codes); the pattern,
/// when present, is appended so its `%Y/%m/%d/…` codes still expand
/// per UTC date during the scan. An empty/absent pattern leaves the
/// base alone; an empty base (URL pointing at the store root) yields
/// the pattern by itself.
fn combine_http_prefix(base_path: &str, prefix_pattern: Option<&str>) -> String {
    let base = base_path.trim_matches('/');
    // `trim()` first so a whitespace-only pattern counts as absent, then
    // strip surrounding slashes so the join produces exactly one
    // separator and no trailing slash (a lone `/` pattern → empty → base).
    let pattern = prefix_pattern
        .map(|p| p.trim().trim_matches('/'))
        .filter(|p| !p.is_empty());
    match pattern {
        Some(pattern) if base.is_empty() => pattern.to_string(),
        Some(pattern) => format!("{base}/{pattern}"),
        None => base.to_string(),
    }
}

/// Human-readable description of a [`Source`] for error messages —
/// names the local directory, the S3 endpoint/bucket/prefix, or the
/// HTTP base URL/prefix, so an operator reading a log entry knows
/// exactly which store to check.
fn source_label(source: &Source) -> String {
    match source {
        Source::Local { data_dir } => data_dir.display().to_string(),
        Source::Remote {
            origin,
            prefix_pattern,
            ..
        } => match origin {
            RemoteOrigin::S3 { endpoint, bucket } => {
                format!("s3 {endpoint}/{bucket}/{prefix_pattern}")
            }
            RemoteOrigin::Http { base_url } => {
                format!("http {base_url} (prefix '{prefix_pattern}')")
            }
        },
        Source::TemplateHttp {
            base_url,
            base_prefix,
            ..
        } => format!("http {base_url} (template-probe, prefix '{base_prefix}')"),
    }
}

impl MapEngine for OdimEngine {
    #[allow(clippy::too_many_arguments)]
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
        let entry = self.select_entry(time).ok_or_else(|| {
            DataServerError::Engine(format!(
                "[{}] empty ODIM catalog — no files available",
                self.collection_id
            ))
        })?;
        let composite = self.load_composite(&entry.location)?;

        let gain = self.gain_override.unwrap_or(composite.gain);
        let offset = self.offset_override.unwrap_or(composite.offset);
        let nodata = self.nodata_override.unwrap_or(composite.nodata);
        let undetect = composite.undetect;

        let [src_w, src_s, src_e, src_n] = composite.bbox;
        let src_dx = (src_e - src_w) / composite.xsize as f64;
        let src_dy = (src_n - src_s) / composite.ysize as f64;
        let (rows, cols) = composite.pixels.shape();

        // Map output pixels to source pixels through a coarse projection grid
        // instead of forward-projecting every pixel: `crs.forward` is a full
        // CRS transform (≈ a dozen transcendental ops for TM/LAEA/LCC/Stereo)
        // and per-pixel it dominates render CPU on large viewports (#236/#203).
        // The output→world axis mapping (linear lon/lat, Mercator Y, or a
        // projected output CRS such as EPSG:3067/3035) is the shared
        // `OutputCrs::project_node` (#160).
        //
        // World (lon/lat) → fractional source pixel. ODIM rows go north→south,
        // so the row index counts from the north edge — that orientation lives
        // here, not in the sampling loop.
        let world_to_src_px = |lon: f64, lat: f64| {
            let (x, y) = composite.crs.forward(lon, lat);
            ((x - src_w) / src_dx, (src_n - y) / src_dy)
        };
        let grid = ProjectionGrid::build_2d(
            width,
            height,
            cols as u32,
            rows as u32,
            |fx, fy| output_crs.project_node(bbox, fx, fy),
            world_to_src_px,
        );

        // Domain guard against "ghost" echoes: at low zoom / extreme viewports
        // the coarse grid (and the projection's out-of-domain forward) can map a
        // far-away output pixel onto a valid source pixel, painting the composite
        // far from its coverage. Bound the output window to the source footprint
        // (WGS84 `seed_spatial_extent`); everything outside is nodata (#449).
        let (px_lo, px_hi, py_lo, py_hi) =
            output_crs.footprint_pixel_window(bbox, self.seed_spatial_extent, width, height);

        // Resample source grid to output dimensions using nearest-neighbour.
        // The grid interpolates only the output→source coordinate map; the data
        // values are still sampled nearest-neighbour (radar dBZ must not be
        // blended across nodata/undetect edges).
        let mut values = Vec::with_capacity((width * height) as usize);
        for oy in 0..height {
            let in_y = oy >= py_lo && oy <= py_hi;
            for ox in 0..width {
                if !in_y || ox < px_lo || ox > px_hi {
                    values.push(None);
                    continue;
                }
                let (col_f, row_f) = grid.sample(ox, oy);
                if !col_f.is_finite() || !row_f.is_finite() {
                    values.push(None);
                    continue;
                }
                let col = col_f.floor() as i64;
                let row = row_f.floor() as i64;
                if col < 0 || col >= cols as i64 || row < 0 || row >= rows as i64 {
                    values.push(None);
                    continue;
                }
                values.push(composite.pixels.sample(
                    row as usize,
                    col as usize,
                    gain,
                    offset,
                    nodata,
                    undetect,
                ));
            }
        }

        Ok(RasterTile {
            width,
            height,
            values,
        })
    }

    fn raster_info(&self) -> RasterInfo {
        // `native_crs` / `spatial_extent` come from the seed fields
        // captured at construction — every timestep shares the same
        // grid, so this needs neither the render-cache `Mutex` nor a
        // warmed cache. Times come from the lock-free `ArcSwap`
        // catalog snapshot.
        let times: Vec<DateTime<Utc>> = self.catalog.load().iter().map(|e| e.time).collect();

        RasterInfo {
            native_crs: self.seed_native_crs.clone(),
            spatial_extent: Some(self.seed_spatial_extent),
            times,
            parameter: self.parameter.clone(),
            unit: self.unit.clone(),
            parameters: vec![],
            vertical: None, // 2-D composite, no vertical dimension
            grid_size: Some([self.seed_xsize, self.seed_ysize]),
            layer_subtitle: None,
            reference_times: Vec::new(),
        }
    }
}

/// Human-readable identifier for a `Crs`. Used by `raster_info()` —
/// approximate, not an authoritative EPSG mapping. ODIM composites in the wild
/// use spheres, so EPSG codes don't strictly apply; this engine deliberately
/// never claims one (unlike engine-geotiff, which upgrades TM35FIN/ETRS89-LAEA
/// ellipsoidal grids to their EPSG codes).
///
/// The generic (non-EPSG) labels must match engine-geotiff's vocabulary so
/// that `ds_core::geo::native_crs_uri` — the single source of truth for
/// `storageCrs` — maps both engines' output consistently.
fn crs_label(crs: &Crs) -> String {
    match crs {
        Crs::Wgs84 => "CRS:84".into(),
        Crs::TransverseMercator { .. } => "TM".into(),
        Crs::LambertAzimuthalEqualArea { .. } => "LAEA".into(),
        Crs::LambertConformalConic { .. } => "LCC".into(),
        Crs::Stereographic { .. } => "stere".into(),
        Crs::RotatedLatLon { .. } => "rotated_ll".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crs_label_covers_every_variant() {
        // If a new `Crs` variant lands without a label, this test
        // forces the addition by triggering an unhandled-match
        // compile error first, not a silent "unknown" at runtime.
        assert_eq!(crs_label(&Crs::Wgs84), "CRS:84");
        let stere = Crs::Stereographic {
            lat0: 0.0,
            lon0: 0.0,
            k0: 1.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        // Generic labels must match engine-geotiff's vocabulary (no EPSG claim
        // for spherical composites) so native_crs_uri maps both consistently.
        assert_eq!(crs_label(&stere), "stere");
        let rot = Crs::RotatedLatLon {
            south_pole_lat: 0.0,
            south_pole_lon: 0.0,
        };
        assert_eq!(crs_label(&rot), "rotated_ll");
        // None of the generic projected labels resolve to a storageCrs URI.
        assert!(ds_core::geo::native_crs_uri("stere").is_none());
        assert!(ds_core::geo::native_crs_uri("rotated_ll").is_none());
    }

    /// Minimal `OdimConfig` for `build_source` routing tests — only the
    /// fields the source resolver reads are meaningful; the rest are
    /// zero/`None`.
    fn config_with(
        endpoint: Option<&str>,
        bucket: Option<&str>,
        prefix_pattern: Option<&str>,
        time_window: Option<&str>,
    ) -> ds_core::config::OdimConfig {
        ds_core::config::OdimConfig {
            filename_template: Some("%Y%m%dT%H%M_radar.h5".into()),
            filename_pattern: None,
            timestamp_format: None,
            parameter: Some("reflectivity".into()),
            unit: Some("dBZ".into()),
            nodata: None,
            gain: None,
            offset: None,
            poll_interval_secs: 30,
            max_files: None,
            endpoint: endpoint.map(str::to_string),
            bucket: bucket.map(str::to_string),
            prefix_pattern: prefix_pattern.map(str::to_string),
            time_window: time_window.map(str::to_string),
            discovery: None,
            cadence_secs: None,
            resampling: Default::default(),
            prewarm_sweeps: 1,
        }
    }

    /// The composite cache is **multi-entry**, not single-slot (#212):
    /// loading composite A, then B, then A again must serve A from the cache
    /// — the load of B must NOT evict A (and A's reload must not evict B).
    ///
    /// Proved by pointer identity: the global [`COMPOSITE_CACHE`] hands back
    /// the same `Arc` for a hit, so `Arc::ptr_eq` of the first and second load
    /// of the same key is true iff the entry survived the intervening load of
    /// the *other* key. The old single-slot cache would have re-decoded
    /// (yielding a fresh `Arc`, `!ptr_eq`) because B's load overwrote the one
    /// slot. Two distinct local paths (two copies of the committed DMI
    /// fixture under timestamped names) give two distinct cache keys; the
    /// unique tempdir path keeps them from colliding with any other test's
    /// global-cache entries.
    #[test]
    fn composite_cache_keeps_distinct_timesteps_resident() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/odim-dmi-fixture.h5")
            .canonicalize()
            .expect("fixture path canonicalises");
        // Two timesteps matching the template `%Y%m%dT%H%M_radar.h5`.
        let path_a = dir.path().join("20260120T1125_radar.h5");
        let path_b = dir.path().join("20260120T1130_radar.h5");
        std::fs::copy(&src, &path_a).expect("copy fixture A");
        std::fs::copy(&src, &path_b).expect("copy fixture B");

        let config = config_with(None, None, None, None);
        let engine = OdimEngine::new(
            "composite-cache-test",
            Some(dir.path().to_str().expect("utf8 fixture path")),
            &config,
        )
        .expect("OdimEngine::new succeeds over the two-timestep dir");

        let loc_a = Location::Local(path_a);
        let loc_b = Location::Local(path_b);

        let (hits_before, _, _, _) = composite_cache_metrics();

        // A → B → A → B. With a multi-entry LRU both stay resident, so the
        // second load of each key is a hit returning the identical `Arc`.
        let a1 = engine.load_composite(&loc_a).expect("load A (1)");
        let b1 = engine.load_composite(&loc_b).expect("load B (1)");
        let a2 = engine.load_composite(&loc_a).expect("load A (2)");
        let b2 = engine.load_composite(&loc_b).expect("load B (2)");

        assert!(
            Arc::ptr_eq(&a1, &a2),
            "loading B must not evict A — A should be served from the cache"
        );
        assert!(
            Arc::ptr_eq(&b1, &b2),
            "reloading A must not evict B — B should be served from the cache"
        );
        // Same fixture ⇒ identical grid dims (sanity that both decoded).
        assert_eq!((a1.xsize, a1.ysize), (b1.xsize, b1.ysize));

        // Global hit counter only grows, so a `>=` delta is race-free under
        // parallel tests: our own reloads of A and B are at least two hits.
        let (hits_after, _, bytes, cap) = composite_cache_metrics();
        assert!(
            hits_after >= hits_before + 2,
            "expected ≥2 composite-cache hits from the two reloads (before={hits_before}, after={hits_after})"
        );
        // Default cap is 1 GiB; both small DMI composites fit, so bytes > 0.
        assert!(cap > 0, "composite cache should report a positive capacity");
        assert!(
            bytes > 0,
            "two resident composites should report a positive resident weight"
        );
    }

    #[test]
    fn combine_http_prefix_joins_base_and_pattern() {
        // URL path only (DWD-style flat directory) — no date pattern.
        assert_eq!(
            combine_http_prefix("weather/radar/composite/hx", None),
            "weather/radar/composite/hx"
        );
        // Base + date pattern: the strftime codes survive for per-date
        // expansion during the scan.
        assert_eq!(
            combine_http_prefix("radar", Some("%Y/%m/%d/COMP/")),
            "radar/%Y/%m/%d/COMP"
        );
        // Surrounding slashes on either side collapse to a single join.
        assert_eq!(
            combine_http_prefix("/radar/", Some("/%Y/%m/%d/")),
            "radar/%Y/%m/%d"
        );
        // Store-root URL (empty path) with a pattern yields the pattern alone.
        assert_eq!(combine_http_prefix("", Some("%Y/%m/%d/")), "%Y/%m/%d");
        // Store-root URL with no pattern is the empty (root) prefix.
        assert_eq!(combine_http_prefix("", None), "");
        // A whitespace-only / empty pattern is treated as absent.
        assert_eq!(combine_http_prefix("radar", Some("")), "radar");
    }

    /// An `http(s)://` `data_path` (no `endpoint`/`bucket`) resolves to a
    /// remote HTTP source, not a local directory — the core of #286.
    /// Building the store is lazy (no network), so this runs offline.
    #[test]
    fn build_source_routes_http_data_path_to_remote() {
        let config = config_with(None, None, None, None);
        let source = build_source(
            "http-test",
            Some("https://opendata.example.org/weather/radar/composite/hx/"),
            &config,
        )
        .expect("http data_path builds a remote source");

        match &source {
            Source::Remote {
                origin,
                prefix_pattern,
                ..
            } => {
                assert!(
                    matches!(origin, RemoteOrigin::Http { .. }),
                    "http data_path must select an HTTP origin"
                );
                // The URL path becomes the (flat) list prefix.
                assert_eq!(prefix_pattern, "weather/radar/composite/hx");
            }
            _ => panic!("http data_path (list mode) must be a Source::Remote"),
        }

        // The label names the HTTP URL, not an S3 bucket.
        let label = source_label(&source);
        assert!(
            label.starts_with("http https://opendata.example.org/"),
            "source_label should name the HTTP base URL, got `{label}`"
        );
    }

    /// URL schemes are case-insensitive (RFC 3986) — an uppercase
    /// `HTTPS://` `data_path` must still route to a remote source, not
    /// fall through to a `Source::Local` that errors as a missing
    /// directory.
    #[test]
    fn build_source_http_scheme_is_case_insensitive() {
        let config = config_with(None, None, None, None);
        let source = build_source("http-test", Some("HTTPS://host.example/radar/"), &config)
            .expect("uppercase https data_path builds a remote source");
        assert!(
            matches!(source, Source::Remote { .. }),
            "an HTTPS:// data_path must select a remote source"
        );
    }

    /// A `prefix_pattern` alongside an HTTP `data_path` is appended under
    /// the URL path so date partitioning still works on a listable store.
    #[test]
    fn build_source_http_appends_prefix_pattern() {
        let config = config_with(None, None, Some("%Y/%m/%d/"), Some("-PT2H"));
        let source = build_source("http-test", Some("https://host.example/radar/"), &config)
            .expect("http data_path with prefix_pattern builds a remote source");
        match source {
            Source::Remote {
                prefix_pattern,
                time_window,
                ..
            } => {
                assert_eq!(prefix_pattern, "radar/%Y/%m/%d");
                assert!(
                    time_window.is_some(),
                    "time_window must apply to an HTTP source"
                );
            }
            _ => panic!("expected a remote HTTP source"),
        }
    }

    /// `discovery = "template"` on an `http(s)://` source with a
    /// `filename_template` + `cadence_secs` builds a `TemplateHttp` source
    /// (the #287 listing-free probe), carrying the URL path as the base
    /// prefix and the template + cadence verbatim.
    #[test]
    fn build_source_template_mode_builds_template_http() {
        let mut config = config_with(None, None, None, Some("-PT2H"));
        config.filename_template = Some("composite_hx_%Y%m%d_%H%M-hd5".into());
        config.discovery = Some("template".into());
        config.cadence_secs = Some(300);

        let source = build_source(
            "dwd",
            Some("https://opendata.dwd.de/weather/radar/composite/hx/"),
            &config,
        )
        .expect("template-mode http source builds");
        match &source {
            Source::TemplateHttp {
                base_prefix,
                template,
                cadence,
                time_window,
                ..
            } => {
                assert_eq!(base_prefix, "weather/radar/composite/hx");
                assert_eq!(template, "composite_hx_%Y%m%d_%H%M-hd5");
                assert_eq!(*cadence, Duration::from_secs(300));
                assert!(time_window.is_some());
            }
            _ => panic!("discovery=template must build a TemplateHttp source"),
        }
        // The label flags the listing-free probe mode.
        assert!(
            source_label(&source).contains("template-probe"),
            "source_label should mark template-probe mode, got `{}`",
            source_label(&source)
        );
    }

    /// Template mode requires `cadence_secs` (> 0) and `filename_template`,
    /// only applies to HTTP, and rejects an unknown `discovery` value.
    #[test]
    fn build_source_template_mode_validation() {
        let http = "https://h.example/radar/";

        // Missing cadence.
        let mut c = config_with(None, None, None, None);
        c.filename_template = Some("c_%Y%m%d_%H%M.h5".into());
        c.discovery = Some("template".into());
        assert!(matches!(
            build_source("t", Some(http), &c),
            Err(EngineError::TemplateNeedsCadence)
        ));
        // cadence = 0 is also rejected.
        c.cadence_secs = Some(0);
        assert!(matches!(
            build_source("t", Some(http), &c),
            Err(EngineError::TemplateNeedsCadence)
        ));

        // Template form required — the `filename_pattern` form can't be inverted.
        let mut c = config_with(None, None, None, None);
        c.filename_template = None;
        c.filename_pattern = Some(r"^c-(?P<timestamp>\d{12})\.h5$".into());
        c.timestamp_format = Some("%Y%m%d%H%M".into());
        c.discovery = Some("template".into());
        c.cadence_secs = Some(300);
        assert!(matches!(
            build_source("t", Some(http), &c),
            Err(EngineError::TemplateNeedsFilenameTemplate)
        ));

        // Template mode only applies to HTTP — rejected on a local data_path.
        let mut c = config_with(None, None, None, None);
        c.filename_template = Some("c_%Y%m%d_%H%M.h5".into());
        c.discovery = Some("template".into());
        c.cadence_secs = Some(300);
        assert!(matches!(
            build_source("t", Some("/var/lib/radar"), &c),
            Err(EngineError::TemplateNeedsHttp)
        ));

        // Unknown discovery value is an explicit error.
        let mut c = config_with(None, None, None, None);
        c.discovery = Some("bogus".into());
        assert!(matches!(
            build_source("t", Some(http), &c),
            Err(EngineError::UnknownDiscovery { value }) if value == "bogus"
        ));
    }

    /// A plain (non-URL) `data_path` is still a local directory source.
    #[test]
    fn build_source_routes_plain_data_path_to_local() {
        let config = config_with(None, None, None, None);
        let source =
            build_source("local-test", Some("/var/lib/radar"), &config).expect("local source");
        assert!(matches!(source, Source::Local { .. }));
    }

    /// `shutdown()` called before `poll_loop()` ever starts must
    /// still cause the loop to exit on its first iteration. The
    /// earlier `watch::Sender<()>`-based implementation lost this
    /// signal because `send()` returns `Err` when there are no
    /// receivers, and the initial receiver was dropped at
    /// `let (tx, _) = watch::channel(())`. The `AtomicBool`-based
    /// flag is edge-triggered and persists across the
    /// before-/after-subscribe boundary.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_before_poll_loop_takes_effect() {
        // Build the smallest possible OdimEngine-like setup. We
        // don't need a real catalog or filesystem — just the
        // shutdown coordination. Use a synthetic loop mirroring
        // `poll_loop`'s structure exactly.
        let shutdown = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());

        // Pre-fire the shutdown signal *before* the loop starts —
        // simulating the race the reviewer flagged. The
        // `notify_waiters` call here has no live waiters and is
        // discarded, mirroring the worst-case timing: the loop
        // must exit anyway via the AtomicBool flag check.
        shutdown.store(true, Ordering::Release);
        notify.notify_waiters();

        let shutdown_loop = shutdown.clone();
        let notify_loop = notify.clone();
        let handle = tokio::spawn(async move {
            // Long interval so the timer can't accidentally rescue
            // us if the flag check is buggy — the test would have
            // to hit the 1-second timeout below instead.
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            interval.tick().await;
            loop {
                if shutdown_loop.load(Ordering::Acquire) {
                    return;
                }
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = notify_loop.notified() => {}
                }
            }
        });

        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("poll loop must exit on the first iteration when shutdown is pre-set")
            .unwrap();
    }
}
