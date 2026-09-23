use super::*;
use zarrs::{
    array::codec::api::AsyncBytesPartialDecoderTraits,
    storage::{byte_range::ByteRangeIterator, StorageError},
};

struct GatedInput {
    bytes: Vec<u8>,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl AsyncBytesPartialDecoderTraits for GatedInput {
    async fn exists(&self) -> Result<bool, StorageError> {
        Ok(true)
    }

    fn size_held(&self) -> usize {
        self.bytes.len()
    }

    fn supports_partial_decode(&self) -> bool {
        false
    }

    async fn decode<'a>(
        &'a self,
        _options: &CodecOptions,
    ) -> Result<Option<ArrayBytesRaw<'a>>, CodecError> {
        self.release.notified().await;
        Ok(Some(Cow::Borrowed(&self.bytes)))
    }

    async fn partial_decode_many<'a>(
        &'a self,
        _regions: ByteRangeIterator<'a>,
        _options: &CodecOptions,
    ) -> Result<Option<Vec<ArrayBytesRaw<'a>>>, CodecError> {
        panic!("Blosc must fetch the complete encoded frame");
    }
}

#[tokio::test]
async fn pending_partial_calls_own_their_context_and_release_on_completion_or_cancel() {
    for outcome in ["success", "deadline", "cancel", "invalid range"] {
        let options = crate::catalog::single_threaded_opts();
        let codec = Arc::new(codec());
        let bytes = codec
            .encode(Cow::Owned(vec![0; 1024]), &options)
            .unwrap()
            .into_owned();
        let scratch = 3 * u64::from(u32::from_le_bytes(bytes[8..12].try_into().unwrap()))
            + 4 * u64::from(bytes[3]);
        let input = Arc::new(GatedInput {
            bytes,
            release: tokio::sync::Notify::new(),
        });
        let decoder = codec
            .async_partial_decoder(
                input.clone(),
                &BytesRepresentation::FixedSize(1024),
                &options,
            )
            .await
            .unwrap();
        let budget = Arc::new(crate::read_budget::Budget::new(128 + scratch));
        let permit = budget.reserve_bytes(128 + scratch).unwrap();
        let scope = crate::encoded::enter_prepaid(Some(budget.clone()), Some(permit), scratch);
        let regions = std::iter::once_with(|| {
            assert_eq!(budget.metrics().0, 128 + scratch);
            if outcome == "invalid range" {
                ByteRange::Suffix(1025)
            } else {
                ByteRange::FromStart(60, Some(12))
            }
        });
        let mut future = decoder.partial_decode_many(Box::new(regions), &options);
        assert!(futures::poll!(&mut future).is_pending());
        drop(scope);
        assert!(crate::encoded::current().is_none());
        assert_eq!(
            budget.metrics().0,
            128 + scratch,
            "pending call retains its owner"
        );
        if outcome == "cancel" {
            drop(future);
        } else {
            input.release.notify_one();
            let _deadline = deadline::enter((outcome == "deadline").then(std::time::Instant::now));
            match (outcome, future.await) {
                ("success", Ok(Some(values))) => assert_eq!(values[0].as_ref(), &[0; 12]),
                ("deadline", Err(error)) => assert!(matches!(
                    crate::catalog::chunk_read_error(error.into()),
                    DataServerError::DeadlineExceeded
                )),
                ("invalid range", Err(error)) => {
                    assert!(error.to_string().contains("outside the frame"))
                }
                (_, result) => panic!("unexpected {outcome} result: {result:?}"),
            }
        }
        assert_eq!(budget.metrics(), (0, 128 + scratch, 0));
    }
}
