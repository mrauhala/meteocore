/// OGC TileMatrixSet definitions and tile coordinate math.
///
/// Defines well-known tiling schemes (WebMercatorQuad, WorldCRS84Quad) and provides
/// methods to compute bounding boxes from tile coordinates.
use std::f64::consts::PI;

/// A tiling scheme definition (OGC TileMatrixSet).
pub struct TileMatrixSetDef {
    pub id: &'static str,
    pub title: &'static str,
    /// OGC CRS URI
    pub crs: &'static str,
    pub well_known_scale_set: Option<&'static str>,
    /// Standard tile width in pixels
    pub tile_width: u32,
    /// Standard tile height in pixels
    pub tile_height: u32,
    /// Maximum zoom level (inclusive)
    pub max_zoom: u32,
}

/// A single zoom level within a TileMatrixSet.
pub struct TileMatrixInfo {
    pub scale_denominator: f64,
    pub cell_size: f64,
    pub top_left_corner: [f64; 2],
    pub tile_width: u32,
    pub tile_height: u32,
    pub matrix_width: u64,
    pub matrix_height: u64,
}

/// Supported TileMatrixSet IDs.
pub const SUPPORTED_TILE_MATRIX_SETS: &[&str] = &["WebMercatorQuad", "WorldCRS84Quad"];

// ---------------------------------------------------------------------------
// WebMercatorQuad (EPSG:3857)
// ---------------------------------------------------------------------------

/// Standard pixel size for scale denominator calculation: 0.28mm = 0.00028m
const PIXEL_SIZE_M: f64 = 0.00028;
/// Earth equatorial circumference in meters (WGS84 semi-major axis * 2 * pi)
const EARTH_CIRCUMFERENCE: f64 = 2.0 * PI * 6_378_137.0;
/// Half the earth circumference (Web Mercator extent in each direction)
const HALF_CIRCUMFERENCE: f64 = PI * 6_378_137.0;

pub static WEB_MERCATOR_QUAD: TileMatrixSetDef = TileMatrixSetDef {
    id: "WebMercatorQuad",
    title: "Google Maps Compatible for the World",
    crs: "http://www.opengis.net/def/crs/EPSG/0/3857",
    well_known_scale_set: Some("http://www.opengis.net/def/wkss/OGC/1.0/GoogleMapsCompatible"),
    tile_width: 256,
    tile_height: 256,
    max_zoom: 24,
};

// ---------------------------------------------------------------------------
// WorldCRS84Quad (CRS:84)
// ---------------------------------------------------------------------------

pub static WORLD_CRS84_QUAD: TileMatrixSetDef = TileMatrixSetDef {
    id: "WorldCRS84Quad",
    title: "CRS84 for the World",
    crs: "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
    well_known_scale_set: Some("http://www.opengis.net/def/wkss/OGC/1.0/GoogleCRS84Quad"),
    tile_width: 256,
    tile_height: 256,
    max_zoom: 24,
};

/// Look up a TileMatrixSet by ID.
pub fn get_tile_matrix_set(id: &str) -> Option<&'static TileMatrixSetDef> {
    match id {
        "WebMercatorQuad" => Some(&WEB_MERCATOR_QUAD),
        "WorldCRS84Quad" => Some(&WORLD_CRS84_QUAD),
        _ => None,
    }
}

impl TileMatrixSetDef {
    /// Get the matrix info for a given zoom level.
    pub fn matrix(&self, zoom: u32) -> Option<TileMatrixInfo> {
        if zoom > self.max_zoom {
            return None;
        }
        match self.id {
            "WebMercatorQuad" => Some(web_mercator_matrix(zoom)),
            "WorldCRS84Quad" => Some(crs84_matrix(zoom)),
            _ => None,
        }
    }

    /// Compute the WGS84 bbox [west, south, east, north] for a tile.
    ///
    /// Tile coordinates follow OGC convention: tileRow is Y (top-to-bottom),
    /// tileCol is X (left-to-right).
    pub fn tile_bbox(&self, zoom: u32, row: u64, col: u64) -> Option<[f64; 4]> {
        match self.id {
            "WebMercatorQuad" => web_mercator_tile_bbox(zoom, row, col),
            "WorldCRS84Quad" => crs84_tile_bbox(zoom, row, col),
            _ => None,
        }
    }

    /// Check if tile coordinates are within matrix bounds.
    pub fn validate_coords(&self, zoom: u32, row: u64, col: u64) -> bool {
        if zoom > self.max_zoom {
            return false;
        }
        if let Some(matrix) = self.matrix(zoom) {
            col < matrix.matrix_width && row < matrix.matrix_height
        } else {
            false
        }
    }

    /// Generate JSON representation of this TileMatrixSet (for metadata endpoint).
    pub fn to_json(&self) -> serde_json::Value {
        let mut matrices = Vec::new();
        for z in 0..=self.max_zoom.min(24) {
            if let Some(m) = self.matrix(z) {
                matrices.push(serde_json::json!({
                    "id": z.to_string(),
                    "scaleDenominator": m.scale_denominator,
                    "cellSize": m.cell_size,
                    "cornerOfOrigin": "topLeft",
                    "pointOfOrigin": m.top_left_corner,
                    "tileWidth": m.tile_width,
                    "tileHeight": m.tile_height,
                    "matrixWidth": m.matrix_width,
                    "matrixHeight": m.matrix_height,
                }));
            }
        }

        serde_json::json!({
            "id": self.id,
            "title": self.title,
            "uri": format!("http://www.opengis.net/def/tilematrixset/OGC/1.0/{}", self.id),
            "crs": self.crs,
            "wellKnownScaleSet": self.well_known_scale_set,
            "tileMatrices": matrices,
        })
    }

    /// Compute TileMatrixSetLimits for a given spatial extent [west, south, east, north].
    pub fn limits_for_extent(&self, bbox: [f64; 4], max_zoom: u32) -> Vec<serde_json::Value> {
        let effective_max = max_zoom.min(self.max_zoom);
        let mut limits = Vec::new();

        for z in 0..=effective_max {
            if let Some(matrix) = self.matrix(z) {
                let (min_col, min_row, max_col, max_row) = match self.id {
                    "WebMercatorQuad" => web_mercator_extent_to_tile_range(z, bbox, &matrix),
                    "WorldCRS84Quad" => crs84_extent_to_tile_range(z, bbox, &matrix),
                    _ => continue,
                };

                limits.push(serde_json::json!({
                    "tileMatrix": z.to_string(),
                    "minTileRow": min_row,
                    "maxTileRow": max_row,
                    "minTileCol": min_col,
                    "maxTileCol": max_col,
                }));
            }
        }

        limits
    }
}

// ---------------------------------------------------------------------------
// WebMercatorQuad math
// ---------------------------------------------------------------------------

fn web_mercator_matrix(zoom: u32) -> TileMatrixInfo {
    let n = 1u64 << zoom;
    let scale_denominator = EARTH_CIRCUMFERENCE / (256.0 * n as f64 * PIXEL_SIZE_M);
    let cell_size = EARTH_CIRCUMFERENCE / (256.0 * n as f64);
    TileMatrixInfo {
        scale_denominator,
        cell_size,
        top_left_corner: [-HALF_CIRCUMFERENCE, HALF_CIRCUMFERENCE],
        tile_width: 256,
        tile_height: 256,
        matrix_width: n,
        matrix_height: n,
    }
}

/// Compute WGS84 bbox for a WebMercatorQuad tile.
/// Row 0 is at the top (north), col 0 is at the left (west).
fn web_mercator_tile_bbox(zoom: u32, row: u64, col: u64) -> Option<[f64; 4]> {
    if zoom > 24 {
        return None;
    }
    let n = 1u64.checked_shl(zoom)?;
    if col >= n || row >= n {
        return None;
    }

    let n_f = n as f64;

    // Longitude: linear mapping from col
    let west = (col as f64 / n_f) * 360.0 - 180.0;
    let east = ((col + 1) as f64 / n_f) * 360.0 - 180.0;

    // Latitude: inverse Mercator from row (row 0 = north)
    let north_y = PI * (1.0 - 2.0 * row as f64 / n_f);
    let south_y = PI * (1.0 - 2.0 * (row + 1) as f64 / n_f);
    let north = north_y.sinh().atan().to_degrees();
    let south = south_y.sinh().atan().to_degrees();

    Some([west, south, east, north])
}

fn web_mercator_extent_to_tile_range(
    _zoom: u32,
    bbox: [f64; 4],
    matrix: &TileMatrixInfo,
) -> (u64, u64, u64, u64) {
    let [west, south, east, north] = bbox;
    let n = matrix.matrix_width as f64;

    // Longitude → column
    let min_col = ((west + 180.0) / 360.0 * n).floor() as u64;
    let max_col = ((east + 180.0) / 360.0 * n - 1e-10).floor() as u64;

    // Latitude → row (Mercator, row 0 = north)
    let lat_to_row = |lat: f64| -> u64 {
        let lat_rad = lat.to_radians();
        let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI) / 2.0;
        (y * n).floor().max(0.0) as u64
    };

    let min_row = lat_to_row(north); // north has smaller row number
    let max_row = lat_to_row(south);

    (
        min_col.min(matrix.matrix_width - 1),
        min_row.min(matrix.matrix_height - 1),
        max_col.min(matrix.matrix_width - 1),
        max_row.min(matrix.matrix_height - 1),
    )
}

// ---------------------------------------------------------------------------
// WorldCRS84Quad math
// ---------------------------------------------------------------------------

fn crs84_matrix(zoom: u32) -> TileMatrixInfo {
    // At zoom 0: 2 tiles wide (360 degrees), 1 tile tall (180 degrees)
    let cols = 2u64 << zoom;
    let rows = 1u64 << zoom;

    // Scale denominator: at zoom 0, 1 pixel = 360/(2*256) degrees
    // OGC uses 0.28mm pixel size and degrees-based CRS
    let degrees_per_pixel = 360.0 / (cols as f64 * 256.0);
    // Convert degrees to meters at equator for scale denominator
    let meters_per_degree = EARTH_CIRCUMFERENCE / 360.0;
    let scale_denominator = degrees_per_pixel * meters_per_degree / PIXEL_SIZE_M;

    TileMatrixInfo {
        scale_denominator,
        cell_size: degrees_per_pixel,
        top_left_corner: [-180.0, 90.0],
        tile_width: 256,
        tile_height: 256,
        matrix_width: cols,
        matrix_height: rows,
    }
}

/// Compute WGS84 bbox for a WorldCRS84Quad tile.
/// At zoom 0: 2 columns (each 180 degrees wide) x 1 row (180 degrees tall).
fn crs84_tile_bbox(zoom: u32, row: u64, col: u64) -> Option<[f64; 4]> {
    if zoom > 24 {
        return None;
    }
    let cols = 2u64.checked_shl(zoom)?;
    let rows = 1u64.checked_shl(zoom)?;
    if col >= cols || row >= rows {
        return None;
    }

    let tile_width_deg = 360.0 / cols as f64;
    let tile_height_deg = 180.0 / rows as f64;

    let west = -180.0 + col as f64 * tile_width_deg;
    let east = west + tile_width_deg;
    let north = 90.0 - row as f64 * tile_height_deg;
    let south = north - tile_height_deg;

    Some([west, south, east, north])
}

fn crs84_extent_to_tile_range(
    _zoom: u32,
    bbox: [f64; 4],
    matrix: &TileMatrixInfo,
) -> (u64, u64, u64, u64) {
    let [west, south, east, north] = bbox;
    let cols = matrix.matrix_width as f64;
    let rows = matrix.matrix_height as f64;

    let tile_width_deg = 360.0 / cols;
    let tile_height_deg = 180.0 / rows;

    let min_col = ((west + 180.0) / tile_width_deg).floor() as u64;
    let max_col = ((east + 180.0) / tile_width_deg - 1e-10).floor() as u64;
    let min_row = ((90.0 - north) / tile_height_deg).floor() as u64;
    let max_row = ((90.0 - south) / tile_height_deg - 1e-10).floor() as u64;

    (
        min_col.min(matrix.matrix_width - 1),
        min_row.min(matrix.matrix_height - 1),
        max_col.min(matrix.matrix_width - 1),
        max_row.min(matrix.matrix_height - 1),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id in `SUPPORTED_TILE_MATRIX_SETS` must resolve via
    /// `get_tile_matrix_set`. This is the CI guard that lets the
    /// `build_collection_metadata` `crs[]` builder use `if let Some` (no panic
    /// in the request path) without silently shortening `crs[]` if the two
    /// constructs ever diverge (review on #298).
    #[test]
    fn every_supported_tms_resolves() {
        for id in SUPPORTED_TILE_MATRIX_SETS {
            assert!(
                get_tile_matrix_set(id).is_some(),
                "SUPPORTED_TILE_MATRIX_SETS entry {id:?} has no TileMatrixSetDef"
            );
        }
    }

    #[test]
    fn test_web_mercator_z0_single_tile() {
        let bbox = web_mercator_tile_bbox(0, 0, 0).unwrap();
        assert!((bbox[0] - (-180.0)).abs() < 1e-10); // west
        assert!((bbox[2] - 180.0).abs() < 1e-10); // east
                                                  // Web Mercator latitude range is ~-85.05 to ~85.05
        assert!(bbox[1] < -85.0); // south
        assert!(bbox[3] > 85.0); // north
    }

    #[test]
    fn test_web_mercator_z1_quadrants() {
        // z=1: 2x2 tiles
        let nw = web_mercator_tile_bbox(1, 0, 0).unwrap();
        let ne = web_mercator_tile_bbox(1, 0, 1).unwrap();
        let sw = web_mercator_tile_bbox(1, 1, 0).unwrap();
        let _se = web_mercator_tile_bbox(1, 1, 1).unwrap();

        // NW tile: west=-180, east=0
        assert!((nw[0] - (-180.0)).abs() < 1e-10);
        assert!((nw[2] - 0.0).abs() < 1e-10);
        assert!(nw[3] > 85.0); // north edge

        // NE tile: west=0, east=180
        assert!((ne[0] - 0.0).abs() < 1e-10);
        assert!((ne[2] - 180.0).abs() < 1e-10);

        // SW tile: south edge is at the bottom
        assert!(sw[1] < -85.0);
        assert!((sw[0] - (-180.0)).abs() < 1e-10);

        // Adjacent tiles share edges
        assert!((nw[2] - ne[0]).abs() < 1e-10); // NW east = NE west
        assert!((nw[1] - sw[3]).abs() < 1e-10); // NW south = SW north
    }

    #[test]
    fn test_web_mercator_known_tile() {
        // z=2, row=1, col=2 should be in the eastern hemisphere, near the equator
        let bbox = web_mercator_tile_bbox(2, 1, 2).unwrap();
        assert!((bbox[0] - 0.0).abs() < 1e-10); // west = 0
        assert!((bbox[2] - 90.0).abs() < 1e-10); // east = 90
        assert!(bbox[1] < 1.0); // south near 0
        assert!(bbox[3] > 60.0); // north around 66
    }

    #[test]
    fn test_web_mercator_out_of_range() {
        assert!(web_mercator_tile_bbox(0, 1, 0).is_none()); // row 1 at z0
        assert!(web_mercator_tile_bbox(0, 0, 1).is_none()); // col 1 at z0
        assert!(web_mercator_tile_bbox(25, 0, 0).is_none()); // z > 24
    }

    #[test]
    fn test_crs84_z0_two_tiles() {
        // z=0: 2 columns, 1 row
        let left = crs84_tile_bbox(0, 0, 0).unwrap();
        let right = crs84_tile_bbox(0, 0, 1).unwrap();

        assert!((left[0] - (-180.0)).abs() < 1e-10);
        assert!((left[2] - 0.0).abs() < 1e-10);
        assert!((left[1] - (-90.0)).abs() < 1e-10);
        assert!((left[3] - 90.0).abs() < 1e-10);

        assert!((right[0] - 0.0).abs() < 1e-10);
        assert!((right[2] - 180.0).abs() < 1e-10);
    }

    #[test]
    fn test_crs84_z1_four_tiles() {
        // z=1: 4 columns, 2 rows
        let tl = crs84_tile_bbox(1, 0, 0).unwrap();
        assert!((tl[0] - (-180.0)).abs() < 1e-10);
        assert!((tl[2] - (-90.0)).abs() < 1e-10);
        assert!((tl[1] - 0.0).abs() < 1e-10);
        assert!((tl[3] - 90.0).abs() < 1e-10);

        // Out of range
        assert!(crs84_tile_bbox(1, 2, 0).is_none()); // row 2 at z1 (only 0,1)
        assert!(crs84_tile_bbox(1, 0, 4).is_none()); // col 4 at z1 (only 0-3)
    }

    #[test]
    fn test_crs84_out_of_range() {
        assert!(crs84_tile_bbox(0, 0, 2).is_none()); // only 2 cols at z0
        assert!(crs84_tile_bbox(0, 1, 0).is_none()); // only 1 row at z0
        assert!(crs84_tile_bbox(25, 0, 0).is_none()); // z > 24
    }

    #[test]
    fn test_validate_coords() {
        assert!(WEB_MERCATOR_QUAD.validate_coords(0, 0, 0));
        assert!(!WEB_MERCATOR_QUAD.validate_coords(0, 1, 0));
        assert!(!WEB_MERCATOR_QUAD.validate_coords(25, 0, 0));

        assert!(WORLD_CRS84_QUAD.validate_coords(0, 0, 0));
        assert!(WORLD_CRS84_QUAD.validate_coords(0, 0, 1));
        assert!(!WORLD_CRS84_QUAD.validate_coords(0, 1, 0)); // only 1 row at z0
    }

    #[test]
    fn test_tile_adjacency_no_gaps() {
        // Verify adjacent tiles share exact edges at z=10
        for col in 0..3 {
            let left = web_mercator_tile_bbox(10, 500, col).unwrap();
            let right = web_mercator_tile_bbox(10, 500, col + 1).unwrap();
            assert!(
                (left[2] - right[0]).abs() < 1e-12,
                "Gap between col {} and {}: {} vs {}",
                col,
                col + 1,
                left[2],
                right[0]
            );
        }
        for row in 0..3 {
            let top = web_mercator_tile_bbox(10, row, 500).unwrap();
            let bottom = web_mercator_tile_bbox(10, row + 1, 500).unwrap();
            assert!(
                (top[1] - bottom[3]).abs() < 1e-12,
                "Gap between row {} and {}: {} vs {}",
                row,
                row + 1,
                top[1],
                bottom[3]
            );
        }
    }

    #[test]
    fn test_matrix_dimensions() {
        let m0 = WEB_MERCATOR_QUAD.matrix(0).unwrap();
        assert_eq!(m0.matrix_width, 1);
        assert_eq!(m0.matrix_height, 1);

        let m1 = WEB_MERCATOR_QUAD.matrix(1).unwrap();
        assert_eq!(m1.matrix_width, 2);
        assert_eq!(m1.matrix_height, 2);

        let m10 = WEB_MERCATOR_QUAD.matrix(10).unwrap();
        assert_eq!(m10.matrix_width, 1024);
        assert_eq!(m10.matrix_height, 1024);

        // CRS84: 2:1 aspect ratio
        let c0 = WORLD_CRS84_QUAD.matrix(0).unwrap();
        assert_eq!(c0.matrix_width, 2);
        assert_eq!(c0.matrix_height, 1);

        let c1 = WORLD_CRS84_QUAD.matrix(1).unwrap();
        assert_eq!(c1.matrix_width, 4);
        assert_eq!(c1.matrix_height, 2);
    }

    #[test]
    fn test_limits_for_extent() {
        // Finland roughly: 19-32E, 59-70N
        let finland = [19.0, 59.0, 32.0, 70.0];
        let limits = WEB_MERCATOR_QUAD.limits_for_extent(finland, 5);
        assert_eq!(limits.len(), 6); // z 0-5

        // At z=0, the single tile should contain Finland
        let z0 = &limits[0];
        assert_eq!(z0["minTileCol"], 0);
        assert_eq!(z0["maxTileCol"], 0);
        assert_eq!(z0["minTileRow"], 0);
        assert_eq!(z0["maxTileRow"], 0);
    }
}
