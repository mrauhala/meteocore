//! Per-cell beam geometry from radar site metadata (#642).
//!
//! Pure functions over [`RadarSiteInfo`]: no I/O, no volume decoding. The
//! engine fetches the site list once per generation and calls
//! [`radar_facts`] per tracked cell.

use ds_core::cell_facts::RadarFacts;
use ds_core::geo::{beam_height_at_ground, great_circle_distance_m};
use ds_core::radar_sites::RadarSiteInfo;

/// Sortable properties added when a radar source is wired.
pub const SORTABLES_RADAR_EXTRAS: &[&str] = &["nearest_radar_distance_km", "beam_height_m"];

fn round_to(v: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (v * f).round() / f
}

/// Beam geometry at `(lon, lat)` from the nearest of `sites`; `None` when
/// there are no sites (a source that has not advertised any yet).
///
/// The nearest radar is nearest by great-circle distance, whether or not it
/// covers the cell: a cell 300 km from every radar still has a nearest one,
/// and reporting it with `in_radar_coverage: false` says more than nothing.
/// The beam fields exist only inside coverage — a beam height for a place
/// the radar does not survey is a number about nothing.
pub fn radar_facts(lon: f64, lat: f64, sites: &[RadarSiteInfo]) -> Option<RadarFacts> {
    let (site, dist_m) = sites
        .iter()
        .map(|s| (s, great_circle_distance_m(lon, lat, s.lon, s.lat)))
        .filter(|(_, d)| d.is_finite())
        .min_by(|a, b| a.1.total_cmp(&b.1))?;
    // Tri-state, not a bool: a site with no advertised range cannot say
    // whether it covers the cell, and `false` would claim it does not.
    let in_radar_coverage = site.max_range_m.map(|r| dist_m <= r);
    let (beam_height_m, beam_elevation_deg) = match (in_radar_coverage, site.lowest_elevation_deg) {
        (Some(true), Some(el)) => (
            Some(round_to(
                site.antenna_height_m + beam_height_at_ground(el, dist_m),
                0,
            )),
            Some(round_to(el, 2)),
        ),
        _ => (None, None),
    };
    Some(RadarFacts {
        nearest_radar_id: site.id.clone(),
        nearest_radar_name: site.name.clone(),
        nearest_radar_distance_km: round_to(dist_m / 1000.0, 1),
        in_radar_coverage,
        beam_height_m,
        beam_elevation_deg,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(id: &str, lon: f64, lat: f64, range_km: Option<f64>, el: Option<f64>) -> RadarSiteInfo {
        RadarSiteInfo {
            id: id.into(),
            name: Some(format!("{id}-town")),
            lon,
            lat,
            antenna_height_m: 100.0,
            max_range_m: range_km.map(|r| r * 1000.0),
            lowest_elevation_deg: el,
        }
    }

    #[test]
    fn picks_the_nearest_site_and_reports_coverage() {
        let sites = vec![
            site("far", 30.0, 65.0, Some(250.0), Some(0.3)),
            site("near", 24.5, 60.56, Some(250.0), Some(0.3)),
        ];
        // ~45 km from "near", inside coverage: beam fields present.
        let f = radar_facts(24.94, 60.17, &sites).unwrap();
        assert_eq!(f.nearest_radar_id, "near");
        assert_eq!(f.nearest_radar_name.as_deref(), Some("near-town"));
        assert!(
            (40.0..50.0).contains(&f.nearest_radar_distance_km),
            "{}",
            f.nearest_radar_distance_km
        );
        assert_eq!(f.in_radar_coverage, Some(true));
        // Antenna 100 m + ~0.35 km rise at 45 km on 0.3°: a few hundred m.
        let h = f.beam_height_m.unwrap();
        assert!((300.0..600.0).contains(&h), "beam height {h}");
        assert_eq!(f.beam_elevation_deg, Some(0.3));
    }

    #[test]
    fn outside_coverage_names_the_radar_but_not_the_beam() {
        let sites = vec![site("r", 25.0, 60.0, Some(100.0), Some(0.5))];
        // ~222 km north: beyond the 100 km range.
        let f = radar_facts(25.0, 62.0, &sites).unwrap();
        assert_eq!(f.nearest_radar_id, "r");
        assert_eq!(f.in_radar_coverage, Some(false));
        assert_eq!(f.beam_height_m, None);
        assert_eq!(f.beam_elevation_deg, None);
        assert!(f.nearest_radar_distance_km > 200.0);
    }

    #[test]
    fn unknown_range_or_elevation_is_not_coverage() {
        // No range advertised: the source cannot say, so neither can we —
        // null, not false (the null contract in the crate notes).
        let f = radar_facts(25.0, 60.1, &[site("r", 25.0, 60.0, None, Some(0.5))]).unwrap();
        assert_eq!(f.in_radar_coverage, None);
        assert_eq!(f.beam_height_m, None);
        // Range known but no sweep angles: in coverage, beam unknown.
        let f = radar_facts(25.0, 60.1, &[site("r", 25.0, 60.0, Some(250.0), None)]).unwrap();
        assert_eq!(f.in_radar_coverage, Some(true));
        assert_eq!(f.beam_height_m, None);
        assert_eq!(f.beam_elevation_deg, None);
    }

    #[test]
    fn no_sites_means_no_facts() {
        assert_eq!(radar_facts(25.0, 60.0, &[]), None);
    }

    #[test]
    fn beam_height_grows_with_range() {
        let sites = vec![site("r", 25.0, 60.0, Some(250.0), Some(0.5))];
        let near = radar_facts(25.0, 60.3, &sites)
            .unwrap()
            .beam_height_m
            .unwrap();
        let far = radar_facts(25.0, 61.5, &sites)
            .unwrap()
            .beam_height_m
            .unwrap();
        assert!(far > near * 3.0, "near {near} far {far}");
    }
}
