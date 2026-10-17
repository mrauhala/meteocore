//! MEPS read live from MET Norway's thredds.met.no (#1067 PR 5). Ignored:
//! it goes to the network. Run it by hand, on a connection that can take
//! about 5 MB:
//!
//! ```text
//! cargo test -p engine-netcdf --test live_thredds -- --ignored --nocapture
//! ```
//!
//! What it reads: a few HEADs for the sentinels and leads of the runs in
//! a 6 h window; leads 00-02 of the newest published sfc run of the
//! control member, opened for `air_temperature_2m` (each file's 1 MiB
//! first read and a few 64 KiB blocks, about 3.5 MB); and one field of it,
//! one ~1.5 MB chunk. It prints the requests, bytes and time of each step
//! and the heap a kept per-lead handle holds, for the crate notes'
//! "handle × 67 × max_runs". `a_ranged_get_names_the_whole_file` is one
//! HEAD and a 1 KiB GET.

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
#[ignore = "reads about 5 MB from thredds.met.no; run by hand"]
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
    println!(
        "block cache {:?}\nchunk cache {:?}\ntotal {:?}",
        engine_netcdf::block_cache_metrics(),
        engine_netcdf::chunk_cache_metrics(),
        traffic(&engine)
    );
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
