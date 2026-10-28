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
//!
//! The CRS84 and EPSG:3857 walk maps each output column and row once
//! (#1331). The per-pixel walk it replaced stays here as its reference,
//! bit for bit.

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

/// The CRS84 / EPSG:3857 walk before #1331: `project_node` at every output
/// pixel. The separable walk must reproduce it bit for bit.
fn per_pixel_walk(
    grid: &DecodedGrid,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    crs: &OutputCrs,
) -> Vec<Option<f64>> {
    let (w, h) = (width as usize, height as usize);
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        let fy = (row as f64 + 0.5) / h as f64;
        for col in 0..w {
            let fx = (col as f64 + 0.5) / w as f64;
            let (lon, lat) = crs.project_node(bbox, fx, fy);
            out.push(grid.bilinear_value(grid.lon_in_frame(lon), lat));
        }
    }
    out
}

/// The committed field's first `nj` rows and `ni` columns placed on another
/// regular grid, `(lon_first, lat_first, lon_inc, lat_inc)`: real values on
/// a regional geometry. A `side × side` block of NaN from node
/// `(nan_row, nan_col)` puts the NaN-neighbour rules in the views too: one
/// node falls back to a valid corner, a wider block also leaves cells with
/// no valid corner empty.
fn regional(
    q: &DecodedGrid,
    (lon_first, lat_first, lon_inc, lat_inc): (f64, f64, f64, f64),
    (ni, nj): (usize, usize),
    (nan_row, nan_col, side): (usize, usize, usize),
) -> DecodedGrid {
    let mut values: Vec<f32> = (0..nj)
        .flat_map(|row| q.values[row * q.ni..row * q.ni + ni].iter().copied())
        .collect();
    for row in nan_row..nan_row + side {
        values[row * ni + nan_col..row * ni + nan_col + side].fill(f32::NAN);
    }
    DecodedGrid {
        ni,
        nj,
        lon_first,
        lat_first,
        lon_inc,
        lat_inc,
        values: Arc::new(values),
        ..q.clone()
    }
}

/// #1331: the CRS84 and EPSG:3857 sampler maps each output column and row
/// once instead of every pixel, and must still give the per-pixel walk's
/// samples bit for bit: the f64 ones and the f32 map tile narrowed after
/// its display conversion. Global grids stored from 180° and from 0°,
/// regional ones across the antimeridian and stored south to north; the
/// seam box 170,10,-170,20 in both forms, boxes a turn past ±180° and
/// non-square outputs.
#[test]
fn separable_walk_matches_the_per_pixel_walk_bit_for_bit() {
    let q = ecmwf_q();
    let greenwich_first = DecodedGrid {
        lon_first: 0.0,
        ..q.clone()
    };
    // 160°E..219.75°E, 40°N..0°N; one NaN node at 178.75°E, 16.25°N.
    let across_seam = regional(&q, (160.0, 40.0, 0.25, -0.25), (240, 161), (95, 75, 1));
    // 30°W..39.9°E, 30°N..74.9°N at 0.1°; NaN from 20°E, 70°N.
    let south_up = regional(&q, (-30.0, 30.0, 0.1, 0.1), (700, 450), (400, 500, 3));
    let grids = [
        ("global from 180°", &q),
        ("global from 0°", &greenwich_first),
        ("regional across the seam", &across_seam),
        ("regional south to north", &south_up),
    ];

    let seam_unwrapped = [170.0, 10.0, 190.0, 20.0];
    let seam_west = [-190.0, 10.0, -170.0, 20.0];
    let europe = [-40.0, 25.0, 50.0, 80.0];
    let views = [
        ([-180.0, -85.0, 180.0, 85.0], 360, 170),
        ([19.0, 59.0, 32.0, 71.0], 256, 256),
        (europe, 300, 200),
        // As OGC API Maps passes it (#828) and as a WMS client wrapping
        // the world west sends it.
        (seam_unwrapped, 256, 128),
        (seam_west, 256, 128),
        // World copies a turn east and a turn west, and one straddling
        // 360°.
        ([360.0, 55.0, 390.0, 70.0], 256, 256),
        ([-355.0, 57.0, -335.0, 68.0], 300, 200),
        ([330.0, 40.0, 370.0, 60.0], 200, 300),
        // Sub-cell magnification, odd sizes.
        ([24.9, 60.1, 25.3, 60.4], 7, 13),
    ];
    let convert = |v: f64| v * 1e6 - 273.15;

    for (name, grid) in grids {
        for (bbox, width, height) in views {
            for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
                let reference = per_pixel_walk(grid, bbox, width, height, &crs);
                let samples = grid.resample(bbox, width, height, &crs);
                let tile = grid.resample_f32(bbox, width, height, &crs, convert);
                assert_eq!(samples.len(), reference.len());
                assert_eq!(tile.len(), reference.len());
                for (i, ((want, got), pixel)) in
                    reference.iter().zip(&samples).zip(&tile).enumerate()
                {
                    let want_f32 = want.map_or(f32::NAN, |v| convert(v) as f32);
                    assert!(
                        want.map(f64::to_bits) == got.map(f64::to_bits)
                            && want_f32.to_bits() == pixel.to_bits(),
                        "{name} {crs:?} {bbox:?} {width}x{height} pixel {i}: \
                         per pixel {want:?} / {want_f32}, separable {got:?} / {pixel}"
                    );
                }
            }
        }
    }

    // What the cases cover. The regional grid across the seam fills both
    // forms of the seam box, the western one only a turn east of where it
    // is asked for, its NaN node through the fallback; the Europe view runs
    // off the south-up grid, and the Finland view inside it meets the NaN
    // block's empty cells.
    for seam in [seam_unwrapped, seam_west] {
        for crs in [OutputCrs::Wgs84, OutputCrs::WebMercator] {
            let samples = across_seam.resample(seam, 256, 128, &crs);
            assert!(samples.iter().all(Option::is_some), "{seam:?} {crs:?}");
        }
    }
    let off_grid = south_up.resample(europe, 300, 200, &OutputCrs::WebMercator);
    assert!(off_grid.iter().any(Option::is_none) && off_grid.iter().any(Option::is_some));
    let nan_cells = south_up.resample([19.0, 59.0, 32.0, 71.0], 256, 256, &OutputCrs::Wgs84);
    assert!(nan_cells.iter().any(Option::is_none));
    let straddling = south_up.resample([330.0, 40.0, 370.0, 60.0], 200, 300, &OutputCrs::Wgs84);
    assert!(straddling.iter().all(Option::is_some));
}
