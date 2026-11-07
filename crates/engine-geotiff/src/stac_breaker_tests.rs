//! The STAC circuit breaker counts failures of the STAC API, never a
//! request the host refused, and guards only the on-demand listings: while
//! it is open, a request the catalog answers is served.

use super::*;

/// A STAC API that lists only the items in a request's `datetime` interval
/// and refuses one it cannot read, as a STAC API does.
fn filtering() -> MockOptions {
    MockOptions {
        filter_datetime: true,
        ..MockOptions::default()
    }
}

const THRESHOLD: usize = STAC_CIRCUIT_BREAKER_THRESHOLD as usize;

/// An EDR window open at one end lists that end open, `..` as the STAC API
/// writes it, so the host answers it. Written as the date EDR parses an
/// open end into, it was a date the host refused, and each refusal counted
/// as a failure of the STAC API.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_ended_windows_are_listed_and_leave_the_breaker_closed() {
    let mock = mock_stac_with(3, filtering()).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    render_at(&engine, times[2], None)
        .await
        .unwrap()
        .expect("the render lists its hour");
    let listed = pages(&mock);

    let hour = chrono::Duration::hours(1);
    let before = (DateTime::<Utc>::MIN_UTC, times[0] - hour);
    let after = (times[2] + hour, DateTime::<Utc>::MAX_UTC);
    for _ in 0..THRESHOLD {
        for window in [before, after] {
            let err = failure(query_position(&engine, window).await);
            assert!(
                matches!(err, DataServerError::LocationNotFound(_)),
                "the host answers the window with no item: {err:?}"
            );
        }
    }
    assert_eq!(pages(&mock), listed + 2 * THRESHOLD, "each window listed");
    assert_eq!(stac_failures(&engine), 0);

    render_at(&engine, times[1], None)
        .await
        .unwrap()
        .expect("the catalog's frames are served");
}

/// A request the STAC API refuses with a 4xx says nothing of its health:
/// a client shapes an EDR window, and a render lists the hour of its time.
/// Each fails as before but counts no failure, and the next request
/// reaches the host again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_the_host_refuses_is_no_failure_of_the_stac_api() {
    let refusing = Arc::new(AtomicBool::new(false));
    let options = MockOptions {
        refusing: Some(Arc::clone(&refusing)),
        ..filtering()
    };
    let mock = mock_stac_with(3, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    render_at(&engine, times[2], None)
        .await
        .unwrap()
        .expect("the render lists its hour");
    refusing.store(true, Ordering::SeqCst);

    let hour = chrono::Duration::hours(1);
    let elsewhere = (times[2] + hour, times[2] + hour * 2);
    let listed = pages(&mock);
    for _ in 0..=THRESHOLD {
        let err = failure(query_position(&engine, elsewhere).await);
        assert!(matches!(err, DataServerError::Engine(_)), "{err:?}");
    }
    assert_eq!(
        pages(&mock),
        listed + THRESHOLD + 1,
        "every request reaches the host"
    );
    assert_eq!(stac_failures(&engine), 0);

    // A render lists only over an empty catalog.
    let empty = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    let listed = pages(&mock);
    for _ in 0..=THRESHOLD {
        let err = failure(render_at(&empty, times[2], None).await.unwrap());
        assert!(matches!(err, DataServerError::Engine(_)), "{err:?}");
    }
    assert_eq!(pages(&mock), listed + THRESHOLD + 1);
    assert_eq!(stac_failures(&empty), 0);
}

/// The circuit breaker guards the on-demand listings, the requests it
/// counts. While it is open, a render or an EDR query the catalog answers
/// is served, a stub loading its header as usual. A request that needs a
/// listing fails at once, without one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_breaker_leaves_the_catalog_served() {
    let down = Arc::new(AtomicBool::new(false));
    let options = MockOptions {
        host_down: Some(Arc::clone(&down)),
        ..filtering()
    };
    let mock = mock_stac_with(3, options).await;
    let times = mock.times.clone();
    let engine = Arc::new(stac_engine(mock.items_url.clone(), mock.allowlist.clone()));
    render_at(&engine, times[2], None)
        .await
        .unwrap()
        .expect("the render lists its hour");
    assert!(
        !engine.catalog.load().entries[&times[1]].is_loaded(),
        "listed, not loaded"
    );

    down.store(true, Ordering::SeqCst);
    let hour = chrono::Duration::hours(1);
    let elsewhere = (times[2] + hour, times[2] + hour * 2);
    for _ in 0..THRESHOLD {
        failure(query_position(&engine, elsewhere).await);
    }
    assert_eq!(stac_failures(&engine), STAC_CIRCUIT_BREAKER_THRESHOLD);
    let listed = pages(&mock);
    failure(query_position(&engine, elsewhere).await);
    assert_eq!(pages(&mock), listed, "an open breaker lists nothing");

    render_at(&engine, times[1], None)
        .await
        .unwrap()
        .expect("a frame of the catalog renders");
    let response = query_position(&engine, (times[0], times[2]))
        .await
        .expect("the catalog answers");
    assert_eq!(series_times(response), times);
    assert_eq!(pages(&mock), listed, "neither lists");
}
