//! Bound compressed output by the codec chain's decoded representation.
//!
//! zarrs 0.23 ignores that representation in gzip/zstd/Blosc decoders. Keep
//! its metadata, encoding and shard machinery, bounding decompression.
use std::{borrow::Cow, io::Read, sync::Arc};

use ds_core::{deadline, error::DataServerError};
use zarrs::{
    array::{
        codec::{
            api::{
                BytesPartialDecoderTraits, BytesToBytesCodecTraits, CodecError,
                CodecMetadataOptions, CodecOptions, CodecPartialDefault, CodecTraits,
                PartialDecoderCapability, PartialEncoderCapability, RecommendedConcurrency,
            },
            BloscCodec, CodecChain, GzipCodec, ShardingCodec, ZstdCodec,
        },
        Array, ArrayBytesRaw, BytesRepresentation,
    },
    metadata::{Configuration, ConfigurationSerialize},
    metadata_ext::codec::sharding::ShardingCodecConfiguration,
    plugin::{ExtensionName, ZarrVersion},
};

use crate::store::EngineStore;

mod blosc;
pub(crate) mod headroom;

pub(crate) fn bounded_array(
    array: Array<EngineStore>,
) -> Result<Array<EngineStore>, DataServerError> {
    let Some(codecs) = bounded_chain(&array.codecs()).map_err(error)? else {
        return Ok(array);
    };
    // Rebuild in memory only: retain chunk keys, attributes, dimensions and
    // storage transformers, including those of arrays originally opened as V2.
    array
        .builder()
        .array_to_bytes_codec(codecs.array_to_bytes_codec().clone())
        .bytes_to_bytes_codecs(codecs.bytes_to_bytes_codecs().to_vec())
        .build(array.storage(), array.path().as_str())
        .map_err(error)
}

fn bounded_chain(chain: &CodecChain) -> Result<Option<CodecChain>, CodecError> {
    let mut changed = false;
    let mut array_codec = chain.array_to_bytes_codec().clone();
    if array_codec.as_any().is::<ShardingCodec>() {
        let config = array_codec
            .configuration_v3(&CodecMetadataOptions::default())
            .ok_or_else(|| CodecError::Other("missing shard configuration".into()))?;
        let ShardingCodecConfiguration::V1(config) =
            ShardingCodecConfiguration::try_from_configuration(config).map_err(codec_error)?
        else {
            return Err(CodecError::Other("unsupported shard configuration".into()));
        };
        let inner = CodecChain::from_metadata(&config.codecs).map_err(codec_error)?;
        let index = CodecChain::from_metadata(&config.index_codecs).map_err(codec_error)?;
        let bounded_inner = bounded_chain(&inner)?;
        let bounded_index = bounded_chain(&index)?;
        if bounded_inner.is_some() || bounded_index.is_some() {
            // Sharding's runtime option controls write order only. This engine
            // opens read-only arrays and keeps upstream partial-range decoding.
            array_codec = Arc::new(ShardingCodec::new(
                config.chunk_shape,
                Arc::new(bounded_inner.unwrap_or(inner)),
                Arc::new(bounded_index.unwrap_or(index)),
                config.index_location,
            ));
            changed = true;
        }
    }
    let bytes_codecs = chain
        .bytes_to_bytes_codecs()
        .iter()
        .map(|codec| {
            let kind = if codec.as_any().is::<GzipCodec>() {
                Kind::Gzip
            } else if codec.as_any().is::<ZstdCodec>() {
                Kind::Zstd
            } else if codec.as_any().is::<BloscCodec>() {
                Kind::Blosc
            } else {
                return codec.clone();
            };
            changed = true;
            Arc::new(BoundedCodec {
                inner: codec.clone(),
                kind,
            }) as Arc<dyn BytesToBytesCodecTraits>
        })
        .collect();
    Ok(changed.then(|| {
        CodecChain::new(
            chain.array_to_array_codecs().to_vec(),
            array_codec,
            bytes_codecs,
        )
    }))
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Gzip,
    Zstd,
    Blosc,
}

#[derive(Debug)]
struct BoundedCodec {
    inner: Arc<dyn BytesToBytesCodecTraits>,
    kind: Kind,
}

impl ExtensionName for BoundedCodec {
    fn name(&self, version: ZarrVersion) -> Option<Cow<'static, str>> {
        self.inner.name(version)
    }
}

impl CodecTraits for BoundedCodec {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn configuration(
        &self,
        version: ZarrVersion,
        options: &CodecMetadataOptions,
    ) -> Option<Configuration> {
        self.inner.configuration(version, options)
    }

    fn partial_decoder_capability(&self) -> PartialDecoderCapability {
        self.inner.partial_decoder_capability()
    }

    fn partial_encoder_capability(&self) -> PartialEncoderCapability {
        self.inner.partial_encoder_capability()
    }
}

#[cfg_attr(feature = "icechunk", async_trait::async_trait)]
impl BytesToBytesCodecTraits for BoundedCodec {
    fn into_dyn(self: Arc<Self>) -> Arc<dyn BytesToBytesCodecTraits> {
        self
    }

    fn recommended_concurrency(
        &self,
        representation: &BytesRepresentation,
    ) -> Result<RecommendedConcurrency, CodecError> {
        self.inner.recommended_concurrency(representation)
    }

    fn encoded_representation(&self, representation: &BytesRepresentation) -> BytesRepresentation {
        self.inner.encoded_representation(representation)
    }

    fn encode<'a>(
        &self,
        bytes: ArrayBytesRaw<'a>,
        options: &CodecOptions,
    ) -> Result<ArrayBytesRaw<'a>, CodecError> {
        self.inner.encode(bytes, options)
    }

    fn decode<'a>(
        &self,
        bytes: ArrayBytesRaw<'a>,
        representation: &BytesRepresentation,
        options: &CodecOptions,
    ) -> Result<ArrayBytesRaw<'a>, CodecError> {
        check_deadline()?;
        let limit = representation.size().ok_or_else(|| {
            CodecError::Other("compressed decoding requires a bounded representation".into())
        })?;
        let limit = usize::try_from(limit).map_err(|_| exhausted())?;
        let intermediate = matches!(representation, BytesRepresentation::BoundedSize(_));
        let output = match self.kind {
            Kind::Blosc => {
                blosc::validate_and_admit(&bytes, representation, false)?;
                self.inner
                    .decode(bytes, representation, options)?
                    .into_owned()
            }
            Kind::Gzip => {
                // Keep the upstream single-member gzip semantics. Probe EOF
                // after filling the destination to validate trailers/checksums
                // and detect excess output without growing past the limit.
                let decoder = flate2::bufread::GzDecoder::new(bytes.as_ref());
                read_bounded(decoder, limit, intermediate)?
            }
            Kind::Zstd => {
                // Single-pass decoding never allocates the frame-sized history
                // window used by streaming zstd. Treat frame headers as hints
                // bounded by the independently computed codec representation.
                let capacity = zstd::bulk::Decompressor::upper_bound(&bytes)
                    .unwrap_or(limit)
                    .min(limit);
                let mut output = Vec::new();
                grow(&mut output, capacity, intermediate)?;
                let length = zstd::bulk::Decompressor::new()?
                    .decompress_to_buffer(&bytes, output.as_mut_slice())?;
                output.truncate(length);
                output
            }
        };
        check_deadline()?;
        if matches!(representation, BytesRepresentation::FixedSize(_)) && output.len() != limit {
            return Err(CodecError::Other(
                "decoded payload does not match its fixed size".into(),
            ));
        }
        Ok(Cow::Owned(output))
    }

    fn partial_decoder(
        self: Arc<Self>,
        input: Arc<dyn BytesPartialDecoderTraits>,
        representation: &BytesRepresentation,
        options: &CodecOptions,
    ) -> Result<Arc<dyn BytesPartialDecoderTraits>, CodecError> {
        if matches!(self.kind, Kind::Blosc) {
            // Validate the full encoded frame before upstream getitem sees it.
            // Keep block-level partial decoding; a full-decode fallback would
            // amplify small map/position reads.
            self.inner.clone().partial_decoder(
                Arc::new(blosc::CheckedInput::new(input, *representation)),
                representation,
                options,
            )
        } else {
            Ok(Arc::new(CodecPartialDefault::new_bytes(
                input,
                *representation,
                self.into_dyn(),
            )))
        }
    }

    #[cfg(feature = "icechunk")]
    async fn async_partial_decoder(
        self: Arc<Self>,
        input: Arc<dyn zarrs::array::codec::api::AsyncBytesPartialDecoderTraits>,
        representation: &BytesRepresentation,
        options: &CodecOptions,
    ) -> Result<Arc<dyn zarrs::array::codec::api::AsyncBytesPartialDecoderTraits>, CodecError> {
        if matches!(self.kind, Kind::Blosc) {
            self.inner
                .clone()
                .async_partial_decoder(
                    Arc::new(blosc::CheckedInput::new(input, *representation)),
                    representation,
                    options,
                )
                .await
        } else {
            Ok(Arc::new(CodecPartialDefault::new_bytes(
                input,
                *representation,
                self.into_dyn(),
            )))
        }
    }
}

fn grow(output: &mut Vec<u8>, length: usize, intermediate: bool) -> Result<(), CodecError> {
    let additional = length - output.len();
    if intermediate && additional != 0 {
        if let Some(context) = crate::encoded::current() {
            context.intermediate(additional).map_err(io_error)?;
        }
    }
    output
        .try_reserve_exact(additional)
        .map_err(|_| exhausted())?;
    output.resize(length, 0);
    Ok(())
}

fn read_bounded(
    mut reader: impl Read,
    limit: usize,
    intermediate: bool,
) -> Result<Vec<u8>, CodecError> {
    const BLOCK: usize = 64 * 1024;
    let mut output = Vec::new();
    // Fixed-size native outputs were already admitted by read_budget. A
    // bounded intermediate may be a very loose shard estimate; grow it only
    // as needed, admitting capacity before each allocation.
    grow(
        &mut output,
        if intermediate {
            limit.min(BLOCK)
        } else {
            limit
        },
        intermediate,
    )?;
    let mut filled = 0;
    loop {
        check_deadline()?;
        if filled == output.len() {
            let mut probe = [0];
            if reader.read(&mut probe)? == 0 {
                break;
            }
            if filled == limit {
                return Err(CodecError::Other(
                    "decoded payload exceeds its declared bound".into(),
                ));
            }
            let length = output.len().saturating_mul(2).max(1).min(limit);
            grow(&mut output, length, intermediate)?;
            output[filled] = probe[0];
            filled += 1;
            continue;
        }
        let end = filled.saturating_add(BLOCK).min(output.len());
        let read = reader.read(&mut output[filled..end])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    output.truncate(filled);
    Ok(output)
}

fn check_deadline() -> Result<(), CodecError> {
    deadline::check().map_err(io_error)
}

fn io_error(error: DataServerError) -> CodecError {
    std::io::Error::other(error).into()
}

fn exhausted() -> CodecError {
    io_error(DataServerError::ResourceExhausted)
}

fn codec_error(error: impl std::fmt::Display) -> CodecError {
    CodecError::Other(error.to_string())
}

fn error(error: impl std::fmt::Display) -> DataServerError {
    DataServerError::Engine(format!("configure bounded Zarr codecs: {error}"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod blosc_tests;
