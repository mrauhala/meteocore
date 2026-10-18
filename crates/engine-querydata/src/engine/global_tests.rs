//! A global lat/lon grid (#1188). Under a projected output CRS its map is
//! the shared sampler's, periodic in longitude (`SourceGrid`, #323), so the
//! EPSG:3035 inverse's cut along 170°W and the pole stay registered and
//! the map meets its budget instead of capping; `sample_grid_bilinear`
//! wraps the columns, so a pixel or point past the last column reads the
//! first. No fixture is
//! global: the tests write a 0.25° grid as a QueryData file, one
//! parameter at one step, holding [`field`].

use std::cell::Cell;

use super::*;
use ds_core::geo::{projected_output_crs, wgs84_envelope};
use ds_core::resample::ProjectionGrid;

/// The grid: 0.25° from 180°W and the South Pole, cell centres on the
/// poles.
const NX: u32 = 1440;
const NY: u32 = 721;
const PARAM: &str = "2 Metre Temperature (2t)";

/// A field smooth on the sphere (linear in its 3-D coordinates), so
/// continuous across the antimeridian and at the poles, steep enough that
/// a pixel sampled a few kilometres off shows: about 0.2 a kilometre.
fn field(lon: f64, lat: f64) -> f64 {
    let (lon, lat) = (lon.to_radians(), lat.to_radians());
    1000.0 * (lat.sin() + lat.cos() * (0.6 * lon.cos() + 0.8 * lon.sin()))
}

/// The engine over a global grid written to a fresh directory, kept alive
/// with it.
fn global_engine() -> (QueryDataEngine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    // The magic, the little-endian mark and the magic again, then the text
    // header: the Kenya fixture's, cut to one parameter, one level and one
    // step, with a global lat/lon area. Locations run from the south-west
    // corner, a row of NX per latitude.
    let mut sqd = vec![0x40, 0x24, 0xb0, 0xa3, 0x51];
    sqd.extend_from_slice(b"INFO");
    sqd.extend_from_slice(&[0x40, 0x24, 0xb0, 0xa3, b' ']);
    let pool = NX as usize * NY as usize * 4;
    sqd.extend_from_slice(
        format!(
            "VER 7\n21 NFmiQueryInfo\n0 0 0 0 \n0\n0\n34 NFmiParamDescriptor\n\
             0 0 0 0 \n1\n4\n24 {PARAM}\n32700 32700 1 32700 32700 4 %.1f\n240\n\
             13 ECMWF Surface\n1 1 1 1 1 0 0 0 \n0\n1 \n4 0 0 0\n0 NFmiLocationBag\n\
             0 NFmiArea\n41 NFmiGrid\n10 NFmiLatLonArea\n0 0\n1 1\n-180 -90\n179.75 90\n\
             0 0\n0 0\n1 1\n1\n33\n{NX} {NY}\n6\n0\n0\n1\n1\n1 0\n0\n0\n\
             1 NFmiTimeDescriptor\n1\n2026 4 4 6 0 0\n2026 4 4 6 0 0\n2026 4 4 6 0 0\n\
             0 0 0 0 0 0 0\n0 0 0 1 0 0 0\n1 \n6\n1\n{pool}\n"
        )
        .as_bytes(),
    );
    for row in 0..NY {
        let lat = -90.0 + 0.25 * f64::from(row);
        for col in 0..NX {
            let lon = -180.0 + 0.25 * f64::from(col);
            sqd.extend_from_slice(&(field(lon, lat) as f32).to_le_bytes());
        }
    }
    std::fs::write(dir.path().join("202604040600_global_surface.sqd"), sqd).unwrap();
    let engine = QueryDataEngine::new(dir.path(), "global", None, 30, 1).unwrap();
    (engine, dir)
}

/// The fractional position of output pixel `(ox, oy)`'s centre.
fn pixel_centre(ox: u32, oy: u32, (width, height): (u32, u32)) -> (f64, f64) {
    (
        (f64::from(ox) + 0.5) / f64::from(width),
        (f64::from(oy) + 0.5) / f64::from(height),
    )
}

/// #1188 on the #323 tile: the EPSG:3035 WMS meta-tile (level 10, 4 km
/// texels) holding the North Pole and the inverse's longitude cut along
/// 170°W. A map without a column period capped at 256 × 256 cells with an
/// estimated error of 900 px: it interpolated across the cut through the
/// whole grid, half a turn away, and left the pixels east of 180° off the
/// grid. The periodic map meets its budget, the engine samples it as it is
/// built, and every pixel is on the grid, where sampling per pixel puts it.
///
/// It costs as much: 1 485 934 projections, the 256-cell density. Beside
/// the pole the bilinear column map converges only linearly, its error
/// about an eighth of a cell a ring out, so a uniform grid meets 0.2 px
/// only at 1 px cells; the pole's own cell gets a sub-grid, its
/// neighbours do not. That is `ProjectionGrid`'s, shared by every global
/// lat/lon source, and the bound below holds it.
#[test]
fn a_global_grid_is_registered_over_the_pole() {
    let (engine, _dir) = global_engine();
    let data = engine.latest_data().unwrap();
    let gt = data.grid.geo_transform();
    let size = (256, 256);
    let laea = projected_output_crs("EPSG:3035").unwrap();
    let tile = [4_096_000.0, 7_168_000.0, 5_120_000.0, 8_192_000.0];
    let bbox = wgs84_envelope(&laea, tile).unwrap();
    let crs = OutputCrs::Projected {
        crs: laea,
        bbox: tile,
    };

    let period = data.grid.source().global_period();
    assert_eq!(period, Some(f64::from(NX)));
    let map = data
        .grid
        .source()
        .output_map_quiet(bbox, size.0, size.1, &crs);
    assert_eq!(map.capped_error(), None);
    // The engine's map is the periodic grid, projection for projection.
    let projections = Cell::new(0);
    let periodic = ProjectionGrid::build_2d_periodic(
        size.0,
        size.1,
        NX,
        NY,
        period,
        |fx, fy| crs.project_node(bbox, fx, fy),
        |lon, lat| {
            projections.set(projections.get() + 1);
            world_to_grid_px(gt, lon, lat)
        },
    );
    assert!(
        projections.get() <= 1_500_000,
        "{} projections",
        projections.get()
    );
    for oy in 0..size.1 {
        for ox in 0..size.0 {
            let (col, row) = periodic.sample(ox, oy);
            let located = (col.is_finite() && (-1.0..f64::from(NY)).contains(&row))
                .then(|| (col.rem_euclid(f64::from(NX)), row));
            assert_eq!(map.position(ox, oy), located, "({ox}, {oy})");
        }
    }

    let rendered = engine
        .get_raster_tile(bbox, size.0, size.1, None, &crs, None, None, None)
        .unwrap();
    let (mut worst, mut worst_field) = (0.0_f64, 0.0_f64);
    for oy in 0..size.1 {
        for ox in 0..size.0 {
            let (fx, fy) = pixel_centre(ox, oy, size);
            let (lon, lat) = crs.project_node(bbox, fx, fy);
            let (col, row) = world_to_grid_px(gt, lon, lat);
            let exact = sample_grid_bilinear(&data, col, row, 0, 0, 0)
                .unwrap_or_else(|| panic!("({ox}, {oy}) at ({lon}, {lat}) is off the grid"));
            let i = (oy * size.0 + ox) as usize;
            let got = rendered
                .values
                .value_at(i)
                .unwrap_or_else(|| panic!("({ox}, {oy}) at ({lon}, {lat}) is empty"));
            worst = worst.max((got - exact).abs());
            worst_field = worst_field.max((got - field(lon, lat)).abs());
        }
    }
    // Within about 2.5 km, under a 4 km texel: the map holds 0.2 px but
    // for the pole's own sub-cell, interpolated across the singularity
    // (0.40 at most here).
    assert!(worst < 0.6, "{worst} from sampling per pixel");
    assert!(worst_field < 0.6, "{worst_field} from the field");
}

/// The seam box 170,10,-170,20, in each form an engine can be handed it,
/// on geographic and Web Mercator output, which sample the lat/lon grid
/// per pixel: every pixel holds the field, the ones past the last column,
/// at 179.75°E..180°, interpolated with the first column at 180°W rather
/// than repeating the last.
#[test]
fn the_seam_box_is_continuous_on_a_global_grid() {
    let (engine, _dir) = global_engine();
    let size = (64, 32);
    for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
        for bbox in [
            [170.0, 10.0, -170.0, 20.0],
            [170.0, 10.0, 190.0, 20.0],
            [-190.0, 10.0, -170.0, 20.0],
        ] {
            let rendered = engine
                .get_raster_tile(bbox, size.0, size.1, None, &crs, None, None, None)
                .unwrap();
            for oy in 0..size.1 {
                for ox in 0..size.0 {
                    let (fx, fy) = pixel_centre(ox, oy, size);
                    let (lon, lat) = crs.project_node([170.0, 10.0, 190.0, 20.0], fx, fy);
                    let i = (oy * size.0 + ox) as usize;
                    let got = rendered.values.value_at(i).unwrap_or_else(|| {
                        panic!("{crs:?} {bbox:?}: ({ox}, {oy}) at {lon}° empty")
                    });
                    let want = field(lon, lat);
                    assert!(
                        (got - want).abs() < 0.05,
                        "{crs:?} {bbox:?}: ({ox}, {oy}) at ({lon}, {lat}) is {got}, \
                         the field {want}"
                    );
                }
            }
        }
    }
}

/// An EDR position between the last column and the antimeridian samples
/// as the map does: interpolated with the first column, not the nearest.
#[test]
fn a_position_past_the_last_column_interpolates_with_the_first() {
    let (engine, _dir) = global_engine();
    for (lon, lat) in [(179.9, 15.0), (-179.9, 15.0), (179.8, -40.0)] {
        let response = engine
            .query_position(&format!("POINT({lon} {lat})"), None, None, None, None)
            .unwrap();
        let CoverageResponse::Single(result) = response else {
            panic!("not a single coverage");
        };
        let got = result.ranges[PARAM].values[0].unwrap();
        let want = field(lon, lat);
        assert!(
            (got - want).abs() < 0.05,
            "({lon}, {lat}) is {got}, the field {want}"
        );
    }
}
