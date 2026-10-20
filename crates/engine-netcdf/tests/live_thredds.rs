//! MEPS read live from MET Norway's thredds.met.no (#1067 PRs 5, 7 and 8).
//! Ignored: it goes to the network. Run it by hand, on a connection that can
//! take about 18 MB for the whole file (about 9 MB for each of the two MEPS
//! tests), or one test at a time:
//!
//! ```text
//! cargo test -p engine-netcdf --test live_thredds -- --ignored --nocapture
//! cargo test -p engine-netcdf --test live_thredds -- --ignored --nocapture \
//!     --exact meps_pressure_levels_from_thredds
//! ```
//!
//! What it reads: a few HEADs for the sentinels and leads of the runs in
//! a 6 h window; leads 00-02 of the newest published sfc run of the
//! control member, opened for `air_temperature_2m` (each file's 1 MiB
//! first read and a few 64 KiB blocks, about 3.5 MB); one field of it,
//! one ~1.5 MB chunk; and an EDR position series of it over the three
//! leads, which fetches the other two leads' chunks (~3 MB). It prints the
//! requests, bytes and time of each step
//! and the heap a kept per-lead handle holds, for the crate notes'
//! "handle × 67 × max_runs". `meps_pressure_levels_from_thredds` opens
//! two pl files and reads four fields, about 9 MB.
//! `a_ranged_get_names_the_whole_file` is one HEAD and a 1 KiB GET.
//! `the_catalog_route_finds_what_probing_finds` (#1170) reads the THREDDS
//! catalogs on the way to the runs in the window, opens the newest run's
//! leads 00-02 found there (about 3.7 MB), and probes the same files by HEAD
//! (their metadata then comes from the block cache): about 4 MB.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Instant;

use ds_core::config::NetcdfConfig;
use ds_core::map_engine::{MapEngine, OutputCrs};
use engine_netcdf::NetcdfEngine;

struct Counting;

static ALLOCATED: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        ALLOCATED.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATED.fetch_add(
            new_size as isize - layout.size() as isize,
            Ordering::Relaxed,
        );
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const CONFIG: &str = r#"
data_path = "https://thredds.met.no/thredds/fileServer/meps25epsarchive/"
prefix_pattern = "%Y/%m/%d/{run}/member_00/"
filename_template = "meps_sfc_{lead}_%Y%m%dT{run}Z.nc"
run_hours = [0, 3, 6, 9, 12, 15, 18, 21]
leads = [0, 1, 2]
time_window = "-PT6H"
variables = ["air_temperature_2m"]
max_runs = 1
"#;

fn traffic(engine: &NetcdfEngine) -> engine_netcdf::RemoteTraffic {
    engine.remote_traffic().unwrap()
}

#[test]
#[ignore = "reads about 9 MB from thredds.met.no; run by hand"]
fn meps_per_lead_files_from_thredds() {
    let config: NetcdfConfig = toml::from_str(CONFIG).unwrap();
    let started = Instant::now();
    let engine = NetcdfEngine::new("meps-live", &config).unwrap();
    let opened = traffic(&engine);
    let times = engine.times();
    println!(
        "first scan: {:.1} s, {opened:?}, run {:?}, valid times {times:?}",
        started.elapsed().as_secs_f64(),
        engine.raster_info().reference_times
    );
    assert_eq!(times.len(), 3, "leads 00-02 of the newest published run");

    // A second engine over the same files opens them from the block cache:
    // what it holds is its handles, not cache.
    let before = ALLOCATED.load(Ordering::Relaxed);
    let started = Instant::now();
    let again = NetcdfEngine::new("meps-live-again", &config).unwrap();
    let held = ALLOCATED.load(Ordering::Relaxed) - before;
    let reopened = traffic(&again);
    println!(
        "second open of the same files: {:.1} s, {} HEADs, {} GETs",
        started.elapsed().as_secs_f64(),
        reopened.heads,
        reopened.gets - opened.gets
    );
    assert_eq!(reopened.gets, opened.gets, "a second open makes no GET");
    let per_handle = held / 3;
    println!(
        "heap held per kept per-lead handle (1 variable served of 221 parsed): {per_handle} \
         bytes; x 67 leads x 4 runs = {:.1} MB",
        (per_handle * 67 * 4) as f64 / 1e6
    );
    drop(again);

    let extent = engine.raster_info().spatial_extent.unwrap();
    let started = Instant::now();
    let tile = engine
        .get_raster_tile(
            extent,
            256,
            256,
            Some(times[0]),
            &OutputCrs::Wgs84,
            Some("air_temperature_2m"),
            None,
            None,
        )
        .unwrap();
    let drawn = (0..256 * 256)
        .filter(|&i| tile.values.value_at(i).is_some())
        .count();
    let field = traffic(&engine);
    println!(
        "one cold field: {:.1} s, {} GETs, {} bytes, {drawn} of 65536 pixels drawn",
        started.elapsed().as_secs_f64(),
        field.gets - opened.gets,
        field.bytes - opened.bytes
    );
    assert!(drawn > 0);
    let started = Instant::now();
    engine
        .get_raster_tile(
            extent,
            256,
            256,
            Some(times[0]),
            &OutputCrs::Wgs84,
            Some("air_temperature_2m"),
            None,
            None,
        )
        .unwrap();
    println!(
        "the same field again: {:.3} s, {} GETs",
        started.elapsed().as_secs_f64(),
        traffic(&engine).gets - field.gets
    );
    // EDR: a position series over the three leads, the first one's chunk
    // cached by the render above.
    let before = traffic(&engine);
    let started = Instant::now();
    let series = {
        use ds_core::edr_engine::EdrEngine;
        let _deadline =
            ds_core::deadline::enter(Some(Instant::now() + std::time::Duration::from_secs(30)));
        match engine
            .query_position(
                "POINT(10.75 59.91)",
                None,
                Some(&["air_temperature_2m".to_string()]),
                None,
                None,
            )
            .unwrap()
        {
            ds_core::model::CoverageResponse::Single(result) => {
                result.ranges["air_temperature_2m"].values.clone()
            }
            ds_core::model::CoverageResponse::Collection(_) => panic!("one coverage"),
        }
    };
    let after = traffic(&engine);
    println!(
        "EDR position series over 3 leads: {:.1} s, {} GETs, {} bytes, values {series:?}",
        started.elapsed().as_secs_f64(),
        after.gets - before.gets,
        after.bytes - before.bytes
    );
    assert_eq!(series.len(), 3);
    assert!(series.iter().all(Option::is_some));
    assert_eq!(
        after.gets - before.gets,
        2,
        "the two uncached leads' chunks"
    );
    println!(
        "block cache {:?}\nchunk cache {:?}\ntotal {:?}",
        engine_netcdf::block_cache_metrics(),
        engine_netcdf::chunk_cache_metrics(),
        traffic(&engine)
    );
}

const PL_CONFIG: &str = r#"
data_path = "https://thredds.met.no/thredds/fileServer/meps25epsarchive/"
prefix_pattern = "%Y/%m/%d/{run}/member_00/"
filename_template = "meps_pl_{lead}_%Y%m%dT{run}Z.nc"
run_hours = [0, 3, 6, 9, 12, 15, 18, 21]
leads = [0, 1]
time_window = "-PT6H"
variables = ["air_temperature_pl"]
max_runs = 1
"#;

/// Pressure levels (#1067 PR 8), live: leads 00-01 of the newest published
/// pl run, opened for `air_temperature_pl` (two files' metadata), and one
/// EDR position query at 850 and 500 hPa over both leads: four fields, one
/// ~1.5 MB chunk each. About 9 MB. It prints the levels, the traffic of
/// the opens and of the query.
#[test]
#[ignore = "reads about 9 MB from thredds.met.no; run by hand"]
fn meps_pressure_levels_from_thredds() {
    use ds_core::edr_engine::EdrEngine;
    let config: NetcdfConfig = toml::from_str(PL_CONFIG).unwrap();
    let started = Instant::now();
    let engine = NetcdfEngine::new("meps-pl-live", &config).unwrap();
    let opened = traffic(&engine);
    let info = engine.raster_info();
    println!(
        "first scan: {:.1} s, {opened:?}, run {:?}, valid times {:?}, levels {:?}",
        started.elapsed().as_secs_f64(),
        info.reference_times,
        info.times,
        info.vertical
    );
    let vertical = info.vertical.expect("a pressure axis");
    assert_eq!(vertical.kind, ds_core::vertical::VerticalKind::Pressure);
    assert_eq!(
        vertical.levels,
        [
            1000.0, 925.0, 850.0, 800.0, 700.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0, 100.0,
            50.0
        ]
    );
    assert_eq!(
        info.times.len(),
        2,
        "leads 00-01 of the newest published run"
    );

    let started = Instant::now();
    let _deadline =
        ds_core::deadline::enter(Some(Instant::now() + std::time::Duration::from_secs(30)));
    let response = engine
        .query_position(
            "POINT(10.75 59.91)",
            None,
            Some(&["air_temperature_pl".to_string()]),
            Some(&[850.0, 500.0]),
            None,
        )
        .unwrap();
    let after = traffic(&engine);
    let ds_core::model::CoverageResponse::Collection(profiles) = response else {
        panic!("a vertical profile per lead");
    };
    let values: Vec<Vec<Option<f64>>> = profiles
        .iter()
        .map(|p| p.ranges["air_temperature_pl"].values.clone())
        .collect();
    println!(
        "EDR position at 850 and 500 hPa over 2 leads: {:.1} s, {} GETs, {} bytes, values \
         {values:?}",
        started.elapsed().as_secs_f64(),
        after.gets - opened.gets,
        after.bytes - opened.bytes
    );
    assert_eq!(values.len(), 2);
    for profile in &values {
        let (t850, t500) = (profile[0].unwrap(), profile[1].unwrap());
        assert!(t850 > t500, "colder aloft: {t850} {t500}");
        assert!((200.0..320.0).contains(&t500), "{t500}");
    }
    assert_eq!(after.gets - opened.gets, 4, "one chunk per field");
    println!("total {:?}", traffic(&engine));
}

/// The catalog of `CONFIG`'s `data_path` directory.
const CATALOG: &str =
    r#"catalog_url = "https://thredds.met.no/thredds/catalog/meps25epsarchive/catalog.xml""#;

/// The catalog route (#1170), live: it finds the files HEAD probing finds.
/// thredds.met.no streams its catalogs chunked, without `Content-Length`,
/// which `DataStore::get_documents` reads. It prints the catalogs read, the
/// HEADs, the time.
#[test]
#[ignore = "reads about 4 MB from thredds.met.no; run by hand"]
fn the_catalog_route_finds_what_probing_finds() {
    let config: NetcdfConfig = toml::from_str(&[CONFIG, CATALOG].join("\n")).unwrap();
    let started = Instant::now();
    let catalog = NetcdfEngine::new("meps-live-catalog", &config).unwrap();
    let found = traffic(&catalog);
    println!(
        "catalog route, first scan: {:.1} s, {found:?}, run {:?}, valid times {:?}",
        started.elapsed().as_secs_f64(),
        catalog.raster_info().reference_times,
        catalog.times()
    );
    assert!(found.catalogs > 0);
    // One HEAD per file of each run in the window, for its version.
    assert!(found.heads >= 3, "{found:?}");
    assert_eq!(catalog.times().len(), 3, "leads 00-02 of the newest run");

    let probing = NetcdfEngine::new(
        "meps-live-probing",
        &toml::from_str::<NetcdfConfig>(CONFIG).unwrap(),
    )
    .unwrap();
    println!("probing, first scan: {:?}", traffic(&probing));
    assert_eq!(
        catalog.raster_info().reference_times,
        probing.raster_info().reference_times
    );
    assert_eq!(catalog.times(), probing.times());
}

/// One file of the archive, the run the committed MEPS sample was cut from.
const ARCHIVED: &str = "https://thredds.met.no/thredds/fileServer/meps25epsarchive/2026/10/15/06/\
                        member_00/meps_sfc_00_20261015T06Z.nc";

/// A HEAD and one 1 KiB ranged GET: the 206's `ObjectVersion` names the
/// whole file's size (from `Content-Range`), so conditional reads match
/// the version a HEAD listed.
#[test]
#[ignore = "one HEAD and a 1 KiB GET to thredds.met.no; run by hand"]
fn a_ranged_get_names_the_whole_file() {
    use ds_storage::object_store::{GetOptions, GetRange, ObjectStore};
    use ds_storage::ObjectVersion;

    let (store, path) = ds_storage::build_http_store(ARCHIVED).unwrap();
    let head = ObjectVersion::from(&store.head(&path).unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (version, range, bytes) = runtime.block_on(async {
        let result = store
            .inner()
            .get_opts(
                &path,
                GetOptions {
                    range: Some(GetRange::Bounded(0..1024)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let (version, range) = (ObjectVersion::from(&result.meta), result.range.clone());
        (version, range, result.bytes().await.unwrap())
    });
    println!(
        "HEAD {head:?}\nranged GET {version:?}, range {range:?}, {} bytes",
        bytes.len()
    );
    assert_eq!(bytes.len(), 1024);
    assert_eq!(range, 0..1024);
    assert_eq!(version.size, head.size, "the whole file's size");
    assert!(head.matches(&version));
}
