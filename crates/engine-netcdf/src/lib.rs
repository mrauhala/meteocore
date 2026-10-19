//! CF-NetCDF gridded forecasts and analyses (#1067), served through WMS,
//! OGC API Maps and Tiles, and OGC API EDR position, area and radius
//! queries ([`edr`]).
//!
//! One collection is one file family, such as MEPS's per-lead surface files:
//! every file carries the collection's variables on one grid, its valid
//! times in `time` and its model run in `forecast_reference_time`. The poll
//! loop finds the files, in a local directory or by probing an HTTP server
//! for the names a template gives ([`remote`]), opens each new file once
//! and keeps its handle while the catalog keeps the file
//! ([`catalog::OpenFile`]); a render reads one 2-D field through the
//! engine-wide field cache ([`field`]) and resamples it with
//! `ds_core::resample::SourceGrid`. A remote file is read in place by HTTP
//! range, never downloaded whole. A family on a CF pressure axis (MEPS's
//! `pl` files) serves its levels as the collection's vertical axis, in hPa,
//! bottom first: one field is one level ([`catalog::Layout::map_level`],
//! [`catalog::Layout::query_levels`]).

mod catalog;
pub mod decode_budget;
mod discover;
mod edr;
mod field;
mod ingest;
mod io;
mod remote;
#[cfg(test)]
mod scan_tests;
#[cfg(test)]
mod test_server;

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::config::NetcdfConfig;
use ds_core::deadline;
use ds_core::error::DataServerError;
use ds_core::geo::GeoTransform;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterInfo, RasterTile};
use ds_poll::{FirstTick, Shutdown};
use ds_storage::discovery::{
    FilenameMatcher, Schedule, Settling, TemplateProber, TemplateSpec, TimeWindow,
};
use netcdf_reader::{DynStorage, NcFile};

pub use field::{detached_fill_metrics, field_cache_metrics};
pub use remote::{block_cache_metrics, chunk_cache_metrics};

use catalog::{Catalog, Identity, Layout, OpenFile, Run, RunKey, Step};
use discover::{Listed, Names};
use ingest::{Inspected, Wanted};
use io::Mode;
use remote::{Changes, Host, RemoteFile, Tuning};

/// Files one poll opens: new and replaced files, newest first, then
/// retained files to open again. A directory of many files is opened over
/// several polls instead of in one long pass (Critical Rule 9). A remote
/// source's cap is at least one run's leads, so a poll always admits a
/// whole run.
const MAX_OPENS_PER_POLL: usize = 128;

/// Remote files a poll opens at once: each open is a few requests (the
/// 1 MiB first read, then blocks of the chunk indexes), which wait for the
/// host's background share ([`remote::BACKGROUND_SHARE`]) while the others
/// parse metadata (Critical Rule 9).
const REMOTE_OPEN_CONCURRENCY: usize = 8;

/// How long [`NetcdfEngine::new`] spends on a remote source's first scan.
/// Collections are built one after another and the server answers no
/// request until all are, so this bounds what a remote collection adds to
/// boot. Probing the newest run takes two rounds of HEADs and opening its
/// 67 MEPS files about 67 MB: seconds from a server's network, which this
/// leaves room for; over a slow link the poll loop, starting at once,
/// opens the rest.
const INITIAL_SCAN_BUDGET: Duration = Duration::from_secs(15);

/// Prober rounds the first scan of a remote source makes until a run is
/// published: the first finds the newest runs' sentinels, the second HEADs
/// their other leads, which settle at once when old enough.
const INITIAL_PROBE_ROUNDS: usize = 3;

pub struct NetcdfEngine {
    collection_id: Arc<str>,
    source: Source,
    variables: Option<Vec<String>>,
    titles: BTreeMap<String, String>,
    member: u32,
    max_runs: usize,
    max_files: usize,
    poll_interval: Duration,
    catalog: ArcSwap<Catalog>,
    /// What the poll knows about the files; only the poll takes it.
    state: Mutex<PollState>,
    /// Fields this engine decoded (cache fills), for tests and diagnostics.
    decodes: Arc<AtomicU64>,
    /// What remote reads found replaced, for the poll and `content_version`.
    changes: Arc<Changes>,
    shutdown: Shutdown,
}

/// Where a collection's files are.
enum Source {
    /// A local directory, listed.
    Local { dir: PathBuf, names: Names },
    /// An HTTP server, probed by template and read by range.
    Remote(Box<Remote>),
}

/// A remote source ([`remote`]).
struct Remote {
    host: Arc<Host>,
    /// `https://thredds.met.no`: the files' URLs are this and their keys.
    origin: String,
    prober: Mutex<TemplateProber>,
    tuning: Tuning,
    /// One run's leads: a poll opens at least this many files.
    leads: usize,
    prewarm: Vec<String>,
    /// HEADs this source's probes have made.
    heads: AtomicU64,
}

/// What a remote source has asked of its host
/// ([`NetcdfEngine::remote_traffic`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteTraffic {
    /// HEADs this collection's probes made.
    pub heads: u64,
    /// Range GETs made to the host, by every collection on it.
    pub gets: u64,
    /// Bytes those GETs read.
    pub bytes: u64,
}

/// How much one scan may open.
struct Limits {
    opens: usize,
    concurrency: usize,
    /// Stop opening, and fail reads in flight, at this instant.
    until: Option<Instant>,
}

/// What the poll remembers between polls.
struct PollState {
    known: HashMap<PathBuf, Known>,
    layout: Option<Arc<Layout>>,
    content_version: u64,
    /// Whether the mixed runs and run-less files WARN was logged.
    warned_mixed: bool,
    /// Fields prewarmed: (file ingest, variable index, time index).
    prewarmed: HashSet<(u64, usize, u32)>,
}

/// A listed file as the poll last left it.
enum Known {
    /// Ingested, open, and catalogued if the retention keeps it.
    Open(Arc<OpenFile>),
    /// Ingested once, then closed: outside the runs or files kept. Opened
    /// again if it comes back into them.
    Closed {
        identity: Identity,
        run: Option<DateTime<Utc>>,
        newest: DateTime<Utc>,
    },
    /// Not servable; why was logged when it was found.
    Skipped { identity: Identity },
}

impl Known {
    fn identity(&self) -> &Identity {
        match self {
            Known::Open(file) => &file.identity,
            Known::Closed { identity, .. } | Known::Skipped { identity } => identity,
        }
    }

    /// The run and newest valid time of an ingested file.
    fn place(&self) -> Option<(Option<DateTime<Utc>>, DateTime<Utc>)> {
        match self {
            Known::Open(file) => Some((file.run, *file.times.iter().max()?)),
            Known::Closed { run, newest, .. } => Some((*run, *newest)),
            Known::Skipped { .. } => None,
        }
    }
}

/// A file [`NetcdfEngine::open_inspect`] read, before it is admitted to the
/// collection's layout.
enum Opened {
    Ready(Box<Ready>),
    /// Not servable: remembered and not opened again until it changes.
    Skip(String),
    /// A remote read failed: forgotten, and tried again next poll.
    Retry(String),
}

struct Ready {
    id: u64,
    nc: NcFile,
    remote: Option<Arc<RemoteFile>>,
    inspected: Inspected,
}

/// A new process-unique ingest id ([`OpenFile::id`]).
fn next_file_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl NetcdfEngine {
    /// Build the engine and scan once, as engine-grib does, so the layers
    /// and their styles exist when the collection is registered. A local
    /// directory is scanned whole; a remote source's first scan is bounded
    /// ([`INITIAL_SCAN_BUDGET`]) to its newest run and the poll loop opens
    /// the rest. A failed scan logs a WARN and the poll retries it.
    pub fn new(collection_id: &str, config: &NetcdfConfig) -> Result<Self, DataServerError> {
        Self::with_tuning(collection_id, config, Tuning::DEFAULT)
    }

    /// [`Self::new`] with a remote source's read sizes.
    fn with_tuning(
        collection_id: &str,
        config: &NetcdfConfig,
        tuning: Tuning,
    ) -> Result<Self, DataServerError> {
        ds_core::config::validate_netcdf(collection_id, config)?;
        let data_path = config.data_path.as_deref().unwrap_or_default();
        let template_error = |e: &dyn std::fmt::Display| {
            DataServerError::Config(format!(
                "Collection '{collection_id}': [netcdf].filename_template: {e}"
            ))
        };
        let source = if config.is_remote() {
            Source::Remote(Box::new(Self::remote(collection_id, config, tuning)?))
        } else {
            let names = match config.filename_template.as_deref() {
                Some(template) => Names::Template(
                    FilenameMatcher::from_template(template).map_err(|e| template_error(&e))?,
                ),
                None => Names::NetcdfSuffix,
            };
            Source::Local {
                dir: PathBuf::from(data_path),
                names,
            }
        };
        // Nonzero and new on every start, so a browser's cached explicit-time
        // images revalidate after a restart that may have replaced files.
        let content_version = chrono::Utc::now().timestamp().max(1) as u64;
        let engine = Self {
            collection_id: collection_id.into(),
            source,
            variables: config.variables.clone(),
            titles: config.titles.clone(),
            member: config.member,
            max_runs: config.max_runs,
            max_files: config.max_files,
            poll_interval: Duration::from_secs(config.poll_interval_secs),
            catalog: ArcSwap::from_pointee(Catalog::empty(content_version)),
            state: Mutex::new(PollState {
                known: HashMap::new(),
                layout: None,
                content_version,
                warned_mixed: false,
                prewarmed: HashSet::new(),
            }),
            decodes: Arc::new(AtomicU64::new(0)),
            changes: Arc::new(Changes::default()),
            shutdown: Shutdown::new(),
        };
        if let Err(e) = engine.initial_scan() {
            tracing::warn!(
                "Collection '{collection_id}': initial NetCDF scan failed (will retry on poll): {e}"
            );
        }
        Ok(engine)
    }

    /// The remote source of `config`: the host's stores and a template
    /// prober over MEPS-like run and lead names.
    fn remote(
        collection_id: &str,
        config: &NetcdfConfig,
        tuning: Tuning,
    ) -> Result<Remote, DataServerError> {
        let data_path = config.data_path.as_deref().unwrap_or_default();
        let (host, base) = remote::host(data_path)?;
        let duration = |value: Option<&str>, default| match value {
            Some(value) => ds_core::datetime::parse_iso8601_duration(value),
            None => Ok(default),
        };
        let window = config.time_window.as_deref().unwrap_or_default();
        let leads = config.leads.clone().unwrap_or_default();
        let spec = TemplateSpec {
            prefix_pattern: config.prefix_pattern.clone().unwrap_or_default(),
            leads: leads.clone(),
            time_window: Some(TimeWindow::parse(window)?),
            settling: Settling::Stable {
                min_age: duration(
                    config.min_age.as_deref(),
                    ds_storage::discovery::DEFAULT_MIN_AGE,
                )?,
            },
            run_grace: duration(
                config.run_grace.as_deref(),
                ds_storage::discovery::DEFAULT_RUN_GRACE,
            )?,
            ..TemplateSpec::new(
                base.as_ref(),
                config.filename_template.clone().unwrap_or_default(),
                Schedule::Runs {
                    run_hours: config.run_hours.clone().unwrap_or_default(),
                },
                collection_id,
            )
        };
        Ok(Remote {
            host,
            origin: remote::origin(data_path).to_string(),
            prober: Mutex::new(TemplateProber::new(spec)?),
            tuning,
            leads: leads.len().max(1),
            prewarm: config.prewarm_variables.clone(),
            heads: AtomicU64::new(0),
        })
    }

    /// Poll until [`Self::shutdown`]. Run on the background poll runtime
    /// (Critical Rule 6): each tick lists or probes the files and opens new
    /// ones. [`Self::new`] has made the first scan: a local one whole, so
    /// the first tick waits a period; a remote one bounded, so the first
    /// tick goes on at once.
    pub async fn poll_loop(&self) {
        let first = match self.source {
            Source::Local { .. } => FirstTick::Skip,
            Source::Remote(_) => FirstTick::Immediate,
        };
        let mut ticker = self.shutdown.ticker(self.poll_interval, first);
        while ticker.tick().await {
            self.poll_once();
        }
        tracing::info!("[{}] netcdf poll loop shutting down", self.collection_id);
    }

    pub fn shutdown(&self) {
        self.shutdown.shutdown();
    }

    pub fn collection_id(&self) -> &str {
        &self.collection_id
    }

    /// Live `/health` status: ready while discovery works and the catalog
    /// has data.
    pub fn live_health(&self) -> Option<ds_core::health::LiveStatus> {
        use ds_core::health::LiveStatus;
        let catalog = self.catalog.load();
        let Some(polled_at) = catalog.polled_at else {
            return Some(LiveStatus::Degraded {
                reason: "waiting for the first poll",
            });
        };
        let stale_after = chrono::Duration::from_std(self.poll_interval * 5)
            .unwrap_or(chrono::Duration::MAX)
            .max(chrono::Duration::minutes(10));
        Some(if Utc::now() - polled_at > stale_after {
            LiveStatus::Degraded {
                reason: match self.source {
                    Source::Local { .. } => "listing the data directory has failed repeatedly",
                    Source::Remote(_) => "probing the remote files has failed repeatedly",
                },
            }
        } else if catalog.runs.is_empty() {
            LiveStatus::Degraded {
                reason: "no servable NetCDF files found",
            }
        } else {
            LiveStatus::Ready
        })
    }

    /// Age of the newest data, for `/health`'s `data_age_secs`: the newest
    /// run's reference time, or the newest valid time without runs.
    pub fn data_age(&self) -> Option<chrono::Duration> {
        let catalog = self.catalog.load();
        let newest = match catalog.runs.last_key_value()? {
            (Some(reference_time), _) => *reference_time,
            (None, run) => *run.steps.last_key_value()?.0,
        };
        Some(Utc::now() - newest)
    }

    /// Every served valid time, ascending.
    pub fn times(&self) -> Vec<DateTime<Utc>> {
        self.catalog.load().info.times.clone()
    }

    /// The grid fields are rendered from, north-up, once a file is ingested.
    pub fn grid(&self) -> Option<GeoTransform> {
        let catalog = self.catalog.load();
        Some(catalog.layout.as_ref()?.grid.transform.clone())
    }

    /// Fields this engine has decoded, every cache fill counted once.
    pub fn field_decodes(&self) -> u64 {
        self.decodes.load(Ordering::Relaxed)
    }

    /// The requests a remote source has made and the bytes it has read,
    /// for diagnostics and the live test; `None` for a local directory.
    pub fn remote_traffic(&self) -> Option<RemoteTraffic> {
        match &self.source {
            Source::Local { .. } => None,
            Source::Remote(remote) => Some(RemoteTraffic {
                heads: remote.heads.load(Ordering::Relaxed),
                gets: remote.host.gets.load(Ordering::Relaxed),
                bytes: remote.host.requests.bytes_read(),
            }),
        }
    }

    /// Find new and replaced files, open them, keep the newest runs (or
    /// files) and publish the catalog; a remote source then prewarms. A
    /// source that cannot be listed or probed keeps the catalog served.
    pub fn poll_once(&self) {
        let started = Instant::now();
        let limits = match &self.source {
            Source::Local { .. } => Limits {
                opens: MAX_OPENS_PER_POLL,
                concurrency: 1,
                until: None,
            },
            Source::Remote(remote) => Limits {
                opens: MAX_OPENS_PER_POLL.max(remote.leads),
                concurrency: REMOTE_OPEN_CONCURRENCY,
                until: None,
            },
        };
        if let Err(e) = self.scan(&limits, 1) {
            tracing::warn!("[{}] NetCDF poll failed: {e}", self.collection_id);
            return;
        }
        if let Source::Remote(remote) = &self.source {
            self.prewarm(remote, started + self.poll_interval);
        }
    }

    /// [`Self::new`]'s scan: a local directory whole; a remote source's
    /// newest published run, within [`INITIAL_SCAN_BUDGET`].
    fn initial_scan(&self) -> Result<(), String> {
        match &self.source {
            Source::Local { .. } => self.scan(
                &Limits {
                    opens: MAX_OPENS_PER_POLL,
                    concurrency: 1,
                    until: None,
                },
                1,
            ),
            Source::Remote(remote) => {
                let limits = Limits {
                    opens: remote.leads,
                    concurrency: REMOTE_OPEN_CONCURRENCY,
                    until: Some(Instant::now() + INITIAL_SCAN_BUDGET),
                };
                self.scan(&limits, INITIAL_PROBE_ROUNDS)
            }
        }
    }

    /// One scan: list the files (a remote source probed up to
    /// `probe_rounds` times, until a run is published), open new and
    /// replaced ones within `limits`, keep the newest runs (or files) and
    /// publish the catalog. `Err` when the files cannot be listed.
    fn scan(&self, limits: &Limits, probe_rounds: usize) -> Result<(), String> {
        // Every remote read of a poll takes the background share.
        let _mode = io::enter(Mode::Background);
        let _deadline = deadline::enter(limits.until);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let listed = match &self.source {
            Source::Local { dir, names } => discover::list(dir, names, &self.collection_id)
                .map_err(|e| format!("cannot list {}: {e}", dir.display()))?,
            Source::Remote(remote) => self.probe(remote, &mut state, probe_rounds, limits)?,
        };
        let present: HashSet<&PathBuf> = listed.iter().map(|l| &l.path).collect();
        state.known.retain(|path, _| present.contains(path));

        let mut opens = limits.opens;
        let mut deferred = 0usize;
        let mut new = Vec::new();
        for file in &listed {
            if state
                .known
                .get(&file.path)
                .is_some_and(|known| *known.identity() == file.identity)
            {
                continue;
            }
            // A file replaced in place keeps its entry, and its old handle
            // keeps drawing, until its new version is opened (`record`):
            // past the cap, a later poll finds it changed again.
            if opens == 0 {
                deferred += 1;
                continue;
            }
            opens -= 1;
            new.push(file.clone());
        }
        deferred += self.open_files(new, &mut state, limits);

        let keep = self.retained(&mut state);
        let mut reopen = Vec::new();
        let paths: Vec<PathBuf> = state.known.keys().cloned().collect();
        for path in paths {
            let kept = keep.contains(&path);
            match state.known.get(&path) {
                Some(Known::Open(file)) if !kept => {
                    let closed = Known::Closed {
                        identity: file.identity.clone(),
                        run: file.run,
                        newest: file.times.iter().max().copied().unwrap_or_default(),
                    };
                    state.known.insert(path, closed);
                }
                Some(Known::Closed { identity, .. }) if kept => {
                    if opens == 0 {
                        deferred += 1;
                        continue;
                    }
                    opens -= 1;
                    // Stays `Closed` until it is open again.
                    reopen.push(Listed {
                        path: path.clone(),
                        identity: identity.clone(),
                    });
                }
                _ => {}
            }
        }
        deferred += self.open_files(reopen, &mut state, limits);
        if deferred > 0 {
            tracing::info!(
                "[{}] opened {} files this scan, {deferred} left for the next",
                self.collection_id,
                limits.opens - opens
            );
        }

        let catalog = self.build_catalog(&state, &keep);
        tracing::debug!(
            "[{}] poll: {} listed, {} runs, {} times",
            self.collection_id,
            listed.len(),
            catalog.runs.len(),
            catalog.info.times.len()
        );
        self.catalog.store(Arc::new(catalog));
        Ok(())
    }

    /// Probe a remote source and list the published files of its newest
    /// `max_runs` runs, the newest first, each run's in lead order: an
    /// older run would be opened only for the retention to close it.
    /// Files a read found replaced are forgotten and probed again first. A
    /// file the prober withdraws while its run is still in the window and
    /// does not list again was removed, or replaced by a version not
    /// settled yet: its old pixels may be cached, so the content version
    /// moves (a run aging out of the window moves nothing; a new version
    /// listed at once is `record`ed when opened). Up to `rounds` probes,
    /// stopping once a run is published or `limits.until` passes.
    fn probe(
        &self,
        remote: &Remote,
        state: &mut PollState,
        rounds: usize,
        limits: &Limits,
    ) -> Result<Vec<Listed>, String> {
        let mut prober = remote.prober.lock().unwrap_or_else(|e| e.into_inner());
        for (key, version) in self.changes.take_stale() {
            prober.recheck(&key);
            let path = PathBuf::from(&key);
            if state
                .known
                .get(&path)
                .is_some_and(|known| *known.identity() == version)
            {
                if matches!(state.known.get(&path), Some(Known::Open(_))) {
                    state.content_version += 1;
                }
                state.known.remove(&path);
            }
        }
        let mut withdrawn = Vec::new();
        for round in 0..rounds.max(1) {
            if round > 0 && past(limits) {
                break;
            }
            let now = Utc::now();
            let report = prober
                .poll(&remote.host.background, now)
                .map_err(|e| format!("cannot probe {}: {e}", remote.origin))?;
            remote
                .heads
                .fetch_add(report.heads as u64, Ordering::Relaxed);
            // The window's start, as the prober takes it: its span back
            // from now, whatever its sign.
            let start = prober.spec().time_window.as_ref().map(|window| {
                let (from, to) = window.to_range(now);
                now - (to - from)
            });
            withdrawn.extend(
                report
                    .withdrawn
                    .into_iter()
                    .filter(|file| start.is_none_or(|start| file.time >= start)),
            );
            // Every HEAD failing is the server unreachable: the catalog
            // stays as it was, and `/health` degrades if it lasts.
            if report.heads > 0 && report.failed == report.heads {
                return Err(format!(
                    "every one of {} HEADs to {} failed",
                    report.heads, remote.origin
                ));
            }
            if !prober.files().is_empty() || report.heads == 0 {
                break;
            }
        }
        let mut files = prober.files();
        files.sort_by_key(|file| (Reverse(file.time), file.lead));
        let mut runs = 0;
        let mut last = None;
        files.retain(|file| {
            if last != Some(file.time) {
                last = Some(file.time);
                runs += 1;
            }
            runs <= self.max_runs
        });
        for file in withdrawn {
            if files.iter().any(|listed| listed.key == file.key) {
                continue;
            }
            let path = PathBuf::from(&file.key);
            let served = state.known.get(&path).is_some_and(|known| {
                *known.identity() == file.version
                    && matches!(known, Known::Open(_) | Known::Closed { .. })
            });
            if served {
                state.content_version += 1;
                state.known.remove(&path);
            }
        }
        Ok(files
            .into_iter()
            .map(|file| Listed {
                path: PathBuf::from(file.key),
                identity: file.version,
            })
            .collect())
    }

    /// Open, inspect and admit `files` in order, up to `limits.concurrency`
    /// at a time once the collection's layout is known; returns how many
    /// were left for the next poll (a failed remote read, or the scan's
    /// time limit).
    fn open_files(&self, files: Vec<Listed>, state: &mut PollState, limits: &Limits) -> usize {
        let mut files = files.into_iter();
        let mut retry = 0;
        // The first servable file sets the layout, and with it which
        // variables the others are read for.
        while state.layout.is_none() && self.variables.is_none() {
            let Some(file) = files.next() else {
                return retry;
            };
            if past(limits) {
                return retry + 1 + files.count();
            }
            let opened = self.open_inspect(&file, &Wanted::All);
            match self.admit(&file, opened, state, past(limits)) {
                Some(known) => self.record(state, file.path, known),
                None => retry += 1,
            }
        }
        let files: Vec<Listed> = files.collect();
        if files.is_empty() {
            return retry;
        }
        let names: Vec<String> = match (&self.variables, &state.layout) {
            (Some(names), _) => names.clone(),
            (None, Some(layout)) => layout
                .variables
                .iter()
                .map(|v| v.name.to_string())
                .collect(),
            (None, None) => unreachable!("the layout is set when there is no allowlist"),
        };
        let wanted = Wanted::Listed(&names);
        let opened = self.open_parallel(&files, &wanted, limits);
        let cut = past(limits);
        for (file, opened) in files.into_iter().zip(opened) {
            match opened.and_then(|opened| self.admit(&file, opened, state, cut)) {
                Some(known) => self.record(state, file.path, known),
                None => retry += 1,
            }
        }
        retry
    }

    /// Remember what an open made of `path`. When it takes the place of
    /// another version the catalog held, open or closed (a file replaced
    /// in place), the content version moves now, in the catalog this scan
    /// publishes with the new version: the old one's pixels may be cached.
    fn record(&self, state: &mut PollState, path: PathBuf, known: Known) {
        let identity = known.identity().clone();
        let replaced = match state.known.insert(path.clone(), known) {
            Some(old @ (Known::Open(_) | Known::Closed { .. })) => *old.identity() != identity,
            _ => false,
        };
        if replaced {
            state.content_version += 1;
            tracing::info!(
                "[{}] {} changed, read again",
                self.collection_id,
                path.display()
            );
        }
    }

    /// [`Self::open_inspect`] every file, `limits.concurrency` at a time on
    /// threads of their own (Critical Rule 9), in Background mode. `None`
    /// for a file not started before `limits.until`.
    fn open_parallel(
        &self,
        files: &[Listed],
        wanted: &Wanted,
        limits: &Limits,
    ) -> Vec<Option<Opened>> {
        let next = AtomicUsize::new(0);
        let results: Vec<Mutex<Option<Opened>>> = files.iter().map(|_| Mutex::new(None)).collect();
        let work = || {
            let _mode = io::enter(Mode::Background);
            let _deadline = deadline::enter(limits.until);
            while limits.until.is_none_or(|until| Instant::now() < until) {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(file) = files.get(i) else {
                    break;
                };
                let opened = self.open_inspect(file, wanted);
                *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(opened);
            }
        };
        let workers = limits.concurrency.min(files.len());
        if workers <= 1 {
            work();
        } else {
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    scope.spawn(work);
                }
            });
        }
        results
            .into_iter()
            .map(|slot| slot.into_inner().unwrap_or_else(|e| e.into_inner()))
            .collect()
    }

    /// Open `file` and read what the catalog needs from it, warming what a
    /// request's read uses. Touches no poll state, so files open in
    /// parallel.
    fn open_inspect(&self, file: &Listed, wanted: &Wanted) -> Opened {
        let id = next_file_id();
        let (nc, storage, remote) = match &self.source {
            Source::Local { .. } => match ingest::open(&file.path) {
                Ok((nc, storage)) => (nc, storage, None),
                Err(e) => return Opened::Skip(e),
            },
            Source::Remote(source) => {
                let remote = Arc::new(RemoteFile::new(
                    source.host.clone(),
                    &source.origin,
                    &file.path.to_string_lossy(),
                    file.identity.clone(),
                    id,
                    source.tuning,
                    self.changes.clone(),
                ));
                if let Err(e) = remote.prefetch() {
                    return Opened::Retry(e.to_string());
                }
                let storage: DynStorage = remote.clone();
                match ingest::open_storage(storage.clone()) {
                    Ok(nc) => (nc, storage, Some(remote)),
                    Err(e) => return verdict(&remote, e),
                }
            }
        };
        let inspected = match ingest::inspect(&nc, &storage, wanted, self.member) {
            Ok(inspected) => inspected,
            Err(e) => {
                return match &remote {
                    Some(remote) => verdict(remote, e),
                    None => Opened::Skip(e),
                }
            }
        };
        if inspected.times.is_empty() {
            return Opened::Skip("its time axis is empty".into());
        }
        // Warm what a request's read uses: the dataset lookup, and the chunk
        // index walked once without reading a chunk. A contiguous variable
        // or a classic file has no index.
        for field in &inspected.fields {
            if let Err(e) = nc.iter_variable_chunks(&field.read.name) {
                tracing::debug!(
                    "[{}] {}: no chunk index for '{}': {e}",
                    self.collection_id,
                    file.path.display(),
                    field.read.name
                );
            }
        }
        Opened::Ready(Box::new(Ready {
            id,
            nc,
            remote,
            inspected,
        }))
    }

    /// What the poll remembers of `file` once opened: an open file that
    /// fits the collection's layout (setting it when there is none yet), a
    /// skipped one, or `None` to try it again next poll. `cut`: the scan's
    /// time limit has passed, so a failed read is expected, and only the
    /// scan's summary logs it.
    fn admit(
        &self,
        file: &Listed,
        opened: Opened,
        state: &mut PollState,
        cut: bool,
    ) -> Option<Known> {
        let skipped = |reason: String| {
            tracing::warn!(
                "[{}] skipping {}: {reason}",
                self.collection_id,
                file.path.display()
            );
            Known::Skipped {
                identity: file.identity.clone(),
            }
        };
        match opened {
            Opened::Retry(reason) => {
                if !cut {
                    tracing::warn!(
                        "[{}] cannot read {} now (will retry next poll): {reason}",
                        self.collection_id,
                        file.path.display()
                    );
                }
                None
            }
            Opened::Skip(reason) => Some(skipped(reason)),
            Opened::Ready(ready) => {
                let Ready {
                    id,
                    nc,
                    remote,
                    inspected,
                } = *ready;
                if let Err(e) = self.fits_layout(&inspected, state) {
                    return Some(skipped(e));
                }
                Some(Known::Open(Arc::new(OpenFile {
                    id,
                    path: file.path.clone(),
                    identity: file.identity.clone(),
                    nc,
                    remote,
                    run: inspected.run,
                    times: inspected.times,
                    reads: inspected.fields.into_iter().map(|f| f.read).collect(),
                })))
            }
        }
    }

    /// Fetch the prewarm variables' fields of the newest run ahead of
    /// requests, into the compressed-chunk cache, two at a time (the
    /// background share), until `until`. Each field once per ingest.
    fn prewarm(&self, remote: &Remote, until: Instant) {
        if remote.prewarm.is_empty() {
            return;
        }
        let catalog = self.catalog.load_full();
        let (Some(layout), Some((_, run))) = (&catalog.layout, catalog.runs.last_key_value())
        else {
            return;
        };
        let indices: Vec<usize> = remote
            .prewarm
            .iter()
            .filter_map(|name| layout.variables.iter().position(|v| &*v.name == name))
            .collect();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let open: HashSet<u64> = catalog
            .runs
            .values()
            .flat_map(|run| run.steps.values().map(|step| step.file.id))
            .collect();
        state.prewarmed.retain(|(file, ..)| open.contains(file));
        let jobs: Vec<(&Step, usize)> = run
            .steps
            .values()
            .flat_map(|step| indices.iter().map(move |&index| (step, index)))
            .filter(|(step, index)| {
                !state
                    .prewarmed
                    .contains(&(step.file.id, *index, step.index))
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        let next = AtomicUsize::new(0);
        let done = Mutex::new(Vec::new());
        let stop = std::sync::atomic::AtomicBool::new(false);
        let flipped = layout.grid.flipped;
        let work = || {
            let _mode = io::enter(Mode::Background);
            while !stop.load(Ordering::Relaxed) && Instant::now() < until {
                let Some(&(step, index)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) else {
                    break;
                };
                // The level a map without ELEVATION draws (the bottom one
                // on a pressure axis): prewarming every level would be
                // 13 MEPS fields per step and variable.
                match field::prewarm(&step.file, index, step.index, 0, flipped) {
                    Ok(_) => done.lock().unwrap_or_else(|e| e.into_inner()).push((
                        step.file.id,
                        index,
                        step.index,
                    )),
                    Err(e) => {
                        // Requests come first: stop at the first failure,
                        // budget or network, and go on next poll.
                        tracing::debug!("[{}] prewarm stopped: {e}", self.collection_id);
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        };
        std::thread::scope(|scope| {
            for _ in 0..remote::BACKGROUND_SHARE.min(jobs.len()) {
                scope.spawn(work);
            }
        });
        let done = done.into_inner().unwrap_or_else(|e| e.into_inner());
        tracing::debug!(
            "[{}] prewarmed {} of {} fields",
            self.collection_id,
            done.len(),
            jobs.len()
        );
        state.prewarmed.extend(done);
    }

    /// Check `inspected` against the collection's layout, or make it the
    /// layout when there is none yet.
    fn fits_layout(&self, inspected: &Inspected, state: &mut PollState) -> Result<(), String> {
        let Some(layout) = &state.layout else {
            for (name, reason) in &inspected.skipped {
                tracing::warn!(
                    "[{}] not serving variable '{name}': it {reason}",
                    self.collection_id
                );
            }
            if inspected.fields.is_empty() {
                return Err("has no servable variable".into());
            }
            for field in &inspected.fields {
                let read = &field.read;
                if read.chunk_bytes > read.field_bytes {
                    tracing::warn!(
                        "[{}] variable '{}' is chunked across {} fields: each field read \
                         inflates them all",
                        self.collection_id,
                        read.name,
                        read.chunk_bytes / read.field_bytes.max(1)
                    );
                }
            }
            let t = &inspected.grid.transform;
            tracing::info!(
                "[{}] serving {} variables on a {} x {} {} grid{}",
                self.collection_id,
                inspected.fields.len(),
                t.width,
                t.height,
                match t.crs {
                    ds_core::geo::Crs::Wgs84 => "latitude/longitude",
                    ds_core::geo::Crs::LambertConformalConic { .. } => "Lambert conformal conic",
                    ds_core::geo::Crs::Geostationary { .. } => "geostationary",
                    ds_core::geo::Crs::Stereographic { .. } => "polar stereographic",
                    ds_core::geo::Crs::LambertAzimuthalEqualArea { .. } => {
                        "Lambert azimuthal equal-area"
                    }
                    ds_core::geo::Crs::TransverseMercator { .. } => "transverse Mercator",
                    _ => "projected",
                },
                if inspected.grid.flipped {
                    " (rows stored south to north)"
                } else {
                    ""
                }
            );
            state.layout = Some(Arc::new(Layout::new(
                inspected
                    .fields
                    .iter()
                    .map(|f| f.variable.clone())
                    .collect(),
                inspected.grid.clone(),
            )));
            return Ok(());
        };
        if !layout.grid.same(&inspected.grid) {
            return Err("is on another grid than the collection's".into());
        }
        for (served, field) in layout.variables.iter().zip(&inspected.fields) {
            let found = &field.variable;
            if !served.same_quantity(found) {
                let pressure = |levels: &Option<Vec<f64>>| match levels {
                    Some(levels) => format!(" on {} hPa", ingest::list_levels(levels)),
                    None => String::new(),
                };
                return Err(format!(
                    "variable '{}' has units '{}' at levels {:?}{}, the collection's has '{}' at \
                     {:?}{}",
                    served.name,
                    found.units,
                    found.levels,
                    pressure(&found.pressure),
                    served.units,
                    served.levels,
                    pressure(&served.pressure)
                ));
            }
        }
        Ok(())
    }

    /// The ingested files the catalog keeps: the newest `max_runs` runs, as
    /// many of them, newest first, as `max_files` holds; without runs, the
    /// `max_files` files with the newest times. A directory mixing files
    /// with and without a run keeps the runs.
    fn retained(&self, state: &mut PollState) -> HashSet<PathBuf> {
        // Per run, each file's newest valid time and path.
        let mut groups: BTreeMap<RunKey, Vec<(DateTime<Utc>, &PathBuf)>> = BTreeMap::new();
        for (path, known) in &state.known {
            if let Some((run, newest)) = known.place() {
                groups.entry(run).or_default().push((newest, path));
            }
        }
        let has_runs = groups.keys().any(Option::is_some);
        if has_runs && groups.remove(&None).is_some() && !state.warned_mixed {
            state.warned_mixed = true;
            tracing::warn!(
                "[{}] the directory mixes files with and without forecast_reference_time; \
                 serving only those with one",
                self.collection_id
            );
        }
        let mut keep = HashSet::new();
        let runs = groups
            .into_values()
            .rev()
            .take(if has_runs { self.max_runs } else { 1 });
        for mut files in runs {
            let room = self.max_files - keep.len();
            if room == 0 {
                break;
            }
            if files.len() > room {
                if !keep.is_empty() {
                    break;
                }
                // The newest run alone: its earliest times, or without
                // runs the newest files.
                if has_runs {
                    files.sort();
                } else {
                    files.sort_by(|a, b| b.cmp(a));
                }
                tracing::warn!(
                    "[{}] keeping {room} of {} files: max_files",
                    self.collection_id,
                    files.len()
                );
                files.truncate(room);
            }
            keep.extend(files.into_iter().map(|(_, path)| path.clone()));
        }
        keep
    }

    /// The catalog of the retained open files.
    fn build_catalog(&self, state: &PollState, keep: &HashSet<PathBuf>) -> Catalog {
        let mut runs: BTreeMap<RunKey, Run> = BTreeMap::new();
        // Of two files with a step at one valid time in one run, the greater
        // path wins, whatever the listing order.
        let mut files: Vec<&Arc<OpenFile>> = state
            .known
            .iter()
            .filter(|(path, _)| keep.contains(*path))
            .filter_map(|(_, known)| match known {
                Known::Open(file) => Some(file),
                _ => None,
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        for file in files {
            let run = runs.entry(file.run).or_insert_with(|| Run {
                reference_time: file.run,
                steps: BTreeMap::new(),
            });
            for (index, &valid) in file.times.iter().enumerate() {
                run.steps.insert(
                    valid,
                    Step {
                        file: file.clone(),
                        index: index as u32,
                    },
                );
            }
        }
        let layout = state.layout.clone();
        let info = catalog::raster_info(layout.as_deref(), &runs, &self.titles);
        Catalog {
            layout,
            runs,
            info: Arc::new(info),
            content_version: state.content_version,
            polled_at: Some(Utc::now()),
        }
    }

    /// The layout index of `parameter`, the first variable for `None`.
    fn variable_index(layout: &Layout, parameter: Option<&str>) -> Result<usize, DataServerError> {
        match parameter {
            None => Ok(0),
            Some(name) => layout
                .variables
                .iter()
                .position(|v| &*v.name == name)
                .ok_or_else(|| {
                    DataServerError::InvalidParameter(format!(
                        "Parameter '{name}' is unavailable in this collection"
                    ))
                }),
        }
    }

    /// The decoded field of variable `index` at `step`, served level
    /// `level` ([`Layout::map_level`]).
    fn field(
        &self,
        layout: &Layout,
        step: &Step,
        index: usize,
        level: usize,
    ) -> Result<field::Values, DataServerError> {
        field::read(
            &step.file,
            index,
            step.index,
            level,
            layout.grid.flipped,
            &self.decodes,
        )
    }
}

impl MapEngine for NetcdfEngine {
    #[allow(clippy::too_many_arguments)] // bbox/size/time/crs/parameter/z/reference_time are all genuine selectors
    fn get_raster_tile(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameter: Option<&str>,
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<RasterTile, DataServerError> {
        let catalog = self.catalog.load_full();
        let selected = catalog.select(reference_time, time)?;
        let layout = catalog
            .layout
            .as_ref()
            .ok_or_else(|| DataServerError::Engine("No NetCDF data available yet".to_string()))?;
        let index = Self::variable_index(layout, parameter)?;
        let level = layout.map_level(z)?;
        let values = self.field(layout, selected.step, index, level)?;
        layout
            .source
            .render_f32(&values, None, bbox, width, height, output_crs)
    }

    /// Every band from the one step `select` picks and the one level
    /// `map_level` picks, sampled from one output→source map: the variables
    /// share the grid.
    #[allow(clippy::too_many_arguments)] // mirrors get_raster_tile
    fn get_raster_tiles(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        time: Option<DateTime<Utc>>,
        output_crs: &OutputCrs,
        parameters: &[&str],
        z: Option<f64>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Result<Vec<RasterTile>, DataServerError> {
        if parameters.is_empty() {
            return Ok(Vec::new());
        }
        let catalog = self.catalog.load_full();
        let selected = catalog.select(reference_time, time)?;
        let layout = catalog
            .layout
            .as_ref()
            .ok_or_else(|| DataServerError::Engine("No NetCDF data available yet".to_string()))?;
        let indices = parameters
            .iter()
            .map(|&p| Self::variable_index(layout, Some(p)))
            .collect::<Result<Vec<_>, _>>()?;
        let level = layout.map_level(z)?;
        let map = layout.source.output_map(bbox, width, height, output_crs);
        indices
            .into_iter()
            .map(|index| map.sample_f32(&self.field(layout, selected.step, index, level)?, None))
            .collect()
    }

    fn raster_info(&self) -> RasterInfo {
        (*self.raster_info_shared()).clone()
    }

    fn raster_info_shared(&self) -> Arc<RasterInfo> {
        self.catalog.load().info.clone()
    }

    /// The valid time `get_raster_tile` draws, through the same
    /// [`Catalog::select`]; a failed selection echoes the request, since
    /// the render then fails and caches nothing.
    fn resolve_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        match self.catalog.load().select(reference_time, time) {
            Ok(selected) => Some(selected.valid),
            Err(_) => time,
        }
    }

    /// The run `get_raster_tile` draws from, through the same
    /// [`Catalog::select`], an older run's included when the newest does
    /// not cover the time; a failed selection echoes the request.
    fn resolve_reference_time(
        &self,
        time: Option<DateTime<Utc>>,
        reference_time: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        match self.catalog.load().select(reference_time, time) {
            Ok(selected) => selected.reference_time,
            Err(_) => reference_time,
        }
    }

    /// Moves when a catalogued file is replaced (#1067 decision 7): found
    /// by a poll, or by a remote read at once. The rendered caches key on
    /// it.
    fn content_version(&self) -> u64 {
        self.catalog.load().content_version + self.changes.bumps()
    }
}

/// The layout's wind facts for `ds_core::wind::DerivedWind` (#897): an `Arc`
/// built once with the layout, so the plan is built once per engine.
impl ds_core::wind::WindSource for NetcdfEngine {
    fn wind_facts(&self) -> Arc<ds_core::wind::WindFacts> {
        self.catalog
            .load()
            .layout
            .as_ref()
            .map_or_else(ds_core::wind::WindFacts::none, |layout| layout.wind.clone())
    }
}

/// Whether the scan's time limit has passed.
fn past(limits: &Limits) -> bool {
    limits.until.is_some_and(|until| Instant::now() >= until)
}

/// Why a remote file failed to open: a failed read is tried again next
/// poll, anything else is the file.
fn verdict(remote: &RemoteFile, reason: String) -> Opened {
    if remote.read_failed() {
        Opened::Retry(reason)
    } else {
        Opened::Skip(reason)
    }
}

impl Drop for NetcdfEngine {
    fn drop(&mut self) {
        self.shutdown.shutdown();
    }
}

impl std::fmt::Debug for NetcdfEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match &self.source {
            Source::Local { dir, .. } => dir.display().to_string(),
            Source::Remote(remote) => remote.origin.clone(),
        };
        f.debug_struct("NetcdfEngine")
            .field("collection_id", &self.collection_id)
            .field("source", &source)
            .finish_non_exhaustive()
    }
}
