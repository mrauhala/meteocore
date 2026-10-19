//! Admission for transient native reads and sampling windows, separate from caches.
use std::sync::{Arc, LazyLock};

use ds_core::{deadline, error::DataServerError};
use ds_executor::budget::{ByteBudget, BytePermit};
use zarrs::array::{Array, ArrayShardedExt, ArraySubset};

use crate::store::EngineStore;

pub(crate) static BUDGET: LazyLock<Arc<Budget>> = LazyLock::new(|| {
    Arc::new(Budget::new(
        ds_cache::env_mb("MC_ZARR_READ_MEMORY_MB", 1024).saturating_mul(ds_cache::MIB),
    ))
});

/// Zarr's admission planning and deadline checks over the shared fail-fast
/// [`ByteBudget`], which keeps the account.
pub(crate) struct Budget {
    bytes: Arc<ByteBudget>,
}

impl Budget {
    pub(crate) fn new(capacity: u64) -> Self {
        Self {
            bytes: Arc::new(ByteBudget::new(capacity)),
        }
    }

    pub(crate) fn metrics(&self) -> (u64, u64, u64) {
        self.bytes.metrics()
    }

    /// Transient chunk, encoded storage or intermediate codec capacity. Never
    /// wait while holding a native window.
    pub(crate) fn reserve_bytes(&self, bytes: u64) -> Result<Arc<Permit>, DataServerError> {
        self.try_reserve_bytes(bytes)?.ok_or_else(|| self.reject())
    }

    /// Probe another batch slot without counting reduced fan-out as a rejected
    /// request. The caller must reject if even the first slot cannot fit.
    pub(crate) fn try_reserve_bytes(
        &self,
        bytes: u64,
    ) -> Result<Option<Arc<Permit>>, DataServerError> {
        deadline::check()?;
        Ok(self.bytes.try_reserve(bytes).map(|permit| {
            Arc::new(Permit {
                _bytes: permit,
                parallelism: 1,
            })
        }))
    }

    pub(crate) fn reject(&self) -> DataServerError {
        self.bytes.reject()
    }

    /// Fail fast: callers already own an executor slot, and may hold another
    /// window (e.g. across the antimeridian). Waiting here can deadlock them.
    pub(crate) fn reserve(
        &self,
        array: &Array<EngineStore>,
        subset: &ArraySubset,
        extra_bytes: Option<u64>,
        split_inner_chunks: bool,
    ) -> Result<Arc<Permit>, DataServerError> {
        deadline::check()?;
        let result = (|| {
            let extra_bytes = extra_bytes.ok_or(DataServerError::ResourceExhausted)?;
            let plan = plan(
                array,
                subset,
                extra_bytes,
                split_inner_chunks,
                self.bytes.capacity(),
            )?;
            let bytes = plan
                .source
                .checked_add(plan.workspace)
                .ok_or(DataServerError::ResourceExhausted)?;
            let permit = self
                .bytes
                .try_reserve(bytes)
                .ok_or(DataServerError::ResourceExhausted)?;
            Ok(Arc::new(Permit {
                _bytes: permit,
                parallelism: plan.parallelism,
            }))
        })();
        result.map_err(|err| match err {
            DataServerError::ResourceExhausted => self.reject(),
            err => err,
        })
    }
}

/// Returns its bytes to the budget when dropped.
pub(crate) struct Permit {
    _bytes: BytePermit,
    parallelism: usize,
}

impl Permit {
    /// Requested fan-out limit for a source reservation. The decoded reader
    /// must still admit each chunk separately after cache lookup.
    pub(crate) fn parallelism(&self) -> usize {
        self.parallelism
    }
}

/// Reserved estimate, capacity, and rejected admissions. Process-wide across
/// collections, snapshots, reloads, and Maps/WMS/Tiles/EDR callers.
pub fn metrics() -> (u64, u64, u64) {
    BUDGET.metrics()
}

// Account for native subset bytes + a typed conversion copy + raw f64 + the
// physical f64 sampling window. Some lifetimes do not overlap: deliberately
// admit all native/conversion buffers together before payload reads.
// The decoded reader admits chunk buffers separately after the cache lookup;
// other paths reserve one decode workspace together with the source window.
// Encoded reads consume prepaid headroom or reserve additional bytes at collection.
// Other codec-private scratch, persistent metadata, caches, and API outputs remain
// separate from this estimate; it is not an allocator-enforced RSS limit.
struct Plan {
    source: u64,
    workspace: u64,
    parallelism: usize,
}

fn plan(
    array: &Array<EngineStore>,
    subset: &ArraySubset,
    extra_bytes: u64,
    split_inner_chunks: bool,
    capacity: u64,
) -> Result<Plan, DataServerError> {
    let exhausted = || DataServerError::ResourceExhausted;
    let native = array.data_type().fixed_size().ok_or_else(exhausted)? as u64;
    let source = bytes(subset.shape(), native * 2 + 16)?
        .checked_add(extra_bytes)
        .filter(|&n| n <= capacity)
        .ok_or_else(exhausted)?;
    let workspace = if split_inner_chunks {
        0
    } else {
        workspace_bytes(array, subset, false)?
    };
    let parallelism = if split_inner_chunks {
        array
            .subchunk_grid()
            .chunks_in_array_subset(subset)
            .map_err(|_| exhausted())?
            .map_or(1, |chunks| {
                chunks
                    .indices()
                    .into_iter()
                    .take(crate::decoded::MAX_PARALLEL_CHUNKS)
                    .count()
                    .max(1)
            })
    } else {
        1
    };
    Ok(Plan {
        source,
        workspace,
        parallelism,
    })
}

/// Four native/index buffers for a cold read. Use full stored shapes even
/// when the requested region clips a padded chunk. Cached reads skip this
/// estimate and instead reserve the capacity of the buffer they keep alive.
pub(crate) fn workspace_bytes(
    array: &Array<EngineStore>,
    subset: &ArraySubset,
    split_inner_chunks: bool,
) -> Result<u64, DataServerError> {
    let exhausted = || DataServerError::ResourceExhausted;
    let native = array.data_type().fixed_size().ok_or_else(exhausted)? as u64;
    let chunks = array
        .chunks_in_array_subset(subset)
        .map_err(|_| exhausted())?
        .ok_or_else(exhausted)?;
    let inner = array
        .effective_subchunk_shape()
        .map(|shape| shape.iter().map(|d| d.get()).collect::<Vec<_>>());
    let exclusively_sharded = array.is_exclusively_sharded();
    let mut workspace = 0;
    for indices in chunks.indices() {
        deadline::check()?;
        let outer = array.chunk_shape(&indices).map_err(|_| exhausted())?;
        let outer: Vec<u64> = outer.iter().map(|d| d.get()).collect();
        let (decode, index) = if let Some(inner) = &inner {
            // Use the full stored chunk shape, including edge padding and all
            // forecast leads. A tiny requested overlap cannot shrink decoding.
            let entries = outer.iter().zip(inner).try_fold(1u64, |n, (&o, &i)| {
                n.checked_mul(o.div_ceil(i)).ok_or_else(exhausted)
            })?;
            let full_shard = !split_inner_chunks
                && array
                    .chunk_subset(&indices)
                    .map_err(|_| exhausted())?
                    .overlap(subset)
                    .map_err(|_| exhausted())?
                    .shape()
                    == outer;
            let decode = if exclusively_sharded && !full_shard {
                bytes(inner, native)?
            } else {
                bytes(&outer, native)?
            };
            (decode, entries.checked_mul(16).ok_or_else(exhausted)?)
        } else {
            // Outer transforms may require the whole shard to be decoded.
            (bytes(&outer, native)?, 0)
        };
        let current = decode
            .checked_add(index)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(exhausted)?;
        workspace = workspace.max(current);
    }
    Ok(workspace)
}

fn bytes(shape: &[u64], element_size: u64) -> Result<u64, DataServerError> {
    shape.iter().try_fold(element_size, |n, &dim| {
        n.checked_mul(dim).ok_or(DataServerError::ResourceExhausted)
    })
}

#[cfg(test)]
mod tests;
