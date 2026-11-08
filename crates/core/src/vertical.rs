//! Shared vertical-dimension descriptor.
//!
//! A collection exposes at most one vertical axis (issue #185). The same
//! [`VerticalDimension`] value is surfaced on both the Map surface
//! (`RasterInfo`) and the EDR surface (`EdrEngine::get_vertical_extent`) so
//! the two never diverge.

/// The kind of vertical coordinate a collection's levels represent.
///
/// Vertical coordinates are heterogeneous in *kind*, not just value, so the
/// kind has to be carried explicitly: WMS needs a `units=` on the
/// `ELEVATION` dimension and CoverageJSON needs a vertical CRS in the domain
/// `referencing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalKind {
    /// Pressure level (hPa). Increasing value is downward.
    Pressure,
    /// Height above ground or mean sea level (m). Increasing value is upward.
    Height,
    /// Radar beam elevation angle (degrees). Increasing value is upward.
    ElevationAngle,
    /// Metres above a radar antenna, used as the cross-section vertical
    /// axis (`Section` domain). Distinct from `Height` so the
    /// CoverageJSON `VerticalCRS` axis can carry the antenna-relative
    /// label rather than a generic "Height" — the value is *not* metres
    /// above mean sea level.
    HeightAboveAntenna,
    /// Ordinal model / hybrid level. Increasing value is downward.
    ModelLevel,
    /// Isentropic (potential-temperature) level (K). Increasing value is upward.
    Isentropic,
}

impl VerticalKind {
    /// Canonical unit symbol — also the WMS `Dimension units=` value.
    pub fn default_unit(self) -> &'static str {
        match self {
            VerticalKind::Pressure => "hPa",
            VerticalKind::Height => "m",
            VerticalKind::ElevationAngle => "deg",
            VerticalKind::HeightAboveAntenna => "m",
            VerticalKind::ModelLevel => "1",
            VerticalKind::Isentropic => "K",
        }
    }

    /// Default human-readable axis label.
    pub fn default_label(self) -> &'static str {
        match self {
            VerticalKind::Pressure => "Pressure",
            VerticalKind::Height => "Height",
            VerticalKind::ElevationAngle => "Elevation angle",
            VerticalKind::HeightAboveAntenna => "Height above antenna",
            VerticalKind::ModelLevel => "Model level",
            VerticalKind::Isentropic => "Isentropic level",
        }
    }

    /// Direction of increasing coordinate value, as used by a CoverageJSON
    /// `VerticalCRS` coordinate-system axis.
    pub fn direction(self) -> &'static str {
        match self {
            VerticalKind::Pressure | VerticalKind::ModelLevel => "down",
            VerticalKind::Height
            | VerticalKind::ElevationAngle
            | VerticalKind::HeightAboveAntenna
            | VerticalKind::Isentropic => "up",
        }
    }

    /// Vertical reference system string for OGC EDR collection metadata
    /// (`extent.vertical.vrs`). The OGC EDR 1.1 schema marks `vrs` as a
    /// required `string` with no `format` constraint, so any string
    /// satisfies it; we prefer a resolvable EPSG URI when one exists
    /// (`Height` → EPSG:5714) and fall back to an inline WKT2
    /// `VERTCRS[...]` for kinds with no registered URI (radar elevation,
    /// height-above-antenna, atmospheric pressure, model level,
    /// isentropic).
    ///
    /// **Tradeoff (claude-review on PR #275)**: the JSON schema is
    /// satisfied either way, but strict OGC CITE / EDR-spec clients
    /// sometimes verify that `vrs` is a *resolvable* OGC or EPSG URI
    /// rather than an inline WKT — so the inline-WKT kinds may fail
    /// such conformance checks, trading "vrs absent" (the prior
    /// behaviour) for "vrs unresolvable". An invented placeholder URI
    /// would be worse (silently wrong, the way `EPSG:5798` was for
    /// pressure before this rewrite), and there is no public OGC
    /// namespace for these custom verticals, so WKT2 is the right
    /// pragmatic choice. Override per-kind here if a future EPSG/OGC
    /// registration covers one of these.
    pub fn vrs(self) -> &'static str {
        match self {
            // EPSG:5714 = "MSL height", the closest match for altimetric height.
            VerticalKind::Height => "http://www.opengis.net/def/crs/EPSG/0/5714",
            // EPSG has no standard URI for an atmospheric pressure CRS
            // (an earlier draft of this code referenced EPSG:5798, which
            // is actually "EGM96 height" — wrong kind entirely; flagged
            // by claude-review on PR #275). Emit an inline WKT2 instead.
            // VDATUM is "Atmosphere" rather than "Mean sea level" — MSL
            // is an altimetric gravity-surface datum, semantically wrong
            // for a pressure axis. "Atmosphere" matches how NWP vertical
            // CRS definitions describe atmospheric levels (GRIB2 §3.0).
            // Flagged by claude-review on PR #275.
            VerticalKind::Pressure => {
                "VERTCRS[\"Atmospheric pressure\",\
                 VDATUM[\"Atmosphere\"],\
                 CS[vertical,1],\
                 AXIS[\"Pressure (hPa)\",down],\
                 UNIT[\"hPa\",100]]"
            }
            VerticalKind::ElevationAngle => {
                "VERTCRS[\"Elevation angle\",\
                 VDATUM[\"Antenna horizon\"],\
                 CS[vertical,1],\
                 AXIS[\"Elevation (deg)\",up],\
                 UNIT[\"degree\",0.0174532925199433]]"
            }
            VerticalKind::HeightAboveAntenna => {
                "VERTCRS[\"Height above antenna\",\
                 VDATUM[\"Antenna phase centre\"],\
                 CS[vertical,1],\
                 AXIS[\"Height (m)\",up],\
                 UNIT[\"metre\",1]]"
            }
            VerticalKind::ModelLevel => {
                "VERTCRS[\"Model level\",\
                 VDATUM[\"Model surface\"],\
                 CS[vertical,1],\
                 AXIS[\"Level\",down],\
                 UNIT[\"unity\",1]]"
            }
            VerticalKind::Isentropic => {
                "VERTCRS[\"Isentropic\",\
                 VDATUM[\"Potential temperature\"],\
                 CS[vertical,1],\
                 AXIS[\"Theta (K)\",up],\
                 UNIT[\"kelvin\",1]]"
            }
        }
    }
}

/// A collection's single vertical axis: the kind of coordinate plus the
/// discrete levels available for querying.
///
/// The label and unit are derived from [`VerticalKind`] (see
/// [`label`](Self::label) / [`unit`](Self::unit)) so there is a single
/// source of truth — the CoverageJSON `VerticalCRS` description and the
/// advertised collection metadata can never disagree.
#[derive(Debug, Clone, PartialEq)]
pub struct VerticalDimension {
    pub kind: VerticalKind,
    /// Available level values, monotonic, in the order the engine answers
    /// without `z`: bottom first on pressure and model levels (GRIB,
    /// NetCDF), lowest first on radar elevation angles (ODIM PVOL). A `z`
    /// list is put in this order ([`in_axis_order`]).
    pub levels: Vec<f64>,
}

/// `requested` levels in the order `axis` runs, each once (#1434).
///
/// CoverageJSON 1.0 §9.6.1.1 makes an axis's values monotonic, and an EDR
/// `z` list (`z=500,1000,850`, a repeat, or the levels `Rn/min/step`
/// expands to) may name its levels in any order. Sorted the way the
/// collection lists its levels, descending when `axis` ends below where it
/// starts (a bottom-first pressure axis) and ascending otherwise (radar
/// elevation angles, or an axis of fewer than two levels), a listed subset
/// reads in the same order as the answer without `z`. Nothing is matched
/// here: a value `axis` lacks stays, for the engine to drop
/// (`/req/edr/z-response` B, #969).
///
/// api-edr orders every `z` list with it before an engine sees one, and an
/// engine matching a list against its own levels orders it the same way
/// first, so the engine answers the same whoever calls it.
pub fn in_axis_order(requested: &[f64], axis: &[f64]) -> Vec<f64> {
    let mut levels = requested.to_vec();
    levels.sort_by(f64::total_cmp);
    levels.dedup();
    let descending =
        matches!((axis.first(), axis.last()), (Some(first), Some(last)) if last < first);
    if descending {
        levels.reverse();
    }
    levels
}

impl VerticalDimension {
    /// Build a descriptor for `kind` over the given `levels`.
    pub fn new(kind: VerticalKind, levels: Vec<f64>) -> Self {
        Self { kind, levels }
    }

    /// Human-readable axis label (e.g. `"Elevation angle"`).
    pub fn label(&self) -> &'static str {
        self.kind.default_label()
    }

    /// Unit symbol (e.g. `"deg"`, `"hPa"`).
    pub fn unit(&self) -> &'static str {
        self.kind.default_unit()
    }

    /// The `[min, max]` extent of the available levels, or `None` when there
    /// are no levels.
    pub fn extent(&self) -> Option<(f64, f64)> {
        let mut iter = self.levels.iter().copied();
        let first = iter.next()?;
        Some(iter.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1434: a list follows the axis's direction, bottom first on a
    /// pressure axis and lowest first on elevation angles, each level once;
    /// a level the axis lacks stays for the engine to drop.
    #[test]
    fn in_axis_order_follows_the_axis_and_drops_repeats() {
        let pressure = [1000.0, 850.0, 500.0];
        assert_eq!(
            in_axis_order(&[500.0, 1000.0, 850.0], &pressure),
            [1000.0, 850.0, 500.0]
        );
        assert_eq!(
            in_axis_order(&[850.0, 925.0, 850.0], &pressure),
            [925.0, 850.0]
        );
        let angles = [0.3, 0.7, 1.5];
        assert_eq!(
            in_axis_order(&[1.5, 0.3, 0.7, 0.3], &angles),
            [0.3, 0.7, 1.5]
        );
        // `-0.0` and `0.0` are one level, as `==` matches them.
        assert_eq!(in_axis_order(&[0.0, -0.0, 0.5], &angles).len(), 2);
        // Without a direction to follow, ascending.
        for axis in [&[][..], &[850.0]] {
            assert_eq!(in_axis_order(&[850.0, 500.0], axis), [500.0, 850.0]);
        }
    }
}
