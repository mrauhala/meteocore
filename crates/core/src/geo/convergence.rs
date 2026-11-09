//! Grid convergence: how far a conformal grid's north turns from true north
//! (#897). Grid-relative wind components are turned by it before their
//! direction is taken ([`crate::wind`]).

use std::f64::consts::{FRAC_PI_2, PI};

use super::{conformal_latitude, rotlatlon_forward, Crs, Lcc, Stere, TmSeries};

#[cfg(test)]
mod tests;

/// The grid convergence γ of a conformal grid, point by point: the angle
/// from true north to grid north, the grid's +y axis, clockwise. It is
/// positive where grid north lies east of true north, as on a northern
/// Lambert cone east of its central meridian. PROJ's `proj -V` prints it as
/// `Convergence` with the same sign, and the tests pin it there.
///
/// A vector given along the grid's x and y axes points along east and north
/// once it is turned clockwise by γ ([`crate::wind::to_earth_relative`]).
///
/// Each projection by its exact formula, its constants computed once from
/// the [`Crs`] the grid is laid out in:
///
/// - **Lambert conformal conic:** γ = n (λ − λ₀), with n the cone constant
///   the projection itself computes from its one or two standard parallels
///   (Snyder, USGS PP 1395, eq. 15-8; sin φ₁ for one). Exact on the sphere
///   and the ellipsoid.
/// - **Polar stereographic:** the cone of a pole, n = +1 at the north pole
///   and −1 at the south: γ = ±(λ − λ₀).
/// - **Oblique stereographic:** on the conformal sphere the projection
///   projects (Snyder eqs. 21-26 to 21-30), `tan γ = sin Δλ (sin χ + sin χ₀)
///   / (cos χ cos χ₀ + (1 + sin χ sin χ₀) cos Δλ)`, χ the conformal latitude.
///   The conformal latitude keeps meridians and angles, so the sphere's
///   angle is the ellipsoid's.
/// - **Transverse Mercator:** Karney's ("Transverse Mercator with an
///   accuracy of a few nanometers", J. Geodesy 85, 2011) γ = γ′ + γ″: the
///   spherical projection's of the conformal sphere,
///   `tan γ′ = sin χ tan Δλ`, plus the turn of Krüger's series,
///   `γ″ = atan2(q′, p′)` from its derivative `p′ − i q′ = 1 + Σ 2j αⱼ cos
///   2jζ′`, with the same αⱼ the projection uses.
/// - **Rotated latitude/longitude:** minus the bearing of the geographic
///   north pole, seen from the point in the rotated frame: true north is
///   the way along the meridian to it.
///
/// A north-aligned grid (lat/lon, Mercator) has none, nor has a grid that
/// is not conformal, whose axes are not orthogonal on the ground.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Convergence(Kind);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// γ = n (λ − λ₀): a Lambert cone, or a polar stereographic plane
    /// (n = ±1).
    Conic { n: f64, lon0: f64 },
    /// An oblique stereographic, on the conformal sphere of its figure.
    ObliqueStereographic {
        lon0: f64,
        sin_chi0: f64,
        cos_chi0: f64,
        /// The figure's eccentricity; 0 on a sphere.
        e: f64,
    },
    /// A Transverse Mercator, by its Krüger series.
    TransverseMercator { lon0: f64, series: TmSeries },
    /// A rotated latitude/longitude grid: the rotation, and the geographic
    /// north pole's rotated longitude and latitude, radians.
    RotatedPole {
        south_pole_lat: f64,
        south_pole_lon: f64,
        north_lon: f64,
        north_lat: f64,
    },
}

/// How close to a pole, radians (0.6 m), a point has no north: true north
/// is undefined at a geographic pole, and a rotated grid's own axes at its.
/// Over the 2e-8 rad a rotated latitude computed by `asin` next to its
/// pole is off by.
const POLE: f64 = 1e-7;

impl Convergence {
    /// The convergence of a grid laid out in `crs`. `None` for a
    /// north-aligned grid (lat/lon, Web Mercator) and one that is not
    /// conformal (azimuthal equal-area, geostationary), and for a cone
    /// whose constant is not a number or zero: parameters that make no
    /// cone, so its convergence is unknown.
    pub fn of(crs: &Crs) -> Option<Convergence> {
        let kind = match *crs {
            Crs::LambertConformalConic {
                lat1,
                lat2,
                lat0,
                lon0,
                false_e,
                false_n,
                radius,
            } => {
                let n = Lcc::new(lat1, lat2, lat0, lon0, false_e, false_n, radius).n;
                if !n.is_finite() || n == 0.0 {
                    return None;
                }
                Kind::Conic { n, lon0 }
            }
            Crs::Stereographic {
                lat0,
                lon0,
                k0,
                false_e,
                false_n,
                figure,
            } => {
                let stere = Stere::new(lat0, lon0, k0, false_e, false_n, &figure);
                if stere.polar() {
                    Kind::Conic {
                        n: lat0.signum(),
                        lon0,
                    }
                } else {
                    let (sin_chi0, cos_chi0) = conformal_latitude(lat0, stere.e).sin_cos();
                    Kind::ObliqueStereographic {
                        lon0,
                        sin_chi0,
                        cos_chi0,
                        e: stere.e,
                    }
                }
            }
            Crs::TransverseMercator { lon0, figure, .. } => Kind::TransverseMercator {
                lon0,
                series: TmSeries::of(&figure),
            },
            Crs::RotatedLatLon {
                south_pole_lat,
                south_pole_lon,
            } => {
                let (north_lon, north_lat) =
                    rotlatlon_forward(FRAC_PI_2, 0.0, south_pole_lat, south_pole_lon);
                Kind::RotatedPole {
                    south_pole_lat,
                    south_pole_lon,
                    north_lon: north_lon.to_radians(),
                    north_lat: north_lat.to_radians(),
                }
            }
            Crs::Wgs84
            | Crs::WebMercator
            | Crs::LambertAzimuthalEqualArea { .. }
            | Crs::Geostationary { .. } => return None,
        };
        Some(Convergence(kind))
    }

    /// γ at `(lon_deg, lat_deg)`, in degrees within ±180°. `None` at a
    /// pole, where true north is undefined, at a rotated grid's own pole,
    /// and for a point that is not finite.
    pub fn degrees(&self, lon_deg: f64, lat_deg: f64) -> Option<f64> {
        if !(lon_deg.is_finite() && lat_deg.is_finite()) {
            return None;
        }
        let (lon, lat) = (lon_deg.to_radians(), lat_deg.to_radians());
        if FRAC_PI_2 - lat.abs() < POLE {
            return None;
        }
        let gamma = match self.0 {
            Kind::Conic { n, lon0 } => n * longitude_from(lon, lon0),
            Kind::ObliqueStereographic {
                lon0,
                sin_chi0,
                cos_chi0,
                e,
            } => {
                let (sin_chi, cos_chi) = conformal_latitude(lat, e).sin_cos();
                let (sin_dl, cos_dl) = longitude_from(lon, lon0).sin_cos();
                (sin_dl * (sin_chi + sin_chi0))
                    .atan2(cos_chi * cos_chi0 + (1.0 + sin_chi * sin_chi0) * cos_dl)
            }
            Kind::TransverseMercator { lon0, ref series } => {
                tm_convergence(series, lat, longitude_from(lon, lon0))
            }
            Kind::RotatedPole {
                south_pole_lat,
                south_pole_lon,
                north_lon,
                north_lat,
            } => {
                let (lon_r, lat_r) = rotlatlon_forward(lat, lon, south_pole_lat, south_pole_lon);
                let (lon_r, lat_r) = (lon_r.to_radians(), lat_r.to_radians());
                if FRAC_PI_2 - lat_r.abs() < POLE {
                    return None;
                }
                // The initial bearing from the point to the north pole, in
                // the rotated frame.
                let dl = north_lon - lon_r;
                let bearing = (dl.sin() * north_lat.cos()).atan2(
                    lat_r.cos() * north_lat.sin() - lat_r.sin() * north_lat.cos() * dl.cos(),
                );
                -bearing
            }
        };
        let degrees = gamma.to_degrees();
        degrees.is_finite().then_some(degrees)
    }
}

/// `lon − lon0`, radians, into [−π, π): the side of the central meridian
/// the point lies on, whatever turn its longitude is written in.
fn longitude_from(lon: f64, lon0: f64) -> f64 {
    (lon - lon0 + PI).rem_euclid(2.0 * PI) - PI
}

/// Karney's γ′ + γ″ of a Transverse Mercator at latitude `lat` and
/// longitude `lam` from its central meridian, radians.
fn tm_convergence(series: &TmSeries, lat: f64, lam: f64) -> f64 {
    let (sin_lat, cos_lat) = lat.sin_cos();
    let sigma = series.sigma(sin_lat);
    // τ′ = tan χ, as `TmSeries::xi_eta` computes it.
    let taup = (sin_lat * sigma.hypot(1.0) - sigma) / cos_lat;
    let (sin_lam, cos_lam) = lam.sin_cos();
    let xip = taup.atan2(cos_lam);
    let etap = (sin_lam / taup.hypot(cos_lam)).asinh();
    // tan γ′ = sin χ tan λ, sin χ = τ′ / √(1 + τ′²).
    let spherical = (taup * sin_lam).atan2(taup.hypot(1.0) * cos_lam);
    // dζ/dζ′ = p′ − i q′.
    let (mut p, mut q) = (1.0, 0.0);
    for (j, alpha) in (1..).zip(series.alpha) {
        let k = f64::from(2 * j);
        p += k * alpha * (k * xip).cos() * (k * etap).cosh();
        q += k * alpha * (k * xip).sin() * (k * etap).sinh();
    }
    spherical + q.atan2(p)
}
