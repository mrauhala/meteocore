//! Preflight Blosc frames without replacing its block-level partial decoder.
use super::*;
use zarrs::{
    array::codec::bytes_to_bytes::blosc::blosc_validate,
    storage::{
        byte_range::{extract_byte_ranges, ByteRangeIterator},
        StorageError,
    },
};

pub(super) fn validate_and_admit(
    bytes: &[u8],
    representation: &BytesRepresentation,
    partial: bool,
) -> Result<(), CodecError> {
    check_deadline()?;
    let limit = representation.size().ok_or_else(|| {
        CodecError::Other("Blosc decoding requires a bounded representation".into())
    })?;
    // Upstream validation checks the 16-byte header, format version, encoded
    // length and maximum output size without allocating a decode destination.
    let length =
        blosc_validate(bytes).ok_or_else(|| CodecError::Other("invalid Blosc frame".into()))?;
    if length as u64 > limit
        || (matches!(representation, BytesRepresentation::FixedSize(_)) && length as u64 != limit)
    {
        return Err(CodecError::Other(
            "Blosc output does not match its declared representation".into(),
        ));
    }
    // c-blosc 1's header stores a little-endian block size at bytes 8..12.
    // Bound it before getitem/full decoding can allocate native scratch.
    let block = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as u64;
    let typesize = u64::from(bytes[3]);
    // c-blosc's BLOSC_MAX_BLOCKSIZE ensures 3*block + 4*typesize fits int32.
    const MAX_BLOCK: u64 = (i32::MAX as u64 - 4 * 255) / 3;
    if block == 0 || block > length as u64 || block > MAX_BLOCK || typesize == 0 {
        return Err(CodecError::Other("invalid Blosc block/type size".into()));
    }
    if let Some(context) = crate::encoded::current() {
        // serial_blosc uses 2*block + 4*typesize; getitem uses 3*block +
        // 4*typesize, independently of how few values the request selects.
        let scratch = block * if partial { 3 } else { 2 } + 4 * typesize;
        context.codec_scratch(scratch).map_err(io_error)?;
        if matches!(representation, BytesRepresentation::BoundedSize(_)) {
            context.intermediate(length).map_err(io_error)?;
        }
    }
    Ok(())
}

pub(super) struct CheckedInput<T: ?Sized> {
    inner: Arc<T>,
    representation: BytesRepresentation,
}

impl<T: ?Sized> CheckedInput<T> {
    pub(super) fn new(inner: Arc<T>, representation: BytesRepresentation) -> Self {
        Self {
            inner,
            representation,
        }
    }
}

impl BytesPartialDecoderTraits for CheckedInput<dyn BytesPartialDecoderTraits> {
    fn exists(&self) -> Result<bool, StorageError> {
        self.inner.exists()
    }

    fn size_held(&self) -> usize {
        self.inner.size_held()
    }

    fn supports_partial_decode(&self) -> bool {
        false
    }

    fn decode(&self, options: &CodecOptions) -> Result<Option<ArrayBytesRaw<'_>>, CodecError> {
        check_deadline()?;
        let value = self.inner.decode(options)?;
        if let Some(bytes) = &value {
            validate_and_admit(bytes, &self.representation, true)?;
        }
        Ok(value)
    }

    fn partial_decode_many(
        &self,
        regions: ByteRangeIterator,
        options: &CodecOptions,
    ) -> Result<Option<Vec<ArrayBytesRaw<'_>>>, CodecError> {
        self.decode(options)?
            .map(|bytes| {
                extract_byte_ranges(&bytes, regions)
                    .map(|ranges| ranges.into_iter().map(Cow::Owned).collect())
                    .map_err(CodecError::from)
            })
            .transpose()
    }
}

#[cfg(feature = "icechunk")]
use zarrs::array::codec::api::AsyncBytesPartialDecoderTraits;

#[cfg(feature = "icechunk")]
#[async_trait::async_trait]
impl AsyncBytesPartialDecoderTraits for CheckedInput<dyn AsyncBytesPartialDecoderTraits> {
    async fn exists(&self) -> Result<bool, StorageError> {
        self.inner.exists().await
    }

    fn size_held(&self) -> usize {
        self.inner.size_held()
    }

    fn supports_partial_decode(&self) -> bool {
        false
    }

    async fn decode<'a>(
        &'a self,
        options: &CodecOptions,
    ) -> Result<Option<ArrayBytesRaw<'a>>, CodecError> {
        check_deadline()?;
        let value = self.inner.decode(options).await?;
        if let Some(bytes) = &value {
            validate_and_admit(bytes, &self.representation, true)?;
        }
        Ok(value)
    }

    async fn partial_decode_many<'a>(
        &'a self,
        regions: ByteRangeIterator<'a>,
        options: &CodecOptions,
    ) -> Result<Option<Vec<ArrayBytesRaw<'a>>>, CodecError> {
        self.decode(options)
            .await?
            .map(|bytes| {
                extract_byte_ranges(&bytes, regions)
                    .map(|ranges| ranges.into_iter().map(Cow::Owned).collect())
                    .map_err(CodecError::from)
            })
            .transpose()
    }
}
