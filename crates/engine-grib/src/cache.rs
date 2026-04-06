//! LRU cache for decoded GRIB grid data.
//!
//! Caches decoded f64 arrays keyed by (grib_url, offset) to avoid
//! repeated byte-range fetches and GRIB decoding.

use std::sync::Arc;

use quick_cache::sync::Cache;

/// Cache key: identifies a specific GRIB message.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct GridKey {
    /// URL or path to the GRIB file.
    url: String,
    /// Byte offset of the message within the file.
    offset: u64,
}

/// A decoded grid with its metadata.
#[derive(Debug, Clone)]
pub struct DecodedGrid {
    /// Number of points along the longitude axis.
    pub ni: usize,
    /// Number of points along the latitude axis.
    pub nj: usize,
    /// First longitude (westernmost).
    pub lon_first: f64,
    /// First latitude (northernmost, typically 90.0).
    pub lat_first: f64,
    /// Longitude increment (positive eastward).
    pub lon_inc: f64,
    /// Latitude increment (positive northward, but stored negative for N→S grids).
    pub lat_inc: f64,
    /// Decoded values in row-major order (north to south, west to east).
    pub values: Arc<Vec<f64>>,
}

impl DecodedGrid {
    /// Memory size estimate in bytes.
    fn size_bytes(&self) -> usize {
        self.values.len() * 8 + 64 // values + struct overhead
    }

    /// Get the value at the grid point nearest to (lon, lat).
    /// Returns None if the point is outside the grid.
    pub fn nearest_value(&self, lon: f64, lat: f64) -> Option<f64> {
        let (col, row) = self.lonlat_to_grid(lon, lat)?;
        Some(self.values[row * self.ni + col])
    }

    /// Convert (lon, lat) to (col, row) grid indices.
    fn lonlat_to_grid(&self, lon: f64, lat: f64) -> Option<(usize, usize)> {
        // Normalize longitude to grid range
        let mut lon = lon;
        if lon < self.lon_first {
            lon += 360.0;
        }
        if lon >= self.lon_first + (self.ni as f64) * self.lon_inc {
            lon -= 360.0;
        }

        let col = ((lon - self.lon_first) / self.lon_inc).round() as isize;
        let row = ((lat - self.lat_first) / self.lat_inc).round() as isize;

        if col < 0 || col >= self.ni as isize || row < 0 || row >= self.nj as isize {
            return None;
        }

        Some((col as usize, row as usize))
    }

    /// Extract a grid subset for the given bbox [west, south, east, north].
    /// Returns (x_coords, y_coords, values) for a Grid domain.
    #[allow(clippy::type_complexity)]
    pub fn extract_bbox(&self, bbox: [f64; 4]) -> Option<(Vec<f64>, Vec<f64>, Vec<Option<f64>>)> {
        let [west, south, east, north] = bbox;

        // Find column range
        let col_start = ((west - self.lon_first) / self.lon_inc).floor() as isize;
        let col_end = ((east - self.lon_first) / self.lon_inc).ceil() as isize;

        // Find row range (lat_inc is negative for N→S grids)
        let (row_start, row_end) = if self.lat_inc < 0.0 {
            // N→S: north has smaller row index
            let rs = ((north - self.lat_first) / self.lat_inc).floor() as isize;
            let re = ((south - self.lat_first) / self.lat_inc).ceil() as isize;
            (rs, re)
        } else {
            let rs = ((south - self.lat_first) / self.lat_inc).floor() as isize;
            let re = ((north - self.lat_first) / self.lat_inc).ceil() as isize;
            (rs, re)
        };

        // Clamp to grid bounds
        let col_start = col_start.max(0) as usize;
        let col_end = (col_end.min(self.ni as isize) as usize).max(col_start);
        let row_start = row_start.max(0) as usize;
        let row_end = (row_end.min(self.nj as isize) as usize).max(row_start);

        if col_start >= col_end || row_start >= row_end {
            return None;
        }

        let nx = col_end - col_start;
        let ny = row_end - row_start;

        let mut x_coords = Vec::with_capacity(nx);
        for c in col_start..col_end {
            x_coords.push(self.lon_first + c as f64 * self.lon_inc);
        }

        let mut y_coords = Vec::with_capacity(ny);
        for r in row_start..row_end {
            y_coords.push(self.lat_first + r as f64 * self.lat_inc);
        }
        // y_coords should be in ascending order for CoverageJSON
        if self.lat_inc < 0.0 {
            y_coords.reverse();
        }

        let mut values = Vec::with_capacity(nx * ny);
        // Output in y-ascending order (south to north)
        let row_iter: Box<dyn Iterator<Item = usize>> = if self.lat_inc < 0.0 {
            Box::new((row_start..row_end).rev())
        } else {
            Box::new(row_start..row_end)
        };
        for r in row_iter {
            for c in col_start..col_end {
                values.push(Some(self.values[r * self.ni + c]));
            }
        }

        Some((x_coords, y_coords, values))
    }

    /// Resample grid to output dimensions for map rendering.
    /// bbox is [west, south, east, north] in WGS84.
    pub fn resample(
        &self,
        bbox: [f64; 4],
        width: u32,
        height: u32,
        web_mercator: bool,
    ) -> Vec<Option<f64>> {
        let [west, south, east, north] = bbox;
        let w = width as usize;
        let h = height as usize;
        let mut out = Vec::with_capacity(w * h);

        for row in 0..h {
            // For WebMercator, compute latitude using Mercator inverse
            let lat = if web_mercator {
                let y_frac = 1.0 - (row as f64 + 0.5) / h as f64;
                let y_merc_south = south.to_radians().sin().atanh();
                let y_merc_north = north.to_radians().sin().atanh();
                let y_merc = y_merc_south + y_frac * (y_merc_north - y_merc_south);
                y_merc.sinh().atan().to_degrees()
            } else {
                north - (row as f64 + 0.5) * (north - south) / h as f64
            };

            for col in 0..w {
                let lon = west + (col as f64 + 0.5) * (east - west) / w as f64;
                out.push(self.nearest_value(lon, lat));
            }
        }

        out
    }
}

/// LRU cache for decoded grids.
pub struct GridCache {
    cache: Cache<GridKey, Arc<DecodedGrid>>,
}

impl GridCache {
    /// Create a new grid cache with the given size limit in MB.
    /// Returns None if size_mb is 0 (cache disabled).
    pub fn new(size_mb: u64) -> Option<Self> {
        if size_mb == 0 {
            return None;
        }
        // Estimate: each grid is ~8 MB (1440*721*8 bytes).
        // With 256 MB, that's ~32 grids.
        let estimated_items = (size_mb as usize * 1024 * 1024) / (8 * 1024 * 1024);
        let items = estimated_items.max(16);
        Some(Self {
            cache: Cache::new(items),
        })
    }

    /// Get a cached grid, or None if not cached.
    pub fn get(&self, url: &str, offset: u64) -> Option<Arc<DecodedGrid>> {
        let key = GridKey {
            url: url.to_string(),
            offset,
        };
        self.cache.get(&key)
    }

    /// Insert a decoded grid into the cache.
    pub fn insert(&self, url: &str, offset: u64, grid: Arc<DecodedGrid>) {
        let key = GridKey {
            url: url.to_string(),
            offset,
        };
        let weight = grid.size_bytes() as u64;
        let _ = weight; // quick_cache uses item count, not weight
        self.cache.insert(key, grid);
    }
}
