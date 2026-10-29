//! Shard indexes kept resident per catalog (#1353).
//!
//! A cold inner-chunk read through `Array::retrieve_array_subset_opt` builds
//! a new sharding partial decoder, and building one reads the shard index.
//! Here each shard's decoder is built once, at catalog build for the default
//! run or on its first read, and kept for the catalog's snapshot: a cold
//! inner chunk then costs one request, for its own bytes.
//!
//! Not zarrs' `ArrayShardedReadableExtCache`: for an exclusively sharded
//! array it rebuilds the sharding codec from metadata, which would decode
//! with zarrs' unbounded codecs instead of `codec_limits`. A decoder from
//! `Array::partial_decoder_opt` is the one upstream builds for the same read,
//! through the array's bounded chain, so values cannot differ. An exclusively
//! sharded chain has no bytes-to-bytes codec, so the decoder retains no
//! payload bytes, only the decoded index.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ds_core::{deadline, error::DataServerError};
use zarrs::array::{
    codec::api::ArrayPartialDecoderTraits, Array, ArrayShardedExt, ArraySubset, CodecOptions,
};

use crate::store::EngineStore;

/// The resident indexes of one variable, by estimated size: offset and
/// length per inner chunk, plus decoder overhead. Shards past it are read
/// as before, their index fetched for each cold read.
const RESIDENT_BYTES: u64 = 256 * 1024;
const DECODER_OVERHEAD: u64 = 256;

type Decoder = Arc<dyn ArrayPartialDecoderTraits>;

pub(crate) struct ShardIndexes {
    resident: Mutex<HashMap<Vec<u64>, Decoder>>,
    /// Most shards kept: the first ones loaded, the default run's first.
    capacity: usize,
}

impl ShardIndexes {
    /// `None` unless `array` is exclusively sharded and one shard's index
    /// fits the per-variable allowance.
    pub(crate) fn new(array: &Array<EngineStore>) -> Option<Self> {
        if !array.is_exclusively_sharded() {
            return None;
        }
        let inner = array.subchunk_shape()?;
        let shard = array.chunk_shape(&vec![0; array.dimensionality()]).ok()?;
        let subchunks = shard
            .iter()
            .zip(inner.iter())
            .try_fold(1u64, |n, (shard, inner)| {
                n.checked_mul(shard.get().div_ceil(inner.get()))
            })?;
        let per_shard = subchunks.checked_mul(16)?.checked_add(DECODER_OVERHEAD)?;
        let capacity = usize::try_from(RESIDENT_BYTES / per_shard).ok()?;
        (capacity > 0).then(|| Self {
            resident: Mutex::new(HashMap::new()),
            capacity,
        })
    }

    /// Shards resident now.
    #[cfg(all(test, feature = "icechunk"))]
    pub(crate) fn len(&self) -> usize {
        self.resident
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// The resident shards' indices, sorted.
    #[cfg(all(test, feature = "icechunk"))]
    pub(crate) fn resident(&self) -> Vec<Vec<u64>> {
        let mut shards: Vec<_> = self
            .resident
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        shards.sort();
        shards
    }

    /// Load `shard`'s index ahead of its reads, unless it is resident.
    pub(crate) fn load(
        &self,
        array: &Array<EngineStore>,
        shard: &[u64],
        options: &CodecOptions,
    ) -> Result<(), DataServerError> {
        self.decoder(array, shard, options).map(drop)
    }

    /// Decode `source`, a subset within one shard, with that shard's resident
    /// decoder: upstream's `retrieve_chunk_subset_opt` with the index kept.
    /// `None` when `source` spans shards or is a whole shard, which upstream
    /// reads in one request; the caller takes the ordinary path then.
    pub(crate) fn read(
        &self,
        array: &Array<EngineStore>,
        source: &ArraySubset,
        options: &CodecOptions,
    ) -> Result<Option<Vec<u8>>, DataServerError> {
        let read_error = crate::catalog::chunk_read_error;
        let grid_error = |e: &dyn std::fmt::Display| {
            DataServerError::Engine(format!("Zarr decoded chunk read: {e}"))
        };
        let Some(shards) = array
            .chunks_in_array_subset(source)
            .map_err(|e| grid_error(&e))?
        else {
            return Ok(None);
        };
        if shards.num_elements() != 1 {
            return Ok(None);
        }
        let shard = shards.start();
        let region = array.chunk_subset(shard).map_err(|e| grid_error(&e))?;
        if region == *source {
            return Ok(None);
        }
        let within = source
            .relative_to(region.start())
            .map_err(|e| grid_error(&e))?;
        let decoder = self.decoder(array, shard, options)?;
        deadline::check()?;
        let result = decoder.partial_decode(&within, options).and_then(|bytes| {
            bytes.validate(within.num_elements(), array.data_type())?;
            Ok(bytes.into_owned())
        });
        // Preserve the typed deadline even when the codec erased the storage error.
        deadline::check()?;
        let bytes = result
            .map_err(|e| read_error(e.into()))?
            .into_fixed()
            .map_err(|e| grid_error(&e))?
            .into_owned();
        Ok(Some(bytes))
    }

    /// `shard`'s resident decoder, or a new one, which reads its index and
    /// stays resident while the allowance lasts. Never holds the lock across
    /// that read: concurrent first reads of one shard each fetch its index.
    fn decoder(
        &self,
        array: &Array<EngineStore>,
        shard: &[u64],
        options: &CodecOptions,
    ) -> Result<Decoder, DataServerError> {
        if let Some(decoder) = self
            .resident
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(shard)
        {
            return Ok(decoder.clone());
        }
        deadline::check()?;
        let decoder = array.partial_decoder_opt(shard, options);
        deadline::check()?;
        let decoder = decoder.map_err(crate::catalog::chunk_read_error)?;
        let mut resident = self.resident.lock().unwrap_or_else(|e| e.into_inner());
        if resident.len() < self.capacity {
            return Ok(resident.entry(shard.to_vec()).or_insert(decoder).clone());
        }
        Ok(decoder)
    }
}
