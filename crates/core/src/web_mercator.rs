//! Single source of truth for **EPSG:3857 (spherical Web Mercator) ↔ WGS84**
//! conversions, shared by every output/bbox/tile coordinate path.
//!
//! These were previously hand-rolled in four places — `map_engine` (the
//! `OutputCrs` output axes), `ds-render`'s meta-tile assembly, `api-wms`'s
//! bbox-metres→degrees, and `api-tiles`' tile→bbox — and they **drifted**: the
//! meta-tile copy clamped latitude to ±85° for its viewport bounds while the
//! others didn't, mis-scaling assembled images vs how the client reads them and
//! displacing data ~10° toward the pole on zoomed-out views (#452). Centralising
//! the math here removes that whole failure mode.
//!
//! **All conversions are UNCLAMPED.** Web Mercator is mathematically defined for
//! any latitude in (−90°, 90°); the conventional ±85.0511° limit ([`LAT_LIMIT_DEG`])
//! is *only* about where the square tile grid is cut off, NOT a domain limit on
//! the coordinate transform. A viewport / bbox conversion must therefore never
//! clamp — a zoomed-out request legitimately reaches past ±85° toward a pole, and
//! the client maps the returned image over the FULL requested extent. Clamp to
//! [`LAT_LIMIT_DEG`] **only** when selecting tile-grid indices (no tiles exist
//! beyond it); the caller does that explicitly, e.g.
//! `lat_to_y(lat.clamp(-LAT_LIMIT_DEG, LAT_LIMIT_DEG))`.

/// EPSG:3857 sphere radius (WGS84 semi-major axis), metres. The projection is
/// spherical, so this single radius defines the whole metre scale.
pub const EARTH_RADIUS: f64 = 6_378_137.0;

/// The conventional Web Mercator latitude limit (degrees): the latitude whose
/// Mercator northing equals ±π·`EARTH_RADIUS`, i.e. the square world edge where
/// the standard tile grid is cut off. Use it **only** to clamp tile-grid index
/// selection — never a viewport/bbox conversion (see the module note).
pub const LAT_LIMIT_DEG: f64 = 85.051_128_779_806_59;

/// Longitude (degrees) → Web Mercator easting (metres).
#[inline]
pub fn lon_to_x(lon_deg: f64) -> f64 {
    EARTH_RADIUS * lon_deg.to_radians()
}

/// Web Mercator easting (metres) → longitude (degrees).
#[inline]
pub fn x_to_lon(x: f64) -> f64 {
    (x / EARTH_RADIUS).to_degrees()
}

/// Latitude (degrees) → Web Mercator northing (metres). Unclamped; `±90°` maps
/// to `±∞` (real inputs never reach it — see the module note).
#[inline]
pub fn lat_to_y(lat_deg: f64) -> f64 {
    EARTH_RADIUS * ((std::f64::consts::FRAC_PI_4 + lat_deg.to_radians() / 2.0).tan()).ln()
}

/// Web Mercator northing (metres) → latitude (degrees).
///
/// Uses `π/2 − 2·atan(exp(−y/R))`, which is algebraically equal to the textbook
/// `2·atan(exp(y/R)) − π/2` (the Gudermannian) but negates the exponent so
/// `exp()` decays toward 0 as `|y|` grows rather than overflowing — numerically
/// stable across the full range under f64.
#[inline]
pub fn y_to_lat(y: f64) -> f64 {
    (std::f64::consts::FRAC_PI_2 - 2.0 * (-y / EARTH_RADIUS).exp().atan()).to_degrees()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_across_the_full_range() {
        for &lon in &[-180.0, -123.4, 0.0, 24.94, 180.0] {
            assert!((x_to_lon(lon_to_x(lon)) - lon).abs() < 1e-9, "lon {lon}");
        }
        // Includes latitudes well past the ±85° tile limit (the #452 case).
        for &lat in &[-89.0, -85.05, -32.8, 0.0, 60.17, 85.05, 87.3, 89.0] {
            assert!((y_to_lat(lat_to_y(lat)) - lat).abs() < 1e-6, "lat {lat}");
        }
    }

    #[test]
    fn lat_limit_is_the_square_world_edge() {
        // At LAT_LIMIT_DEG the northing equals ±π·R (the world half-height).
        let edge = std::f64::consts::PI * EARTH_RADIUS;
        assert!((lat_to_y(LAT_LIMIT_DEG) - edge).abs() < 1.0);
        assert!((lat_to_y(-LAT_LIMIT_DEG) + edge).abs() < 1.0);
    }

    #[test]
    fn lat_to_y_is_not_clamped_past_the_limit() {
        // Load-bearing: a viewport conversion must NOT clamp at ±85° — past the
        // limit the northing keeps growing (this is what #452 relied on).
        assert!(
            lat_to_y(87.3) > lat_to_y(LAT_LIMIT_DEG),
            "lat_to_y must keep increasing past the ±85° tile limit, not clamp"
        );
    }

    #[test]
    fn matches_gudermannian_tile_form() {
        // api-tiles expresses the inverse as `atan(sinh(y_norm))` with
        // y_norm = y/R; assert it equals `y_to_lat` (the Gudermannian identity)
        // so consolidating that path is exact.
        for &y_norm in &[-3.0_f64, -1.0, 0.0, 0.7, 2.5] {
            let gd = y_norm.sinh().atan().to_degrees();
            assert!((y_to_lat(y_norm * EARTH_RADIUS) - gd).abs() < 1e-9);
        }
    }
}
