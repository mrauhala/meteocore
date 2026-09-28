use super::*;

/// Decode exact byte ranges from one checked frame per call with c-blosc
/// getitem. Validating and admitting the frame per call avoids sharing guards
/// between concurrent users or retaining scratch for the lifetime of a
/// reusable partial decoder.
///
/// This stands in for zarrs' `BloscPartialDecoder`, which divides byte
/// offsets and lengths by the frame's typesize and so returns the wrong bytes
/// for unaligned ranges of opaque intermediate streams (#780; zarrs 0.23.14,
/// unchanged on its main branch). Delegate to it again once upstream returns
/// exact ranges, including the final partial element.
pub(crate) struct PartialDecoder<T: ?Sized> {
    input: Arc<T>,
    codec: Arc<dyn BytesToBytesCodecTraits>,
    representation: BytesRepresentation,
}

impl<T: ?Sized> PartialDecoder<T> {
    pub(crate) fn new(
        input: Arc<T>,
        codec: Arc<dyn BytesToBytesCodecTraits>,
        representation: BytesRepresentation,
    ) -> Self {
        Self {
            input,
            codec,
            representation,
        }
    }

    // getitem addresses whole typesize elements: expand each range to its
    // covering elements and trim the owned output in place. Aligned ranges
    // make the same getitem call as upstream; rounding adds under two elements
    // of output capacity and never passes the admitted frame length. A frame
    // whose length is not a multiple of typesize ends in a partial element
    // getitem cannot index. Ranges reaching it slice a single full decode of
    // the admitted output per call, and the last exact whole-frame request
    // takes that buffer after every other slice is copied, so a batch holds no
    // extra frame-sized copy. Its serial scratch fits the admitted getitem
    // allowance. Every range is validated before any native call.
    fn decode_ranges(
        &self,
        encoded: &[u8],
        frame: &Frame,
        regions: ByteRangeIterator<'_>,
        options: &CodecOptions,
    ) -> Result<Option<Vec<ArrayBytesRaw<'static>>>, CodecError> {
        let typesize = frame.typesize as usize;
        let length = frame.length as usize;
        let ranges = regions
            .map(|range| {
                frame.bounds(range).ok_or_else(|| {
                    CodecError::Other("Blosc decoded byte range is outside the frame".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let reaches_tail = |&(start, end): &(usize, usize)| {
            start < end && end.div_ceil(typesize) * typesize > length
        };
        let mut full = None;
        if ranges.iter().any(reaches_tail) {
            let decoded = self
                .codec
                .decode(Cow::Borrowed(encoded), &self.representation, options)?
                .into_owned();
            if decoded.len() != length {
                return Err(CodecError::Other(
                    "Blosc output does not match its frame".into(),
                ));
            }
            full = Some(decoded);
        }
        let owner = full
            .as_ref()
            .and_then(|_| ranges.iter().rposition(|&range| range == (0, length)));
        let mut values = Vec::with_capacity(ranges.len());
        for (index, &(start, end)) in ranges.iter().enumerate() {
            let (first, last) = (
                start / typesize * typesize,
                end.div_ceil(typesize) * typesize,
            );
            let value = match &full {
                // getitem reports zero items as a failure; the owner's
                // placeholder receives the decoded buffer below.
                _ if start == end || Some(index) == owner => Vec::new(),
                _ if last <= length => {
                    let mut value =
                        blosc_decompress_bytes_partial(encoded, first, last - first, typesize)
                            .map_err(|error| CodecError::Other(error.to_string()))?;
                    value.truncate(end - first);
                    value.drain(..start - first);
                    value
                }
                Some(decoded) => decoded[start..end].to_vec(),
                None => unreachable!("ranges reaching the partial element decode the frame"),
            };
            values.push(value);
        }
        if let (Some(index), Some(decoded)) = (owner, full) {
            values[index] = decoded;
        }
        check_deadline()?;
        Ok(Some(values.into_iter().map(Cow::Owned).collect()))
    }
}

impl BytesPartialDecoderTraits for PartialDecoder<dyn BytesPartialDecoderTraits> {
    fn exists(&self) -> Result<bool, StorageError> {
        self.input.exists()
    }

    fn size_held(&self) -> usize {
        self.input.size_held()
    }

    fn supports_partial_decode(&self) -> bool {
        true
    }

    fn partial_decode_many(
        &self,
        regions: ByteRangeIterator,
        options: &CodecOptions,
    ) -> Result<Option<Vec<ArrayBytesRaw<'_>>>, CodecError> {
        check_deadline()?;
        let context = crate::encoded::current();
        let Some(encoded) = self.input.decode(options)? else {
            return Ok(None);
        };
        // Scratch is admitted once the frame is available and held until
        // every native call below has returned or unwound.
        let frame = validate_and_admit(&encoded, &self.representation, true, context.as_ref())?;
        self.decode_ranges(&encoded, &frame, regions, options)
    }
}

#[cfg(feature = "icechunk")]
#[async_trait::async_trait]
impl AsyncBytesPartialDecoderTraits for PartialDecoder<dyn AsyncBytesPartialDecoderTraits> {
    async fn exists(&self) -> Result<bool, StorageError> {
        self.input.exists().await
    }

    fn size_held(&self) -> usize {
        self.input.size_held()
    }

    fn supports_partial_decode(&self) -> bool {
        true
    }

    async fn partial_decode_many<'a>(
        &'a self,
        regions: ByteRangeIterator<'a>,
        options: &CodecOptions,
    ) -> Result<Option<Vec<ArrayBytesRaw<'a>>>, CodecError> {
        check_deadline()?;
        // A pending call keeps the scope it started in, even after the caller
        // leaves that scope; cancellation drops it with the future.
        let context = crate::encoded::current();
        let Some(encoded) = self.input.decode(options).await? else {
            return Ok(None);
        };
        let frame = validate_and_admit(&encoded, &self.representation, true, context.as_ref())?;
        self.decode_ranges(&encoded, &frame, regions, options)
    }
}
