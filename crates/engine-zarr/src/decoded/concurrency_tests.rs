//! Controlled storage gates test actual chunk fan-out without network timing.
use super::*;
use crate::read_budget::Budget;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Condvar, Mutex,
};
use zarrs::storage::{
    byte_range::ByteRangeIterator, ListableStorageTraits, MaybeBytes, MaybeBytesIterator,
    ReadableStorageTraits, StorageError, StoreKey, StoreKeys, StoreKeysPrefixes, StorePrefix,
};

static GATED_TESTS: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
enum Outcome {
    Success,
    Error,
    Panic,
    Deadline,
}

struct Probe {
    deadline: Option<Instant>,
    active: AtomicUsize,
    peak: AtomicUsize,
    calls: AtomicUsize,
    started: mpsc::Sender<()>,
    open: Mutex<bool>,
    wake: Condvar,
    budget: Arc<Budget>,
    outcome: Outcome,
}

impl Probe {
    fn read<T>(
        &self,
        key: &StoreKey,
        read: impl FnOnce() -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        if !key.as_str().contains("/c/") {
            return read();
        }
        assert_eq!(deadline::current(), self.deadline);
        let encoded = crate::encoded::current().expect("chunk worker inherits its encoded budget");
        assert!(Arc::ptr_eq(&encoded.budget, &self.budget));
        encoded
            .object(key.as_str(), 16)
            .map_err(crate::store::io_err)?;
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        struct Active<'a>(&'a Probe);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                assert!(self.0.budget.metrics().0 > 0, "worker lost its reservation");
                self.0.active.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _active = Active(self);
        self.started.send(()).unwrap();
        let end = self
            .deadline
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(5));
        let (open, _) = self
            .wake
            .wait_timeout_while(
                self.open.lock().unwrap(),
                end.saturating_duration_since(Instant::now()),
                |open| !*open,
            )
            .unwrap();
        deadline::check().map_err(crate::store::io_err)?;
        assert!(*open, "test did not release its storage gate");
        drop(open);
        if call == 0 {
            match self.outcome {
                Outcome::Error => {
                    return Err(crate::store::io_err(DataServerError::Storage(
                        "injected read failure".into(),
                    )))
                }
                Outcome::Panic => panic!("injected codec panic"),
                _ => {}
            }
        }
        read()
    }
}

struct ProbedStore {
    inner: Arc<EngineStore>,
    probe: Arc<Probe>,
}

impl ReadableStorageTraits for ProbedStore {
    fn get(&self, key: &StoreKey) -> Result<MaybeBytes, StorageError> {
        self.probe.read(key, || self.inner.get(key))
    }

    fn get_partial_many<'a>(
        &'a self,
        key: &StoreKey,
        byte_ranges: ByteRangeIterator<'a>,
    ) -> Result<MaybeBytesIterator<'a>, StorageError> {
        self.probe
            .read(key, || self.inner.get_partial_many(key, byte_ranges))
    }

    fn size_key(&self, key: &StoreKey) -> Result<Option<u64>, StorageError> {
        self.probe.read(key, || self.inner.size_key(key))
    }

    fn supports_get_partial(&self) -> bool {
        self.inner.supports_get_partial()
    }
}

impl ListableStorageTraits for ProbedStore {
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

#[test]
fn fanout_joins_workers_and_holds_memory_on_success_error_panic_and_deadline() {
    let _exclusive = GATED_TESTS.lock().unwrap();
    for outcome in [
        Outcome::Success,
        Outcome::Error,
        Outcome::Panic,
        Outcome::Deadline,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let original = super::tests::fixture(dir.path(), "/a", false, 0.0);
        let subset = original.subset_all();
        let options = crate::catalog::single_threaded_opts();
        let expected = read_native(&original, &subset, &options).unwrap();
        let budget = Arc::new(Budget::new(MIB));
        let (started, ready) = mpsc::channel();
        let end = Instant::now() + Duration::from_secs(1);
        let probe = Arc::new(Probe {
            deadline: Some(end),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            started,
            open: Mutex::new(false),
            wake: Condvar::new(),
            budget: budget.clone(),
            outcome,
        });
        let array = Array::open(
            Arc::new(EngineStore::new(ProbedStore {
                inner: original.storage().clone(),
                probe: probe.clone(),
            })),
            "/a",
        )
        .unwrap();
        // No retention: every chunk must exercise the storage bridge.
        let reader = DecodedArray::new(
            &array,
            Some("snapshot"),
            "a",
            Arc::new(DecodedCache::new(0)),
        )
        .unwrap();
        let permit = budget.reserve(&array, &subset, Some(0), true).unwrap();
        assert_eq!(permit.parallelism(), MAX_PARALLEL_CHUNKS);
        std::thread::scope(|scope| {
            let worker_permit = permit.clone();
            let worker_budget = budget.clone();
            let worker = scope.spawn(move || {
                let _deadline = deadline::enter(Some(end));
                let _encoded = crate::encoded::enter(Some(worker_budget));
                let permit = worker_permit;
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reader.read(&array, &subset, &options, permit.parallelism())
                }))
            });
            for _ in 0..MAX_PARALLEL_CHUNKS {
                ready.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            assert_eq!(probe.active.load(Ordering::SeqCst), MAX_PARALLEL_CHUNKS);
            assert!(budget.metrics().0 > 0);
            // Dropping the waiter cannot release a worker-owned reservation.
            drop(permit);
            assert!(budget.metrics().0 > 0);
            if !matches!(outcome, Outcome::Deadline) {
                *probe.open.lock().unwrap() = true;
                probe.wake.notify_all();
            }
            let result = worker.join().unwrap();
            match outcome {
                Outcome::Success => assert_eq!(
                    result.unwrap().unwrap().into_fixed().unwrap().as_ref(),
                    expected
                ),
                Outcome::Error => {
                    assert!(matches!(result.unwrap(), Err(DataServerError::Engine(_))))
                }
                Outcome::Panic => assert!(result.is_err()),
                Outcome::Deadline => assert!(matches!(
                    result.unwrap(),
                    Err(DataServerError::DeadlineExceeded)
                )),
            }
        });
        assert_eq!(probe.active.load(Ordering::SeqCst), 0);
        assert_eq!(probe.peak.load(Ordering::SeqCst), MAX_PARALLEL_CHUNKS);
        assert_eq!(budget.metrics().0, 0);
        if !matches!(outcome, Outcome::Success) {
            assert_eq!(
                probe.calls.load(Ordering::SeqCst),
                MAX_PARALLEL_CHUNKS,
                "must not launch another batch after failure"
            );
        }
    }
}

#[test]
fn concurrent_requests_share_the_worker_limit() {
    let _exclusive = GATED_TESTS.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let original = super::tests::fixture(dir.path(), "/a", false, 0.0);
    let subset = original.subset_all();
    let options = crate::catalog::single_threaded_opts();
    let expected = read_native(&original, &subset, &options).unwrap();
    let budget = Arc::new(Budget::new(MIB));
    let (started, ready) = mpsc::channel();
    let probe = Arc::new(Probe {
        deadline: None,
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        started,
        open: Mutex::new(false),
        wake: Condvar::new(),
        budget: budget.clone(),
        outcome: Outcome::Success,
    });
    let array = Array::open(
        Arc::new(EngineStore::new(ProbedStore {
            inner: original.storage().clone(),
            probe: probe.clone(),
        })),
        "/a",
    )
    .unwrap();
    let start = std::sync::Barrier::new(3);
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..2 {
            let (array, subset, options, start) = (&array, &subset, &options, &start);
            let permit = budget.reserve(array, subset, Some(0), true).unwrap();
            assert_eq!(permit.parallelism(), MAX_PARALLEL_CHUNKS);
            let reader =
                DecodedArray::new(array, Some("snapshot"), "a", Arc::new(DecodedCache::new(0)))
                    .unwrap();
            let worker_budget = budget.clone();
            workers.push(scope.spawn(move || {
                let _encoded = crate::encoded::enter(Some(worker_budget));
                start.wait();
                reader.read(array, subset, options, permit.parallelism())
            }));
        }
        start.wait();
        for _ in 0..MAX_PARALLEL_CHUNKS {
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert_eq!(probe.active.load(Ordering::SeqCst), MAX_PARALLEL_CHUNKS);
        *probe.open.lock().unwrap() = true;
        probe.wake.notify_all();
        for worker in workers {
            assert_eq!(
                worker
                    .join()
                    .unwrap()
                    .unwrap()
                    .into_fixed()
                    .unwrap()
                    .as_ref(),
                expected
            );
        }
    });
    assert_eq!(probe.peak.load(Ordering::SeqCst), MAX_PARALLEL_CHUNKS);
    assert_eq!(probe.active.load(Ordering::SeqCst), 0);
    assert_eq!(budget.metrics().0, 0);
}

#[test]
fn tight_budgets_reduce_cold_and_mixed_batches_without_rejecting() {
    let _exclusive = GATED_TESTS.lock().unwrap();
    for cached_first in [false, true] {
        for slots in 1..=3 {
            let dir = tempfile::tempdir().unwrap();
            let original = super::tests::fixture(dir.path(), "/a", false, 0.0);
            let subset = original.subset_all();
            let options = crate::catalog::single_threaded_opts();
            let expected = read_native(&original, &subset, &options).unwrap();
            let cache = Arc::new(DecodedCache::new(if cached_first { MIB } else { 0 }));
            if cached_first {
                let reader =
                    DecodedArray::new(&original, Some("snapshot"), "a", cache.clone()).unwrap();
                reader
                    .read(
                        &original,
                        &ArraySubset::new_with_ranges(&[0..1, 0..1, 0..1]),
                        &options,
                        1,
                    )
                    .unwrap();
            }
            // 105 source values; each cold chunk needs four 192-byte native
            // buffers and the probe's two 16-byte encoded buffers. Padding
            // counts even for the missing edge chunks.
            let budget = Arc::new(Budget::new(2520 + slots * (768 + 32)));
            let (started, ready) = mpsc::channel();
            let probe = Arc::new(Probe {
                deadline: None,
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
                started,
                open: Mutex::new(false),
                wake: Condvar::new(),
                budget: budget.clone(),
                outcome: Outcome::Success,
            });
            let array = Array::open(
                Arc::new(EngineStore::new(ProbedStore {
                    inner: original.storage().clone(),
                    probe: probe.clone(),
                })),
                "/a",
            )
            .unwrap();
            let reader = DecodedArray::new(&array, Some("snapshot"), "a", cache).unwrap();
            let source = budget.reserve(&array, &subset, Some(0), true).unwrap();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let _encoded = crate::encoded::enter(Some(budget.clone()));
                    reader.read(&array, &subset, &options, source.parallelism())
                });
                // With a cached first chunk, its pin uses part of the first
                // batch's capacity; later all-cold batches reach `slots`.
                let first_batch = if cached_first {
                    (slots - 1).max(1)
                } else {
                    slots
                };
                for _ in 0..first_batch {
                    ready.recv_timeout(Duration::from_secs(2)).unwrap();
                }
                assert_eq!(probe.active.load(Ordering::SeqCst), first_batch as usize);
                assert_eq!(budget.metrics().2, 0);
                *probe.open.lock().unwrap() = true;
                probe.wake.notify_all();
                assert_eq!(
                    worker
                        .join()
                        .unwrap()
                        .unwrap()
                        .into_fixed()
                        .unwrap()
                        .as_ref(),
                    expected
                );
            });
            assert!(probe.peak.load(Ordering::SeqCst) <= slots as usize);
            assert!(
                probe.calls.load(Ordering::SeqCst) > slots as usize,
                "multiple batches must complete"
            );
            assert_eq!(
                budget.metrics().0,
                2520,
                "chunk reservations end before sampling"
            );
            assert_eq!(
                budget.metrics().2,
                0,
                "reducing a batch is not a rejected request"
            );
            drop(source);
            assert_eq!(budget.metrics().0, 0);
        }
    }
}
