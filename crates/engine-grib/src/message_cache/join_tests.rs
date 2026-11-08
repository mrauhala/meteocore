//! A request's wait for another reader's fill of a message (#1399).

use super::*;
use std::sync::mpsc;

/// A request waiting for another reader's fetch of a message, the
/// `Timeout` arm of [`MessageCache::get_with`], waits no longer than its
/// deadline, and that fetch lands for the next request. Without a deadline
/// it waits for the fetch and takes its bytes. No waiter sends a GET.
#[test]
fn a_wait_for_another_message_fetch_ends_at_the_deadline() {
    let cache = MessageCache::new(1).unwrap();
    let store = DataStore::new(Arc::new(ds_storage::object_store::memory::InMemory::new()));
    let path = Path::from("held.grib2");
    let bytes = Bytes::from(vec![7u8; 64]);
    let entry = MessageEntry {
        source_url: None,
        param: "TMP".into(),
        levtype: "sfc".into(),
        level: None,
        offset: 0,
        length: Some(bytes.len() as u64),
        step_kind: crate::wgrib2_index::StepKind::Instant,
    };
    let key = MessageKey::new(path.as_ref(), &entry);
    let read = |deadline: Option<Instant>| {
        let _deadline = deadline::enter(deadline);
        cache
            .get_with(&store, &path, &entry, false, |_| Ok(()))
            .map(|(bytes, _)| bytes)
    };
    for bounded in [true, false] {
        // Another reader's fetch, claimed and not yet published.
        let CacheEntry::Vacant(fill) = cache
            .cache
            .get_value_or_guard_untracked(&key, Duration::ZERO)
        else {
            panic!("the key is neither resident nor claimed");
        };
        std::thread::scope(|scope| {
            let (done_tx, done_rx) = mpsc::channel();
            scope.spawn(move || {
                let start = Instant::now();
                let deadline = bounded.then(|| start + Duration::from_millis(200));
                done_tx.send((read(deadline), start.elapsed())).unwrap();
            });
            if bounded {
                let waited = done_rx.recv_timeout(Duration::from_secs(3));
                let _ = fill.insert(bytes.clone());
                let (read, elapsed) = waited.expect("the request waited past its deadline");
                assert!(
                    matches!(read, Err(DataServerError::DeadlineExceeded)),
                    "{read:?}"
                );
                assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
            } else {
                assert!(
                    done_rx.recv_timeout(Duration::from_millis(300)).is_err(),
                    "without a deadline the request waits for the fetch"
                );
                let _ = fill.insert(bytes.clone());
                let (read, _) = done_rx.recv().unwrap();
                assert_eq!(read.unwrap(), bytes);
            }
        });
        // The fetch landed: the next request reads it from the cache.
        assert_eq!(read(None).unwrap(), bytes);
        cache.cache.remove(&key).expect("resident");
    }
    assert_eq!(store.bytes_read(), 0, "no waiter fetched the message");
}
