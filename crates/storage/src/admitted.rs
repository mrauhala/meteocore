use super::*;
use futures::TryStreamExt;

impl DataStore {
    /// Optional full-object read with admission after GET headers, before body
    /// collection. The caller must retain its reservation through use of the
    /// returned bytes. No preliminary HEAD request or size/GET race is needed.
    /// Bodies exceeding the advertised size are rejected before growing the
    /// destination buffer. Missing objects do not invoke `admit`.
    pub fn get_opt_admitted(
        &self,
        path: &ObjectPath,
        admit: impl FnOnce(u64) -> Result<(), DataServerError>,
    ) -> Result<Option<Bytes>, DataServerError> {
        let bytes = self.block_on_result(None, async {
            let result = match self.inner.get(path).await {
                Ok(result) => result,
                Err(object_store::Error::NotFound { .. }) => return Ok(None),
                Err(error) => return Err(StorageError::from(error).into()),
            };
            let size = result
                .range
                .end
                .checked_sub(result.range.start)
                .ok_or(DataServerError::ResourceExhausted)?;
            admit(size)?;
            collect(result, size).await.map(Some)
        })?;
        if let Some(bytes) = &bytes {
            self.bytes_read
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(bytes)
    }
}

async fn collect(result: object_store::GetResult, size: u64) -> Result<Bytes, DataServerError> {
    let size = usize::try_from(size).map_err(|_| DataServerError::ResourceExhausted)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| DataServerError::ResourceExhausted)?;
    let mut stream = result.into_stream();
    while let Some(part) = stream.try_next().await.map_err(StorageError::from)? {
        if part.len() > size - bytes.len() {
            return Err(DataServerError::ResourceExhausted);
        }
        bytes.extend_from_slice(&part);
    }
    if bytes.len() != size {
        return Err(DataServerError::Storage(
            "Object body length differs from its advertised size".into(),
        ));
    }
    Ok(bytes.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{stream, StreamExt};

    fn response(size: u64, parts: Vec<&'static [u8]>) -> object_store::GetResult {
        object_store::GetResult {
            payload: object_store::GetResultPayload::Stream(
                stream::iter(parts.into_iter().map(|part| Ok(Bytes::from_static(part)))).boxed(),
            ),
            meta: ObjectMeta {
                location: ObjectPath::from("chunk"),
                last_modified: chrono::Utc::now(),
                size,
                e_tag: None,
                version: None,
            },
            range: 0..size,
            attributes: Default::default(),
            extensions: Default::default(),
        }
    }

    #[tokio::test]
    async fn collection_checks_actual_body_against_admitted_size() {
        assert_eq!(
            collect(response(4, vec![b"ab", b"cd"]), 4).await.unwrap(),
            b"abcd"[..]
        );
        assert!(matches!(
            collect(response(4, vec![b"ab", b"cde"]), 4).await,
            Err(DataServerError::ResourceExhausted)
        ));
        assert!(matches!(
            collect(response(4, vec![b"abc"]), 4).await,
            Err(DataServerError::Storage(_))
        ));
        assert!(collect(response(0, vec![]), 0).await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admitted_get_preserves_missing_errors_and_successful_byte_counts() {
        let store = DataStore::new(Arc::new(object_store::memory::InMemory::new()));
        let path = ObjectPath::from("chunk");
        assert!(store
            .get_opt_admitted(&path, |_| panic!("missing payload must not reserve"))
            .unwrap()
            .is_none());
        store
            .inner
            .put(&path, Bytes::from_static(b"abc").into())
            .await
            .unwrap();
        assert!(matches!(
            store.get_opt_admitted(&path, |_| Err(DataServerError::ResourceExhausted)),
            Err(DataServerError::ResourceExhausted)
        ));
        assert_eq!(store.bytes_read(), 0);
        assert_eq!(
            store
                .get_opt_admitted(&path, |size| {
                    assert_eq!(size, 3);
                    Ok(())
                })
                .unwrap()
                .unwrap(),
            b"abc"[..]
        );
        assert_eq!(store.bytes_read(), 3);
    }
}
