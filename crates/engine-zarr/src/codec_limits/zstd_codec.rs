//! engine-zarr's `zstd` bytes-to-bytes codec, registered for Zarr V3
//! (`"name": "zstd"`) and V2 (`"id": "zstd"`).
//!
//! zarrs' own codec is behind its `zstd` feature, which pins the zstd 0.13
//! line (RUSTSEC-2026-0391 and RUSTSEC-2026-0395 to 0406, none patched in
//! 0.13). That feature stays off and this plugin takes the name. Metadata,
//! the encoder, the encoded-size bound and the partial capabilities match
//! zarrs 0.23's codec, so stores and fixtures round-trip unchanged.
//!
//! Decoding is the bounded zstd path in [`super`], the same one
//! [`super::BoundedCodec`] runs: output is capped by the codec representation,
//! intermediates are admitted to the read budget and the request deadline is
//! checked. Arrays outside `bounded_array`, such as coordinate variables,
//! therefore never run an unbounded zstd decode either.
use std::{borrow::Cow, sync::Arc};

use zarrs::{
    array::{
        codec::api::{
            BytesToBytesCodecTraits, Codec, CodecError, CodecMetadataOptions, CodecOptions,
            CodecPluginV2, CodecPluginV3, CodecTraits, CodecTraitsV2, CodecTraitsV3,
            PartialDecoderCapability, PartialEncoderCapability, RecommendedConcurrency,
        },
        ArrayBytesRaw, BytesRepresentation,
    },
    metadata::{v2::MetadataV2, v3::MetadataV3, Configuration},
    metadata_ext::codec::zstd::{ZstdCodecConfiguration, ZstdCodecConfigurationV1},
    plugin::{PluginCreateError, ZarrVersion},
};

/// The `zstd` codec: compression level and whether frames carry a checksum.
#[derive(Clone, Debug)]
pub(crate) struct ZstdCodec {
    level: i32,
    checksum: bool,
}

zarrs::plugin::impl_extension_aliases!(ZstdCodec, v3: "zstd", v2: "zstd");

inventory::submit! {
    CodecPluginV3::new::<ZstdCodec>()
}

inventory::submit! {
    CodecPluginV2::new::<ZstdCodec>()
}

impl ZstdCodec {
    pub(crate) const fn new(level: i32, checksum: bool) -> Self {
        Self { level, checksum }
    }

    fn new_with_configuration(
        configuration: &ZstdCodecConfiguration,
    ) -> Result<Self, PluginCreateError> {
        match configuration {
            ZstdCodecConfiguration::V1(configuration) => Ok(Self::new(
                configuration.level.into(),
                configuration.checksum,
            )),
            // numcodecs' level-only form: its frames carry no checksum.
            ZstdCodecConfiguration::Numcodecs(configuration) => {
                Ok(Self::new(configuration.level.into(), false))
            }
            _ => Err(PluginCreateError::Other(
                "this zstd codec configuration variant is unsupported".into(),
            )),
        }
    }
}

impl CodecTraitsV3 for ZstdCodec {
    fn create(metadata: &MetadataV3) -> Result<Codec, PluginCreateError> {
        let configuration: ZstdCodecConfigurationV1 = metadata.to_typed_configuration()?;
        Ok(Codec::BytesToBytes(Arc::new(Self::new(
            configuration.level.into(),
            configuration.checksum,
        ))))
    }
}

impl CodecTraitsV2 for ZstdCodec {
    fn create(metadata: &MetadataV2) -> Result<Codec, PluginCreateError> {
        // Both numcodecs' `{"level": ..}` and the form recent numcodecs and
        // zarr-python write with an additional `checksum`.
        let configuration: ZstdCodecConfiguration = metadata.to_typed_configuration()?;
        Ok(Codec::BytesToBytes(Arc::new(Self::new_with_configuration(
            &configuration,
        )?)))
    }
}

impl CodecTraits for ZstdCodec {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn configuration(
        &self,
        _version: ZarrVersion,
        _options: &CodecMetadataOptions,
    ) -> Option<Configuration> {
        Some(
            ZstdCodecConfiguration::V1(ZstdCodecConfigurationV1::new(
                self.level.into(),
                self.checksum,
            ))
            .into(),
        )
    }

    fn partial_decoder_capability(&self) -> PartialDecoderCapability {
        PartialDecoderCapability {
            partial_read: false,
            partial_decode: false,
        }
    }

    fn partial_encoder_capability(&self) -> PartialEncoderCapability {
        PartialEncoderCapability {
            partial_encode: false,
        }
    }
}

impl BytesToBytesCodecTraits for ZstdCodec {
    fn into_dyn(self: Arc<Self>) -> Arc<dyn BytesToBytesCodecTraits> {
        self
    }

    fn recommended_concurrency(
        &self,
        _representation: &BytesRepresentation,
    ) -> Result<RecommendedConcurrency, CodecError> {
        Ok(RecommendedConcurrency::new_maximum(1))
    }

    fn encode<'a>(
        &self,
        bytes: ArrayBytesRaw<'a>,
        _options: &CodecOptions,
    ) -> Result<ArrayBytesRaw<'a>, CodecError> {
        // The server only reads; tests and fixture generators write.
        let mut compressor = zstd::bulk::Compressor::new(self.level)?;
        compressor.include_checksum(self.checksum)?;
        Ok(Cow::Owned(compressor.compress(&bytes)?))
    }

    fn decode<'a>(
        &self,
        bytes: ArrayBytesRaw<'a>,
        representation: &BytesRepresentation,
        _options: &CodecOptions,
    ) -> Result<ArrayBytesRaw<'a>, CodecError> {
        super::bounded(bytes, representation, |bytes, limit, intermediate| {
            super::decode_zstd(&bytes, limit, intermediate)
        })
    }

    fn encoded_representation(&self, representation: &BytesRepresentation) -> BytesRepresentation {
        // zarrs 0.23's bound: frame header and trailer, plus a block header per
        // minimum window. headroom.rs sizes admission from it.
        const HEADER_TRAILER_OVERHEAD: u64 = 4 + 14 + 4;
        const MIN_WINDOW_SIZE: u64 = 1000;
        const BLOCK_OVERHEAD: u64 = 3;
        representation
            .size()
            .map_or(BytesRepresentation::UnboundedSize, |size| {
                BytesRepresentation::BoundedSize(
                    size + HEADER_TRAILER_OVERHEAD
                        + BLOCK_OVERHEAD * size.div_ceil(MIN_WINDOW_SIZE),
                )
            })
    }
}

#[cfg(test)]
mod tests;
