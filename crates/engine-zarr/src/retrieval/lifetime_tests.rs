use super::*;
use std::sync::{Mutex, Weak};
use std::time::{Duration, Instant};
use zarrs::storage::{
    byte_range::ByteRangeIterator, ListableStorageTraits, MaybeBytes, MaybeBytesIterator,
    ReadableStorageTraits, StorageError, StoreKey, StoreKeys, StoreKeysPrefixes, StorePrefix,
};

#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Success,
    Exhausted,
    Error,
    Panic,
    Deadline,
}

struct Probe {
    inner: Arc<EngineStore>,
    calls: Mutex<Vec<(String, &'static str)>>,
    previous: Mutex<Option<(String, Weak<encoded::Context>)>>,
    check_lifetimes: bool,
    thread: std::thread::ThreadId,
    deadline: Option<Instant>,
    outcome: Outcome,
}

impl Probe {
    fn read<T>(
        &self,
        key: &StoreKey,
        method: &'static str,
        read: impl FnOnce() -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        if !key.as_str().contains("/c/") {
            return read();
        }
        assert_eq!(std::thread::current().id(), self.thread);
        assert_eq!(deadline::current(), self.deadline);
        self.calls.lock().unwrap().push((key.to_string(), method));
        let context = encoded::current().unwrap();
        if self.check_lifetimes {
            let mut previous = self.previous.lock().unwrap();
            if let Some((previous_key, previous_context)) = &*previous {
                if previous_key == key.as_str() {
                    assert!(Arc::ptr_eq(&context, &previous_context.upgrade().unwrap()));
                } else {
                    assert!(
                        previous_context.upgrade().is_none(),
                        "finished chunk retained its scope"
                    );
                }
            }
            *previous = Some((key.to_string(), Arc::downgrade(&context)));
        }
        let result = read();
        if key.as_str() == "a/c/0/1" {
            // Fail after the first completed chunk and the next encoded read.
            // The native/source owner must remain admitted throughout cleanup.
            match self.outcome {
                Outcome::Exhausted => context
                    .intermediate(context.budget.metrics().1 as usize)
                    .map_err(crate::store::io_err)?,
                Outcome::Error => return Err(StorageError::Other("injected read failure".into())),
                Outcome::Panic => panic!("injected retrieval panic"),
                Outcome::Deadline => {
                    std::thread::sleep(
                        self.deadline
                            .unwrap()
                            .saturating_duration_since(Instant::now()),
                    );
                    deadline::check().map_err(crate::store::io_err)?;
                }
                Outcome::Success => {}
            }
        }
        result
    }
}

impl ReadableStorageTraits for Probe {
    fn get(&self, key: &StoreKey) -> Result<MaybeBytes, StorageError> {
        self.read(key, "get", || self.inner.get(key))
    }
    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        self.read(key, "ranges", || self.inner.get_partial_many(key, ranges))
    }
    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        self.read(key, "size", || self.inner.size_key(key))
    }
    fn supports_get_partial(&self) -> bool {
        self.inner.supports_get_partial()
    }
}

impl ListableStorageTraits for Probe {
    fn list(&self) -> Result<StoreKeys, StorageError> {
        self.inner.list()
    }
    fn list_prefix(&self, prefix: &StorePrefix) -> Result<StoreKeys, StorageError> {
        self.inner.list_prefix(prefix)
    }
    fn list_dir(&self, prefix: &StorePrefix) -> Result<StoreKeysPrefixes, StorageError> {
        self.inner.list_dir(prefix)
    }
    fn size_prefix(&self, prefix: &StorePrefix) -> Result<u64, StorageError> {
        self.inner.size_prefix(prefix)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serial_scopes_preserve_storage_operations_and_cleanup_at_chunk_boundaries() {
    let options = crate::catalog::single_threaded_opts();
    for layout in ["plain", "shard", "outer-shard", "outer-nested-shard"] {
        let dir = tempfile::tempdir().unwrap();
        let original = fixture(dir.path(), layout);
        for subset in [
            original.subset_all(),
            ArraySubset::new_with_ranges(&[1..7, 1..7]),
        ] {
            let mut expected_calls = Vec::new();
            for outcome in [
                Outcome::Success,
                Outcome::Exhausted,
                Outcome::Error,
                Outcome::Panic,
                Outcome::Deadline,
            ] {
                // Success first compares the exact storage operation sequence
                // to upstream, then each failure checks the same scope cleanup.
                for scoped in if outcome == Outcome::Success {
                    vec![false, true]
                } else {
                    vec![true]
                } {
                    let end = (outcome == Outcome::Deadline)
                        .then(|| Instant::now() + Duration::from_millis(200));
                    let _deadline = deadline::enter(end);
                    let probe = Arc::new(Probe {
                        inner: original.storage(),
                        calls: Mutex::new(Vec::new()),
                        previous: Mutex::new(None),
                        check_lifetimes: scoped,
                        thread: std::thread::current().id(),
                        deadline: end,
                        outcome,
                    });
                    let array = bounded_array(
                        Array::open(Arc::new(EngineStore::new(probe.clone())), "/a").unwrap(),
                    )
                    .unwrap();
                    let budget = Arc::new(Budget::new(ds_cache::MIB));
                    let source = budget.reserve(&array, &subset, Some(0), false).unwrap();
                    let baseline = budget.metrics().0;
                    let scope = encoded::enter(Some(budget.clone()));
                    let parent = encoded::current().unwrap();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if scoped {
                            serial(&array, &subset, &options)
                        } else {
                            array
                                .retrieve_array_subset_opt::<ArrayBytes>(&subset, &options)
                                .map_err(chunk_read_error)
                        }
                    }));
                    match outcome {
                        Outcome::Success => {
                            result.unwrap().unwrap();
                        }
                        Outcome::Exhausted => assert!(matches!(
                            result.unwrap(),
                            Err(DataServerError::ResourceExhausted)
                        )),
                        Outcome::Error => {
                            assert!(matches!(result.unwrap(), Err(DataServerError::Engine(_))))
                        }
                        Outcome::Panic => assert!(result.is_err()),
                        Outcome::Deadline => assert!(matches!(
                            result.unwrap(),
                            Err(DataServerError::DeadlineExceeded)
                        )),
                    }
                    if scoped {
                        assert_eq!(budget.metrics().0, baseline);
                        assert!(probe
                            .previous
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .1
                            .upgrade()
                            .is_none());
                        assert!(Arc::ptr_eq(&parent, &encoded::current().unwrap()));
                        if outcome == Outcome::Success {
                            assert_eq!(
                                *probe.calls.lock().unwrap(),
                                expected_calls,
                                "{layout}: {subset:?}"
                            );
                        }
                    } else {
                        expected_calls = probe.calls.lock().unwrap().clone();
                    }
                    drop(parent);
                    drop(scope);
                    assert_eq!(budget.metrics().0, baseline);
                    drop(source);
                    assert_eq!(budget.metrics().0, 0);
                }
            }
        }
    }
}
