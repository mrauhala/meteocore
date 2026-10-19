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

/// A stored value of `variable` in the committed MEPS fixtures as the
/// engine serves it (#1209), each conversion written out here rather than
/// taken from `ds_core::units`: temperatures from K to °C, MSLP from Pa to
/// hPa, geopotential from m^2/s^2 to gpm. The precipitation's kg/m^2 is mm
/// unchanged, and fractions (unit 1) and m/s have no rule.
fn served(variable: &str, stored: f64) -> f64 {
    match variable {
        "air_temperature_2m" | "air_temperature_pl" => stored - 273.15,
        "air_pressure_at_sea_level" => stored / 100.0,
        "geopotential_pl" => stored / 9.80665,
        _ => stored,
    }
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
    // Display units (#1209): the file's K and kg/m^2 as °C and mm.
    assert_eq!(info.unit, "°C");
    let precipitation = &info.parameters[6];
    assert_eq!(
        (precipitation.title.as_str(), precipitation.unit.as_str()),
        ("Precipitation accumulated since run start", "mm")
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
    assert_eq!(info.parameters[0].unit, "°C");
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
        let expected = served("air_temperature_2m", t2m[j][i]);
        assert!(
            (drawn - expected).abs() < 1e-3,
            "cell ({j}, {i}): drawn {drawn}, served {expected}"
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
    assert!(south > north + 1.0, "south {south} °C, north {north} °C");
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
    let stored_at = |file: &str| {
        let t2m = stored(&format!("meps-netcdf/{file}"), "air_temperature_2m")[j][i];
        served("air_temperature_2m", t2m)
    };
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
        let t2m = stored(
            &format!("meps-netcdf/meps_sfc_0{n}_20261015T06Z.nc"),
            "air_temperature_2m",
        )[0][0];
        served("air_temperature_2m", t2m)
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

// ---------------------------------------------------------------------------
// EDR (#1067 PR 7)
// ---------------------------------------------------------------------------

use ds_core::edr_engine::EdrEngine;
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected a single coverage"),
    }
}

fn names(parameters: &[&str]) -> Vec<String> {
    parameters.iter().map(|p| p.to_string()).collect()
}

/// The hours of a series' time axis.
fn series_hours(result: &QueryResult) -> Vec<u32> {
    let DomainDescription::PointSeries { t, .. } = &result.domain else {
        panic!("not a PointSeries: {:?}", result.domain);
    };
    t.iter().map(chrono::Timelike::hour).collect()
}

/// A position series at MET Norway's own coordinates of fixture cell
/// `(j, i)`, through EDR.
fn position(
    engine: &NetcdfEngine,
    (j, i): (usize, usize),
    parameter: &str,
    datetime: Option<(&str, &str)>,
    run: Option<&str>,
) -> Result<QueryResult, DataServerError> {
    let (lat, lon) = met_latlon();
    engine
        .query_position(
            &format!("POINT({} {})", lon[j][i], lat[j][i]),
            datetime.map(|(start, end)| (at(start), at(end))),
            Some(&names(&[parameter])),
            None,
            run.map(at),
        )
        .map(single)
}

/// A position series reads each step's stored value at the cell, in the run
/// EDR selects, grib's rule: without `datetime` every step of the newest
/// run; an instant only the step at exactly it, from the newest run that
/// has it; an interval from the newest run with a step inside that also
/// spans its start; a pinned run never another. A window without a step is
/// a 404 (`LocationNotFound`), so a `datetime` list skips it (#968).
#[test]
fn edr_position_reads_the_stored_values_of_the_selected_run() {
    let engine = meps();
    let cell = (2, 7);
    let stored_at = |file: &str| {
        let t2m = stored(&format!("meps-netcdf/{file}"), "air_temperature_2m")[cell.0][cell.1];
        served("air_temperature_2m", t2m)
    };
    let close = |got: &[Option<f64>], want: &[f64]| {
        got.len() == want.len()
            && got
                .iter()
                .zip(want)
                .all(|(g, w)| g.is_some_and(|g| (g - w).abs() < 1e-3))
    };
    let series = |datetime, run| {
        let result = position(&engine, cell, "AIR_TEMPERATURE_2M", datetime, run).unwrap();
        let values = result.ranges["air_temperature_2m"].values.clone();
        (series_hours(&result), values)
    };

    let (hours, values) = series(None, None);
    assert_eq!(hours, [9, 10, 11], "the newest run, every step");
    let newest: Vec<f64> = ["00", "01", "02"]
        .iter()
        .map(|lead| stored_at(&format!("meps_sfc_{lead}_20261015T09Z.nc")))
        .collect();
    assert!(close(&values, &newest), "{values:?} {newest:?}");

    let eight = ("2026-10-15T08:00:00Z", "2026-10-15T08:00:00Z");
    let (hours, values) = series(Some(eight), None);
    assert_eq!(hours, [8], "only the 06Z run has 08Z");
    assert!(close(&values, &[stored_at("meps_sfc_02_20261015T06Z.nc")]));

    let nine = ("2026-10-15T09:00:00Z", "2026-10-15T09:00:00Z");
    let (_, values) = series(Some(nine), None);
    assert!(close(&values, &[stored_at("meps_sfc_00_20261015T09Z.nc")]));
    let (_, values) = series(Some(nine), Some("2026-10-15T06:00:00Z"));
    assert!(close(&values, &[stored_at("meps_sfc_03_20261015T06Z.nc")]));

    // 09Z has 09:00 inside 07:00/09:30 but does not span 07:00; 06Z does.
    let (hours, _) = series(Some(("2026-10-15T07:00:00Z", "2026-10-15T09:30:00Z")), None);
    assert_eq!(hours, [7, 8, 9]);
    // An open start spans no run: the newest with a step inside.
    let open = (DateTime::<Utc>::MIN_UTC, at("2026-10-15T10:00:00Z"));
    let result = engine
        .query_position(
            "POINT(15 63)",
            Some(open),
            Some(&names(&["air_temperature_2m"])),
            None,
            None,
        )
        .map(single)
        .unwrap();
    assert_eq!(series_hours(&result), [9, 10]);

    // No step in the window, or none in the pinned run: a 404.
    for (datetime, run) in [
        (("2026-10-15T08:30:00Z", "2026-10-15T08:30:00Z"), None),
        (eight, Some("2026-10-15T09:00:00Z")),
        (("2026-10-16T00:00:00Z", "2026-10-16T06:00:00Z"), None),
    ] {
        assert!(
            matches!(
                position(&engine, cell, "air_temperature_2m", Some(datetime), run),
                Err(DataServerError::LocationNotFound(_))
            ),
            "{datetime:?} {run:?}"
        );
    }
    assert!(matches!(
        position(
            &engine,
            cell,
            "air_temperature_2m",
            None,
            Some("2026-10-15T03:00:00Z")
        ),
        Err(DataServerError::ReferenceTimeNotFound(_))
    ));
    assert!(matches!(
        position(&engine, cell, "bogus", None, None),
        Err(DataServerError::InvalidParameter(_))
    ));

    // The parameter as the collection describes it.
    let result = position(&engine, cell, "air_temperature_2m", Some(nine), None).unwrap();
    let description = &result.parameters["air_temperature_2m"];
    assert_eq!(description.label, "Screen level temperature (T2M)");
    assert_eq!(description.unit, "°C");
    assert_eq!(
        description.standard_name.as_deref(),
        Some("air_temperature")
    );
    let DomainDescription::PointSeries { x, y, .. } = result.domain else {
        unreachable!()
    };
    let (lat, lon) = met_latlon();
    assert_eq!((x, y), (lon[cell.0][cell.1], lat[cell.0][cell.1]));
    // Off the grid: a null, not an error, and no field is read for it.
    let decodes = engine.field_decodes();
    let far = engine
        .query_position(
            "POINT(-100 10)",
            Some((at(nine.0), at(nine.1))),
            None,
            None,
            None,
        )
        .map(single)
        .unwrap();
    assert!(far
        .ranges
        .values()
        .all(|range| range.values.iter().all(Option::is_none)));
    assert_eq!(
        far.ranges.len(),
        8,
        "every parameter without parameter-name"
    );
    let DomainDescription::PointSeries { t, .. } = &far.domain else {
        unreachable!()
    };
    assert_eq!(t.len(), 1);
    assert!(far.ranges.values().all(|range| range.values.len() == 1));
    assert_eq!(engine.field_decodes(), decodes, "nothing read off the grid");
    // A MULTIPOINT with one point on the grid reads its fields: the point
    // off it stays null.
    let mut series = Vec::new();
    engine
        .query_positions(
            &names(&["POINT(-100 10)", "POINT(15 63)"]),
            Some((at(nine.0), at(nine.1))),
            Some(&names(&["air_temperature_2m"])),
            None,
            None,
            &mut |response| {
                series.push(single(response));
                Ok(())
            },
        )
        .unwrap();
    let values: Vec<Option<f64>> = series
        .iter()
        .map(|result| result.ranges["air_temperature_2m"].values[0])
        .collect();
    assert!(values[0].is_none() && values[1].is_some(), "{values:?}");
    assert_eq!(engine.field_decodes(), decodes + 1);
}

/// A point between cell centres is bilinear in the grid's x/y, as the map
/// draws it, so GetFeatureInfo answers what GetMap drew.
#[test]
fn edr_position_samples_what_the_map_draws() {
    let engine = meps();
    let nine = at("2026-10-15T09:00:00Z");
    for (lon, lat) in [(10.75, 59.91), (24.94, 60.17), (18.0, 69.6), (5.32, 60.39)] {
        let drawn = value_at(
            &engine,
            lon,
            lat,
            Some("wind_speed_of_gust"),
            Some(nine),
            None,
        )
        .unwrap()
        .unwrap();
        let result = single(
            engine
                .query_position(
                    &format!("POINT({lon} {lat})"),
                    Some((nine, nine)),
                    Some(&names(&["wind_speed_of_gust"])),
                    None,
                    None,
                )
                .unwrap(),
        );
        let value = result.ranges["wind_speed_of_gust"].values[0].unwrap();
        assert!(
            (value - drawn).abs() < 1e-3,
            "({lon}, {lat}): {value} {drawn}"
        );
    }
}

/// The `units` attribute of `variable` in a fixture file.
fn stored_unit(file: &str, variable: &str) -> String {
    let nc = netcdf_reader::NcFile::open(testdata(file)).unwrap();
    let var = nc.variable(variable).unwrap();
    var.attribute("units")
        .and_then(|a| a.value.as_string())
        .unwrap()
}

/// #1209: every value is served in its display unit, converted from the
/// variable's CF `units` after sampling: K as °C, Pa as hPa, kg/m^2 as mm.
/// At a cell centre the map pixel, the EDR position value and the query
/// GetFeatureInfo makes (the step and run the map resolved) agree, and are
/// the stored value converted; every unit reported is the converted one. A
/// unit without a rule, CF's `1` or m/s, is served as stored.
#[test]
fn values_are_served_in_display_units() {
    let engine = meps();
    let (j, i) = (5, 4);
    let (lat, lon) = met_latlon();
    let (x, y) = (lon[j][i], lat[j][i]);
    // A map without TIME draws the newest run's last step, 11Z of the 09Z run.
    let file = "meps-netcdf/meps_sfc_02_20261015T09Z.nc";
    let (time, run) = (
        engine.resolve_time(None, None).unwrap(),
        engine.resolve_reference_time(None, None),
    );
    assert_eq!(
        (time, run),
        (at("2026-10-15T11:00:00Z"), Some(at("2026-10-15T09:00:00Z")))
    );
    let info = engine.raster_info();
    let descriptions = engine.get_parameter_descriptions();
    let wind = ds_core::wind::WindSource::wind_facts(&engine);
    for (name, unit, display, expected) in [
        ("air_temperature_2m", "K", "°C", 278.38034 - 273.15),
        ("air_pressure_at_sea_level", "Pa", "hPa", 1007.51086),
        ("precipitation_amount_acc", "kg/m^2", "mm", 0.021972656),
        ("relative_humidity_2m", "1", "1", 1.000006),
        ("wind_speed_of_gust", "m/s", "m/s", 12.461373),
    ] {
        assert_eq!(stored_unit(file, name), unit, "{name}: the fixture's unit");
        let stored = stored(file, name)[j][i];
        // The CDL's value, converted as written above.
        assert!((served(name, stored) - expected).abs() < 1e-4, "{name}");
        let drawn = value_at(&engine, x, y, Some(name), None, None)
            .unwrap()
            .unwrap();
        assert!(
            (drawn - expected).abs() < 1e-3,
            "{name}: drawn {drawn}, expected {expected}"
        );
        let feature_info = single(
            engine
                .query_position(
                    &format!("POINT({x} {y})"),
                    Some((time, time)),
                    Some(&names(&[name])),
                    None,
                    run,
                )
                .unwrap(),
        );
        let queried = feature_info.ranges[name].values[0].unwrap();
        assert!(
            (queried - drawn).abs() < 1e-3,
            "{name}: queried {queried}, drawn {drawn}"
        );
        let series = position(&engine, (j, i), name, None, None).unwrap();
        assert_eq!(series_hours(&series), [9, 10, 11]);
        assert_eq!(series.ranges[name].values[2], Some(queried), "{name}");

        let advertised = info.parameters.iter().find(|p| p.name == name).unwrap();
        assert_eq!(advertised.unit, display, "{name}: RasterInfo");
        assert_eq!(descriptions[name].unit, display, "{name}: EDR metadata");
        assert_eq!(series.parameters[name].unit, display, "{name}: coverage");
    }
    assert_eq!(info.unit, "°C", "the default parameter's");
    // The derived wind combines the components as served: m/s, no rule.
    let u = wind.parameters.iter().find(|p| p.name == "x_wind_10m");
    assert_eq!(u.map(|p| p.unit.as_str()), Some("m/s"));

    // Geopotential in m^2/s^2 as gpm, on a pressure level.
    let pl = meps_pl();
    let ten = at("2026-10-15T10:00:00Z");
    let cell = met_cell((j, i));
    let pl_file = "meps-netcdf-pl/meps_pl_01_20261015T09Z.nc";
    assert_eq!(stored_unit(pl_file, "geopotential_pl"), "m^2/s^2");
    let stored = stored_pl("meps_pl_01_20261015T09Z.nc", "geopotential_pl", 500.0)[j][i];
    let drawn = value_at_level(&pl, cell, "geopotential_pl", ten, Some(500.0))
        .unwrap()
        .unwrap();
    assert!(
        (drawn - stored / 9.80665).abs() < 1e-2,
        "{drawn} gpm, stored {stored} m^2/s^2"
    );
    assert!((5000.0..6000.0).contains(&drawn), "500 hPa near 5.5 km");
    let unit = &pl.get_parameter_descriptions()["geopotential_pl"].unit;
    assert_eq!(unit, "gpm");
}

/// Several points share every field read; a query decodes each field it
/// reads once, uses a field a render has decoded, and leaves none in the
/// render cache: a series must not evict the frames being drawn.
#[test]
fn edr_positions_read_each_field_once_and_cache_none() {
    let engine = meps();
    let first = at("2026-10-15T09:00:00Z");
    let last = at("2026-10-15T11:00:00Z");
    // A render decodes the 11Z field and caches it.
    value_at(
        &engine,
        15.0,
        63.0,
        Some("cloud_area_fraction"),
        Some(last),
        None,
    )
    .unwrap();
    assert_eq!(engine.field_decodes(), 1);
    let points = names(&["POINT(10 60)", "POINT(20 65)", "POINT(25 70)"]);
    let mut series = Vec::new();
    engine
        .query_positions(
            &points,
            None,
            Some(&names(&["cloud_area_fraction"])),
            None,
            None,
            &mut |response| {
                series.push(single(response));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(series.len(), 3);
    assert_eq!(engine.field_decodes(), 3, "two more fields, once each");
    for (result, point) in series.iter().zip(&points) {
        assert_eq!(series_hours(result), [9, 10, 11]);
        let alone = single(
            engine
                .query_position(
                    point,
                    None,
                    Some(&names(&["cloud_area_fraction"])),
                    None,
                    None,
                )
                .unwrap(),
        );
        assert_eq!(
            alone.ranges["cloud_area_fraction"].values,
            result.ranges["cloud_area_fraction"].values
        );
    }
    // The query cached nothing: the 09Z frame is decoded for its render.
    let decodes = engine.field_decodes();
    value_at(
        &engine,
        15.0,
        63.0,
        Some("cloud_area_fraction"),
        Some(first),
        None,
    )
    .unwrap();
    assert_eq!(engine.field_decodes(), decodes + 1);
}

/// An area is a CRS84 grid over the polygon's bbox at the source's mean
/// cell size in degrees, each cell centre sampled as the map draws it and
/// cells outside the polygon null; without `datetime` the step a map
/// without `TIME` draws, an interval on a `t` axis; a polygon off the grid
/// a 404. A radius is the area of its circle.
#[test]
fn edr_area_is_a_crs84_grid_sampled_like_the_map() {
    let engine = meps();
    let triangle = "POLYGON((5 55, 30 55, 5 68, 5 55))";
    let result = single(
        engine
            .query_area(
                triangle,
                None,
                Some(&names(&["air_pressure_at_sea_level"])),
                None,
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid { x, y, t, z } = &result.domain else {
        panic!("not a Grid: {:?}", result.domain);
    };
    assert!(t.is_none() && z.is_none());
    // The fixture's cells are 250 km apart: some 7° of longitude and 2.4°
    // of latitude over its extent.
    assert!(
        (4..=6).contains(&y.len()) && (3..=5).contains(&x.len()),
        "{x:?} {y:?}"
    );
    let range = &result.ranges["air_pressure_at_sea_level"];
    assert_eq!(range.shape, [y.len(), x.len()]);
    assert_eq!(range.axis_names, ["y", "x"]);
    let mut inside = 0;
    for (iy, &lat) in y.iter().enumerate() {
        for (ix, &lon) in x.iter().enumerate() {
            let value = range.values[iy * x.len() + ix];
            // Inside the triangle: west of the line from (30, 55) to (5, 68).
            if lat > 55.0 && lon > 5.0 && (lon - 5.0) / 25.0 + (lat - 55.0) / 13.0 < 1.0 {
                inside += 1;
                let drawn = value_at(
                    &engine,
                    lon,
                    lat,
                    Some("air_pressure_at_sea_level"),
                    None,
                    None,
                )
                .unwrap()
                .unwrap();
                let value = value.unwrap_or_else(|| panic!("({lon}, {lat}) is null"));
                assert!(
                    (value - drawn).abs() < 0.05,
                    "({lon}, {lat}): {value} {drawn}"
                );
            } else if (lon - 5.0) / 25.0 + (lat - 55.0) / 13.0 > 1.05 {
                assert_eq!(value, None, "({lon}, {lat}) is outside the triangle");
            }
        }
    }
    assert!(inside >= 3, "{inside} cells inside");

    // An interval: every step inside it on a `t` axis, here the 06Z run's.
    let window = (at("2026-10-15T07:00:00Z"), at("2026-10-15T08:00:00Z"));
    let result = single(
        engine
            .query_area(
                "10,58,20,64",
                Some(window),
                Some(&names(&["air_temperature_2m", "cloud_area_fraction"])),
                None,
                None,
            )
            .unwrap(),
    );
    let DomainDescription::Grid { x, y, t, .. } = &result.domain else {
        unreachable!()
    };
    let hours: Vec<u32> = t
        .as_ref()
        .unwrap()
        .iter()
        .map(chrono::Timelike::hour)
        .collect();
    assert_eq!(hours, [7, 8]);
    for range in result.ranges.values() {
        assert_eq!(range.shape, [2, y.len(), x.len()]);
        assert_eq!(range.axis_names, ["t", "y", "x"]);
        assert!(range.values.iter().all(Option::is_some));
    }
    assert!(matches!(
        engine.query_area("100,0,110,10", None, None, None, None),
        Err(DataServerError::LocationNotFound(_))
    ));
    assert!(matches!(
        engine.query_area(
            "10,58,20,64",
            Some((at("2026-10-15T07:30:00Z"), at("2026-10-15T07:30:00Z"))),
            None,
            None,
            None
        ),
        Err(DataServerError::LocationNotFound(_))
    ));

    // A radius: its circle's bbox, null outside the circle.
    let result = single(
        engine
            .query_radius(
                "POINT(15 63)",
                600_000.0,
                None,
                Some(&names(&["air_temperature_2m"])),
                None,
                None,
            )
            .unwrap(),
    );
    let values = &result.ranges["air_temperature_2m"].values;
    assert!(values.iter().any(Option::is_some) && values.iter().any(Option::is_none));
}

/// Each run is an EDR instance, ascending, with its own valid times; a
/// collection of files without `forecast_reference_time` has none.
#[test]
fn edr_instances_are_the_runs() {
    let engine = meps();
    assert!(engine.has_instances());
    let runs: Vec<_> = engine
        .get_instances()
        .into_iter()
        .map(|run| {
            let hours: Vec<u32> = run.valid_times.iter().map(chrono::Timelike::hour).collect();
            (run.instance_id(), hours)
        })
        .collect();
    assert_eq!(
        runs,
        [
            ("2026-10-15T06:00:00Z".to_string(), vec![6, 7, 8, 9]),
            ("2026-10-15T09:00:00Z".to_string(), vec![9, 10, 11]),
        ]
    );
    assert_eq!(
        engine.instance_reference_times(),
        [at("2026-10-15T06:00:00Z"), at("2026-10-15T09:00:00Z")]
    );
    let six = engine.find_instance(at("2026-10-15T06:00:00Z")).unwrap();
    assert_eq!(six.valid_times.len(), 4);
    assert!(engine.find_instance(at("2026-10-15T03:00:00Z")).is_none());
    assert_eq!(
        engine.supported_query_types(),
        ["position", "area", "radius"]
    );
    assert_eq!(
        engine.get_temporal_extent(),
        Some((at("2026-10-15T06:00:00Z"), at("2026-10-15T11:00:00Z")))
    );
    assert_eq!(engine.get_available_times().unwrap().len(), 6);

    let global = global();
    assert!(!global.has_instances());
    assert!(global.get_instances().is_empty());
    assert!(global.instance_reference_times().is_empty());
    // The run-less timeline answers without a run, across the seam too.
    let seam = single(
        global
            .query_area("170,10,-170,20", None, None, None, None)
            .unwrap(),
    );
    let DomainDescription::Grid { x, .. } = &seam.domain else {
        unreachable!()
    };
    assert!(x.iter().any(|&lon| lon > 170.0) && x.iter().any(|&lon| lon < -170.0));
    assert!(seam
        .ranges
        .values()
        .all(|range| range.values.iter().all(Option::is_some)));
}

/// MEPS's 10 m wind is grid-relative (`x_wind`/`y_wind`) on a Lambert grid:
/// the derived wind is its speed, on every API, and no direction until the
/// components are turned to true north (#1067 decision 6).
#[test]
fn derived_wind_is_the_speed_of_the_grid_relative_components() {
    use ds_core::wind::{DerivedWind, GridAxes, VectorFrame, WindSource};
    let engine = Arc::new(meps());
    let facts = engine.wind_facts();
    assert_eq!(facts.grid, GridAxes::Rotated);
    let u = facts
        .parameters
        .iter()
        .find(|p| p.name == "x_wind_10m")
        .unwrap();
    assert_eq!(u.frame, VectorFrame::Grid);
    assert_eq!(u.level.as_deref(), Some("10 m"));
    assert!(Arc::ptr_eq(&facts, &engine.wind_facts()), "one snapshot");

    let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = outcomes.clone();
    let wind = DerivedWind::new(
        "meps",
        engine.clone(),
        Arc::new(move |_: &str, outcome: &ds_core::wind::PairOutcome| {
            log.lock().unwrap().push(outcome.clone());
        }),
    );
    let outcome = outcomes.lock().unwrap()[0].clone();
    assert_eq!(outcome.speed.as_deref(), Ok("wind_speed_10m"));
    assert!(outcome.direction.is_err(), "{outcome:?}");
    let info = MapEngine::raster_info(&wind);
    let speed = info
        .parameters
        .iter()
        .find(|p| p.name == "wind_speed_10m")
        .unwrap();
    assert_eq!(
        (speed.title.as_str(), speed.unit.as_str()),
        ("Wind speed (10 m)", "m/s")
    );
    assert!(!info.parameters.iter().any(|p| p.name.contains("direction")));

    let nine = at("2026-10-15T09:00:00Z");
    let (lat, lon) = met_latlon();
    let (j, i) = (4, 3);
    let result = single(
        EdrEngine::query_position(
            &wind,
            &format!("POINT({} {})", lon[j][i], lat[j][i]),
            Some((nine, nine)),
            Some(&names(&["wind_speed_10m"])),
            None,
            None,
        )
        .unwrap(),
    );
    let file = "meps-netcdf/meps_sfc_00_20261015T09Z.nc";
    let (x, y) = (
        stored(file, "x_wind_10m")[j][i],
        stored(file, "y_wind_10m")[j][i],
    );
    let speed = result.ranges["wind_speed_10m"].values[0].unwrap();
    assert!(
        (speed - x.hypot(y)).abs() < 1e-3,
        "the derived speed is not the hypot of the stored components"
    );
    let drawn = wind
        .get_raster_tile(
            [
                lon[j][i] - 1e-4,
                lat[j][i] - 1e-4,
                lon[j][i] + 1e-4,
                lat[j][i] + 1e-4,
            ],
            1,
            1,
            Some(nine),
            &OutputCrs::Wgs84,
            Some("wind_speed_10m"),
            None,
            None,
        )
        .unwrap();
    assert!((drawn.values.value_at(0).unwrap() - speed).abs() < 1e-3);
}

/// Critical Rule 7: EDR queries complete on the shared query runtime's
/// async workers, as api-edr runs them, and on a WMS GetFeatureInfo job's
/// (`RenderJob::run_query`) under a current-thread serving runtime: the
/// field jobs `block_in_place` on the query runtime, never on the runtime
/// serving the request.
#[test]
fn edr_queries_run_on_the_query_runtime() {
    let engine = Arc::new(meps());
    let worker = engine.clone();
    let on_query_runtime = ds_executor::query_runtime()
        .block_on(ds_executor::query_runtime().spawn(async move {
            let _deadline = ds_core::deadline::enter(Some(
                std::time::Instant::now() + std::time::Duration::from_secs(30),
            ));
            worker.query_position(
                "POINT(15 63)",
                None,
                Some(&names(&["air_temperature_2m"])),
                None,
                None,
            )
        }))
        .unwrap()
        .map(single)
        .unwrap();
    assert_eq!(series_hours(&on_query_runtime), [9, 10, 11]);

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
            job.run_query(move || {
                assert!(ds_core::deadline::current().is_some());
                worker.query_position(
                    "POINT(15 63)",
                    Some((at("2026-10-15T10:00:00Z"), at("2026-10-15T10:00:00Z"))),
                    Some(&names(&["air_temperature_2m"])),
                    None,
                    Some(at("2026-10-15T09:00:00Z")),
                )
            })
            .await
        })
        .unwrap()
        .map(single)
        .unwrap();
    assert_eq!(
        from_feature_info.ranges["air_temperature_2m"].values,
        on_query_runtime.ranges["air_temperature_2m"].values[1..2]
    );
}

// ---------------------------------------------------------------------------
// Pressure levels (#1067 PR 8)
// ---------------------------------------------------------------------------

use ds_core::vertical::{VerticalDimension, VerticalKind};

/// The MEPS pressure-level fixture as its example configures it: the `pl`
/// family's names and five variables on 300, 500, 850 and 1000 hPa.
fn meps_pl() -> NetcdfEngine {
    let config = config(
        &testdata("meps-netcdf-pl"),
        r#"filename_template = "meps_pl_{lead}_%Y%m%dT{run}Z.nc"
           variables = ["air_temperature_pl", "x_wind_pl", "y_wind_pl", "geopotential_pl",
                        "relative_humidity_pl"]"#,
    );
    let engine = NetcdfEngine::new("meps-pl", &config).unwrap();
    engine.poll_once();
    engine
}

/// The fixture's levels as stored, top first: stored index of each.
const PL_STORED: [f64; 4] = [300.0, 500.0, 850.0, 1000.0];

/// A stored field of a `meps-netcdf-pl` file at `hpa`, `[row][column]`
/// with row 0 south.
fn stored_pl(file: &str, variable: &str, hpa: f64) -> Vec<Vec<f64>> {
    let k = PL_STORED.iter().position(|&l| l == hpa).unwrap();
    stored(&format!("meps-netcdf-pl/{file}"), variable)[k * 11..(k + 1) * 11].to_vec()
}

/// The value drawn at `(lon, lat)` at level `z`.
fn value_at_level(
    engine: &dyn MapEngine,
    (lon, lat): (f64, f64),
    parameter: &str,
    time: DateTime<Utc>,
    z: Option<f64>,
) -> Result<Option<f64>, DataServerError> {
    let d = 1e-4;
    let tile = engine.get_raster_tile(
        [lon - d, lat - d, lon + d, lat + d],
        1,
        1,
        Some(time),
        &OutputCrs::Wgs84,
        Some(parameter),
        z,
        None,
    )?;
    Ok(tile.values.value_at(0))
}

/// MET Norway's own coordinates of fixture cell `(j, i)`, row 0 south.
fn met_cell((j, i): (usize, usize)) -> (f64, f64) {
    let (lat, lon) = met_latlon();
    (lon[j][i], lat[j][i])
}

/// A multi-level pressure dimension is the collection's vertical axis:
/// `VerticalKind::Pressure` in hPa, bottom first, on the map side
/// (`RasterInfo::vertical`, WMS `ELEVATION`, Maps/Tiles `elevation`) and the
/// EDR side (`z`) alike. The surface family has none.
#[test]
fn pressure_levels_are_the_vertical_axis() {
    let engine = meps_pl();
    let info = engine.raster_info();
    let axis = VerticalDimension::new(VerticalKind::Pressure, vec![1000.0, 850.0, 500.0, 300.0]);
    assert_eq!(info.vertical.as_ref(), Some(&axis));
    assert_eq!(axis.unit(), "hPa");
    assert_eq!(EdrEngine::get_vertical_extent(&engine), Some(axis));
    assert_eq!(engine.raster_info_shared().vertical, info.vertical);
    let names: Vec<&str> = info.parameters.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "air_temperature_pl",
            "x_wind_pl",
            "y_wind_pl",
            "geopotential_pl",
            "relative_humidity_pl"
        ]
    );
    assert_eq!(info.parameters[3].unit, "gpm");
    assert_eq!(
        info.reference_times,
        [at("2026-10-15T06:00:00Z"), at("2026-10-15T09:00:00Z")]
    );
    let hours: Vec<u32> = info.times.iter().map(chrono::Timelike::hour).collect();
    assert_eq!(hours, [6, 7, 8, 9, 10]);

    let surface = meps();
    assert_eq!(surface.raster_info().vertical, None);
    assert_eq!(EdrEngine::get_vertical_extent(&surface), None);
}

/// `ELEVATION`/`elevation` select a level exactly: each draws its own
/// stored field, a level the collection lacks is a 400 naming the levels,
/// and several bands at one level sample that level. Each level is a field
/// of its own in the field cache: two levels are two decodes, and drawing
/// one again decodes nothing.
#[test]
fn each_pressure_level_draws_its_own_field() {
    let engine = meps_pl();
    let ten = at("2026-10-15T10:00:00Z");
    let (j, i) = (5, 4);
    let cell = met_cell((j, i));
    let mut drawn = Vec::new();
    for hpa in [1000.0, 850.0, 500.0, 300.0] {
        let value = value_at_level(&engine, cell, "air_temperature_pl", ten, Some(hpa))
            .unwrap()
            .unwrap();
        let stored = stored_pl("meps_pl_01_20261015T09Z.nc", "air_temperature_pl", hpa)[j][i];
        let stored = served("air_temperature_pl", stored);
        assert!((value - stored).abs() < 1e-3, "{hpa}: {value} {stored}");
        drawn.push(value);
    }
    assert!(
        drawn.windows(2).all(|pair| pair[0] > pair[1]),
        "colder aloft: {drawn:?}"
    );
    let missing = value_at_level(&engine, cell, "air_temperature_pl", ten, Some(600.0));
    assert!(
        matches!(&missing, Err(DataServerError::InvalidParameter(m)) if m.contains("1000, 850, 500, 300 hPa")),
        "{missing:?}"
    );
    // Several bands at one level, from one output map.
    let bbox = [5.0, 55.0, 30.0, 70.0];
    let bands = engine
        .get_raster_tiles(
            bbox,
            16,
            12,
            Some(ten),
            &OutputCrs::Wgs84,
            &["air_temperature_pl", "geopotential_pl"],
            Some(500.0),
            None,
        )
        .unwrap();
    for (band, parameter) in bands.iter().zip(["air_temperature_pl", "geopotential_pl"]) {
        let one = engine
            .get_raster_tile(
                bbox,
                16,
                12,
                Some(ten),
                &OutputCrs::Wgs84,
                Some(parameter),
                Some(500.0),
                None,
            )
            .unwrap();
        assert_eq!(values(band), values(&one), "{parameter}");
    }

    // A fresh time: two levels are two fields; again, none.
    let six = at("2026-10-15T06:00:00Z");
    let before = engine.field_decodes();
    for hpa in [850.0, 300.0] {
        value_at_level(&engine, cell, "y_wind_pl", six, Some(hpa)).unwrap();
    }
    assert_eq!(engine.field_decodes() - before, 2);
    for hpa in [850.0, 300.0] {
        value_at_level(&engine, cell, "y_wind_pl", six, Some(hpa)).unwrap();
    }
    assert_eq!(engine.field_decodes() - before, 2, "both levels cached");
}

/// A map without `ELEVATION` draws the first advertised level, the bottom
/// one (1000 hPa), so the default `resolve_elevation` names the level drawn
/// and GetFeatureInfo, which queries that level, reads what GetMap drew.
#[test]
fn a_map_without_elevation_draws_the_level_get_feature_info_queries() {
    let engine = meps_pl();
    let nine = at("2026-10-15T09:00:00Z");
    let bbox = [5.0, 55.0, 30.0, 70.0];
    let tile = |z: Option<f64>| {
        engine
            .get_raster_tile(
                bbox,
                24,
                16,
                Some(nine),
                &OutputCrs::Wgs84,
                Some("geopotential_pl"),
                z,
                None,
            )
            .unwrap()
    };
    let default = values(&tile(None));
    assert!(default.iter().any(Option::is_some));
    assert_eq!(default, values(&tile(Some(1000.0))));
    assert_ne!(default, values(&tile(Some(300.0))));
    let level = engine.resolve_elevation(Some("geopotential_pl"), None, Some(nine), None);
    assert_eq!(level, Some(1000.0));
    assert_eq!(
        engine.resolve_elevation(Some("geopotential_pl"), Some(500.0), Some(nine), None),
        Some(500.0)
    );

    // GetFeatureInfo's query: the resolved level, the drawn step.
    let cell = met_cell((6, 3));
    let drawn = value_at_level(&engine, cell, "geopotential_pl", nine, None)
        .unwrap()
        .unwrap();
    let result = single(
        engine
            .query_position(
                &format!("POINT({} {})", cell.0, cell.1),
                Some((nine, nine)),
                Some(&names(&["geopotential_pl"])),
                Some(&[level.unwrap()]),
                None,
            )
            .unwrap(),
    );
    let queried = result.ranges["geopotential_pl"].values[0].unwrap();
    assert!((queried - drawn).abs() < 1e-2, "{queried} {drawn}");
}

/// EDR `z`: one level is a series with a one-value `z` axis; several, or
/// every level without `z`, a vertical profile per step, in `z`'s order;
/// an area puts the levels on a `z` axis, `[t, z, y, x]`. A `z` naming
/// none of the levels is a 400.
#[test]
fn edr_z_selects_the_levels_a_query_reads() {
    let engine = meps_pl();
    let (j, i) = (4, 6);
    let cell = met_cell((j, i));
    let point = format!("POINT({} {})", cell.0, cell.1);
    let ten = at("2026-10-15T10:00:00Z");
    let file = "meps_pl_01_20261015T09Z.nc";
    let query = |datetime: Option<(DateTime<Utc>, DateTime<Utc>)>, z: Option<&[f64]>| {
        engine.query_position(
            &point,
            datetime,
            Some(&names(&["air_temperature_pl", "relative_humidity_pl"])),
            z,
            None,
        )
    };

    // Two levels at one step: one vertical profile, in `z`'s order.
    let CoverageResponse::Collection(profiles) =
        query(Some((ten, ten)), Some(&[850.0, 500.0])).unwrap()
    else {
        panic!("a profile per step");
    };
    assert_eq!(profiles.len(), 1);
    let DomainDescription::VerticalProfile { t, z, .. } = &profiles[0].domain else {
        panic!("{:?}", profiles[0].domain);
    };
    assert_eq!(
        (*t, z.kind, z.values.clone()),
        (Some(ten), VerticalKind::Pressure, vec![850.0, 500.0])
    );
    for parameter in ["air_temperature_pl", "relative_humidity_pl"] {
        let range = &profiles[0].ranges[parameter];
        assert_eq!(
            (range.shape.as_slice(), range.axis_names.as_slice()),
            (&[2][..], &["z".to_string()][..])
        );
        for (value, hpa) in range.values.iter().zip([850.0, 500.0]) {
            let stored = served(parameter, stored_pl(file, parameter, hpa)[j][i]);
            assert!((value.unwrap() - stored).abs() < 1e-3, "{parameter} {hpa}");
        }
    }

    // Without `z`: every level of every step of the newest run.
    let CoverageResponse::Collection(profiles) = query(None, None).unwrap() else {
        panic!("a profile per step");
    };
    let steps: Vec<Option<DateTime<Utc>>> = profiles
        .iter()
        .map(|p| match &p.domain {
            DomainDescription::VerticalProfile { t, z, .. } => {
                assert_eq!(z.values, [1000.0, 850.0, 500.0, 300.0]);
                *t
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(steps, [Some(at("2026-10-15T09:00:00Z")), Some(ten)]);
    let at_850 = profiles[1].ranges["air_temperature_pl"].values[1].unwrap();
    let stored = served(
        "air_temperature_pl",
        stored_pl(file, "air_temperature_pl", 850.0)[j][i],
    );
    assert!((at_850 - stored).abs() < 1e-3);

    // One level: a series over the run's steps with a one-value `z` axis.
    let series = single(query(None, Some(&[300.0])).unwrap());
    let DomainDescription::PointSeries { t, z, .. } = &series.domain else {
        panic!("{:?}", series.domain);
    };
    assert_eq!(t.len(), 2);
    assert_eq!(z.as_ref().map(|z| z.values.clone()), Some(vec![300.0]));
    let stored = stored_pl(file, "relative_humidity_pl", 300.0)[j][i];
    let value = series.ranges["relative_humidity_pl"].values[1].unwrap();
    assert!((value - stored).abs() < 1e-3, "{value} {stored}");

    // None of the levels: a 400.
    assert!(matches!(
        query(Some((ten, ten)), Some(&[925.0])),
        Err(DataServerError::InvalidParameter(_))
    ));

    // An area: the levels on a `z` axis, each the single level's grid.
    let area = |z: &[f64]| {
        single(
            engine
                .query_area(
                    "POLYGON((10 58,20 58,20 64,10 64,10 58))",
                    Some((at("2026-10-15T09:00:00Z"), ten)),
                    Some(&names(&["air_temperature_pl"])),
                    Some(z),
                    None,
                )
                .unwrap(),
        )
    };
    let both = area(&[1000.0, 300.0]);
    let DomainDescription::Grid { x, y, t, z } = &both.domain else {
        panic!("{:?}", both.domain);
    };
    assert_eq!(t.as_ref().map(Vec::len), Some(2));
    assert_eq!(
        z.as_ref().map(|z| z.values.clone()),
        Some(vec![1000.0, 300.0])
    );
    let range = &both.ranges["air_temperature_pl"];
    let (ny, nx) = (y.len(), x.len());
    assert_eq!(range.shape, [2, 2, ny, nx]);
    assert_eq!(range.axis_names, ["t", "z", "y", "x"]);
    let upper = area(&[300.0]);
    let one = &upper.ranges["air_temperature_pl"];
    assert_eq!(one.shape, [2, 1, ny, nx]);
    let plane = ny * nx;
    for step in 0..2 {
        assert_eq!(
            range.values[(step * 2 + 1) * plane..(step * 2 + 2) * plane],
            one.values[step * plane..(step + 1) * plane],
            "step {step}: the 300 hPa plane"
        );
    }
}

/// Derived wind on pressure levels: `x_wind_pl`/`y_wind_pl` have no pairing
/// level (they span the vertical axis) and pair into `wind_speed_pl`, read
/// at the one level asked for, on the map and in EDR, speed only on the
/// Lambert grid. The fixture's 300 hPa jet passes the surface components'
/// ±40 m/s, which is why the pressure-level styles are wider.
#[test]
fn derived_wind_pairs_the_components_level_by_level() {
    use ds_core::wind::{DerivedWind, VectorFrame, WindSource};
    let engine = Arc::new(meps_pl());
    let facts = engine.wind_facts();
    let u = facts
        .parameters
        .iter()
        .find(|p| p.name == "x_wind_pl")
        .unwrap();
    assert_eq!((u.frame, u.level.as_deref()), (VectorFrame::Grid, None));
    let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = outcomes.clone();
    let wind = DerivedWind::new(
        "meps-pl",
        engine.clone(),
        Arc::new(move |_: &str, outcome: &ds_core::wind::PairOutcome| {
            log.lock().unwrap().push(outcome.clone());
        }),
    );
    let outcome = outcomes.lock().unwrap()[0].clone();
    assert_eq!(outcome.speed.as_deref(), Ok("wind_speed_pl"));
    assert!(outcome.direction.is_err(), "{outcome:?}");
    let info = MapEngine::raster_info(&wind);
    let speed = info
        .parameters
        .iter()
        .find(|p| p.name == "wind_speed_pl")
        .unwrap();
    assert_eq!(
        (speed.title.as_str(), speed.unit.as_str()),
        ("Wind speed", "m/s")
    );
    assert_eq!(info.vertical, engine.raster_info().vertical);

    let six = at("2026-10-15T06:00:00Z");
    let (j, i) = (4, 0);
    let cell = met_cell((j, i));
    let file = "meps_pl_00_20261015T06Z.nc";
    let hypot = |hpa: f64| {
        stored_pl(file, "x_wind_pl", hpa)[j][i].hypot(stored_pl(file, "y_wind_pl", hpa)[j][i])
    };
    assert!(
        stored_pl(file, "x_wind_pl", 300.0)[j][i] > 40.0,
        "the 300 hPa jet in the fixture"
    );
    let CoverageResponse::Collection(profiles) = EdrEngine::query_position(
        &wind,
        &format!("POINT({} {})", cell.0, cell.1),
        Some((six, six)),
        Some(&names(&["wind_speed_pl"])),
        Some(&[300.0, 850.0]),
        Some(six),
    )
    .unwrap() else {
        panic!("a profile");
    };
    let values = &profiles[0].ranges["wind_speed_pl"].values;
    for (value, hpa) in values.iter().zip([300.0, 850.0]) {
        let value = value.unwrap();
        assert!(
            (value - hypot(hpa)).abs() < 1e-3,
            "{hpa}: {value} {}",
            hypot(hpa)
        );
        let drawn = value_at_level(&wind, cell, "wind_speed_pl", six, Some(hpa))
            .unwrap()
            .unwrap();
        assert!((drawn - value).abs() < 1e-3, "{hpa}: {drawn} {value}");
    }
    assert!(values[0].unwrap() > values[1].unwrap(), "faster aloft");
}

/// A pressure axis stored in Pa (`level`, 50000/85000/100000 Pa, top first)
/// is served in hPa, bottom first, and selected in hPa. On a
/// latitude/longitude grid earth-relative u/v (`eastward_wind`/
/// `northward_wind`) give speed and direction at every level.
#[test]
fn pressure_in_pa_is_served_in_hpa_with_wind_direction_per_level() {
    use ds_core::wind::DerivedWind;
    let engine =
        Arc::new(NetcdfEngine::new("pa", &config(&testdata("netcdf-pressure-pa"), "")).unwrap());
    let info = engine.raster_info();
    assert_eq!(
        info.vertical,
        Some(VerticalDimension::new(
            VerticalKind::Pressure,
            vec![1000.0, 850.0, 500.0]
        ))
    );
    assert!(info.reference_times.is_empty());
    let six = at("2026-01-01T06:00:00Z");
    // Column 2 (22°E), row 1 (61°N), step 1: t = 300 - 0.05 hPa + 1 + 0.2 - 0.2
    // K, served in °C (#1209).
    let cell = (22.0, 61.0);
    let t850 = value_at_level(engine.as_ref(), cell, "t", six, Some(850.0))
        .unwrap()
        .unwrap();
    assert!((t850 - (258.5 - 273.15)).abs() < 1e-4, "{t850}");
    let t_default = value_at_level(engine.as_ref(), cell, "t", six, None)
        .unwrap()
        .unwrap();
    assert!(
        (t_default - (251.0 - 273.15)).abs() < 1e-4,
        "1000 hPa: {t_default}"
    );
    assert!(value_at_level(engine.as_ref(), cell, "t", six, Some(85000.0)).is_err());

    let wind = DerivedWind::new(
        "pa",
        engine.clone(),
        Arc::new(|_: &str, _: &ds_core::wind::PairOutcome| {}),
    );
    let CoverageResponse::Collection(profiles) = EdrEngine::query_position(
        &wind,
        "POINT(22 61)",
        Some((six, six)),
        Some(&names(&["ws", "wdir"])),
        Some(&[1000.0, 500.0]),
        None,
    )
    .unwrap() else {
        panic!("a profile");
    };
    // u = 10 (k + 1) + 1 + 1, v = -5 (k + 1) + 0.25 at stored index k:
    // 1000 hPa is k = 2, 500 hPa k = 0.
    for (n, (u, v)) in [(32.0_f64, -14.75_f64), (12.0, -4.75)]
        .into_iter()
        .enumerate()
    {
        let speed = profiles[0].ranges["ws"].values[n].unwrap();
        let direction = profiles[0].ranges["wdir"].values[n].unwrap();
        assert!((speed - u.hypot(v)).abs() < 1e-4, "{n}: {speed}");
        let expected = (-u).atan2(-v).to_degrees().rem_euclid(360.0);
        assert!(
            (direction - expected).abs() < 1e-3,
            "{n}: {direction} {expected}"
        );
    }
}
