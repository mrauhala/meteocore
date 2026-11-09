//! Reflectivity to rain rate: the Z–R relation `Z = a·R^b` (#1379).
//!
//! `Z` is the linear reflectivity factor in mm⁶ m⁻³ (`dBZ = 10·log10 Z`)
//! and `R` the rain rate in mm/h, so `R = (10^(dBZ/10) / a)^(1/b)`. The
//! default is Marshall & Palmer (1948), `a = 200`, `b = 1.6`. Below
//! `min_dbz` an echo counts as no rain: drizzle, clutter and noise would
//! otherwise add up over a day's frames. Pure functions, no I/O.

/// A validated Z–R relation with its no-rain threshold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZR {
    a: f64,
    b: f64,
    min_dbz: f64,
}

impl ZR {
    /// `a` and `b` must be finite and positive, `min_dbz` finite.
    pub fn new(a: f64, b: f64, min_dbz: f64) -> Result<Self, String> {
        if !(a.is_finite() && a > 0.0) {
            return Err(format!("rain_rate.a must be a positive number, got {a}"));
        }
        if !(b.is_finite() && b > 0.0) {
            return Err(format!("rain_rate.b must be a positive number, got {b}"));
        }
        if !min_dbz.is_finite() {
            return Err(format!("rain_rate.min_dbz must be a number, got {min_dbz}"));
        }
        Ok(Self { a, b, min_dbz })
    }

    pub fn a(&self) -> f64 {
        self.a
    }

    pub fn b(&self) -> f64 {
        self.b
    }

    pub fn min_dbz(&self) -> f64 {
        self.min_dbz
    }

    /// The rain rate in mm/h of a reflectivity in dBZ: 0 below `min_dbz`,
    /// NaN for a non-finite input (nodata stays nodata).
    #[inline]
    pub fn rate(&self, dbz: f64) -> f64 {
        if !dbz.is_finite() {
            return f64::NAN;
        }
        if dbz < self.min_dbz {
            return 0.0;
        }
        (10f64.powf(dbz / 10.0) / self.a).powf(1.0 / self.b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marshall_palmer() -> ZR {
        ZR::new(200.0, 1.6, -1000.0).unwrap()
    }

    /// Marshall–Palmer rain rates as published for `Z = 200 R^1.6`: the
    /// table "Reflectivity in dBZ and Rainrate" of
    /// <https://en.wikipedia.org/wiki/DBZ_(meteorology)>, every row, to the
    /// decimals the table prints (the rounding is the table's).
    #[test]
    fn marshall_palmer_rates_match_the_published_table() {
        let zr = marshall_palmer();
        for (dbz, published, digits) in [
            (5.0, 0.07, 2),
            (10.0, 0.15, 2),
            (15.0, 0.3, 1),
            (20.0, 0.6, 1),
            (25.0, 1.3, 1),
            (30.0, 2.7, 1),
            (35.0, 5.6, 1),
            (40.0, 11.53, 2),
            (45.0, 23.7, 1),
            (50.0, 48.6, 1),
            (55.0, 100.0, 0),
            (60.0, 205.0, 0),
            (65.0, 421.0, 0),
        ] {
            let rate = zr.rate(dbz);
            let scale = 10f64.powi(digits);
            assert_eq!(
                (rate * scale).round() / scale,
                published,
                "{dbz} dBZ: {rate} mm/h"
            );
        }
        // Closed form, exact: 10^((dBZ/10 - log10 200) / 1.6).
        let exact = 10f64.powf((4.0 - 200f64.log10()) / 1.6);
        assert!((zr.rate(40.0) - exact).abs() < 1e-12);
        // Z = a R^b back again.
        let r = zr.rate(37.0);
        assert!((10.0 * (200.0 * r.powf(1.6)).log10() - 37.0).abs() < 1e-9);
    }

    /// The default threshold, 7 dBZ, is 0.1 mm/h; below it no rain, and
    /// nodata stays nodata.
    #[test]
    fn below_min_dbz_is_dry_and_nodata_stays_nodata() {
        let zr = ZR::new(200.0, 1.6, 7.0).unwrap();
        assert!((zr.rate(7.0) - 0.1).abs() < 2e-4, "{}", zr.rate(7.0));
        assert_eq!(zr.rate(6.99), 0.0);
        assert_eq!(zr.rate(-31.5), 0.0);
        assert!(zr.rate(f64::NAN).is_nan());
        assert!(zr.rate(f64::INFINITY).is_nan());
    }

    #[test]
    fn a_relation_must_be_positive_and_finite() {
        assert!(ZR::new(0.0, 1.6, 7.0).is_err());
        assert!(ZR::new(200.0, -1.0, 7.0).is_err());
        assert!(ZR::new(f64::NAN, 1.6, 7.0).is_err());
        assert!(ZR::new(200.0, 1.6, f64::INFINITY).is_err());
        // Another published relation: WSR-88D convective, Z = 300 R^1.4.
        let nexrad = ZR::new(300.0, 1.4, 0.0).unwrap();
        let r = nexrad.rate(40.0);
        assert!((r - 10f64.powf((4.0 - 300f64.log10()) / 1.4)).abs() < 1e-12);
    }
}
