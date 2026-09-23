use super::*;

/// Wrap one upstream getitem invocation with scratch ownership. Constructing
/// its checked input per call avoids sharing guards between concurrent users
/// or retaining scratch for the lifetime of a reusable partial decoder.
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
}

// c-blosc 1 getitem returns before freeing scratch on out-of-bounds regions.
// Check each range after CheckedInput has validated the frame, but before the
// upstream iterator invokes getitem. Stop on error, then discard prior outputs.
fn checked_ranges<'a>(
    regions: ByteRangeIterator<'a>,
    call: &'a Mutex<Call>,
) -> ByteRangeIterator<'a> {
    Box::new(regions.take_while(move |range| call.lock().unwrap().check_range(*range)))
}

fn finish(
    result: Result<Option<Vec<ArrayBytesRaw<'_>>>, CodecError>,
    call: &Mutex<Call>,
) -> Result<Option<Vec<ArrayBytesRaw<'static>>>, CodecError> {
    check_deadline()?;
    if call.lock().unwrap().invalid_range {
        return Err(CodecError::Other(
            "Blosc decoded byte range is outside the frame".into(),
        ));
    }
    // Upstream getitem returns owned Vecs. Explicitly own the results before
    // dropping its temporary decoder; no additional payload copy is needed.
    result.map(|values| {
        values.map(|values| {
            values
                .into_iter()
                .map(|value| Cow::Owned(value.into_owned()))
                .collect()
        })
    })
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
        let call = Call::new();
        let decoder = self.codec.clone().partial_decoder(
            Arc::new(CheckedInput::new(
                self.input.clone(),
                self.representation,
                call.clone(),
            )),
            &self.representation,
            options,
        )?;
        finish(
            decoder.partial_decode_many(checked_ranges(regions, &call), options),
            &call,
        )
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
        let call = Call::new();
        let decoder = self
            .codec
            .clone()
            .async_partial_decoder(
                Arc::new(CheckedInput::new(
                    self.input.clone(),
                    self.representation,
                    call.clone(),
                )),
                &self.representation,
                options,
            )
            .await?;
        finish(
            decoder
                .partial_decode_many(checked_ranges(regions, &call), options)
                .await,
            &call,
        )
    }
}
