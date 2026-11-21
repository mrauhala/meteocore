//! An EDR query's composites, read a few at a time (#1495): against a store
//! whose every HEAD and GET takes a set time, a position query over 24
//! remote composites keeps eight reads in flight and takes about 24 ÷ 8
//! reads, not 24. Each frame of a position and an area answer sits at its
//! own step, a file that does not decode a null one among them. Past the
//! deadline no further read starts and the query is `DeadlineExceeded`, the
//! 504 of EDR. A local catalog reads one composite at a time. Every read
//! past a query's first reserves its bytes from the read budget, and one
//! that finds no room waits for the unbudgeted read instead, never refused.
//!
//! The one decodable composite fixture is the DMI one, so the decodable
//! steps of an answer hold the same file: placement shows in where the
//! undecodable steps' nulls fall.

use std::time::{Duration, Instant};

use ds_core::edr_engine::EdrEngine;
use ds_core::model::{CoverageResponse, DomainDescription};
use ds_storage::test_util::SlowReads;

use super::composite_read_failure_tests::{echo, FIXTURE};
use super::*;
use crate::catalog::CatalogEntry;
use crate::edr::{frame_reads, Admitted, FrameAdmission, MAX_FRAME_READS};
use crate::engine::CompositeReader;
use crate::read_budget::Budget;

/// Composites of a query in the tests: as many as the issue's case.
const FRAMES: usize = 24;

/// Whether frame `i` holds bytes that do not decode: a null step, read
/// like any other but never cached. Three in four, so the tests put few
/// composites in the process-wide cache, and not symmetric in time, so an
/// answer in reverse order differs too.
fn undecodable(i: usize) -> bool {
    i % 4 != 3
}

/// The `i`th of [`FRAMES`] timesteps, five minutes apart from 10:00.
fn time_of(i: usize) -> DateTime<Utc> {
    "2026-01-20T10:00:00Z".parse::<DateTime<Utc>>().unwrap()
        + chrono::Duration::minutes(5 * i as i64)
}

/// An engine over [`FRAMES`] composites in `store` under `prefix`, a prefix
/// of the test's own (the composite cache is process-wide and keyed by the
/// object key): the DMI fixture, but bytes that do not decode at each
/// [`undecodable`] frame. The newest, a fixture, is the seed the engine
/// reads and caches at construction. The store answers at once until a
/// test sets its delay.
fn engine_over(store: &Arc<SlowReads>, prefix: &str) -> OdimEngine {
    let fixture = std::fs::read(FIXTURE).unwrap();
    for i in 0..FRAMES {
        let key = format!("{prefix}{}_radar.h5", time_of(i).format("%Y%m%dT%H%M"));
        let bytes = match undecodable(i) {
            true => vec![0x55; fixture.len()],
            false => fixture.clone(),
        };
        store.put(&key, bytes);
    }
    assert!(!undecodable(FRAMES - 1), "the seed decodes");
    let config = super::tests::config_with(None, None, None, None);
    let engine = OdimEngine::new_remote_for_test(
        "frame-reads",
        ds_storage::DataStore::new(store.clone()),
        prefix,
        "reflectivity",
        "dBZ",
        &config,
    )
    .expect("the engine reads its seed composite");
    assert_eq!(engine.catalog.load().len(), FRAMES);
    engine
}

/// The `t` axis and values of a single coverage's one range.
fn series(response: CoverageResponse) -> (Vec<DateTime<Utc>>, Vec<Option<f64>>) {
    let CoverageResponse::Single(result) = response else {
        panic!("a single coverage");
    };
    let t = match result.domain {
        DomainDescription::PointSeries { t, .. } => t,
        DomainDescription::Grid { t, .. } => t.unwrap(),
        other => panic!("{other:?}"),
    };
    (t, result.ranges["reflectivity"].values.clone())
}

/// `(from, to)` over frame `i` alone, or every frame.
fn window(i: Option<usize>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    i.map(|i| (time_of(i), time_of(i)))
}

/// The acceptance case of #1495: a position query over 24 remote
/// composites, every HEAD and GET taking 100 ms, keeps eight reads in
/// flight and answers in about three composite reads (the seed is cached,
/// 23 are read). One at a time took 23 (4.6 s); the bound here, half of
/// that, is loose enough for a loaded CI host. A file that does not decode
/// is a null at its own step.
#[test]
fn a_position_query_reads_its_composites_eight_at_a_time() {
    let _held = hold_composite_cache_for_test();
    let store = Arc::new(SlowReads::default());
    let engine = engine_over(&store, "frame-reads-1495-position/");
    assert_eq!(frame_reads(&engine.catalog.load()), MAX_FRAME_READS);
    let (lon, lat) = echo(&engine);
    let delay = Duration::from_millis(100);
    store.reset();
    store.set_delay(delay);

    let started = Instant::now();
    let point = format!("POINT({lon} {lat})");
    let (times, values) = series(
        engine
            .query_position(&point, None, None, None, None)
            .unwrap(),
    );
    let elapsed = started.elapsed();

    assert_eq!(times, (0..FRAMES).map(time_of).collect::<Vec<_>>());
    let read = FRAMES - 1;
    assert_eq!(store.reads(), 2 * read, "a HEAD and a GET per composite");
    assert_eq!(store.peak(), MAX_FRAME_READS, "composites in flight");
    assert_eq!(store.fetch_peak(), MAX_FRAME_READS, "GETs in flight");
    let composite = 2 * delay;
    let rounds = read.div_ceil(MAX_FRAME_READS) as u32;
    assert!(
        elapsed >= composite * rounds && elapsed < composite * read as u32 / 2,
        "{elapsed:?} for {read} composites of {composite:?}, {MAX_FRAME_READS} at a time"
    );
    let echo = values[FRAMES - 1];
    assert!(echo.is_some(), "the echo is data");
    for (i, value) in values.iter().enumerate() {
        match undecodable(i) {
            true => assert_eq!(*value, None, "frame {i}"),
            false => assert_eq!(*value, echo, "frame {i}"),
        }
    }
}

/// Each step of a 24-composite area answer, read eight at a time, is what
/// a query of that composite alone answers: the echo's cells, or nulls for
/// a file that does not decode. The area is small enough that 24 steps of
/// it stay at the composite's resolution (#1436), as one does.
#[test]
fn every_area_step_is_its_own_composite() {
    let _held = hold_composite_cache_for_test();
    let store = Arc::new(SlowReads::default());
    let engine = engine_over(&store, "frame-reads-1495-area/");
    let (x, y) = echo(&engine);
    let [w, s, e, n] = engine.seed_spatial_extent;
    let (dx, dy) = ((e - w) / 32.0, (n - s) / 32.0);
    let (x0, x1, y0, y1) = (x - dx, x + dx, y - dy, y + dy);
    let area = format!("POLYGON(({x0} {y0},{x1} {y0},{x1} {y1},{x0} {y1},{x0} {y0}))");

    let CoverageResponse::Single(all) = engine.query_area(&area, None, None, None, None).unwrap()
    else {
        panic!("a single coverage");
    };
    assert!(
        matches!(
            all.domain,
            DomainDescription::Grid {
                coarsened: None,
                ..
            }
        ),
        "at the composite's resolution"
    );
    let (times, cells) = series(CoverageResponse::Single(all));
    assert_eq!(times.len(), FRAMES);
    let steps: Vec<_> = cells.chunks(cells.len() / FRAMES).collect();
    assert!(steps[FRAMES - 1].iter().any(Option::is_some), "the echo");
    for (i, step) in steps.iter().enumerate() {
        let (_, alone) = series(
            engine
                .query_area(&area, window(Some(i)), None, None, None)
                .unwrap(),
        );
        assert_eq!(alone, *step, "frame {i}");
        assert_eq!(
            step.iter().all(Option::is_none),
            undecodable(i),
            "frame {i}"
        );
    }
}

/// Past the deadline no further composite is read: the eight HEADs in
/// flight end at it, nothing else starts, and the query is
/// `DeadlineExceeded`, which EDR answers with 504.
#[test]
fn a_query_past_its_deadline_starts_no_more_reads() {
    let store = Arc::new(SlowReads::default());
    let engine = engine_over(&store, "frame-reads-1495-deadline/");
    let (lon, lat) = echo(&engine);
    store.reset();
    store.set_delay(Duration::from_millis(500));

    let _deadline = ds_core::deadline::enter(Some(Instant::now() + Duration::from_millis(100)));
    let point = format!("POINT({lon} {lat})");
    let answer = engine.query_position(&point, None, None, None, None);
    assert!(
        matches!(answer, Err(DataServerError::DeadlineExceeded)),
        "{answer:?}"
    );
    let reads = store.reads();
    assert!(
        (1..=MAX_FRAME_READS).contains(&reads),
        "{reads} reads started"
    );
    assert_eq!(
        store.active.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "none left running"
    );
}

/// With no room in the read budget, a position query over 24 remote
/// composites fetches them one at a time, each GET waiting for the one
/// before it to decode, and answers in full: no read is refused, nor
/// counted as a rejection. In a process of its own at
/// `MC_PVOL_READ_MEMORY_MB=0`; the HEADs still overlap.
#[test]
fn without_room_in_the_budget_composites_are_read_one_at_a_time() {
    const CHILD: &str = "MC_COMPOSITE_FRAME_BUDGET_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::composite_frame_read_tests::without_room_in_the_budget_composites_are_read_one_at_a_time",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("MC_PVOL_READ_MEMORY_MB", "0")
            .status()
            .unwrap();
        assert!(status.success(), "the zero-budget run failed");
        return;
    }
    assert_eq!(crate::read_budget::metrics().1, 0, "a zero budget");
    let store = Arc::new(SlowReads::default());
    let engine = engine_over(&store, "frame-reads-1495-no-room/");
    let (lon, lat) = echo(&engine);
    let delay = Duration::from_millis(50);
    store.reset();
    store.set_delay(delay);

    let started = Instant::now();
    let point = format!("POINT({lon} {lat})");
    let (times, values) = series(
        engine
            .query_position(&point, None, None, None, None)
            .unwrap(),
    );
    let elapsed = started.elapsed();

    assert_eq!(times.len(), FRAMES);
    for (i, value) in values.iter().enumerate() {
        assert_eq!(value.is_none(), undecodable(i), "frame {i}");
    }
    let read = FRAMES - 1;
    assert_eq!(store.reads(), 2 * read, "a HEAD and a GET per composite");
    assert_eq!(store.fetch_peak(), 1, "GETs in flight");
    assert!(store.peak() > 1, "the HEADs overlap");
    assert!(elapsed >= delay * read as u32, "{elapsed:?}");
    assert_eq!(crate::read_budget::metrics().2, 0, "no rejection");
}

/// A query's first read in flight is unbudgeted; each beside it reserves
/// its file and the decode, given back when the read ends.
#[test]
fn reads_beside_the_first_reserve_their_file_and_decode() {
    let budget = Arc::new(Budget::new(1 << 30));
    let admission = FrameAdmission::new(budget.clone(), 1_000);

    let first = admission.admit(10).unwrap();
    assert!(matches!(first, Admitted::Unbudgeted { .. }));
    assert_eq!(budget.metrics().0, 0);
    let second = admission.admit(20).unwrap();
    assert!(matches!(second, Admitted::Reserved { .. }));
    let third = admission.admit(30).unwrap();
    assert_eq!(budget.metrics().0, 1_020 + 1_030);

    drop(second);
    assert_eq!(budget.metrics().0, 1_030);
    drop(first);
    let again = admission.admit(40).unwrap();
    assert!(matches!(again, Admitted::Unbudgeted { .. }), "given back");
    drop((third, again));
    assert_eq!(budget.metrics(), (0, 1 << 30, 0));
}

/// A read that finds no room in the budget, which keeps half of itself for
/// requests, waits for the unbudgeted read to be given back and takes it:
/// never refused, nor counted as a rejection. Past the request's deadline
/// the wait is `DeadlineExceeded`.
#[test]
fn a_read_without_room_waits_for_the_unbudgeted_read() {
    let budget = Arc::new(Budget::new(2_000));
    let admission = FrameAdmission::new(budget.clone(), 1_000);
    let first = admission.admit(10).unwrap();
    let _reserved = admission.admit(0).unwrap();
    assert_eq!(budget.metrics().0, 1_000, "half the budget");

    let waiter = {
        let admission = admission.clone();
        std::thread::spawn(move || {
            let admitted = admission.admit(10).unwrap();
            (
                matches!(admitted, Admitted::Unbudgeted { .. }),
                Instant::now(),
            )
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    let given_back = Instant::now();
    drop(first);
    let (unbudgeted, admitted) = waiter.join().unwrap();
    assert!(unbudgeted, "it takes the unbudgeted read");
    assert!(admitted >= given_back, "only once it is given back");
    assert_eq!(budget.metrics().2, 0, "no rejection");

    let _first = admission.admit(10).unwrap();
    let _deadline = ds_core::deadline::enter(Some(Instant::now() + Duration::from_millis(50)));
    let started = Instant::now();
    assert!(matches!(
        admission.admit(10),
        Err(DataServerError::DeadlineExceeded)
    ));
    assert!(started.elapsed() >= Duration::from_millis(50));
}

/// A local catalog reads one composite at a time: a local file has no
/// round trip to overlap. A remote one reads eight.
#[test]
fn a_local_catalog_reads_one_composite_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    for time in ["1125", "1130"] {
        std::fs::copy(
            FIXTURE,
            dir.path().join(format!("20260120T{time}_radar.h5")),
        )
        .unwrap();
    }
    let config = super::tests::config_with(None, None, None, None);
    let local = OdimEngine::new("frame-reads-local", dir.path().to_str(), &config).unwrap();
    assert_eq!(frame_reads(&local.catalog.load()), 1);

    let store = Arc::new(SlowReads::default());
    let remote = engine_over(&store, "frame-reads-1495-remote/");
    assert_eq!(frame_reads(&remote.catalog.load()), MAX_FRAME_READS);
}

/// Wait until `store` has started `count` reads, at most ten seconds.
fn wait_for_reads(store: &SlowReads, count: usize) {
    let until = Instant::now() + Duration::from_secs(10);
    while store.reads() < count && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(store.reads() >= count, "{} reads started", store.reads());
}

/// The composites of `engine`'s catalog not yet read: every one but the
/// seed, which the engine read at construction.
fn cold_entries(engine: &OdimEngine) -> Vec<CatalogEntry> {
    let catalog = engine.catalog.load();
    catalog[..catalog.len() - 1].to_vec()
}

/// Read `entries` as an EDR query does, through `reader`, eight at a time,
/// each read past the first admitted by `budget`, under a deadline `within`
/// from now; a file that does not decode is no failure, as it is a null
/// step in an answer.
fn read_as_a_query(
    reader: CompositeReader,
    entries: Vec<CatalogEntry>,
    budget: &Arc<Budget>,
    decode_bytes: u64,
    within: Duration,
) -> Result<Duration, DataServerError> {
    let _deadline = ds_core::deadline::enter(Some(Instant::now() + within));
    let admission = FrameAdmission::new(budget.clone(), decode_bytes);
    let started = Instant::now();
    crate::edr::read_frames(
        MAX_FRAME_READS,
        entries,
        move |entry| match reader
            .load(&entry.location, &admission)
            .map_err(LoadError::for_query)
        {
            Err(e) if e.class().is_transient() => Err(e),
            _ => Ok(()),
        },
        |_, ()| {},
    )?;
    Ok(started.elapsed())
}

/// Two queries over the same composites, one in each order, the budget
/// full and the store slow: each reads one composite at a time and shares
/// the other's fills, and both finish well inside their deadline. A query's
/// unbudgeted read only reads and decodes, so this holds with the
/// admission inside the fill too: what admission outside the fill buys is
/// pinned by `a_query_waiting_for_the_budget_holds_no_fill_others_wait_for`.
#[test]
fn two_queries_in_opposite_order_finish_well_inside_the_deadline() {
    let _held = hold_composite_cache_for_test();
    let store = Arc::new(SlowReads::default());
    let engine = engine_over(&store, "frame-reads-1495-opposite/");
    let budget = Arc::new(Budget::new(0));
    let decode = engine.seed_decode_bytes;
    let ascending = cold_entries(&engine);
    let descending: Vec<_> = ascending.iter().rev().cloned().collect();
    let delay = Duration::from_millis(50);
    store.reset();
    store.set_delay(delay);

    let deadline = Duration::from_secs(20);
    let (a, b) = std::thread::scope(|scope| {
        let (a, b) = (engine.composite_reader(), engine.composite_reader());
        let a = scope.spawn(|| read_as_a_query(a, ascending, &budget, decode, deadline));
        let b = scope.spawn(|| read_as_a_query(b, descending, &budget, decode, deadline));
        (a.join().unwrap(), b.join().unwrap())
    });
    // One query alone, one GET at a time: 23 HEADs and GETs, 2.3 s.
    let alone = 2 * delay * (FRAMES - 1) as u32;
    for (order, took) in [("ascending", a), ("descending", b)] {
        let took = took.unwrap_or_else(|e| panic!("{order}: {e:?}"));
        assert!(took < 2 * alone, "{order}: {took:?}");
    }
    assert_eq!(budget.metrics().2, 0, "no rejection");
}

/// While a query's reads wait for its unbudgeted read, the budget being
/// full, map renders of the same composites are not held up behind them:
/// each claims its fill, or joins one in flight, and answers within a
/// render's deadline. The query's reads hold no fill claim while they
/// wait; with the admission inside the fill, these renders time out.
#[test]
fn a_query_waiting_for_the_budget_holds_no_fill_others_wait_for() {
    let _held = hold_composite_cache_for_test();
    let store = Arc::new(SlowReads::default());
    let engine = engine_over(&store, "frame-reads-1495-no-fill-held/");
    let budget = Arc::new(Budget::new(0));
    let decode = engine.seed_decode_bytes;
    let entries = cold_entries(&engine);
    let first: Vec<_> = entries[..MAX_FRAME_READS].to_vec();
    let delay = Duration::from_millis(200);
    store.reset();
    store.set_delay(delay);

    std::thread::scope(|scope| {
        let reader = engine.composite_reader();
        let query = scope
            .spawn(|| read_as_a_query(reader, entries, &budget, decode, Duration::from_secs(30)));
        // The query's first eight reads are under way: their HEADs.
        wait_for_reads(&store, MAX_FRAME_READS);
        let renders: Vec<_> = first
            .iter()
            .map(|entry| {
                let engine = &engine;
                scope.spawn(move || {
                    let deadline = Instant::now() + 6 * delay;
                    let _deadline = ds_core::deadline::enter(Some(deadline));
                    engine.load_composite(&entry.location).map(drop)
                })
            })
            .collect();
        // A file that does not decode is an error of its own, never the
        // deadline.
        for (i, render) in renders.into_iter().enumerate() {
            match (undecodable(i), render.join().unwrap()) {
                (false, Ok(())) => {}
                (true, Err(e)) if !e.class().is_transient() => {}
                (_, answer) => panic!("composite {i}: {answer:?}"),
            }
        }
        query.join().unwrap().expect("the query answers");
    });
}

/// A query whose composites other requests are already filling joins those
/// fills holding nothing: no HEAD of its own, and no reservation, though
/// its unbudgeted read is taken and a read of its own would reserve.
#[test]
fn a_query_joining_fills_in_progress_takes_no_head_and_no_reservation() {
    let _held = hold_composite_cache_for_test();
    let store = Arc::new(SlowReads::default());
    let engine = engine_over(&store, "frame-reads-1495-joins/");
    // Composites that decode, so the fills joined succeed.
    let joined: Vec<_> = cold_entries(&engine)
        .into_iter()
        .enumerate()
        .filter_map(|(i, entry)| (!undecodable(i)).then_some(entry))
        .take(4)
        .collect();
    let budget = Arc::new(Budget::new(1 << 30));
    let delay = Duration::from_millis(200);
    store.reset();
    store.set_delay(delay);

    std::thread::scope(|scope| {
        let renders: Vec<_> = joined
            .iter()
            .map(|entry| {
                let engine = &engine;
                scope.spawn(move || engine.load_composite(&entry.location).map(drop))
            })
            .collect();
        // The renders' fills are claimed once their HEADs are under way.
        wait_for_reads(&store, joined.len());
        let admission = FrameAdmission::new(budget.clone(), engine.seed_decode_bytes);
        let _unbudgeted = admission.admit(0).unwrap();
        let reader = engine.composite_reader();
        let reads = crate::edr::read_frames(
            MAX_FRAME_READS,
            joined.clone(),
            move |entry| {
                reader
                    .load(&entry.location, &admission)
                    .map(drop)
                    .map_err(LoadError::for_query)
            },
            |_, ()| {},
        );
        reads.expect("the query answers");
        for render in renders {
            render.join().unwrap().expect("a render answers");
        }
    });
    assert_eq!(
        store.reads(),
        2 * joined.len(),
        "the renders' HEADs and GETs only"
    );
    assert_eq!(budget.take_peak(), 0, "no reservation");
}
