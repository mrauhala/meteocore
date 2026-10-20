//! Remote reads against a local HTTP server answering like thredds.met.no's
//! fileServer ([`crate::test_server`]), over the synthetic MEPS-like files
//! of testdata/netcdf-remote: request counts, the 206 path, single-flight,
//! detached fills, a replaced file, prewarm, the bounded first scan, and
//! Critical Rule 7's runtime contexts.
//!
//! The fixture files are 62 KB, so the read sizes are production's divided
//! by 64 ([`TUNING`]): a 16 KiB first read covers the metadata, as 1 MiB
//! does a MEPS file's, and each 15 KB field chunk is a chunk read, as a
//! 1.5 MB MEPS chunk is.

use std::sync::{Arc, Barrier, LazyLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, DurationRound, Utc};
use ds_core::config::NetcdfConfig;
use ds_core::deadline;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs};

use super::*;
use crate::test_server::{gets, thredds_catalog, Request, TestServer};
use crate::NetcdfEngine;

const TUNING: Tuning = Tuning {
    block: 1024,
    first_read: 16 * 1024,
    bypass_over: 4 * 1024,
};

/// The fixture's run and its two leads' valid times.
const RUN: &str = "2026-10-15T06:00:00Z";
const LEAD_0: &str = "2026-10-15T06:00:00Z";
const LEAD_1: &str = "2026-10-15T07:00:00Z";

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// When the tests started, on the second: the run slots and modification
/// times derive from it, so they agree within a test whatever the clock
/// does meanwhile.
static START: LazyLock<DateTime<Utc>> = LazyLock::new(|| {
    Utc::now()
        .duration_trunc(chrono::Duration::seconds(1))
        .unwrap()
});

/// The run slot the files are published under: an hour before the start,
/// on the hour, in every engine's three-hour window.
fn slot() -> DateTime<Utc> {
    (*START - chrono::Duration::hours(1))
        .duration_trunc(chrono::Duration::hours(1))
        .unwrap()
}

/// An hour before the start: old enough to settle on sight.
fn old() -> DateTime<Utc> {
    *START - chrono::Duration::hours(1)
}

fn fixture(lead: u32) -> Vec<u8> {
    let path = format!(
        "{}/../../testdata/netcdf-remote/meps_sfc_{lead:02}_20261015T06Z.nc",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(path).unwrap()
}

/// `bytes` with every little-endian `f64` of a time in `moves` replaced:
/// moves a fixture's run or valid time, which are stored uncompressed.
fn retimed(mut bytes: Vec<u8>, moves: &[(&str, &str)]) -> Vec<u8> {
    let original = bytes.clone();
    for (from, to) in moves {
        let (from, to) = (
            (at(from).timestamp() as f64).to_le_bytes(),
            (at(to).timestamp() as f64).to_le_bytes(),
        );
        for i in 0..bytes.len() - 8 {
            if bytes[i..i + 8] == from {
                bytes[i..i + 8].copy_from_slice(&to);
            }
        }
    }
    assert_ne!(bytes, original);
    bytes
}

/// The key of `lead` in the run slot `run`, as on thredds.met.no.
fn key(run: DateTime<Utc>, lead: u32) -> String {
    format!(
        "thredds/fileServer/meps25epsarchive/{}/member_00/meps_sfc_{lead:02}_{}Z.nc",
        run.format("%Y/%m/%d/%H"),
        run.format("%Y%m%dT%H")
    )
}

/// A server with the fixture's two leads in [`slot`].
fn server() -> TestServer {
    let server = TestServer::new();
    for lead in [0, 1] {
        server.put(&key(slot(), lead), fixture(lead), old());
    }
    server
}

fn config(server: &TestServer, extra: &str) -> NetcdfConfig {
    let hours: Vec<String> = (0..24).map(|h| h.to_string()).collect();
    toml::from_str(&format!(
        r#"data_path = "{}/thredds/fileServer/meps25epsarchive/"
           prefix_pattern = "%Y/%m/%d/{{run}}/member_00/"
           filename_template = "meps_sfc_{{lead}}_%Y%m%dT{{run}}Z.nc"
           run_hours = [{}]
           leads = [0, 1]
           time_window = "-PT3H"
           {extra}"#,
        server.url,
        hours.join(", ")
    ))
    .unwrap()
}

fn engine(server: &TestServer, extra: &str) -> NetcdfEngine {
    NetcdfEngine::with_tuning("remote", &config(server, extra), TUNING).unwrap()
}

/// The same files read locally.
fn local() -> NetcdfEngine {
    let config: NetcdfConfig = toml::from_str(&format!(
        r#"data_path = "{}/../../testdata/netcdf-remote"
           filename_template = "meps_sfc_{{lead}}_%Y%m%dT{{run}}Z.nc""#,
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    NetcdfEngine::new("local", &config).unwrap()
}

/// A 24 × 24 CRS84 map inside the fixture's grid.
fn render(
    engine: &NetcdfEngine,
    time: &str,
    parameter: &str,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let extent = engine.raster_info().spatial_extent.unwrap();
    let (dx, dy) = (extent[2] - extent[0], extent[3] - extent[1]);
    let bbox = [
        extent[0] + dx * 0.3,
        extent[1] + dy * 0.3,
        extent[2] - dx * 0.3,
        extent[3] - dy * 0.3,
    ];
    let tile = engine.get_raster_tile(
        bbox,
        24,
        24,
        Some(at(time)),
        &OutputCrs::Wgs84,
        Some(parameter),
        None,
        None,
    )?;
    Ok((0..24 * 24).map(|i| tile.values.value_at(i)).collect())
}

/// [`render`] under a deadline `ms` from now.
fn render_within(
    engine: &NetcdfEngine,
    ms: u64,
    time: &str,
    parameter: &str,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let _deadline = deadline::enter(Some(Instant::now() + Duration::from_millis(ms)));
    render(engine, time, parameter)
}

fn paths(requests: &[&Request]) -> Vec<String> {
    requests.iter().map(|r| r.path.clone()).collect()
}

/// Collections on one host each get stores naming them on the `storage`
/// span (#1233), over the host's one limit, one background share and one
/// GET counter.
#[test]
fn collections_on_one_host_share_its_limit_share_and_counter() {
    let (sfc, base) = host("meps-sfc", "http://127.0.0.4:9/meps/sfc/").unwrap();
    let (pl, _) = host("meps-pl", "http://127.0.0.4:9/meps/pl/").unwrap();
    assert_eq!(base.as_ref(), "meps/sfc");
    assert!(sfc.requests.same_store(&pl.requests));
    assert!(sfc.background.same_store(&pl.background));
    assert!(!sfc.requests.same_store(&sfc.background));
    assert!(Arc::ptr_eq(&sfc.gets, &pl.gets));
}

/// Every map drawn over HTTP ranges equals the same map read from the
/// local files: every GET is ranged and answered 206, never the whole file.
#[test]
fn ranged_reads_draw_what_local_reads_draw() {
    let server = server();
    let remote = engine(&server, "");
    let local = local();
    for time in [LEAD_0, LEAD_1] {
        for parameter in ["air_temperature_2m", "wind_speed_of_gust"] {
            let drawn = render(&remote, time, parameter).unwrap();
            assert_eq!(drawn, render(&local, time, parameter).unwrap());
            assert!(drawn.iter().all(Option::is_some));
        }
    }
    let requests = server.take();
    let gets = gets(&requests);
    assert!(!gets.is_empty());
    assert!(gets
        .iter()
        .all(|get| get.status == 206 && get.range.is_some()));
    assert_eq!(remote.raster_info().reference_times, [at(RUN)]);
    let traffic = remote.remote_traffic().unwrap();
    assert_eq!(traffic.gets as usize, gets.len());
    assert!(traffic.bytes > 0 && traffic.heads > 0);
}

/// What an open and a field read cost: the first scan probes the slots
/// (their sentinels, then the found slot's other lead) and opens both
/// files, four GETs each: the first read, which holds all the metadata
/// `variables()` parses, and three block reads beyond it (the two
/// variables' chunk indexes and the time axis). A field read is one GET,
/// its chunk; a repeat none. A second engine opening the same files, as
/// after a reload, makes no GET.
#[test]
fn an_open_and_a_field_read_cost_a_few_gets_and_a_second_open_none() {
    let server = server();
    let engine = engine(&server, "");
    let requests = server.take();
    let heads: Vec<&Request> = requests.iter().filter(|r| r.method == "HEAD").collect();
    assert!(
        heads.len() <= 8,
        "two rounds of at most four slots: {heads:#?}"
    );
    let lead_1_heads = heads.iter().filter(|h| h.path == key(slot(), 1)).count();
    assert_eq!(lead_1_heads, 1, "lead 01 once its sentinel is seen");
    let opened = gets(&requests);
    for lead in [0, 1] {
        let file: Vec<_> = opened
            .iter()
            .filter(|get| get.path == key(slot(), lead))
            .map(|get| get.range.unwrap())
            .collect();
        assert_eq!(file.len(), 4, "lead {lead}: {opened:#?}");
        assert_eq!(file[0], (0, TUNING.first_read - 1), "the first read");
        for &(start, end) in &file[1..] {
            assert_eq!(start % TUNING.block, 0, "whole blocks");
            assert!(start >= TUNING.first_read && end + 1 - start <= TUNING.bypass_over + 1024);
        }
    }
    assert_eq!(opened.len(), 8);

    render(&engine, LEAD_0, "air_temperature_2m").unwrap();
    let read = server.take();
    assert_eq!(paths(&gets(&read)), [key(slot(), 0)], "one GET, the chunk");
    let (start, end) = gets(&read)[0].range.unwrap();
    assert!(end + 1 - start > TUNING.bypass_over);
    render(&engine, LEAD_0, "air_temperature_2m").unwrap();
    assert!(server.take().is_empty(), "a repeat is cached");

    let again = self::engine(&server, "");
    assert!(
        gets(&server.take()).is_empty(),
        "a second open makes no GET"
    );
    assert_eq!(again.times(), [at(LEAD_0), at(LEAD_1)]);
    render(&again, LEAD_0, "wind_speed_of_gust").unwrap();
    assert_eq!(gets(&server.take()).len(), 1);
}

/// Eight renders of one cold field arriving together, while its chunk is
/// slow to come: one GET and one decode, the same values for all.
#[test]
fn eight_renders_of_a_cold_field_make_one_get() {
    let server = server();
    let engine = engine(&server, "");
    server.take();
    server.slow_over(TUNING.bypass_over, Duration::from_millis(200));
    let start = Barrier::new(8);
    let drawn: Vec<_> = std::thread::scope(|scope| {
        let renders: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    render(&engine, LEAD_1, "wind_speed_of_gust").unwrap()
                })
            })
            .collect();
        renders.into_iter().map(|r| r.join().unwrap()).collect()
    });
    assert!(drawn.windows(2).all(|w| w[0] == w[1]));
    assert_eq!(gets(&server.take()).len(), 1);
    assert_eq!(engine.field_decodes(), 1);
}

/// A cold fetch slower than a request's deadline fails that request and
/// still fills the cache: the requests after it wait for that fill, never
/// fetch again, and draw once it lands.
#[test]
fn a_detached_fill_outlives_its_request_and_warms_the_cache() {
    let server = server();
    let engine = engine(&server, "");
    server.take();
    server.slow_over(TUNING.bypass_over, Duration::from_millis(800));
    let started = Instant::now();
    let late = render_within(&engine, 100, LEAD_0, "wind_speed_of_gust");
    assert!(
        matches!(late, Err(DataServerError::DeadlineExceeded)),
        "{late:?}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "the request waited past its deadline"
    );
    let drawn = loop {
        match render_within(&engine, 50, LEAD_0, "wind_speed_of_gust") {
            Ok(drawn) => break drawn,
            Err(DataServerError::DeadlineExceeded) => {
                assert!(started.elapsed() < Duration::from_secs(5), "never filled");
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert!(drawn.iter().all(Option::is_some));
    assert_eq!(
        gets(&server.take()).len(),
        1,
        "the first request's GET only"
    );
    assert_eq!(engine.field_decodes(), 1);
}

/// A file replaced on the server after it was opened: the next read of it
/// is a 503 and moves the content version; the next poll probes it again
/// and reads the new file.
#[test]
fn a_file_replaced_after_its_open_is_a_503_and_read_again() {
    let server = server();
    let engine = engine(&server, "");
    render(&engine, LEAD_0, "air_temperature_2m").unwrap();
    let version = engine.content_version();

    // Lead 00 now holds lead 01's fields, newer but old enough to settle.
    server.put(
        &key(slot(), 0),
        fixture(1),
        old() + chrono::Duration::minutes(30),
    );
    server.take();
    let replaced = render(&engine, LEAD_0, "wind_speed_of_gust");
    assert!(
        matches!(replaced, Err(DataServerError::ResourceExhausted)),
        "{replaced:?}"
    );
    let statuses: Vec<u16> = gets(&server.take()).iter().map(|get| get.status).collect();
    assert_eq!(statuses, [412]);
    assert!(engine.content_version() > version);
    // Its cached field went with it, and its handle reads nothing more.
    assert!(matches!(
        render(&engine, LEAD_0, "air_temperature_2m"),
        Err(DataServerError::ResourceExhausted)
    ));
    assert!(server.take().is_empty());

    engine.poll_once();
    let probed = server.take();
    assert!(probed
        .iter()
        .any(|r| r.method == "HEAD" && r.path == key(slot(), 0)));
    assert_eq!(engine.times(), [at(LEAD_1)], "only the new file's time");
    render(&engine, LEAD_1, "air_temperature_2m").unwrap();
}

/// The THREDDS catalogs of [`server`]'s files, as thredds.met.no lays them
/// out and serves them, chunked (#1170): the archive root down to the
/// slot's `member_00`, which lists both leads as published [`old`]. Returns
/// the `catalog_url` line.
fn put_catalogs(server: &TestServer) -> String {
    server.chunk_whole_gets();
    let root = "thredds/catalog/meps25epsarchive";
    let segments = [
        slot().format("%Y").to_string(),
        slot().format("%m").to_string(),
        slot().format("%d").to_string(),
        slot().format("%H").to_string(),
        "member_00".to_string(),
    ];
    let modified = old().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let files: Vec<(String, String, String)> = [0, 1]
        .into_iter()
        .map(|lead| {
            let url_path = key(slot(), lead).replace("thredds/fileServer/", "");
            let size = format!("{:.2}", fixture(lead).len() as f64 / 1000.0);
            (url_path, size, modified.clone())
        })
        .collect();
    for depth in 0..=segments.len() {
        let dir = segments[..depth].join("/");
        let path = [root, dir.as_str(), "catalog.xml"]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        let body = match segments.get(depth) {
            Some(next) => thredds_catalog(&[&[next.as_str(), "catalog.xml"].join("/")], &[]),
            None => thredds_catalog(&[], &files),
        };
        server.put(&path, body, old());
    }
    format!("catalog_url = \"{}/{root}/catalog.xml\"", server.url)
}

/// The catalog route (#1170) finds a file a reader found replaced at once,
/// even while its catalog still shows the old version: the next poll HEADs
/// it for its version, and opens the new file.
#[test]
fn a_file_replaced_under_a_catalog_is_headed_again() {
    let server = server();
    let catalog = put_catalogs(&server);
    let engine = engine(&server, &catalog);
    assert_eq!(engine.times(), [at(LEAD_0), at(LEAD_1)]);
    render(&engine, LEAD_0, "air_temperature_2m").unwrap();
    let version = engine.content_version();

    server.put(
        &key(slot(), 0),
        fixture(1),
        old() + chrono::Duration::minutes(30),
    );
    server.take();
    let replaced = render(&engine, LEAD_0, "wind_speed_of_gust");
    assert!(
        matches!(replaced, Err(DataServerError::ResourceExhausted)),
        "{replaced:?}"
    );
    assert!(engine.content_version() > version);

    engine.poll_once();
    let polled = server.take();
    let heads: Vec<&str> = polled
        .iter()
        .filter(|r| r.method == "HEAD")
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(heads, [key(slot(), 0)], "{polled:#?}");
    assert!(
        polled
            .iter()
            .any(|r| r.method == "GET" && r.path.ends_with("member_00/catalog.xml")),
        "the rechecked file's catalog: {polled:#?}"
    );
    assert_eq!(engine.times(), [at(LEAD_1)], "only the new file's time");
    render(&engine, LEAD_1, "air_temperature_2m").unwrap();
}

/// A replacement the prober finds on its own (its sentinel recheck every
/// 30 min, here forced) moves the content version too: tiles of the old
/// file may be cached. A poll that finds nothing new moves nothing.
#[test]
fn a_replacement_the_prober_finds_moves_the_content_version() {
    let server = server();
    let engine = engine(&server, "");
    let version = engine.content_version();
    server.put(
        &key(slot(), 1),
        retimed(
            fixture(1),
            &[("2026-10-15T07:00:00Z", "2026-10-15T08:00:00Z")],
        ),
        old() + chrono::Duration::minutes(30),
    );
    let crate::Source::Remote(remote) = &engine.source else {
        unreachable!("a remote engine");
    };
    assert!(remote.finder.lock().unwrap().recheck(&key(slot(), 1)));
    engine.poll_once();
    assert!(engine.content_version() > version);
    assert_eq!(
        engine.times(),
        [at(LEAD_0), at("2026-10-15T08:00:00Z")],
        "the new file, read again"
    );
    let version = engine.content_version();
    engine.poll_once();
    assert_eq!(engine.content_version(), version, "nothing changed");
}

/// A poll prewarms the newest run's `prewarm_variables` into the chunk
/// cache: a render of one then makes no GET, another variable still does.
#[test]
fn prewarm_fetches_the_newest_runs_chunks_before_requests() {
    let server = server();
    let engine = engine(&server, r#"prewarm_variables = ["wind_speed_of_gust"]"#);
    server.take();
    engine.poll_once();
    let prewarmed = server.take();
    assert_eq!(
        paths(&gets(&prewarmed)).len(),
        2,
        "one chunk per lead: {prewarmed:#?}"
    );
    engine.poll_once();
    assert!(gets(&server.take()).is_empty(), "each field once");

    for time in [LEAD_0, LEAD_1] {
        render(&engine, time, "wind_speed_of_gust").unwrap();
    }
    assert!(gets(&server.take()).is_empty());
    render(&engine, LEAD_0, "air_temperature_2m").unwrap();
    assert_eq!(gets(&server.take()).len(), 1);
}

/// The first scan opens only the newest published run; the poll loop's
/// first poll opens the older one, unless `max_runs` keeps only the newest.
#[test]
fn the_first_scan_opens_the_newest_run_only() {
    let server = server();
    let older = slot() - chrono::Duration::hours(1);
    // The older slot holds a run three hours earlier than the fixture's.
    let earlier = [
        ("2026-10-15T06:00:00Z", "2026-10-15T03:00:00Z"),
        ("2026-10-15T07:00:00Z", "2026-10-15T04:00:00Z"),
    ];
    for lead in [0, 1] {
        server.put(&key(older, lead), retimed(fixture(lead), &earlier), old());
    }
    let engine = engine(&server, "");
    let mut opened: Vec<String> = paths(&gets(&server.take()));
    opened.sort();
    opened.dedup();
    assert_eq!(opened, [key(slot(), 0), key(slot(), 1)]);
    assert_eq!(engine.raster_info().reference_times, [at(RUN)]);

    // Keeping one run, a poll never opens the older run's files.
    let newest = self::engine(&server, "max_runs = 1");
    newest.poll_once();
    assert!(gets(&server.take()).is_empty(), "the newest run is cached");
    assert_eq!(newest.raster_info().reference_times, [at(RUN)]);

    engine.poll_once();
    let mut opened: Vec<String> = paths(&gets(&server.take()));
    opened.sort();
    opened.dedup();
    assert_eq!(opened, [key(older, 0), key(older, 1)]);
    assert_eq!(
        engine.raster_info().reference_times,
        [at("2026-10-15T03:00:00Z"), at(RUN)]
    );
}

fn temperature(engine: &NetcdfEngine, time: &str) -> Vec<Option<f64>> {
    render(engine, time, "air_temperature_2m").unwrap()
}

/// Critical Rule 7: remote reads complete on a poll runtime's async worker
/// (ingest and prewarm), on the render executor's blocking workers and on
/// an async worker of a multi-thread runtime: every read crosses into the
/// I/O runtime, through `block_in_place` on a worker.
#[test]
fn reads_run_on_the_poll_runtime_render_jobs_and_async_workers() {
    let server = TestServer::new();
    server.put(&key(slot(), 0), fixture(0), old());
    let engine = Arc::new(engine(
        &server,
        r#"prewarm_variables = ["wind_speed_of_gust"]"#,
    ));
    // The run waits for lead 01 until its grace has passed: nothing yet.
    assert!(engine.times().is_empty());
    server.put(&key(slot(), 1), fixture(1), old());
    let poll = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let poller = engine.clone();
    poll.block_on(poll.spawn(async move { poller.poll_once() }))
        .unwrap();
    assert_eq!(engine.times(), [at(LEAD_0), at(LEAD_1)]);
    assert_eq!(gets(&server.take()).len(), 10, "two opens, two prewarms");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let slots = Arc::new(tokio::sync::Semaphore::new(8));
    let jobs: Vec<_> = (0..4)
        .map(|_| {
            let (engine, slots) = (engine.clone(), slots.clone());
            runtime.spawn(async move {
                let (job, memory) = ds_executor::RenderJob::acquire_raster(slots, 24, 24)
                    .await
                    .unwrap();
                job.run(move || {
                    let _memory = memory;
                    assert!(deadline::current().is_some());
                    temperature(&engine, LEAD_0)
                })
                .await
                .unwrap()
            })
        })
        .collect();
    let drawn: Vec<_> = jobs
        .into_iter()
        .map(|job| runtime.block_on(job).unwrap())
        .collect();
    assert!(drawn.windows(2).all(|w| w[0] == w[1]));
    let worker = engine.clone();
    let on_async = runtime
        .block_on(runtime.spawn(async move { temperature(&worker, LEAD_1) }))
        .unwrap();
    assert!(on_async.iter().all(Option::is_some));
    assert_eq!(engine.field_decodes(), 2);
    assert_eq!(gets(&server.take()).len(), 2, "one chunk per field");
}

/// The failure behind a reader's error is found through netcdf-reader's
/// and hdf5-reader's wrapping, and keeps its meaning for a request.
#[test]
fn a_read_failure_is_found_through_the_readers_errors() {
    let wrapped = |failure: ReadFailure| {
        netcdf_reader::Error::Hdf5(
            hdf5_reader::error::Error::Io(std::io::Error::other(failure)).with_context("t2m"),
        )
    };
    let replaced = wrapped(ReadFailure::Replaced);
    assert!(matches!(
        ReadFailure::find(&replaced).map(ReadFailure::to_error),
        Some(DataServerError::ResourceExhausted)
    ));
    let late = wrapped(ReadFailure::Failed(DataServerError::DeadlineExceeded));
    assert!(matches!(
        ReadFailure::find(&late).map(ReadFailure::to_error),
        Some(DataServerError::DeadlineExceeded)
    ));
    let other = netcdf_reader::Error::InvalidData("not a remote failure".into());
    assert!(ReadFailure::find(&other).is_none());
    assert_eq!(
        origin("https://thredds.met.no/thredds/fileServer/x/"),
        "https://thredds.met.no"
    );
    assert_eq!(origin("http://127.0.0.1:8080"), "http://127.0.0.1:8080");
}

/// The shipped thredds.met.no examples, surface (sfc) by HEAD probing and
/// through the catalogs (#1170), and pressure levels (pl, #1067 PR 8), are
/// valid and name MET Norway's own paths, checked without contacting the
/// server.
#[test]
fn the_thredds_examples_name_met_norways_files() {
    for (id, family) in [
        ("meps-netcdf-thredds", "sfc"),
        ("meps-netcdf-thredds-catalog", "sfc"),
        ("meps-netcdf-pl-thredds", "pl"),
    ] {
        let path = format!(
            "{}/../../collections.d/{id}.toml.disabled",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table["id"].as_str(), Some(id));
        let config: NetcdfConfig = table["netcdf"].clone().try_into().unwrap();
        ds_core::config::validate_netcdf(id, &config).unwrap();
        assert_eq!(config.leads.as_deref().map(<[u32]>::len), Some(67));
        assert_eq!(config.max_runs, 2, "{id}");
        let remote = NetcdfEngine::remote(id, &config, Tuning::DEFAULT).unwrap();
        let finder = remote.finder.lock().unwrap();
        match (&*finder, config.catalog_url.is_some()) {
            (crate::Finder::Template(_), false) => {}
            (crate::Finder::Catalog { prober, .. }, true) => assert_eq!(
                prober.catalog_url().as_str(),
                "https://thredds.met.no/thredds/catalog/meps25epsarchive/catalog.xml"
            ),
            _ => panic!("{id}: catalog_url and the finder disagree"),
        }
        let key = finder.key(at("2026-10-16T06:00:00Z"), Some(12));
        assert_eq!(
            format!("{}/{key}", remote.origin),
            format!(
                "https://thredds.met.no/thredds/fileServer/meps25epsarchive/2026/10/16/06/\
                 member_00/meps_{family}_12_20261016T06Z.nc"
            )
        );
    }
}

/// An EDR position series of `parameter` at the middle of the fixture's
/// grid over both leads, under a deadline `ms` from now.
fn series_within(
    engine: &NetcdfEngine,
    ms: u64,
    parameter: &str,
) -> Result<Vec<Option<f64>>, DataServerError> {
    use ds_core::edr_engine::EdrEngine;
    let _deadline = deadline::enter(Some(Instant::now() + Duration::from_millis(ms)));
    let [w, s, e, n] = engine.raster_info().spatial_extent.unwrap();
    let response = engine.query_position(
        &format!("POINT({} {})", (w + e) / 2.0, (s + n) / 2.0),
        None,
        Some(&[parameter.to_string()]),
        None,
        None,
    )?;
    let ds_core::model::CoverageResponse::Single(result) = response else {
        panic!("not one coverage");
    };
    Ok(result.ranges[parameter].values.clone())
}

/// Critical Rule 7 for queries (#1067 PR 7): remote reads complete on the
/// shared query runtime's async workers, as api-edr runs a query, and in a
/// GetFeatureInfo job (`RenderJob::run_query`) under a current-thread
/// serving runtime. A series reads each field's chunk once, as a render
/// does, and caches no decoded field: the next render decodes.
#[test]
fn queries_read_on_the_query_runtime_for_edr_and_get_feature_info() {
    let server = server();
    let engine = Arc::new(engine(&server, ""));
    server.take();
    let worker = engine.clone();
    let on_edr = ds_executor::query_runtime()
        .block_on(
            ds_executor::query_runtime()
                .spawn(async move { series_within(&worker, 30_000, "air_temperature_2m") }),
        )
        .unwrap()
        .unwrap();
    assert_eq!(on_edr.len(), 2);
    assert!(on_edr.iter().all(Option::is_some));
    assert_eq!(gets(&server.take()).len(), 2, "one chunk per lead");
    let local = local();
    assert_eq!(
        on_edr,
        series_within(&local, 30_000, "air_temperature_2m").unwrap()
    );

    let serving = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let worker = engine.clone();
    let from_feature_info = serving
        .block_on(async move {
            let job = ds_executor::RenderJob::acquire_feature_info(Arc::new(
                tokio::sync::Semaphore::new(1),
            ))
            .await
            .unwrap();
            job.run_query(move || series_within(&worker, 30_000, "wind_speed_of_gust"))
                .await
        })
        .unwrap()
        .unwrap();
    assert!(from_feature_info.iter().all(Option::is_some));
    assert_eq!(gets(&server.take()).len(), 2);

    // The decoded fields were not kept; their chunks were.
    let decodes = engine.field_decodes();
    render(&engine, LEAD_0, "air_temperature_2m").unwrap();
    assert_eq!(engine.field_decodes(), decodes + 1);
    assert!(gets(&server.take()).is_empty(), "the chunk is cached");
}

/// The middle of the fixture's extent: `[w, s, e, n]` shrunk by 30% on
/// each side.
fn inner_bbox(engine: &NetcdfEngine) -> [f64; 4] {
    let [w, s, e, n] = engine.raster_info().spatial_extent.unwrap();
    let (dx, dy) = (e - w, n - s);
    [w + dx * 0.3, s + dy * 0.3, e - dx * 0.3, n - dy * 0.3]
}

/// Every value of `response`, coverage by coverage, for `parameter`.
fn values_of(response: ds_core::model::CoverageResponse, parameter: &str) -> Samples {
    let results = match response {
        ds_core::model::CoverageResponse::Single(result) => vec![result],
        ds_core::model::CoverageResponse::Collection(results) => results,
    };
    results
        .into_iter()
        .flat_map(|result| result.ranges[parameter].values.clone())
        .collect()
}

/// One query's values, coverage by coverage.
type Samples = Vec<Option<f64>>;

/// A trajectory and a cube under a deadline `ms` from now, over the
/// middle of the extent and both leads.
fn trajectory_and_cube_within(
    engine: &NetcdfEngine,
    ms: u64,
) -> Result<(Samples, Samples), DataServerError> {
    use ds_core::edr_engine::EdrEngine;
    let _deadline = deadline::enter(Some(Instant::now() + Duration::from_millis(ms)));
    let [w, s, e, n] = inner_bbox(engine);
    let trajectory = engine.query_trajectory(
        &format!("LINESTRING({w} {s}, {e} {n})"),
        None,
        Some(&["air_temperature_2m".to_string()]),
        None,
        None,
    )?;
    let cube = engine.query_cube(
        &ds_core::feature::Bbox::new(w, s, e, n).unwrap(),
        Some((at(LEAD_0), at(LEAD_1))),
        Some(&["wind_speed_of_gust".to_string()]),
        None,
        ds_core::cube::CubeResolution::default(),
        None,
    )?;
    Ok((
        values_of(trajectory, "air_temperature_2m"),
        values_of(cube, "wind_speed_of_gust"),
    ))
}

/// Trajectories and cubes (#1169) read remote fields as position does: on
/// the shared query runtime (Critical Rule 7), one chunk GET per field for
/// every sample, the values the local files give, and no decoded field
/// kept. A cold query slower than its deadline is the 504 a series gets,
/// and its reads go on into the chunk cache, so a retry makes progress
/// without another GET.
#[test]
fn trajectories_and_cubes_read_each_remote_chunk_once() {
    let server = server();
    let engine = Arc::new(engine(&server, ""));
    server.take();
    let worker = engine.clone();
    let (trajectory, cube) = ds_executor::query_runtime()
        .block_on(
            ds_executor::query_runtime()
                .spawn(async move { trajectory_and_cube_within(&worker, 30_000) }),
        )
        .unwrap()
        .unwrap();
    assert!(trajectory.len() > 4 && trajectory.iter().all(Option::is_some));
    assert!(cube.len() > 4 && cube.iter().all(Option::is_some));
    assert_eq!(
        gets(&server.take()).len(),
        4,
        "two leads of each variable, one chunk each"
    );
    assert_eq!(
        (trajectory, cube),
        trajectory_and_cube_within(&local(), 30_000).unwrap()
    );
    let decodes = engine.field_decodes();
    render(&engine, LEAD_0, "air_temperature_2m").unwrap();
    assert_eq!(engine.field_decodes(), decodes + 1, "nothing kept decoded");
    assert!(gets(&server.take()).is_empty(), "the chunk is cached");

    let server = self::server();
    let engine = self::engine(&server, "");
    server.take();
    server.slow_over(TUNING.bypass_over, Duration::from_millis(800));
    let started = Instant::now();
    let late = trajectory_and_cube_within(&engine, 100);
    assert!(
        matches!(late, Err(DataServerError::DeadlineExceeded)),
        "{late:?}"
    );
    let done = loop {
        match trajectory_and_cube_within(&engine, 300) {
            Ok(done) => break done,
            Err(DataServerError::DeadlineExceeded) => {
                assert!(started.elapsed() < Duration::from_secs(10), "never cached");
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert!(done.0.iter().chain(&done.1).all(Option::is_some));
    assert_eq!(gets(&server.take()).len(), 4, "each chunk fetched once");
}

/// A cold series slower than its query's deadline fails with the deadline,
/// and the reads it started go on into the chunk cache: a retry makes
/// progress, and the series costs one GET per field however often it is
/// retried. Its reads detach from the query reads' pool, never the render
/// fills' (other tests read concurrently, so only a lower bound holds).
#[test]
fn a_query_past_its_deadline_leaves_its_chunks_cached() {
    let server = server();
    let engine = engine(&server, "");
    server.take();
    server.slow_over(TUNING.bypass_over, Duration::from_millis(800));
    let (_, query_reads, _) = crate::field::QUERY_READS.metrics();
    let started = Instant::now();
    let late = series_within(&engine, 100, "wind_speed_of_gust");
    assert!(
        matches!(late, Err(DataServerError::DeadlineExceeded)),
        "{late:?}"
    );
    let (_, detached, _) = crate::field::QUERY_READS.metrics();
    assert!(
        detached >= query_reads + 2,
        "both leads detached from the query pool"
    );
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "the query waited past its deadline"
    );
    let series = loop {
        match series_within(&engine, 300, "wind_speed_of_gust") {
            Ok(series) => break series,
            Err(DataServerError::DeadlineExceeded) => {
                assert!(started.elapsed() < Duration::from_secs(5), "never cached");
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert!(series.iter().all(Option::is_some));
    assert_eq!(gets(&server.take()).len(), 2, "the first query's GETs only");
}

/// Read sizes for the pressure-level fixture, whose chunks (one level of
/// an 11 × 10 field) are about 330 bytes compressed: each is a chunk read.
const PL_TUNING: Tuning = Tuning {
    block: 64,
    first_read: 1024,
    bypass_over: 128,
};

/// testdata/meps-netcdf-pl's run 06Z lead 00 (four pressure levels), read
/// over HTTP from [`slot`] for `air_temperature_pl`.
fn pl_engine(server: &TestServer) -> NetcdfEngine {
    let file = format!(
        "{}/../../testdata/meps-netcdf-pl/meps_pl_00_20261015T06Z.nc",
        env!("CARGO_MANIFEST_DIR")
    );
    let key = format!(
        "thredds/fileServer/meps25epsarchive/{}/member_00/meps_pl_00_{}Z.nc",
        slot().format("%Y/%m/%d/%H"),
        slot().format("%Y%m%dT%H")
    );
    server.put(&key, std::fs::read(file).unwrap(), old());
    let hours: Vec<String> = (0..24).map(|h| h.to_string()).collect();
    let config: NetcdfConfig = toml::from_str(&format!(
        r#"data_path = "{}/thredds/fileServer/meps25epsarchive/"
           prefix_pattern = "%Y/%m/%d/{{run}}/member_00/"
           filename_template = "meps_pl_{{lead}}_%Y%m%dT{{run}}Z.nc"
           run_hours = [{}]
           leads = [0]
           time_window = "-PT3H"
           variables = ["air_temperature_pl"]"#,
        server.url,
        hours.join(", ")
    ))
    .unwrap();
    NetcdfEngine::with_tuning("remote-pl", &config, PL_TUNING).unwrap()
}

/// [`render`] of `air_temperature_pl` at level `z` under a deadline `ms`
/// from now.
fn render_level_within(
    engine: &NetcdfEngine,
    ms: u64,
    z: f64,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let _deadline = deadline::enter(Some(Instant::now() + Duration::from_millis(ms)));
    let extent = engine.raster_info().spatial_extent.unwrap();
    let (dx, dy) = (extent[2] - extent[0], extent[3] - extent[1]);
    let bbox = [
        extent[0] + dx * 0.3,
        extent[1] + dy * 0.3,
        extent[2] - dx * 0.3,
        extent[3] - dy * 0.3,
    ];
    let tile = engine.get_raster_tile(
        bbox,
        24,
        24,
        Some(at(LEAD_0)),
        &OutputCrs::Wgs84,
        Some("air_temperature_pl"),
        Some(z),
        None,
    )?;
    Ok((0..24 * 24).map(|i| tile.values.value_at(i)).collect())
}

/// Each pressure level is a field of its own (#1067 PR 8): cold fetches of
/// two levels of one variable, both slower than their requests' deadlines,
/// fail those requests and fill the caches detached, each level its own
/// chunk GET (another byte range of the file) and its own decode. The
/// levels then draw their own values, as the local file does, and repeats
/// fetch and decode nothing.
#[test]
fn each_pressure_level_is_a_detached_fill_and_a_decode_of_its_own() {
    let server = TestServer::new();
    let engine = pl_engine(&server);
    assert_eq!(
        engine.raster_info().vertical.unwrap().levels,
        [1000.0, 850.0, 500.0, 300.0]
    );
    server.take();
    server.slow_over(PL_TUNING.bypass_over, Duration::from_millis(400));
    let started = Instant::now();
    for z in [850.0, 300.0] {
        let late = render_level_within(&engine, 100, z);
        assert!(
            matches!(late, Err(DataServerError::DeadlineExceeded)),
            "{z}: {late:?}"
        );
    }
    let mut drawn = Vec::new();
    for z in [850.0, 300.0] {
        drawn.push(loop {
            match render_level_within(&engine, 50, z) {
                Ok(values) => break values,
                Err(DataServerError::DeadlineExceeded) => {
                    assert!(started.elapsed() < Duration::from_secs(5), "never filled");
                }
                Err(e) => panic!("{e}"),
            }
        });
    }
    let fetched = server.take();
    let mut ranges: Vec<(u64, u64)> = gets(&fetched).iter().map(|g| g.range.unwrap()).collect();
    ranges.sort_unstable();
    assert_eq!(ranges.len(), 2, "one chunk GET per level: {fetched:#?}");
    assert_ne!(ranges[0], ranges[1]);
    assert!(ranges
        .iter()
        .all(|&(start, end)| end + 1 - start > PL_TUNING.bypass_over));
    assert_eq!(engine.field_decodes(), 2);

    let local: NetcdfConfig = toml::from_str(&format!(
        r#"data_path = "{}/../../testdata/meps-netcdf-pl"
           filename_template = "meps_pl_{{lead}}_%Y%m%dT{{run}}Z.nc"
           variables = ["air_temperature_pl"]"#,
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let local = NetcdfEngine::new("local-pl", &local).unwrap();
    assert_eq!(
        drawn[0],
        render_level_within(&local, 30_000, 850.0).unwrap()
    );
    assert_eq!(
        drawn[1],
        render_level_within(&local, 30_000, 300.0).unwrap()
    );
    assert_ne!(drawn[0], drawn[1]);
    assert!(drawn.iter().flatten().all(Option::is_some));

    for z in [850.0, 300.0] {
        render_level_within(&engine, 30_000, z).unwrap();
    }
    assert!(gets(&server.take()).is_empty(), "repeats are cached");
    assert_eq!(engine.field_decodes(), 2);
}
