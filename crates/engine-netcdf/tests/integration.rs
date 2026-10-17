//! engine-netcdf over its committed fixtures: a crop of real MET Norway
//! MEPS files (two runs × leads, south-up Lambert conformal conic) and a
//! synthetic global latitude/longitude file (testdata/meps-netcdf,
//! testdata/netcdf-global).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::config::NetcdfConfig;
use ds_core::error::DataServerError;
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile};
use engine_netcdf::NetcdfEngine;

fn testdata(dir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(dir)
}

/// `[netcdf]` over `dir` plus `extra` TOML lines.
fn config(dir: &Path, extra: &str) -> NetcdfConfig {
    toml::from_str(&format!(
        "data_path = {:?}\n{extra}",
        dir.display().to_string()
    ))
    .unwrap()
}

/// The MEPS fixture as the example configures it: the family's file names
/// and eight variables.
fn meps_config(dir: &Path) -> NetcdfConfig {
    config(
        dir,
        r#"filename_template = "meps_sfc_{lead}_%Y%m%dT{run}Z.nc"
           variables = ["air_temperature_2m", "relative_humidity_2m", "x_wind_10m", "y_wind_10m",
                        "wind_speed_of_gust", "air_pressure_at_sea_level",
                        "precipitation_amount_acc", "cloud_area_fraction"]
           titles = { precipitation_amount_acc = "Precipitation accumulated since run start" }"#,
    )
}

fn meps() -> NetcdfEngine {
    let engine = NetcdfEngine::new("meps", &meps_config(&testdata("meps-netcdf"))).unwrap();
    engine.poll_once();
    engine
}

fn global() -> NetcdfEngine {
    let engine = NetcdfEngine::new("global", &config(&testdata("netcdf-global"), "")).unwrap();
    engine.poll_once();
    engine
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().into()
}

/// A stored 2-D field of a fixture file, `[row][column]` in file order
/// (MEPS row 0 is the southern edge). The test reads it whole.
fn stored(file: &str, variable: &str) -> Vec<Vec<f64>> {
    let nc = netcdf_reader::NcFile::open(testdata(file)).unwrap();
    let values = nc.read_variable_as_f64(variable).unwrap();
    let nx = *values.shape().last().unwrap();
    let flat: Vec<f64> = values.iter().copied().collect();
    flat.chunks(nx).map(<[f64]>::to_vec).collect()
}

/// Every value of a tile, row-major from the north-west.
fn values(tile: &RasterTile) -> Vec<Option<f64>> {
    (0..(tile.width * tile.height) as usize)
        .map(|i| tile.values.value_at(i))
        .collect()
}

/// The value drawn at `(lon, lat)`: a one-pixel map centred on it.
fn value_at(
    engine: &NetcdfEngine,
    lon: f64,
    lat: f64,
    parameter: Option<&str>,
    time: Option<DateTime<Utc>>,
    run: Option<DateTime<Utc>>,
) -> Result<Option<f64>, DataServerError> {
    let d = 1e-4;
    let tile = engine.get_raster_tile(
        [lon - d, lat - d, lon + d, lat + d],
        1,
        1,
        time,
        &OutputCrs::Wgs84,
        parameter,
        None,
        run,
    )?;
    Ok(tile.values.value_at(0))
}

const MEPS_00_06Z: &str = "meps-netcdf/meps_sfc_00_20261015T06Z.nc";

/// MET Norway's latitude and longitude of the fixture cells, `[row][column]`
/// with row 0 south.
fn met_latlon() -> (Vec<Vec<f64>>, Vec<Vec<f64>>) {
    (
        stored(MEPS_00_06Z, "latitude"),
        stored(MEPS_00_06Z, "longitude"),
    )
}

#[test]
fn meps_runs_and_variables_are_advertised() {
    let engine = meps();
    let info = engine.raster_info();
    // Projected: no CRS84 storage CRS nor a Common grid of degrees.
    assert_eq!(info.native_crs, "projected");
    assert_eq!(info.grid_size, Some([10, 11]));
    assert_eq!(
        info.reference_times,
        [at("2026-10-15T06:00:00Z"), at("2026-10-15T09:00:00Z")]
    );
    let hours: Vec<u32> = info.times.iter().map(chrono::Timelike::hour).collect();
    assert_eq!(hours, [6, 7, 8, 9, 10, 11]);
    let names: Vec<&str> = info.parameters.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names[..2], ["air_temperature_2m", "relative_humidity_2m"]);
    assert_eq!(names.len(), 8);
    assert_eq!(info.parameter, "air_temperature_2m");
    assert_eq!(info.unit, "K");
    let precipitation = &info.parameters[6];
    assert_eq!(
        (precipitation.title.as_str(), precipitation.unit.as_str()),
        ("Precipitation accumulated since run start", "kg/m^2")
    );
    assert_eq!(info.parameters[0].title, "Screen level temperature (T2M)");
    // The domain, from the grid's inverse-projected edges.
    let [w, s, e, n] = info.spatial_extent.unwrap();
    assert!(
        w < -10.0 && e > 50.0 && s < 50.0 && n > 74.0,
        "{w} {s} {e} {n}"
    );
    assert_eq!(engine.raster_info_shared().times, info.times);
    assert!(matches!(
        engine.live_health(),
        Some(ds_core::health::LiveStatus::Ready)
    ));
    // The data age counts from the newest run's reference time.
    let newest = Utc::now() - engine.data_age().unwrap();
    assert!((newest - at("2026-10-15T09:00:00Z")).num_seconds().abs() <= 1);
}

/// `new` scans the directory once, so the layers exist when the collection
/// is registered; a later poll picks up new files. A directory that cannot
/// be listed is a WARN, retried by the poll.
#[test]
fn new_scans_and_polls_pick_up_new_files() {
    let dir = tempfile::tempdir().unwrap();
    let source = testdata("meps-netcdf");
    let copy = |name: &str| std::fs::copy(source.join(name), dir.path().join(name)).unwrap();
    copy("meps_sfc_00_20261015T06Z.nc");
    let engine = NetcdfEngine::new("meps", &meps_config(dir.path())).unwrap();
    let info = engine.raster_info();
    assert_eq!(info.parameters.len(), 8);
    assert_eq!(info.parameters[0].unit, "K");
    assert_eq!(info.times.len(), 1);
    assert!(matches!(
        engine.live_health(),
        Some(ds_core::health::LiveStatus::Ready)
    ));
    copy("meps_sfc_01_20261015T06Z.nc");
    assert_eq!(engine.raster_info().times.len(), 1, "until the poll");
    engine.poll_once();
    assert_eq!(engine.raster_info().times.len(), 2);

    let missing = NetcdfEngine::new("meps", &meps_config(&dir.path().join("later"))).unwrap();
    assert!(missing.raster_info().parameters.is_empty());
    std::fs::create_dir(dir.path().join("later")).unwrap();
    std::fs::copy(
        source.join("meps_sfc_00_20261015T09Z.nc"),
        dir.path().join("later/meps_sfc_00_20261015T09Z.nc"),
    )
    .unwrap();
    missing.poll_once();
    assert_eq!(missing.raster_info().parameters.len(), 8);
}

/// MET Norway's own latitude/longitude of each fixture cell, against the
/// engine's grid: the cell centre inverse-projected through the CF
/// `projection_lambert`, to a few metres (0.00005° of latitude is 5.6 m).
/// Pinned to their values, not a round trip of ours.
#[test]
fn the_grid_matches_met_norways_latitude_longitude() {
    let engine = meps();
    let grid = engine.grid().unwrap();
    let (lat, lon) = met_latlon();
    let ny = lat.len();
    assert_eq!((grid.width as usize, grid.height as usize), (10, ny));
    for (j, (lat_row, lon_row)) in lat.iter().zip(&lon).enumerate() {
        for (i, (&lat_met, &lon_met)) in lat_row.iter().zip(lon_row).enumerate() {
            // Stored row j counts from the south; the grid's rows from the north.
            let (lon_ours, lat_ours) = grid.pixel_to_world(i as u32, (ny - 1 - j) as u32);
            let dlat = lat_ours - lat_met;
            let dlon = (lon_ours - lon_met) * lat_met.to_radians().cos();
            assert!(
                dlat.abs() < 5e-5 && dlon.abs() < 5e-5,
                "cell ({j}, {i}): ours ({lon_ours}, {lat_ours}), MET Norway ({lon_met}, {lat_met})"
            );
        }
    }
}

/// MEPS stores row 0 at the southern edge: drawn north-up, each cell's
/// stored value is where MET Norway places the cell, and the warmer south
/// is at the bottom of the map.
#[test]
fn south_up_rows_are_drawn_north_up() {
    let engine = meps();
    let (lat, lon) = met_latlon();
    let t2m = stored(MEPS_00_06Z, "air_temperature_2m");
    let six = Some(at("2026-10-15T06:00:00Z"));
    for (j, i) in [(0, 0), (0, 9), (10, 0), (10, 9), (5, 4)] {
        let drawn = value_at(&engine, lon[j][i], lat[j][i], None, six, None)
            .unwrap()
            .unwrap();
        assert!(
            (drawn - t2m[j][i]).abs() < 1e-3,
            "cell ({j}, {i}): drawn {drawn}, stored {}",
            t2m[j][i]
        );
    }
    // Over the whole domain: the southern half is warmer.
    let tile = engine
        .get_raster_tile(
            [-5.0, 52.0, 35.0, 70.0],
            40,
            36,
            six,
            &OutputCrs::Wgs84,
            Some("air_temperature_2m"),
            None,
            None,
        )
        .unwrap();
    let all = values(&tile);
    let mean = |rows: std::ops::Range<usize>| {
        let v: Vec<f64> = rows
            .flat_map(|r| all[r * 40..(r + 1) * 40].iter().flatten().copied())
            .collect();
        v.iter().sum::<f64>() / v.len() as f64
    };
    let (north, south) = (mean(0..18), mean(18..36));
    assert!(south > north + 1.0, "south {south} K, north {north} K");
}

/// 09Z is valid in both runs: unpinned it comes from the newest run, an
/// hour the newest run lacks from the older one, and a pinned run is drawn
/// or fails, never another run.
#[test]
fn the_newest_covering_run_is_drawn_and_older_runs_fill_in() {
    let engine = meps();
    let (lat, lon) = met_latlon();
    let (j, i) = (0, 0);
    let draw = |time: &str, run: Option<&str>| {
        value_at(
            &engine,
            lon[j][i],
            lat[j][i],
            None,
            Some(at(time)),
            run.map(at),
        )
    };
    let stored_at = |file: &str| stored(&format!("meps-netcdf/{file}"), "air_temperature_2m")[j][i];
    let from_09z = stored_at("meps_sfc_00_20261015T09Z.nc");
    let from_06z = stored_at("meps_sfc_03_20261015T06Z.nc");
    assert!((from_09z - from_06z).abs() > 0.05, "the runs differ there");
    let close = |drawn: Option<f64>, expected: f64| (drawn.unwrap() - expected).abs() < 1e-3;

    assert!(close(draw("2026-10-15T09:00:00Z", None).unwrap(), from_09z));
    assert!(close(
        draw("2026-10-15T09:00:00Z", Some("2026-10-15T09:00:00Z")).unwrap(),
        from_09z
    ));
    assert!(close(
        draw("2026-10-15T09:00:00Z", Some("2026-10-15T06:00:00Z")).unwrap(),
        from_06z
    ));
    let eight = stored_at("meps_sfc_02_20261015T06Z.nc");
    assert!(close(draw("2026-10-15T08:00:00Z", None).unwrap(), eight));
    assert!(draw("2026-10-15T08:00:00Z", Some("2026-10-15T09:00:00Z")).is_err());
    assert!(draw("2026-10-15T10:00:00Z", Some("2026-10-15T06:00:00Z")).is_err());
    assert!(matches!(
        draw("2026-10-15T09:00:00Z", Some("2026-10-15T03:00:00Z")),
        Err(DataServerError::ReferenceTimeNotFound(_))
    ));

    // The cache keys name what is drawn (#507, #521).
    let run = |time: &str| engine.resolve_reference_time(Some(at(time)), None);
    assert_eq!(
        run("2026-10-15T09:00:00Z"),
        Some(at("2026-10-15T09:00:00Z"))
    );
    assert_eq!(
        run("2026-10-15T08:10:00Z"),
        Some(at("2026-10-15T06:00:00Z"))
    );
    assert_eq!(
        engine.resolve_time(Some(at("2026-10-15T08:10:00Z")), None),
        Some(at("2026-10-15T08:00:00Z"))
    );
    assert_eq!(
        engine.resolve_time(None, None),
        Some(at("2026-10-15T11:00:00Z"))
    );
    assert_eq!(
        engine.resolve_reference_time(None, None),
        Some(at("2026-10-15T09:00:00Z"))
    );
    // A failed selection echoes the request: the render fails, nothing is
    // cached under it.
    let late = Some(at("2026-10-15T15:00:00Z"));
    assert_eq!(engine.resolve_time(late, None), late);
}

#[test]
fn max_runs_and_max_files_keep_the_newest_runs() {
    let newest_runs = |extra: &str| {
        let engine = NetcdfEngine::new("meps", &config(&testdata("meps-netcdf"), extra)).unwrap();
        engine.poll_once();
        let info = engine.raster_info();
        (info.reference_times, info.times.len())
    };
    let nine = at("2026-10-15T09:00:00Z");
    assert_eq!(newest_runs("max_runs = 1"), (vec![nine], 3));
    // The newest run's three files fit five, the older run's four do not.
    assert_eq!(newest_runs("max_files = 5"), (vec![nine], 3));
    assert_eq!(newest_runs("max_files = 7").0.len(), 2);
}

/// A template must name the time: without strftime codes it matches no
/// timestamp, and the collection fails to load rather than serve nothing.
#[test]
fn an_unusable_filename_template_fails_the_collection() {
    for template in ["meps_sfc.nc", "meps_{lead}_%Y%m%d{lead}.nc"] {
        let config = config(
            &testdata("meps-netcdf"),
            &format!("filename_template = {template:?}"),
        );
        let error = NetcdfEngine::new("meps", &config).unwrap_err();
        assert!(
            matches!(&error, DataServerError::Config(m) if m.contains("filename_template")),
            "{error}"
        );
    }
}

#[test]
fn an_unknown_parameter_is_a_client_error() {
    let engine = meps();
    let result = value_at(&engine, 15.0, 63.0, Some("snowfall"), None, None);
    assert!(matches!(result, Err(DataServerError::InvalidParameter(_))));
}

/// The bands of one request come from one step and one output map.
#[test]
fn several_parameters_render_like_one_at_a_time() {
    let engine = meps();
    let bbox = [5.0, 55.0, 30.0, 70.0];
    let time = Some(at("2026-10-15T10:00:00Z"));
    let parameters = ["x_wind_10m", "y_wind_10m"];
    let tiles = engine
        .get_raster_tiles(
            bbox,
            16,
            12,
            time,
            &OutputCrs::WebMercator,
            &parameters,
            None,
            None,
        )
        .unwrap();
    for (tile, parameter) in tiles.iter().zip(parameters) {
        let one = engine
            .get_raster_tile(
                bbox,
                16,
                12,
                time,
                &OutputCrs::WebMercator,
                Some(parameter),
                None,
                None,
            )
            .unwrap();
        assert_eq!(values(tile), values(&one), "{parameter}");
        assert!(values(tile).iter().any(Option::is_some));
    }
}

/// A global grid's columns wrap: a map across the antimeridian
/// interpolates between the cells either side of 180°, with no gap.
#[test]
fn the_global_grid_is_continuous_across_the_antimeridian() {
    let engine = global();
    let info = engine.raster_info();
    assert_eq!(info.native_crs, "CRS:84");
    assert_eq!(info.grid_size, Some([72, 36]));
    assert!(info.reference_times.is_empty());
    assert_eq!(info.times.len(), 3);
    assert_eq!(info.spatial_extent, Some([-180.0, -90.0, 180.0, 90.0]));
    let tile = engine
        .get_raster_tile(
            [170.0, 10.0, -170.0, 20.0],
            40,
            10,
            None,
            &OutputCrs::Wgs84,
            Some("air_temperature"),
            None,
            None,
        )
        .unwrap();
    let all = values(&tile);
    assert!(all.iter().all(Option::is_some), "no gap at the seam");
    // Along row 5 (14.5°N) only the 20 sin(lon) term changes.
    let row = &all[5 * 40..6 * 40];
    for pair in row.windows(2) {
        let step = (pair[1].unwrap() - pair[0].unwrap()).abs();
        assert!(step < 0.5, "a jump of {step} along the row");
    }
    // Columns 19 and 20 are centred 0.25° either side of 180°, between the
    // cells at 177.5° and -177.5° (182.5°).
    let model = |lon: f64| 20.0 * lon.to_radians().sin();
    let (west, east) = (model(177.5), model(-177.5));
    let linear = |lon: f64| west + (east - west) * (lon - 177.5) / 5.0;
    let base = row[19].unwrap() - linear(179.75);
    assert!((row[20].unwrap() - base - linear(180.25)).abs() < 0.02);
    // Without a time, the newest step.
    assert_eq!(
        engine.resolve_time(None, None),
        Some(at("2026-01-01T12:00:00Z"))
    );
    assert_eq!(engine.resolve_reference_time(None, None), None);
}

/// A packed short field is unpacked, and its fill is nodata.
#[test]
fn packed_values_are_unpacked_and_fill_is_nodata() {
    let engine = global();
    let draw = |parameter: &str, lon: f64, lat: f64| {
        value_at(&engine, lon, lat, Some(parameter), None, None).unwrap()
    };
    // The fill covers the cells centred at 2.5° and 7.5° east and north.
    assert_eq!(draw("t_packed", 7.5, 7.5), None);
    let packed = draw("t_packed", 102.5, 42.5).unwrap();
    let float = draw("air_temperature", 102.5, 42.5).unwrap();
    assert!((packed - float).abs() < 0.011, "{packed} {float}");
}

/// A field read that fails is an error, never a map of nodata, and is not
/// cached: the next render reads again.
#[test]
fn a_failed_read_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("global.nc");
    std::fs::copy(testdata("netcdf-global/global_5deg.nc"), &path).unwrap();
    let engine = NetcdfEngine::new("broken", &config(dir.path(), "")).unwrap();
    engine.poll_once();
    // Emptied under the open handle.
    std::fs::write(&path, b"").unwrap();
    for _ in 0..2 {
        let result = value_at(&engine, 102.5, 42.5, None, None, None);
        assert!(
            matches!(result, Err(DataServerError::Storage(_))),
            "{result:?}"
        );
    }
    assert_eq!(engine.field_decodes(), 2, "a failed fill is not cached");
}

/// A file replaced on disk is read again, and the content version moves so
/// cached tiles of it are not served; a new file does not move it.
#[test]
fn a_replaced_file_moves_the_content_version() {
    let dir = tempfile::tempdir().unwrap();
    let source = testdata("meps-netcdf");
    for name in ["meps_sfc_00_20261015T06Z.nc", "meps_sfc_01_20261015T06Z.nc"] {
        std::fs::copy(source.join(name), dir.path().join(name)).unwrap();
    }
    let engine = NetcdfEngine::new("meps", &meps_config(dir.path())).unwrap();
    engine.poll_once();
    let version = engine.content_version();
    assert_ne!(version, 0);
    let seven = Some(at("2026-10-15T07:00:00Z"));
    let (lat, lon) = met_latlon();
    let draw = || {
        value_at(&engine, lon[0][0], lat[0][0], None, seven, None)
            .unwrap()
            .unwrap()
    };
    let lead = |n: u32| {
        stored(
            &format!("meps-netcdf/meps_sfc_0{n}_20261015T06Z.nc"),
            "air_temperature_2m",
        )[0][0]
    };
    assert!((draw() - lead(1)).abs() < 1e-3);

    std::fs::copy(
        source.join("meps_sfc_02_20261015T06Z.nc"),
        dir.path().join("meps_sfc_02_20261015T06Z.nc"),
    )
    .unwrap();
    engine.poll_once();
    assert_eq!(
        engine.content_version(),
        version,
        "a new file draws no old time"
    );

    // Lead 01 rewritten in place with lead 03's fields, still valid at 07Z.
    let replaced = dir.path().join("meps_sfc_01_20261015T06Z.nc");
    let mut bytes = std::fs::read(source.join("meps_sfc_03_20261015T06Z.nc")).unwrap();
    let (three, one) = (
        (at("2026-10-15T09:00:00Z").timestamp() as f64).to_le_bytes(),
        (at("2026-10-15T07:00:00Z").timestamp() as f64).to_le_bytes(),
    );
    let offsets: Vec<usize> = (0..bytes.len() - 8)
        .filter(|&i| bytes[i..i + 8] == three)
        .collect();
    assert_eq!(offsets.len(), 1, "the time is stored once, little-endian");
    bytes[offsets[0]..offsets[0] + 8].copy_from_slice(&one);
    std::fs::write(&replaced, &bytes).unwrap();
    engine.poll_once();
    assert!(engine.content_version() > version);
    assert!((draw() - lead(3)).abs() < 1e-3, "the new file's values");
}

fn render_clouds(engine: &NetcdfEngine, time: &str) -> Result<Vec<Option<f64>>, DataServerError> {
    let tile = engine.get_raster_tile(
        [0.0, 50.0, 40.0, 72.0],
        32,
        32,
        Some(at(time)),
        &OutputCrs::WebMercator,
        Some("cloud_area_fraction"),
        None,
        None,
    )?;
    Ok(values(&tile))
}

/// Critical Rule 7: field reads complete on the render executor's blocking
/// workers and on an async worker of a multi-thread runtime, where the
/// fill runs in `block_in_place`; concurrent renders of one cold field
/// decode it once.
#[test]
fn renders_on_render_jobs_and_async_workers() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let engine = Arc::new(meps());
    let slots = Arc::new(tokio::sync::Semaphore::new(8));
    let jobs: Vec<_> = (0..8)
        .map(|_| {
            let (engine, slots) = (engine.clone(), slots.clone());
            runtime.spawn(async move {
                let (job, memory) = ds_executor::RenderJob::acquire_raster(slots, 32, 32)
                    .await
                    .unwrap();
                job.run(move || {
                    let _memory = memory;
                    assert!(ds_core::deadline::current().is_some());
                    render_clouds(&engine, "2026-10-15T10:00:00Z")
                })
                .await
                .unwrap()
            })
        })
        .collect();
    let drawn: Vec<_> = jobs
        .into_iter()
        .map(|job| runtime.block_on(job).unwrap().unwrap())
        .collect();
    assert!(drawn.windows(2).all(|w| w[0] == w[1]));
    assert!(drawn[0].iter().any(Option::is_some));
    assert_eq!(engine.field_decodes(), 1, "eight renders, one decode");

    // A cold field on an async worker.
    let worker = engine.clone();
    let on_async = runtime
        .block_on(runtime.spawn(async move { render_clouds(&worker, "2026-10-15T11:00:00Z") }))
        .unwrap()
        .unwrap();
    assert!(on_async.iter().any(Option::is_some));
    assert_eq!(engine.field_decodes(), 2);
}
