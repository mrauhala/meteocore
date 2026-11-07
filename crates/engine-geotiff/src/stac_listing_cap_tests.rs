//! A STAC listing cut by its page or time cap (#1400), with the caps lowered
//! so a test reaches them without a thousand pages or ten minutes. Nothing
//! lists a window again once it holds an item, so a request path installs
//! nothing from a cut listing and a poll, whose next listing starts after
//! the newest item, installs what it read.

use super::*;
use crate::stac::{ListingLimits, StacListing};

const PAGE: usize = stac::STAC_PAGE_LIMIT as usize;

/// A cap of one page: a listing of more than [`PAGE`] items is cut.
fn one_page() -> ListingLimits {
    ListingLimits {
        max_pages: 1,
        ..ListingLimits::default()
    }
}

fn capped(mut engine: GeoTiffEngine, limits: ListingLimits) -> Arc<GeoTiffEngine> {
    set_limits(&mut engine, limits);
    Arc::new(engine)
}

fn set_limits(engine: &mut GeoTiffEngine, limits: ListingLimits) {
    let StoreMode::RemoteStac { client } = &mut engine.store_mode else {
        unreachable!("a STAC engine")
    };
    client.set_listing_limits(limits);
}

/// Lift the caps of an engine no request is running on.
fn uncap(engine: &mut Arc<GeoTiffEngine>) {
    let engine = Arc::get_mut(engine).expect("no request is running");
    set_limits(engine, ListingLimits::default());
}

/// A STAC API serving `count` items in pages, filtered by `datetime`.
fn paged() -> MockOptions {
    MockOptions {
        filter_datetime: true,
        paged: true,
        sized: true,
        ..MockOptions::default()
    }
}

fn catalog_times(engine: &GeoTiffEngine) -> Vec<DateTime<Utc>> {
    engine.catalog.load().entries.keys().copied().collect()
}

/// The ends of a listing: both caps cut only with a page left. A listing
/// whose last page is the cap's is complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_listing_is_cut_only_with_a_page_left() {
    let mock = mock_stac_with(PAGE, paged()).await;
    let engine = capped(
        stac_engine(mock.items_url.clone(), mock.allowlist.clone()),
        one_page(),
    );
    let StoreMode::RemoteStac { client } = &engine.store_mode else {
        unreachable!("a STAC engine")
    };
    match client.fetch_items_until(None, None, |_| false).unwrap() {
        StacListing::Complete(items) => assert_eq!(items.len(), PAGE),
        other => panic!("expected a complete listing, got {other:?}"),
    }

    let mock = mock_stac_with(PAGE + 1, paged()).await;
    let engine = capped(
        stac_engine(mock.items_url.clone(), mock.allowlist.clone()),
        one_page(),
    );
    let StoreMode::RemoteStac { client } = &engine.store_mode else {
        unreachable!("a STAC engine")
    };
    match client.fetch_items_until(None, None, |_| false).unwrap() {
        StacListing::Cut(items) => {
            let read: Vec<_> = items.iter().map(|item| item.datetime).collect();
            assert_eq!(read, mock.times[..PAGE], "sorted, as a complete listing");
        }
        other => panic!("expected a cut listing, got {other:?}"),
    }
    assert_eq!(pages(&mock), 1);
}

/// #1400: an EDR window whose listing the page cap cuts is a 400 that
/// installs nothing, so the next request lists it again and, the cap no
/// longer cutting it, sees its later items. Before, the items read so far
/// answered as the whole window, and it was never listed again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_the_page_cap_cuts_is_listed_again() {
    let count = PAGE + 1;
    let mock = mock_stac_with(count, paged()).await;
    let times = mock.times.clone();
    let mut engine = capped(
        stac_engine(mock.items_url.clone(), mock.allowlist.clone()),
        one_page(),
    );
    let window = (times[0], times[count - 1]);

    let refused = query_position(&engine, window).await;
    let msg = expect_too_large(refused, "did not finish");
    assert!(msg.ends_with("narrow the datetime window"), "{msg}");
    assert!(catalog_times(&engine).is_empty(), "nothing installed");
    assert_eq!(pages(&mock), 1);
    assert_eq!(asset_requests(&mock), 0, "nothing loaded");
    assert_eq!(stac_failures(&engine), 0, "the STAC API did not fail");

    // Nothing remembers the cut: every request pays for the listing again.
    expect_too_large(query_position(&engine, window).await, "did not finish");
    assert_eq!(pages(&mock), 2, "listed again by the next request");

    uncap(&mut engine);
    let response = query_position(&engine, window)
        .await
        .expect("the listing is no longer cut");
    assert_eq!(series_times(response), times, "with its later item");
    assert_eq!(pages(&mock), 2 + 2, "listed again from its first page");
}

/// #1400: the main way to reach a cap, a `max_files` catalog, which lists a
/// window in full. Before, the trim kept the newest of what was read, not of
/// the window, and the window was never listed again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_max_files_catalog_never_keeps_the_newest_of_a_cut_listing() {
    let count = PAGE + 1;
    let mock = mock_stac_with(count, paged()).await;
    let times = mock.times.clone();
    let mut engine = capped(
        stac_engine_with(mock.items_url.clone(), mock.allowlist.clone(), 0, Some(10)),
        one_page(),
    );
    let window = (times[0], times[count - 1]);

    expect_too_large(query_position(&engine, window).await, "did not finish");
    assert!(catalog_times(&engine).is_empty(), "nothing installed");

    uncap(&mut engine);
    let response = query_position(&engine, window)
        .await
        .expect("the listing is no longer cut");
    assert_eq!(series_times(response), times[count - 10..]);
}

/// #1400: a query without `datetime` on an empty catalog lists the last hour.
/// Its 400 tells it to add a window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_query_without_a_window_is_told_to_add_one() {
    let options = MockOptions {
        filter_datetime: false,
        ..paged()
    };
    let mock = mock_stac_with(PAGE + 1, options).await;
    let engine = capped(
        stac_engine(mock.items_url.clone(), mock.allowlist.clone()),
        one_page(),
    );

    let refused = query_position_at(&engine, None, None).await;
    let msg = expect_too_large(refused, "did not finish");
    assert!(msg.ends_with("add a datetime window"), "{msg}");
    assert!(catalog_times(&engine).is_empty(), "nothing installed");
}

/// #1400: the time cap is the page cap's case. The first page answers past
/// it, so the listing ends there with its second page unread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_window_the_time_cap_cuts_is_listed_again() {
    let items_gate = Arc::new(tokio::sync::Semaphore::new(0));
    let count = PAGE + 1;
    let options = MockOptions {
        items_gate: Some(Arc::clone(&items_gate)),
        ..paged()
    };
    let mock = mock_stac_with(count, options).await;
    let times = mock.times.clone();
    let total_timeout = Duration::from_secs(1);
    let mut engine = capped(
        stac_engine(mock.items_url.clone(), mock.allowlist.clone()),
        ListingLimits {
            total_timeout,
            ..ListingLimits::default()
        },
    );
    let window = (times[0], times[count - 1]);

    let query = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move { query_position(&engine, window).await }
    });
    until_requests(&mock, 1, pages).await;
    tokio::time::sleep(total_timeout + Duration::from_millis(100)).await;
    items_gate.close();

    expect_too_large(query.await.unwrap(), "did not finish");
    assert!(catalog_times(&engine).is_empty(), "nothing installed");
    assert_eq!(pages(&mock), 1, "the second page is never asked for");

    uncap(&mut engine);
    let response = query_position(&engine, window)
        .await
        .expect("the listing is no longer cut");
    assert_eq!(series_times(response), times, "with its later item");
    assert_eq!(pages(&mock), 1 + 2, "listed again from its first page");
}

/// #1400: a render lists only over an empty catalog, so it has no window a
/// client could narrow and nothing else to show. A cut listing is the 503 of
/// an unavailable source and installs nothing. It counts as a failed listing,
/// and the render after one that is not cut lists again and resets the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_render_over_a_cut_listing_is_unavailable_and_lists_again() {
    let count = PAGE + 1;
    let options = MockOptions {
        filter_datetime: false,
        ..paged()
    };
    let mock = mock_stac_with(count, options).await;
    let times = mock.times.clone();
    let mut engine = capped(
        stac_engine(mock.items_url.clone(), mock.allowlist.clone()),
        one_page(),
    );
    let time = times[count - 1];

    let err = failure(render_at(&engine, time, None).await.unwrap());
    assert!(matches!(err, DataServerError::Unavailable(_)), "{err:?}");
    assert!(
        err.class().is_transient(),
        "a 503 in WMS too, no error tile"
    );
    assert!(catalog_times(&engine).is_empty(), "nothing installed");
    assert_eq!(pages(&mock), 1);
    assert_eq!(stac_failures(&engine), 1);

    uncap(&mut engine);
    render_at(&engine, time, None)
        .await
        .unwrap()
        .expect("the listing is no longer cut");
    assert_eq!(catalog_times(&engine), times, "with its later item");
    assert_eq!(pages(&mock), 1 + 2, "listed again from its first page");
    assert_eq!(stac_failures(&engine), 0);
}

/// #1400: what repeated renders over a cut listing cost is bounded: they stop
/// listing at the circuit breaker's threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_renders_over_a_cut_listing_open_the_circuit_breaker() {
    let count = PAGE + 1;
    let options = MockOptions {
        filter_datetime: false,
        ..paged()
    };
    let mock = mock_stac_with(count, options).await;
    let engine = capped(
        stac_engine(mock.items_url.clone(), mock.allowlist.clone()),
        one_page(),
    );
    let time = mock.times[count - 1];

    for _ in 0..STAC_CIRCUIT_BREAKER_THRESHOLD {
        let err = failure(render_at(&engine, time, None).await.unwrap());
        assert!(matches!(err, DataServerError::Unavailable(_)), "{err:?}");
    }
    let threshold = STAC_CIRCUIT_BREAKER_THRESHOLD as usize;
    assert_eq!(pages(&mock), threshold);

    failure(render_at(&engine, time, None).await.unwrap());
    assert_eq!(pages(&mock), threshold, "no further listing");
}

/// #1400: a poll keeps installing what a cut listing read. It lists from the
/// newest entry on, so its next listing goes on after those items, here on a
/// host that lists oldest first; refusing them would freeze a catalog whose
/// backlog never fits the caps. It is no failure of the poll, either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_poll_installs_a_cut_listing_and_the_next_goes_on_after_it() {
    let count = PAGE + 2;
    let mock = mock_stac_with(count, paged()).await;
    let times = mock.times.clone();
    let engine = capped(
        stac_engine(mock.items_url.clone(), mock.allowlist.clone()),
        one_page(),
    );
    let href = format!("{}item-0.tif", mock.allowlist);
    let mut seeded = Catalog::empty();
    seeded.entries.insert(
        times[0],
        catalog::FileEntry::stac_stub(
            PathBuf::from(&href),
            0,
            catalog::StacStub {
                bbox: None,
                asset_url: href.clone(),
            },
        ),
    );
    engine.catalog.store(Arc::new(seeded));

    engine.poll_once();
    assert_eq!(catalog_times(&engine), times[..=PAGE], "the page it read");
    assert_eq!(pages(&mock), 1);
    assert_eq!(engine.consecutive_poll_failures.load(Ordering::SeqCst), 0);

    engine.poll_once();
    assert_eq!(
        catalog_times(&engine),
        times,
        "the next poll goes on after it"
    );
    assert_eq!(pages(&mock), 2);
}
