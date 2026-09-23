//! Same-chunk contention must not acquire a second decode workspace.
use super::*;

fn fixture(
    dir: &std::path::Path,
    budget: Arc<Budget>,
    outcome: Outcome,
) -> (
    Array<EngineStore>,
    DecodedArray,
    Arc<Probe>,
    mpsc::Receiver<()>,
) {
    let original = crate::decoded::tests::fixture(dir, "/a", false, 0.0);
    let (started, ready) = mpsc::channel();
    let probe = Arc::new(Probe {
        deadline: None,
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        started,
        open: Mutex::new(false),
        wake: Condvar::new(),
        budget,
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
    let array = crate::codec_limits::bounded_array(array).unwrap();
    let reader = DecodedArray::new(
        &array,
        Some("snapshot"),
        "a",
        Arc::new(DecodedCache::new(MIB)),
    )
    .unwrap();
    (array, reader, probe, ready)
}

fn release(probe: &Probe) {
    *probe.open.lock().unwrap() = true;
    probe.wake.notify_all();
}

#[test]
fn coalesced_reader_reserves_only_the_result_and_keeps_its_deadline() {
    let _exclusive = GATED_TESTS.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Two 24-byte source reservations + one 1252-byte cold allowance + a
    // 192-byte waiter pin. There is no room for a second 768-byte workspace.
    let budget = Arc::new(Budget::new(1492));
    let (array, reader, probe, ready) = fixture(dir.path(), budget.clone(), Outcome::Success);
    let subset = ArraySubset::new_with_ranges(&[0..1, 0..1, 0..1]);
    let options = crate::catalog::single_threaded_opts();
    let owner_source = budget.reserve(&array, &subset, Some(0), true).unwrap();
    let waiter_source = budget.reserve(&array, &subset, Some(0), true).unwrap();
    let (started, waiting) = mpsc::channel();
    let (done, result) = mpsc::channel();
    std::thread::scope(|scope| {
        let owner = scope.spawn(|| {
            let _encoded = crate::encoded::enter(Some(budget.clone()));
            reader.read(&array, &subset, &options, owner_source.parallelism())
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(budget.metrics(), (1300, 1492, 0));
        {
            let _encoded = crate::encoded::enter(Some(budget.clone()));
            let _deadline = deadline::enter(Some(Instant::now() + Duration::from_millis(20)));
            assert!(matches!(
                reader.read(&array, &subset, &options, waiter_source.parallelism()),
                Err(DataServerError::DeadlineExceeded)
            ));
        }
        // The waiter reached its own deadline rather than rejecting admission,
        // taking over the fill, or cancelling the owner's storage operation.
        assert_eq!(budget.metrics(), (1300, 1492, 0));
        assert_eq!(probe.active.load(Ordering::SeqCst), 1);
        scope.spawn(|| {
            let _encoded = crate::encoded::enter(Some(budget.clone()));
            started.send(()).unwrap();
            done.send(reader.read(&array, &subset, &options, waiter_source.parallelism()))
                .unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(budget.metrics(), (1300, 1492, 0));
        release(&probe);
        for bytes in [
            owner.join().unwrap().unwrap(),
            result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap(),
        ] {
            assert_eq!(bytes.into_fixed().unwrap().as_ref(), 0.0f32.to_ne_bytes());
        }
    });
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(reader.cache.metrics().misses, 1);
    assert_eq!(reader.cache.metrics().hits, 1);
    assert_eq!(budget.metrics(), (48, 1492, 0));
    drop((owner_source, waiter_source));
    assert_eq!(budget.metrics().0, 0);
}

#[test]
fn failed_or_panicking_owner_releases_admission_before_waiter_retries() {
    let _exclusive = GATED_TESTS.lock().unwrap();
    for outcome in [Outcome::Error, Outcome::Panic] {
        let dir = tempfile::tempdir().unwrap();
        // Exactly one cold allowance plus both source buffers. A successor
        // cannot reserve until the failed owner's workspace/headroom is gone.
        let budget = Arc::new(Budget::new(1300));
        let (array, reader, probe, ready) = fixture(dir.path(), budget.clone(), outcome);
        let subset = ArraySubset::new_with_ranges(&[0..1, 0..1, 0..1]);
        let options = crate::catalog::single_threaded_opts();
        let owner_source = budget.reserve(&array, &subset, Some(0), true).unwrap();
        let waiter_source = budget.reserve(&array, &subset, Some(0), true).unwrap();
        let (started, waiting) = mpsc::channel();
        let (done, result) = mpsc::channel();
        std::thread::scope(|scope| {
            let owner = scope.spawn(|| {
                let _encoded = crate::encoded::enter(Some(budget.clone()));
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reader.read(&array, &subset, &options, owner_source.parallelism())
                }))
            });
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            scope.spawn(|| {
                let _encoded = crate::encoded::enter(Some(budget.clone()));
                started.send(()).unwrap();
                done.send(reader.read(&array, &subset, &options, waiter_source.parallelism()))
                    .unwrap();
            });
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                result.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            assert_eq!(budget.metrics(), (1300, 1300, 0));
            release(&probe);
            let failed = owner.join().unwrap();
            match outcome {
                Outcome::Panic => assert!(failed.is_err()),
                _ => assert!(failed.unwrap().is_err()),
            }
            assert_eq!(
                result
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap()
                    .into_fixed()
                    .unwrap()
                    .as_ref(),
                0.0f32.to_ne_bytes()
            );
        });
        assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
        assert_eq!(probe.peak.load(Ordering::SeqCst), 1);
        assert_eq!(reader.cache.metrics().misses, 2);
        assert_eq!(budget.metrics(), (48, 1300, 0));
        drop((owner_source, waiter_source));
        assert_eq!(budget.metrics().0, 0);
    }
}

#[test]
fn successor_with_only_pin_capacity_rejects_before_decoding_and_releases_claim() {
    let _exclusive = GATED_TESTS.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let budget = Arc::new(Budget::new(1276));
    let (array, reader, probe, ready) = fixture(dir.path(), budget.clone(), Outcome::Error);
    let subset = ArraySubset::new_with_ranges(&[0..1, 0..1, 0..1]);
    let options = crate::catalog::single_threaded_opts();
    let owner_source = budget.reserve(&array, &subset, Some(0), true).unwrap();
    let waiter_budget = Arc::new(Budget::new(216));
    let waiter_source = waiter_budget
        .reserve(&array, &subset, Some(0), true)
        .unwrap();
    let (done, result) = mpsc::channel();
    std::thread::scope(|scope| {
        let owner = scope.spawn(|| {
            let _encoded = crate::encoded::enter(Some(budget.clone()));
            reader.read(&array, &subset, &options, owner_source.parallelism())
        });
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        scope.spawn(|| {
            let _encoded = crate::encoded::enter(Some(waiter_budget.clone()));
            done.send(reader.read(&array, &subset, &options, waiter_source.parallelism()))
                .unwrap();
        });
        assert!(matches!(
            result.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release(&probe);
        assert!(owner.join().unwrap().is_err());
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(DataServerError::ResourceExhausted)
        ));
    });
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(reader.cache.metrics().misses, 1);
    assert_eq!(waiter_budget.metrics(), (24, 216, 1));
    let _encoded = crate::encoded::enter(Some(budget.clone()));
    assert!(reader.read(&array, &subset, &options, 1).is_ok());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    assert_eq!(reader.cache.metrics().misses, 2);
    drop((owner_source, waiter_source));
    assert_eq!(budget.metrics().0, 0);
    assert_eq!(waiter_budget.metrics().0, 0);
}

#[test]
fn queued_fill_runs_before_waiting_for_another_owner() {
    let _exclusive = GATED_TESTS.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let budget = Arc::new(Budget::new(MIB));
    let (array, reader, probe, ready) = fixture(dir.path(), budget.clone(), Outcome::Success);
    let subset = ArraySubset::new_with_ranges(&[0..1, 0..1, 0..7]);
    let options = crate::catalog::single_threaded_opts();
    let source = budget.reserve(&array, &subset, Some(0), true).unwrap();
    let other = reader
        .prepare_chunk(&array, &subset, vec![0, 0, 1], 4, Duration::ZERO)
        .unwrap()
        .unwrap();
    let fill = other.fill.unwrap();
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let _encoded = crate::encoded::enter(Some(budget.clone()));
            reader.read(&array, &subset, &options, source.parallelism())
        });
        // The second chunk belongs to `fill`, but the first must reach storage
        // before it is published. Waiting with the queued first claim would
        // deadlock a second request that needs that first chunk to make progress.
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        release(&probe);
        fill.insert(Arc::new((-999.0f32).to_ne_bytes().repeat(8)))
            .unwrap();
        let bytes = worker.join().unwrap().unwrap().into_fixed().unwrap();
        let expected: Vec<u8> = [0.0f32, 1.0, 2.0, 3.0, 4.0, 5.0, -999.0]
            .into_iter()
            .flat_map(f32::to_ne_bytes)
            .collect();
        assert_eq!(bytes.as_ref(), expected);
    });
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    assert_eq!(reader.cache.metrics().misses, 1);
    assert_eq!(reader.cache.metrics().hits, 1);
    drop(source);
    assert_eq!(budget.metrics(), (0, MIB, 0));
}
