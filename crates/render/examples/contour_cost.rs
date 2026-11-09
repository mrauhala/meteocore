//! Render cost of a contour style against the fill-only style on WMS
//! meta-tiles (#1378): a 1024 × 1024 EPSG:3857 view on the ladder, 16
//! meta-tiles, every tile a cache miss, with the engine read replaced by an
//! analytic pressure field sampled at the pixel centres (the same samples
//! for every style, so the times compare what the styles add).
//!
//! cargo run --release -p ds-render --example contour_cost

use std::time::Instant;

use ds_core::error::DataServerError;
use ds_core::map_engine::{OutputCrs, RasterTile, RasterValues};
use ds_core::web_mercator::EARTH_RADIUS;
use ds_render::{
    assemble_metatiled, assemble_metatiled_contours, ContourStyle, MetaTilePixels, MetaTileStats,
    StyleContext, StyleSpec, TileKeyPrefix, TilePixelCache,
};

/// Mean sea-level pressure in hPa over Europe's latitudes, ~25 isobars
/// across the view at 4 hPa.
fn field(bbox: [f64; 4], width: u32, height: u32, crs: &OutputCrs) -> RasterTile {
    let mut data = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        for x in 0..width {
            let (lon, lat) = crs.project_node(
                bbox,
                (f64::from(x) + 0.5) / f64::from(width),
                (f64::from(y) + 0.5) / f64::from(height),
            );
            let p = 1010.0 + 25.0 * (lon / 7.0).sin() * (lat / 5.0).cos() + 0.3 * lon;
            data.push(p as f32);
        }
    }
    RasterTile {
        width,
        height,
        values: RasterValues::F32 { data, nodata: None },
    }
}

fn prefix(style: &str) -> TileKeyPrefix {
    TileKeyPrefix {
        layer: "nwp/msl".into(),
        parameter: Some("msl".into()),
        style: style.into(),
        time: None,
        z: None,
        reference_time: None,
        content_version: 0,
    }
}

fn stats(pixels: MetaTilePixels) -> MetaTileStats {
    match pixels {
        MetaTilePixels::Image { stats, .. } => stats,
        _ => panic!("expected an image"),
    }
}

fn main() -> Result<(), DataServerError> {
    // 0..90°E, 0..66.5°N at 1024 px: ladder level 8 (zoom 4) exactly, a
    // pure crop of 4 × 4 meta-tiles.
    let span = std::f64::consts::PI * EARTH_RADIUS / 2.0;
    let (x0, y0) = (0.0, 0.0);
    let bbox_m = [x0, y0, x0 + span, y0 + span];
    let bbox_deg = [
        ds_core::web_mercator::x_to_lon(bbox_m[0]),
        ds_core::web_mercator::y_to_lat(bbox_m[1]),
        ds_core::web_mercator::x_to_lon(bbox_m[2]),
        ds_core::web_mercator::y_to_lat(bbox_m[3]),
    ];
    let crs = OutputCrs::WebMercator;
    let (w, h) = (1024, 1024);
    let fill = StyleContext::with_builtins()
        .build_colormap(&StyleSpec {
            colormap: Some("pressure"),
            min: Some(950.0),
            max: Some(1050.0),
            ..Default::default()
        })
        .map_err(DataServerError::Render)?
        .colormap;
    let lines = ContourStyle::new(4.0, 0.0, None, None, false).map_err(DataServerError::Render)?;
    let over_fill = ContourStyle {
        fill: true,
        ..lines.clone()
    };
    const RUNS: u32 = 20;
    let report = |name: &str, run: &dyn Fn() -> Result<MetaTileStats, DataServerError>| {
        let mut best = f64::MAX;
        let (mut engine, mut draw) = (0.0, 0.0);
        for _ in 0..RUNS {
            let start = Instant::now();
            let stats = run()?;
            let total = start.elapsed().as_secs_f64() * 1e3;
            if total < best {
                best = total;
                engine = stats.engine.as_secs_f64() * 1e3;
                draw = stats.colorize.as_secs_f64() * 1e3;
            }
        }
        println!(
            "{name:<22} {best:7.2} ms  (field sampling {engine:6.2} ms, colorize/trace+draw {draw:6.2} ms)"
        );
        Ok::<(), DataServerError>(())
    };
    report("fill only", &|| {
        let cache = TilePixelCache::new(64);
        assemble_metatiled(
            bbox_deg,
            &crs,
            w,
            h,
            &prefix("default"),
            fill.as_ref(),
            &cache,
            |b, tw, th, c| Ok(field(b, tw, th, c)),
        )
        .map(stats)
    })?;
    report("contour lines", &|| {
        let cache = TilePixelCache::new(64);
        assemble_metatiled_contours(
            bbox_deg,
            &crs,
            w,
            h,
            &prefix("contours"),
            &lines,
            None,
            None,
            &cache,
            |b, tw, th, c| Ok(field(b, tw, th, c)),
        )
        .map(stats)
    })?;
    report("contours over fill", &|| {
        let cache = TilePixelCache::new(64);
        assemble_metatiled_contours(
            bbox_deg,
            &crs,
            w,
            h,
            &prefix("filled"),
            &over_fill,
            Some(fill.as_ref()),
            None,
            &cache,
            |b, tw, th, c| Ok(field(b, tw, th, c)),
        )
        .map(stats)
    })?;
    report("contours, smoothed x2", &|| {
        let cache = TilePixelCache::new(64);
        assemble_metatiled_contours(
            bbox_deg,
            &crs,
            w,
            h,
            &prefix("smoothed"),
            &lines,
            None,
            // A source eight times finer than the output: two passes.
            Some((bbox_deg[2] - bbox_deg[0]) / f64::from(w) / 8.0),
            &cache,
            |b, tw, th, c| Ok(field(b, tw, th, c)),
        )
        .map(stats)
    })?;
    Ok(())
}
