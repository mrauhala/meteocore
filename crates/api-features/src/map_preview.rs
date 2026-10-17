//! The items page's map data (#1135).
//!
//! The listing's MapLibre preview draws the page's features. Embedding their
//! geometry verbatim made a page of 100 municipality polygons about 10 MB,
//! nearly all of it hidden map data. So each ring and line is simplified to
//! about one pixel at the map's initial zoom (Douglas–Peucker in Web Mercator
//! metres, the plane MapLibre draws in), holes and parts smaller than a pixel
//! are left out, coordinates are rounded to five decimals (about 1 m), and the page's
//! shapes share a vertex budget: a feature whose shape no longer fits is drawn
//! as its bounding box when that is smaller. Only this preview changes; the
//! GeoJSON response and the item page keep every vertex.

use std::f64::consts::PI;

use ds_core::simplify::{douglas_peucker_closed, douglas_peucker_open};
use ds_core::web_mercator::{lat_to_y, lon_to_x, EARTH_RADIUS, LAT_LIMIT_DEG};
use serde_json::{json, Value};

/// Positions the simplified shapes of one page may hold together. Past it a
/// feature of more than five positions is drawn as its bounding box, so a
/// page's map data holds at most `VERTEX_BUDGET + 5 × features` positions.
pub(crate) const VERTEX_BUDGET: usize = 10_000;

/// Pixels the page's extent is taken to span at the initial zoom. `map.js`
/// fits the page into a panel of about 500 × 340 CSS px or less, with 45 px
/// of padding, so the real span is usually smaller and the tolerance errs
/// fine.
const FIT_PIXELS: f64 = 512.0;

/// `map.js` caps a feature page's initial `fitBounds` at zoom 5, so a pixel
/// there is the finest a small page is first drawn at.
const MAX_INITIAL_ZOOM: i32 = 5;

/// MapLibre's world is `512 × 2^zoom` pixels wide.
const WORLD_PIXELS_AT_ZOOM_0: f64 = 512.0;

/// Five decimals of a degree, about 1.1 m: well below a pixel at zoom 5.
const DECIMALS: f64 = 1e5;

/// The geometry the map preview draws for each feature of a page, in page
/// order: `Value::Null` where a feature has none.
pub(crate) fn preview_geometries(features: &[Value]) -> Vec<Value> {
    simplify_page(features, tolerance(features))
}

/// The page's shapes simplified to `tol` (Web Mercator metres), in page
/// order, within [`VERTEX_BUDGET`].
fn simplify_page(features: &[Value], tol: f64) -> Vec<Value> {
    let mut used = 0;
    features
        .iter()
        .map(|feature| {
            let geometry = &feature["geometry"];
            let (mut shape, mut positions) = simplify(geometry, tol);
            if used + positions > VERTEX_BUDGET {
                // A shape no bigger than its box (a short line, a triangle)
                // is drawn as it is.
                if let Some((bbox, n)) = bbox_shape(geometry).filter(|&(_, n)| n < positions) {
                    (shape, positions) = (bbox, n);
                }
            }
            used += positions;
            shape
        })
        .collect()
}

/// About one pixel at the initial zoom, in Web Mercator metres.
fn tolerance(features: &[Value]) -> f64 {
    let world = 2.0 * PI * EARTH_RADIUS;
    let finest = world / (WORLD_PIXELS_AT_ZOOM_0 * 2f64.powi(MAX_INITIAL_ZOOM));
    let mut extent = Extent::EMPTY;
    for feature in features {
        for_each_position(&feature["geometry"], &mut |p| extent.add(mercator(p)));
    }
    if extent.is_empty() {
        return finest;
    }
    (extent.width().max(extent.height()) / FIT_PIXELS).max(finest)
}

/// `geometry` simplified to `tol`, and the positions it holds.
fn simplify(geometry: &Value, tol: f64) -> (Value, usize) {
    if geometry.is_null() {
        return (Value::Null, 0);
    }
    match geometry["type"].as_str() {
        Some("Polygon") => {
            let polygon = simplify_polygon(&parse_rings(&geometry["coordinates"]), tol);
            let n = polygon.iter().map(Vec::len).sum();
            (json!({"type": "Polygon", "coordinates": polygon}), n)
        }
        Some("MultiPolygon") => {
            let members: Vec<Vec<Vec<[f64; 2]>>> = geometry["coordinates"]
                .as_array()
                .map(|members| members.iter().map(parse_rings).collect())
                .unwrap_or_default();
            let extents: Vec<Extent> = members
                .iter()
                .map(|rings| rings.first().map_or(Extent::EMPTY, |r| Extent::of(r)))
                .collect();
            // A part smaller than a pixel cannot be seen, but the feature
            // keeps its largest part so it never vanishes from the map.
            let largest = extents
                .iter()
                .enumerate()
                .fold(None, |best: Option<(usize, f64)>, (i, e)| match best {
                    Some((_, area)) if area >= e.area() => best,
                    _ => Some((i, e.area())),
                })
                .map(|(i, _)| i);
            let polygons: Vec<Vec<Vec<[f64; 2]>>> = members
                .iter()
                .enumerate()
                .filter(|&(i, _)| Some(i) == largest || !extents[i].within(tol))
                .map(|(_, rings)| simplify_polygon(rings, tol))
                .collect();
            let n = polygons.iter().flatten().map(Vec::len).sum();
            (json!({"type": "MultiPolygon", "coordinates": polygons}), n)
        }
        Some("GeometryCollection") => {
            let mut n = 0;
            let members: Vec<Value> = geometry["geometries"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|member| {
                    let (shape, positions) = simplify(member, tol);
                    n += positions;
                    shape
                })
                .collect();
            (
                json!({"type": "GeometryCollection", "geometries": members}),
                n,
            )
        }
        Some("LineString") => {
            let line = simplify_line(&parse_positions(&geometry["coordinates"]), tol);
            let n = line.len();
            (json!({"type": "LineString", "coordinates": line}), n)
        }
        Some("MultiLineString") => {
            let lines: Vec<Vec<[f64; 2]>> = parse_rings(&geometry["coordinates"])
                .iter()
                .map(|line| simplify_line(line, tol))
                .collect();
            let n = lines.iter().map(Vec::len).sum();
            (json!({"type": "MultiLineString", "coordinates": lines}), n)
        }
        // Points: rounded.
        _ => {
            let mut n = 0;
            let coordinates = rounded_coordinates(&geometry["coordinates"], &mut n);
            (
                json!({"type": geometry["type"], "coordinates": coordinates}),
                n,
            )
        }
    }
}

/// A polygon's rings, each simplified to `tol`; holes smaller than a pixel
/// are left out.
fn simplify_polygon(rings: &[Vec<[f64; 2]>], tol: f64) -> Vec<Vec<[f64; 2]>> {
    rings
        .iter()
        .enumerate()
        .filter(|&(i, ring)| i == 0 || !Extent::of(ring).within(tol))
        .map(|(_, ring)| simplify_ring(ring, tol))
        .collect()
}

/// One closed ring simplified to `tol`, its positions rounded.
fn simplify_ring(ring: &[[f64; 2]], tol: f64) -> Vec<[f64; 2]> {
    // GeoJSON repeats the first position last; the loop is simplified
    // without it and closed again.
    let open = match ring {
        [first, .., last] if first == last => &ring[..ring.len() - 1],
        _ => ring,
    };
    let plane: Vec<(f64, f64)> = open.iter().map(|&p| mercator(p)).collect();
    let mut out: Vec<[f64; 2]> = douglas_peucker_closed(&plane, tol)
        .into_iter()
        .map(|i| round(open[i]))
        .collect();
    if let Some(&first) = out.first() {
        out.push(first);
    }
    out
}

/// One open line simplified to `tol`, its positions rounded.
fn simplify_line(line: &[[f64; 2]], tol: f64) -> Vec<[f64; 2]> {
    let plane: Vec<(f64, f64)> = line.iter().map(|&p| mercator(p)).collect();
    douglas_peucker_open(&plane, tol)
        .into_iter()
        .map(|i| round(line[i]))
        .collect()
}

/// The bounding box of `geometry` as a rectangle (a point for a single
/// position), and the positions it holds. An area's box is a polygon; a
/// line's or a point set's is its outline, so the map does not shade it.
/// The box takes the shorter way round in longitude: a feature split at the
/// antimeridian (RFC 7946 §3.1.9) gets a box from 170° to 190°, not one round
/// the whole world.
fn bbox_shape(geometry: &Value) -> Option<(Value, usize)> {
    let mut spans = Vec::new();
    lon_spans(geometry, &mut spans);
    let (west, east) = covering_lons(spans)?;
    let (mut south, mut north) = (f64::INFINITY, f64::NEG_INFINITY);
    for_each_position(geometry, &mut |[_, lat]| {
        south = south.min(lat);
        north = north.max(lat);
    });
    if west == east && south == north {
        return Some((
            json!({"type": "Point", "coordinates": round([west, south])}),
            1,
        ));
    }
    let ring = [
        [west, south],
        [east, south],
        [east, north],
        [west, north],
        [west, south],
    ]
    .map(round);
    let shape = if is_areal(geometry) {
        json!({"type": "Polygon", "coordinates": [ring]})
    } else {
        json!({"type": "LineString", "coordinates": ring})
    };
    Some((shape, 5))
}

/// Whether `geometry` covers an area.
fn is_areal(geometry: &Value) -> bool {
    match geometry["type"].as_str() {
        Some("Polygon" | "MultiPolygon") => true,
        Some("GeometryCollection") => geometry["geometries"]
            .as_array()
            .into_iter()
            .flatten()
            .any(is_areal),
        _ => false,
    }
}

/// The longitude range of each part of `geometry`: each ring, line and
/// point. GeoJSON draws an edge straight in longitude and latitude, so a
/// part covers its whole range; only separate parts can meet across the
/// antimeridian.
fn lon_spans(geometry: &Value, spans: &mut Vec<(f64, f64)>) {
    let span = |positions: &Value, spans: &mut Vec<(f64, f64)>| {
        let (mut west, mut east) = (f64::INFINITY, f64::NEG_INFINITY);
        for [lon, _] in positions
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(position)
        {
            west = west.min(lon);
            east = east.max(lon);
        }
        if west <= east {
            spans.push((west, east));
        }
    };
    let coordinates = &geometry["coordinates"];
    let members = || coordinates.as_array().into_iter().flatten();
    match geometry["type"].as_str() {
        Some("Point") => spans.extend(position(coordinates).map(|[lon, _]| (lon, lon))),
        Some("MultiPoint") => {
            spans.extend(members().filter_map(position).map(|[lon, _]| (lon, lon)))
        }
        Some("LineString") => span(coordinates, spans),
        Some("MultiLineString" | "Polygon") => members().for_each(|line| span(line, spans)),
        Some("MultiPolygon") => members()
            .flat_map(|polygon| polygon.as_array().into_iter().flatten())
            .for_each(|ring| span(ring, spans)),
        Some("GeometryCollection") => {
            for member in geometry["geometries"].as_array().into_iter().flatten() {
                lon_spans(member, spans);
            }
        }
        _ => {}
    }
}

/// The narrowest longitude range, west to east, that covers every span: the
/// circle less its widest gap between spans. When that gap is the one across
/// the antimeridian the range is the plain one; otherwise it runs east past
/// 180°. ±180° are one meridian, so a part reaching both is a band round the
/// whole world, and parts meeting at it close their gap.
fn covering_lons(mut spans: Vec<(f64, f64)>) -> Option<(f64, f64)> {
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(spans.len());
    for (west, east) in spans {
        match merged.last_mut() {
            Some(last) if west <= last.1 => last.1 = last.1.max(east),
            _ => merged.push((west, east)),
        }
    }
    let (first, last) = (*merged.first()?, *merged.last()?);
    let mut range = (first.0, last.1);
    let mut widest = first.0 + 360.0 - last.1;
    for pair in merged.windows(2) {
        let gap = pair[1].0 - pair[0].1;
        if gap > widest {
            (range, widest) = ((pair[1].0, pair[0].1 + 360.0), gap);
        }
    }
    Some(range)
}

/// Web Mercator metres, the plane MapLibre draws in. A vertex past the tile
/// grid's latitude limit is measured at the limit, where MapLibre's GeoJSON
/// tiling draws it.
fn mercator([lon, lat]: [f64; 2]) -> (f64, f64) {
    (
        lon_to_x(lon),
        lat_to_y(lat.clamp(-LAT_LIMIT_DEG, LAT_LIMIT_DEG)),
    )
}

fn round([lon, lat]: [f64; 2]) -> [f64; 2] {
    [
        (lon * DECIMALS).round() / DECIMALS,
        (lat * DECIMALS).round() / DECIMALS,
    ]
}

fn position(value: &Value) -> Option<[f64; 2]> {
    let lon = value.get(0)?.as_f64()?;
    let lat = value.get(1)?.as_f64()?;
    (lon.is_finite() && lat.is_finite()).then_some([lon, lat])
}

/// A ring's or a line's positions.
fn parse_positions(coordinates: &Value) -> Vec<[f64; 2]> {
    coordinates
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(position)
        .collect()
}

/// A polygon's rings, or a MultiLineString's lines, as positions.
fn parse_rings(coordinates: &Value) -> Vec<Vec<[f64; 2]>> {
    coordinates
        .as_array()
        .into_iter()
        .flatten()
        .map(parse_positions)
        .collect()
}

/// Every position of `geometry`, members of a collection included.
fn for_each_position(geometry: &Value, f: &mut dyn FnMut([f64; 2])) {
    fn walk(coordinates: &Value, f: &mut dyn FnMut([f64; 2])) {
        if let Some(p) = position(coordinates) {
            f(p);
        } else if let Some(items) = coordinates.as_array() {
            for item in items {
                walk(item, f);
            }
        }
    }
    walk(&geometry["coordinates"], f);
    for member in geometry["geometries"].as_array().into_iter().flatten() {
        for_each_position(member, f);
    }
}

/// `coordinates` with every position rounded and reduced to two dimensions.
fn rounded_coordinates(coordinates: &Value, n: &mut usize) -> Value {
    if let Some(p) = position(coordinates) {
        *n += 1;
        json!(round(p))
    } else if let Some(items) = coordinates.as_array() {
        Value::Array(
            items
                .iter()
                .map(|item| rounded_coordinates(item, n))
                .collect(),
        )
    } else {
        coordinates.clone()
    }
}

/// An axis-aligned extent in one plane.
#[derive(Clone, Copy)]
struct Extent {
    min: (f64, f64),
    max: (f64, f64),
}

impl Extent {
    const EMPTY: Self = Self {
        min: (f64::INFINITY, f64::INFINITY),
        max: (f64::NEG_INFINITY, f64::NEG_INFINITY),
    };

    /// The extent of a ring of lon/lat positions, in Web Mercator metres.
    fn of(ring: &[[f64; 2]]) -> Self {
        let mut extent = Self::EMPTY;
        for &p in ring {
            extent.add(mercator(p));
        }
        extent
    }

    fn add(&mut self, (x, y): (f64, f64)) {
        self.min = (self.min.0.min(x), self.min.1.min(y));
        self.max = (self.max.0.max(x), self.max.1.max(y));
    }

    fn is_empty(&self) -> bool {
        self.min.0 > self.max.0
    }

    fn width(&self) -> f64 {
        self.max.0 - self.min.0
    }

    fn height(&self) -> f64 {
        self.max.1 - self.min.1
    }

    fn area(&self) -> f64 {
        if self.is_empty() {
            0.0
        } else {
            self.width() * self.height()
        }
    }

    /// Fits within one `tol` × `tol` pixel (an empty extent does).
    fn within(&self, tol: f64) -> bool {
        self.is_empty() || (self.width() <= tol && self.height() <= tol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(geometry: Value) -> Value {
        json!({"type": "Feature", "geometry": geometry, "properties": {}})
    }

    fn point(lon: f64, lat: f64) -> Value {
        json!({"type": "Point", "coordinates": [lon, lat]})
    }

    /// A closed ring of `n` vertices around (`lon`, `lat`), alternating
    /// between radius `outer` and `inner` degrees (a circle when equal).
    fn star(lon: f64, lat: f64, outer: f64, inner: f64, n: usize) -> Vec<[f64; 2]> {
        let mut ring: Vec<[f64; 2]> = (0..n)
            .map(|k| {
                let r = if k % 2 == 0 { outer } else { inner };
                let a = 2.0 * PI * k as f64 / n as f64;
                [lon + r * a.cos(), lat + r * a.sin()]
            })
            .collect();
        ring.push(ring[0]);
        ring
    }

    fn square(lon: f64, lat: f64, side: f64) -> Vec<[f64; 2]> {
        vec![
            [lon, lat],
            [lon + side, lat],
            [lon + side, lat + side],
            [lon, lat + side],
            [lon, lat],
        ]
    }

    fn positions(geometry: &Value) -> usize {
        let mut n = 0;
        for_each_position(geometry, &mut |_| n += 1);
        n
    }

    #[test]
    fn a_dense_ring_is_simplified_and_rounded_and_points_are_kept() {
        // A municipality-sized circle sampled every few metres, on a page
        // spanning ten degrees.
        let circle = star(25.0, 62.0, 0.2, 0.2, 5_000);
        let page = [
            feature(json!({"type": "Polygon", "coordinates": [circle]})),
            feature(point(20.123456789, 60.0)),
            feature(point(30.0, 65.0)),
            feature(Value::Null),
        ];
        let out = preview_geometries(&page);
        let ring = out[0]["coordinates"][0].as_array().unwrap();
        assert!((4..100).contains(&ring.len()), "{} positions", ring.len());
        assert_eq!(ring.first(), ring.last());
        for value in ring.iter().flat_map(|p| p.as_array().unwrap()) {
            let v = value.as_f64().unwrap();
            assert_eq!((v * DECIMALS).round() / DECIMALS, v);
        }
        assert_eq!(out[1], point(20.12346, 60.0));
        assert_eq!(out[2], point(30.0, 65.0));
        assert_eq!(out[3], Value::Null);
    }

    #[test]
    fn lines_are_simplified_and_keep_their_ends() {
        // A track sampled every 0.0001° along a gentle arc, on a page
        // spanning ten degrees: its few kilometres of bend fit a handful of
        // positions.
        let track: Vec<[f64; 2]> = (0..=10_000)
            .map(|k| {
                let t = k as f64 / 10_000.0;
                [20.0 + t, 60.0 + 0.1 * (PI * t).sin()]
            })
            .collect();
        let page = [
            feature(json!({"type": "LineString", "coordinates": track})),
            feature(
                json!({"type": "MultiLineString", "coordinates": [track, [[29.0, 69.0], [30.0, 70.0]]]}),
            ),
        ];
        let out = preview_geometries(&page);
        let line = out[0]["coordinates"].as_array().unwrap();
        assert!((2..50).contains(&line.len()), "{} positions", line.len());
        assert_eq!(line.first(), Some(&json!([20.0, 60.0])));
        assert_eq!(line.last(), Some(&json!([21.0, 60.0])));
        assert_eq!(out[1]["type"], "MultiLineString");
        assert_eq!(out[1]["coordinates"][0], out[0]["coordinates"]);
        assert_eq!(
            out[1]["coordinates"][1],
            json!([[29.0, 69.0], [30.0, 70.0]])
        );
    }

    #[test]
    fn the_tolerance_follows_the_page_extent() {
        let circle = feature(json!({"type": "Polygon",
            "coordinates": [star(25.0, 62.0, 0.2, 0.2, 5_000)]}));
        let local = preview_geometries(std::slice::from_ref(&circle));
        let world = preview_geometries(&[
            circle,
            feature(point(-170.0, -60.0)),
            feature(point(170.0, 70.0)),
        ]);
        assert!(positions(&world[0]) < positions(&local[0]));
    }

    #[test]
    fn sub_pixel_holes_and_parts_are_left_out_but_the_largest_part_stays() {
        // A page spanning ten degrees: a pixel is a few kilometres, the
        // 0.001° (about 100 m) squares are well below it.
        let page = [
            feature(json!({"type": "MultiPolygon", "coordinates": [
                [square(20.0, 60.0, 1.0), square(20.5, 60.5, 0.001)],
                [square(22.0, 60.0, 0.001)],
            ]})),
            feature(json!({"type": "MultiPolygon", "coordinates": [
                [square(25.0, 62.0, 0.001)],
                [square(26.0, 62.0, 0.002)],
            ]})),
            feature(point(30.0, 70.0)),
        ];
        let out = preview_geometries(&page);
        let first = out[0]["coordinates"].as_array().unwrap();
        assert_eq!(first.len(), 1, "the island is dropped");
        assert_eq!(first[0].as_array().unwrap().len(), 1, "the hole is dropped");
        assert_eq!(
            out[1]["coordinates"],
            json!([[[
                [26.0, 62.0],
                [26.002, 62.0],
                [26.002, 62.002],
                [26.0, 62.002],
                [26.0, 62.0]
            ]]]),
            "an all-tiny feature keeps its largest part"
        );
    }

    #[test]
    fn past_the_budget_a_feature_is_drawn_as_its_bbox() {
        // At tolerance zero no vertex of a star can go: 2 001 positions
        // each, so four shapes fit the budget and the rest become boxes.
        let page: Vec<Value> = (0..20)
            .map(|i| {
                feature(json!({"type": "Polygon",
                    "coordinates": [star(i as f64, 0.0, 0.4, 0.2, 2_000)]}))
            })
            .collect();
        let out = simplify_page(&page, 0.0);
        assert_eq!(positions(&out[4]), 5);
        assert_eq!(positions(&out[0]), 2_001);
        assert_eq!(positions(&out[3]), 2_001);
        let total: usize = out.iter().map(positions).sum();
        assert!(total <= VERTEX_BUDGET + 5 * page.len(), "{total}");
        assert_eq!(
            out[19]["coordinates"],
            json!([[
                [18.6, -0.4],
                [19.4, -0.4],
                [19.4, 0.4],
                [18.6, 0.4],
                [18.6, -0.4]
            ]])
        );
    }

    #[test]
    fn past_the_budget_only_a_box_smaller_than_the_shape_replaces_it() {
        // At tolerance zero, stars of 2 001, 2 001, 2 001, 2 001 and 1 996
        // positions fill the budget exactly.
        let star_feature = |i: usize, n: usize| {
            feature(json!({"type": "Polygon",
                "coordinates": [star(i as f64, 0.0, 0.4, 0.2, n)]}))
        };
        let mut page: Vec<Value> = (0..4).map(|i| star_feature(i, 2_000)).collect();
        page.push(star_feature(4, 1_995));
        let segment = json!({"type": "LineString", "coordinates": [[10.0, 0.0], [11.0, 1.0]]});
        let triangle = json!({"type": "Polygon",
            "coordinates": [[[12.0, 0.0], [13.0, 0.0], [12.5, 1.0], [12.0, 0.0]]]});
        let zigzag: Vec<[f64; 2]> = (0..100)
            .map(|k| [14.0 + k as f64 * 0.01, (k % 2) as f64 * 0.5])
            .collect();
        // Points either side of the antimeridian, on it at ±180° too.
        let seam = json!({"type": "MultiPoint", "coordinates": [
            [170.0, 10.0], [-170.0, 20.0], [175.0, 15.0],
            [-175.0, 12.0], [180.0, 11.0], [-180.0, 19.0],
        ]});
        page.extend(
            [
                segment.clone(),
                triangle.clone(),
                json!({"type": "LineString", "coordinates": zigzag}),
                seam,
            ]
            .map(feature),
        );
        page.push(star_feature(9, 2_000));
        let out = simplify_page(&page, 0.0);
        assert_eq!(positions(&out[3]), 2_001);
        assert_eq!(positions(&out[4]), 1_996, "the budget is full");
        assert_eq!(out[5], segment, "a segment is smaller than its box");
        assert_eq!(out[6], triangle, "so is a triangle");
        assert_eq!(
            out[7],
            json!({"type": "LineString", "coordinates": [
                [14.0, 0.0], [14.99, 0.0], [14.99, 0.5], [14.0, 0.5], [14.0, 0.0],
            ]}),
            "a line's box is an outline"
        );
        assert_eq!(
            out[8],
            json!({"type": "LineString", "coordinates": [
                [170.0, 10.0], [190.0, 10.0], [190.0, 20.0], [170.0, 20.0], [170.0, 10.0],
            ]}),
            "a point set's box is an outline, the short way round"
        );
        assert_eq!(out[9]["type"], "Polygon", "an area's box is shaded");
        assert_eq!(positions(&out[9]), 5);
    }

    #[test]
    fn a_band_along_the_antimeridian_goes_round_the_world() {
        // Edges along -180° and 180° run the whole way round: ±180° is one
        // meridian, but this one part reaches both.
        for band in [
            json!([[
                [-180.0, -60.0],
                [180.0, -60.0],
                [180.0, -90.0],
                [-180.0, -90.0],
                [-180.0, -60.0]
            ]]),
            json!([[
                [-180.0, -60.0],
                [0.0, -60.0],
                [180.0, -60.0],
                [180.0, -90.0],
                [0.0, -90.0],
                [-180.0, -90.0],
                [-180.0, -60.0]
            ]]),
        ] {
            let (shape, n) = bbox_shape(&json!({"type": "Polygon", "coordinates": band})).unwrap();
            assert_eq!(n, 5);
            assert_eq!(
                shape["coordinates"],
                json!([[
                    [-180.0, -90.0],
                    [180.0, -90.0],
                    [180.0, -60.0],
                    [-180.0, -60.0],
                    [-180.0, -90.0]
                ]])
            );
        }
    }

    #[test]
    fn a_box_across_the_antimeridian_takes_the_short_way_round() {
        let split = json!({"type": "MultiPolygon", "coordinates": [
            [[[170.0, 10.0], [180.0, 10.0], [180.0, 20.0], [170.0, 20.0], [170.0, 10.0]]],
            [[[-180.0, 10.0], [-170.0, 10.0], [-170.0, 20.0], [-180.0, 20.0], [-180.0, 10.0]]],
        ]});
        let (shape, n) = bbox_shape(&split).unwrap();
        assert_eq!(n, 5);
        assert_eq!(
            shape["coordinates"],
            json!([[
                [170.0, 10.0],
                [190.0, 10.0],
                [190.0, 20.0],
                [170.0, 20.0],
                [170.0, 10.0]
            ]])
        );
        let (shape, _) = bbox_shape(&json!({"type": "Polygon",
            "coordinates": [square(-10.0, 10.0, 20.0)]}))
        .unwrap();
        assert_eq!(shape["coordinates"][0][1], json!([10.0, 10.0]));
        let (shape, n) = bbox_shape(&point(24.9, 60.2)).unwrap();
        assert_eq!((shape, n), (point(24.9, 60.2), 1));
    }
}
