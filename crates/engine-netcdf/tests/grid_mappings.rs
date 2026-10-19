//! engine-netcdf over grids on the CF `polar_stereographic`,
//! `lambert_azimuthal_equal_area` and `transverse_mercator` mappings
//! (#1167): testdata/netcdf-projections, whose two fields are each cell's
//! own projected coordinates. A read at a longitude/latitude returns the
//! projected point the server puts it at, pinned to PROJ's `cs2cs` (PROJ
//! 9.8.1), forward `-f %.3f` and inverse `-I -f %.9f`, with the commands
//! beside each fixture's points.

use std::path::PathBuf;

use ds_core::config::NetcdfConfig;
use ds_core::edr_engine::EdrEngine;
use ds_core::map_engine::{MapEngine, OutputCrs};
use ds_core::model::{CoverageResponse, DomainDescription, QueryResult};
use engine_netcdf::NetcdfEngine;

fn engine(dir: &str) -> NetcdfEngine {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/netcdf-projections")
        .join(dir);
    let config: NetcdfConfig =
        toml::from_str(&format!("data_path = {:?}", path.display().to_string())).unwrap();
    let engine = NetcdfEngine::new(dir, &config).unwrap();
    engine.poll_once();
    engine
}

fn single(response: CoverageResponse) -> QueryResult {
    match response {
        CoverageResponse::Single(result) => result,
        CoverageResponse::Collection(_) => panic!("expected a single coverage"),
    }
}

fn fields() -> Vec<String> {
    vec!["easting".to_string(), "northing".to_string()]
}

/// `(easting, northing)` an EDR position query reads at `(lon, lat)`.
fn edr_xy(engine: &NetcdfEngine, lon: f64, lat: f64) -> (f64, f64) {
    let result = single(
        engine
            .query_position(
                &format!("POINT({lon} {lat})"),
                None,
                Some(&fields()),
                None,
                None,
            )
            .unwrap(),
    );
    let value = |field: &str| result.ranges[field].values[0].unwrap();
    (value("easting"), value("northing"))
}

/// `(easting, northing)` a one-pixel map centred on `(lon, lat)` draws.
fn map_xy(engine: &NetcdfEngine, lon: f64, lat: f64) -> (f64, f64) {
    let d = 1e-4;
    let drawn = |field: &str| {
        let tile = engine
            .get_raster_tile(
                [lon - d, lat - d, lon + d, lat + d],
                1,
                1,
                None,
                &OutputCrs::Wgs84,
                Some(field),
                None,
                None,
            )
            .unwrap();
        tile.values.value_at(0).unwrap()
    };
    (drawn("easting"), drawn("northing"))
}

/// Asserts each `(lon, lat)` reads and draws as PROJ's `(x, y)`, within the
/// half metre each of the stored and the drawn `f32` rounds by.
fn assert_lands_where_proj_says(engine: &NetcdfEngine, cases: &[(f64, f64, f64, f64)]) {
    for &(lon, lat, x_proj, y_proj) in cases {
        for (how, (x, y)) in [
            ("EDR", edr_xy(engine, lon, lat)),
            ("map", map_xy(engine, lon, lat)),
        ] {
            assert!(
                (x - x_proj).abs() < 1.0 && (y - y_proj).abs() < 1.0,
                "{how} at ({lon}, {lat}) reads ({x}, {y}), PROJ ({x_proj}, {y_proj})"
            );
        }
    }
}

/// EPSG:3413's parameters: `cs2cs +proj=longlat +datum=WGS84 +to
/// +proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 +x_0=0 +y_0=0 +datum=WGS84
/// +units=m`; the last two points are the cell centres (750000, 750000)
/// and (-500000, -250000) through `-I`.
#[test]
fn polar_stereographic_reads_where_proj_puts_it() {
    let engine = engine("polar-stereographic");
    assert_eq!(engine.raster_info().native_crs, "projected");
    assert_lands_where_proj_says(
        &engine,
        &[
            (-45.0, 90.0, 0.0, 0.0),
            (0.0, 85.0, 383228.329, -383228.329),
            (100.0, 82.0, 497848.471, 711001.302),
            (-160.0, 84.0, -589584.511, 274927.773),
            (135.0, 82.0, 0.0, 867972.322),
            (10.0, 81.5, 755591.115, -529070.595),
            (90.0, 80.231521757, 750000.0, 750000.0),
            (-108.434948823, 84.842900456, -500000.0, -250000.0),
        ],
    );
}

/// The grid holds the North Pole, which its edges circle and stay short of
/// (they reach 79.64°N, `cs2cs -I` of (0, 1125000)): the extent runs to the
/// pole at every longitude, and the pole reads as the origin from any
/// meridian. The extent's south is the corners', 75.39°N.
#[test]
fn a_grid_around_the_pole_reaches_it() {
    let engine = engine("polar-stereographic");
    let [w, s, e, n] = engine.raster_info().spatial_extent.unwrap();
    assert_eq!((w, e, n), (-180.0, 180.0, 90.0));
    assert!((s - 75.3896).abs() < 1e-3, "{s}");
    for lon in [-170.0, -45.0, 0.0, 120.0] {
        let (x, y) = edr_xy(&engine, lon, 90.0);
        assert!(x.abs() < 1.0 && y.abs() < 1.0, "({lon}, 90): ({x}, {y})");
    }
    // A cap north of every edge is on the grid, not a 404.
    let result = single(
        engine
            .query_area("-180,88,180,90", None, Some(&fields()), None, None)
            .unwrap(),
    );
    let DomainDescription::Grid { .. } = &result.domain else {
        panic!("not a Grid: {:?}", result.domain);
    };
    let northing = &result.ranges["northing"].values;
    assert!(!northing.is_empty());
    for value in northing {
        // Within 2° of the pole: within 230 km of it.
        let value = value.expect("a cap around the pole is on the grid");
        assert!(value.abs() < 230_000.0, "{value}");
    }
}

/// EPSG:3035's parameters on GRS80: `cs2cs +proj=longlat +ellps=GRS80 +to
/// +proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80
/// +units=m`; the last point is the cell centre (5250000, 2500000) through
/// `-I`. The rows are stored south to north.
#[test]
fn lambert_azimuthal_equal_area_reads_where_proj_puts_it() {
    let engine = engine("laea");
    assert_lands_where_proj_says(
        &engine,
        &[
            (10.0, 52.0, 4321000.0, 3210000.0),
            (2.35, 48.85, 3760536.823, 2888771.021),
            (21.0, 52.2, 5069785.387, 3288963.280),
            (16.37, 48.21, 4793982.420, 2808914.514),
            (25.0, 60.0, 5152827.254, 4188383.611),
            (21.807138024, 44.948540663, 5250000.0, 2500000.0),
        ],
    );
}

/// UTM zone 33N: `cs2cs +proj=longlat +datum=WGS84 +to +proj=tmerc
/// +lat_0=0 +lon_0=15 +k=0.9996 +x_0=500000 +y_0=0 +datum=WGS84 +units=m`;
/// the last point is the cell centre (700000, 6450000) through `-I`.
#[test]
fn transverse_mercator_reads_where_proj_puts_it() {
    let engine = engine("transverse-mercator");
    assert_lands_where_proj_says(
        &engine,
        &[
            (15.0, 60.0, 500000.0, 6651411.190),
            (18.0, 60.0, 667294.821, 6655205.484),
            (12.0, 58.5, 325187.240, 6488282.468),
            (17.5, 62.5, 628768.839, 6932375.116),
            (11.0, 62.0, 290586.062, 6880638.214),
            (18.398284982, 58.145953698, 700000.0, 6450000.0),
        ],
    );
}
