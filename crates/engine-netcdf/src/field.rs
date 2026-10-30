//! The engine-wide cache of decoded fields, and the one place a request
//! reads a file.
//!
//! Every field read goes through [`FIELDS`], keyed by the file's ingest,
//! the variable, the time step and the pressure level. Its single-flight guard makes N
//! concurrent renders of one cold field decode it once: the first reader
//! owns the fill and reserves the transient peak from the decode budget,
//! the others wait for its value and reserve nothing, and a hit reserves
//! nothing either. The wait and the fill run through [`io::blocking`]
//! (`block_in_place` on a Tokio worker, root CLAUDE.md Critical Rule 7), so
//! a waiting async worker does not hold up the runtime.
//!
//! A remote file's cold field, read by HTTP range or through OPeNDAP, is
//! filled **detached** (#1067): the request
//! claims the fill, hands the claim to a task on the I/O runtime that reads
//! under [`Mode::Detached`] (no request deadline,
//! [`ds_storage::FetchBudget::Background`]), and waits for it only until its
//! own deadline. A fetch that outlives one request's 3 s still fills the
//! cache for the next. At most [`MAX_DETACHED_FILLS`] run at once; past
//! that a fill runs inline, under the request's deadline. An EDR or
//! GetFeatureInfo read ([`read_uncached`]) detaches from a pool of its own,
//! [`MAX_QUERY_DETACHED_READS`], so query load never takes the slots render
//! fills need to leave a warm cache behind a missed 3 s deadline. A query
//! read of a field another query's detached read is still reading joins
//! that read ([`QUERY_IN_FLIGHT`]) rather than taking a slot of its own, so
//! retries of a query that missed its deadline never fill the pool.
//!
//! A query read through OPeNDAP may read a window of its field rather than
//! the whole (#1171): the cells its positions sample
//! (`ds_core::resample::SourceGrid::cells_read`), sampled from the window
//! alone ([`QueryField::value_at`]) to the value the whole field gives. The
//! window of several pressure levels of one field at one step is one
//! hyperslab over the stored levels from the lowest to the highest asked
//! ([`crate::ingest::Span`]), decoded to a plane per level; and the windows
//! of several variables at one step are one read (#1274): one request of a
//! hyperslab per variable, one reply, decoded into each variable's planes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use ds_cache::{ByteBoundedCache, CacheEntry};
use ds_core::deadline;
use ds_core::error::DataServerError;
use ds_core::model::ValuePrecision;
use ds_core::resample::{CellWindow, SourceGrid};

use crate::catalog::OpenFile;
use crate::decode_budget::BUDGET;
use crate::ingest::{FieldRead, Span};
use crate::io::{self, Mode};
use crate::source::Reader;

/// A decoded 2-D field: rows north to south, NaN for nodata.
pub(crate) type Values = Arc<[f32]>;

/// The precision of every EDR range (#1341): each value is sampled from
/// decoded [`Values`], which are `f32` whatever the stored type (CF
/// unpacking decodes to `f32`). Interpolation and display conversion add no
/// digits.
pub(crate) const VALUE_PRECISION: ValuePrecision = ValuePrecision::F32;

/// One variable's decoded fields of a read: a plane per stored level of its
/// [`Span`], in stored order; one for a field with one level.
type Planes = Vec<Values>;

/// One variable of a query read ([`read_uncached`]): variable `index` at
/// served levels `levels`, whole or over `window` (#1171).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Member<'a> {
    pub index: usize,
    pub levels: &'a [usize],
    pub window: Option<CellWindow>,
}

/// What one request reads of one variable: its index, the stored levels of
/// its hyperslab, and its window (`None`: the whole field).
type Part = (usize, Span, Option<CellWindow>);

/// A field an EDR or GetFeatureInfo query samples ([`read_uncached`]):
/// the whole field, or the window of it the query read (#1171).
pub(crate) struct QueryField {
    /// North-up, NaN for nodata: the field's cells, or the window's.
    pub values: Values,
    /// The window `values` hold; `None` for the whole field.
    pub window: Option<CellWindow>,
}

impl QueryField {
    /// The value at `position` on `source` (`SourceGrid::position`), as
    /// `SourceGrid::value_at` samples the whole field: from a window, by
    /// `SourceGrid::value_in`, which reads the same cells in the same
    /// arithmetic.
    pub(crate) fn value_at(&self, source: &SourceGrid, position: (f64, f64)) -> Option<f64> {
        match self.window {
            None => source.value_at(&self.values, None, position),
            Some(window) => source.value_in(window, &self.values, None, position),
        }
    }
}

/// What identifies a decoded field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FieldKey {
    /// The ingest that opened the file ([`crate::catalog::OpenFile::id`]),
    /// unique in the process: a file replaced on disk is a new ingest, and
    /// never reads the fields of the old one.
    pub file: u64,
    pub variable: Arc<str>,
    pub time: u32,
    /// The served level: an index into the layout's pressure levels
    /// (#1067 PR 8), 0 for a field with one level. One level is one field:
    /// it is fetched, decoded, cached and budgeted alone.
    pub level: u32,
}

/// A MEPS field (949 × 1069 `f32`) is 4.06 MB.
const TYPICAL_FIELD_BYTES: u64 = 4 * ds_cache::MIB;

fn weigh(key: &FieldKey, values: &Values) -> u64 {
    field_weight(values.len() as u64, &key.variable)
}

/// The weight of a decoded field of `cells` values of `variable`: its
/// values, the key's name and the entry's overhead.
fn field_weight(cells: u64, variable: &str) -> u64 {
    cells * std::mem::size_of::<f32>() as u64 + variable.len() as u64 + 96
}

/// How the field cache's demand is computed (#1309), for every collection.
const FIELD_DEMAND_RULE: &str = "16 × decoded field bytes, summed over collections";

/// What a collection of `catalog` needs of the field cache (#1309):
/// [`ds_cache::plan::DEMAND_FIELDS`] fields of its grid, each weighed as
/// the cache weighs it under its longest variable name. `None` before the
/// first file gives the grid.
pub(crate) fn field_demand(
    catalog: &crate::catalog::Catalog,
) -> Option<ds_cache::plan::CacheDemand> {
    use ds_cache::plan::{CacheDemand, DEMAND_FIELDS};
    let layout = catalog.layout.as_ref()?;
    let (width, height) = layout.grid.size();
    let cells = u64::from(width) * u64::from(height);
    let name = layout
        .variables
        .iter()
        .map(|v| &*v.name)
        .max_by_key(|name| name.len())
        .unwrap_or_default();
    let field = field_weight(cells, name);
    Some(
        CacheDemand::demand(
            DEMAND_FIELDS * field,
            FIELD_DEMAND_RULE,
            "MC_NETCDF_FIELD_CACHE_MB",
        )
        .input("fields", DEMAND_FIELDS)
        .input("cells", cells)
        .input("field_bytes", field),
    )
}

/// Decoded fields, `MC_NETCDF_FIELD_CACHE_MB` (default 256 MiB), shared by
/// every NetCDF collection.
static FIELDS: LazyLock<ByteBoundedCache<FieldKey, Values>> = LazyLock::new(|| {
    let (bytes, source) = ds_cache::plan::env_bytes("MC_NETCDF_FIELD_CACHE_MB", 256);
    ByteBoundedCache::new(bytes, TYPICAL_FIELD_BYTES, weigh).registered(
        "netcdf_field_cache",
        ds_cache::plan::Scope::Engine("netcdf"),
        source,
    )
});

/// The field cache's counters and occupancy.
pub fn field_cache_metrics() -> ds_cache::CacheMetrics {
    FIELDS.metrics()
}

/// Detached fills running at once, over every collection: twice the
/// requests a host takes at once ([`crate::remote::HOST_REQUEST_LIMIT`]),
/// so the next frames' fetches queue behind the current ones without
/// piling up work no request still waits for.
pub(crate) const MAX_DETACHED_FILLS: usize = 16;

/// Detached EDR and GetFeatureInfo reads running at once, over every
/// collection: a pool apart from the render fills', half their size. One
/// cold series runs four reads at once (`ds_executor::field_jobs`), each for
/// as long as a 1.5 MB fetch takes, so a few concurrent series would
/// otherwise hold every render slot, and the renders' cold fills would run
/// inline, under their 3 s deadline, where a fetch cut off caches nothing.
pub(crate) const MAX_QUERY_DETACHED_READS: usize = MAX_DETACHED_FILLS / 2;

/// A cap on reads running detached from the request that started them, and
/// its counters.
pub(crate) struct DetachedPool {
    cap: usize,
    pending: AtomicUsize,
    total: AtomicU64,
    overflow: AtomicU64,
}

impl DetachedPool {
    const fn new(cap: usize) -> Self {
        Self {
            cap,
            pending: AtomicUsize::new(0),
            total: AtomicU64::new(0),
            overflow: AtomicU64::new(0),
        }
    }

    /// A slot for a detached read, unless `cap` are running: then `None`,
    /// counted as a read run inline.
    fn claim(&'static self) -> Option<Detached> {
        let claimed = self
            .pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.cap).then_some(n + 1)
            })
            .is_ok();
        if !claimed {
            self.overflow.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.total.fetch_add(1, Ordering::Relaxed);
        Some(Detached(self))
    }

    /// `(running now, started in total, run inline past the cap)`.
    pub(crate) fn metrics(&self) -> (usize, u64, u64) {
        (
            self.pending.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
            self.overflow.load(Ordering::Relaxed),
        )
    }
}

/// The render fills' pool ([`MAX_DETACHED_FILLS`]).
static RENDER_FILLS: DetachedPool = DetachedPool::new(MAX_DETACHED_FILLS);

/// The EDR and GetFeatureInfo reads' pool ([`MAX_QUERY_DETACHED_READS`]).
pub(crate) static QUERY_READS: DetachedPool = DetachedPool::new(MAX_QUERY_DETACHED_READS);

/// Detached query reads in flight, by field: a query read of a field in
/// here waits for that read instead of starting its own.
///
/// Without it, every retry of a query that missed its deadline started a
/// detached read of each field the first attempt was still reading. Each
/// waited on the first read's chunk (one GET, `crate::remote`'s
/// single-flight) while holding a [`QUERY_READS`] slot. One cold query and
/// three retries held 8 slots, the whole pool, for 2 fields. The next
/// cold field then ran inline under its query's deadline, which cut its GET
/// off: nothing cached, and the next retry fetched it again.
static QUERY_IN_FLIGHT: LazyLock<Mutex<HashMap<QueryKey, Arc<InFlight>>>> =
    LazyLock::new(Mutex::default);

/// What a query read reads: fields at one step, each variable over the
/// stored levels of a span, whole (one level) or a window of it (#1171),
/// one variable or the windows of several in one request (#1274). A read is
/// joined only by a read of the same variables, spans and windows in the
/// same order, the planned order (ascending variable index): what one
/// `.dods` constraint selects. A read of a subset of them is a read of its
/// own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QueryKey {
    /// The ingest ([`FieldKey::file`]).
    file: u64,
    time: u32,
    /// Each variable read: its name, span and window.
    parts: Vec<(Arc<str>, Span, Option<CellWindow>)>,
}

/// One detached query read, for the queries that joined it.
#[derive(Default)]
struct InFlight {
    /// `Some(Some(planes))` once read, each variable's planes in the
    /// read's order; `Some(None)` once failed.
    outcome: Mutex<Option<Option<Vec<Planes>>>>,
    done: Condvar,
}

impl InFlight {
    /// The read's planes, `Ok(None)` if it failed, or `Err(())` if it is
    /// still running at `end` (30 s without one).
    fn wait(&self, end: Option<Instant>) -> Result<Option<Vec<Planes>>, ()> {
        let wait = end.map_or(Duration::from_secs(30), |end| {
            end.saturating_duration_since(Instant::now())
        });
        let outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        let (outcome, _) = self
            .done
            .wait_timeout_while(outcome, wait, |outcome| outcome.is_none())
            .unwrap_or_else(|e| e.into_inner());
        outcome.clone().ok_or(())
    }
}

/// Settles a detached query read when dropped, a panic included: records
/// its outcome (a failure if none was recorded), wakes the queries that
/// joined it, and takes it out of [`QUERY_IN_FLIGHT`], so a later read of
/// the field starts afresh (its chunk then cached).
struct Settle {
    key: QueryKey,
    flight: Arc<InFlight>,
    values: Option<Vec<Planes>>,
}

impl Drop for Settle {
    fn drop(&mut self) {
        let mut in_flight = QUERY_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if in_flight
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            in_flight.remove(&self.key);
        }
        drop(in_flight);
        *self
            .flight
            .outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(self.values.take());
        self.flight.done.notify_all();
    }
}

/// How a query reads a remote field: joining another query's detached read
/// of it, starting one (with its pool slot), or inline when the pool is
/// full.
enum QueryRead {
    Join(Arc<InFlight>),
    Start(Arc<InFlight>, Detached),
    Inline,
}

/// The render fills' detached reads: `(running now, started in total,
/// filled inline because MAX_DETACHED_FILLS were running)`. Query reads
/// ([`QUERY_READS`]) are not counted here.
pub fn detached_fill_metrics() -> (usize, u64, u64) {
    RENDER_FILLS.metrics()
}

/// One running detached read, counted in its pool until dropped.
struct Detached(&'static DetachedPool);

impl Drop for Detached {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Drop the decoded fields of the ingest `file`: its file was found
/// replaced.
pub(crate) fn evict_file(file: u64) {
    FIELDS.retain(|key, _| key.file != file);
}

/// The transient peak of decoding `read`, which the fill reserves: three
/// times the decoded bytes (the stored-type result, the inflated chunk and
/// its unshuffled copy) and three times the compressed chunk. hdf5-reader
/// exposes no chunk's stored size before reading it, so the decoded chunk
/// size bounds the compressed one: about 24 MB for a MEPS field.
fn peak_bytes(read: &FieldRead) -> u64 {
    let decoded = read.field_bytes.max(read.chunk_bytes);
    decoded
        .saturating_mul(3)
        .saturating_add(read.chunk_bytes.saturating_mul(3))
}

/// The transient peak of reading `window` of `read` (#1171): the whole
/// field's ([`peak_bytes`]) in proportion to the cells read, at least one
/// byte. A window read through OPeNDAP holds its reply, its values in their
/// stored type and decoded, as a whole field's read does.
fn window_peak_bytes(read: &FieldRead, window: CellWindow) -> u64 {
    let (ny, nx) = read.shape();
    let cells = (ny as u64).saturating_mul(nx as u64).max(1);
    peak_bytes(read)
        .saturating_mul(window.cells())
        .div_ceil(cells)
        .max(1)
}

/// The transient peak of one read of `span`'s stored levels of `read`,
/// whole or over `window`, which the read reserves: each level's
/// ([`peak_bytes`], [`window_peak_bytes`]), so a window over 13 levels
/// reserves 13 windows' peaks, its reply and planes included.
fn read_peak_bytes(read: &FieldRead, span: Span, window: Option<CellWindow>) -> u64 {
    window
        .map_or_else(|| peak_bytes(read), |w| window_peak_bytes(read, w))
        .saturating_mul(span.levels())
}

/// Variable `index` of `file` at time step `time` and served level `level`,
/// from the cache or decoded once for every concurrent reader. `flipped`
/// reverses the stored rows to north-up; `decodes` counts the fills this
/// call makes.
pub(crate) fn read(
    file: &Arc<OpenFile>,
    index: usize,
    time: u32,
    level: usize,
    flipped: bool,
    decodes: &Arc<AtomicU64>,
) -> Result<Values, DataServerError> {
    deadline::check()?;
    let key = field_key(file, index, time, level);
    if let Some(values) = FIELDS.get_untracked(&key) {
        FIELDS.record_hit();
        return Ok(values);
    }
    let fill = Fill {
        file,
        index,
        time,
        level,
        flipped,
        decodes,
        end: deadline::current(),
    };
    // A waiting async worker must not hold up the runtime the owner may
    // need. On RenderJob's blocking threads `block_in_place` runs the
    // closure as is.
    let result = io::blocking(|| {
        if file.reader.is_remote() {
            fill.detached(&key)
        } else {
            fill.inline(&key)
        }
    });
    // A fill can complete after this caller's deadline passed: the value
    // stays cached for the next request.
    deadline::check()?;
    result
}

/// The fields an EDR query reads of `file` at step `time` (#1067): for each
/// of `members`, variable `index` at its served levels, one [`QueryField`]
/// per level in `levels`' order. A level is the decoded field when the
/// cache holds it (a frame being drawn), else decoded for this query alone
/// and not cached, so a series over a whole run never evicts the frames
/// being drawn. A remote file's compressed chunk still lands in the chunk
/// cache ([`crate::remote`]), so the same field read again costs a decode,
/// not a fetch. The lookup is not counted in the field cache's hits and
/// misses, which measure the renders it serves. Without a window, a
/// member's levels are read one by one, each its whole field.
///
/// With a window (#1171, a file read through OPeNDAP), only those cells are
/// read, of every level of the member at once: the hyperslab over the
/// stored levels from the lowest of them to the highest ([`FieldRead::span`]:
/// the levels between, at window size, cost next to nothing). The windows
/// of every member are one request (#1274): one `.dods` whose constraint
/// holds each member's hyperslab, kept in the chunk cache under that
/// constraint, decoded into each member's planes. A member whose every
/// level is held whole, in the field cache or as a whole reply in the
/// chunk cache (a render's, a prewarm's, another query's), is left out of
/// the request and decoded from those instead. The result says which it
/// holds ([`QueryField::window`]).
///
/// A remote read is detached, as a render's fill is: it runs on the I/O
/// runtime under [`Mode::Detached`] (no deadline, the background fetch
/// budget), and the query waits for it only until its own deadline. A
/// query that misses its deadline fails, while the reads it started go on
/// into the chunk cache, so a retry makes progress. A query making a read
/// (the same variables' fields or windows over the same levels) another
/// query's detached read is still making joins it ([`QUERY_IN_FLIGHT`]): no
/// slot, no decode of its own; if that read fails, the joiner reads it
/// itself, under its deadline, for its own error. The slot comes from
/// [`QUERY_READS`], never the render fills' pool; past
/// [`MAX_QUERY_DETACHED_READS`] the read runs here, under the deadline.
/// Each read reserves its transient peak from the decode budget, every
/// level of every variable it holds.
pub(crate) fn read_uncached(
    file: &Arc<OpenFile>,
    time: u32,
    members: &[Member],
    flipped: bool,
    decodes: &Arc<AtomicU64>,
) -> Result<Vec<Vec<QueryField>>, DataServerError> {
    deadline::check()?;
    let held: Vec<Vec<Option<Values>>> = members
        .iter()
        .map(|member| {
            member
                .levels
                .iter()
                .map(|&level| FIELDS.get_untracked(&field_key(file, member.index, time, level)))
                .collect()
        })
        .collect();
    // Every level held whole costs a decode at most; a window, a fetch.
    let windows: Vec<Option<(Span, CellWindow)>> = members
        .iter()
        .zip(&held)
        .map(|(member, held)| {
            let read = &file.reads[member.index];
            let all_held =
                member.levels.iter().zip(held).all(|(&level, held)| {
                    held.is_some() || file.reader.holds_field(read, time, level)
                });
            member
                .window
                .filter(|_| !all_held)
                .map(|window| (read.span(member.levels), window))
        })
        .collect();
    let parts: Vec<Part> = members
        .iter()
        .zip(&windows)
        .filter_map(|(member, window)| window.map(|(span, w)| (member.index, span, Some(w))))
        .collect();
    let mut fetched = if parts.is_empty() {
        Vec::new()
    } else {
        read_parts(file, time, &parts, flipped, decodes)?
    }
    .into_iter();
    members
        .iter()
        .zip(held)
        .zip(windows)
        .map(|((member, held), window)| {
            let read = &file.reads[member.index];
            let Some((span, window)) = window else {
                return member
                    .levels
                    .iter()
                    .zip(held)
                    .map(|(&level, held)| {
                        let values = match held {
                            Some(values) => values,
                            None => {
                                let part = (member.index, read.span(&[level]), None);
                                one(one(read_parts(file, time, &[part], flipped, decodes)?)?)?
                            }
                        };
                        Ok(QueryField {
                            values,
                            window: None,
                        })
                    })
                    .collect();
            };
            let planes = fetched.next().ok_or_else(|| {
                DataServerError::Engine("a NetCDF query read decoded too few variables".into())
            })?;
            member
                .levels
                .iter()
                .map(|&level| {
                    let values = planes
                        .get(span.plane(read.stored_level(level)))
                        .cloned()
                        .ok_or_else(|| {
                            DataServerError::Engine(format!(
                                "variable '{}': level {level} is not in the levels read",
                                read.name
                            ))
                        })?;
                    Ok(QueryField {
                        values,
                        window: Some(window),
                    })
                })
                .collect()
        })
        .collect()
}

/// One query read of [`read_uncached`]: `parts` of `file` at step `time`,
/// each a variable's stored levels whole or over a window, in one request
/// through OPeNDAP, the planes of each part in `parts`' order; detached and
/// joined as [`read_uncached`] says for a remote file, here for a local
/// one.
fn read_parts(
    file: &Arc<OpenFile>,
    time: u32,
    parts: &[Part],
    flipped: bool,
    decodes: &Arc<AtomicU64>,
) -> Result<Vec<Planes>, DataServerError> {
    let key = QueryKey {
        file: file.id,
        time,
        parts: parts
            .iter()
            .map(|&(index, span, window)| (file.reads[index].name.clone(), span, window))
            .collect(),
    };
    let end = deadline::current();
    let timed_out = || {
        if end.is_some() {
            DataServerError::DeadlineExceeded
        } else {
            DataServerError::Storage("timed out waiting for a NetCDF field read".into())
        }
    };
    let result = io::blocking(|| {
        if !file.reader.is_remote() {
            return fill_parts(file, time, parts, flipped, decodes);
        }
        let read = {
            let mut in_flight = QUERY_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            match in_flight.get(&key) {
                Some(flight) => QueryRead::Join(flight.clone()),
                None => match QUERY_READS.claim() {
                    Some(slot) => {
                        let flight = Arc::new(InFlight::default());
                        in_flight.insert(key.clone(), flight.clone());
                        QueryRead::Start(flight, slot)
                    }
                    None => QueryRead::Inline,
                },
            }
        };
        let (flight, slot) = match read {
            QueryRead::Inline => return fill_parts(file, time, parts, flipped, decodes),
            QueryRead::Join(flight) => {
                return match flight.wait(end) {
                    Ok(Some(planes)) => Ok(planes),
                    // The read joined failed: read here, for this query's
                    // own error, or values if the failure has passed.
                    Ok(None) => fill_parts(file, time, parts, flipped, decodes),
                    Err(()) => Err(timed_out()),
                };
            }
            QueryRead::Start(flight, slot) => (flight, slot),
        };
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let (file, decodes, parts) = (file.clone(), decodes.clone(), parts.to_vec());
        let key = key.clone();
        io::handle().spawn_blocking(move || {
            let _slot = slot;
            let _mode = io::enter(Mode::Detached);
            let mut settle = Settle {
                key,
                flight,
                values: None,
            };
            let result = fill_parts(&file, time, &parts, flipped, &decodes);
            settle.values = result.as_ref().ok().cloned();
            drop(settle);
            // The query may have given up: the reply is cached anyway.
            let _ = sender.send(result);
        });
        let wait = end.map_or(Duration::from_secs(30), |end| {
            end.saturating_duration_since(Instant::now())
        });
        match receiver.recv_timeout(wait) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(timed_out()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(DataServerError::Engine(
                "a detached NetCDF field read stopped".into(),
            )),
        }
    });
    deadline::check()?;
    result
}

/// The one item of a read of one variable or one level.
fn one<T>(read: Vec<T>) -> Result<T, DataServerError> {
    read.into_iter()
        .next()
        .ok_or_else(|| DataServerError::Engine("a NetCDF field read decoded nothing".into()))
}

/// The cache key of variable `index` of `file` at step `time`, level
/// `level`.
fn field_key(file: &OpenFile, index: usize, time: u32, level: usize) -> FieldKey {
    FieldKey {
        file: file.id,
        variable: file.reads[index].name.clone(),
        time,
        level: level as u32,
    }
}

/// One reader's fill of one field.
struct Fill<'a> {
    file: &'a Arc<OpenFile>,
    index: usize,
    time: u32,
    level: usize,
    flipped: bool,
    decodes: &'a Arc<AtomicU64>,
    /// The reader's deadline.
    end: Option<Instant>,
}

impl Fill<'_> {
    /// How long the reader waits for another's fill: until its deadline,
    /// 30 s without one.
    fn wait(&self) -> Duration {
        self.end.map_or(Duration::from_secs(30), |end| {
            end.saturating_duration_since(Instant::now())
        })
    }

    fn timed_out(&self) -> DataServerError {
        if self.end.is_some() {
            DataServerError::DeadlineExceeded
        } else {
            DataServerError::Storage("timed out waiting for a NetCDF field read".into())
        }
    }

    /// Fill in this thread, or wait for the reader that does.
    fn inline(&self, key: &FieldKey) -> Result<Values, DataServerError> {
        FIELDS.get_or_insert_with_timeout(
            key,
            self.wait(),
            || {
                // A failed owner hands the fill to a waiter: recheck that
                // caller's deadline before reading.
                deadline::check()?;
                fill(
                    self.file,
                    self.index,
                    self.time,
                    self.level,
                    self.flipped,
                    self.decodes,
                )
            },
            || self.timed_out(),
        )
    }

    /// Claim the fill and detach it to the I/O runtime, or wait for the
    /// reader that has; inline past [`MAX_DETACHED_FILLS`].
    fn detached(&self, key: &FieldKey) -> Result<Values, DataServerError> {
        match FIELDS.get_value_or_guard_untracked(key, Duration::ZERO) {
            CacheEntry::Value(values) => {
                FIELDS.record_hit();
                return Ok(values);
            }
            CacheEntry::Vacant(guard) => {
                FIELDS.record_miss();
                let Some(slot) = RENDER_FILLS.claim() else {
                    let values = fill(
                        self.file,
                        self.index,
                        self.time,
                        self.level,
                        self.flipped,
                        self.decodes,
                    )?;
                    let _ = guard.insert(values.clone());
                    return Ok(values);
                };
                let (file, decodes) = (self.file.clone(), self.decodes.clone());
                let (index, time, level, flipped) =
                    (self.index, self.time, self.level, self.flipped);
                io::handle().spawn_blocking(move || {
                    let _slot = slot;
                    let _mode = io::enter(Mode::Detached);
                    match fill(&file, index, time, level, flipped, &decodes) {
                        Ok(values) => {
                            let _ = guard.insert(values);
                        }
                        // The guard drops: a waiting reader fills it and
                        // gets the error itself.
                        Err(e) => tracing::debug!("detached NetCDF field read failed: {e}"),
                    }
                });
                return self.wait_for(key, true);
            }
            CacheEntry::Timeout => {}
        }
        self.wait_for(key, false)
    }

    /// Wait for another fill of `key` until the deadline; fill it here if
    /// that one failed. `own` is this reader's own detached fill, already
    /// counted as a miss.
    fn wait_for(&self, key: &FieldKey, own: bool) -> Result<Values, DataServerError> {
        match FIELDS.get_value_or_guard_untracked(key, self.wait()) {
            CacheEntry::Value(values) => {
                if !own {
                    FIELDS.record_hit();
                }
                Ok(values)
            }
            CacheEntry::Vacant(guard) => {
                deadline::check()?;
                if !own {
                    FIELDS.record_miss();
                }
                let values = fill(
                    self.file,
                    self.index,
                    self.time,
                    self.level,
                    self.flipped,
                    self.decodes,
                )?;
                let _ = guard.insert(values.clone());
                Ok(values)
            }
            CacheEntry::Timeout => Err(self.timed_out()),
        }
    }
}

/// Reserve the decode's transient peak and decode variable `index` of
/// `file` at step `time`, served level `level`, whole: one cache fill.
fn fill(
    file: &OpenFile,
    index: usize,
    time: u32,
    level: usize,
    flipped: bool,
    decodes: &AtomicU64,
) -> Result<Values, DataServerError> {
    let part = (index, file.reads[index].span(&[level]), None);
    one(one(fill_parts(file, time, &[part], flipped, decodes)?)?)
}

/// Reserve the decode's transient peak, every level's of every part, and
/// decode `parts` of `file` at step `time`, each a variable over the stored
/// levels of its span, whole or over its window: one cache fill or one
/// query read, one request through OPeNDAP however many variables and
/// levels it holds, a decode counted per variable.
fn fill_parts(
    file: &OpenFile,
    time: u32,
    parts: &[Part],
    flipped: bool,
    decodes: &AtomicU64,
) -> Result<Vec<Planes>, DataServerError> {
    let peak = parts.iter().fold(0u64, |peak, &(index, span, window)| {
        peak.saturating_add(read_peak_bytes(&file.reads[index], span, window))
    });
    let _permit = BUDGET.reserve(peak)?;
    decodes.fetch_add(parts.len() as u64, Ordering::Relaxed);
    let reads: Vec<(&FieldRead, Span, Option<CellWindow>)> = parts
        .iter()
        .map(|&(index, span, window)| (&file.reads[index], span, window))
        .collect();
    let decoded = decode(&file.reader, time, &reads, flipped)?;
    Ok(decoded
        .into_iter()
        .map(|planes| planes.into_iter().map(Values::from).collect())
        .collect())
}

/// Fetch variable `index` of `file` at step `time`, level `level`, ahead of
/// requests, for the poll's prewarm: its compressed chunk lands in the
/// chunk cache ([`crate::remote`]); the decoded field is dropped, not
/// cached, so a prewarm never evicts the fields being drawn. `Ok(false)`
/// when the field is decoded already.
pub(crate) fn prewarm(
    file: &OpenFile,
    index: usize,
    time: u32,
    level: usize,
    flipped: bool,
) -> Result<bool, DataServerError> {
    let read = &file.reads[index];
    if FIELDS.contains_key(&field_key(file, index, time, level)) {
        return Ok(false);
    }
    let _permit = BUDGET.reserve(peak_bytes(read))?;
    decode(
        &file.reader,
        time,
        &[(read, read.span(&[level]), None)],
        flipped,
    )
    .map(|_| true)
}

/// Read and decode `reads` at step `time`, each a field over the stored
/// levels of its span, or its window of them: the typed hyperslabs (one
/// request through OPeNDAP for all of them, [`Reader::read_fields`]), CF
/// masking and packing, and the rows reversed to north-up when the file
/// stores them south-up, a plane per stored level, each decoded as a read
/// of that level alone decodes it; per read in `reads`' order. A failed
/// remote read keeps its meaning: a deadline, or a replaced file's 503.
fn decode(
    reader: &Reader,
    time: u32,
    reads: &[(&FieldRead, Span, Option<CellWindow>)],
    flipped: bool,
) -> Result<Vec<Vec<Vec<f32>>>, DataServerError> {
    let mut slabs = Vec::with_capacity(reads.len());
    for &(read, span, window) in reads {
        slabs.push(match window {
            None => (read.span_selection(time, span), read.shape()),
            Some(window) => (
                read.window_selection(time, span, window, flipped)
                    .ok_or_else(|| {
                        DataServerError::Engine(format!(
                            "variable '{}': window {window:?} is not on its field",
                            read.name
                        ))
                    })?,
                (window.height() as usize, window.width() as usize),
            ),
        });
    }
    let asked: Vec<(&FieldRead, Span, &netcdf_reader::NcSliceInfo)> = reads
        .iter()
        .zip(&slabs)
        .map(|(&(read, span, _), (selection, _))| (read, span, selection))
        .collect();
    let raws = reader.read_fields(time, &asked)?;
    reads
        .iter()
        .zip(&slabs)
        .zip(raws)
        .map(|((&(read, span, _), &(_, (ny, nx))), raw)| {
            let plane = ny.saturating_mul(nx);
            let planes = span.levels() as usize;
            if raw.len() != plane.saturating_mul(planes) {
                return Err(DataServerError::Storage(format!(
                    "variable '{}' step {time} stored levels {}-{}: read {} values for {planes} \
                     levels of a {nx} x {ny} field",
                    read.name,
                    span.first,
                    span.last,
                    raw.len()
                )));
            }
            Ok((0..planes)
                .map(|p| raw.decode_plane(p * plane..(p + 1) * plane, &read.packing, flipped, nx))
                .collect())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use netcdf_reader::{DynStorage, FileStorage, Storage, StorageBuffer};

    use super::*;
    use crate::ingest::{inspect, open_storage, Wanted};

    /// File storage whose reads take `delay` once `slow` is set.
    struct Slow {
        inner: FileStorage,
        slow: std::sync::atomic::AtomicBool,
        reads: AtomicU64,
    }

    impl Storage for Slow {
        fn len(&self) -> u64 {
            self.inner.len()
        }

        fn read_range(&self, offset: u64, len: usize) -> hdf5_reader::error::Result<StorageBuffer> {
            if self.slow.load(Ordering::Relaxed) {
                self.reads.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(20));
            }
            self.inner.read_range(offset, len)
        }
    }

    /// Eight readers of one cold field arrive while the first decodes it
    /// (each storage read takes 20 ms): one decode, one set of reads, one
    /// shared value.
    #[test]
    fn concurrent_readers_of_a_cold_field_share_one_decode() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/meps-netcdf/meps_sfc_01_20261015T06Z.nc");
        let storage = Arc::new(Slow {
            inner: FileStorage::open(&path).unwrap(),
            slow: false.into(),
            reads: AtomicU64::new(0),
        });
        let dynamic: DynStorage = storage.clone();
        let reader = Reader::Nc {
            nc: open_storage(dynamic.clone()).unwrap(),
            storage: dynamic,
            remote: None,
        };
        let names = vec!["wind_speed_of_gust".to_string()];
        let inspected = inspect(&reader, &Wanted::Listed(&names), 0).unwrap();
        let flipped = inspected.grid.flipped();
        let file = Arc::new(OpenFile {
            id: crate::next_file_id(),
            path: path.clone(),
            identity: crate::catalog::Identity::sized(0),
            reader,
            run: None,
            times: Vec::new(),
            reads: vec![inspected.fields[0].read.clone()],
            heap_bytes: 0,
        });
        storage.slow.store(true, Ordering::Relaxed);
        let decodes = Arc::new(AtomicU64::new(0));
        let start = Barrier::new(8);
        let values: Vec<Values> = std::thread::scope(|scope| {
            let readers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        read(&file, 0, 0, 0, flipped, &decodes).unwrap()
                    })
                })
                .collect();
            readers.into_iter().map(|r| r.join().unwrap()).collect()
        });
        assert_eq!(decodes.load(Ordering::Relaxed), 1);
        let reads = storage.reads.load(Ordering::Relaxed);
        assert!(
            (1..=9).contains(&reads),
            "{reads} storage reads for one decode"
        );
        assert!(values.iter().all(|v| Arc::ptr_eq(v, &values[0])));
        assert_eq!(values[0].len(), 110);
    }

    /// A pool lends at most its cap of slots, counts a claim past it as a
    /// read run inline, and takes a slot back when its read drops it. Query
    /// reads have a pool of their own, half the render fills'.
    #[test]
    fn a_detached_pool_lends_at_most_its_cap() {
        let pool: &'static DetachedPool = Box::leak(Box::new(DetachedPool::new(2)));
        let first = pool.claim().unwrap();
        let _second = pool.claim().unwrap();
        assert!(pool.claim().is_none());
        assert_eq!(pool.metrics(), (2, 2, 1));
        drop(first);
        let _third = pool.claim().unwrap();
        assert_eq!(pool.metrics(), (2, 3, 1));
        assert_eq!((RENDER_FILLS.cap, QUERY_READS.cap), (16, 8));
    }

    /// A window read reserves the whole field's peak in proportion to its
    /// cells (#1171): a 2 × 2 window of an 11 × 10 `f32` field 4/110 of it.
    /// A read over several stored levels reserves every level's: a window
    /// over 13 levels 13 windows' peaks, six times its reply's values.
    #[test]
    fn a_window_reserves_its_share_of_the_fields_peak() {
        use crate::ingest::{Dim, Packing, Stored};
        let read = FieldRead {
            name: "t".into(),
            dims: vec![Dim::Time, Dim::Whole(11), Dim::Whole(10)],
            stored: Stored::Float,
            packing: Packing {
                scale: 1.0,
                offset: 0.0,
                fill: Vec::new(),
            },
            chunk_bytes: 440,
            field_bytes: 440,
            levels: Vec::new(),
        };
        assert_eq!(peak_bytes(&read), 6 * 440);
        let window = |columns, rows| CellWindow { columns, rows };
        assert_eq!(window_peak_bytes(&read, window((4, 5), (5, 6))), 6 * 4 * 4);
        assert_eq!(window_peak_bytes(&read, window((0, 9), (0, 10))), 6 * 440);
        assert_eq!(window_peak_bytes(&read, window((3, 3), (3, 3))), 24);
        let one = Span { first: 4, last: 4 };
        let thirteen = Span { first: 0, last: 12 };
        let point = window((4, 5), (5, 6));
        assert_eq!(read_peak_bytes(&read, one, None), 6 * 440);
        assert_eq!(read_peak_bytes(&read, one, Some(point)), 6 * 4 * 4);
        assert_eq!(
            read_peak_bytes(&read, thirteen, Some(point)),
            13 * 6 * 4 * 4
        );
    }

    /// A south-up field's rows are reversed while decoding, its values
    /// unpacked and its fill NaN, in every stored type.
    #[test]
    fn decoding_reverses_rows_and_unpacks() {
        use crate::ingest::Packing;
        use crate::source::Raw;

        let packing = Packing {
            scale: 0.5,
            offset: 1.0,
            fill: vec![-1.0],
        };
        let flipped = Raw::I16(vec![1, 2, 3, 4, 5, -1]).decode(&packing, true, 2);
        assert_eq!(
            (flipped[0], flipped[2..].to_vec()),
            (3.5, vec![2.5, 3.0, 1.5, 2.0])
        );
        assert!(flipped[1].is_nan(), "the fill");
        let column = Raw::F64(vec![1.0, 2.0, 3.0, 4.0]).decode(&packing, true, 1);
        assert_eq!(column, [3.0, 2.5, 2.0, 1.5]);
        let as_stored = Raw::U8(vec![2, 4]).decode(&packing, false, 2);
        assert_eq!(as_stored, [2.0, 3.0]);
    }
}
