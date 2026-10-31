//! Load each variable's manifest and the default run's shard indexes at
//! catalog build, once per snapshot (#1353).
//!
//! A variable's first read after a run update otherwise waits for its
//! array's manifest, then its shard index, then its inner chunk, one after
//! another. Loading the first two here, on the poll runtime and before the
//! catalog is published, leaves a cold read one request: its inner chunk.
//! The manifests stay in Icechunk's manifest cache; the shard indexes stay
//! with the catalog's variables (`decoded::shards`).
//!
//! The run is the catalog's own default, `Catalog::resolve_run(None)`, so
//! this follows whatever the read path selects. A store without runs loads
//! the shards of its latest time step.
//!
//! The loading is bounded in time ([`BOUND`]): storage slow enough to
//! exceed it would hold back publishing the new catalog, while the previous
//! one keeps serving the old run. Past the bound the catalog publishes with
//! what has loaded, and reads fetch the rest as they did before.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zarrs::array::ArraySubset;
use zarrs::storage::ReadableStorageTraits;

use super::{single_threaded_opts, Catalog, Variable};

/// Loads in flight at once (Critical Rule 9): each waits for its requests.
const CONCURRENCY: usize = 8;
/// The longest a catalog build spends loading ahead. A healthy remote
/// repository needs a few round trips of eight loads: a manifest, then an
/// index, a few seconds in all. Each saves one variable's first read under
/// a second, while every request waits for the new run until the catalog
/// publishes.
pub(crate) const BOUND: Duration = Duration::from_secs(30);

/// Most shards of one variable loaded ahead; a default run normally has one
/// or two.
const MAX_SHARDS: usize = 64;

enum Job<'a> {
    /// Read a shard's index, which also loads the manifest it is listed in.
    Shard(&'a Variable, Vec<u64>),
    /// Look up a chunk's size: loads the array's manifest, and no payload,
    /// for a variable whose shard indexes are not kept.
    Manifest(&'a Variable, Vec<u64>),
}

impl Catalog {
    /// Load ahead for an Icechunk catalog, for at most `bound`; `true` when
    /// it stopped there with loads left. Failures only log: a read then
    /// fetches what is missing, as it did before.
    pub(crate) fn preload(&self, collection_id: &str, bound: Duration) -> bool {
        run(self, collection_id, bound)
    }
}

fn run(catalog: &Catalog, collection_id: &str, bound: Duration) -> bool {
    // A plain store has no manifests, and its size lookups would fetch
    // whole objects.
    if catalog.revision.is_none() {
        return false;
    }
    let run = match catalog.resolve_run(None) {
        Ok(run) => run,
        Err(error) => {
            tracing::warn!("[{collection_id}] Zarr preload skipped: {error}");
            return false;
        }
    };
    let mut jobs = Vec::new();
    for var in &catalog.vars {
        let subset = default_subset(var, run);
        let chunks = match var.array.chunks_in_array_subset(&subset) {
            Ok(Some(chunks)) => chunks,
            Ok(None) | Err(_) => continue,
        };
        match var.decoded.as_ref().and_then(|d| d.shard_indexes()) {
            Some(_) => jobs.extend(
                chunks
                    .indices()
                    .into_iter()
                    .take(MAX_SHARDS)
                    .map(|shard| Job::Shard(var, shard.to_vec())),
            ),
            None => jobs.push(Job::Manifest(var, chunks.start().to_vec())),
        }
    }
    if jobs.is_empty() {
        return false;
    }
    let started = Instant::now();
    // Every load runs under this deadline, its storage requests included:
    // one in flight at the bound ends there.
    let end = started + bound;
    let next = AtomicUsize::new(0);
    let loaded = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let failure = Mutex::new(None);
    let work = || {
        let _deadline = ds_core::deadline::enter(Some(end));
        let options = single_threaded_opts();
        while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
            if ds_core::deadline::check().is_err() {
                stopped.store(true, Ordering::Relaxed);
                break;
            }
            let result = match job {
                Job::Shard(var, shard) => var
                    .decoded
                    .as_ref()
                    .and_then(|d| d.shard_indexes())
                    .map_or(Ok(()), |shards| shards.load(&var.array, shard, &options)),
                Job::Manifest(var, chunk) => var
                    .array
                    .storage()
                    .size_key(&var.array.chunk_key(chunk))
                    .map(drop)
                    .map_err(|e| ds_core::error::DataServerError::Storage(e.to_string())),
            };
            match result {
                Ok(()) => {
                    loaded.fetch_add(1, Ordering::Relaxed);
                }
                // Cut off at the bound, not failed.
                Err(_) if ds_core::deadline::check().is_err() => {
                    stopped.store(true, Ordering::Relaxed);
                    break;
                }
                Err(error) => {
                    let name = match job {
                        Job::Shard(var, _) | Job::Manifest(var, _) => &var.name,
                    };
                    failure
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_or_insert(format!("{name}: {error}"));
                }
            }
        }
    };
    blocking(|| {
        std::thread::scope(|scope| {
            for i in 1..CONCURRENCY.min(jobs.len()) {
                // A thread that cannot start leaves its share to the others.
                let _ = std::thread::Builder::new()
                    .name(format!("zarr-preload-{i}"))
                    .spawn_scoped(scope, work);
            }
            work();
        })
    });
    let loaded = loaded.into_inner();
    let ms = started.elapsed().as_millis();
    let stopped = stopped.into_inner();
    match failure.into_inner().unwrap_or_else(|e| e.into_inner()) {
        _ if stopped => tracing::warn!(
            "[{collection_id}] Zarr preload stopped at its {} s bound: {loaded} of {} \
             shard indexes and manifests loaded; the catalog publishes, reads fetch the rest",
            bound.as_secs_f64(),
            jobs.len()
        ),
        None => tracing::info!(
            "[{collection_id}] Zarr preload: {loaded} shard indexes and manifests in {ms} ms"
        ),
        Some(first) => tracing::warn!(
            "[{collection_id}] Zarr preload: {loaded} of {} loaded in {ms} ms; \
             reads fetch the rest (first failure: {first})",
            jobs.len()
        ),
    }
    stopped
}

/// What a default read of `var` covers: the run's whole grid over all its
/// steps, or a store's latest step without runs, other axes at index 0 as
/// the read path pins them.
fn default_subset(var: &Variable, run: Option<usize>) -> ArraySubset {
    let shape = var.array.shape();
    let ranges: Vec<_> = (0..var.ndim)
        .map(|axis| {
            let len = shape[axis];
            if Some(axis) == var.ref_axis {
                run.map_or(0..1, |r| r as u64..r as u64 + 1)
            } else if Some(axis) == var.time_axis {
                if var.ref_axis.is_some() {
                    0..len
                } else {
                    len.saturating_sub(1)..len
                }
            } else if axis == var.lat_axis || axis == var.lon_axis {
                0..len
            } else {
                0..len.min(1)
            }
        })
        .collect();
    ArraySubset::new_with_ranges(&ranges)
}

/// Run `f`, which blocks on storage, without parking a runtime worker's
/// scheduler core when called from one.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

#[cfg(all(test, feature = "icechunk"))]
impl Catalog {
    /// Each variable's resident shards, in variable order.
    pub(crate) fn preloaded_shards(&self) -> Vec<Vec<Vec<u64>>> {
        self.vars
            .iter()
            .map(|var| {
                var.decoded
                    .as_ref()
                    .and_then(|d| d.shard_indexes())
                    .map_or_else(Vec::new, |shards| shards.resident())
            })
            .collect()
    }
}
