//! Radar site metadata as a data source for other engines (#642).
//!
//! A nowcast collection joins tracked storm cells to the nearest radar so a
//! consumer can see how far from any radar a cell is and how high the lowest
//! beam passes over it. Those two numbers are the cheapest evidence there is
//! about whether an echo is meteorological: a bright, stationary echo under a
//! beam that is 200 m off the ground is a wind farm or a mast; the same echo
//! under a beam 3 km up is weather.
//!
//! The trait is deliberately data-only: no volume decoding, no I/O. An
//! implementation reads its site catalog snapshot and returns it, so calling
//! this once per generation on the poll runtime costs nothing measurable.

/// One radar site as advertised by a polar-volume collection.
#[derive(Debug, Clone, PartialEq)]
pub struct RadarSiteInfo {
    /// Stable site id (ODIM `NOD` code, e.g. `"fivih"`).
    pub id: String,
    /// Human place name, when the source provides one.
    pub name: Option<String>,
    /// Antenna longitude, degrees east (WGS84).
    pub lon: f64,
    /// Antenna latitude, degrees north (WGS84).
    pub lat: f64,
    /// Antenna height above mean sea level, metres.
    pub antenna_height_m: f64,
    /// Maximum ground range the site surveys on ANY sweep, metres — the
    /// coverage question. `None` when the source cannot say (malformed
    /// sweep geometry).
    pub max_range_m: Option<f64>,
    /// Ground range of the LOWEST sweep itself, metres — how far a
    /// lowest-beam height may honestly be extrapolated. A longer-range
    /// higher tilt (a Doppler tilt at higher PRF, say) does not extend the
    /// lowest beam. `None` when unknown.
    pub lowest_sweep_range_m: Option<f64>,
    /// Lowest sweep elevation angle, degrees above horizontal. `None` when
    /// the source has not advertised its sweep angles.
    pub lowest_elevation_deg: Option<f64>,
}

/// A collection that can list its radar sites from a snapshot.
///
/// Contract: O(1)-ish from an in-memory snapshot, never blocking I/O — the
/// nowcast engine calls it once per generation and it must not turn a
/// generation into a catalog scan.
pub trait RadarSiteSource: Send + Sync {
    fn radar_sites(&self) -> Vec<RadarSiteInfo>;
}
