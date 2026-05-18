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
            VerticalKind::ModelLevel => "Model level",
            VerticalKind::Isentropic => "Isentropic level",
        }
    }

    /// Direction of increasing coordinate value, as used by a CoverageJSON
    /// `VerticalCRS` coordinate-system axis.
    pub fn direction(self) -> &'static str {
        match self {
            VerticalKind::Pressure | VerticalKind::ModelLevel => "down",
            VerticalKind::Height | VerticalKind::ElevationAngle | VerticalKind::Isentropic => "up",
        }
    }
}

/// A collection's single vertical axis: the kind of coordinate plus the
/// discrete levels available for querying.
#[derive(Debug, Clone, PartialEq)]
pub struct VerticalDimension {
    pub kind: VerticalKind,
    /// Human-readable axis label (e.g. `"Elevation angle"`).
    pub label: String,
    /// Unit symbol (e.g. `"deg"`, `"hPa"`).
    pub unit: String,
    /// Available level values, in the engine's natural order.
    pub levels: Vec<f64>,
}

impl VerticalDimension {
    /// Build a descriptor for `kind`, taking the label and unit from the
    /// kind's defaults.
    pub fn new(kind: VerticalKind, levels: Vec<f64>) -> Self {
        Self {
            kind,
            label: kind.default_label().to_string(),
            unit: kind.default_unit().to_string(),
            levels,
        }
    }

    /// The `[min, max]` extent of the available levels, or `None` when there
    /// are no levels.
    pub fn extent(&self) -> Option<(f64, f64)> {
        let mut iter = self.levels.iter().copied();
        let first = iter.next()?;
        Some(iter.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v))))
    }
}
