use std::collections::HashMap;

use chrono::{DateTime, Utc};

/// GeoJSON-style geometry.
#[derive(Debug, Clone)]
pub enum Geometry {
    Point {
        x: f64,
        y: f64,
    },
    Polygon {
        /// Exterior ring as [lon, lat] coordinate pairs.
        exterior: Vec<[f64; 2]>,
        /// Interior rings (holes), each as [lon, lat] coordinate pairs.
        holes: Vec<Vec<[f64; 2]>>,
    },
    MultiPolygon {
        /// Each polygon is (exterior ring, holes).
        polygons: Vec<(Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>)>,
    },
}

impl Geometry {
    /// Compute the bounding box [west, south, east, north] of this geometry.
    pub fn bbox(&self) -> [f64; 4] {
        match self {
            Geometry::Point { x, y } => [*x, *y, *x, *y],
            Geometry::Polygon { exterior, .. } => ring_bbox(exterior),
            Geometry::MultiPolygon { polygons } => {
                let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
                for (ext, _) in polygons {
                    let b = ring_bbox(ext);
                    bbox[0] = bbox[0].min(b[0]);
                    bbox[1] = bbox[1].min(b[1]);
                    bbox[2] = bbox[2].max(b[2]);
                    bbox[3] = bbox[3].max(b[3]);
                }
                bbox
            }
        }
    }

    /// Compute the centroid (lon, lat) of this geometry.
    pub fn centroid(&self) -> (f64, f64) {
        match self {
            Geometry::Point { x, y } => (*x, *y),
            Geometry::Polygon { exterior, .. } => ring_centroid(exterior),
            Geometry::MultiPolygon { polygons } => {
                // Area-weighted centroid across all polygons
                let mut total_area = 0.0_f64;
                let mut cx = 0.0_f64;
                let mut cy = 0.0_f64;
                for (ext, _) in polygons {
                    let area = ring_signed_area(ext).abs();
                    let (px, py) = ring_centroid(ext);
                    total_area += area;
                    cx += px * area;
                    cy += py * area;
                }
                if total_area > 0.0 {
                    (cx / total_area, cy / total_area)
                } else {
                    // Degenerate: average of first points
                    let n = polygons.len() as f64;
                    let sx: f64 = polygons.iter().map(|(ext, _)| ext[0][0]).sum();
                    let sy: f64 = polygons.iter().map(|(ext, _)| ext[0][1]).sum();
                    (sx / n, sy / n)
                }
            }
        }
    }
}

fn ring_bbox(ring: &[[f64; 2]]) -> [f64; 4] {
    let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for &[x, y] in ring {
        bbox[0] = bbox[0].min(x);
        bbox[1] = bbox[1].min(y);
        bbox[2] = bbox[2].max(x);
        bbox[3] = bbox[3].max(y);
    }
    bbox
}

fn ring_signed_area(ring: &[[f64; 2]]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        area += ring[i][0] * ring[j][1];
        area -= ring[j][0] * ring[i][1];
    }
    area / 2.0
}

fn ring_centroid(ring: &[[f64; 2]]) -> (f64, f64) {
    let n = ring.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let area = ring_signed_area(ring);
    if area.abs() < f64::EPSILON {
        // Degenerate polygon: use simple average
        let sx: f64 = ring.iter().map(|c| c[0]).sum();
        let sy: f64 = ring.iter().map(|c| c[1]).sum();
        return (sx / n as f64, sy / n as f64);
    }
    let mut cx = 0.0;
    let mut cy = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        let cross = ring[i][0] * ring[j][1] - ring[j][0] * ring[i][1];
        cx += (ring[i][0] + ring[j][0]) * cross;
        cy += (ring[i][1] + ring[j][1]) * cross;
    }
    let factor = 1.0 / (6.0 * area);
    (cx * factor, cy * factor)
}

/// A typed property value. Keeps ds-core free of serde_json.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    String(String),
    Float(f64),
    Integer(i64),
    Bool(bool),
    Null,
}

/// A single feature with geometry and properties.
#[derive(Debug, Clone)]
pub struct Feature {
    pub id: String,
    pub geometry: Geometry,
    pub properties: HashMap<String, PropertyValue>,
}

/// A page of features with pagination metadata.
#[derive(Debug, Clone)]
pub struct FeaturePage {
    pub features: Vec<Feature>,
    pub number_matched: usize,
    pub number_returned: usize,
    pub next_offset: Option<usize>,
}

/// Bounding box: west, south, east, north.
#[derive(Debug, Clone, Copy)]
pub struct Bbox {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

impl Bbox {
    pub fn new(west: f64, south: f64, east: f64, north: f64) -> Result<Self, String> {
        for v in [west, south, east, north] {
            if v.is_nan() || v.is_infinite() {
                return Err("bbox coordinates must be finite numbers".into());
            }
        }
        if west < -180.0 || east > 180.0 || south < -90.0 || north > 90.0 {
            return Err("bbox coordinates out of range (lon: -180..180, lat: -90..90)".into());
        }
        if west > east {
            return Err("bbox west must be <= east".into());
        }
        if south > north {
            return Err("bbox south must be <= north".into());
        }
        Ok(Self {
            west,
            south,
            east,
            north,
        })
    }

    /// Check if a point falls within this bbox.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.west && x <= self.east && y >= self.south && y <= self.north
    }

    /// Check if this bbox intersects another bbox (as [west, south, east, north]).
    pub fn intersects_bbox(&self, other: &[f64; 4]) -> bool {
        self.west <= other[2]
            && self.east >= other[0]
            && self.south <= other[3]
            && self.north >= other[1]
    }
}

/// Query parameters for feature retrieval.
#[derive(Debug, Clone)]
pub struct FeatureQuery {
    pub bbox: Option<Bbox>,
    pub limit: usize,
    pub offset: usize,
    pub datetime: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

impl Default for FeatureQuery {
    fn default() -> Self {
        Self {
            bbox: None,
            limit: 100,
            offset: 0,
            datetime: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_valid() {
        let bbox = Bbox::new(24.0, 60.0, 25.0, 61.0).unwrap();
        assert!(bbox.contains(24.5, 60.5));
        assert!(!bbox.contains(23.0, 60.5));
    }

    #[test]
    fn bbox_rejects_nan() {
        assert!(Bbox::new(f64::NAN, 60.0, 25.0, 61.0).is_err());
    }

    #[test]
    fn bbox_rejects_infinity() {
        assert!(Bbox::new(f64::INFINITY, 60.0, 25.0, 61.0).is_err());
    }

    #[test]
    fn bbox_rejects_reversed_lon() {
        assert!(Bbox::new(25.0, 60.0, 24.0, 61.0).is_err());
    }

    #[test]
    fn bbox_rejects_reversed_lat() {
        assert!(Bbox::new(24.0, 61.0, 25.0, 60.0).is_err());
    }

    #[test]
    fn bbox_rejects_out_of_range() {
        assert!(Bbox::new(-200.0, 60.0, 25.0, 61.0).is_err());
        assert!(Bbox::new(24.0, -100.0, 25.0, 61.0).is_err());
    }
}
