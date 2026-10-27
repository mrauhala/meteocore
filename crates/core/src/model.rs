use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, LazyLock, OnceLock};

use crate::vertical::VerticalKind;

#[derive(Debug, Clone)]
pub struct Location {
    pub id: String,
    pub label: String,
    pub latitude: f64,
    pub longitude: f64,
}

/// The locations an [`EdrEngine`](crate::edr_engine::EdrEngine) serves, in
/// its inventory order, with a lookup by id (#1083).
///
/// An engine builds it when it loads or refreshes its data and hands the same
/// `Arc` to every request from
/// [`EdrEngine::get_locations`](crate::edr_engine::EdrEngine::get_locations).
/// The API borrows from it: `/locations` pages are slices of it, and `items`,
/// GeoJSON naming and the unknown-id check of a location list look ids up
/// with [`Self::by_id`]. Nothing per request copies the list. It dereferences
/// to the `[Location]` slice.
///
/// The id lookup is the list's indices sorted by id, searched by bisection:
/// one `usize` per location, not a second copy of every id. When ids repeat,
/// [`Self::by_id`] finds the first in inventory order. The lookup by exact
/// coordinates ([`Self::at`]), which names a station engine's coverages, is
/// built on its first use and kept with the snapshot, so it too is built
/// once per load or refresh, never per request.
///
/// Deliberately not `Clone`: a copy of the list is what this type removes.
/// Share the `Arc`.
#[derive(Debug, Default)]
pub struct LocationSnapshot {
    locations: Vec<Location>,
    /// Indices into `locations`, ordered by id; equal ids keep inventory
    /// order.
    id_order: Vec<usize>,
    /// Index into `locations` of the one location at each point, `None` for
    /// a point several share; built by the first [`Self::at`].
    by_point: OnceLock<HashMap<(u64, u64), Option<usize>>>,
}

/// Hash key of a coordinate pair; `-0.0` and `0.0` are one point.
fn point_key(x: f64, y: f64) -> (u64, u64) {
    ((x + 0.0).to_bits(), (y + 0.0).to_bits())
}

impl LocationSnapshot {
    /// Index `locations`, keeping their order. O(n log n): build it at load
    /// or refresh time, never per request.
    pub fn new(locations: Vec<Location>) -> Self {
        let mut id_order: Vec<usize> = (0..locations.len()).collect();
        // Stable, so equal ids stay in inventory order and `by_id` finds the
        // first of them.
        id_order.sort_by(|&a, &b| locations[a].id.cmp(&locations[b].id));
        Self {
            locations,
            id_order,
            by_point: OnceLock::new(),
        }
    }

    /// The one shared empty snapshot, for an engine without locations: an
    /// `Arc` clone, no allocation.
    pub fn empty() -> Arc<Self> {
        static EMPTY: LazyLock<Arc<LocationSnapshot>> = LazyLock::new(Default::default);
        Arc::clone(&EMPTY)
    }

    /// The locations in inventory order.
    pub fn as_slice(&self) -> &[Location] {
        &self.locations
    }

    /// The location with this id, the first in inventory order when ids
    /// repeat. O(log n), no allocation.
    pub fn by_id(&self, id: &str) -> Option<&Location> {
        let at = self
            .id_order
            .partition_point(|&i| self.locations[i].id.as_str() < id);
        let location = &self.locations[*self.id_order.get(at)?];
        (location.id == id).then_some(location)
    }

    /// Whether a location has this id. O(log n), no allocation.
    pub fn contains(&self, id: &str) -> bool {
        self.by_id(id).is_some()
    }

    /// The one location at exactly longitude `x`, latitude `y`: `None` when
    /// no location, or more than one, is there. The point index is built by
    /// the first call, O(n), and kept for the snapshot's life, so later
    /// calls, from any request, are O(1).
    pub fn at(&self, x: f64, y: f64) -> Option<&Location> {
        let by_point = self.by_point.get_or_init(|| {
            let mut by_point: HashMap<(u64, u64), Option<usize>> =
                HashMap::with_capacity(self.locations.len());
            for (i, loc) in self.locations.iter().enumerate() {
                if !(loc.longitude.is_finite() && loc.latitude.is_finite()) {
                    continue;
                }
                by_point
                    .entry(point_key(loc.longitude, loc.latitude))
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some(i));
            }
            by_point
        });
        let i = (*by_point.get(&point_key(x, y))?)?;
        self.locations.get(i)
    }
}

impl crate::heap::HeapSize for LocationSnapshot {
    /// The locations and the id order; the point index too once a request
    /// built it.
    fn heap_size(&self) -> usize {
        use crate::heap::HeapSize;
        self.locations.heap_size()
            + self.id_order.heap_size()
            + self.by_point.get().map_or(0, HeapSize::heap_size)
    }
}

impl Deref for LocationSnapshot {
    type Target = [Location];

    fn deref(&self) -> &[Location] {
        &self.locations
    }
}

/// A vertical coordinate carried by a domain: the kind of level plus the
/// concrete level values selected for this coverage.
#[derive(Debug, Clone)]
pub struct VerticalCoord {
    pub kind: VerticalKind,
    pub values: Vec<f64>,
}

#[derive(Debug, Clone)]
pub enum DomainDescription {
    /// A single point at a single (optional) time — one scattered event,
    /// e.g. a lightning strike. An events query returns a
    /// `CoverageResponse::Collection` of these, one coverage per event.
    /// The CoverageJSON `Point` domain allows only single-value axes, so
    /// `z`, when present, must carry exactly one level.
    Point {
        x: f64,
        y: f64,
        t: Option<DateTime<Utc>>,
        z: Option<VerticalCoord>,
    },
    /// A time series at a single point (x, y fixed, t varies). `z`, when
    /// present, pins the series to a single vertical level.
    PointSeries {
        x: f64,
        y: f64,
        t: Vec<DateTime<Utc>>,
        z: Option<VerticalCoord>,
    },
    /// A regular grid, optionally with time and vertical dimensions.
    Grid {
        x: Vec<f64>,
        y: Vec<f64>,
        t: Option<Vec<DateTime<Utc>>>,
        z: Option<VerticalCoord>,
    },
    /// A vertical profile at a single point and time (x, y, t fixed, z varies).
    VerticalProfile {
        x: f64,
        y: f64,
        t: Option<DateTime<Utc>>,
        z: VerticalCoord,
    },
    /// A vertical cross-section along a path: the CoverageJSON `Section`
    /// domain. `nodes` carries one `(time, longitude, latitude)` tuple
    /// per along-path output column and is serialised as the mandatory
    /// composite axis with `coordinates: ["t", "x", "y"]`; `z` carries
    /// the vertical levels. The ndarray range shape is `[N_nodes, N_z]`.
    ///
    /// `coverage_floor`, when present, is one value per node: the height
    /// (in `z`'s unit, metres above the antenna for radar) of the lowest
    /// surveyed beam at that node's ground distance. Below the floor the
    /// volume is *unobserved* — not "no echo" (#514). Length must equal
    /// `nodes.len()`; values are raw (may dip below 0 near the radar or
    /// exceed the `z` axis top far out).
    Section {
        nodes: Vec<(DateTime<Utc>, f64, f64)>,
        z: VerticalCoord,
        coverage_floor: Option<Vec<f64>>,
    },
    /// Values sampled along a path through space and time: the CoverageJSON
    /// `Trajectory` domain (#926). `nodes` carries one `(time, longitude,
    /// latitude)` tuple per sample, serialised as the mandatory composite
    /// axis; the range shape is `[nodes.len()]` over axis `composite`.
    ///
    /// At most one of the two vertical fields is set. `node_z` gives every
    /// node its own level (`values.len() == nodes.len()`), which puts `z`
    /// into the composite tuples (`["t", "x", "y", "z"]`); `z` is one level
    /// the whole path shares, a single-valued `z` axis beside the
    /// composite one. Composite tuples must be unique (CoverageJSON
    /// `uniqueItems`).
    Trajectory {
        nodes: Vec<(DateTime<Utc>, f64, f64)>,
        node_z: Option<VerticalCoord>,
        z: Option<VerticalCoord>,
    },
}

#[derive(Debug, Clone)]
pub struct ParameterDescription {
    pub label: String,
    pub unit: String,
    pub observed_property: String,
    /// CF standard name of the observed property (`air_temperature`), only
    /// when the source asserts one — a CF `standard_name` attribute. `None`
    /// when unknown; never guessed from the parameter name. The EDR layer
    /// publishes it as the NERC vocabulary URI in `observedProperty.id`.
    pub standard_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NdArray {
    pub shape: Vec<usize>,
    pub axis_names: Vec<String>,
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub domain: DomainDescription,
    pub parameters: HashMap<String, ParameterDescription>,
    pub ranges: HashMap<String, NdArray>,
}

/// Result of an EDR position / area / location query — either a single
/// coverage (e.g. a Grid) or a collection of coverages (e.g. one PointSeries
/// per station, or one VerticalProfile per timestep).
#[derive(Debug, Clone)]
pub enum CoverageResponse {
    /// A single coverage.
    Single(QueryResult),
    /// Multiple coverages, serialised as a CoverageJSON `CoverageCollection`.
    Collection(Vec<QueryResult>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(id: &str, label: &str) -> Location {
        Location {
            id: id.into(),
            label: label.into(),
            latitude: 60.0,
            longitude: 25.0,
        }
    }

    #[test]
    fn keeps_inventory_order_and_finds_every_id() {
        let ids = ["s2", "a", "Ä", "s10", "", "s1", "b c"];
        let snapshot = LocationSnapshot::new(ids.iter().map(|id| loc(id, id)).collect());
        let listed: Vec<&str> = snapshot.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(listed, ids, "the slice is the inventory order");
        assert_eq!(snapshot.as_slice().len(), ids.len());
        for id in ids {
            assert_eq!(snapshot.by_id(id).map(|l| l.id.as_str()), Some(id));
            assert!(snapshot.contains(id));
        }
        for missing in ["s", "s3", "A", "a ", "zzz", "b"] {
            assert!(snapshot.by_id(missing).is_none(), "{missing:?}");
            assert!(!snapshot.contains(missing));
        }
    }

    /// A coverage is named by the one location at its exact point; a shared
    /// point names none, `-0.0` is `0.0`, and the index is built once per
    /// snapshot.
    #[test]
    fn at_finds_the_one_location_at_a_point() {
        let at = |id: &str, x: f64, y: f64| Location {
            id: id.into(),
            label: id.into(),
            latitude: y,
            longitude: x,
        };
        let snapshot = LocationSnapshot::new(vec![
            at("a", 25.0, 60.0),
            at("b", 0.0, 0.0),
            at("c", 24.5, 61.0),
            at("d", 24.5, 61.0),
            at("nan", f64::NAN, 61.0),
        ]);
        assert_eq!(snapshot.at(25.0, 60.0).map(|l| l.id.as_str()), Some("a"));
        assert_eq!(snapshot.at(-0.0, 0.0).map(|l| l.id.as_str()), Some("b"));
        assert!(snapshot.at(24.5, 61.0).is_none(), "two share the point");
        assert!(snapshot.at(60.0, 25.0).is_none(), "axes are not swapped");
        assert!(snapshot.at(f64::NAN, 61.0).is_none());
        let index = snapshot.by_point.get().expect("built by the first call");
        assert!(std::ptr::eq(
            index,
            snapshot.by_point.get().expect("kept with the snapshot")
        ));
    }

    #[test]
    fn a_repeated_id_finds_its_first_location() {
        let snapshot = LocationSnapshot::new(vec![
            loc("b", "first b"),
            loc("a", "a"),
            loc("b", "second b"),
            loc("b", "third b"),
        ]);
        assert_eq!(snapshot.by_id("b").unwrap().label, "first b");
        assert_eq!(snapshot.len(), 4, "the list keeps every entry");
    }

    #[test]
    fn empty_is_one_shared_snapshot() {
        let (a, b) = (LocationSnapshot::empty(), LocationSnapshot::empty());
        assert!(a.is_empty());
        assert!(a.by_id("").is_none());
        assert!(Arc::ptr_eq(&a, &b), "no allocation per call");
    }
}
