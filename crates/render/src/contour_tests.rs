//! Contour tracing and drawing (#1378), over fields whose lines are known.

use super::*;
use ds_core::web_mercator::{lat_to_y, lon_to_x, x_to_lon, y_to_lat};

/// Lines alone, `interval` apart through 0, black, 1 px.
fn lines(interval: f64) -> ContourStyle {
    ContourStyle::new(interval, 0.0, Some("#000000"), Some(1.0), false).unwrap()
}

/// What an engine returns for a view: `field(lon, lat)` at each pixel
/// centre, mapped through the output CRS as every engine maps it. `None`
/// is nodata.
fn sample(
    field: &dyn Fn(f64, f64) -> Option<f64>,
    bbox: [f64; 4],
    width: u32,
    height: u32,
    crs: &OutputCrs,
) -> RasterTile {
    let mut data = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        for x in 0..width {
            let (lon, lat) = crs.project_node(
                bbox,
                (f64::from(x) + 0.5) / f64::from(width),
                (f64::from(y) + 0.5) / f64::from(height),
            );
            data.push(field(lon, lat).map_or(f32::NAN, |v| v as f32));
        }
    }
    RasterTile {
        width,
        height,
        values: RasterValues::F32 { data, nodata: None },
    }
}

/// Render `style` over the view through a [`ContourPlan`], as the API
/// layers do: the engine is asked for the plan's grown view.
fn render(
    field: &dyn Fn(f64, f64) -> Option<f64>,
    style: &ContourStyle,
    fill: Option<&dyn ColorMap>,
    bbox: [f64; 4],
    (width, height): (u32, u32),
    crs: &OutputCrs,
    source_deg: Option<f64>,
) -> Option<Vec<u8>> {
    let plan = ContourPlan::new(style, bbox, width, height, crs, source_deg);
    let tile = sample(
        field,
        plan.fetch_bbox(),
        plan.fetch_width(),
        plan.fetch_height(),
        plan.fetch_crs(),
    );
    plan.draw(&tile, style, fill).unwrap()
}

/// A cone around (50, 50): its value is the distance from that point, so
/// the line at `L` is the circle of radius `L`.
fn cone(lon: f64, lat: f64) -> Option<f64> {
    Some(((lon - 50.0).powi(2) + (lat - 50.0).powi(2)).sqrt())
}

/// One degree per pixel: pixel `(x, y)` has its centre at
/// `(x + 0.5, 99.5 − y)`.
const UNIT_VIEW: [f64; 4] = [0.0, 0.0, 100.0, 100.0];

fn centre(x: usize, y: usize) -> (f64, f64) {
    (x as f64 + 0.5, 99.5 - y as f64)
}

/// Every pixel the line covers at least half lies within half a pixel
/// (the line's half-width) and a little interpolation error of a true
/// circle, and every point of every circle in view has a drawn pixel at
/// its place.
#[test]
fn lines_follow_the_level_sets() {
    let style = lines(7.0);
    let rgba = render(
        &cone,
        &style,
        None,
        UNIT_VIEW,
        (100, 100),
        &OutputCrs::Wgs84,
        None,
    )
    .expect("circles in view");
    let alpha = |x: usize, y: usize| rgba[(y * 100 + x) * 4 + 3];
    let mut drawn = 0;
    for y in 0..100 {
        for x in 0..100 {
            if alpha(x, y) >= 128 {
                drawn += 1;
                let (lon, lat) = centre(x, y);
                let r = cone(lon, lat).unwrap();
                let off = (r - (r / 7.0).round() * 7.0).abs();
                assert!(off <= 0.55, "pixel ({x}, {y}) is {off} px off its circle");
            }
        }
    }
    assert!(drawn > 500, "{drawn} line pixels");
    for k in 1..=7 {
        let radius = 7.0 * f64::from(k);
        for step in 0..360 {
            let angle = f64::from(step).to_radians();
            let (lon, lat) = (50.0 + radius * angle.cos(), 50.0 + radius * angle.sin());
            let (x, y) = (lon.floor() as usize, (100.0 - lat).floor() as usize);
            if x < 100 && y < 100 {
                assert!(alpha(x, y) > 0, "circle {radius} has a gap at {step}°");
            }
        }
    }
}

/// A 2 × 2 block of tiles is one render at twice the size: the lines leaving
/// one tile meet the same lines in the next, in Web Mercator and in a
/// projected CRS, with and without smoothing, with and without a fill.
#[test]
fn a_block_of_tiles_matches_one_render_of_the_block() {
    let field = |lon: f64, lat: f64| {
        Some(1000.0 + 3.0 * (lon * 0.15).sin() + 2.0 * (lat * 0.2).cos() + 0.01 * lon * lat)
    };
    let fill_cmap = crate::StyleContext::with_builtins()
        .build_colormap(&crate::StyleSpec {
            colormap: Some("pressure"),
            min: Some(980.0),
            max: Some(1040.0),
            ..Default::default()
        })
        .unwrap()
        .colormap;
    let tm35fin = ds_core::geo::projected_output_crs("EPSG:3067").unwrap();
    const TILE: u32 = 64;
    // Views in their CRS's own linear units: Mercator metres, TM35FIN metres.
    let views: [(&str, [f64; 4]); 2] = [
        ("EPSG:3857", {
            let (x0, y1) = (lon_to_x(20.0), lat_to_y(62.0));
            let span = 2.0 * 40_000.0 * f64::from(TILE);
            [x0, y1 - span, x0 + span, y1]
        }),
        ("EPSG:3067", {
            let span = 2.0 * 10_000.0 * f64::from(TILE);
            [
                -200_000.0,
                6_600_000.0,
                -200_000.0 + span,
                6_600_000.0 + span,
            ]
        }),
    ];
    let view = |name: &str, [x0, y0, x1, y1]: [f64; 4]| -> ([f64; 4], OutputCrs) {
        match name {
            "EPSG:3857" => (
                [x_to_lon(x0), y_to_lat(y0), x_to_lon(x1), y_to_lat(y1)],
                OutputCrs::WebMercator,
            ),
            _ => (
                ds_core::geo::wgs84_envelope(&tm35fin, [x0, y0, x1, y1]).unwrap(),
                OutputCrs::Projected {
                    crs: tm35fin.clone(),
                    bbox: [x0, y0, x1, y1],
                },
            ),
        }
    };
    for (name, block) in views {
        // No source grid (no smoothing), then a source far finer than an
        // output pixel (two passes).
        for source_deg in [None, Some(1e-4)] {
            for fill in [None, Some(fill_cmap.as_ref())] {
                let style = ContourStyle {
                    fill: fill.is_some(),
                    ..lines(0.5)
                };
                let (bbox, crs) = view(name, block);
                let whole = render(
                    &field,
                    &style,
                    fill,
                    bbox,
                    (2 * TILE, 2 * TILE),
                    &crs,
                    source_deg,
                )
                .unwrap();
                let plan = ContourPlan::new(&style, bbox, 2 * TILE, 2 * TILE, &crs, source_deg);
                assert_eq!(plan.smoothing(), if source_deg.is_some() { 2 } else { 0 });
                let mut stitched = vec![0u8; whole.len()];
                let [x0, y0, x1, y1] = block;
                let (half_x, half_y) = ((x1 - x0) / 2.0, (y1 - y0) / 2.0);
                for (col, row) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let tile_block = [
                        x0 + f64::from(col) * half_x,
                        y1 - f64::from(row + 1) * half_y,
                        x0 + f64::from(col + 1) * half_x,
                        y1 - f64::from(row) * half_y,
                    ];
                    let (tbbox, tcrs) = view(name, tile_block);
                    let tile = render(&field, &style, fill, tbbox, (TILE, TILE), &tcrs, source_deg)
                        .unwrap();
                    for y in 0..TILE as usize {
                        let out = ((row as usize * TILE as usize + y) * 2 * TILE as usize
                            + col as usize * TILE as usize)
                            * 4;
                        let src = y * TILE as usize * 4;
                        stitched[out..out + TILE as usize * 4]
                            .copy_from_slice(&tile[src..src + TILE as usize * 4]);
                    }
                }
                let worst = whole
                    .iter()
                    .zip(&stitched)
                    .map(|(a, b)| a.abs_diff(*b))
                    .max()
                    .unwrap();
                assert!(
                    worst <= 1,
                    "{name}, source {source_deg:?}, fill {}: tiles differ from one render by {worst}",
                    fill.is_some()
                );
                // Lines do cross both seams, so the comparison holds them.
                if fill.is_none() {
                    let side = 2 * TILE as usize;
                    let drawn = |x: usize, y: usize| whole[(y * side + x) * 4 + 3] > 0;
                    let seam = TILE as usize;
                    let across = (0..side)
                        .filter(|&y| drawn(seam - 1, y) && drawn(seam, y))
                        .count();
                    let down = (0..side)
                        .filter(|&x| drawn(x, seam - 1) && drawn(x, seam))
                        .count();
                    assert!(
                        across > 2 && down > 2,
                        "{name}: {across} / {down} seam pixels"
                    );
                }
            }
        }
    }
}

/// A line ends where the data does: no line is drawn over nodata, and the
/// lines on either side are kept.
#[test]
fn lines_break_at_nodata() {
    let gap = |lon: f64, lat: f64| {
        if (40.0..60.0).contains(&lon) {
            None
        } else {
            cone(lon, lat)
        }
    };
    let rgba = render(
        &gap,
        &lines(7.0),
        None,
        UNIT_VIEW,
        (100, 100),
        &OutputCrs::Wgs84,
        None,
    )
    .unwrap();
    let alpha = |x: usize, y: usize| rgba[(y * 100 + x) * 4 + 3];
    for y in 0..100 {
        // Columns 41..=58 are nodata and at least a pixel from data.
        for x in 41..=58 {
            assert_eq!(alpha(x, y), 0, "line drawn over nodata at ({x}, {y})");
        }
    }
    let side = |xs: std::ops::Range<usize>| {
        xs.flat_map(|x| (0..100).map(move |y| (x, y)))
            .filter(|&(x, y)| alpha(x, y) > 0)
            .count()
    };
    assert!(side(0..40) > 100 && side(60..100) > 100);

    // With a fill, nodata stays transparent and data is painted.
    let fill = crate::StyleContext::with_builtins()
        .build_colormap(&crate::StyleSpec {
            colormap: Some("viridis"),
            min: Some(0.0),
            max: Some(70.0),
            ..Default::default()
        })
        .unwrap();
    let style = ContourStyle {
        fill: true,
        ..lines(7.0)
    };
    let rgba = render(
        &gap,
        &style,
        Some(fill.colormap.as_ref()),
        UNIT_VIEW,
        (100, 100),
        &OutputCrs::Wgs84,
        None,
    )
    .unwrap();
    assert_eq!(rgba[(10 * 100 + 50) * 4 + 3], 0, "nodata stays empty");
    assert_eq!(rgba[(10 * 100 + 20) * 4 + 3], 255, "data is filled");
}

/// No data at all draws nothing; data without a line in view draws nothing
/// either when there is no fill.
#[test]
fn nothing_to_draw_is_none() {
    let none = |_: f64, _: f64| None;
    assert!(render(
        &none,
        &lines(1.0),
        None,
        UNIT_VIEW,
        (32, 32),
        &OutputCrs::Wgs84,
        None
    )
    .is_none());
    let flat = |_: f64, _: f64| Some(1000.5);
    assert!(render(
        &flat,
        &lines(1.0),
        None,
        UNIT_VIEW,
        (32, 32),
        &OutputCrs::Wgs84,
        None
    )
    .is_none());
}

/// A saddle cell (diagonal corners on one side of the level) is decided by
/// its mean, the same in every tile: above it, the high corners join
/// through the centre and the low ones are cut off.
#[test]
fn saddles_follow_the_cell_mean() {
    let segments = |values: [f32; 4]| {
        // tl, tr, bl, br of one cell.
        let grid = Grid {
            width: 2,
            height: 2,
            values: values.to_vec(),
        };
        let mut out = Vec::new();
        trace(&grid, 0, 0.0, &lines(10.0), &mut |p, q| out.push((p, q))).unwrap();
        out
    };
    // Highs at tl and br, mean 10.5 ≥ 10: cut off tr and bl.
    let joined = segments([15.0, 8.0, 8.0, 11.0]);
    assert_eq!(joined.len(), 2);
    let near = |(x, y): (f64, f64), (cx, cy): (f64, f64)| (x - cx).abs() + (y - cy).abs() < 1.0;
    // tr corner is at view (1.5, 0.5), bl at (0.5, 1.5).
    assert!(joined
        .iter()
        .all(|&(p, q)| (near(p, (1.5, 0.5)) && near(q, (1.5, 0.5)))
            || (near(p, (0.5, 1.5)) && near(q, (0.5, 1.5)))));
    // Mean 9.625 < 10: the highs are cut off instead.
    let apart = segments([12.0, 8.0, 8.0, 10.5]);
    assert_eq!(apart.len(), 2);
    assert!(apart
        .iter()
        .all(|&(p, q)| (near(p, (0.5, 0.5)) && near(q, (0.5, 0.5)))
            || (near(p, (1.5, 1.5)) && near(q, (1.5, 1.5)))));
}

/// Two cells sharing an edge find the same crossing point on it, whichever
/// side traces it: the samples' order decides the arithmetic, not the cell.
#[test]
fn shared_edges_cross_at_one_point() {
    let grid = Grid {
        width: 3,
        height: 2,
        values: vec![0.3, 1.7, 0.2, 0.9, 0.6, 0.4],
    };
    let mut out = Vec::new();
    trace(&grid, 0, 0.0, &lines(1.0), &mut |p, q| out.push((p, q))).unwrap();
    // The edge between samples (1, 0) and (1, 1) is the right edge of the
    // first cell and the left edge of the second.
    let on_shared = |(x, _): (f64, f64)| (x - 1.5).abs() < 1e-12;
    let points: Vec<(f64, f64)> = out
        .iter()
        .flat_map(|&(p, q)| [p, q])
        .filter(|&p| on_shared(p))
        .collect();
    assert_eq!(points.len(), 2, "{out:?}");
    assert_eq!(points[0], points[1]);
}

/// The plan reads past the view by the line's reach plus the smoothing
/// passes, which the zoom decides: two or more source cells per pixel.
#[test]
fn the_plan_grows_by_reach_and_smoothing() {
    assert_eq!(lines(1.0).line_margin(), 1);
    let wide = ContourStyle {
        width: 3.0,
        ..lines(1.0)
    };
    assert_eq!(wide.line_margin(), 2);
    assert_eq!(smoothing_passes(0.1, None), 0);
    assert_eq!(smoothing_passes(0.1, Some(0.1)), 0);
    assert_eq!(smoothing_passes(0.25, Some(0.1)), 1);
    assert_eq!(smoothing_passes(0.5, Some(0.1)), 2);
    let plan = ContourPlan::new(&wide, UNIT_VIEW, 100, 100, &OutputCrs::Wgs84, Some(0.25));
    assert_eq!(plan.smoothing(), 2);
    assert_eq!((plan.fetch_width(), plan.fetch_height()), (108, 108));
    assert_eq!(plan.fetch_bbox(), [-4.0, -4.0, 104.0, 104.0]);
    // A tile of the wrong size is an error, never a misaligned drawing.
    let tile = sample(&cone, UNIT_VIEW, 100, 100, &OutputCrs::Wgs84);
    assert!(plan.draw(&tile, &wide, None).is_err());
}

/// Its description names the interval in the parameter's unit, and the
/// value a line passes through when it is not a multiple of it.
#[test]
fn descriptions_name_the_interval() {
    assert_eq!(lines(4.0).description(Some("hPa")), "every 4 hPa");
    assert_eq!(lines(400.0).description(Some("Pa")), "every 400 Pa");
    assert_eq!(lines(0.5).description(None), "every 0.5");
    let kelvin = ContourStyle::new(2.0, 273.15, None, None, false).unwrap();
    assert_eq!(kelvin.description(Some("K")), "every 2 K through 273.15 K");
    assert_eq!(kelvin.color, [0, 0, 0, 255]);
    assert_eq!(kelvin.width, 1.0);
    let through_zero = ContourStyle::new(6.0, 546.0, None, None, false).unwrap();
    assert_eq!(through_zero.description(Some("dam")), "every 6 dam");
}

/// The legends: the JSON carries the contour member (and no colour scale
/// for lines alone), the image draws a swatch in the line colour.
#[test]
fn legends_describe_the_lines() {
    let resolved = crate::StyleContext::with_builtins()
        .build_colormap(&crate::StyleSpec {
            colormap: Some("pressure"),
            ..Default::default()
        })
        .unwrap();
    let contour = ContourStyle::new(4.0, 0.0, Some("#ff0000"), Some(2.0), false).unwrap();
    let style = StyleInfo {
        name: "contours".into(),
        title: "Isobars".into(),
        colormap: resolved.colormap.clone(),
        palette: resolved.palette.clone(),
        min: resolved.min,
        max: resolved.max,
        parameter: Some("msl".into()),
        contour: Some(Arc::new(contour.clone())),
    };
    let json = crate::legend_json(&style, Some("msl"), Some("hPa"));
    assert_eq!(json["contour"]["description"], "every 4 hPa");
    assert_eq!(json["contour"]["interval"], 4.0);
    assert_eq!(json["contour"]["color"], "#FF0000");
    assert_eq!(json["contour"]["width"], 2.0);
    assert_eq!(json["contour"]["fill"], false);
    assert_eq!(json["unit"], "hPa");
    assert!(json.get("stops").is_none() && json.get("min").is_none());
    // Over a fill, the fill's scale stays.
    let filled = StyleInfo {
        contour: Some(Arc::new(ContourStyle {
            fill: true,
            ..contour.clone()
        })),
        ..style.clone()
    };
    let json = crate::legend_json(&filled, Some("msl"), Some("hPa"));
    assert!(json["stops"].as_array().is_some_and(|s| !s.is_empty()));
    assert_eq!(json["contour"]["fill"], true);

    for (style, fill) in [(&style, false), (&filled, true)] {
        let png = crate::render_style_legend(
            style,
            Some("hPa"),
            180,
            300,
            ImageFormat::Png,
            Some("msl (hPa)\nIsobars"),
        )
        .unwrap();
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut buf).unwrap();
        assert_eq!(frame.color_type, png::ColorType::Rgba);
        let pixels = buf[..frame.buffer_size()].to_vec();
        let red = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] == 255 && p[1] == 0 && p[2] == 0)
            .count();
        assert!(
            red >= 24,
            "the swatch is drawn in the line colour: {red} px"
        );
        // The fill's colour bar is drawn only for a filled style: the
        // pressure palette's colours are neither white, black nor red.
        let coloured = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| {
                let grey = p[0] == p[1] && p[1] == p[2];
                let red = p[0] > 0 && p[1] == p[2] && p[1] < p[0];
                !grey && !red
            })
            .count();
        assert_eq!(coloured > 100, fill, "colour bar only under a fill");
    }
}
