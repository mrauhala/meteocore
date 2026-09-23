//! Metadata estimates for the encoded/codec allowances of one cold chunk.
//! These are admission hints, not new frame limits. Storage and codecs still
//! admit actual sizes, including representations larger than an encoder bound.
use super::*;
use zarrs::array::{
    codec::{BytesCodec, Crc32cCodec, TransposeCodec},
    ArrayShardedExt, ArraySubset,
};

pub(crate) enum Plan {
    Chunk(Chain),
    Shard {
        shape: Vec<u64>,
        inner: Chain,
        index: Chain,
    },
    Unknown,
}

pub(crate) struct Chain(Vec<Step>);

struct Step {
    codec: Arc<dyn BytesToBytesCodecTraits>,
    compressed: bool,
    blosc: bool,
}

impl Plan {
    /// Parse once with the catalog, rather than rebuilding codecs per tile.
    pub(crate) fn new(array: &Array<EngineStore>) -> Self {
        let chain = array.codecs();
        if array.is_exclusively_sharded() {
            let shard = (|| {
                let config = chain
                    .array_to_bytes_codec()
                    .configuration_v3(&CodecMetadataOptions::default())?;
                let ShardingCodecConfiguration::V1(config) =
                    ShardingCodecConfiguration::try_from_configuration(config).ok()?
                else {
                    return None;
                };
                Some(Self::Shard {
                    shape: config.chunk_shape.iter().map(|n| n.get()).collect(),
                    inner: Chain::new(&CodecChain::from_metadata(&config.codecs).ok()?)?,
                    index: Chain::new(&CodecChain::from_metadata(&config.index_codecs).ok()?)?,
                })
            })();
            shard.unwrap_or(Self::Unknown)
        } else {
            Chain::new(&chain).map_or(Self::Unknown, Self::Chunk)
        }
    }

    pub(crate) fn bytes(&self, array: &Array<EngineStore>, subset: &ArraySubset) -> Option<u64> {
        let element = array.data_type().fixed_size()? as u64;
        let chunks = array.chunks_in_array_subset(subset).ok()??;
        let mut total = 0u64;
        for indices in chunks.indices() {
            deadline::check().ok()?;
            let outer = array.chunk_shape(&indices).ok()?;
            let current = match self {
                Self::Chunk(chain) => chain.bytes(
                    outer
                        .iter()
                        .try_fold(element, |n, d| n.checked_mul(d.get()))?,
                )?,
                Self::Shard {
                    shape,
                    inner,
                    index,
                } => {
                    let native = shape.iter().try_fold(element, |n, &d| n.checked_mul(d))?;
                    let index_bytes = outer
                        .iter()
                        .zip(shape)
                        .try_fold(16u64, |n, (o, &i)| n.checked_mul(o.get().div_ceil(i)))?;
                    inner
                        .bytes(native)?
                        .checked_add(index.bytes(index_bytes)?)?
                }
                Self::Unknown => return None,
            };
            total = total.checked_add(current)?;
        }
        Some(total)
    }
}

impl Chain {
    fn new(chain: &CodecChain) -> Option<Self> {
        // Transpose and endian conversion preserve byte counts. Nested shards
        // and other transforms retain serial, actual-size admission for now.
        if !chain.array_to_bytes_codec().as_any().is::<BytesCodec>()
            || chain
                .array_to_array_codecs()
                .iter()
                .any(|c| !c.as_any().is::<TransposeCodec>())
        {
            return None;
        }
        let steps = chain
            .bytes_to_bytes_codecs()
            .iter()
            .map(|codec| {
                let any = codec.as_any();
                let bounded = any.downcast_ref::<BoundedCodec>();
                let blosc = any.is::<BloscCodec>()
                    || bounded.is_some_and(|c| matches!(c.kind, Kind::Blosc));
                let compressed =
                    blosc || any.is::<GzipCodec>() || any.is::<ZstdCodec>() || bounded.is_some();
                (compressed || any.is::<Crc32cCodec>()).then(|| Step {
                    codec: codec.clone(),
                    compressed,
                    blosc,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self(steps))
    }

    fn bytes(&self, native: u64) -> Option<u64> {
        let mut representation = BytesRepresentation::FixedSize(native);
        let mut codec_bytes = 0u64;
        for step in &self.0 {
            let size = representation.size()?;
            // The recognized encoder bounds use small additions/multipliers.
            // Guard their unchecked arithmetic before calling upstream code.
            size.checked_mul(4)?.checked_add(1024)?;
            if step.compressed && matches!(representation, BytesRepresentation::BoundedSize(_)) {
                codec_bytes = codec_bytes.checked_add(size.checked_mul(2)?)?;
            }
            if step.blosc {
                // The validated block cannot exceed decoded length; a frame's
                // typesize is one byte. Cover getitem as well as full decode.
                codec_bytes =
                    codec_bytes.checked_add(size.checked_mul(3)?.checked_add(4 * 255)?)?;
            }
            representation = step.codec.encoded_representation(&representation);
        }
        // The collected body, encoded copy, and intermediates remain admitted
        // through retrieval. Scratch credit can be reused after each native
        // call; summing its bounds here remains a conservative prepayment.
        codec_bytes.checked_add(representation.size()?.checked_mul(2)?)
    }
}

#[cfg(test)]
mod tests;
