//! Snapshot-keyed native decoded chunks. Shards are containers, not cache units.
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use ds_cache::{ByteBoundedCache, CacheMetrics, MIB};
use ds_core::{deadline, error::DataServerError};
use rayon::prelude::*;
use zarrs::array::{Array, ArrayBytes, ArrayShardedExt, ArraySubset, ChunkGrid, CodecOptions};

use crate::store::EngineStore;

// Never expand a tiny subset into an arbitrarily large cache fill. Oversized
// chunks retain the ordinary partial-read path, without decoded retention.
const MAX_CHUNK_BYTES: u64 = 64 * MIB;

// Separate from zarrs' internal parallelism: these workers explicitly inherit
// deadlines and use Icechunk's persistent I/O runtime. Codec work never runs
// on the I/O reactor. One shared pool bounds fan-out across collections.
pub(crate) const MAX_PARALLEL_CHUNKS: usize = 4;
static READERS: LazyLock<Result<rayon::ThreadPool, rayon::ThreadPoolBuildError>> =
    LazyLock::new(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(MAX_PARALLEL_CHUNKS)
            .thread_name(|i| format!("zarr-chunk-{i}"))
            .build()
    });

#[derive(Clone, Hash, PartialEq, Eq)]
struct Key {
    revision: Arc<str>,
    array: Arc<str>,
    indices: Vec<u64>,
}

fn overhead(key: &Key) -> u64 {
    (key.revision.len() + key.array.len() + key.indices.capacity() * 8 + 160) as u64
}

pub(crate) struct DecodedCache(ByteBoundedCache<Key, Arc<Vec<u8>>>);

impl DecodedCache {
    pub(crate) fn new(capacity: u64) -> Self {
        Self(ByteBoundedCache::new(capacity, 16 * MIB, |key, value| {
            overhead(key).saturating_add(value.capacity() as u64)
        }))
    }

    pub(crate) fn metrics(&self) -> CacheMetrics {
        self.0.metrics()
    }
}

pub(crate) struct DecodedArray {
    cache: Arc<DecodedCache>,
    revision: Arc<str>,
    name: Arc<str>,
    grid: ChunkGrid,
}

impl DecodedArray {
    pub(crate) fn new(
        array: &Array<EngineStore>,
        revision: Option<&str>,
        name: &str,
        cache: Arc<DecodedCache>,
    ) -> Option<Self> {
        let revision = revision?;
        // Outer transforms can make an inner chunk require decoding a whole
        // shard. Keep the ordinary path for those codec layouts. A zero cache
        // budget disables retention, but still permits bounded inner reads.
        if array.is_sharded() && !array.is_exclusively_sharded() {
            return None;
        }
        Some(Self {
            cache,
            revision: revision.into(),
            name: name.into(),
            grid: array.subchunk_grid(),
        })
    }

    pub(crate) fn read(
        &self,
        array: &Array<EngineStore>,
        subset: &ArraySubset,
        options: &CodecOptions,
        parallelism: usize,
    ) -> Result<ArrayBytes<'static>, DataServerError> {
        deadline::check()?;
        let size = array
            .data_type()
            .fixed_size()
            .ok_or_else(|| error("non-numeric chunk"))?;
        let length = byte_length(subset.shape(), size)?;
        let Some(chunks) = self.grid.chunks_in_array_subset(subset).map_err(error)? else {
            return read_native(array, subset, options).map(ArrayBytes::new_flen);
        };
        let mut output = vec![0; length];
        let mut indices = chunks.indices().into_iter();
        let parallelism = parallelism.clamp(1, MAX_PARALLEL_CHUNKS);
        let end = deadline::current();
        let budget = crate::encoded::current().map(|context| context.budget.clone());
        loop {
            deadline::check()?;
            // Bound queued jobs and completed-but-not-copied buffers as well
            // as active decodes. Never collect all chunks of a large window.
            let batch: Vec<_> = indices
                .by_ref()
                .take(parallelism)
                .map(|indices| indices.to_vec())
                .collect();
            if batch.is_empty() {
                break;
            }
            let load = |indices: Vec<u64>| {
                let _deadline = deadline::enter(end);
                let _encoded = crate::encoded::enter(budget.clone());
                self.read_chunk(array, subset, options, indices, size)
            };
            let results: Vec<_> = if batch.len() == 1 {
                batch.into_iter().map(load).collect()
            } else {
                // install joins every job even on failure/unwind. The caller
                // owns its memory permit until workers and results are gone.
                READERS
                    .as_ref()
                    .map_err(error)?
                    .install(|| batch.into_par_iter().map(load).collect())
            };
            deadline::check()?;
            for result in results {
                let (bytes, source) = result?;
                let overlap = source.overlap(subset).map_err(error)?;
                copy_overlap(&bytes, &source, &mut output, subset, &overlap, size)?;
            }
        }
        deadline::check()?;
        Ok(ArrayBytes::new_flen(output))
    }

    fn read_chunk(
        &self,
        array: &Array<EngineStore>,
        subset: &ArraySubset,
        options: &CodecOptions,
        indices: Vec<u64>,
        size: usize,
    ) -> Result<(Arc<Vec<u8>>, ArraySubset), DataServerError> {
        deadline::check()?;
        let chunk = self
            .grid
            .subset(&indices)
            .map_err(error)?
            .ok_or_else(|| error("chunk is outside the grid"))?
            .overlap(&array.subset_all())
            .map_err(error)?;
        let key = Key {
            revision: self.revision.clone(),
            array: self.name.clone(),
            indices,
        };
        let chunk_length = byte_length(chunk.shape(), size)?;
        let eligible = chunk_length as u64 <= MAX_CHUNK_BYTES
            && (chunk_length as u64).saturating_add(overhead(&key))
                <= self.cache.0.capacity_bytes();
        if !eligible {
            let overlap = chunk.overlap(subset).map_err(error)?;
            return Ok((Arc::new(read_native(array, &overlap, options)?), overlap));
        }
        let end = deadline::current();
        let wait = end.map_or(Duration::from_secs(30), |end| {
            end.saturating_duration_since(Instant::now())
        });
        let bytes = self.cache.0.get_or_insert_with_timeout(
            &key,
            wait,
            || read_native(array, &chunk, options).map(Arc::new),
            || {
                if end.is_some() {
                    DataServerError::DeadlineExceeded
                } else {
                    error("timed out waiting for a decoded chunk")
                }
            },
        )?;
        Ok((bytes, chunk))
    }
}

fn byte_length(shape: &[u64], element_size: usize) -> Result<usize, DataServerError> {
    shape.iter().try_fold(element_size, |n, &dim| {
        usize::try_from(dim)
            .ok()
            .and_then(|dim| n.checked_mul(dim))
            .ok_or(DataServerError::ResourceExhausted)
    })
}

fn read_native(
    array: &Array<EngineStore>,
    subset: &ArraySubset,
    options: &CodecOptions,
) -> Result<Vec<u8>, DataServerError> {
    deadline::check()?;
    let result = array.retrieve_array_subset_opt::<ArrayBytes<'static>>(subset, options);
    // Preserve the typed deadline even when the codec erased the storage error.
    deadline::check()?;
    let bytes = result
        .map_err(crate::catalog::chunk_read_error)?
        .into_fixed()
        .map_err(error)?
        .into_owned();
    Ok(bytes)
}

// Copy contiguous rows in logical array order. zarrs owns all decoding,
// transposition, endian conversion, edge clipping and fill-value semantics.
fn copy_overlap(
    source: &[u8],
    source_region: &ArraySubset,
    target: &mut [u8],
    target_region: &ArraySubset,
    overlap: &ArraySubset,
    size: usize,
) -> Result<(), DataServerError> {
    let ndim = overlap.shape().len();
    let mut rows = overlap.shape().to_vec();
    let width = *rows.last().ok_or_else(|| error("scalar data variable"))? as usize * size;
    rows[ndim - 1] = 1;
    for row in ArraySubset::new_with_shape(rows).indices() {
        deadline::check()?;
        let offset = |region: &ArraySubset| -> usize {
            row.iter().enumerate().fold(0, |offset, (axis, &i)| {
                offset * region.shape()[axis] as usize
                    + (overlap.start()[axis] + i - region.start()[axis]) as usize
            }) * size
        };
        let from = offset(source_region);
        let to = offset(target_region);
        let src = source
            .get(from..from + width)
            .ok_or_else(|| error("decoded chunk is too short"))?;
        let dst = target
            .get_mut(to..to + width)
            .ok_or_else(|| error("invalid subset copy"))?;
        dst.copy_from_slice(src);
    }
    Ok(())
}

fn error(error: impl std::fmt::Display) -> DataServerError {
    DataServerError::Engine(format!("Zarr decoded chunk read: {error}"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod concurrency_tests;
