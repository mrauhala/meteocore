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
//!
//! The decoder does not expose that index, so each shard also keeps its own
//! copy, decoded from the bytes building the decoder read ([`IndexTap`]):
//! the byte range of each inner chunk, which a query fetches ahead of its
//! decodes (`Catalog::prefetch`).
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ds_core::{deadline, error::DataServerError};
use zarrs::array::{
    codec::{
        api::{
            ArrayPartialDecoderTraits, ArrayToBytesCodecTraits, BytesPartialDecoderTraits,
            CodecError, CodecMetadataOptions,
        },
        BloscCodec, CodecChain, GzipCodec, ShardingCodec,
    },
    Array, ArrayBytesRaw, ArrayError, ArrayShardedExt, ArraySubset, CodecOptions,
    StoragePartialDecoder,
};
use zarrs::metadata::ConfigurationSerialize;
use zarrs::metadata_ext::codec::sharding::ShardingCodecConfiguration;
use zarrs::storage::{byte_range::ByteRangeIterator, StorageError, StorageHandle};

use crate::store::EngineStore;

/// The resident indexes of one variable, by estimated size: offset and
/// length per inner chunk twice, the decoder's and ours, plus decoder
/// overhead. Shards past it are read as before, their index fetched for
/// each cold read. Twice what one copy had, so as many shards stay.
const RESIDENT_BYTES: u64 = 512 * 1024;
const DECODER_OVERHEAD: u64 = 256;

type Decoder = Arc<dyn ArrayPartialDecoderTraits>;

struct Shard {
    decoder: Decoder,
    /// `(offset, length)` pairs in index order; `None` for an absent shard,
    /// whose inner chunks are all fill.
    index: Option<Vec<u64>>,
}

pub(crate) struct ShardIndexes {
    resident: Mutex<HashMap<Vec<u64>, Shard>>,
    /// Most shards kept: the first ones loaded, the default run's first.
    capacity: usize,
    /// Inner chunks per shard along each axis.
    per_shard: Vec<NonZeroU64>,
    /// Whether a decode reads each inner chunk as its one indexed range:
    /// the inner chain's outermost codec decodes whole frames.
    ranged: bool,
}

/// One inner chunk's encoded bytes: `length` bytes at `offset` in the
/// object of `shard`.
pub(crate) struct InnerRange {
    pub(crate) shard: Vec<u64>,
    pub(crate) offset: u64,
    pub(crate) length: u64,
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
        let per_shard: Vec<NonZeroU64> = shard
            .iter()
            .zip(inner.iter())
            .map(|(shard, inner)| NonZeroU64::new(shard.get().div_ceil(inner.get())))
            .collect::<Option<_>>()?;
        let subchunks = per_shard
            .iter()
            .try_fold(1u64, |n, per| n.checked_mul(per.get()))?;
        let per_shard_bytes = subchunks.checked_mul(32)?.checked_add(DECODER_OVERHEAD)?;
        let capacity = usize::try_from(RESIDENT_BYTES / per_shard_bytes).ok()?;
        (capacity > 0).then(|| Self {
            resident: Mutex::new(HashMap::new()),
            capacity,
            per_shard,
            ranged: whole_frames(array),
        })
    }

    /// Where `inner`, indices on the array's inner-chunk grid, lies in its
    /// shard, from that shard's resident index. `None` when the shard is not
    /// resident, the inner chunk is fill, or a decode of it reads more than
    /// that one range.
    pub(crate) fn inner_range(&self, inner: &[u64]) -> Option<InnerRange> {
        if !self.ranged || inner.len() != self.per_shard.len() {
            return None;
        }
        let shard: Vec<u64> = inner
            .iter()
            .zip(&self.per_shard)
            .map(|(i, per)| i / per.get())
            .collect();
        let entry = inner
            .iter()
            .zip(&self.per_shard)
            .try_fold(0u64, |entry, (i, per)| {
                entry.checked_mul(per.get())?.checked_add(i % per.get())
            })?;
        let entry = usize::try_from(entry).ok()?.checked_mul(2)?;
        let resident = self.resident.lock().unwrap_or_else(|e| e.into_inner());
        let index = resident.get(&shard)?.index.as_ref()?;
        let (&offset, &length) = (index.get(entry)?, index.get(entry + 1)?);
        // u64::MAX for both marks an inner chunk that is all fill.
        (offset != u64::MAX && length != u64::MAX && length > 0).then_some(InnerRange {
            shard,
            offset,
            length,
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
        if let Some(resident) = self
            .resident
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(shard)
        {
            return Ok(resident.decoder.clone());
        }
        deadline::check()?;
        let built = self.build(array, shard, options);
        deadline::check()?;
        let (decoder, index) = built.map_err(crate::catalog::chunk_read_error)?;
        let mut resident = self.resident.lock().unwrap_or_else(|e| e.into_inner());
        if resident.len() < self.capacity {
            let kept = resident
                .entry(shard.to_vec())
                .or_insert(Shard { decoder, index });
            return Ok(kept.decoder.clone());
        }
        Ok(decoder)
    }

    /// Upstream's `Array::partial_decoder_opt`, its input tapped for the
    /// index the build reads, which is then decoded with the array's own
    /// shard index codecs. A tapped read that is not an index decodes to an
    /// error and keeps no index: its inner chunks are then only read, never
    /// fetched ahead.
    fn build(
        &self,
        array: &Array<EngineStore>,
        shard: &[u64],
        options: &CodecOptions,
    ) -> Result<(Decoder, Option<Vec<u64>>), ArrayError> {
        let storage = array
            .storage_transformers()
            .create_readable_transformer(Arc::new(StorageHandle::new(array.storage())))?;
        let tap = Arc::new(IndexTap {
            input: Arc::new(StoragePartialDecoder::new(storage, array.chunk_key(shard))),
            armed: AtomicBool::new(true),
            read: Mutex::new(None),
        });
        let decoder = array.codecs().partial_decoder(
            tap.clone(),
            &array.chunk_shape(shard)?,
            array.data_type(),
            array.fill_value(),
            options,
        )?;
        let read = tap.read.lock().unwrap_or_else(|e| e.into_inner()).take();
        let index = read.and_then(|bytes| {
            array
                .codecs()
                .array_to_bytes_codec()
                .as_any()
                .downcast_ref::<ShardingCodec>()?
                .decode_index(&bytes, &self.per_shard, options)
                .ok()
        });
        Ok((decoder, index))
    }
}

/// A shard's input that keeps what the first read through it returns:
/// building a sharding decoder reads the shard index, and nothing else.
/// Later reads, of inner chunks, pass through untouched.
struct IndexTap {
    input: Arc<dyn BytesPartialDecoderTraits>,
    armed: AtomicBool,
    read: Mutex<Option<Vec<u8>>>,
}

impl BytesPartialDecoderTraits for IndexTap {
    fn exists(&self) -> Result<bool, StorageError> {
        self.input.exists()
    }

    fn size_held(&self) -> usize {
        self.input.size_held()
    }

    fn partial_decode_many(
        &self,
        regions: ByteRangeIterator,
        options: &CodecOptions,
    ) -> Result<Option<Vec<ArrayBytesRaw<'_>>>, CodecError> {
        let result = self.input.partial_decode_many(regions, options)?;
        if self.armed.swap(false, Ordering::Relaxed) {
            if let Some([bytes]) = result.as_deref() {
                *self.read.lock().unwrap_or_else(|e| e.into_inner()) = Some(bytes.to_vec());
            }
        }
        Ok(result)
    }

    fn supports_partial_decode(&self) -> bool {
        self.input.supports_partial_decode()
    }
}

/// Whether the outermost codec of `array`'s inner chunks decodes whole
/// frames, reading its input once whatever part of the chunk is wanted:
/// Blosc (`codec_limits::blosc::PartialDecoder`), gzip and zstd
/// (`CodecPartialDefault`). Uncompressed or checksummed inner chunks are
/// read as the parts a decode wants, at other ranges.
fn whole_frames(array: &Array<EngineStore>) -> bool {
    (|| {
        let config = array
            .codecs()
            .array_to_bytes_codec()
            .configuration_v3(&CodecMetadataOptions::default())?;
        let ShardingCodecConfiguration::V1(config) =
            ShardingCodecConfiguration::try_from_configuration(config).ok()?
        else {
            return None;
        };
        let inner = CodecChain::from_metadata(&config.codecs).ok()?;
        let outermost = inner.bytes_to_bytes_codecs().last()?.as_any();
        Some(
            outermost.is::<BloscCodec>()
                || outermost.is::<GzipCodec>()
                || outermost.is::<crate::codec_limits::ZstdCodec>(),
        )
    })()
    .unwrap_or(false)
}
