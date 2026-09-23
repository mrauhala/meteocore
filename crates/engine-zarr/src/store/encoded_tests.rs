use super::{test_server::HttpStore, *};
use crate::{encoded, read_budget::Budget};
use ds_core::{deadline, error::DataServerError};

fn exhausted(error: &StorageError) -> bool {
    matches!(error, StorageError::IOError(error) if matches!(
        error.get_ref().and_then(|error| error.downcast_ref::<DataServerError>()),
        Some(DataServerError::ResourceExhausted)
    ))
}

#[test]
fn oversized_response_is_rejected_from_headers_and_can_retry() {
    let server = HttpStore::new(1);
    let budget = Arc::new(Budget::new(64));
    std::thread::scope(|threads| {
        let worker = threads.spawn(|| {
            let _deadline = deadline::enter(Some(Instant::now() + Duration::from_secs(2)));
            let _encoded = encoded::enter(Some(budget.clone()));
            server.store.get(&StoreKey::new("chunk").unwrap())
        });
        let mut request = server.next("/chunk");
        request.headers(200, ds_cache::MIB);
        // No body is supplied: admission must fail before body collection.
        assert!(exhausted(&worker.join().unwrap().unwrap_err()));
    });
    assert_eq!(budget.metrics(), (0, 64, 1));
    assert_eq!(server.store.shared.store.bytes_read(), 0);
    std::thread::scope(|threads| {
        let worker = threads.spawn(|| {
            let _encoded = encoded::enter(Some(budget.clone()));
            server.store.get(&StoreKey::new("chunk").unwrap())
        });
        server.next("/chunk").reply(200, b"small");
        assert_eq!(worker.join().unwrap().unwrap().unwrap(), b"small"[..]);
    });
    assert_eq!(budget.metrics().0, 0);
    assert_eq!(server.calls(), 2);
}

#[test]
fn cached_full_range_and_size_reads_keep_one_allowance_through_decode() {
    let server = HttpStore::new(1);
    let store = &server.store;
    let key = StoreKey::new("chunk").unwrap();
    std::thread::scope(|threads| {
        let worker = threads.spawn(|| store.get(&key));
        server.next("/chunk").reply(200, b"abcdef");
        worker.join().unwrap().unwrap();
    });
    let budget = Arc::new(Budget::new(12));
    {
        let _scope = encoded::enter(Some(budget.clone()));
        assert_eq!(store.size_key(&key).unwrap(), Some(6));
        let bytes = store.get(&key).unwrap().unwrap();
        let owned: Vec<u8> = bytes.into(); // zarrs makes this conversion too
        let ranges = store
            .get_partial_many(
                &key,
                Box::new([zarrs::storage::byte_range::ByteRange::from(1..3)].into_iter()),
            )
            .unwrap()
            .unwrap();
        assert_eq!(ranges.collect::<Result<Vec<_>, _>>().unwrap()[0], b"bc"[..]);
        assert_eq!(budget.metrics().0, 12);
        drop(owned);
        assert_eq!(
            budget.metrics().0,
            12,
            "scope covers codec copies after Bytes are dropped"
        );
    }
    assert_eq!(
        budget.metrics().0,
        0,
        "resident cache does not retain the source allowance"
    );
    let tiny = Arc::new(Budget::new(11));
    let _scope = encoded::enter(Some(tiny.clone()));
    assert!(exhausted(&store.get(&key).unwrap_err()));
    server.assert_idle();
    assert_eq!(server.calls(), 1);
}

#[test]
fn coalesced_waiter_cannot_bypass_its_own_encoded_budget() {
    let server = HttpStore::new(1);
    let store = &server.store;
    let owner_budget = Arc::new(Budget::new(12));
    let waiter_budget = Arc::new(Budget::new(11));
    std::thread::scope(|threads| {
        let owner = threads.spawn(|| {
            let _scope = encoded::enter(Some(owner_budget.clone()));
            store.get(&StoreKey::new("chunk").unwrap())
        });
        let request = server.next("/chunk");
        let waiter = threads.spawn(|| {
            let _scope = encoded::enter(Some(waiter_budget.clone()));
            store.get(&StoreKey::new("chunk").unwrap())
        });
        server.assert_idle();
        request.reply(200, b"abcdef");
        assert_eq!(owner.join().unwrap().unwrap().unwrap(), b"abcdef"[..]);
        assert!(exhausted(&waiter.join().unwrap().unwrap_err()));
    });
    assert_eq!(owner_budget.metrics().0, 0);
    assert_eq!(waiter_budget.metrics(), (0, 11, 1));
    assert_eq!(server.calls(), 1);
}

#[test]
fn body_timeout_releases_admission() {
    let server = HttpStore::new(0);
    let budget = Arc::new(Budget::new(12));
    let store = &server.store;
    std::thread::scope(|threads| {
        let worker = threads.spawn(|| {
            let _deadline = deadline::enter(Some(Instant::now() + Duration::from_millis(200)));
            let _scope = encoded::enter(Some(budget.clone()));
            store.get(&StoreKey::new("chunk").unwrap())
        });
        let mut request = server.next("/chunk");
        request.headers(200, 6);
        let error = worker.join().unwrap().unwrap_err();
        assert!(matches!(error, StorageError::IOError(error) if matches!(
            error.get_ref().and_then(|error| error.downcast_ref::<DataServerError>()),
            Some(DataServerError::DeadlineExceeded)
        )));
    });
    assert_eq!(budget.metrics().0, 0);
}
