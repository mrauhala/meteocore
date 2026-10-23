//! OPeNDAP reads (#1264) against the DAP2 server of [`crate::test_server`],
//! which answers `.dds`, `.das` and `.dods` with constraints for the
//! committed MEPS fixtures as thredds.met.no's `dodsC` answers, chunked and
//! without `Last-Modified`, the files found through THREDDS catalogs: a
//! collection read through OPeNDAP serves what the same files serve read
//! locally, field for field, levels and runs included; the requests an
//! open and a field cost; replaced and removed files; and THREDDS's own
//! metadata of a MEPS file, ingested.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DurationRound, Timelike};
use ds_core::config::NetcdfConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};

use super::*;
use crate::ingest::{inspect, FileGrid, Wanted};
use crate::source::Reader;
use crate::test_server::{thredds_catalog, Request, TestServer};
use crate::NetcdfEngine;

const CATALOGS: &str = "thredds/catalog/meps25epsarchive";
const DAP: &str = "thredds/dodsC/meps25epsarchive";

fn testdata(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(path)
}

/// The two run slots files are published in: the newer an hour before now
/// on the hour, the older three hours before it.
fn slots() -> (DateTime<Utc>, DateTime<Utc>) {
    let late = (Utc::now() - chrono::Duration::hours(1))
        .duration_trunc(chrono::Duration::hours(1))
        .unwrap();
    (late - chrono::Duration::hours(3), late)
}

/// A file of `family`'s `lead` in the slot at `slot`: its directory under
/// the archive and its name, as on thredds.met.no.
fn place(family: &str, lead: u32, slot: DateTime<Utc>) -> (String, String) {
    (
        slot.format("%Y/%m/%d/%H/member_00").to_string(),
        format!("meps_{family}_{lead:02}_{}Z.nc", slot.format("%Y%m%dT%H")),
    )
}

/// The OPeNDAP path of `family`'s `lead` in `slot`.
fn dataset(family: &str, lead: u32, slot: DateTime<Utc>) -> String {
    let (dir, name) = place(family, lead, slot);
    [DAP, &dir, &name].join("/")
}

/// One fixture file published as `(family, lead)` in a slot.
type Published = (&'static str, &'static str, u32, DateTime<Utc>);

/// Serve `files` as OPeNDAP datasets (Grids when `grids`, replies carrying
/// `last_modified` when given), with THREDDS catalogs from the archive
/// root down to each slot's `member_00`, listing them as modified an hour
/// ago, all chunked.
fn publish(
    server: &TestServer,
    files: &[Published],
    grids: bool,
    last_modified: Option<DateTime<Utc>>,
) {
    server.chunk_whole_gets();
    let modified = (Utc::now() - chrono::Duration::hours(1))
        .duration_trunc(chrono::Duration::seconds(1))
        .unwrap();
    type Listing = (BTreeSet<String>, Vec<(String, String, String)>);
    let mut tree: BTreeMap<String, Listing> = BTreeMap::new();
    for &(fixture, family, lead, slot) in files {
        let bytes = std::fs::read(testdata(fixture)).unwrap();
        let (dir, name) = place(family, lead, slot);
        let parts: Vec<&str> = dir.split('/').collect();
        for depth in 0..parts.len() {
            tree.entry(parts[..depth].join("/"))
                .or_default()
                .0
                .insert(parts[depth].to_string());
        }
        tree.entry(dir.clone()).or_default().1.push((
            ["meps25epsarchive", &dir, &name].join("/"),
            format!("{:.2}", bytes.len() as f64 / 1000.0),
            modified.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ));
        server.put_opendap(&dataset(family, lead, slot), bytes, grids, last_modified);
    }
    for (dir, (children, listed)) in tree {
        let refs: Vec<String> = children
            .iter()
            .map(|c| [c, "catalog.xml"].join("/"))
            .collect();
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        let path = [CATALOGS, &dir, "catalog.xml"]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        server.put(&path, thredds_catalog(&refs, &listed), modified);
    }
}

/// The MEPS sfc fixture's 06Z and 09Z runs, leads 00-02 each, in the two
/// slots: time comes from the file, never its name.
fn sfc_files() -> Vec<Published> {
    let (early, late) = slots();
    let mut files = Vec::new();
    for (run, slot) in [("06", early), ("09", late)] {
        for (lead, fixture) in [
            (0, ["meps-netcdf/meps_sfc_00_20261015T", run, "Z.nc"]),
            (1, ["meps-netcdf/meps_sfc_01_20261015T", run, "Z.nc"]),
            (2, ["meps-netcdf/meps_sfc_02_20261015T", run, "Z.nc"]),
        ] {
            let fixture: &'static str = Box::leak(fixture.concat().into_boxed_str());
            files.push((fixture, "sfc", lead, slot));
        }
    }
    files
}

/// The MEPS pl fixture's 06Z and 09Z runs, leads 00-01 each.
fn pl_files() -> Vec<Published> {
    let (early, late) = slots();
    vec![
        ("meps-netcdf-pl/meps_pl_00_20261015T06Z.nc", "pl", 0, early),
        ("meps-netcdf-pl/meps_pl_01_20261015T06Z.nc", "pl", 1, early),
        ("meps-netcdf-pl/meps_pl_00_20261015T09Z.nc", "pl", 0, late),
        ("meps-netcdf-pl/meps_pl_01_20261015T09Z.nc", "pl", 1, late),
    ]
}

const SFC_VARIABLES: &str = r#"variables = ["air_temperature_2m", "relative_humidity_2m",
    "x_wind_10m", "y_wind_10m", "wind_speed_of_gust", "air_pressure_at_sea_level",
    "precipitation_amount_acc", "cloud_area_fraction"]"#;

/// A collection reading `family`'s `leads` through OPeNDAP from `server`,
/// as the shipped example does.
fn config(server: &TestServer, family: &str, leads: &[u32], extra: &str) -> NetcdfConfig {
    let (early, late) = slots();
    toml::from_str(&format!(
        r#"access = "opendap"
           data_path = "{url}/{DAP}/"
           catalog_url = "{url}/{CATALOGS}/catalog.xml"
           prefix_pattern = "%Y/%m/%d/{{run}}/member_00/"
           filename_template = "meps_{family}_{{lead}}_%Y%m%dT{{run}}Z.nc"
           run_hours = [{}, {}]
           leads = {leads:?}
           time_window = "-PT6H"
           max_runs = 2
           {extra}"#,
        early.hour(),
        late.hour(),
        url = server.url,
    ))
    .unwrap()
}

/// An engine over `server` that has opened every published run: the first
/// scan opens the newest, the poll the other.
fn opened(config: &NetcdfConfig) -> NetcdfEngine {
    let engine = NetcdfEngine::new("dap", config).unwrap();
    engine.poll_once();
    engine
}

/// The same fixture files read locally, from a directory of their own.
fn local(files: &[Published], family: &str, extra: &str) -> (tempfile::TempDir, NetcdfEngine) {
    let dir = tempfile::tempdir().unwrap();
    for (fixture, ..) in files {
        let path = testdata(fixture);
        std::fs::copy(&path, dir.path().join(path.file_name().unwrap())).unwrap();
    }
    let config: NetcdfConfig = toml::from_str(&format!(
        "data_path = {:?}\nfilename_template = \"meps_{family}_{{lead}}_%Y%m%dT{{run}}Z.nc\"\n{extra}",
        dir.path().display().to_string()
    ))
    .unwrap();
    let engine = NetcdfEngine::new("local", &config).unwrap();
    (dir, engine)
}

fn values(tile: &ds_core::map_engine::RasterTile) -> Vec<Option<f64>> {
    (0..(tile.width * tile.height) as usize)
        .map(|i| tile.values.value_at(i))
        .collect()
}

/// An EDR position series of `parameter` at the middle of the grid, in
/// `run`, at `z`.
fn series(
    engine: &NetcdfEngine,
    parameter: &str,
    z: Option<f64>,
    run: DateTime<Utc>,
) -> Result<Vec<Option<f64>>, DataServerError> {
    let [w, s, e, n] = engine.raster_info().spatial_extent.unwrap();
    let z = z.map(|z| vec![z]);
    let response = engine.query_position(
        &format!("POINT({} {})", (w + e) / 2.0, (s + n) / 2.0),
        None,
        Some(&[parameter.to_string()]),
        z.as_deref(),
        Some(run),
    )?;
    let ds_core::model::CoverageResponse::Single(result) = response else {
        panic!("not one coverage");
    };
    Ok(result.ranges[parameter].values.clone())
}

/// `dap` serves what `local` serves: the same parameters, units, titles,
/// times, runs, grid, extent and levels; every parameter at every level,
/// run and time drawn to the same pixels (or refused alike); the same EDR
/// series; the same instances.
fn assert_serve_alike(dap: &NetcdfEngine, local: &NetcdfEngine) {
    let (a, b) = (dap.raster_info(), local.raster_info());
    assert_eq!(a.parameters, b.parameters);
    assert_eq!(
        (&a.times, &a.reference_times, a.grid_size, &a.native_crs),
        (&b.times, &b.reference_times, b.grid_size, &b.native_crs)
    );
    assert_eq!(
        (a.spatial_extent, &a.vertical),
        (b.spatial_extent, &b.vertical)
    );
    assert_eq!(a.reference_times.len(), 2, "both runs");
    assert_eq!(dap.get_instances(), local.get_instances());
    let levels: Vec<Option<f64>> = a
        .vertical
        .as_ref()
        .map_or(vec![None], |v| v.levels.iter().copied().map(Some).collect());
    let extent = a.spatial_extent.unwrap();
    let mut drawn = 0;
    for &run in &a.reference_times {
        for parameter in &a.parameters {
            for &z in &levels {
                for &time in &a.times {
                    let draw = |engine: &NetcdfEngine| {
                        engine
                            .get_raster_tile(
                                extent,
                                12,
                                12,
                                Some(time),
                                &OutputCrs::Wgs84,
                                Some(&parameter.name),
                                z,
                                Some(run),
                            )
                            .map(|tile| values(&tile))
                    };
                    match (draw(dap), draw(local)) {
                        (Ok(through_dap), Ok(read_locally)) => {
                            assert_eq!(
                                through_dap, read_locally,
                                "{} at {time} of {run}, level {z:?}",
                                parameter.name
                            );
                            drawn += through_dap.iter().filter(|v| v.is_some()).count();
                        }
                        (Err(_), Err(_)) => {}
                        (dap, local) => panic!("{}: {dap:?} against {local:?}", parameter.name),
                    }
                }
                assert_eq!(
                    series(dap, &parameter.name, z, run).unwrap(),
                    series(local, &parameter.name, z, run).unwrap(),
                    "{} series of {run}, level {z:?}",
                    parameter.name
                );
            }
        }
    }
    assert!(drawn > 1000, "{drawn} pixels drawn");
}

/// The issue's acceptance test: MEPS's sfc fixture through OPeNDAP serves
/// what it serves read locally, field for field: the eight variables and
/// the per-step precipitation, display units, both runs, the Lambert grid
/// stored south-up, EDR series. With Grids, as THREDDS serves fields, a
/// field read projects the Grid's array; served as plain arrays, the
/// variable itself.
#[test]
fn an_opendap_collection_serves_what_its_files_serve() {
    let files = sfc_files();
    let (_dir, local) = local(&files, "sfc", SFC_VARIABLES);
    for grids in [true, false] {
        let server = TestServer::new();
        publish(&server, &files, grids, None);
        let dap = opened(&config(&server, "sfc", &[0, 1, 2], SFC_VARIABLES));
        assert_eq!(dap.raster_info().times.len(), 6);
        server.take();
        assert_serve_alike(&dap, &local);
        let fields: Vec<String> = server
            .take()
            .into_iter()
            .filter(|r| r.path.contains(".dods?air_temperature_2m"))
            .map(|r| r.path)
            .collect();
        let projected = if grids {
            "air_temperature_2m.air_temperature_2m%5B"
        } else {
            "air_temperature_2m%5B"
        };
        assert!(
            !fields.is_empty() && fields.iter().all(|path| path.contains(projected)),
            "{fields:#?}"
        );
    }
}

/// Pressure levels through OPeNDAP: the same levels, bottom first, and each
/// level its own hyperslab of the stored pressure axis (MEPS stores them
/// top first).
#[test]
fn pressure_levels_are_read_level_by_level() {
    let files = pl_files();
    let variables = r#"variables = ["air_temperature_pl", "x_wind_pl", "y_wind_pl",
        "geopotential_pl", "relative_humidity_pl"]"#;
    let (_dir, local) = local(&files, "pl", variables);
    let server = TestServer::new();
    publish(&server, &files, true, None);
    let dap = opened(&config(&server, "pl", &[0, 1], variables));
    let levels = dap.raster_info().vertical.unwrap().levels;
    assert_eq!(levels, [1000.0, 850.0, 500.0, 300.0]);
    server.take();
    assert_serve_alike(&dap, &local);
    let slabs: BTreeSet<String> = server
        .take()
        .into_iter()
        .filter_map(|r| {
            let (_, slab) = r
                .path
                .split_once("air_temperature_pl.air_temperature_pl%5B0%5D")?;
            Some(slab.split("%5D").next()?.to_string())
        })
        .collect();
    assert_eq!(
        slabs.into_iter().collect::<Vec<_>>(),
        ["%5B0", "%5B1", "%5B2", "%5B3"],
        "every stored level, one per read"
    );
}

fn dap_requests(requests: &[Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|r| r.path.starts_with(DAP))
        .map(|r| r.path[DAP.len() + 1..].to_string())
        .collect()
}

/// An open is three requests in two rounds: the `.dds` and `.das`, then
/// one `.dods` of every scalar and coordinate. A field drawn is one `.dods`
/// of its hyperslab, every reply chunked as THREDDS sends them; drawn
/// again, none. An EDR query reads a field not drawn with one more, and
/// the same query again none: the reply stays in the chunk cache, so a
/// query past its deadline still leaves its fields fetched.
#[test]
fn an_open_is_three_requests_and_a_field_one() {
    let (_, late) = slots();
    let server = TestServer::new();
    publish(
        &server,
        &[("meps-netcdf/meps_sfc_00_20261015T09Z.nc", "sfc", 0, late)],
        true,
        None,
    );
    let engine = NetcdfEngine::new("dap", &config(&server, "sfc", &[0], SFC_VARIABLES)).unwrap();
    let requests = server.take();
    let (dir, name) = place("sfc", 0, late);
    let file = [dir.as_str(), &name].join("/");
    let opened = dap_requests(&requests);
    assert_eq!(opened.len(), 3, "{opened:#?}");
    assert_eq!(opened[..2].iter().collect::<BTreeSet<_>>().len(), 2);
    for suffix in [".dds", ".das"] {
        assert!(opened.contains(&[file.as_str(), suffix].concat()));
    }
    let (dods, coordinates) = opened[2].split_once('?').unwrap();
    assert_eq!(dods, [file.as_str(), ".dods"].concat());
    let coordinates: BTreeSet<&str> = coordinates.split(',').collect();
    assert_eq!(
        coordinates,
        BTreeSet::from([
            "forecast_reference_time",
            "height0",
            "height1",
            "height6",
            "height_above_msl",
            "projection_lambert",
            "time",
            "x",
            "y"
        ]),
        "every scalar and coordinate, in one reply"
    );
    assert!(requests.iter().all(|r| r.chunked && r.status == 200));

    let extent = engine.raster_info().spatial_extent.unwrap();
    let draw = |parameter: &str| {
        engine
            .get_raster_tile(
                extent,
                8,
                8,
                None,
                &OutputCrs::Wgs84,
                Some(parameter),
                None,
                None,
            )
            .unwrap()
    };
    draw("air_temperature_2m");
    assert_eq!(
        dap_requests(&server.take()),
        [format!(
            "{file}.dods?air_temperature_2m.air_temperature_2m\
             %5B0%5D%5B0%5D%5B0:1:10%5D%5B0:1:9%5D"
        )]
    );
    draw("air_temperature_2m");
    assert!(server.take().is_empty(), "drawn from the field cache");

    let run = engine.raster_info().reference_times[0];
    series(&engine, "wind_speed_of_gust", None, run).unwrap();
    assert_eq!(dap_requests(&server.take()).len(), 1);
    series(&engine, "wind_speed_of_gust", None, run).unwrap();
    assert!(server.take().is_empty(), "the reply is cached");
    assert!(engine.remote_traffic().unwrap().gets >= 5);
}

/// THREDDS sends no `Last-Modified`: a file removed from the server is a
/// 404 to the next read, a 503 to that request, its cached pixels stale at
/// once; the next poll reads its catalog again and opens it, and with the
/// dataset gone, drops it. A server sending `Last-Modified` with the
/// `.dds` has every reply compared with it: a replaced file is found by
/// its next read, and opened again.
#[test]
fn a_replaced_or_removed_file_is_found_by_its_next_read() {
    let (_, late) = slots();
    let lead = |lead: u32| -> Published {
        let fixture = [
            "meps-netcdf/meps_sfc_00_20261015T09Z.nc",
            "meps-netcdf/meps_sfc_01_20261015T09Z.nc",
        ][lead as usize];
        (fixture, "sfc", lead, late)
    };
    let gone = |result: Result<ds_core::map_engine::RasterTile, DataServerError>| {
        matches!(result, Err(DataServerError::ResourceExhausted))
    };
    for modified in [None, Some(Utc::now() - chrono::Duration::hours(2))] {
        let server = TestServer::new();
        publish(&server, &[lead(0), lead(1)], true, modified);
        let engine = opened(&config(&server, "sfc", &[0, 1], SFC_VARIABLES));
        let times = engine.times();
        assert_eq!(times.len(), 2);
        let version = engine.content_version();
        let extent = engine.raster_info().spatial_extent.unwrap();
        let draw = |time: DateTime<Utc>| {
            engine.get_raster_tile(
                extent,
                8,
                8,
                Some(time),
                &OutputCrs::Wgs84,
                Some("air_temperature_2m"),
                None,
                None,
            )
        };
        let path = dataset("sfc", 1, late);
        match modified {
            None => server.remove_opendap(&path),
            Some(before) => {
                let bytes = std::fs::read(testdata(lead(1).0)).unwrap();
                let after = before + chrono::Duration::minutes(30);
                server.put_opendap(&path, bytes, true, Some(after));
            }
        }
        server.take();
        assert!(gone(draw(times[1])), "{modified:?}");
        assert!(engine.content_version() > version);
        engine.poll_once();
        let polled = server.take();
        assert!(
            polled.iter().any(|r| r
                .path
                .ends_with(&[place("sfc", 1, late).0.as_str(), "catalog.xml"].join("/"))),
            "the catalog read again: {polled:#?}"
        );
        assert!(polled
            .iter()
            .any(|r| r.path == [path.as_str(), ".dds"].concat()));
        match modified {
            None => assert_eq!(engine.times(), times[..1], "the removed file dropped"),
            Some(_) => {
                assert_eq!(engine.times(), times, "the new version opened");
                assert!(draw(times[1]).is_ok());
            }
        }
    }
}

/// thredds.met.no's own metadata of a MEPS file (testdata/netcdf-opendap,
/// fetched 2026-10-23: the DDS, DAS and coordinates reply its open reads),
/// served as captured, ingests as the NetCDF file does: the variables on
/// MEPS's 949 × 1069 Lambert grid stored south-up, levels by value, the
/// run and valid time; the pl family's 13 pressure levels bottom first.
#[test]
fn thredds_metadata_of_meps_ingests_as_its_files_do() {
    let server = TestServer::new();
    let version = ObjectVersion {
        size: 166_700_000,
        last_modified: None,
        e_tag: None,
    };
    let run: DateTime<Utc> = "2026-10-23T12:00:00Z".parse().unwrap();
    for (family, coordinates) in [
        (
            "sfc",
            "time,forecast_reference_time,mean_sea_level,surface,height_above_msl,height0,\
             height1,height2,height3,height4,height5,height6,grib2_vLevel14,grib2_vLevel16,\
             local_vertical_level_192,isotherm_0C_level,isotherm_0C_level_from_above,\
             adiabatic_condensation_level,maximum_wind_level,tropopause,top_of_atmosphere,\
             projection_lambert,x,y",
        ),
        (
            "pl",
            "time,forecast_reference_time,pressure,projection_lambert,x,y",
        ),
    ] {
        let name = format!("meps_{family}_00_20261023T12Z");
        let key = format!("{DAP}/2026/10/23/12/member_00/{name}.nc");
        let read = |file: &str| std::fs::read(testdata(&format!("netcdf-opendap/{file}"))).unwrap();
        server.put(&format!("{key}.dds"), read(&format!("{name}.nc.dds")), run);
        server.put(&format!("{key}.das"), read(&format!("{name}.nc.das")), run);
        server.put(
            &format!("{key}.dods?{coordinates}"),
            read(&format!("{name}_coordinates.dods")),
            run,
        );
        let (host, _) = remote::host("thredds", &format!("{}/{DAP}/", server.url)).unwrap();
        let file = DapFile::open(
            host,
            &server.url,
            &key,
            version.clone(),
            crate::next_file_id(),
            Arc::default(),
        )
        .unwrap();
        let wanted = match family {
            "sfc" => [
                "air_temperature_2m",
                "x_wind_10m",
                "precipitation_amount_acc",
            ]
            .map(String::from)
            .to_vec(),
            _ => Vec::new(),
        };
        let inspected = inspect(
            &Reader::Dap(file),
            &if wanted.is_empty() {
                Wanted::All
            } else {
                Wanted::Listed(&wanted)
            },
            0,
        )
        .unwrap();
        assert_eq!(
            (inspected.run, &inspected.times[..]),
            (Some(run), &[run][..])
        );
        let FileGrid::Regular { transform, flipped } = &inspected.grid else {
            panic!("{:?}", inspected.grid);
        };
        assert!(*flipped, "MEPS stores its rows south to north");
        assert_eq!((transform.width, transform.height), (949, 1069));
        assert!(matches!(
            transform.crs,
            ds_core::geo::Crs::LambertConformalConic { .. }
        ));
        assert_eq!(transform.pixel_width, 2500.0);
        let variable = |name: &str| {
            &inspected
                .fields
                .iter()
                .find(|f| &*f.variable.name == name)
                .unwrap()
                .variable
        };
        match family {
            "sfc" => {
                let t2m = variable("air_temperature_2m");
                assert_eq!(t2m.units, "K");
                assert_eq!(
                    (t2m.levels[0].value, t2m.levels[0].units.as_str()),
                    (2.0, "m")
                );
                assert_eq!(variable("x_wind_10m").levels[0].value, 10.0);
                assert_eq!(variable("precipitation_amount_acc").units, "kg/m^2");
            }
            _ => {
                let names: Vec<&str> = inspected.fields.iter().map(|f| &*f.variable.name).collect();
                assert!(names.contains(&"air_temperature_pl"), "{names:?}");
                assert_eq!(
                    variable("geopotential_pl").pressure.as_deref(),
                    Some(
                        &[
                            1000.0, 925.0, 850.0, 800.0, 700.0, 500.0, 400.0, 300.0, 250.0, 200.0,
                            150.0, 100.0, 50.0
                        ][..]
                    )
                );
            }
        }
    }
}

/// The shipped OPeNDAP examples (#1264) are valid and read MET Norway's
/// own paths through its catalogs, checked without contacting the server.
#[test]
fn the_opendap_examples_name_met_norways_datasets() {
    for (id, family) in [
        ("meps-netcdf-opendap", "sfc"),
        ("meps-netcdf-pl-opendap", "pl"),
    ] {
        let path = format!(
            "{}/../../collections.d/{id}.toml.disabled",
            env!("CARGO_MANIFEST_DIR")
        );
        let table: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(table["id"].as_str(), Some(id));
        let config: NetcdfConfig = table["netcdf"].clone().try_into().unwrap();
        ds_core::config::validate_netcdf(id, &config).unwrap();
        assert_eq!(config.access(), ds_core::config::NetcdfAccess::Opendap);
        assert_eq!(config.leads.as_deref().map(<[u32]>::len), Some(67));
        let remote = NetcdfEngine::remote(id, &config, remote::Tuning::DEFAULT).unwrap();
        let finder = remote.finder.lock().unwrap();
        let crate::Finder::Catalog { prober, .. } = &*finder else {
            panic!("{id}: found through the catalogs");
        };
        assert_eq!(
            prober.catalog_url().as_str(),
            "https://thredds.met.no/thredds/catalog/meps25epsarchive/catalog.xml"
        );
        let key = finder.key("2026-10-23T12:00:00Z".parse().unwrap(), Some(5));
        assert_eq!(
            format!("{}/{key}", remote.origin),
            format!(
                "https://thredds.met.no/thredds/dodsC/meps25epsarchive/2026/10/23/12/\
                 member_00/meps_{family}_05_20261023T12Z.nc"
            )
        );
    }
}
