//! GRIB's map sampler ([`DecodedGrid::resample_f32`]) pinned to the shared
//! one, `ds_core::resample::SourceGrid` (#1067), where the two agree: a
//! global grid on CRS84 and projected output, the antimeridian seam box in
//! both forms, and the EPSG:3035 Arctic view of #323. The committed field
//! through both gives the same pixels, value for value, so a rule changed
//! in one and not the other fails here (#1172).
//!
//! The agreement is per field, not by construction: the two compute the
//! same interpolation through different float expressions (a column from
//! the first node here, from the first cell's corner there). On a 1°
//! Greenwich-first grid, CRS84 views west of 0° differ by one f32 ulp in
//! a few pixels in 1000.
//!
//! They differ elsewhere, which is why GRIB keeps its own sampler:
//! EPSG:3857 maps each output pixel exactly here and through a projection
//! grid there; a regional grid's edges and a NaN neighbour follow other
//! rules; and the display conversion runs here before narrowing.

use super::*;
use ds_core::geo::{projected_envelope, projected_output_crs, wgs84_envelope, Crs, GeoTransform};
use ds_core::map_engine::RasterValues;
use ds_core::resample::SourceGrid;

/// The committed ECMWF `q` @ 150 hPa message: a global 0.25° grid,
/// 1440 × 721 nodes, whose first column is 180°.
fn ecmwf_q() -> DecodedGrid {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/grib-local/sample-message.grib2"
    );
    crate::reader::decode_message(&std::fs::read(path).unwrap(), "q").unwrap()
}

/// The grid as the shared sampler sees it. GRIB nodes are cell centres, so
/// the transform's origin, the first cell's outer corner, lies half a cell
/// west and north of the first node.
fn source_grid(grid: &DecodedGrid) -> SourceGrid {
    SourceGrid::new(GeoTransform {
        origin_x: grid.lon_first - grid.lon_inc / 2.0,
        origin_y: grid.lat_first - grid.lat_inc / 2.0,
        pixel_width: grid.lon_inc,
        pixel_height: -grid.lat_inc,
        width: grid.ni as u32,
        height: grid.nj as u32,
        crs: Crs::Wgs84,
    })
}

/// Whether a CRS84 view's pixel centres cross the grid's storage seam:
/// the column after its last one is its first again. The view is narrower
/// than a turn, so its wrapped columns fall back exactly where it crosses.
fn crosses_storage_seam(grid: &DecodedGrid, [west, _, east, _]: [f64; 4], width: u32) -> bool {
    let period = grid.wrap_modulus().expect("a global grid");
    let column = |ox: u32| {
        let lon = west + (f64::from(ox) + 0.5) / f64::from(width) * (east - west);
        ((lon - grid.lon_first) / grid.lon_inc).rem_euclid(period)
    };
    (1..width).any(|ox| column(ox) < column(ox - 1))
}

/// The projected request for a WGS84 box: its EPSG metres and the WGS84
/// envelope the API layers pass alongside.
fn projected(epsg: &str, wgs84: [f64; 4]) -> ([f64; 4], OutputCrs) {
    let crs = projected_output_crs(epsg).unwrap();
    let bbox = projected_envelope(&crs, wgs84);
    let read = wgs84_envelope(&crs, bbox).unwrap();
    (read, OutputCrs::Projected { crs, bbox })
}

/// The EPSG:3035 WMS meta-tile of #323 (level 10, 4 km texels): the North
/// Pole at pixel (56.25, 205.57) and the 3035 inverse's longitude cut along
/// 170°W.
fn arctic() -> ([f64; 4], OutputCrs) {
    let crs = projected_output_crs("EPSG:3035").unwrap();
    let bbox = [4_096_000.0, 7_168_000.0, 5_120_000.0, 8_192_000.0];
    let read = wgs84_envelope(&crs, bbox).unwrap();
    (read, OutputCrs::Projected { crs, bbox })
}

/// Both samplers over every view, value for value; on a global grid without
/// NaN every pixel holds data.
fn assert_same_pixels(name: &str, grid: &DecodedGrid, views: &[([f64; 4], u32, u32, OutputCrs)]) {
    let shared = source_grid(grid);
    for (bbox, width, height, crs) in views {
        let ours = grid.resample_f32(*bbox, *width, *height, crs, |v| v);
        let tile = shared
            .render_f32(&grid.values, None, *bbox, *width, *height, crs)
            .unwrap();
        let RasterValues::F32 { data: theirs, .. } = tile.values else {
            panic!("SourceGrid renders F32");
        };
        assert_eq!(ours.len(), theirs.len());
        for (i, (a, b)) in ours.iter().zip(&theirs).enumerate() {
            assert!(
                !a.is_nan() && a.to_bits() == b.to_bits(),
                "{name} {crs:?} {bbox:?} pixel {i}: GRIB {a}, SourceGrid {b}"
            );
        }
    }
}

#[test]
fn shared_sampler_renders_a_global_grid_as_grib_does() {
    let seam_unwrapped = [170.0, 10.0, 190.0, 20.0];
    let seam_west = [-190.0, 10.0, -170.0, 20.0];
    let greenwich = [-20.0, 35.0, 40.0, 72.0];
    let (arctic_read, arctic_crs) = arctic();
    let mut views = vec![
        ([-180.0, -90.0, 180.0, 90.0], 512, 256, OutputCrs::Wgs84),
        ([19.0, 59.0, 32.0, 71.0], 256, 256, OutputCrs::Wgs84),
        (greenwich, 300, 200, OutputCrs::Wgs84),
        // The seam box 170,10,-170,20 as OGC API Maps passes it (#828) and
        // as a WMS client wrapping the world west sends it.
        (seam_unwrapped, 256, 128, OutputCrs::Wgs84),
        (seam_west, 256, 128, OutputCrs::Wgs84),
        // World copies: a turn east and a turn west.
        ([360.0, 55.0, 390.0, 70.0], 256, 256, OutputCrs::Wgs84),
        ([-355.0, 57.0, -335.0, 68.0], 300, 200, OutputCrs::Wgs84),
        (arctic_read, 256, 256, arctic_crs.clone()),
    ];
    for (epsg, wgs84) in [
        ("EPSG:3035", [-10.0, 35.0, 40.0, 70.0]),
        ("EPSG:3067", [19.0, 59.0, 32.0, 71.0]),
    ] {
        let (read, crs) = projected(epsg, wgs84);
        views.push((read, 256, 256, crs));
    }

    let q = ecmwf_q();
    assert_eq!(
        (q.ni, q.nj, q.lon_first, q.lon_inc),
        (1440, 721, 180.0, 0.25)
    );
    assert!(q.values.iter().all(|v| !v.is_nan()), "no NaN neighbours");
    assert_same_pixels("ECMWF q", &q, &views);

    // What the cases cover. The grid's storage seam lies at 180°: both
    // forms of the seam box cross it, so their columns wrap from the
    // grid's last to its first; the Greenwich view does not.
    for seam in [seam_unwrapped, seam_west] {
        assert!(crosses_storage_seam(&q, seam, 256), "{seam:?}");
    }
    assert!(!crosses_storage_seam(&q, greenwich, 300));
    // The Arctic tile holds the pole and the 170°W cut, where neighbouring
    // pixels' longitudes jump a turn.
    let at = |ox: u32, oy: u32| {
        let (fx, fy) = ((f64::from(ox) + 0.5) / 256.0, (f64::from(oy) + 0.5) / 256.0);
        arctic_crs.project_node(arctic_read, fx, fy)
    };
    assert!(at(56, 205).1 > 89.9, "pixel (56, 205) is next to the pole");
    let (west, east) = (at(55, 100), at(56, 100));
    let near_170w = |lon: f64| ((lon + 180.0).rem_euclid(360.0) - 10.0).abs() < 1.0;
    assert!(
        (east.0 - west.0).abs() > 180.0 && near_170w(west.0) && near_170w(east.0),
        "the 170°W cut runs between pixels 55 and 56 of row 100: {west:?} {east:?}"
    );
}
