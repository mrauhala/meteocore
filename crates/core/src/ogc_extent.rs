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
use crate::geo::{crs84_bbox_spans, crs84_extent, is_crs84_grid, native_crs_uri};
use crate::native_grid::{registered_crs, NativeGrid};
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

/// `extent.spatial`: a single WGS84 (CRS84) bbox, the extent and per-axis
/// grid in the storage CRS where the standard can say them, and the
/// `meteocore:nativeGrid` extension member (#1237).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpatialExtent {
    pub bbox: Vec<[f64; 4]>,
    pub crs: &'static str,
    /// The extent in the `storageCrs`, in its axis order: the outer cell
    /// edges of a registered native grid (`/req/collection-map/desc-crs`
    /// C, `/rec/collections/description-extent-storage-crs-bbox`).
    #[serde(rename = "storageCrsBbox", skip_serializing_if = "Option::is_none")]
    pub storage_crs_bbox: Option<Vec<[f64; 4]>>,
    /// One axis per entry, in the units and axis order of the
    /// `storageCrs` (CRS84 without one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid: Option<Vec<GridAxis>>,
    /// The native grid in its own CRS, for every grid that is not lat/lon,
    /// registered or not; see [`NativeGridMember`].
    #[serde(
        rename = "meteocore:nativeGrid",
        skip_serializing_if = "Option::is_none"
    )]
    pub native_grid: Option<NativeGridMember>,
}

/// One axis of the spatial `grid` resolution descriptor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GridAxis {
    #[serde(rename = "cellsCount")]
    pub cells_count: u32,
    pub resolution: f64,
    /// Required by the Common regular-grid schema, which permits null. A
    /// CRS84 grid built from bounds and cell counts leaves it null: they do
    /// not establish sample registration (centre vs edge) or axis
    /// direction. A registered native grid gives its first cell centre,
    /// the axis minimum plus half a cell.
    #[serde(rename = "firstCoordinate")]
    pub first_coordinate: Option<f64>,
    /// The cell's bounds relative to its coordinate, `[-res/2, res/2]` for
    /// a cell-centred native grid: the registration stated, not implied.
    #[serde(rename = "relativeBounds", skip_serializing_if = "Option::is_none")]
    pub relative_bounds: Option<[f64; 2]>,
}

/// `extent.spatial["meteocore:nativeGrid"]`, a MeteoCore extension member
/// (#1237): the grid the data lies on, in its own CRS, for every grid that
/// is not lat/lon.
///
/// The standard `grid` can only describe a grid in the collection's
/// `storageCrs`, and OGC API – Maps rules out advertising a CRS MeteoCore
/// cannot render as one: `storageCrs` "SHALL be the URI or the safe CURIE"
/// (`/req/collection-map/desc-crs` B) and the map response "SHALL be in the
/// storage CRS" by default (`/req/core/map-response` B). This member says
/// the same for any CRS without touching `storageCrs` or `crs`.
///
/// - `crs`: the OGC URI of the EPSG code the source states, else the WKT2
///   (ISO 19162:2019) of the CRS ([`crate::native_grid::crs_wkt2`]).
/// - `cellsCount` `[nx, ny]`, `resolution` `[dx, dy]` in `unit` (`m`, or
///   `deg` for rotated lat/lon) and `bbox` `[min_x, min_y, max_x, max_y]`,
///   the cells' outer edges: all in grid order, x (columns, easting) then
///   y (rows, northing), whatever axis order a registered `crs` defines.
///   Cell centres lie half a cell inside the edges.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NativeGridMember {
    pub crs: String,
    #[serde(rename = "cellsCount")]
    pub cells_count: [u32; 2],
    pub resolution: [f64; 2],
    pub unit: &'static str,
    pub bbox: [f64; 4],
}

impl NativeGridMember {
    fn of(grid: &NativeGrid) -> Self {
        Self {
            crs: grid.crs_ref().to_owned(),
            cells_count: grid.size(),
            resolution: grid.spacing(),
            unit: grid.unit(),
            bbox: grid.bbox(),
        }
    }
}

/// The standard `storageCrsBbox` and `grid` of a native grid whose
/// source-stated EPSG code is the collection's `storageCrs`, in that CRS's
/// axis order, cell-centred: `firstCoordinate` is the axis minimum plus
/// half a cell, `relativeBounds` the half cell either side, and
/// `storageCrsBbox` the outer edges, so `cellsCount` = extent / resolution
/// as `/req/uad-collections/grid-description` C gives it for area cells.
/// `None` for any other grid: the standard `grid` is in `storageCrs` units.
fn storage_grid(grid: &NativeGrid, storage_crs: Option<&str>) -> Option<([f64; 4], Vec<GridAxis>)> {
    let uri = grid.authority_uri()?;
    if storage_crs != Some(uri) {
        return None;
    }
    let registered = registered_crs(uri)?;
    let [x0, y0, x1, y1] = grid.bbox();
    let [nx, ny] = grid.size();
    let [dx, dy] = grid.spacing();
    let axis = |cells_count: u32, min: f64, resolution: f64| GridAxis {
        cells_count,
        resolution,
        first_coordinate: Some(min + resolution / 2.0),
        relative_bounds: Some([-resolution / 2.0, resolution / 2.0]),
    };
    let (x, y) = (axis(nx, x0, dx), axis(ny, y0, dy));
    Some(if registered.northing_first {
        ([y0, x0, y1, x1], vec![y, x])
    } else {
        ([x0, y0, x1, y1], vec![x, y])
    })
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

/// `extent.vertical`, a Common Part 2 Uniform Additional Dimension: a numeric
/// `[lo, hi]` interval, its vertical reference system, the level values, the
/// unit symbol and the irregular level grid. UAD requires exactly one of
/// `definition`, `trs` or `vrs` beside `interval`; `vrs` is the dimension's
/// [`VerticalKind::vrs`](crate::vertical::VerticalKind::vrs), a registered
/// URI or an inline WKT2 CRS when none is registered (as EDR advertises).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerticalExtent {
    pub interval: Vec<[f64; 2]>,
    pub vrs: &'static str,
    pub values: Vec<f64>,
    pub unit: String,
    pub grid: VerticalGrid,
}

/// `extent.vertical.grid`: an irregular grid of the explicit level
/// coordinates. Common Part 2 grids require `cellsCount`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VerticalGrid {
    #[serde(rename = "cellsCount")]
    pub cells_count: usize,
    pub coordinates: Vec<f64>,
}

/// Assemble the OGC API – Common Part 2 `extent` object from the primitive
/// inputs every Map/Feature engine already exposes. Returns `None` when the
/// collection advertises no spatial, temporal, or vertical extent.
///
/// - `spatial_extent` — WGS84 bbox `[west, south, east, north]`. Callers with a
///   raster source pass its bbox; vector-only callers (Tiles, Features) pass a
///   feature bbox. It is normalized to the CRS84 domain
///   ([`crs84_extent`]); a box describing no area is omitted.
/// - `grid_size` / `native_crs` — native cell counts `[nx, ny]` and CRS label.
///   A CRS84 grid (label `CRS:84`) gets the per-axis `grid` in degrees from
///   the bbox. A projected grid gets no degree resolution: its cells aren't
///   degree-regular, so a CRS84-degree resolution would imply a regularity
///   that doesn't hold. Pass `grid_size = None` (e.g. vector collections)
///   to omit the grid entirely.
/// - `native_grid` — the engine's [`NativeGrid`], when it reports one
///   (#1237). When the source states an EPSG code and it is the
///   `storageCrs` the label already advertises ([`native_crs_uri`]), the
///   extent gains the standard `storageCrsBbox` and a `grid` in that CRS
///   ([`storage_grid`]). Every grid that is not lat/lon also gets the
///   `meteocore:nativeGrid` member ([`NativeGridMember`]); a lat/lon grid is
///   the CRS84 `grid`'s to describe. The `storageCrs` itself never changes.
/// - `times` — ascending timestamp series; empty omits the temporal extent.
/// - `vertical` — optional vertical dimension; an empty one is omitted.
pub fn build_extent(
    spatial_extent: Option<[f64; 4]>,
    grid_size: Option<[u32; 2]>,
    native_crs: &str,
    native_grid: Option<&NativeGrid>,
    times: &[DateTime<Utc>],
    vertical: Option<&VerticalDimension>,
) -> Option<Extent> {
    let spatial = spatial_extent.and_then(|raw| {
        let bbox = crs84_extent(raw)?;
        // Grid resolution only for geographic grids with positive spans, from
        // the raw cell-edge span (clamping at a pole would skew it).
        // `crs84_bbox_spans` keeps the spans positive across the anti-meridian.
        let grid = grid_size.and_then(|[nx, ny]| {
            if nx > 0 && ny > 0 && is_crs84_grid(native_crs) {
                let (lon_span, lat_span) = crs84_bbox_spans(raw);
                // Skip a degenerate (zero-span) bbox: 0.0/nx would emit
                // "resolution": 0.0, which is invalid per OGC API Common Part 2.
                if lon_span > 0.0 && lat_span > 0.0 {
                    return Some(vec![
                        GridAxis {
                            cells_count: nx,
                            resolution: lon_span / nx as f64,
                            first_coordinate: None,
                            relative_bounds: None,
                        },
                        GridAxis {
                            cells_count: ny,
                            resolution: lat_span / ny as f64,
                            first_coordinate: None,
                            relative_bounds: None,
                        },
                    ]);
                }
            }
            None
        });
        let storage = if grid.is_none() {
            native_grid.and_then(|g| storage_grid(g, native_crs_uri(native_crs)))
        } else {
            None
        };
        let (storage_crs_bbox, grid) = match storage {
            Some((storage_bbox, axes)) => (Some(vec![storage_bbox]), Some(axes)),
            None => (None, grid),
        };
        let native_grid = native_grid
            .filter(|g| !g.is_lon_lat())
            .map(NativeGridMember::of);
        Some(SpatialExtent {
            bbox: vec![bbox],
            crs: CRS84_URI,
            storage_crs_bbox,
            grid,
            native_grid,
        })
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
            vrs: v.kind.vrs(),
            values: v.levels.clone(),
            unit: v.unit().to_string(),
            grid: VerticalGrid {
                cells_count: v.levels.len(),
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
    use crate::geo::Crs;
    use crate::vertical::{VerticalDimension, VerticalKind};
    use chrono::TimeZone;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn empty_inputs_yield_none() {
        assert!(build_extent(None, None, "CRS:84", None, &[], None).is_none());
    }

    #[test]
    fn spatial_only_has_no_grid_without_grid_size() {
        let e = build_extent(Some([20.0, 60.0, 30.0, 70.0]), None, "", None, &[], None).unwrap();
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
            None,
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
            None,
            &[],
            None,
        )
        .unwrap();
        let spatial = e.spatial.unwrap();
        assert!(spatial.grid.is_none());
        assert!(spatial.storage_crs_bbox.is_none() && spatial.native_grid.is_none());
    }

    fn tm35fin() -> Crs {
        Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 27.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        }
    }

    /// The EPSG:3067 radar fixture's grid (testdata/radar-tm35fin): 480 ×
    /// 360 cells from (-196593.004, 6255329.699) m.
    fn radar(epsg: Option<u32>) -> NativeGrid {
        NativeGrid::new(
            tm35fin(),
            epsg,
            [480, 360],
            [-196_593.004, 6_255_329.699],
            [2_584.937_5, 5080.84],
        )
        .unwrap()
    }

    const RADAR_WGS84: [f64; 4] = [6.68, 55.93, 43.12, 72.2];

    /// #1237: a source-stated EPSG code that is the advertised storage CRS
    /// gives the standard `storageCrsBbox` and `grid` in that CRS: outer
    /// edges, cell-centred first coordinates and their half-cell bounds.
    #[test]
    fn a_registered_native_grid_is_described_in_its_storage_crs() {
        let grid = radar(Some(3067));
        let e = build_extent(
            Some(RADAR_WGS84),
            Some([480, 360]),
            "EPSG:3067",
            Some(&grid),
            &[],
            None,
        )
        .unwrap();
        let spatial = e.spatial.unwrap();
        assert_eq!(spatial.bbox, vec![RADAR_WGS84]);
        assert_eq!(spatial.crs, CRS84_URI);
        let [x0, y0, x1, y1] = grid.bbox();
        assert_eq!(spatial.storage_crs_bbox, Some(vec![[x0, y0, x1, y1]]));
        let axes = spatial.grid.unwrap();
        assert_eq!(axes[0].cells_count, 480);
        assert_eq!(axes[0].resolution, 2_584.937_5);
        assert_eq!(
            axes[0].first_coordinate,
            Some(-196_593.004 + 2_584.937_5 / 2.0)
        );
        assert_eq!(axes[0].relative_bounds, Some([-1_292.468_75, 1_292.468_75]));
        assert_eq!(axes[1].cells_count, 360);
        assert_eq!(axes[1].first_coordinate, Some(6_255_329.699 + 2540.42));
        assert_eq!(axes[1].relative_bounds, Some([-2540.42, 2540.42]));
        // `cellsCount` is the extent over the resolution: area cells.
        for (axis, (lo, hi)) in axes.iter().zip([(x0, x1), (y0, y1)]) {
            let cells = (hi - lo) / axis.resolution;
            assert!(
                (cells - f64::from(axis.cells_count)).abs() < 1e-9,
                "{cells}"
            );
        }
        // The extension member says the same, by the source's URI.
        let member = spatial.native_grid.unwrap();
        assert_eq!(member.crs, "http://www.opengis.net/def/crs/EPSG/0/3067");
        assert_eq!(member.cells_count, [480, 360]);
        assert_eq!(member.resolution, [2_584.937_5, 5080.84]);
        assert_eq!(member.unit, "m");
        assert_eq!(member.bbox, grid.bbox());
    }

    /// EPSG:3035 lists northing first: its box and grid follow.
    #[test]
    fn a_northing_first_storage_crs_orders_its_axes() {
        let laea = Crs::LambertAzimuthalEqualArea {
            lat0: 52.0_f64.to_radians(),
            lon0: 10.0_f64.to_radians(),
            false_e: 4_321_000.0,
            false_n: 3_210_000.0,
        };
        let grid = NativeGrid::new(
            laea,
            Some(3035),
            [400, 300],
            [2.0e6, 1.0e6],
            [500.0, 1000.0],
        )
        .unwrap();
        let spatial = build_extent(
            Some([-10.0, 35.0, 30.0, 70.0]),
            Some([400, 300]),
            "EPSG:3035",
            Some(&grid),
            &[],
            None,
        )
        .unwrap()
        .spatial
        .unwrap();
        assert_eq!(
            spatial.storage_crs_bbox,
            Some(vec![[1.0e6, 2.0e6, 1.3e6, 2.2e6]])
        );
        let axes = spatial.grid.unwrap();
        assert_eq!((axes[0].cells_count, axes[0].resolution), (300, 1000.0));
        assert_eq!((axes[1].cells_count, axes[1].resolution), (400, 500.0));
        // The member keeps grid order, x first.
        assert_eq!(
            spatial.native_grid.unwrap().bbox,
            [2.0e6, 1.0e6, 2.2e6, 1.3e6]
        );
    }

    /// TM35FIN parameters without a stated code are not EPSG:3067, even
    /// where the engine's label advertises that `storageCrs`: no standard
    /// grid, the member by its WKT2. A custom CRS likewise, with no
    /// `storageCrs` at all; the storage CRS is never changed.
    #[test]
    fn an_unregistered_native_grid_is_only_the_member() {
        let grid = radar(None);
        let spatial = build_extent(
            Some(RADAR_WGS84),
            Some([480, 360]),
            "EPSG:3067",
            Some(&grid),
            &[],
            None,
        )
        .unwrap()
        .spatial
        .unwrap();
        assert!(spatial.storage_crs_bbox.is_none() && spatial.grid.is_none());
        let member = spatial.native_grid.unwrap();
        assert!(member.crs.starts_with("PROJCRS[\"Transverse Mercator\""));
        // A stated code that is not the advertised storage CRS.
        let spatial = build_extent(
            Some(RADAR_WGS84),
            Some([480, 360]),
            "TM",
            Some(&radar(Some(3067))),
            &[],
            None,
        )
        .unwrap()
        .spatial
        .unwrap();
        assert!(spatial.storage_crs_bbox.is_none() && spatial.grid.is_none());
        assert_eq!(
            spatial.native_grid.unwrap().crs,
            "http://www.opengis.net/def/crs/EPSG/0/3067"
        );
        // MEPS: 949 × 1069 cells of 2.5 km on its Lambert conformal conic.
        let meps = NativeGrid::new(
            Crs::LambertConformalConic {
                lat1: 63.3_f64.to_radians(),
                lat2: 63.3_f64.to_radians(),
                lat0: 63.3_f64.to_radians(),
                lon0: 15.0_f64.to_radians(),
                false_e: 0.0,
                false_n: 0.0,
                radius: Some(6_371_000.0),
            },
            None,
            [949, 1069],
            [-1_060_084.0, -1_332_469.0],
            [2500.0, 2500.0],
        )
        .unwrap();
        let spatial = build_extent(
            Some([-18.1, 49.8, 54.2, 75.2]),
            Some([949, 1069]),
            "projected",
            Some(&meps),
            &[],
            None,
        )
        .unwrap()
        .spatial
        .unwrap();
        assert!(spatial.storage_crs_bbox.is_none() && spatial.grid.is_none());
        let member = spatial.native_grid.unwrap();
        assert_eq!(member.crs, crate::native_grid::crs_wkt2(meps.crs()));
        assert_eq!(member.cells_count, [949, 1069]);
        assert_eq!(member.resolution, [2500.0, 2500.0]);
        assert_eq!(
            member.bbox,
            [-1_060_084.0, -1_332_469.0, 1_312_416.0, 1_340_031.0]
        );
    }

    /// A lat/lon grid keeps its CRS84 grid and gets no member.
    #[test]
    fn a_lon_lat_native_grid_keeps_the_crs84_grid() {
        let grid = NativeGrid::new(Crs::Wgs84, None, [10, 5], [0.0, 0.0], [1.0, 1.0]).unwrap();
        let with = build_extent(
            Some([0.0, 0.0, 10.0, 5.0]),
            Some([10, 5]),
            "CRS:84",
            Some(&grid),
            &[],
            None,
        );
        let without = build_extent(
            Some([0.0, 0.0, 10.0, 5.0]),
            Some([10, 5]),
            "CRS:84",
            None,
            &[],
            None,
        );
        assert_eq!(with, without);
        let spatial = with.unwrap().spatial.unwrap();
        assert!(spatial.native_grid.is_none() && spatial.storage_crs_bbox.is_none());
        assert_eq!(spatial.grid.unwrap()[0].first_coordinate, None);
    }

    #[test]
    fn temporal_regular_series_emits_grid() {
        let times = vec![
            t("2026-06-02T00:00:00Z"),
            t("2026-06-02T01:00:00Z"),
            t("2026-06-02T02:00:00Z"),
        ];
        let e = build_extent(None, None, "", None, &times, None).unwrap();
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
        let e = build_extent(None, None, "", None, &times, None).unwrap();
        assert!(e.temporal.unwrap().grid.is_none());
    }

    #[test]
    fn vertical_extent_round_trips() {
        let vd = VerticalDimension {
            kind: VerticalKind::ElevationAngle,
            levels: vec![0.5, 1.5, 3.0],
        };
        let e = build_extent(None, None, "", None, &[], Some(&vd)).unwrap();
        let v = e.vertical.unwrap();
        assert_eq!(v.interval, vec![[0.5, 3.0]]);
        assert_eq!(v.values, vec![0.5, 1.5, 3.0]);
        assert_eq!(v.grid.coordinates, vec![0.5, 1.5, 3.0]);
        assert_eq!(v.grid.cells_count, 3);
        assert_eq!(v.vrs, VerticalKind::ElevationAngle.vrs());
    }

    #[test]
    fn spatial_extent_is_normalized_to_the_crs84_domain() {
        let global = build_extent(
            Some([-180.125, -90.125, 179.875, 90.125]),
            Some([1440, 721]),
            "CRS:84",
            None,
            &[],
            None,
        )
        .unwrap();
        let spatial = global.spatial.unwrap();
        assert_eq!(spatial.bbox, vec![[-180.0, -90.0, 180.0, 90.0]]);
        let grid = spatial.grid.unwrap();
        assert!((grid[0].resolution - 0.25).abs() < 1e-12);
        assert!((grid[1].resolution - 0.25).abs() < 1e-12, "raw cell span");
        // An extent that saw no valid point is not advertised at all.
        let times = vec![t("2026-06-02T00:00:00Z")];
        let sentinel = build_extent(
            Some([f64::MAX, f64::MAX, f64::MIN, f64::MIN]),
            None,
            "projected",
            None,
            &times,
            None,
        )
        .unwrap();
        assert!(sentinel.spatial.is_none());
        assert!(sentinel.temporal.is_some());
    }
}
