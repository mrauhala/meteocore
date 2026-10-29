use super::*;
use futures::TryStreamExt as _;

async fn read_all(reader: Pin<Box<dyn AsyncBufRead + Send>>) -> Vec<u8> {
    let mut reader = reader;
    let mut body = Vec::new();
    reader.read_to_end(&mut body).await.unwrap();
    body
}

async fn storage(payload_bytes: u64) -> (MeteredStorage, Arc<Meter>, Settings) {
    let backend = icechunk::new_in_memory_storage().await.unwrap();
    let settings = backend.default_settings().await.unwrap();
    let ctx = StorageContext::unattributed(&settings);
    let chunk: Vec<u8> = (0..100).collect();
    for (path, body) in [
        ("chunks/0123456789ABCDEF", chunk),
        ("manifests/0123456789ABCDEF", b"manifest".to_vec()),
        ("chunksfile", b"not a chunk".to_vec()),
    ] {
        backend
            .put_object(&ctx, path, body.into(), None, vec![], None)
            .await
            .unwrap();
    }
    let meter = Arc::new(Meter::new(payload_bytes));
    (MeteredStorage::new(backend, meter.clone()), meter, settings)
}

#[tokio::test]
async fn counts_requests_and_body_bytes_of_every_read() {
    let (storage, meter, settings) = storage(1024 * 1024).await;
    let ctx = StorageContext::unattributed(&settings);

    let (stream, _) = storage
        .get_object_range(&ctx, "manifests/0123456789ABCDEF", None)
        .await
        .unwrap();
    let body: Vec<Bytes> = stream.try_collect().await.unwrap();
    assert_eq!(body.concat(), b"manifest");
    assert_eq!(meter.counts(), (1, 8));

    // Unranged and non-chunk reads take the trait's path, counted per part.
    let (reader, _) = storage.get_object(&ctx, "chunksfile", None).await.unwrap();
    assert_eq!(read_all(reader).await, b"not a chunk");
    assert_eq!(meter.counts(), (2, 19));

    match storage
        .get_object_conditional(&ctx, "manifests/0123456789ABCDEF", None)
        .await
        .unwrap()
    {
        GetModifiedResult::Modified { data, .. } => assert_eq!(read_all(data).await, b"manifest"),
        other => panic!("unconditional read returned {other}"),
    }
    assert_eq!(meter.counts(), (3, 27));

    let listed: Vec<_> = storage
        .list_objects(&ctx, "chunks")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert!(!listed.is_empty());
    assert_eq!(
        meter.counts(),
        (4, 27),
        "a listing is a request, not body bytes"
    );
    assert_eq!(
        meter
            .take_log()
            .into_iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>(),
        [
            "manifests/0123456789ABCDEF",
            "chunksfile",
            "manifests/0123456789ABCDEF",
            "chunks"
        ]
    );
}

#[tokio::test]
async fn chunk_ranges_are_cached_by_path_and_range() {
    let (storage, meter, settings) = storage(1024 * 1024).await;
    let ctx = StorageContext::unattributed(&settings);
    let path = "chunks/0123456789ABCDEF";

    let (reader, _) = storage
        .get_object(&ctx, path, Some(&(10..60)))
        .await
        .unwrap();
    assert_eq!(read_all(reader).await, (10..60).collect::<Vec<u8>>());
    // The in-memory backend's settings split a range into several parts,
    // each a request.
    let (parts, bytes) = meter.counts();
    assert!(parts >= 1);
    assert_eq!(bytes, 50);
    let (reader, _) = storage
        .get_object(&ctx, path, Some(&(10..60)))
        .await
        .unwrap();
    assert_eq!(read_all(reader).await, (10..60).collect::<Vec<u8>>());
    assert_eq!(meter.counts(), (parts, 50), "the second read is a hit");
    let (reader, _) = storage
        .get_object(&ctx, path, Some(&(0..10)))
        .await
        .unwrap();
    assert_eq!(read_all(reader).await, (0..10).collect::<Vec<u8>>());
    assert!(meter.counts().0 > parts, "another range is another entry");
    assert_eq!(meter.counts().1, 60);

    let cache = meter.payload_metrics();
    assert_eq!((cache.hits, cache.misses), (1, 2));
    assert_eq!(cache.capacity_bytes, 1024 * 1024);
    assert!(cache.bytes >= 60, "{cache:?}");
}

#[tokio::test]
async fn a_zero_payload_budget_keeps_nothing() {
    let (storage, meter, settings) = storage(0).await;
    let ctx = StorageContext::unattributed(&settings);
    let mut requests = Vec::new();
    for _ in 0..2 {
        let (reader, _) = storage
            .get_object(&ctx, "chunks/0123456789ABCDEF", Some(&(0..100)))
            .await
            .unwrap();
        assert_eq!(read_all(reader).await.len(), 100);
        requests.push(meter.counts().0);
    }
    assert_eq!(requests[1], 2 * requests[0], "both reads are fetched");
    assert_eq!(meter.counts().1, 200);
    let cache = meter.payload_metrics();
    assert_eq!((cache.hits, cache.misses, cache.bytes), (0, 2, 0));
    assert_eq!(cache.capacity_bytes, 0);
}
