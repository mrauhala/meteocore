//! OGC API – Common Part 2 `extent` object, modeled as `serde::Serialize`
//! types so the Maps, Tiles, and Features `/collections{,/{id}}` builders share
//! a single definition and the JSON shape can't drift between `/maps/...`,
//! `/tiles/...`, and `/features/...`. ds-core never builds `serde_json::Value`
//! (architecture rule), so the API crates turn an [`Extent`] into JSON with
//! `serde_json::to_value`.
//!
//! **EDR is intentionally not a consumer.** OGC API – EDR 1.1 mandates a
//! different extent shape: string-typed vertical `interval`/`values`, a `vrs`
//! reference system, and a `temporal.values` list instead of a
//! `temporal.grid`. `api-edr` therefore keeps its own builder.

use crate::datetime::{temporal_grid, TemporalGrid};
use crate::geo::{crs84_bbox_spans, is_crs84_grid};
use crate::vertical::VerticalDimension;
use chrono::{DateTime, Utc};
use serde::Serialize;

const CRS84_URI: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
const ISO8601_TRS: &str = "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian";

/// OGC API – Common Part 2 `extent` object. At least one of the three
/// sub-extents is present whenever [`build_extent`] returns `Some`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Extent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spatial: Option<SpatialExtent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal: Option<TemporalExtent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vertical: Option<VerticalExtent>,
}

/// `extent.spatial`: a single WGS84 (CRS84) bbox plus, for geographic grids
/// only, the per-axis cell resolution.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpatialExtent {
    pub bbox: Vec<[f64; 4]>,
    pub crs: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid: Option<Vec<GridAxis>>,
}

/// One axis of the spatial `grid` resolution descriptor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GridAxis {
    #[serde(rename = "cellsCount")]
    pub cells_count: u32,
    pub resolution: f64,
}

/// `extent.temporal`: one `[start, end]` RFC 3339 interval plus the shared
/// [`TemporalGrid`] cadence descriptor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TemporalExtent {
    pub interval: Vec<[String; 2]>,
    pub trs: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid: Option<TemporalGrid>,
}

/// `extent.vertical`: numeric `[lo, hi]` interval, the level values, the unit
/// symbol, and the level coordinates. `vrs` is omitted — the only kind in use
/// (radar elevation angle) has no standard OGC vertical-CRS URI.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerticalExtent {
    pub interval: Vec<[f64; 2]>,
    pub values: Vec<f64>,
    pub unit: String,
    pub grid: VerticalGrid,
}

/// `extent.vertical.grid`: the explicit level coordinates.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerticalGrid {
    pub coordinates: Vec<f64>,
}

/// Assemble the OGC API – Common Part 2 `extent` object from the primitive
/// inputs every Map/Feature engine already exposes. Returns `None` when the
/// collection advertises no spatial, temporal, or vertical extent.
///
/// - `spatial_extent` — WGS84 bbox `[west, south, east, north]`. Callers with a
///   raster source pass its bbox; vector-only callers (Tiles, Features) pass a
///   feature bbox.
/// - `grid_size` / `native_crs` — native cell counts `[nx, ny]` and CRS label.
///   The per-axis `grid` resolution is emitted **only** for geographic
///   (lon/lat) grids: projected cells aren't degree-regular, so a single
///   CRS84-degree resolution would imply a regularity that doesn't hold. Pass
///   `grid_size = None` (e.g. vector collections) to omit the grid entirely.
/// - `times` — ascending timestamp series; empty omits the temporal extent.
/// - `vertical` — optional vertical dimension; an empty one is omitted.
pub fn build_extent(
    spatial_extent: Option<[f64; 4]>,
    grid_size: Option<[u32; 2]>,
    native_crs: &str,
    times: &[DateTime<Utc>],
    vertical: Option<&VerticalDimension>,
) -> Option<Extent> {
    let spatial = spatial_extent.map(|bbox| {
        // Grid resolution only for geographic grids with positive spans.
        // `crs84_bbox_spans` keeps the spans positive across the anti-meridian.
        let grid = grid_size.and_then(|[nx, ny]| {
            if nx > 0 && ny > 0 && is_crs84_grid(native_crs) {
                let (lon_span, lat_span) = crs84_bbox_spans(bbox);
                // Skip a degenerate (zero-span) bbox: 0.0/nx would emit
                // "resolution": 0.0, which is invalid per OGC API Common Part 2.
                if lon_span > 0.0 && lat_span > 0.0 {
                    return Some(vec![
                        GridAxis {
                            cells_count: nx,
                            resolution: lon_span / nx as f64,
                        },
                        GridAxis {
                            cells_count: ny,
                            resolution: lat_span / ny as f64,
                        },
                    ]);
                }
            }
            None
        });
        SpatialExtent {
            bbox: vec![bbox],
            crs: CRS84_URI,
            grid,
        }
    });

    // Temporal — present only when there is at least one timestamp. The
    // `grid` cadence descriptor needs two (handled by `temporal_grid`).
    let temporal = match (times.first(), times.last()) {
        (Some(first), Some(last)) => Some(TemporalExtent {
            interval: vec![[first.to_rfc3339(), last.to_rfc3339()]],
            trs: ISO8601_TRS,
            grid: temporal_grid(times),
        }),
        _ => None,
    };

    // Vertical — omitted when the dimension carries no levels: Part 2 requires
    // a non-null `interval` when the extent object is present.
    let vertical = vertical.and_then(|v| {
        v.extent().map(|(lo, hi)| VerticalExtent {
            interval: vec![[lo, hi]],
            values: v.levels.clone(),
            unit: v.unit().to_string(),
            grid: VerticalGrid {
                coordinates: v.levels.clone(),
            },
        })
    });

    if spatial.is_none() && temporal.is_none() && vertical.is_none() {
        None
    } else {
        Some(Extent {
            spatial,
            temporal,
            vertical,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vertical::{VerticalDimension, VerticalKind};
    use chrono::TimeZone;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn empty_inputs_yield_none() {
        assert!(build_extent(None, None, "CRS:84", &[], None).is_none());
    }

    #[test]
    fn spatial_only_has_no_grid_without_grid_size() {
        let e = build_extent(Some([20.0, 60.0, 30.0, 70.0]), None, "", &[], None).unwrap();
        let s = e.spatial.unwrap();
        assert_eq!(s.bbox, vec![[20.0, 60.0, 30.0, 70.0]]);
        assert_eq!(s.crs, CRS84_URI);
        assert!(s.grid.is_none());
        assert!(e.temporal.is_none() && e.vertical.is_none());
    }

    #[test]
    fn geographic_grid_emits_resolution() {
        let e = build_extent(
            Some([0.0, 0.0, 10.0, 5.0]),
            Some([10, 5]),
            "CRS:84",
            &[],
            None,
        )
        .unwrap();
        let grid = e.spatial.unwrap().grid.unwrap();
        assert_eq!(grid[0].cells_count, 10);
        assert!((grid[0].resolution - 1.0).abs() < 1e-12);
        assert_eq!(grid[1].cells_count, 5);
        assert!((grid[1].resolution - 1.0).abs() < 1e-12);
    }

    #[test]
    fn projected_grid_omits_resolution() {
        let e = build_extent(
            Some([0.0, 0.0, 10.0, 5.0]),
            Some([10, 5]),
            "EPSG:3067",
            &[],
            None,
        )
        .unwrap();
        assert!(e.spatial.unwrap().grid.is_none());
    }

    #[test]
    fn temporal_regular_series_emits_grid() {
        let times = vec![
            t("2026-06-02T00:00:00Z"),
            t("2026-06-02T01:00:00Z"),
            t("2026-06-02T02:00:00Z"),
        ];
        let e = build_extent(None, None, "", &times, None).unwrap();
        let tmp = e.temporal.unwrap();
        assert_eq!(
            tmp.interval,
            vec![[
                "2026-06-02T00:00:00+00:00".to_string(),
                "2026-06-02T02:00:00+00:00".to_string()
            ]]
        );
        assert!(matches!(tmp.grid, Some(TemporalGrid::Regular { .. })));
    }

    #[test]
    fn single_timestamp_has_no_grid() {
        let times = vec![Utc.with_ymd_and_hms(2026, 6, 2, 0, 0, 0).unwrap()];
        let e = build_extent(None, None, "", &times, None).unwrap();
        assert!(e.temporal.unwrap().grid.is_none());
    }

    #[test]
    fn vertical_extent_round_trips() {
        let vd = VerticalDimension {
            kind: VerticalKind::ElevationAngle,
            levels: vec![0.5, 1.5, 3.0],
        };
        let e = build_extent(None, None, "", &[], Some(&vd)).unwrap();
        let v = e.vertical.unwrap();
        assert_eq!(v.interval, vec![[0.5, 3.0]]);
        assert_eq!(v.values, vec![0.5, 1.5, 3.0]);
        assert_eq!(v.grid.coordinates, vec![0.5, 1.5, 3.0]);
    }
}
