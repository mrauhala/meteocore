//! Map renders whose tile the TM35FIN source's footprint misses, or only
//! partly covers (#1129). Far past ±180° the Transverse Mercator series
//! folds back onto the raster, so before the footprint check moved ahead of
//! the read, such a Web Mercator meta-tile chose a level by its folded
//! samples, decoded it and refined a projection grid over the whole tile to
//! its cell cap, for pixels the footprint guard then left empty.
//!
//! Each test compares `get_raster_tile` with [`whole_tile_render`], the
//! render as it was before: what it draws, and the source projections
//! (`SOURCE_PROJECTIONS`) it makes.

use std::cell::Cell;

use ds_core::geo::{projected_output_crs, wgs84_envelope};
use ds_core::map_engine::{MapEngine, OutputCrs, RasterTile, RasterValues};
use ds_core::resample::ProjectionGrid;
use ds_core::web_mercator::{x_to_lon, y_to_lat, EARTH_RADIUS};

use super::*;

/// The committed 480 × 360 TM35FIN composite, with overviews of 240 × 180
/// and 120 × 90: the production composite's extent at a tenth of its
/// resolution.
fn engine() -> GeoTiffEngine {
    let config = ds_core::config::GeoTiffConfig {
        filename_template: Some("radar_tm35_%Y%m%dT%H%MZ.tif".to_string()),
        filename_pattern: None,
        timestamp_format: None,
        parameter: "reflectivity".to_string(),
        unit: "dBZ".to_string(),
        poll_interval_secs: 3600,
        tile_cache_mb: 16,
        band: 1,
        max_files: None,
        nodata: None,
        scale: None,
        offset: None,
        exclude_patterns: vec![],
        endpoint: None,
        bucket: None,
        prefix_pattern: None,
        time_window: None,
        scan_days: None,
        stac_url: None,
        stac_asset_key: "data".to_string(),
        stac_asset_allowlist: None,
    };
    GeoTiffEngine::new(
        "radar-tm35fin",
        Some("../../testdata/radar-tm35fin"),
        &config,
    )
    .expect("engine over the committed TM35FIN fixture")
}

/// The WGS84 box of a WMS Web Mercator meta-tile: `level` of the
/// half-octave ladder, `2^(level/2)` tiles a world, longitudes unwrapped
/// past ±180° as `ds_render::metatile` passes them.
fn meta_tile(level: i32, col: i64, row: i64) -> [f64; 4] {
    let origin = std::f64::consts::PI * EARTH_RADIUS;
    let span = 2.0 * origin / 2f64.powf(f64::from(level) / 2.0);
    let (x0, top) = (-origin + col as f64 * span, origin - row as f64 * span);
    [
        x_to_lon(x0),
        y_to_lat(top - span),
        x_to_lon(x0 + span),
        y_to_lat(top),
    ]
}

/// A tile of `size` × `size` pixels from `get_raster_tile`, with the source
/// projections its render made.
fn render(
    engine: &GeoTiffEngine,
    bbox: [f64; 4],
    size: u32,
    crs: &OutputCrs,
) -> (RasterTile, usize) {
    SOURCE_PROJECTIONS.with(|n| n.set(0));
    let tile = engine
        .get_raster_tile(bbox, size, size, None, crs, None, None, None)
        .expect("render");
    (tile, SOURCE_PROJECTIONS.with(|n| n.get()))
}

/// The same tile rendered as before #1129: the level chosen for the whole
/// tile, a grid refined over the whole tile, the guard's window from the
/// level's envelope, then the read and the resample `get_raster_tile` still
/// makes. With the source projections its grid made.
fn whole_tile_render(
    engine: &GeoTiffEngine,
    bbox: [f64; 4],
    size: u32,
    crs: &OutputCrs,
) -> (RasterTile, usize) {
    let catalog = engine.catalog.load();
    let (_, entry) = catalog.entries.iter().next_back().expect("one frame");
    let metadata = entry.metadata().expect("metadata");
    let [west, south, east, north] = bbox;
    let overview = metadata.select_overview(west, south, east, north, size, size);
    let gt = overview.map_or_else(
        || metadata.geo_transform.clone(),
        |ov| metadata.overview_geo_transform(ov),
    );
    let projections = Cell::new(0);
    let grid = ProjectionGrid::build_2d(
        size,
        size,
        gt.width,
        gt.height,
        |fx, fy| crs.project_node(bbox, fx, fy),
        |lon, lat| {
            projections.set(projections.get() + 1);
            gt.world_to_pixel_f64(lon, lat)
        },
    );
    let pixels = (size * size) as usize;
    let values = match gt.bbox_to_pixels(west, south, east, north) {
        None => RasterValues::F64(vec![None; pixels]),
        Some((c0, r0, c1, r1)) => {
            let window = reader::read_bbox_u8(
                entry.source().expect("source"),
                metadata,
                overview,
                c0,
                r0,
                c1,
                r1,
                engine.band_index,
            )
            .expect("read")
            .expect("the fixture takes the u8 path");
            let mut data = Vec::with_capacity(pixels);
            resample_nearest(
                &grid,
                size,
                size,
                &window.data,
                (c1 - c0) as usize,
                (c0, r0, c1, r1),
                window.nodata,
                crs.footprint_pixel_window(bbox, gt.bbox(), size, size),
                &mut data,
            );
            RasterValues::U8 {
                data,
                nodata: Some(window.nodata),
                gain: metadata.scale.unwrap_or(1.0),
                offset: metadata.offset.unwrap_or(0.0),
            }
        }
    };
    let tile = RasterTile {
        width: size,
        height: size,
        values,
    };
    (tile, projections.get())
}

fn values(tile: &RasterTile) -> Vec<Option<f64>> {
    tile.values.iter_values().collect()
}

/// The production meta-tiles past ±180° that logged the cap: no read, no
/// projection, and as empty as they were. Before, the level-3 tile past
/// 180°E projected 1 486 134 times, its grid refined to 256 × 256 cells,
/// and each such tile took about 170 ms. A projected tile far from the
/// source, an EPSG:3035 one over Iberia, is answered the same way.
#[test]
fn off_world_meta_tiles_read_and_project_nothing() {
    let engine = engine();
    let merc = OutputCrs::WebMercator;
    let laea = projected_output_crs("EPSG:3035").unwrap();
    let iberia = [3_072_000.0, 1_024_000.0, 4_096_000.0, 2_048_000.0];
    let mut tiles: Vec<(String, [f64; 4], OutputCrs)> = [
        (2, -1, 0),
        (2, 2, 0),
        (3, -1, 2),
        (3, 3, 2),
        (4, -2, 3),
        (4, -1, 3),
        (4, 5, 3),
    ]
    .iter()
    .map(|&(level, col, row)| {
        let name = format!("EPSG:3857 level {level} col {col} row {row}");
        (name, meta_tile(level, col, row), merc.clone())
    })
    .collect();
    tiles.push((
        "EPSG:3035 over Iberia".into(),
        wgs84_envelope(&laea, iberia).unwrap(),
        OutputCrs::Projected {
            crs: laea,
            bbox: iberia,
        },
    ));
    for (name, bbox, crs) in &tiles {
        let (tile, projections) = render(&engine, *bbox, 256, crs);
        assert!(tile.is_empty(), "{name} draws data");
        assert_eq!(projections, 0, "{name} projects");
        let (before, _) = whole_tile_render(&engine, *bbox, 256, crs);
        assert!(before.is_empty(), "{name} drew data before");
    }
    let (_, before) = whole_tile_render(&engine, meta_tile(3, 3, 2), 256, &merc);
    assert_eq!(before, 1_486_134, "the cell cap's projections");
}

/// Tiles the footprint covers, in Web Mercator, CRS:84 and EPSG:3067, draw
/// pixel for pixel what they drew before, with no more projections.
#[test]
fn in_world_tiles_draw_as_before() {
    let engine = engine();
    let tm = projected_output_crs("EPSG:3067").unwrap();
    let north = [0.0, 7_168_000.0, 1_024_000.0, 8_192_000.0];
    let mut tiles: Vec<(String, [f64; 4], OutputCrs)> =
        [(7, 6, 2), (8, 8, 3), (8, 9, 3), (8, 8, 4), (8, 9, 4)]
            .iter()
            .map(|&(level, col, row)| {
                let name = format!("EPSG:3857 level {level} col {col} row {row}");
                (name, meta_tile(level, col, row), OutputCrs::WebMercator)
            })
            .collect();
    tiles.push((
        "CRS:84 over Finland".into(),
        [20.0, 60.0, 32.0, 67.0],
        OutputCrs::Wgs84,
    ));
    tiles.push((
        "EPSG:3067 over northern Finland".into(),
        wgs84_envelope(&tm, north).unwrap(),
        OutputCrs::Projected {
            crs: tm,
            bbox: north,
        },
    ));
    for (name, bbox, crs) in &tiles {
        let (tile, projections) = render(&engine, *bbox, 256, crs);
        let (before, before_projections) = whole_tile_render(&engine, *bbox, 256, crs);
        let drawn = values(&tile).iter().filter(|v| v.is_some()).count();
        assert!(drawn > 1000, "{name} draws {drawn} pixels");
        assert!(values(&tile) == values(&before), "{name} moved pixels");
        assert!(
            projections <= before_projections,
            "{name}: {projections} projections, {before_projections} before"
        );
    }
}

/// A tile the footprint only partly covers refines its grid for the
/// footprint alone. Past it the forward folds, which drove the world-scale
/// level-2 tile holding the data to the cell cap: the same 1 486 134
/// projections, without a log line. Where the coarser grid picks another
/// source pixel than the denser one did, the exact position lies within
/// the grid's interpolation budget, 0.2 source pixels, plus the slack
/// between its error probes, of that pixel's edge.
#[test]
fn a_partly_covered_tile_is_refined_for_its_footprint() {
    let engine = engine();
    let merc = OutputCrs::WebMercator;
    let catalog = engine.catalog.load();
    let (_, entry) = catalog.entries.iter().next_back().expect("one frame");
    let metadata = entry.metadata().expect("metadata");
    for (level, col, row, at_most) in [(2, 1, 0, 3_000), (6, 4, 2, 700), (7, 6, 3, 800)] {
        let name = format!("level {level} col {col} row {row}");
        let bbox = meta_tile(level, col, row);
        let (tile, projections) = render(&engine, bbox, 256, &merc);
        let (_, before) = whole_tile_render(&engine, bbox, 256, &merc);
        assert!(
            projections <= at_most && projections < before,
            "{name}: {projections} projections, {before} before"
        );
        // Every drawn pixel against the exact per-pixel projection.
        let [west, south, east, north] = bbox;
        let overview = metadata.select_overview(west, south, east, north, 256, 256);
        let gt = overview.map_or_else(
            || metadata.geo_transform.clone(),
            |ov| metadata.overview_geo_transform(ov),
        );
        let source = reader::read_bbox_u8(
            entry.source().expect("source"),
            metadata,
            overview,
            0,
            0,
            gt.width,
            gt.height,
            engine.band_index,
        )
        .expect("read")
        .expect("the fixture takes the u8 path");
        // Within the footprint: outside it the forward folds, and the
        // guard leaves those pixels empty whatever they project to.
        let (x_lo, x_hi, y_lo, y_hi) = merc
            .footprint_window_in_image(bbox, metadata.geo_transform.bbox(), 256, 256)
            .expect("the footprint meets the tile");
        let mut drawn = 0;
        for (i, value) in values(&tile).into_iter().enumerate() {
            let (ox, oy) = ((i % 256) as u32, (i / 256) as u32);
            if ox < x_lo || ox > x_hi || oy < y_lo || oy > y_hi {
                assert_eq!(value, None, "{name}: pixel {i} past the footprint");
                continue;
            }
            drawn += usize::from(value.is_some());
            let (fx, fy) = ((f64::from(ox) + 0.5) / 256.0, (f64::from(oy) + 0.5) / 256.0);
            let (lon, lat) = merc.project_node(bbox, fx, fy);
            let (c, r) = gt.world_to_pixel_f64(lon, lat);
            let on_raster =
                (0.0..f64::from(gt.width)).contains(&c) && (0.0..f64::from(gt.height)).contains(&r);
            let exact = on_raster
                .then(|| source.data[r as usize * gt.width as usize + c as usize])
                .filter(|&v| v != source.nodata)
                .map(f64::from);
            if value != exact {
                let edge = (c - c.round()).abs().min((r - r.round()).abs());
                assert!(edge < 0.3, "{name}: pixel {i} is {edge} px from an edge");
            }
        }
        assert!(drawn > 400, "{name} draws {drawn} pixels");
    }
}

/// The antimeridian: a box across it in each form an engine is handed (east
/// past 180° from Maps and the meta-tiling, `west > east` otherwise) and
/// the Web Mercator meta-tiles either side of it. The source is half a
/// world away, so each draws nothing, as before, without a projection.
#[test]
fn tiles_at_the_antimeridian_draw_as_before() {
    let engine = engine();
    let merc = OutputCrs::WebMercator;
    let tiles = [
        (
            "unwrapped, CRS:84",
            [170.0, 10.0, 190.0, 20.0],
            OutputCrs::Wgs84,
        ),
        (
            "unwrapped, EPSG:3857",
            [170.0, 10.0, 190.0, 20.0],
            merc.clone(),
        ),
        (
            "west > east, CRS:84",
            [170.0, 10.0, -170.0, 20.0],
            OutputCrs::Wgs84,
        ),
        ("level 8 col 15 row 7", meta_tile(8, 15, 7), merc.clone()),
        ("level 8 col 16 row 7", meta_tile(8, 16, 7), merc.clone()),
    ];
    for (name, bbox, crs) in &tiles {
        let (tile, projections) = render(&engine, *bbox, 256, crs);
        let (before, _) = whole_tile_render(&engine, *bbox, 256, crs);
        assert!(values(&tile) == values(&before), "{name} moved pixels");
        assert!(tile.is_empty(), "{name} draws data");
        assert_eq!(projections, 0, "{name} projects");
    }
}
