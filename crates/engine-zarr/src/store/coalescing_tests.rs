use super::test_server::HttpStore;
use super::*;
use ds_core::{deadline, error::DataServerError};
use zarrs::storage::byte_range::ByteRange;

fn core_error(error: &StorageError) -> Option<&DataServerError> {
    match error {
        StorageError::IOError(error) => error.get_ref()?.downcast_ref(),
        _ => None,
    }
}

fn read(store: &DsStore, key: &str, operation: usize) -> Result<Option<Bytes>, StorageError> {
    let _deadline = deadline::enter(Some(Instant::now() + Duration::from_secs(5)));
    let key = StoreKey::new(key).unwrap();
    match operation {
        0 => store.get(&key),
        1 => store
            .get_partial_many(
                &key,
                Box::new([ByteRange::from(1..3), ByteRange::Suffix(2)].into_iter()),
            )?
            .map(|ranges| {
                ranges
                    .collect::<Result<Vec<_>, _>>()
                    .map(|ranges| Bytes::from(ranges.concat()))
            })
            .transpose(),
        _ => store
            .size_key(&key)
            .map(|size| size.map(|size| Bytes::from(size.to_string()))),
    }
}

#[test]
fn concurrent_get_range_and_size_share_present_and_missing_fills() {
    for missing in [false, true] {
        let server = HttpStore::new(1);
        let store = &server.store;
        std::thread::scope(|scope| {
            let owner = scope.spawn(|| read(store, "chunk", 0));
            let request = server.next("/chunk");
            let waiters: Vec<_> = (0..8)
                .map(|i| scope.spawn(move || read(store, "chunk", i % 3)))
                .collect();
            server.assert_idle();
            request.reply(if missing { 404 } else { 200 }, b"abcdef");
            let expected = (!missing).then(|| Bytes::from_static(b"abcdef"));
            assert_eq!(owner.join().unwrap().unwrap(), expected);
            for (i, waiter) in waiters.into_iter().enumerate() {
                let expected = (!missing).then(|| {
                    Bytes::from_static(match i % 3 {
                        0 => b"abcdef",
                        1 => b"bcef",
                        _ => b"6",
                    })
                });
                assert_eq!(waiter.join().unwrap().unwrap(), expected);
            }
        });
        server.assert_idle();
        assert_eq!(server.calls(), 1, "nine readers share one HTTP GET");
        assert_eq!(store.shared.cache.stats(), (8, 1));
        assert_eq!(store.shared.store.bytes_read(), if missing { 0 } else { 6 });
    }
}

#[test]
fn waiter_deadlines_do_not_cancel_fills_with_zero_or_oversized_retention() {
    for (capacity, size) in [(1, 6), (0, 6), (1, ds_cache::MIB as usize + 1)] {
        let server = HttpStore::new(capacity);
        let store = &server.store;
        let payload = vec![42; size];
        std::thread::scope(|scope| {
            let owner = scope.spawn(|| read(store, "chunk", 0));
            let request = server.next("/chunk");
            let waiter = scope.spawn(|| {
                let _deadline = deadline::enter(Some(Instant::now() + Duration::from_millis(100)));
                store.get(&StoreKey::new("chunk").unwrap())
            });
            let error = waiter.join().unwrap().unwrap_err();
            assert!(matches!(
                core_error(&error),
                Some(DataServerError::DeadlineExceeded)
            ));
            server.assert_idle();
            assert_eq!(server.calls(), 1, "waiting must not start a duplicate GET");
            request.reply(200, &payload);
            assert_eq!(owner.join().unwrap().unwrap().unwrap().as_ref(), payload);

            let retained = capacity > 0 && size < ds_cache::MIB as usize;
            assert_eq!(store.shared.cache.metrics().bytes > 0, retained);
            let later = scope.spawn(|| read(store, "chunk", 0));
            if !retained {
                server.next("/chunk").reply(200, &payload);
            }
            assert_eq!(later.join().unwrap().unwrap().unwrap().as_ref(), payload);
            assert_eq!(server.calls(), if retained { 1 } else { 2 });
        });
    }
}

#[test]
fn other_keys_and_new_generations_progress_while_an_old_fill_is_pending() {
    let server = HttpStore::new(1);
    let old = &server.store;
    let new = old.fresh();
    std::thread::scope(|scope| {
        let owner = scope.spawn(|| read(old, "chunk", 0));
        let request = server.next("/chunk");
        let waiter = scope.spawn(|| read(old, "chunk", 0));
        let replacement = scope.spawn(|| read(&new, "chunk", 0));
        server.next("/chunk").reply(200, b"new");
        assert_eq!(
            replacement.join().unwrap().unwrap().unwrap().as_ref(),
            b"new"
        );
        let other = scope.spawn(|| read(old, "other", 0));
        server.next("/other").reply(200, b"other");
        assert_eq!(other.join().unwrap().unwrap().unwrap().as_ref(), b"other");

        old.generation.retire();
        let error = read(old, "chunk", 0).unwrap_err();
        assert!(matches!(
            core_error(&error),
            Some(DataServerError::ResourceExhausted)
        ));
        request.reply(200, b"old");
        for reader in [owner, waiter] {
            let error = reader.join().unwrap().unwrap_err();
            assert!(matches!(
                core_error(&error),
                Some(DataServerError::ResourceExhausted)
            ));
        }
        assert_eq!(read(&new, "chunk", 0).unwrap().unwrap().as_ref(), b"new");
        assert_eq!(read(old, "other", 0).unwrap().unwrap().as_ref(), b"other");
    });
    server.assert_idle();
    assert_eq!(
        server.calls(),
        3,
        "retired waiters must not retry backend I/O"
    );
    assert!(!old.shared.cache.contains_key(&Key {
        generation: old.generation.version,
        path: "chunk".into(),
    }));
}

#[test]
fn failed_or_expired_owners_release_the_fill_for_a_waiters_retry() {
    for expires in [false, true] {
        let server = HttpStore::new(1);
        let store = &server.store;
        std::thread::scope(|scope| {
            let owner = scope.spawn(|| {
                let timeout = if expires {
                    Duration::from_millis(500)
                } else {
                    Duration::from_secs(5)
                };
                let _deadline = deadline::enter(Some(Instant::now() + timeout));
                store.get(&StoreKey::new("chunk").unwrap())
            });
            let mut request = Some(server.next("/chunk"));
            let waiter = scope.spawn(|| read(store, "chunk", 0));
            if !expires {
                request.take().unwrap().reply(503, b"injected failure");
            }
            let error = owner.join().unwrap().unwrap_err();
            if expires {
                assert!(matches!(
                    core_error(&error),
                    Some(DataServerError::DeadlineExceeded)
                ));
            } else {
                assert!(matches!(
                    core_error(&error),
                    Some(DataServerError::Storage(_))
                ));
            }
            drop(request);
            server.next("/chunk").reply(200, b"retry");
            assert_eq!(waiter.join().unwrap().unwrap().unwrap().as_ref(), b"retry");
            assert_eq!(read(store, "chunk", 0).unwrap().unwrap().as_ref(), b"retry");
        });
        server.assert_idle();
        assert_eq!(server.calls(), 2);
        assert_eq!(store.shared.cache.metrics().misses, 2);
    }
}

#[test]
fn tokio_workers_remain_available_while_waiting_for_an_object() {
    let server = HttpStore::new(1);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let owner = {
        let store = server.store.clone();
        runtime.spawn(async move { read(&store, "chunk", 0) })
    };
    let request = server.next("/chunk");
    let (started, ready) = std::sync::mpsc::channel();
    let waiters: Vec<_> = (0..2)
        .map(|_| {
            let store = server.store.clone();
            let started = started.clone();
            runtime.spawn(async move {
                // No await between this signal and the synchronous read: each
                // task occupies a runtime worker until the read yields it.
                started.send(()).unwrap();
                read(&store, "chunk", 0)
            })
        })
        .collect();
    for _ in 0..2 {
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    let (progress, progressed) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        progress.send(()).unwrap();
    });
    progressed
        .recv_timeout(Duration::from_secs(1))
        .expect("cache waiters must release runtime workers needed by the owner's storage I/O");
    server.assert_idle();
    request.reply(200, b"shared");
    for reader in std::iter::once(owner).chain(waiters) {
        assert_eq!(
            runtime.block_on(reader).unwrap().unwrap().unwrap().as_ref(),
            b"shared"
        );
    }
    assert_eq!(server.calls(), 1);
}
