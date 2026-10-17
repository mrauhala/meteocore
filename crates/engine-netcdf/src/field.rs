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
//! A remote file's cold field is filled **detached** (#1067): the request
//! claims the fill, hands the claim to a task on the I/O runtime that reads
//! under [`Mode::Detached`] (no request deadline,
//! [`ds_storage::FetchBudget::Background`]), and waits for it only until its
//! own deadline. A fetch that outlives one request's 3 s still fills the
//! cache for the next. At most [`MAX_DETACHED_FILLS`] run at once; past
//! that a fill runs inline, under the request's deadline. An EDR or
//! GetFeatureInfo read ([`read_uncached`]) detaches from a pool of its own,
//! [`MAX_QUERY_DETACHED_READS`], so query load never takes the slots render
//! fills need to leave a warm cache behind a missed 3 s deadline.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use ds_cache::{ByteBoundedCache, CacheEntry};
use ds_core::deadline;
use ds_core::error::DataServerError;
use netcdf_reader::NcFile;

use crate::catalog::OpenFile;
use crate::decode_budget::BUDGET;
use crate::ingest::{FieldRead, Stored};
use crate::io::{self, Mode};
use crate::remote::ReadFailure;

/// A decoded 2-D field: rows north to south, NaN for nodata.
pub(crate) type Values = Arc<[f32]>;

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
    std::mem::size_of_val::<[f32]>(values) as u64 + key.variable.len() as u64 + 96
}

/// Decoded fields, `MC_NETCDF_FIELD_CACHE_MB` (default 256 MiB), shared by
/// every NetCDF collection.
static FIELDS: LazyLock<ByteBoundedCache<FieldKey, Values>> = LazyLock::new(|| {
    ByteBoundedCache::from_env("MC_NETCDF_FIELD_CACHE_MB", 256, TYPICAL_FIELD_BYTES, weigh)
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
        if file.remote.is_some() {
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

/// Variable `index` of `file` at step `time` for an EDR query (#1067): the
/// decoded field when the cache holds it (a frame being drawn), else decoded
/// for this query alone and not cached, so a series over a whole run never
/// evicts the frames being drawn. A remote file's compressed chunk still
/// lands in the chunk cache ([`crate::remote`]), so the same field read
/// again costs a decode, not a fetch. The lookup is not counted in the
/// field cache's hits and misses, which measure the renders it serves.
///
/// A remote read is detached, as a render's fill is: it runs on the I/O
/// runtime under [`Mode::Detached`] (no deadline, the background fetch
/// budget), and the query waits for it only until its own deadline. A
/// query that misses its deadline fails, while the reads it started go on
/// into the chunk cache, so a retry makes progress. The slot comes from
/// [`QUERY_READS`], never the render fills' pool; past
/// [`MAX_QUERY_DETACHED_READS`] the read runs here, under the deadline.
/// Each read reserves its transient peak from the decode budget.
pub(crate) fn read_uncached(
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
        return Ok(values);
    }
    let end = deadline::current();
    let result = io::blocking(|| {
        let slot = file.remote.as_ref().and_then(|_| QUERY_READS.claim());
        let Some(slot) = slot else {
            return fill(file, index, time, level, flipped, decodes);
        };
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let (file, decodes) = (file.clone(), decodes.clone());
        io::handle().spawn_blocking(move || {
            let _slot = slot;
            let _mode = io::enter(Mode::Detached);
            // The query may have given up: the chunk is cached anyway.
            let _ = sender.send(fill(&file, index, time, level, flipped, &decodes));
        });
        let wait = end.map_or(Duration::from_secs(30), |end| {
            end.saturating_duration_since(Instant::now())
        });
        match receiver.recv_timeout(wait) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if end.is_some() => {
                Err(DataServerError::DeadlineExceeded)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(DataServerError::Storage(
                "timed out waiting for a NetCDF field read".into(),
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(DataServerError::Engine(
                "a detached NetCDF field read stopped".into(),
            )),
        }
    });
    deadline::check()?;
    result
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
/// `file` at step `time`, level `level`: one cache fill.
fn fill(
    file: &OpenFile,
    index: usize,
    time: u32,
    level: usize,
    flipped: bool,
    decodes: &AtomicU64,
) -> Result<Values, DataServerError> {
    let read = &file.reads[index];
    let _permit = BUDGET.reserve(peak_bytes(read))?;
    decodes.fetch_add(1, Ordering::Relaxed);
    decode(&file.nc, read, time, level, flipped).map(Values::from)
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
    decode(&file.nc, read, time, level, flipped).map(|_| true)
}

/// Read and decode one field: the typed hyperslab, CF masking and packing,
/// and the rows reversed to north-up when the file stores them south-up.
/// A failed remote read keeps its meaning ([`ReadFailure::to_error`]): a
/// deadline, or a replaced file's 503.
fn decode(
    nc: &NcFile,
    read: &FieldRead,
    time: u32,
    level: usize,
    flipped: bool,
) -> Result<Vec<f32>, DataServerError> {
    let selection = read.selection(time, level);
    let name = &*read.name;
    let packing = &read.packing;
    let (ny, nx) = read.shape();
    let cells = ny * nx;
    let fail = |e: netcdf_reader::Error| match ReadFailure::find(&e) {
        Some(failure) => failure.to_error(),
        None => {
            DataServerError::Storage(format!("variable '{name}' step {time} level {level}: {e}"))
        }
    };
    macro_rules! typed {
        ($t:ty) => {{
            let array = nc
                .read_variable_slice::<$t>(name, &selection)
                .map_err(fail)?;
            let mut out = Vec::with_capacity(cells);
            match array.as_slice() {
                Some(values) if values.len() == cells => {
                    if flipped {
                        for row in values.chunks_exact(nx).rev() {
                            out.extend(row.iter().map(|&v| packing.decode(v as f64)));
                        }
                    } else {
                        out.extend(values.iter().map(|&v| packing.decode(v as f64)));
                    }
                }
                _ => {
                    out.extend(array.iter().map(|&v| packing.decode(v as f64)));
                    if out.len() == cells && flipped {
                        flip_rows(&mut out, nx);
                    }
                }
            }
            out
        }};
    }
    let values = match read.stored {
        Stored::Byte => typed!(i8),
        Stored::UByte => typed!(u8),
        Stored::Short => typed!(i16),
        Stored::UShort => typed!(u16),
        Stored::Int => typed!(i32),
        Stored::UInt => typed!(u32),
        Stored::Float => typed!(f32),
        Stored::Double => typed!(f64),
    };
    if values.len() != cells {
        return Err(DataServerError::Storage(format!(
            "variable '{name}' step {time} level {level}: read {} values for a {nx} x {ny} field",
            values.len()
        )));
    }
    Ok(values)
}

/// Reverse the rows of a row-major field `nx` wide, in place.
fn flip_rows(values: &mut [f32], nx: usize) {
    let ny = values.len() / nx;
    for top in 0..ny / 2 {
        let bottom = ny - 1 - top;
        let (upper, lower) = values.split_at_mut(bottom * nx);
        upper[top * nx..(top + 1) * nx].swap_with_slice(&mut lower[..nx]);
    }
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
        let nc = open_storage(dynamic.clone()).unwrap();
        let names = vec!["wind_speed_of_gust".to_string()];
        let inspected = inspect(&nc, &dynamic, &Wanted::Listed(&names), 0).unwrap();
        let flipped = inspected.grid.flipped;
        let file = Arc::new(OpenFile {
            id: crate::next_file_id(),
            path: path.clone(),
            identity: crate::catalog::Identity::sized(0),
            nc,
            remote: None,
            run: None,
            times: Vec::new(),
            reads: vec![inspected.fields[0].read.clone()],
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

    #[test]
    fn flip_rows_reverses_rows_in_place() {
        let mut values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        flip_rows(&mut values, 2);
        assert_eq!(values, [5.0, 6.0, 3.0, 4.0, 1.0, 2.0]);
        let mut even = vec![1.0, 2.0, 3.0, 4.0];
        flip_rows(&mut even, 1);
        assert_eq!(even, [4.0, 3.0, 2.0, 1.0]);
    }
}
