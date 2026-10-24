//! Picking the features a map layer draws at one pixel: what WMS
//! GetFeatureInfo answers on a layer that draws features (alert areas,
//! storm-cell outlines, lightning strikes) rather than a parameter's values
//! (#1100).
//!
//! The API layer describes the picked pixel of the map it shows
//! ([`FeaturePick`]); the engine selects what its map draws there, by the
//! rule its render uses, and returns domain types ([`PickedFeatures`]),
//! never JSON or markup. [`crate::map_engine::MapEngine::layer_picks_features`]
//! and [`crate::map_engine::MapEngine::pick_features`] are the trait side.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::feature::{Geometry, PropertyValue};
use crate::map_engine::OutputCrs;

/// The map a pixel is picked on: the grid `get_raster_tile` draws it on, a
/// `width` × `height` map of `output_crs` over `wgs84_bbox` (the
/// `get_raster_tile` arguments). A layer that draws symbols wider than a
/// pixel grows a pick by its symbol on this grid
/// ([`FeaturePick::grown_ring`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PickMap<'a> {
    pub output_crs: &'a OutputCrs,
    pub wgs84_bbox: [f64; 4],
    pub width: u32,
    pub height: u32,
}

/// The pixel of a map a WMS GetFeatureInfo picks on a layer that draws
/// features, and the frame the map draws the layer at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeaturePick<'a> {
    /// The layer's parameter; `None` is the collection's default.
    pub parameter: Option<&'a str>,
    /// The map the pixel is on.
    pub map: PickMap<'a>,
    /// The picked pixel `(I, J)`: its column, and its row from the top.
    pub pixel: (u32, u32),
    /// The pixel's centre, `(lon, lat)`, its longitude in −180..=180.
    pub point: (f64, f64),
    /// The timestep the map draws the layer at, as
    /// [`crate::map_engine::MapEngine::resolve_parameter_time`] resolved it.
    pub time: Option<DateTime<Utc>>,
    /// The model run the map draws, for an engine with runs.
    pub reference_time: Option<DateTime<Utc>>,
    /// The `ELEVATION` the layer takes, `None` without one.
    pub z: Option<f64>,
    /// The most features to return, most relevant first; the rest are only
    /// counted ([`PickedFeatures::number_matched`]). At least 1.
    pub limit: usize,
}

impl FeaturePick<'_> {
    /// Whether the map draws `geometry` at the picked pixel: a polygon that
    /// contains the pixel's centre (its boundary included, holes excluded),
    /// or a point inside the pixel's footprint. A geometry stored with
    /// longitudes past ±180 is tested a turn east and west too, where a map
    /// draws its world copies. A null geometry is never drawn.
    pub fn hits(&self, geometry: &Geometry) -> bool {
        const TURNS: [f64; 3] = [0.0, 360.0, -360.0];
        let (lon, lat) = self.point;
        match geometry {
            Geometry::Point { x, y } => {
                let footprint = Geometry::Polygon {
                    exterior: self.grown_ring(0, |_| 0),
                    holes: Vec::new(),
                };
                TURNS.iter().any(|k| footprint.contains(x + k, *y))
            }
            Geometry::Polygon { .. } | Geometry::MultiPolygon { .. } => {
                TURNS.iter().any(|k| geometry.contains(lon - k, lat))
            }
            Geometry::Null => false,
        }
    }

    /// The pixel's ground area: its corners' `(lon, lat)`, top left, top
    /// right, bottom right, bottom left, mapped as [`Self::corner`] maps
    /// them.
    pub fn footprint(&self) -> [[f64; 2]; 4] {
        [(0, 0), (1, 0), (1, 1), (0, 1)].map(|offset| self.corner(offset))
    }

    /// The ground position of the pixel-grid corner `(dx, dy)` away from
    /// the picked pixel's top-left corner, columns right and rows down (any
    /// integers: a corner may lie off the map). Mapped as
    /// [`OutputCrs::project_node`] maps every node a map is rendered with,
    /// its longitude taken within half a turn of [`Self::point`]'s, so the
    /// corners stay continuous across the antimeridian and may pass ±180. A
    /// corner off the Earth is clamped to the pole past one, or, outside a
    /// projection's valid area while the centre is inside, replaced by the
    /// centre.
    pub fn corner(&self, (dx, dy): (i64, i64)) -> [f64; 2] {
        let map = &self.map;
        let fx = (i64::from(self.pixel.0) + dx) as f64 / f64::from(map.width);
        let fy = (i64::from(self.pixel.1) + dy) as f64 / f64::from(map.height);
        let (lon, lat) = map.output_crs.project_node(map.wgs84_bbox, fx, fy);
        let (centre_lon, centre_lat) = self.point;
        if !(lon.is_finite() && lat.is_finite()) {
            return [centre_lon, centre_lat];
        }
        [
            centre_lon + crate::geo::wrap_lon(lon - centre_lon),
            lat.clamp(-90.0, 90.0),
        ]
    }

    /// The ground area of the pixels a symbol drawn around its own pixel
    /// reaches the picked pixel from, as a closed ring: the pixels `dy` rows
    /// above or below it for `dy` in `-radius..=radius`, and on each row the
    /// `half_width(dy)` pixels left and right of its column. A layer that
    /// draws each feature as a symbol grows its pick by the symbol's own
    /// shape this way, so a click anywhere on a drawn symbol finds it.
    /// `radius` 0 with `half_width` 0 is the pixel's footprint. The outline
    /// follows the rows' pixel edges, each corner mapped by [`Self::corner`].
    pub fn grown_ring(&self, radius: u32, half_width: impl Fn(i64) -> i64) -> Vec<[f64; 2]> {
        let radius = i64::from(radius);
        let rows: Vec<(i64, i64)> = (-radius..=radius).map(|dy| (dy, half_width(dy))).collect();
        let mut offsets: Vec<(i64, i64)> = Vec::with_capacity(4 * rows.len() + 1);
        // Down the right edges, then up the left edges.
        for &(dy, w) in &rows {
            offsets.extend([(w + 1, dy), (w + 1, dy + 1)]);
        }
        for &(dy, w) in rows.iter().rev() {
            offsets.extend([(-w, dy + 1), (-w, dy)]);
        }
        offsets.dedup();
        // From the top row's top-left corner, as `footprint` lists them.
        offsets.rotate_right(1);
        offsets.push(offsets[0]);
        offsets
            .into_iter()
            .map(|offset| self.corner(offset))
            .collect()
    }

    /// `ring`, then, when it passes ±180, its copy a turn back into
    /// −180..=180: together they hold every point a store whose longitudes
    /// lie in −180..=180 has inside the ring.
    pub fn with_turn_copies(ring: Vec<[f64; 2]>) -> Vec<Vec<[f64; 2]>> {
        let shifted = |k: f64| ring.iter().map(|&[lon, lat]| [lon + k, lat]).collect();
        let east = ring.iter().any(|[lon, _]| *lon > 180.0);
        let west = ring.iter().any(|[lon, _]| *lon < -180.0);
        let mut rings = Vec::with_capacity(3);
        if east {
            rings.push(shifted(-360.0));
        }
        if west {
            rings.push(shifted(360.0));
        }
        rings.insert(0, ring);
        rings
    }
}

/// One feature a layer draws at a picked pixel: its id and geometry as the
/// collection's Features item has them, and the properties a reader of the
/// pick wants, in the order the engine lists them (Features item names and
/// values).
#[derive(Debug, Clone)]
pub struct PickedFeature {
    pub id: String,
    pub geometry: Arc<Geometry>,
    pub properties: Vec<(String, PropertyValue)>,
}

/// What a layer draws at a picked pixel: at most [`FeaturePick::limit`]
/// features, most relevant first, and how many there are in all. No feature
/// there is an empty answer, not an error.
#[derive(Debug, Clone, Default)]
pub struct PickedFeatures {
    pub features: Vec<PickedFeature>,
    pub number_matched: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pick of pixel `pixel` of a `width` × `height` map over `bbox`,
    /// centred where the API layer places it.
    fn pick_on(
        output_crs: &OutputCrs,
        bbox: [f64; 4],
        (width, height): (u32, u32),
        pixel: (u32, u32),
    ) -> FeaturePick<'_> {
        let fx = (f64::from(pixel.0) + 0.5) / f64::from(width);
        let fy = (f64::from(pixel.1) + 0.5) / f64::from(height);
        let (lon, lat) = output_crs.project_node(bbox, fx, fy);
        FeaturePick {
            parameter: None,
            map: PickMap {
                output_crs,
                wgs84_bbox: bbox,
                width,
                height,
            },
            pixel,
            point: (crate::geo::wrap_lon(lon), lat),
            time: None,
            reference_time: None,
            z: None,
            limit: 10,
        }
    }

    fn square(west: f64, south: f64, east: f64, north: f64) -> Geometry {
        Geometry::Polygon {
            exterior: vec![
                [west, south],
                [east, south],
                [east, north],
                [west, north],
                [west, south],
            ],
            holes: Vec::new(),
        }
    }

    /// A CRS:84 map's pixel is the box its edges bound, linear in lon/lat.
    #[test]
    fn a_geographic_pixel_is_its_box() {
        let pick = pick_on(
            &OutputCrs::Wgs84,
            [10.0, 50.0, 30.0, 60.0],
            (20, 10),
            (3, 2),
        );
        assert_eq!(pick.point, (13.5, 57.5));
        assert_eq!(
            pick.footprint(),
            [[13.0, 58.0], [14.0, 58.0], [14.0, 57.0], [13.0, 57.0]]
        );
    }

    /// A Web Mercator pixel spans equal Mercator metres: its latitudes are
    /// `web_mercator`'s, not linear in degrees.
    #[test]
    fn a_web_mercator_pixel_spans_mercator_rows() {
        use crate::web_mercator::{lat_to_y, y_to_lat};
        let (top, bottom) = (lat_to_y(60.0), lat_to_y(0.0));
        let row = |j: f64| y_to_lat(top - j / 4.0 * (top - bottom));
        let pick = pick_on(
            &OutputCrs::WebMercator,
            [0.0, 0.0, 10.0, 60.0],
            (4, 4),
            (0, 1),
        );
        let footprint = pick.footprint();
        assert_eq!(footprint[0], [0.0, row(1.0)]);
        assert_eq!(footprint[2], [2.5, row(2.0)]);
    }

    /// A pixel on the antimeridian keeps continuous longitudes, past 180,
    /// and its rings include the copy a turn back.
    #[test]
    fn a_pixel_on_the_antimeridian_is_continuous() {
        // A CRS:84 map from 170°E to 190°E, 20 px wide: pixel 10 spans
        // 180..181, whose centre wraps to -179.5.
        let pick = pick_on(
            &OutputCrs::Wgs84,
            [170.0, 10.0, 190.0, 20.0],
            (20, 10),
            (10, 0),
        );
        assert_eq!(pick.point, (-179.5, 19.5));
        assert_eq!(pick.footprint()[0], [-180.0, 20.0]);
        assert_eq!(pick.footprint()[1], [-179.0, 20.0]);

        let pick = pick_on(
            &OutputCrs::Wgs84,
            [170.0, 10.0, 190.0, 20.0],
            (20, 10),
            (9, 0),
        );
        assert_eq!(pick.point, (179.5, 19.5));
        assert_eq!(pick.footprint()[1], [180.0, 20.0]);

        // A pixel across 180, 179.75..180.25: the ring passes 180, and its
        // copy a turn back is -180.25..-179.75.
        let pick = pick_on(
            &OutputCrs::Wgs84,
            [170.25, 10.0, 190.25, 20.0],
            (40, 20),
            (19, 1),
        );
        let rings = FeaturePick::with_turn_copies(pick.grown_ring(0, |_| 0));
        assert_eq!(rings.len(), 2);
        assert_eq!(rings[0][..2], [[179.75, 19.5], [180.25, 19.5]]);
        assert_eq!(rings[1][..2], [[-180.25, 19.5], [-179.75, 19.5]]);
        // Strikes stored at 179.8 and -179.8 (180.2) are both in it.
        assert!(pick.hits(&Geometry::Point { x: 179.8, y: 19.2 }));
        assert!(pick.hits(&Geometry::Point { x: -179.8, y: 19.2 }));
        assert!(!pick.hits(&Geometry::Point { x: -179.7, y: 19.2 }));
    }

    /// A corner past a pole is clamped to it; one off a projection's area
    /// takes the centre.
    #[test]
    fn corners_off_the_earth_stay_on_it() {
        let mut pick = pick_on(&OutputCrs::Wgs84, [0.0, 80.0, 10.0, 100.0], (10, 2), (0, 0));
        pick.point = (0.5, 89.0);
        assert_eq!(pick.footprint()[0], [0.0, 90.0]);

        // EPSG:3035's inverse has no answer past its disk.
        let laea = OutputCrs::Projected {
            crs: crate::geo::projected_output_crs("EPSG:3035").unwrap(),
            bbox: [-1.0e8, -1.0e8, 1.0e8, 1.0e8],
        };
        let mut pick = pick_on(&laea, [f64::NAN; 4], (1, 1), (0, 0));
        pick.point = (1.0, 2.0);
        assert!(pick.footprint().iter().all(|&corner| corner == [1.0, 2.0]));
    }

    /// A pick grown by a symbol holds the pixels the symbol reaches the
    /// picked pixel from: here a disc of radius 2, 13 pixels, on 1° pixels.
    #[test]
    fn a_grown_ring_holds_the_symbols_pixels() {
        let disc = |dy: i64| ((4 - dy * dy) as f64).sqrt().floor() as i64;
        let pick = pick_on(&OutputCrs::Wgs84, [0.0, 0.0, 10.0, 10.0], (10, 10), (5, 5));
        let region = Geometry::Polygon {
            exterior: pick.grown_ring(2, disc),
            holes: Vec::new(),
        };
        for dy in -3..=3i64 {
            for dx in -3..=3i64 {
                // The centre of the pixel (dx, dy) away from (5, 5).
                let (lon, lat) = (5.5 + dx as f64, 4.5 - dy as f64);
                assert_eq!(
                    region.contains(lon, lat),
                    dx * dx + dy * dy <= 4,
                    "({dx}, {dy})"
                );
            }
        }
        assert_eq!(pick.grown_ring(0, |_| 0), {
            let [a, b, c, d] = pick.footprint();
            vec![a, b, c, d, a]
        });
    }

    /// A polygon is drawn at the pixel when it contains the centre, a point
    /// when it lies in the footprint, each also a turn east and west.
    #[test]
    fn hits_follow_what_the_map_draws() {
        // One 0.2° pixel around (25, 60).
        let pick = FeaturePick {
            point: (25.0, 60.0),
            ..pick_on(&OutputCrs::Wgs84, [24.9, 59.9, 25.1, 60.1], (1, 1), (0, 0))
        };
        assert!(pick.hits(&square(24.0, 59.0, 26.0, 61.0)));
        assert!(!pick.hits(&square(25.05, 59.0, 26.0, 61.0)));
        // The boundary is inside.
        assert!(pick.hits(&square(25.0, 59.0, 26.0, 61.0)));
        // A hole around the centre is not.
        let holed = Geometry::Polygon {
            exterior: match square(24.0, 59.0, 26.0, 61.0) {
                Geometry::Polygon { exterior, .. } => exterior,
                _ => unreachable!(),
            },
            holes: vec![match square(24.95, 59.95, 25.05, 60.05) {
                Geometry::Polygon { exterior, .. } => exterior,
                _ => unreachable!(),
            }],
        };
        assert!(!pick.hits(&holed));
        assert!(pick.hits(&Geometry::MultiPolygon {
            polygons: vec![
                (
                    vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]],
                    Vec::new()
                ),
                (
                    vec![[24.0, 59.0], [26.0, 59.0], [26.0, 61.0], [24.0, 59.0]],
                    Vec::new()
                ),
            ],
        }));
        // A polygon stored past 180 (385 = 25 a turn east).
        assert!(pick.hits(&square(384.0, 59.0, 386.0, 61.0)));
        // Points: inside the footprint, past its edge, and a turn away.
        assert!(pick.hits(&Geometry::Point { x: 25.09, y: 59.95 }));
        assert!(!pick.hits(&Geometry::Point { x: 25.11, y: 60.0 }));
        assert!(pick.hits(&Geometry::Point { x: -335.0, y: 60.0 }));
        assert!(!pick.hits(&Geometry::Null));
        assert_eq!(
            FeaturePick::with_turn_copies(pick.grown_ring(0, |_| 0)).len(),
            1
        );
    }
}
