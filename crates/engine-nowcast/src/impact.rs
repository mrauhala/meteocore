//! Impact context for tracked cells: what a cell is over, what it is heading
//! toward, and how long until it gets there.
//!
//! This is the term that makes a storm-cell ranking operational. Radar
//! attributes answer "how intense is this cell"; only impact answers "does
//! anyone care", and those are different questions — a very severe cell
//! drifting over the Gulf of Bothnia matters less than a moderate one closing
//! on a town. A ranker without this is wrong in exactly the way that costs
//! someone something.
//!
//! Deliberately generic over the polygon source: any `FeatureEngine` whose
//! features carry a name works, and an optional numeric property (population,
//! households, insured value) weights the result. With no weight property the
//! scoring degrades to purely geometric, which is honest rather than broken —
//! it simply cannot rank Helsinki above a rural municipality.

use std::sync::Arc;

use ds_core::cell_facts::ImpactFacts;
use ds_core::feature::{Bbox, FeatureQuery, Geometry};
use ds_core::feature_engine::FeatureEngine;
use ds_core::geo::destination_point;

use crate::cells2d::MAX_CELL_SPEED_MS;

/// How far ahead along the motion vector to look for the next affected area.
///
/// One hour, not the full nowcast horizon: beyond that, advection skill on
/// convective cells has decayed enough (the crate's own gate result) that an
/// ETA would be a number with no information in it.
const LOOKAHEAD_MIN: f64 = 60.0;

/// Sampling step along the track. 2 minutes at 15 m/s ≈ 1.8 km — finer than
/// any municipality boundary this is likely to be run against, and it bounds
/// the sweep at 30 probes per cell.
const STEP_MIN: f64 = 2.0;

/// Weight given to a cell that will arrive somewhere versus one already
/// there. Being under a storm is worse than expecting one, but not by so much
/// that approach warnings — the actionable case — get buried.
const APPROACHING_FACTOR: f64 = 0.7;

/// Bounds of the log-scaled weight ramp. Linear population weighting would
/// collapse every municipality except the capital to ~0; a log ramp between a
/// hamlet and a capital keeps rural Finland meaningfully ranked while still
/// putting a city well above it.
const WEIGHT_FLOOR: f64 = 100.0;
const WEIGHT_REFERENCE: f64 = 700_000.0;

/// Cap on polygons pulled from the impact source per generation. A source
/// with more than this is almost certainly the wrong granularity for
/// "which named area is this storm over" (postal codes, parcels), and the
/// per-probe bbox scan below is linear in this count.
const MAX_IMPACT_AREAS: usize = 5_000;

/// One named area a cell can affect.
struct Area {
    /// Source feature id. Arrival is decided on THIS, not on `name`: the
    /// module is generic over the polygon source, and names are not unique in
    /// every plausible one (service areas or postal regions repeating a name
    /// under different parents). Comparing names would silently suppress a
    /// real transition between two same-named polygons.
    id: String,
    name: String,
    /// `Arc`-shared with the source feature — these rings are re-fetched every
    /// generation, and deep-copying up to `MAX_IMPACT_AREAS` of them would be
    /// pure waste (`contains` only needs `&self`).
    geometry: Arc<Geometry>,
    bbox: [f64; 4],
    /// Pre-normalized 0..=1 from the configured weight property; 1.0 when no
    /// property is configured (purely geometric scoring).
    weight: f64,
}

/// Named areas for one generation, fetched once and queried many times.
///
/// Built per generation rather than cached for the engine's lifetime so that
/// a reloaded or repolled impact collection is picked up without a restart —
/// the source is in-memory (engine-geojson holds an R-tree), so the fetch is
/// cheap relative to a generation.
pub struct ImpactIndex {
    areas: Vec<Area>,
}

impl ImpactIndex {
    /// Fetch the areas overlapping `bbox` from `source`.
    ///
    /// ONE bounded call per generation, never one per cell — the same
    /// contract `EventSource` documents, and for the same reason: an impact
    /// source may be a sync bridge over a database.
    ///
    /// Callers are on the background poll runtime, which is where a sync
    /// bridge is legal (root CLAUDE.md rules 6/7).
    pub fn build(
        source: &dyn FeatureEngine,
        bbox: [f64; 4],
        name_property: &str,
        weight_property: Option<&str>,
    ) -> Result<Self, ds_core::error::DataServerError> {
        let [west, south, east, north] = pad_for_lookahead(bbox);
        // A malformed bbox must FAIL this generation's join, not silently
        // become `None` — an unfiltered query would pull the source's entire
        // catalog every generation, quietly, forever.
        let bbox = Bbox::new(west, south, east, north).map_err(|e| {
            ds_core::error::DataServerError::Engine(format!("impact query bbox invalid: {e}"))
        })?;
        let query = FeatureQuery {
            bbox: Some(bbox),
            limit: MAX_IMPACT_AREAS,
            ..Default::default()
        };
        let page = source.get_features(&query)?;
        if page.number_matched > MAX_IMPACT_AREAS {
            tracing::warn!(
                "impact source returned {} areas; only the first {MAX_IMPACT_AREAS} are used \
                 (is this the right granularity for named-area impact?)",
                page.number_matched
            );
        }

        let areas: Vec<Area> = page
            .features
            .iter()
            .filter_map(|f| {
                let name = f.properties.get(name_property)?.as_str()?.to_string();
                let bbox = f.geometry.bbox()?;
                let weight = weight_property
                    .and_then(|p| f.properties.get(p))
                    .and_then(|v| v.as_f64())
                    .map(normalize_weight)
                    // No weight property configured, or this feature lacks
                    // it: fall back to geometric. Treating a missing value as
                    // zero would silently erase an area from the ranking.
                    .unwrap_or(1.0);
                Some(Area {
                    id: f.id.clone(),
                    name,
                    geometry: Arc::clone(&f.geometry),
                    bbox,
                    weight,
                })
            })
            .collect();

        Ok(Self { areas })
    }

    pub fn is_empty(&self) -> bool {
        self.areas.is_empty()
    }

    /// Which area contains this point, if any. Bbox prefilter first — the
    /// ray-cast walks every ring, and most areas miss on the bbox.
    fn area_at(&self, lon: f64, lat: f64) -> Option<&Area> {
        self.areas.iter().find(|a| {
            lon >= a.bbox[0]
                && lon <= a.bbox[2]
                && lat >= a.bbox[1]
                && lat <= a.bbox[3]
                && a.geometry.contains(lon, lat)
        })
    }

    /// Resolve impact for one cell.
    ///
    /// `speed_ms` / `bearing_deg` are the cell's own motion; without them
    /// (a newborn track) only the "over" case can be answered — an ETA
    /// invented from no velocity would be worse than no ETA.
    pub fn resolve(
        &self,
        lon: f64,
        lat: f64,
        speed_ms: Option<f64>,
        bearing_deg: Option<f64>,
    ) -> ImpactFacts {
        let over = self.area_at(lon, lat);

        let mut approaching = None;
        let mut eta_minutes = None;
        if let (Some(speed), Some(bearing)) = (speed_ms, bearing_deg) {
            if speed > 0.0 {
                let mut minutes = STEP_MIN;
                while minutes <= LOOKAHEAD_MIN {
                    let (plon, plat) = destination_point(lon, lat, speed * minutes * 60.0, bearing);
                    if let Some(area) = self.area_at(plon, plat) {
                        // The first area that isn't the one it's already
                        // over: staying put is not an arrival. Compared by
                        // id, not name — see `Area::id`.
                        if over.is_none_or(|o| o.id != area.id) {
                            approaching = Some(area);
                            eta_minutes = Some(minutes);
                            break;
                        }
                    }
                    minutes += STEP_MIN;
                }
            }
        }

        // Being under it outranks being about to be under it; an arrival
        // decays linearly to zero at the lookahead limit.
        let exposure = match (over, approaching) {
            (Some(area), _) => area.weight,
            (None, Some(area)) => {
                let eta = eta_minutes.unwrap_or(LOOKAHEAD_MIN);
                let closeness = (1.0 - eta / LOOKAHEAD_MIN).clamp(0.0, 1.0);
                area.weight * closeness * APPROACHING_FACTOR
            }
            (None, None) => 0.0,
        };

        ImpactFacts {
            over: over.map(|a| a.name.clone()),
            approaching: approaching.map(|a| a.name.clone()),
            eta_minutes,
            exposure: exposure.clamp(0.0, 1.0),
        }
    }
}

/// Grow a bbox by the furthest a cell could travel within the lookahead.
///
/// The working grid is the radar composite's extent, but a cell near its edge
/// moving outward can reach an area that lies entirely OUTSIDE that extent —
/// and outside it is exactly where a coastal or border radar's cells go. A
/// fetch bounded by the grid alone would report `approaching: null` for the
/// real, imminent arrival, which is worse than reporting nothing at all
/// because it looks like an answer.
///
/// Longitude padding widens with latitude (meridians converge), computed at
/// whichever edge is nearer the pole, and is capped so a high-latitude domain
/// cannot ask for a whole hemisphere.
///
/// Longitude WRAPS rather than clamping: `Bbox` supports antimeridian-crossing
/// boxes (`west > east`, OGC API Features §7.15.3), and clamping at ±180°
/// would truncate a dateline-adjacent domain's reach — reintroducing on the
/// antimeridian exactly the silent miss this function exists to prevent at the
/// composite edge. Latitude still clamps: the poles do not wrap.
fn pad_for_lookahead(bbox: [f64; 4]) -> [f64; 4] {
    const MAX_LON_PAD_DEG: f64 = 20.0;
    const KM_PER_DEG_LAT: f64 = 111.32;
    /// Wrap a longitude into (−180, 180].
    fn wrap_lon(v: f64) -> f64 {
        (v + 180.0).rem_euclid(360.0) - 180.0
    }

    let [west, south, east, north] = bbox;
    if !bbox.iter().all(|v| v.is_finite()) {
        return bbox;
    }
    let reach_km = f64::from(MAX_CELL_SPEED_MS) * LOOKAHEAD_MIN * 60.0 / 1000.0;
    let lat_pad = reach_km / KM_PER_DEG_LAT;

    let worst_lat = south.abs().max(north.abs()).min(89.0);
    let lon_pad =
        (reach_km / (KM_PER_DEG_LAT * worst_lat.to_radians().cos()).max(1e-6)).min(MAX_LON_PAD_DEG);

    // Unroll an already-crossing input onto a monotonic span before padding,
    // so the width is the true width in both cases.
    let unrolled_east = if east < west { east + 360.0 } else { east };
    let padded_west = west - lon_pad;
    let padded_east = unrolled_east + lon_pad;
    let (out_west, out_east) = if padded_east - padded_west >= 360.0 {
        // The pad swallowed the globe; a crossing box would be ambiguous, so
        // ask for everything explicitly.
        (-180.0, 180.0)
    } else {
        (wrap_lon(padded_west), wrap_lon(padded_east))
    };

    [
        out_west,
        (south - lat_pad).max(-90.0),
        out_east,
        (north + lat_pad).min(90.0),
    ]
}

/// Log-scale a raw weight (population, households, …) onto 0..=1.
///
/// Linear would make everything below a capital city indistinguishable from
/// zero; a log ramp from a hamlet to a capital preserves the ordering while
/// keeping small communities on the scale.
fn normalize_weight(raw: f64) -> f64 {
    if !raw.is_finite() || raw <= 0.0 {
        return 0.0;
    }
    let lo = (1.0 + WEIGHT_FLOOR).log10();
    let hi = (1.0 + WEIGHT_REFERENCE).log10();
    (((1.0 + raw).log10() - lo) / (hi - lo)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::feature::{Feature, PropertyValue};
    use std::collections::HashMap;

    /// A unit square from (x, y) to (x+1, y+1).
    fn square(name: &str, x: f64, y: f64, population: Option<f64>) -> Feature {
        let mut props = HashMap::new();
        props.insert("name".to_string(), PropertyValue::String(name.into()));
        if let Some(p) = population {
            props.insert("population".to_string(), PropertyValue::Float(p));
        }
        Feature {
            id: name.into(),
            geometry: Arc::new(Geometry::Polygon {
                exterior: vec![
                    [x, y],
                    [x + 1.0, y],
                    [x + 1.0, y + 1.0],
                    [x, y + 1.0],
                    [x, y],
                ],
                holes: vec![],
            }),
            properties: Arc::new(props),
        }
    }

    struct MockAreas(Vec<Feature>);

    impl FeatureEngine for MockAreas {
        fn get_features(
            &self,
            _query: &FeatureQuery,
        ) -> Result<ds_core::feature::FeaturePage, ds_core::error::DataServerError> {
            Ok(ds_core::feature::FeaturePage {
                number_matched: self.0.len(),
                number_returned: self.0.len(),
                features: self.0.clone(),
                next_offset: None,
            })
        }

        fn get_feature(
            &self,
            _id: &str,
        ) -> Result<ds_core::feature::Feature, ds_core::error::DataServerError> {
            unreachable!("the impact index only ever calls get_features")
        }
    }

    fn index(features: Vec<Feature>, weight: Option<&str>) -> ImpactIndex {
        ImpactIndex::build(&MockAreas(features), [0.0, 0.0, 10.0, 10.0], "name", weight)
            .expect("index builds")
    }

    #[test]
    fn a_cell_over_an_area_reports_it() {
        let idx = index(vec![square("Town", 0.0, 0.0, None)], None);
        let facts = idx.resolve(0.5, 0.5, None, None);
        assert_eq!(facts.over.as_deref(), Some("Town"));
        assert_eq!(facts.approaching, None);
        assert_eq!(facts.eta_minutes, None);
        assert_eq!(facts.exposure, 1.0);
    }

    #[test]
    fn a_cell_over_nothing_and_going_nowhere_has_zero_exposure() {
        let idx = index(vec![square("Town", 5.0, 5.0, None)], None);
        let facts = idx.resolve(0.5, 0.5, None, None);
        assert_eq!(facts.over, None);
        assert_eq!(facts.exposure, 0.0);
    }

    #[test]
    fn an_approaching_cell_gets_an_eta_and_decayed_exposure() {
        // Town spans lat 1.0..2.0; the cell sits just south of it at 0.95 and
        // moves due north at 20 m/s (~1.2 km/min), so it crosses in ~5 min.
        let idx = index(vec![square("Town", 0.0, 1.0, None)], None);
        let facts = idx.resolve(0.5, 0.95, Some(20.0), Some(0.0));
        assert_eq!(facts.over, None);
        assert_eq!(facts.approaching.as_deref(), Some("Town"));
        let eta = facts.eta_minutes.expect("northbound cell must get an ETA");
        assert!((2.0..=20.0).contains(&eta), "implausible eta {eta}");
        assert!(
            facts.exposure > 0.0 && facts.exposure < APPROACHING_FACTOR,
            "approach exposure should decay below the arrival factor: {}",
            facts.exposure
        );
    }

    #[test]
    fn a_cell_moving_away_reports_no_approach() {
        let idx = index(vec![square("Town", 0.0, 1.0, None)], None);
        // Same geometry, heading due SOUTH.
        let facts = idx.resolve(0.5, 0.95, Some(20.0), Some(180.0));
        assert_eq!(facts.approaching, None);
        assert_eq!(facts.eta_minutes, None);
        assert_eq!(facts.exposure, 0.0);
    }

    #[test]
    fn staying_inside_one_area_is_not_an_arrival() {
        // A large area the cell is already inside and stays inside.
        let idx = index(vec![square("Big", 0.0, 0.0, None)], None);
        let facts = idx.resolve(0.5, 0.1, Some(1.0), Some(0.0));
        assert_eq!(facts.over.as_deref(), Some("Big"));
        assert_eq!(
            facts.approaching, None,
            "still being over the same area is not an approach"
        );
    }

    #[test]
    fn population_weighting_ranks_a_city_above_a_village() {
        let areas = vec![
            square("City", 0.0, 0.0, Some(694_392.0)),
            square("Village", 2.0, 0.0, Some(1_200.0)),
        ];
        let idx = index(areas, Some("population"));
        let city = idx.resolve(0.5, 0.5, None, None).exposure;
        let village = idx.resolve(2.5, 0.5, None, None).exposure;
        assert!(
            city > village,
            "a city must outrank a village: {city} vs {village}"
        );
        assert!(
            village > 0.1,
            "log weighting must keep small communities on the scale, got {village}"
        );
    }

    #[test]
    fn missing_weight_property_falls_back_to_geometric() {
        // Configured to weight by population, but this feature has none.
        // Treating that as zero would silently erase the area.
        let idx = index(vec![square("Unknown", 0.0, 0.0, None)], Some("population"));
        assert_eq!(idx.resolve(0.5, 0.5, None, None).exposure, 1.0);
    }

    #[test]
    fn weight_normalization_is_monotone_and_bounded() {
        let samples = [0.0, 1.0, 100.0, 1_200.0, 8_982.0, 263_337.0, 694_392.0, 1e9];
        let mut prev = -1.0;
        for s in samples {
            let w = normalize_weight(s);
            assert!((0.0..=1.0).contains(&w), "weight {w} out of range for {s}");
            assert!(w >= prev, "weighting must be monotone: {s} gave {w}");
            prev = w;
        }
        assert_eq!(normalize_weight(f64::NAN), 0.0);
        assert_eq!(normalize_weight(-5.0), 0.0);
    }

    #[test]
    fn an_unnamed_or_null_geometry_area_is_skipped_not_fatal() {
        let mut nameless = square("x", 0.0, 0.0, None);
        nameless.properties = Arc::new(HashMap::new());
        let mut null_geom = square("Ghost", 0.0, 0.0, None);
        null_geom.geometry = Arc::new(Geometry::Null);
        let idx = index(vec![nameless, null_geom], None);
        assert!(idx.is_empty());
        assert_eq!(idx.resolve(0.5, 0.5, None, None).exposure, 0.0);
    }

    #[test]
    fn same_named_but_distinct_areas_still_count_as_an_arrival() {
        // Generic sources (service areas, postal regions) do not guarantee
        // unique names. Comparing names would read this as "still in the same
        // place" and suppress a real transition.
        let mut a = square("Keskusta", 0.0, 0.0, None);
        a.id = "a".into();
        let mut b = square("Keskusta", 0.0, 1.0, None);
        b.id = "b".into();
        let idx = index(vec![a, b], None);
        let facts = idx.resolve(0.5, 0.95, Some(20.0), Some(0.0));
        assert_eq!(facts.over.as_deref(), Some("Keskusta"));
        assert_eq!(
            facts.approaching.as_deref(),
            Some("Keskusta"),
            "crossing into a DIFFERENT polygon is an arrival even when the names match"
        );
        assert!(facts.eta_minutes.is_some());
    }

    #[test]
    fn a_malformed_bbox_fails_the_join_instead_of_querying_everything() {
        // Silently dropping the filter would pull the source's whole catalog
        // every generation, with no log line to notice it by.
        let err = ImpactIndex::build(
            &MockAreas(vec![square("Town", 0.0, 0.0, None)]),
            [f64::NAN, 0.0, 10.0, 10.0],
            "name",
            None,
        );
        assert!(err.is_err(), "a non-finite bbox must fail the join");

        // south > north is equally malformed and equally must not degrade.
        assert!(ImpactIndex::build(
            &MockAreas(vec![square("Town", 0.0, 0.0, None)]),
            [0.0, 60.0, 10.0, 50.0],
            "name",
            None,
        )
        .is_err());
    }

    #[test]
    fn the_fetch_bbox_covers_where_cells_can_travel_not_just_the_grid() {
        // A cell at the domain edge moving outward reaches areas OUTSIDE the
        // radar composite — coastal and border radars do this constantly.
        let grid = [20.0, 60.0, 25.0, 65.0];
        let [w, s, e, n] = pad_for_lookahead(grid);
        assert!(w < grid[0] && s < grid[1] && e > grid[2] && n > grid[3]);

        // The pad must cover the furthest a cell could actually get.
        let reach_deg = f64::from(MAX_CELL_SPEED_MS) * LOOKAHEAD_MIN * 60.0 / 1000.0 / 111.32;
        assert!(
            grid[1] - s >= reach_deg - 1e-9,
            "latitude pad {} must cover the {reach_deg} deg reach",
            grid[1] - s
        );

        // Latitude clamps at the poles; longitude here spans nearly the
        // globe already, so the pad asks for everything rather than an
        // ambiguous crossing box.
        let [w, s, e, n] = pad_for_lookahead([-179.0, 88.0, 179.0, 89.5]);
        assert_eq!((w, e), (-180.0, 180.0));
        assert!(s >= -90.0 && n <= 90.0);

        // Garbage in, garbage out — but unchanged, so Bbox::new rejects it.
        assert!(pad_for_lookahead([f64::NAN, 0.0, 1.0, 1.0])[0].is_nan());
    }

    #[test]
    fn padding_wraps_across_the_antimeridian_instead_of_truncating() {
        // A dateline-adjacent domain: clamping at +180 would silently drop
        // the reach just past it — the same failure this padding exists to
        // prevent at the composite edge.
        let [w, _, e, _] = pad_for_lookahead([170.0, 60.0, 179.0, 65.0]);
        assert!(w > 160.0 && w < 170.0, "west should pad westward, got {w}");
        assert!(
            e < 0.0,
            "east should wrap past the antimeridian to a negative lon, got {e}"
        );
        assert!(
            w > e,
            "the result must be a crossing bbox (west > east), got {w}..{e}"
        );
        // Bbox accepts it (OGC API Features 7.15.3), so the query is valid.
        assert!(Bbox::new(w, 60.0, e, 65.0).is_ok());
    }

    #[test]
    fn padding_an_already_crossing_bbox_stays_crossing() {
        // Input already spans the dateline: 175E .. 175W.
        let [w, _, e, _] = pad_for_lookahead([175.0, 60.0, -175.0, 65.0]);
        assert!(w < 175.0 || w > 0.0, "west padded westward, got {w}");
        assert!(w > e, "must remain a crossing bbox, got {w}..{e}");
        assert!(Bbox::new(w, 60.0, e, 65.0).is_ok());
    }
}
