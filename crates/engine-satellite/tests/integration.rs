//! engine-satellite against cropped real GOES-19 scans (`testdata/goes19-abi`):
//! C13 clean-IR brightness temperature cut across the north-east limb, and
//! L2 cloud top temperature cut from the disk interior, both from the
//! 2026-09-25 19:00 full-disk scan. Values are the original packed integers.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ds_core::config::{SatelliteConfig, SatelliteProductConfig};
use ds_core::map_engine::{MapEngine, OutputCrs};
use engine_satellite::SatelliteEngine;
use netcdf_reader::NcFile;

const C13: &str = "OR_ABI-L2-CMIPF-M6C13_G19_s20262681900199_e20262681909519_c20262681909592.nc";
const ACHT: &str = "OR_ABI-L2-ACHTF-M6_G19_s20262681900199_e20262681909507_c20262681912337.nc";
/// The C13 scan ten minutes later, as the tests republish it.
const C13_LATER: &str =
    "OR_ABI-L2-CMIPF-M6C13_G19_s20262681910199_e20262681919519_c20262681919592.nc";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/goes19-abi")
        .join(name)
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// A directory holding `files` (fixture name, published name), nested like
/// the bucket so discovery must list recursively.
fn directory(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (from, to) in files {
        let nested = dir.path().join("ABI-L2/2026/268/19");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::copy(fixture(from), nested.join(to)).unwrap();
    }
    dir
}

fn config(dir: &Path) -> SatelliteConfig {
    SatelliteConfig {
        provider: "goes-r".into(),
        data_path: Some(dir.to_string_lossy().into_owned()),
        endpoint: None,
        bucket: None,
        time_window: None,
        poll_interval_secs: 60,
        products: vec![
            SatelliteProductConfig {
                parameter: "ir_10_3".into(),
                title: "IR 10.3 µm brightness temperature".into(),
                unit: "K".into(),
                product: "ABI-L2-CMIPF".into(),
                band: Some(13),
                variable: "CMI".into(),
            },
            SatelliteProductConfig {
                parameter: "cloud_top_temperature".into(),
                title: "Cloud top temperature".into(),
                unit: "K".into(),
                product: "ABI-L2-ACHTF".into(),
                band: None,
                variable: "TEMP".into(),
            },
        ],
    }
}

fn engine(files: &[(&str, &str)]) -> (SatelliteEngine, tempfile::TempDir) {
    let dir = directory(files);
    let engine = SatelliteEngine::new("goes19-fd", &config(dir.path())).unwrap();
    engine.poll_once();
    (engine, dir)
}

/// Physical value and geographic position of pixel `(row, col)` of a
/// fixture, decoded straight from the file: the independent reference the
/// engine's render is checked against.
fn reference_pixel(name: &str, variable: &str, row: usize, col: usize) -> (f64, f64, Option<f64>) {
    let nc = NcFile::open(fixture(name)).unwrap();
    let var = nc.variable(variable).unwrap();
    let attr =
        |v: &netcdf_reader::NcVariable, a: &str| v.attribute(a).and_then(|x| x.value.as_f64());
    let raw = nc.read_variable_as_f64(variable).unwrap();
    let raw = raw[[row, col]];
    let value = (raw != attr(var, "_FillValue").unwrap())
        .then(|| raw * attr(var, "scale_factor").unwrap() + attr(var, "add_offset").unwrap());
    let coord = |name: &str, i: usize| {
        let v = nc.variable(name).unwrap();
        let raw = nc.read_variable_as_f64(name).unwrap()[[i]];
        raw * attr(v, "scale_factor").unwrap() + attr(v, "add_offset").unwrap()
    };
    let h = attr(
        nc.variable("goes_imager_projection").unwrap(),
        "perspective_point_height",
    )
    .unwrap();
    let crs = ds_core::geo::Crs::Geostationary {
        lon0: (-75.0_f64).to_radians(),
        height: h,
        semi_major: ds_core::geo::WGS84_A,
        semi_minor: 6356752.31414,
        sweep: ds_core::geo::SweepAxis::X,
    };
    let (lon, lat) = crs
        .inverse(coord("x", col) * h, coord("y", row) * h)
        .unwrap();
    (lon, lat, value)
}

#[test]
fn discovers_products_with_their_own_time_axes() {
    let (engine, _dir) = engine(&[(C13, C13), (ACHT, ACHT), (C13, C13_LATER)]);
    let info = engine.raster_info();
    assert_eq!(info.native_crs, "geos");
    assert_eq!(
        info.parameters
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["ir_10_3", "cloud_top_temperature"]
    );
    // The collection advertises the union; each product its own scans,
    // keyed on the scan's minute.
    let (t0, t1) = (at("2026-09-25T19:00:00Z"), at("2026-09-25T19:10:00Z"));
    assert_eq!(info.times, [t0, t1]);
    assert_eq!(&*engine.parameter_times("ir_10_3").unwrap(), [t0, t1]);
    assert_eq!(
        &*engine.parameter_times("cloud_top_temperature").unwrap(),
        [t0]
    );
    assert!(engine.parameter_times("nope").is_none());
    // Cloud top temperature lacks 19:10: it snaps to its own 19:00 scan.
    assert_eq!(
        engine.resolve_parameter_time(Some("cloud_top_temperature"), Some(t1), None),
        Some(t0)
    );
    assert_eq!(
        engine.resolve_parameter_time(Some("ir_10_3"), None, None),
        Some(t1)
    );
    // A nominal time finds its scan (which started 20 s later).
    assert_eq!(
        engine.resolve_parameter_time(Some("ir_10_3"), Some(at("2026-09-25T19:05:00Z")), None),
        Some(t0)
    );
    let extent = info.spatial_extent.unwrap();
    assert!(extent.iter().all(|v| v.is_finite()), "{extent:?}");
    // Both fixture crops are 320 x 240, so the collection has one grid.
    assert_eq!(info.grid_size, Some([320, 240]));
    assert_eq!(engine.status().0, 2);
}

/// A 3×3 render centred on one pixel returns that pixel's value, decoded
/// independently from the file — for the signed IR field and the unsigned
/// cloud product.
#[test]
fn renders_the_file_values_at_full_resolution() {
    let (engine, _dir) = engine(&[(C13, C13), (ACHT, ACHT)]);
    for (name, variable, parameter, row, col) in [
        (C13, "CMI", "ir_10_3", 200, 40),
        (ACHT, "TEMP", "cloud_top_temperature", 120, 160),
    ] {
        let (lon, lat, expected) = reference_pixel(name, variable, row, col);
        let d = 0.0005;
        let tile = engine
            .get_raster_tile(
                [lon - d, lat - d, lon + d, lat + d],
                3,
                3,
                None,
                &OutputCrs::Wgs84,
                Some(parameter),
                None,
                None,
            )
            .unwrap();
        let value = tile.values.value_at(4);
        match expected {
            Some(expected) => {
                let value = value.unwrap_or_else(|| panic!("{parameter}: no value"));
                assert!(
                    (value - expected).abs() < 1e-6,
                    "{parameter}: {value} vs {expected}"
                );
                assert!((180.0..340.0).contains(&value), "{parameter}: {value} K");
            }
            None => assert_eq!(value, None, "{parameter}"),
        }
    }
}

/// The IR crop reaches past the limb: pixels in space are nodata, pixels on
/// the disk are brightness temperatures. A request far wider than the crop
/// samples the overview, which must agree.
#[test]
fn space_is_nodata_and_the_overview_agrees() {
    // Only the IR scan, so the collection extent is the IR crop's.
    let (engine, _dir) = engine(&[(C13, C13)]);
    let [w, s, e, n] = engine.raster_info().spatial_extent.unwrap();
    let render = |bbox: [f64; 4], size: u32| {
        engine
            .get_raster_tile(
                bbox,
                size,
                size,
                None,
                &OutputCrs::Wgs84,
                Some("ir_10_3"),
                None,
                None,
            )
            .unwrap()
    };
    let full = render([w, s, e, n], 64);
    let count = |tile: &ds_core::map_engine::RasterTile| {
        (0..tile.values.len())
            .filter_map(|i| tile.values.value_at(i))
            .inspect(|v| assert!((180.0..340.0).contains(v), "{v} K"))
            .count()
    };
    let on_disk = count(&full);
    assert!(
        on_disk > 0 && on_disk < 64 * 64,
        "{on_disk} of 4096 pixels on the disk"
    );
    // Four times the extent at a quarter of the density: the overview.
    let (dx, dy) = (e - w, n - s);
    let wide = render([w - 1.5 * dx, s - 1.5 * dy, e + 1.5 * dx, n + 1.5 * dy], 16);
    assert!(count(&wide) > 0, "the overview renders the crop too");
}

/// Web Mercator and a projected output CRS render through the same grid.
#[test]
fn renders_in_web_mercator() {
    let (engine, _dir) = engine(&[(C13, C13)]);
    let [w, s, e, n] = engine.raster_info().spatial_extent.unwrap();
    let tile = engine
        .get_raster_tile(
            [w, s, e, n],
            32,
            32,
            None,
            &OutputCrs::WebMercator,
            None,
            None,
            None,
        )
        .unwrap();
    assert!((0..tile.values.len()).any(|i| tile.values.value_at(i).is_some()));
}

#[test]
fn unknown_parameter_is_an_error_and_empty_catalog_renders_nothing() {
    let (engine, _dir) = engine(&[(C13, C13)]);
    let render = |parameter| {
        engine.get_raster_tile(
            [-80.0, 30.0, -70.0, 40.0],
            4,
            4,
            None,
            &OutputCrs::Wgs84,
            Some(parameter),
            None,
            None,
        )
    };
    assert!(render("nope").is_err());
    // No cloud top temperature scan was published.
    let tile = render("cloud_top_temperature").unwrap();
    assert!((0..16).all(|i| tile.values.value_at(i).is_none()));
    assert_eq!(engine.status().0, 1);
}

/// The shipped example collection parses, validates and builds an engine
/// (construction does no I/O; the first poll would download).
#[test]
fn example_collection_configs_are_valid() {
    for file in ["goes19-fd.toml", "goes18-fd.toml"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../collections.d")
            .join(file);
        let collection: ds_core::config::CollectionConfig =
            toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(collection.engine_type, "satellite");
        let satellite = collection.satellite.as_ref().unwrap();
        ds_core::config::validate_satellite(&collection.id, satellite).unwrap();
        let engine = SatelliteEngine::new(&collection.id, satellite).unwrap();
        let info = engine.raster_info();
        assert_eq!(info.parameters.len(), 2, "{file}");
        assert!(
            info.times.is_empty(),
            "{file}: nothing is fetched before the first poll"
        );
    }
}

// --- EDR -----------------------------------------------------------------

use ds_core::edr_engine::EdrEngine;
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        other => panic!("expected one coverage, got {other:?}"),
    }
}

/// A position is the time series of the pixel under it, on the union of
/// the products' scans; a product without a scan there — or whose crop does
/// not reach the point — reads null.
#[test]
fn edr_position_is_a_time_series_on_the_union_axis() {
    let (engine, _dir) = engine(&[(C13, C13), (ACHT, ACHT), (C13, C13_LATER)]);
    let (lon, lat, expected) = reference_pixel(C13, "CMI", 200, 40);
    let expected = expected.expect("an on-disk reference pixel");
    let point = format!("POINT({lon} {lat})");

    let result = single(
        engine
            .query_position(&point, None, None, None, None)
            .unwrap(),
    );
    let DomainDescription::PointSeries { t, .. } = &result.domain else {
        panic!("expected a point series");
    };
    assert_eq!(t, &[at("2026-09-25T19:00:00Z"), at("2026-09-25T19:10:00Z")]);
    let ir = &result.ranges["ir_10_3"];
    assert_eq!(ir.shape, [2]);
    for value in &ir.values {
        assert!(
            (value.unwrap() - expected).abs() < 1e-6,
            "{value:?} vs {expected}"
        );
    }
    // The cloud crop lies elsewhere on the disk: null, not an error.
    assert!(result.ranges["cloud_top_temperature"]
        .values
        .iter()
        .all(Option::is_none));
    assert_eq!(result.parameters["ir_10_3"].unit, "K");

    // An instant snaps to its scan, per product.
    let instant = at("2026-09-25T19:05:00Z");
    let result = single(
        engine
            .query_position(
                &point,
                Some((instant, instant)),
                Some(&["ir_10_3".to_string()]),
                None,
                None,
            )
            .unwrap(),
    );
    let DomainDescription::PointSeries { t, .. } = &result.domain else {
        panic!("expected a point series");
    };
    assert_eq!(t, &[at("2026-09-25T19:00:00Z")]);
    assert_eq!(result.ranges.len(), 1);

    // Behind the Earth, and an unknown parameter.
    assert!(matches!(
        engine.query_position("POINT(100 0)", None, None, None, None),
        Err(ds_core::error::DataServerError::LocationNotFound(_))
    ));
    assert!(engine
        .query_position(&point, None, Some(&["nope".to_string()]), None, None)
        .is_err());
}

/// An area is a CRS84 grid at the nadir resolution over the polygon,
/// masked to it; with two scans it gains a time axis. A radius delegates to
/// the area query.
#[test]
fn edr_area_and_radius_sample_the_scans() {
    let (engine, _dir) = engine(&[(C13, C13), (C13, C13_LATER)]);
    let (lon, lat, expected) = reference_pixel(C13, "CMI", 120, 60);
    let expected = expected.expect("an on-disk reference pixel");
    let d = 0.25;
    let polygon = format!(
        "POLYGON(({w} {s},{e} {s},{e} {n},{w} {n},{w} {s}))",
        w = lon - d,
        e = lon + d,
        s = lat - d,
        n = lat + d
    );
    let result = single(
        engine
            .query_area(&polygon, None, Some(&["ir_10_3".to_string()]), None, None)
            .unwrap(),
    );
    let DomainDescription::Grid { x, y, t, .. } = &result.domain else {
        panic!("expected a grid");
    };
    assert_eq!(t.as_ref().map(Vec::len), Some(2));
    // ~2 km cells over half a degree.
    assert!(x.len() >= 20 && y.len() >= 20, "{} × {}", x.len(), y.len());
    let ir = &result.ranges["ir_10_3"];
    assert_eq!(ir.shape, [2, y.len(), x.len()]);
    let values: Vec<f64> = ir.values.iter().flatten().copied().collect();
    assert!(values.len() > x.len() * y.len(), "both scans carry values");
    assert!(values.iter().all(|v| (180.0..340.0).contains(v)));
    // The cell nearest the reference pixel reads a value near it (the grid
    // samples the nearest source pixel of each cell centre).
    let (ix, iy) = (nearest(x, lon), nearest(y, lat));
    let centre = ir.values[iy * x.len() + ix].unwrap();
    assert!((centre - expected).abs() < 15.0, "{centre} vs {expected}");

    let radius = single(
        engine
            .query_radius(
                &format!("POINT({lon} {lat})"),
                20_000.0,
                None,
                None,
                None,
                None,
            )
            .unwrap(),
    );
    assert!(radius.ranges["ir_10_3"].values.iter().any(Option::is_some));
    // Outside the disk entirely.
    assert!(engine
        .query_area(
            "POLYGON((100 0,110 0,110 10,100 10,100 0))",
            None,
            None,
            None,
            None
        )
        .is_err());
}

fn nearest(axis: &[f64], value: f64) -> usize {
    (0..axis.len())
        .min_by(|&a, &b| (axis[a] - value).abs().total_cmp(&(axis[b] - value).abs()))
        .unwrap()
}

#[test]
fn edr_metadata_advertises_per_parameter_times() {
    let (engine, _dir) = engine(&[(C13, C13), (ACHT, ACHT), (C13, C13_LATER)]);
    assert_eq!(
        engine.get_parameter_available_times("cloud_top_temperature"),
        Some(vec![at("2026-09-25T19:00:00Z")])
    );
    assert_eq!(
        engine
            .get_parameter_available_times("ir_10_3")
            .map(|t| t.len()),
        Some(2)
    );
    assert_eq!(
        engine.get_temporal_extent(),
        Some((at("2026-09-25T19:00:00Z"), at("2026-09-25T19:10:00Z")))
    );
    assert_eq!(
        engine.supported_query_types(),
        ["position", "area", "radius"]
    );
    assert_eq!(
        engine.get_parameter_descriptions()["ir_10_3"].label,
        "IR 10.3 µm brightness temperature"
    );
}
