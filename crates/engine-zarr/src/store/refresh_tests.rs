use super::*;
use ds_storage::object_store::local::LocalFileSystem;

fn local(path: &std::path::Path, cache_mb: u64) -> DsStore {
    DsStore::new(
        DataStore::new(Arc::new(LocalFileSystem::new_with_prefix(path).unwrap())),
        "",
        cache_mb,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generations_separate_present_and_missing_objects_without_clearing_old_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("chunk"), b"old").unwrap();
    let old = local(dir.path(), 1);
    let key = StoreKey::new("chunk").unwrap();
    let missing = StoreKey::new("missing").unwrap();
    assert_eq!(old.get(&key).unwrap().unwrap().as_ref(), b"old");
    assert!(old.get(&missing).unwrap().is_none());
    std::fs::write(dir.path().join("chunk"), b"new").unwrap();
    std::fs::write(dir.path().join("missing"), b"written").unwrap();
    std::fs::write(dir.path().join("uncached"), b"new").unwrap();
    let new = old.fresh();
    assert!(
        Arc::ptr_eq(&old.shared, &new.shared),
        "one client and byte budget"
    );
    assert_ne!(old.generation.version, new.generation.version);
    assert_eq!(new.get(&key).unwrap().unwrap().as_ref(), b"new");
    assert_eq!(new.get(&missing).unwrap().unwrap().as_ref(), b"written");
    old.generation.retire();
    assert!(old.list_dir(&StorePrefix::new("").unwrap()).is_err());
    assert_eq!(old.get(&key).unwrap().unwrap().as_ref(), b"old");
    assert!(old.get(&missing).unwrap().is_none());
    assert!(old.get(&StoreKey::new("uncached").unwrap()).is_err());
    assert_eq!(new.get(&key).unwrap().unwrap().as_ref(), b"new");
    old.shared
        .cache
        .retain(|key, _| key.generation != old.generation.version);
    assert!(
        old.get(&key).is_err(),
        "eviction cannot make an old reader fetch new bytes"
    );
    let _deadline = ds_core::deadline::enter(Some(std::time::Instant::now()));
    assert!(
        new.get(&key).is_err(),
        "even retained values observe deadlines"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_shares_the_byte_budget_and_zero_disables_retention() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("chunk"), vec![1; 600_000]).unwrap();
    let key = StoreKey::new("chunk").unwrap();
    for capacity in [0, 1] {
        let old = local(dir.path(), capacity);
        assert_eq!(old.get(&key).unwrap().unwrap().len(), 600_000);
        for _ in 0..8 {
            let new = old.fresh();
            assert_eq!(new.get(&key).unwrap().unwrap().len(), 600_000);
        }
        assert!(old.shared.cache.metrics().bytes <= capacity * ds_cache::MIB);
        if capacity == 0 {
            assert_eq!(old.shared.cache.metrics().bytes, 0);
            old.generation.retire();
            assert!(old.get(&key).is_err());
        }
    }
}

#[test]
fn a_fetch_racing_retirement_is_discarded() {
    let server = super::test_server::HttpStore::new(1);
    let store = &server.store;
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let _deadline = ds_core::deadline::enter(Some(Instant::now() + Duration::from_secs(5)));
            store.get(&StoreKey::new("chunk").unwrap())
        });
        let request = server.next("/chunk");
        store.generation.retire();
        request.reply(200, b"new");
        let error = worker.join().unwrap().unwrap_err();
        let StorageError::IOError(io) = error else {
            panic!("retirement must preserve a typed IO payload: {error}");
        };
        assert!(matches!(
            io.get_ref().unwrap().downcast_ref(),
            Some(ds_core::error::DataServerError::ResourceExhausted)
        ));
    });
    assert_eq!(
        store.shared.cache.metrics().bytes,
        0,
        "discard the stale fill"
    );
}
