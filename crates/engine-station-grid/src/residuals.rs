//! Leave-one-out residuals (#1206): for each station of an analysis, the
//! estimate at its own position from the other stations, minus its value.
//! A dependency-free pure function, like [`crate::interpolate`].
//!
//! Each estimate is [`estimate_without`], the per-target function every grid
//! cell is estimated with, so a residual uses exactly the grid's method,
//! neighbours and radius. A large residual is a station the others do not
//! support: a bad sensor, a misplaced station, or a real local feature the
//! network is too sparse to resolve. Their RMSE over an analysis is a
//! measure of how well the method fits the network.
//!
//! The estimate is formed from the values the grid interpolates
//! ([`Stations::value`]), and the station's own observed value is added back
//! at its own position: `estimate = observed + (ê − interpolated)`. Without
//! a correction both are the observed value, so the offset is exactly 0. A
//! lapse-rate corrected parameter interpolates values reduced to sea level
//! (`Slice::interpolated`, #1206): the offset re-applies the station's own
//! reduction `Γ·z_station`, and the residual `ê − interpolated` is the same
//! in either space.
//!
//! Cost: one neighbour search and one weighting per station, on the pool,
//! with the grid; never on a request.

use crate::interpolate::{estimate_without, Scratch, Settings, Station, Stations};

/// One station of an analysis and how the other stations estimate it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Residual {
    pub lon: f64,
    pub lat: f64,
    /// The station's observed value in the analysis slice.
    pub value: f64,
    /// The analysis at the station from the other stations; `None` with no
    /// other station within the search radius.
    pub estimate: Option<f64>,
    /// The other stations the estimate was formed from.
    pub neighbours: u32,
}

impl Residual {
    /// `estimate − value`; `None` without an estimate.
    pub fn residual(&self) -> Option<f64> {
        self.estimate.map(|e| e - self.value)
    }
}

/// The residuals of a set of stations, and their root mean square.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    /// Stations with an estimate from the others.
    pub count: usize,
    /// `sqrt(mean(residual²))` over them; `None` when there are none.
    pub rmse: Option<f64>,
}

/// The leave-one-out residual of every station, in the order of `observed`.
///
/// `stations` is the index the grid was interpolated from, built over
/// `observed` in the same order (its values may be reduced, see the module
/// docs); `observed` holds each station's own position and value.
pub fn leave_one_out(
    stations: &Stations,
    observed: &[Station],
    settings: &Settings,
) -> Vec<Residual> {
    debug_assert_eq!(stations.len(), observed.len());
    let mut scratch = Scratch::default();
    observed
        .iter()
        .enumerate()
        .map(|(i, station)| {
            let estimate = estimate_without(stations, settings, i, &mut scratch);
            // Exactly 0 while the grid interpolates the observed values.
            let offset = station.value - stations.value(i);
            Residual {
                lon: station.lon,
                lat: station.lat,
                value: station.value,
                estimate: estimate.map(|e| e + offset),
                neighbours: scratch.used() as u32,
            }
        })
        .collect()
}

/// The count and RMSE of the residuals that have an estimate.
pub fn summarize(residuals: &[Residual]) -> Summary {
    let (count, sum) = residuals
        .iter()
        .filter_map(Residual::residual)
        .fold((0usize, 0.0f64), |(n, s), r| (n + 1, s + r * r));
    Summary {
        count,
        rmse: (count > 0).then(|| (sum / count as f64).sqrt()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpolate::{estimate_at, Method};

    fn st(lon: f64, lat: f64, value: f64) -> Station {
        Station { lon, lat, value }
    }

    fn settings(method: Method) -> Settings {
        Settings {
            method,
            max_neighbours: 3,
            radius_m: 200_000.0,
        }
    }

    const METHODS: [Method; 3] = [
        Method::Idw { power: 2.0 },
        Method::Adw {
            decay_m: 150_000.0,
            exponent: 4.0,
        },
        Method::Nearest,
    ];

    fn layout() -> Vec<Station> {
        vec![
            st(24.0, 60.0, 1.0),
            st(24.5, 60.2, 2.0),
            st(25.0, 60.1, 4.0),
            st(24.3, 60.6, -1.0),
            st(25.4, 60.5, 0.5),
            // Isolated: nothing within 200 km.
            st(30.0, 66.0, 9.0),
        ]
    }

    /// A leave-one-out estimate is the estimate at the station's position
    /// over the network without it, by every method, with the same
    /// neighbour count; a station with no other in reach has none.
    #[test]
    fn leaving_one_out_is_estimating_without_it() {
        let observed = layout();
        for method in METHODS {
            let settings = settings(method);
            let residuals = leave_one_out(&Stations::new(&observed), &observed, &settings);
            for (i, r) in residuals.iter().enumerate() {
                let mut others = observed.clone();
                let me = others.remove(i);
                let mut scratch = Scratch::default();
                let want = estimate_at(
                    &Stations::new(&others),
                    &settings,
                    me.lon,
                    me.lat,
                    &mut scratch,
                );
                assert_eq!(r.estimate, want, "{method:?} station {i}");
                assert_eq!(r.neighbours as usize, scratch.used(), "{method:?} {i}");
                assert_eq!((r.lon, r.lat, r.value), (me.lon, me.lat, me.value));
            }
            assert_eq!(residuals[5].estimate, None);
            assert_eq!(residuals[5].residual(), None);
            assert_eq!(residuals[5].neighbours, 0);
            if method != Method::Nearest {
                assert_eq!(residuals[0].neighbours, 3, "capped at max_neighbours");
            }
        }
    }

    /// A co-located twin is the other station's estimate exactly: it is the
    /// coincident value once the station itself is left out.
    #[test]
    fn a_twin_estimates_its_twin() {
        let observed = vec![
            st(24.0, 60.0, 1.0),
            st(24.0, 60.0, 3.0),
            st(24.5, 60.0, 9.0),
        ];
        let residuals = leave_one_out(&Stations::new(&observed), &observed, &settings(METHODS[0]));
        assert_eq!(residuals[0].estimate, Some(3.0));
        assert_eq!(residuals[1].estimate, Some(1.0));
        assert_eq!(residuals[0].neighbours, 1);
    }

    /// Interpolated values reduced per station (a station height times a
    /// lapse rate): the estimate is put back at the station's own offset,
    /// the value stays the observed one, and the residual is the one in the
    /// reduced space.
    #[test]
    fn a_per_station_reduction_is_reapplied_at_the_station() {
        let observed = layout();
        let settings = settings(METHODS[0]);
        let offsets = [0.0, 1.3, -0.7, 2.1, 0.4, 5.0];
        let reduced: Vec<Station> = observed
            .iter()
            .zip(offsets)
            .map(|(s, o)| st(s.lon, s.lat, s.value - o))
            .collect();
        let index = Stations::new(&reduced);
        let residuals = leave_one_out(&index, &observed, &settings);
        for (i, r) in residuals.iter().enumerate() {
            let in_reduced = estimate_without(&index, &settings, i, &mut Scratch::default());
            assert_eq!(r.value, observed[i].value);
            match (r.estimate, in_reduced) {
                (Some(got), Some(e)) => {
                    assert!((got - (e + offsets[i])).abs() < 1e-12, "{i}");
                    let residual = r.residual().unwrap();
                    assert!((residual - (e - reduced[i].value)).abs() < 1e-12, "{i}");
                }
                (got, e) => assert_eq!(got, e, "{i}"),
            }
        }
    }

    #[test]
    fn summary_is_the_rms_of_the_residuals_with_an_estimate() {
        let r = |value, estimate| Residual {
            lon: 0.0,
            lat: 0.0,
            value,
            estimate,
            neighbours: 1,
        };
        let summary = summarize(&[r(1.0, Some(4.0)), r(2.0, Some(-2.0)), r(5.0, None)]);
        assert_eq!(summary.count, 2);
        assert!((summary.rmse.unwrap() - (12.5f64).sqrt()).abs() < 1e-12);
        assert_eq!(
            summarize(&[r(1.0, None)]),
            Summary {
                count: 0,
                rmse: None
            }
        );
    }
}
