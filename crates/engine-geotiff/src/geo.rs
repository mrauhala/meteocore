use std::f64::consts::PI;

/// WGS84 ellipsoid parameters
const WGS84_A: f64 = 6_378_137.0; // semi-major axis (meters)
const WGS84_F: f64 = 1.0 / 298.257223563; // flattening
const WGS84_E2: f64 = 2.0 * WGS84_F - WGS84_F * WGS84_F; // eccentricity squared

/// Coordinate reference system parsed from GeoTIFF GeoKeys.
#[derive(Debug, Clone)]
pub enum Crs {
    /// WGS84 geographic (EPSG:4326 / CRS84). Coordinates are (lon, lat) in degrees.
    Wgs84,
    /// Transverse Mercator (e.g., EPSG:3067 TM35FIN).
    TransverseMercator {
        lat0: f64,     // latitude of natural origin (radians)
        lon0: f64,     // central meridian (radians)
        k0: f64,       // scale factor at natural origin
        false_e: f64,  // false easting (meters)
        false_n: f64,  // false northing (meters)
    },
    /// Lambert Azimuthal Equal Area (e.g., EPSG:3035).
    LambertAzimuthalEqualArea {
        lat0: f64,     // latitude of natural origin (radians)
        lon0: f64,     // longitude of natural origin (radians)
        false_e: f64,  // false easting (meters)
        false_n: f64,  // false northing (meters)
    },
    /// Lambert Conformal Conic with 2 standard parallels.
    LambertConformalConic {
        lat1: f64,     // first standard parallel (radians)
        lat2: f64,     // second standard parallel (radians)
        lat0: f64,     // latitude of false origin (radians)
        lon0: f64,     // longitude of false origin (radians)
        false_e: f64,  // false easting (meters)
        false_n: f64,  // false northing (meters)
    },
}

impl Crs {
    /// Forward-transform WGS84 (lon_deg, lat_deg) to projected (easting, northing).
    /// For Wgs84, returns (lon, lat) unchanged.
    pub fn forward(&self, lon_deg: f64, lat_deg: f64) -> (f64, f64) {
        match self {
            Crs::Wgs84 => (lon_deg, lat_deg),
            Crs::TransverseMercator { lat0, lon0, k0, false_e, false_n } => {
                tm_forward(lat_deg.to_radians(), lon_deg.to_radians(), *lat0, *lon0, *k0, *false_e, *false_n)
            }
            Crs::LambertAzimuthalEqualArea { lat0, lon0, false_e, false_n } => {
                laea_forward(lat_deg.to_radians(), lon_deg.to_radians(), *lat0, *lon0, *false_e, *false_n)
            }
            Crs::LambertConformalConic { lat1, lat2, lat0, lon0, false_e, false_n } => {
                lcc_forward(lat_deg.to_radians(), lon_deg.to_radians(), *lat1, *lat2, *lat0, *lon0, *false_e, *false_n)
            }
        }
    }

    /// Inverse-transform projected (easting, northing) to WGS84 (lon_deg, lat_deg).
    /// For Wgs84, returns (x, y) unchanged.
    pub fn inverse(&self, x: f64, y: f64) -> (f64, f64) {
        match self {
            Crs::Wgs84 => (x, y),
            Crs::TransverseMercator { lat0, lon0, k0, false_e, false_n } => {
                let (lat, lon) = tm_inverse(x, y, *lat0, *lon0, *k0, *false_e, *false_n);
                (lon.to_degrees(), lat.to_degrees())
            }
            Crs::LambertAzimuthalEqualArea { lat0, lon0, false_e, false_n } => {
                let (lat, lon) = laea_inverse(x, y, *lat0, *lon0, *false_e, *false_n);
                (lon.to_degrees(), lat.to_degrees())
            }
            Crs::LambertConformalConic { lat1, lat2, lat0, lon0, false_e, false_n } => {
                let (lat, lon) = lcc_inverse(x, y, *lat1, *lat2, *lat0, *lon0, *false_e, *false_n);
                (lon.to_degrees(), lat.to_degrees())
            }
        }
    }
}

/// Affine transform mapping pixel coordinates to projected (or geographic) coordinates.
#[derive(Debug, Clone)]
pub struct GeoTransform {
    /// Origin X in source CRS (easting or longitude).
    pub origin_x: f64,
    /// Origin Y in source CRS (northing or latitude).
    pub origin_y: f64,
    pub pixel_width: f64,
    pub pixel_height: f64,
    pub width: u32,
    pub height: u32,
    /// Coordinate reference system of the raster.
    pub crs: Crs,
}

impl GeoTransform {
    /// Convert WGS84 (lon, lat) to pixel coordinate (col, row).
    /// Handles CRS transformation internally.
    /// Returns None if the coordinate is outside the raster bounds.
    pub fn world_to_pixel(&self, lon: f64, lat: f64) -> Option<(u32, u32)> {
        let (x, y) = self.crs.forward(lon, lat);
        let col = ((x - self.origin_x) / self.pixel_width) as i64;
        let row = ((self.origin_y - y) / self.pixel_height) as i64;

        if col >= 0 && col < self.width as i64 && row >= 0 && row < self.height as i64 {
            Some((col as u32, row as u32))
        } else {
            None
        }
    }

    /// Compute the bounding box in WGS84 [west, south, east, north].
    /// For projected CRS, samples points along all edges (not just corners)
    /// to handle projection distortion.
    pub fn bbox(&self) -> [f64; 4] {
        let x_min = self.origin_x;
        let x_max = self.origin_x + self.width as f64 * self.pixel_width;
        let y_max = self.origin_y;
        let y_min = self.origin_y - self.height as f64 * self.pixel_height;

        let mut min_lon = f64::MAX;
        let mut max_lon = f64::MIN;
        let mut min_lat = f64::MAX;
        let mut max_lat = f64::MIN;

        // Sample N points along each edge
        let n = 20;
        for i in 0..=n {
            let frac = i as f64 / n as f64;
            // Top and bottom edges
            let x = x_min + frac * (x_max - x_min);
            for &y in &[y_max, y_min] {
                let (lon, lat) = self.crs.inverse(x, y);
                min_lon = min_lon.min(lon);
                max_lon = max_lon.max(lon);
                min_lat = min_lat.min(lat);
                max_lat = max_lat.max(lat);
            }
            // Left and right edges
            let y = y_min + frac * (y_max - y_min);
            for &x in &[x_min, x_max] {
                let (lon, lat) = self.crs.inverse(x, y);
                min_lon = min_lon.min(lon);
                max_lon = max_lon.max(lon);
                min_lat = min_lat.min(lat);
                max_lat = max_lat.max(lat);
            }
        }

        [min_lon, min_lat, max_lon, max_lat]
    }

    /// Convert pixel coordinate to WGS84 (lon, lat) at pixel center.
    pub fn pixel_to_world(&self, col: u32, row: u32) -> (f64, f64) {
        let x = self.origin_x + (col as f64 + 0.5) * self.pixel_width;
        let y = self.origin_y - (row as f64 + 0.5) * self.pixel_height;
        self.crs.inverse(x, y)
    }

    /// Convert a WGS84 bbox [west, south, east, north] to pixel range.
    /// Transforms all four corners to the source CRS, takes the envelope, then maps to pixels.
    /// Returns (col_start, row_start, col_end, row_end) clamped to raster bounds. Exclusive end.
    pub fn bbox_to_pixels(&self, west: f64, south: f64, east: f64, north: f64) -> Option<(u32, u32, u32, u32)> {
        // Transform bbox corners to source CRS
        let corners = [
            self.crs.forward(west, south),
            self.crs.forward(east, south),
            self.crs.forward(west, north),
            self.crs.forward(east, north),
        ];

        let mut min_x = f64::MAX;
        let mut max_x = f64::MIN;
        let mut min_y = f64::MAX;
        let mut max_y = f64::MIN;

        for (x, y) in &corners {
            min_x = min_x.min(*x);
            max_x = max_x.max(*x);
            min_y = min_y.min(*y);
            max_y = max_y.max(*y);
        }

        let col_start = ((min_x - self.origin_x) / self.pixel_width).floor().max(0.0) as u32;
        let col_end = ((max_x - self.origin_x) / self.pixel_width).ceil().min(self.width as f64) as u32;
        let row_start = ((self.origin_y - max_y) / self.pixel_height).floor().max(0.0) as u32;
        let row_end = ((self.origin_y - min_y) / self.pixel_height).ceil().min(self.height as f64) as u32;

        if col_start >= col_end || row_start >= row_end {
            return None;
        }

        Some((col_start, row_start, col_end, row_end))
    }
}

// ============================================================================
// Transverse Mercator — used by EPSG:3067 (TM35FIN) and UTM zones
// Reference: Snyder, USGS PP 1395, equations 8-9 through 8-15
// ============================================================================

fn tm_forward(lat: f64, lon: f64, lat0: f64, lon0: f64, k0: f64, false_e: f64, false_n: f64) -> (f64, f64) {
    let e2 = WGS84_E2;
    let ep2 = e2 / (1.0 - e2); // e'^2

    let dl = lon - lon0;
    let cos_lat = lat.cos();
    let sin_lat = lat.sin();
    let tan_lat = lat.tan();

    let n_val = WGS84_A / (1.0 - e2 * sin_lat * sin_lat).sqrt();
    let t = tan_lat;
    let t2 = t * t;
    let c = ep2 * cos_lat * cos_lat;
    let a_coeff = dl * cos_lat;
    let a2 = a_coeff * a_coeff;

    let m = meridian_arc(lat);
    let m0 = meridian_arc(lat0);

    let x = k0 * n_val * (a_coeff
        + (1.0 - t2 + c) * a2 * a_coeff / 6.0
        + (5.0 - 18.0 * t2 + t2 * t2 + 72.0 * c - 58.0 * ep2) * a2 * a2 * a_coeff / 120.0);

    let y = k0 * (m - m0
        + n_val * tan_lat * (a2 / 2.0
            + (5.0 - t2 + 9.0 * c + 4.0 * c * c) * a2 * a2 / 24.0
            + (61.0 - 58.0 * t2 + t2 * t2 + 600.0 * c - 330.0 * ep2) * a2 * a2 * a2 / 720.0));

    (false_e + x, false_n + y)
}

fn tm_inverse(x: f64, y: f64, lat0: f64, lon0: f64, k0: f64, false_e: f64, false_n: f64) -> (f64, f64) {
    // Newton iteration approach — works at any distance from central meridian.
    // Start with the footpoint latitude, then iterate to find (lat, lon).
    let e2 = WGS84_E2;
    let ep2 = e2 / (1.0 - e2);
    let e1 = (1.0 - (1.0 - e2).sqrt()) / (1.0 + (1.0 - e2).sqrt());

    let m0 = meridian_arc(lat0);
    let m = m0 + (y - false_n) / k0;

    // Footpoint latitude from meridian arc
    let mu = m / (WGS84_A * (1.0 - e2 / 4.0 - 3.0 * e2 * e2 / 64.0 - 5.0 * e2 * e2 * e2 / 256.0));
    let lat1 = mu
        + (3.0 * e1 / 2.0 - 27.0 * e1 * e1 * e1 / 32.0) * (2.0 * mu).sin()
        + (21.0 * e1 * e1 / 16.0 - 55.0 * e1 * e1 * e1 * e1 / 32.0) * (4.0 * mu).sin()
        + (151.0 * e1 * e1 * e1 / 96.0) * (6.0 * mu).sin()
        + (1097.0 * e1 * e1 * e1 * e1 / 512.0) * (8.0 * mu).sin();

    // Use series for initial guess, then refine with Newton iteration
    let sin_lat1 = lat1.sin();
    let cos_lat1 = lat1.cos();
    let tan_lat1 = lat1.tan();
    let n1 = WGS84_A / (1.0 - e2 * sin_lat1 * sin_lat1).sqrt();
    let r1 = WGS84_A * (1.0 - e2) / (1.0 - e2 * sin_lat1 * sin_lat1).powf(1.5);
    let t12 = tan_lat1 * tan_lat1;
    let c1 = ep2 * cos_lat1 * cos_lat1;
    let d = (x - false_e) / (n1 * k0);
    let d2 = d * d;

    // Series initial guess
    let mut lat = lat1
        - (n1 * tan_lat1 / r1) * (d2 / 2.0
            - (5.0 + 3.0 * t12 + 10.0 * c1 - 4.0 * c1 * c1 - 9.0 * ep2) * d2 * d2 / 24.0
            + (61.0 + 90.0 * t12 + 298.0 * c1 + 45.0 * t12 * t12 - 252.0 * ep2 - 3.0 * c1 * c1) * d2 * d2 * d2 / 720.0);

    let mut lon = lon0
        + (d - (1.0 + 2.0 * t12 + c1) * d2 * d / 6.0
            + (5.0 - 2.0 * c1 + 28.0 * t12 - 3.0 * c1 * c1 + 8.0 * ep2 + 24.0 * t12 * t12) * d2 * d2 * d / 120.0)
        / cos_lat1;

    // Newton refinement: iterate forward(lat,lon) towards target (x,y)
    for _ in 0..20 {
        let (fx, fy) = tm_forward(lat, lon, lat0, lon0, k0, false_e, false_n);
        let ex = x - fx; // easting residual
        let ey = y - fy; // northing residual
        if ex.abs() < 0.001 && ey.abs() < 0.001 {
            break;
        }
        // Numerical Jacobian: d(easting,northing)/d(lat,lon)
        let h = 1e-8;
        let (fx_dlat, fy_dlat) = tm_forward(lat + h, lon, lat0, lon0, k0, false_e, false_n);
        let (fx_dlon, fy_dlon) = tm_forward(lat, lon + h, lat0, lon0, k0, false_e, false_n);
        // J = [[de/dlat, de/dlon], [dn/dlat, dn/dlon]]
        let de_dlat = (fx_dlat - fx) / h;
        let de_dlon = (fx_dlon - fx) / h;
        let dn_dlat = (fy_dlat - fy) / h;
        let dn_dlon = (fy_dlon - fy) / h;
        let det = de_dlat * dn_dlon - de_dlon * dn_dlat;
        if det.abs() < 1e-30 {
            break;
        }
        // J^-1 * [ex, ey]
        let dlat = (dn_dlon * ex - de_dlon * ey) / det;
        let dlon = (-dn_dlat * ex + de_dlat * ey) / det;
        lat += dlat;
        lon += dlon;
    }

    (lat, lon)
}

/// Meridian arc from equator to latitude (Helmert series).
fn meridian_arc(lat: f64) -> f64 {
    let e2 = WGS84_E2;
    let e4 = e2 * e2;
    let e6 = e4 * e2;
    WGS84_A * ((1.0 - e2 / 4.0 - 3.0 * e4 / 64.0 - 5.0 * e6 / 256.0) * lat
        - (3.0 * e2 / 8.0 + 3.0 * e4 / 32.0 + 45.0 * e6 / 1024.0) * (2.0 * lat).sin()
        + (15.0 * e4 / 256.0 + 45.0 * e6 / 1024.0) * (4.0 * lat).sin()
        - (35.0 * e6 / 3072.0) * (6.0 * lat).sin())
}

// ============================================================================
// Lambert Azimuthal Equal Area — used by EPSG:3035 (ETRS89-LAEA)
// Reference: Snyder, "Map Projections: A Working Manual", USGS PP 1395, p.187
// ============================================================================

fn laea_forward(lat: f64, lon: f64, lat0: f64, lon0: f64, false_e: f64, false_n: f64) -> (f64, f64) {
    let e2 = WGS84_E2;
    let e = e2.sqrt();

    let q_val = q_authalic(lat, e);
    let q0 = q_authalic(lat0, e);
    let qp = q_authalic(PI / 2.0, e);

    let beta = (q_val / qp).clamp(-1.0, 1.0).asin();
    let beta0 = (q0 / qp).clamp(-1.0, 1.0).asin();

    let rq = WGS84_A * (qp / 2.0).sqrt();
    let m0 = lat0.cos() / (1.0 - e2 * lat0.sin().powi(2)).sqrt();
    let dd = WGS84_A * m0 / (rq * beta0.cos());

    let dl = lon - lon0;

    // Snyder eq. 24-2, 24-3 (oblique)
    let b = rq * (2.0 / (1.0 + beta0.sin() * beta.sin() + beta0.cos() * beta.cos() * dl.cos())).max(0.0).sqrt();

    let x = false_e + b * dd * beta.cos() * dl.sin();
    let y = false_n + (b / dd) * (beta0.cos() * beta.sin() - beta0.sin() * beta.cos() * dl.cos());

    (x, y)
}

fn laea_inverse(x: f64, y: f64, lat0: f64, lon0: f64, false_e: f64, false_n: f64) -> (f64, f64) {
    // Newton iteration from projection center — robust at any distance.
    let mut lat = lat0;
    let mut lon = lon0;

    for _ in 0..20 {
        let (fx, fy) = laea_forward(lat, lon, lat0, lon0, false_e, false_n);
        let ex = x - fx;
        let ey = y - fy;
        if ex.abs() < 0.01 && ey.abs() < 0.01 {
            break;
        }
        let h = 1e-8;
        let (fx_dlat, fy_dlat) = laea_forward(lat + h, lon, lat0, lon0, false_e, false_n);
        let (fx_dlon, fy_dlon) = laea_forward(lat, lon + h, lat0, lon0, false_e, false_n);
        let de_dlat = (fx_dlat - fx) / h;
        let de_dlon = (fx_dlon - fx) / h;
        let dn_dlat = (fy_dlat - fy) / h;
        let dn_dlon = (fy_dlon - fy) / h;
        let det = de_dlat * dn_dlon - de_dlon * dn_dlat;
        if det.abs() < 1e-30 {
            break;
        }
        lat += (dn_dlon * ex - de_dlon * ey) / det;
        lon += (-dn_dlat * ex + de_dlat * ey) / det;
    }

    (lat, lon)
}

fn q_authalic(lat: f64, e: f64) -> f64 {
    let sin_lat = lat.sin();
    let e_sin = e * sin_lat;
    (1.0 - e * e) * (sin_lat / (1.0 - e_sin * e_sin) - (1.0 / (2.0 * e)) * ((1.0 - e_sin) / (1.0 + e_sin)).ln())
}

#[allow(dead_code)]
fn authalic_inverse(q: f64, e: f64) -> f64 {
    let e2 = e * e;
    let e4 = e2 * e2;
    let e6 = e4 * e2;
    let qp = q_authalic(PI / 2.0, e);

    // Initial approximation using series (Snyder eq. 3-18)
    let beta = (q / qp).clamp(-1.0, 1.0).asin();
    let mut lat = beta
        + (e2 / 3.0 + 31.0 * e4 / 180.0 + 517.0 * e6 / 5040.0) * (2.0 * beta).sin()
        + (23.0 * e4 / 360.0 + 251.0 * e6 / 3780.0) * (4.0 * beta).sin()
        + (761.0 * e6 / 45360.0) * (6.0 * beta).sin();

    // Newton iteration
    for _ in 0..6 {
        let sin_lat = lat.sin();
        let cos_lat = lat.cos();
        if cos_lat.abs() < 1e-14 {
            break;
        }
        let q_lat = q_authalic(lat, e);
        let delta = q - q_lat;
        let denom = (1.0 - e2 * sin_lat * sin_lat).powi(2) / (2.0 * (1.0 - e2) * cos_lat);
        lat += delta * denom;
        if delta.abs() < 1e-12 {
            break;
        }
    }
    lat
}

// ============================================================================
// Lambert Conformal Conic (2SP) — used by MET Norway radar
// Reference: Snyder, USGS PP 1395, p.107
// ============================================================================

fn lcc_forward(lat: f64, lon: f64, lat1: f64, lat2: f64, lat0: f64, lon0: f64, false_e: f64, false_n: f64) -> (f64, f64) {
    let e2 = WGS84_E2;
    let e = e2.sqrt();

    let m1 = lcc_m(lat1, e2);
    let m2 = lcc_m(lat2, e2);
    let t0 = lcc_t(lat0, e);
    let t1 = lcc_t(lat1, e);
    let t2 = lcc_t(lat2, e);
    let t = lcc_t(lat, e);

    let n = (m1.ln() - m2.ln()) / (t1.ln() - t2.ln());
    let f_val = m1 / (n * t1.powf(n));
    let rho0 = WGS84_A * f_val * t0.powf(n);
    let rho = WGS84_A * f_val * t.powf(n);
    let theta = n * (lon - lon0);

    let x = false_e + rho * theta.sin();
    let y = false_n + rho0 - rho * theta.cos();

    (x, y)
}

fn lcc_inverse(x: f64, y: f64, lat1: f64, lat2: f64, lat0: f64, lon0: f64, false_e: f64, false_n: f64) -> (f64, f64) {
    let e2 = WGS84_E2;
    let e = e2.sqrt();

    let m1 = lcc_m(lat1, e2);
    let m2 = lcc_m(lat2, e2);
    let t0 = lcc_t(lat0, e);
    let t1 = lcc_t(lat1, e);
    let t2 = lcc_t(lat2, e);

    let n = (m1.ln() - m2.ln()) / (t1.ln() - t2.ln());
    let f_val = m1 / (n * t1.powf(n));
    let rho0 = WGS84_A * f_val * t0.powf(n);

    let xp = x - false_e;
    let yp = rho0 - (y - false_n);

    let rho = (xp * xp + yp * yp).sqrt().copysign(n);
    let t = (rho / (WGS84_A * f_val)).powf(1.0 / n);
    let theta = xp.atan2(yp);

    let lon = theta / n + lon0;

    // Iterative inverse
    let mut lat = PI / 2.0 - 2.0 * t.atan();
    for _ in 0..10 {
        let e_sin = e * lat.sin();
        let new_lat = PI / 2.0 - 2.0 * (t * ((1.0 - e_sin) / (1.0 + e_sin)).powf(e / 2.0)).atan();
        if (new_lat - lat).abs() < 1e-12 {
            break;
        }
        lat = new_lat;
    }

    (lat, lon)
}

fn lcc_m(lat: f64, e2: f64) -> f64 {
    lat.cos() / (1.0 - e2 * lat.sin().powi(2)).sqrt()
}

fn lcc_t(lat: f64, e: f64) -> f64 {
    let e_sin = e * lat.sin();
    (PI / 4.0 - lat / 2.0).tan() / ((1.0 - e_sin) / (1.0 + e_sin)).powf(e / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wgs84_transform() -> GeoTransform {
        GeoTransform {
            origin_x: 0.419,
            origin_y: 74.810,
            pixel_width: 0.01,
            pixel_height: 0.01,
            width: 3249,
            height: 1750,
            crs: Crs::Wgs84,
        }
    }

    #[test]
    fn wgs84_world_to_pixel_inside() {
        let gt = wgs84_transform();
        let (col, row) = gt.world_to_pixel(10.0, 65.0).unwrap();
        assert_eq!(col, ((10.0 - 0.419) / 0.01) as u32);
        assert_eq!(row, ((74.810 - 65.0) / 0.01) as u32);
    }

    #[test]
    fn wgs84_world_to_pixel_outside() {
        let gt = wgs84_transform();
        assert!(gt.world_to_pixel(-10.0, 65.0).is_none());
        assert!(gt.world_to_pixel(10.0, 80.0).is_none());
    }

    #[test]
    fn wgs84_bbox_correct() {
        let gt = wgs84_transform();
        let bbox = gt.bbox();
        assert!((bbox[0] - 0.419).abs() < 1e-6);
        assert!((bbox[2] - (0.419 + 3249.0 * 0.01)).abs() < 1e-6);
        assert!((bbox[3] - 74.810).abs() < 1e-6);
    }

    // TM35FIN: Helsinki (24.9384, 60.1699) ≈ (388356, 6672362)
    #[test]
    fn tm35fin_roundtrip() {
        let crs = Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 27.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        };
        let (e, n) = crs.forward(24.9384, 60.1699);
        // Known approximate values for TM35FIN
        assert!((e - 385_597.0).abs() < 500.0, "easting={e}");
        assert!((n - 6_672_097.0).abs() < 500.0, "northing={n}");

        let (lon, lat) = crs.inverse(e, n);
        assert!((lon - 24.9384).abs() < 0.001, "lon={lon}");
        assert!((lat - 60.1699).abs() < 0.001, "lat={lat}");
    }

    // LAEA EPSG:3035-like: center (10, 55), false origin (1950000, -2100000)
    // OPERA radar uses this
    #[test]
    fn laea_roundtrip() {
        let crs = Crs::LambertAzimuthalEqualArea {
            lat0: 55.0_f64.to_radians(),
            lon0: 10.0_f64.to_radians(),
            false_e: 1_950_000.0,
            false_n: -2_100_000.0,
        };
        // Center of projection should map to (false_e, false_n)
        let (e, n) = crs.forward(10.0, 55.0);
        assert!((e - 1_950_000.0).abs() < 1.0, "easting={e}");
        assert!((n - (-2_100_000.0)).abs() < 1.0, "northing={n}");

        // Helsinki roundtrip
        let (e, n) = crs.forward(24.9384, 60.1699);
        let (lon, lat) = crs.inverse(e, n);
        assert!((lon - 24.9384).abs() < 0.001, "lon={lon}");
        assert!((lat - 60.1699).abs() < 0.001, "lat={lat}");
    }

    // Lambert Conformal Conic: MET Norway radar
    #[test]
    fn lcc_roundtrip() {
        let crs = Crs::LambertConformalConic {
            lat1: 58.964_f64.to_radians(),
            lat2: 69.987_f64.to_radians(),
            lat0: 0.0,
            lon0: 0.0,
            false_e: 0.0,
            false_n: 0.0,
        };
        // Oslo roundtrip
        let (e, n) = crs.forward(10.75, 59.91);
        let (lon, lat) = crs.inverse(e, n);
        assert!((lon - 10.75).abs() < 0.001, "lon={lon}");
        assert!((lat - 59.91).abs() < 0.001, "lat={lat}");
    }

    // FMI radar extreme corners — verified against GDAL gdaltransform
    #[test]
    fn tm35fin_extreme_inverse() {
        let crs = Crs::TransverseMercator {
            lat0: 0.0,
            lon0: 27.0_f64.to_radians(),
            k0: 0.9996,
            false_e: 500_000.0,
            false_n: 0.0,
        };
        // UL: (-208000, 7926000) should be ~(7.79, 70.42)
        let (lon, lat) = crs.inverse(-208_000.0, 7_926_000.0);
        assert!((lon - 7.79).abs() < 0.5, "UL lon={lon}, expected ~7.79");
        assert!((lat - 70.42).abs() < 0.5, "UL lat={lat}, expected ~70.42");

        // LR: (1072000, 6390000) should be ~(36.51, 57.29)
        let (lon, lat) = crs.inverse(1_072_000.0, 6_390_000.0);
        assert!((lon - 36.51).abs() < 0.5, "LR lon={lon}, expected ~36.51");
        assert!((lat - 57.29).abs() < 0.5, "LR lat={lat}, expected ~57.29");

        // UR: (1072000, 7926000) should be ~(42.71, 70.77)
        let (lon, lat) = crs.inverse(1_072_000.0, 7_926_000.0);
        assert!((lon - 42.71).abs() < 0.5, "UR lon={lon}, expected ~42.71");
        assert!((lat - 70.77).abs() < 0.5, "UR lat={lat}, expected ~70.77");
    }

    // OPERA LAEA extreme corners — verified against GDAL
    #[test]
    fn laea_extreme_inverse() {
        let crs = Crs::LambertAzimuthalEqualArea {
            lat0: 55.0_f64.to_radians(),
            lon0: 10.0_f64.to_radians(),
            false_e: 1_950_000.0,
            false_n: -2_100_000.0,
        };
        // UL: (-1000, 1000) should be ~(-39.57, 67.02)
        let (lon, lat) = crs.inverse(-1000.0, 1000.0);
        assert!((lon - (-39.57)).abs() < 1.0, "UL lon={lon}, expected ~-39.57");
        assert!((lat - 67.02).abs() < 1.0, "UL lat={lat}, expected ~67.02");

        // LR: (3799000, -4399000) should be ~(29.41, 31.99)
        let (lon, lat) = crs.inverse(3_799_000.0, -4_399_000.0);
        assert!((lon - 29.41).abs() < 1.0, "LR lon={lon}, expected ~29.41");
        assert!((lat - 31.99).abs() < 1.0, "LR lat={lat}, expected ~31.99");
    }

    // Test GeoTransform with projected CRS
    #[test]
    fn projected_world_to_pixel() {
        // Simplified OPERA-like raster: LAEA, origin at (-1000, 1000), 2km pixels
        let gt = GeoTransform {
            origin_x: -1000.0,
            origin_y: 1000.0,
            pixel_width: 2000.0,
            pixel_height: 2000.0,
            width: 1900,
            height: 2200,
            crs: Crs::LambertAzimuthalEqualArea {
                lat0: 55.0_f64.to_radians(),
                lon0: 10.0_f64.to_radians(),
                false_e: 1_950_000.0,
                false_n: -2_100_000.0,
            },
        };

        // Center of projection (10, 55) should map to approximately (1950000, -2100000)
        // in projected coords, which in pixel coords is ((1950000+1000)/2000, (1000+2100000)/2000)
        let pixel = gt.world_to_pixel(10.0, 55.0);
        assert!(pixel.is_some(), "Center of projection should be inside raster");
    }
}
