//! Minimal PROJ-string parser for ODIM `/where/projdef` values.
//!
//! ODIM_H5 stores the grid projection as a PROJ.4 string (e.g.
//! `+proj=stere +lat_0=90 +lon_0=0 +lat_ts=60 +R=6371228 +x_0=0 +y_0=0`).
//! This module hand-parses the four projections we encounter across
//! European weather radar producers — `stere`, `tmerc`, `laea`, and
//! `longlat` — and maps each into a [`ds_core::geo::Crs`] variant.
//!
//! Out of scope (Phase 1 — see [[project_odim_engine_plan]] for the
//! consolidated plan):
//! - Arbitrary PROJ.4 strings beyond the four supported `+proj=` values
//! - Datum-shift parameters (`+towgs84`, `+nadgrids`, …) — ODIM v2.x
//!   uses sphere-based projections so these don't appear in COMP files
//! - WKT / PROJ-JSON
//!
//! When the input contains an unsupported `+proj=` or a malformed
//! parameter, return an error rather than silently falling back — a
//! quiet projection mismatch would surface as wrong pixel-to-world
//! mappings far downstream.

use std::collections::HashMap;

use ds_core::geo::Crs;

/// Errors from [`parse`].
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    #[error("PROJ string is missing the required `+proj=` token")]
    MissingProj,
    #[error("unsupported `+proj={0}` (Phase 1 supports stere/tmerc/laea/longlat)")]
    UnsupportedProj(String),
    #[error("PROJ parameter `{param}={value}` is not a valid number")]
    InvalidNumber { param: String, value: String },
    #[error("PROJ parameter `+{0}` is required for `+proj={1}` but missing")]
    MissingParam(&'static str, &'static str),
    #[error(
        "PROJ parameter `+R={radius}` indicates a sphere-based projection, but Phase 1's stere_forward/stere_inverse hardcodes WGS84 ellipsoid constants. Producer `xscale`/`yscale` would be in sphere metres while our forward output is in ellipsoid metres — bbox would be off by ~{percent:.3}%. Tracked as a follow-up; for now ship `+ellps=WGS84` instead, or omit `+R=` (which we then treat as WGS84)."
    )]
    SphereProjection { radius: f64, percent: f64 },
}

/// Parse a PROJ.4 string from an ODIM `/where/projdef` value into a
/// [`Crs`].
///
/// Recognised parameters per projection:
/// - `stere`: `+lat_0`, `+lon_0`, `+k_0`/`+k`, `+lat_ts`, `+x_0`, `+y_0`
/// - `tmerc`: `+lat_0`, `+lon_0`, `+k_0`/`+k`, `+x_0`, `+y_0`
/// - `laea`:  `+lat_0`, `+lon_0`, `+x_0`, `+y_0`
/// - `longlat`: (no parameters needed beyond `+proj=longlat`)
///
/// Unrecognised parameters (`+ellps=`, `+datum=`, `+units=`,
/// `+no_defs`, etc.) are silently ignored — for the three Phase 1
/// tested producers (DMI, DWD, OPERA, all `+ellps=WGS84`) the
/// hardcoded WGS84 constants in `stere_forward` already match.
///
/// **`+R=` is the exception**: if a producer ships a custom sphere
/// radius (e.g. FMI's `+R=6371228`), `stere_forward` would still
/// compute WGS84-ellipsoid metres while the file's `xscale`/`yscale`
/// are in sphere metres — producing a bbox that's wrong by ~0.1%
/// (~5 px at OPERA scale). Until we add sphere-radius support
/// (carry `R` through `Crs::Stereographic` and either scale or
/// branch in `stere_forward`), reject `+R=` with a clear error
/// when it differs materially from `WGS84_A`. A value within
/// `WGS84_A_TOLERANCE` is accepted (some producers ship
/// `+R=6378137` as a no-op).
///
/// For polar stereographic (`+lat_0=±90`) with `+lat_ts` and without an
/// explicit `+k_0`, the scale factor is computed on the sphere as
/// `k0 = (1 + sin|lat_ts|) / 2`. This is the standard PROJ relationship
/// for the FMI/OPERA polar composites where `lat_ts=60` is typical.
pub fn parse(projdef: &str) -> Result<Crs, ParseError> {
    let params = tokenize(projdef);

    let proj = params.get("proj").ok_or(ParseError::MissingProj)?.as_str();

    match proj {
        "longlat" | "latlong" | "lonlat" | "latlon" => Ok(Crs::Wgs84),
        "stere" => parse_stere(&params),
        "tmerc" => parse_tmerc(&params),
        "laea" => parse_laea(&params),
        other => Err(ParseError::UnsupportedProj(other.to_string())),
    }
}

/// Split a PROJ.4 string into `key → value` pairs. Bare flags
/// (`+no_defs`) parse to an empty string. The leading `+` is stripped.
/// Whitespace is the only separator — PROJ doesn't allow quoting and
/// neither do we.
fn tokenize(projdef: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for token in projdef.split_whitespace() {
        let Some(body) = token.strip_prefix('+') else {
            continue;
        };
        match body.split_once('=') {
            Some((k, v)) => {
                out.insert(k.to_string(), v.to_string());
            }
            None => {
                out.insert(body.to_string(), String::new());
            }
        }
    }
    out
}

fn parse_f64(params: &HashMap<String, String>, key: &str) -> Result<Option<f64>, ParseError> {
    match params.get(key) {
        None => Ok(None),
        Some(v) => v
            .parse::<f64>()
            .map(Some)
            .map_err(|_| ParseError::InvalidNumber {
                param: key.to_string(),
                value: v.clone(),
            }),
    }
}

/// `+k_0` is the canonical form; `+k` is the historical shorthand
/// PROJ still accepts. Prefer `+k_0` when both appear.
fn parse_k0(params: &HashMap<String, String>) -> Result<Option<f64>, ParseError> {
    if params.contains_key("k_0") {
        parse_f64(params, "k_0")
    } else {
        parse_f64(params, "k")
    }
}

/// WGS84 semi-major axis in metres (matches `ds_core::geo::WGS84_A`).
const WGS84_A: f64 = 6_378_137.0;
/// Tolerance for accepting `+R=` as WGS84-compatible. The producer
/// is reaffirming the WGS84 sphere within ~1 metre.
const WGS84_A_TOLERANCE: f64 = 1.0;

/// Reject `+R=` when it differs materially from `WGS84_A`. See the
/// module-level doc for rationale: until `Crs::Stereographic`
/// carries a radius, the file's `xscale`/`yscale` (in producer
/// sphere metres) would be combined with `stere_forward` output
/// (in WGS84 ellipsoid metres) and the bbox would drift.
fn check_radius(params: &HashMap<String, String>) -> Result<(), ParseError> {
    if let Some(r) = parse_f64(params, "R")? {
        if (r - WGS84_A).abs() > WGS84_A_TOLERANCE {
            let percent = (r - WGS84_A).abs() / WGS84_A * 100.0;
            return Err(ParseError::SphereProjection { radius: r, percent });
        }
    }
    Ok(())
}

fn parse_stere(params: &HashMap<String, String>) -> Result<Crs, ParseError> {
    check_radius(params)?;
    let lat0 = parse_f64(params, "lat_0")?.unwrap_or(0.0);
    let lon0 = parse_f64(params, "lon_0")?.unwrap_or(0.0);
    let false_e = parse_f64(params, "x_0")?.unwrap_or(0.0);
    let false_n = parse_f64(params, "y_0")?.unwrap_or(0.0);

    let k0 = match parse_k0(params)? {
        Some(k) => k,
        None => match parse_f64(params, "lat_ts")? {
            // Derive k0 from lat_ts so the scale at the latitude-of-
            // true-scale is exactly 1. Two cases:
            //
            // Polar (lat_0 = ±90): PROJ's `+proj=stere` uses Snyder
            // eq. 21-39 `ρ = a·m_c·t/t_c` with lat_ts directly. To
            // make our case-1 formula `ρ = 2·a·k₀·t/D` match exactly
            // we need the ellipsoidal-corrected k0:
            //
            //   k0 = m_c · D / (2 · t_c)
            //
            // where:
            //   m_c = cos(lat_ts) / √(1 - e²·sin²(lat_ts))
            //   t_c = tan(π/4 - lat_ts/2) · ((1+e·sin lat_ts)/(1-e·sin lat_ts))^(e/2)
            //   D   = √((1+e)^(1+e) · (1-e)^(1-e))
            //
            // (Snyder, "Map Projections — A Working Manual", USGS
            // PP 1395, 1987.) The spherical shortcut
            // `(1 + sin|lat_ts|)/2` is ~0.012% off, which translates
            // to ~200 m on a 3000 km radius — observable in
            // `stereographic_inverse_absolute_polar`.
            //
            // Oblique (|lat_0| < π/2): no comparable case-2 formula
            // applies; ODIM oblique producers (DMI) ship +k=1
            // explicitly rather than relying on lat_ts, so this
            // branch only triggers for hypothetical oblique
            // configs and we keep the general spherical form.
            Some(lat_ts) => {
                let lat0_rad = lat0.to_radians();
                let lat_ts_rad = lat_ts.to_radians();
                if (lat0_rad.abs() - std::f64::consts::FRAC_PI_2).abs() < 1e-10 {
                    // WGS84: e² = 2f - f² with f = 1/298.257223563.
                    // Kept local to avoid exporting an internal
                    // constant from `ds_core::geo` for this one
                    // call site.
                    let flat: f64 = 1.0 / 298.257_223_563;
                    let e2 = 2.0 * flat - flat * flat;
                    let e: f64 = e2.sqrt();
                    let sin_lat_ts = lat_ts_rad.sin();
                    let cos_lat_ts = lat_ts_rad.cos();
                    let m_c = cos_lat_ts / (1.0 - e2 * sin_lat_ts * sin_lat_ts).sqrt();
                    let t_c = (std::f64::consts::FRAC_PI_4 - lat_ts_rad / 2.0).tan()
                        * ((1.0 + e * sin_lat_ts) / (1.0 - e * sin_lat_ts)).powf(e / 2.0);
                    let d = ((1.0 + e).powf(1.0 + e) * (1.0 - e).powf(1.0 - e)).sqrt();
                    m_c * d / (2.0 * t_c)
                } else {
                    (1.0 + lat0_rad.sin() * lat_ts_rad.sin() + lat0_rad.cos() * lat_ts_rad.cos())
                        / 2.0
                }
            }
            // No +k_0, no +lat_ts → PROJ default is 1.0.
            None => 1.0,
        },
    };

    Ok(Crs::Stereographic {
        lat0: lat0.to_radians(),
        lon0: lon0.to_radians(),
        k0,
        false_e,
        false_n,
    })
}

fn parse_tmerc(params: &HashMap<String, String>) -> Result<Crs, ParseError> {
    check_radius(params)?;
    let lat0 = parse_f64(params, "lat_0")?.unwrap_or(0.0);
    let lon0 = parse_f64(params, "lon_0")?.unwrap_or(0.0);
    let k0 = parse_k0(params)?.unwrap_or(1.0);
    let false_e = parse_f64(params, "x_0")?.unwrap_or(0.0);
    let false_n = parse_f64(params, "y_0")?.unwrap_or(0.0);

    Ok(Crs::TransverseMercator {
        lat0: lat0.to_radians(),
        lon0: lon0.to_radians(),
        k0,
        false_e,
        false_n,
    })
}

fn parse_laea(params: &HashMap<String, String>) -> Result<Crs, ParseError> {
    check_radius(params)?;
    let lat0 = parse_f64(params, "lat_0")?.unwrap_or(0.0);
    let lon0 = parse_f64(params, "lon_0")?.unwrap_or(0.0);
    let false_e = parse_f64(params, "x_0")?.unwrap_or(0.0);
    let false_n = parse_f64(params, "y_0")?.unwrap_or(0.0);

    Ok(Crs::LambertAzimuthalEqualArea {
        lat0: lat0.to_radians(),
        lon0: lon0.to_radians(),
        false_e,
        false_n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts two `Crs` values are equal across float-comparison
    /// tolerance. The enum doesn't derive `PartialEq` because it carries
    /// floats, so we destructure each variant and compare individually.
    fn assert_crs_eq(actual: &Crs, expected: &Crs) {
        const EPS: f64 = 1e-9;
        match (actual, expected) {
            (Crs::Wgs84, Crs::Wgs84) => {}
            (
                Crs::Stereographic {
                    lat0: a_lat0,
                    lon0: a_lon0,
                    k0: a_k0,
                    false_e: a_e,
                    false_n: a_n,
                },
                Crs::Stereographic {
                    lat0: e_lat0,
                    lon0: e_lon0,
                    k0: e_k0,
                    false_e: e_e,
                    false_n: e_n,
                },
            ) => {
                assert!((a_lat0 - e_lat0).abs() < EPS, "lat0: {a_lat0} vs {e_lat0}");
                assert!((a_lon0 - e_lon0).abs() < EPS, "lon0: {a_lon0} vs {e_lon0}");
                assert!((a_k0 - e_k0).abs() < EPS, "k0: {a_k0} vs {e_k0}");
                assert!((a_e - e_e).abs() < EPS, "false_e: {a_e} vs {e_e}");
                assert!((a_n - e_n).abs() < EPS, "false_n: {a_n} vs {e_n}");
            }
            (
                Crs::TransverseMercator {
                    lat0: a_lat0,
                    lon0: a_lon0,
                    k0: a_k0,
                    false_e: a_e,
                    false_n: a_n,
                },
                Crs::TransverseMercator {
                    lat0: e_lat0,
                    lon0: e_lon0,
                    k0: e_k0,
                    false_e: e_e,
                    false_n: e_n,
                },
            ) => {
                assert!((a_lat0 - e_lat0).abs() < EPS);
                assert!((a_lon0 - e_lon0).abs() < EPS);
                assert!((a_k0 - e_k0).abs() < EPS);
                assert!((a_e - e_e).abs() < EPS);
                assert!((a_n - e_n).abs() < EPS);
            }
            (
                Crs::LambertAzimuthalEqualArea {
                    lat0: a_lat0,
                    lon0: a_lon0,
                    false_e: a_e,
                    false_n: a_n,
                },
                Crs::LambertAzimuthalEqualArea {
                    lat0: e_lat0,
                    lon0: e_lon0,
                    false_e: e_e,
                    false_n: e_n,
                },
            ) => {
                assert!((a_lat0 - e_lat0).abs() < EPS);
                assert!((a_lon0 - e_lon0).abs() < EPS);
                assert!((a_e - e_e).abs() < EPS);
                assert!((a_n - e_n).abs() < EPS);
            }
            (a, e) => panic!("CRS variant mismatch: {a:?} vs {e:?}"),
        }
    }

    #[test]
    fn longlat_maps_to_wgs84() {
        let crs = parse("+proj=longlat +ellps=WGS84 +no_defs").unwrap();
        assert_crs_eq(&crs, &Crs::Wgs84);
    }

    /// DMI's Denmark national composite uses *oblique* stereographic
    /// with `+lat_0=56 +lat_ts=56`. Under the general spherical
    /// scale-factor formula, when `lat_ts == lat_0` the scale at the
    /// origin is unity, so `k0 = 1.0`. The naive polar-only formula
    /// `(1 + sin|lat_ts|)/2` would give ~0.914 — a 9% error that
    /// expands the projected grid bbox by ~50% and causes ~100% of
    /// output pixels to land outside the source data when sampling.
    #[test]
    fn dmi_oblique_stereographic_lat_ts_equals_lat_0_gives_k0_one() {
        let crs = parse("+proj=stere +ellps=WGS84 +lat_0=56 +lon_0=10.5666 +lat_ts=56").unwrap();
        match crs {
            Crs::Stereographic { k0, .. } => {
                assert!(
                    (k0 - 1.0).abs() < 1e-9,
                    "DMI oblique stereographic with lat_ts=lat_0 must give k0=1, got {k0}"
                );
            }
            other => panic!("expected Stereographic, got {other:?}"),
        }
    }

    /// FMI/OPERA polar stereographic composite — the canonical Phase 1
    /// shape. `+lat_ts=60` converts to the **ellipsoidal** k0 such
    /// that `Crs::Stereographic`'s case-1 forward formula matches
    /// PROJ's case-2 forward formula (`+proj=stere +lat_ts=…` uses
    /// Snyder eq. 21-39). The spherical shortcut `(1 + sin|lat_ts|)/2`
    /// is ~0.012% off and produces ~200 m error on a 3000 km radius;
    /// the absolute-coord test
    /// `core::geo::stereographic_inverse_absolute_polar` would
    /// regress if this conversion drifted.
    ///
    /// Expected value computed offline:
    ///   m_c = cos(60°)/√(1 - e²·sin²(60°))
    ///   t_c = tan(15°) · ((1+e·sin60°)/(1−e·sin60°))^(e/2)
    ///   D   = √((1+e)^(1+e) · (1−e)^(1−e))
    ///   k0  = m_c · D / (2 · t_c)  ≈  0.9330690717363566
    #[test]
    fn fmi_polar_stereographic_with_lat_ts_converts_to_k0() {
        // Use `+ellps=WGS84` rather than the historical FMI
        // `+R=6371228`; the sphere-radius case is now rejected
        // explicitly by `check_radius` and exercised by
        // `sphere_radius_distinct_from_wgs84_is_rejected`.
        let crs =
            parse("+proj=stere +lat_0=90 +lon_0=0 +lat_ts=60 +ellps=WGS84 +x_0=0 +y_0=0 +no_defs")
                .unwrap();

        assert_crs_eq(
            &crs,
            &Crs::Stereographic {
                lat0: 90f64.to_radians(),
                lon0: 0.0,
                k0: 0.933_069_071_736_356_6,
                false_e: 0.0,
                false_n: 0.0,
            },
        );
    }

    /// Single-site oblique stereographic. `+k_0=1` is the explicit
    /// form; no `+lat_ts`. False easting/northing carry through.
    #[test]
    fn oblique_stereographic_with_explicit_k0() {
        let crs = parse("+proj=stere +lat_0=56 +lon_0=10.5667 +k_0=1.0 +x_0=0 +y_0=0 +ellps=WGS84")
            .unwrap();

        assert_crs_eq(
            &crs,
            &Crs::Stereographic {
                lat0: 56f64.to_radians(),
                lon0: 10.5667f64.to_radians(),
                k0: 1.0,
                false_e: 0.0,
                false_n: 0.0,
            },
        );
    }

    /// `+k` (no underscore) is PROJ's historical shorthand for `+k_0`.
    /// We accept either; both forms appear in real-world ODIM files.
    #[test]
    fn stere_accepts_legacy_k_shorthand() {
        let crs = parse("+proj=stere +lat_0=90 +lon_0=0 +k=0.933 +x_0=0 +y_0=0").unwrap();
        assert_crs_eq(
            &crs,
            &Crs::Stereographic {
                lat0: 90f64.to_radians(),
                lon0: 0.0,
                k0: 0.933,
                false_e: 0.0,
                false_n: 0.0,
            },
        );
    }

    /// EPSG:3067 TM35FIN — the projection FMI publishes single-site
    /// radar data in.
    #[test]
    fn tmerc_epsg3067_tm35fin() {
        let crs = parse("+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +x_0=500000 +y_0=0 +ellps=GRS80")
            .unwrap();

        assert_crs_eq(
            &crs,
            &Crs::TransverseMercator {
                lat0: 0.0,
                lon0: 27f64.to_radians(),
                k0: 0.9996,
                false_e: 500_000.0,
                false_n: 0.0,
            },
        );
    }

    /// EPSG:3035 ETRS89-LAEA — the OPERA pan-European grid.
    #[test]
    fn laea_epsg3035_etrs89() {
        let crs =
            parse("+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80").unwrap();

        assert_crs_eq(
            &crs,
            &Crs::LambertAzimuthalEqualArea {
                lat0: 52f64.to_radians(),
                lon0: 10f64.to_radians(),
                false_e: 4_321_000.0,
                false_n: 3_210_000.0,
            },
        );
    }

    /// Missing `+proj=` is the most common malformed-input case: a
    /// truncated PROJ string from a partial HDF5 read. Surface it as
    /// a structured error rather than silently defaulting to lon/lat.
    #[test]
    fn missing_proj_token_is_an_error() {
        let err = parse("+lat_0=90 +lon_0=0").unwrap_err();
        assert_eq!(err, ParseError::MissingProj);
    }

    /// Unsupported `+proj=` should name the offender so an operator
    /// can decide whether to add support or fix the file.
    #[test]
    fn unsupported_proj_lists_the_value() {
        let err = parse("+proj=merc +lat_0=0").unwrap_err();
        assert_eq!(err, ParseError::UnsupportedProj("merc".into()));
    }

    /// A non-numeric `+lat_0` is a real-world failure mode for files
    /// written by buggy producers (e.g. ASCII garbage spliced into the
    /// projdef). Refuse with a structured error.
    #[test]
    fn non_numeric_parameter_is_an_error() {
        let err = parse("+proj=stere +lat_0=ninety +lon_0=0").unwrap_err();
        assert_eq!(
            err,
            ParseError::InvalidNumber {
                param: "lat_0".into(),
                value: "ninety".into(),
            }
        );
    }

    /// Bare flags like `+no_defs` mustn't perturb anything. PROJ
    /// strings shipped by real producers always include at least one
    /// such flag.
    #[test]
    fn bare_flags_are_ignored() {
        let crs = parse("+proj=longlat +no_defs +wktext").unwrap();
        assert_crs_eq(&crs, &Crs::Wgs84);
    }

    /// All four `longlat` aliases PROJ recognises. ODIM v2.x doesn't
    /// distinguish between them so neither should we.
    #[test]
    fn all_longlat_aliases_map_to_wgs84() {
        for proj in ["longlat", "latlong", "lonlat", "latlon"] {
            let projdef = format!("+proj={proj}");
            let crs = parse(&projdef).unwrap();
            assert_crs_eq(&crs, &Crs::Wgs84);
        }
    }

    /// `+R=6371228` (the common ODIM/PROJ "mean Earth radius") is
    /// rejected with `SphereProjection` to prevent silent bbox
    /// drift versus the WGS84 ellipsoid metres our `stere_forward`
    /// hardcodes. Phase 1's three tested producers ship
    /// `+ellps=WGS84` (no `+R=`) so this never fires in practice;
    /// the rejection is here so a future FMI / KNMI sphere-based
    /// producer fails loudly at config-load time instead of
    /// silently producing tiles ~0.1% off.
    #[test]
    fn sphere_radius_distinct_from_wgs84_is_rejected() {
        let err = parse("+proj=stere +lat_0=90 +lon_0=25 +lat_ts=60 +R=6371228").unwrap_err();
        match err {
            ParseError::SphereProjection { radius, .. } => {
                assert!((radius - 6_371_228.0).abs() < 0.5);
            }
            other => panic!("expected SphereProjection, got {other:?}"),
        }
    }

    /// `+R=6378137` (= `WGS84_A` exactly) is accepted as a no-op.
    /// Some producers re-declare the WGS84 semi-major axis as
    /// `+R=` redundantly; rejecting that would create a false
    /// alarm.
    #[test]
    fn sphere_radius_matching_wgs84_a_is_accepted() {
        let crs = parse("+proj=stere +lat_0=56 +lon_0=10.5666 +k=1 +R=6378137").unwrap();
        assert!(matches!(crs, Crs::Stereographic { .. }));
    }

    /// Same rejection rule applies to tmerc and laea — the bbox
    /// unit-mismatch concern isn't projection-specific.
    #[test]
    fn sphere_radius_rejected_for_tmerc_and_laea() {
        assert!(matches!(
            parse("+proj=tmerc +lat_0=0 +lon_0=27 +k=0.9996 +R=6371228"),
            Err(ParseError::SphereProjection { .. })
        ));
        assert!(matches!(
            parse("+proj=laea +lat_0=52 +lon_0=10 +R=6371228"),
            Err(ParseError::SphereProjection { .. })
        ));
    }

    /// `thiserror` supports `{name:fmt}` format specs (not just
    /// bare `{name}`) — the `#[error(...)]` body desugars to a
    /// standard `write!` so anything `format!` accepts works. This
    /// test pins that contract: the `{percent:.3}` spec in
    /// `ParseError::SphereProjection` must render the numeric
    /// value with three decimal digits, not the literal text
    /// `{percent:.3}`.
    #[test]
    fn sphere_projection_error_renders_percent_format_spec() {
        let err = ParseError::SphereProjection {
            radius: 6_371_228.0,
            percent: 0.108_125,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("0.108"),
            "expected `{{percent:.3}}` to render the value, got: {msg}"
        );
        assert!(
            !msg.contains("{percent"),
            "thiserror format spec rendered literally: {msg}"
        );
    }

    /// Defaults: stere with no parameters except `+proj=stere` should
    /// produce a CRS with `lat0=0, lon0=0, k0=1.0, false_e=false_n=0`.
    /// PROJ has the same defaults; surprising a downstream caller with
    /// a different-flavoured "default" would mask bugs.
    #[test]
    fn stere_defaults_match_proj_defaults() {
        let crs = parse("+proj=stere").unwrap();
        assert_crs_eq(
            &crs,
            &Crs::Stereographic {
                lat0: 0.0,
                lon0: 0.0,
                k0: 1.0,
                false_e: 0.0,
                false_n: 0.0,
            },
        );
    }
}
