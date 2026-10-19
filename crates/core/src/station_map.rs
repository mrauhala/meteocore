//! Station-symbol map layers (#1063, #1204): the domain half every station
//! engine shares. CSV, BUFR and PostGIS station collections serve one WMS/
//! Maps/Tiles layer per parameter, each station a marker coloured by its
//! latest value within `max_age`.
//!
//! - [`StationMapSettings`]: `[postgis.map]`, `[bufr.map]` or `[csv.map]`
//!   ([`crate::config::StationMapConfig`]) resolved over the defaults.
//! - The time axis: frames every `time_step` ([`advertised_times`] over the
//!   last [`STATION_MAP_TIME_WINDOW`] of data), a requested `TIME` floored to
//!   its step ([`quantize_time`]), and the one selection
//!   [`resolve_frame_end`] that `get_raster_tile`, `resolve_time` and
//!   `resolve_parameter_time` all use (#507).
//! - A frame at `end` reads [`frame_window`] `[end - max_age, end]` and
//!   keeps each station's latest non-null, finite value
//!   ([`LatestValues`] for a stream of rows), scanning at most
//!   [`MAX_FRAME_ROWS`] observation rows.
//!
//! The drawing half (overlap order, discs, the nodata marker, the shared
//! frame cache) is `ds_render::station_symbols`.

use chrono::{DateTime, Duration, Utc};

use crate::config::{
    StationMapConfig, STATION_MAP_MAX_AGE_CEILING, STATION_MAP_MAX_MARKER_SIZE,
    STATION_MAP_MIN_TIME_STEP, STATION_MAP_TIME_WINDOW,
};
use crate::datetime::parse_iso8601_duration;

/// Observation rows one frame may read: stations × the rows each has in the
/// window. 2 000 000 is 20 000 stations at 10-minute steps over 16 hours.
/// Past it the frame is a `QueryTooLarge` naming `max_age`. An in-memory
/// engine stops at a station's latest value, so it usually reads one row a
/// station; the cap bounds a window of rows without the parameter.
pub const MAX_FRAME_ROWS: usize = 2_000_000;

/// The station layers' settings (#1063): the frame window, the marker size,
/// what to do with stations without a value, and the TIME step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StationMapSettings {
    /// A frame at `TIME` shows each station's latest non-null value in
    /// `[TIME - max_age, TIME]`.
    pub max_age: Duration,
    /// Marker diameter in output pixels.
    pub marker_size: u32,
    /// Draw stations without a value in the window as nodata markers.
    pub draw_missing: bool,
    /// The layers' TIME step: the advertised steps over the last 6 h, and
    /// what a requested TIME floors to. Whole seconds, at least a minute.
    pub time_step: Duration,
}

impl Default for StationMapSettings {
    fn default() -> Self {
        Self {
            max_age: Duration::hours(1),
            marker_size: 6,
            draw_missing: false,
            time_step: Duration::minutes(10),
        }
    }
}

impl StationMapSettings {
    /// Lower a `[<engine>.map]` section; absent fields take the defaults, and
    /// no section is all defaults. Defense in depth:
    /// [`crate::config::validate_station_map`] already bounds every field at
    /// load. `Err` names the offending field and value, e.g.
    /// `max_age 'P8D'`.
    pub fn resolve(raw: Option<&StationMapConfig>) -> Result<Self, String> {
        let defaults = Self::default();
        let Some(raw) = raw else {
            return Ok(defaults);
        };
        let max_age = match raw.max_age.as_deref() {
            None => defaults.max_age,
            Some(s) => parse_iso8601_duration(s)
                .ok()
                .filter(|d| *d > Duration::zero() && *d <= STATION_MAP_MAX_AGE_CEILING)
                .ok_or_else(|| format!("max_age '{s}'"))?,
        };
        let marker_size = raw.marker_size.unwrap_or(defaults.marker_size);
        if marker_size == 0 || marker_size > STATION_MAP_MAX_MARKER_SIZE {
            return Err(format!("marker_size {marker_size}"));
        }
        let time_step = match raw.time_step.as_deref() {
            None => defaults.time_step,
            Some(s) => parse_iso8601_duration(s)
                .ok()
                .filter(|d| {
                    d.subsec_nanos() == 0
                        && *d >= STATION_MAP_MIN_TIME_STEP
                        && *d <= STATION_MAP_TIME_WINDOW
                })
                .ok_or_else(|| format!("time_step '{s}'"))?,
        };
        Ok(Self {
            max_age,
            marker_size,
            draw_missing: raw.draw_missing.unwrap_or(defaults.draw_missing),
            time_step,
        })
    }
}

/// The observation window a frame at `end` reads: `[end - max_age, end]`,
/// both ends included.
pub fn frame_window(end: DateTime<Utc>, max_age: Duration) -> (DateTime<Utc>, DateTime<Utc>) {
    (end - max_age, end)
}

/// Floor a requested instant to a multiple of `step` since the epoch.
pub fn quantize_time(t: DateTime<Utc>, step: Duration) -> DateTime<Utc> {
    let step = step.num_seconds().max(1);
    let secs = t.timestamp();
    DateTime::from_timestamp(secs - secs.rem_euclid(step), 0).unwrap_or(t)
}

/// The advertised TIME steps: on `step`, ascending, newest last, from
/// [`STATION_MAP_TIME_WINDOW`] below the newest observation (or the first
/// observation, if later) to the newest. Empty without data. Built once per
/// metadata snapshot, never per request.
pub fn advertised_times(
    temporal_extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
    step: Duration,
) -> Vec<DateTime<Utc>> {
    let Some((first, last)) = temporal_extent else {
        return Vec::new();
    };
    let newest = quantize_time(last, step);
    let floor = quantize_time((last - STATION_MAP_TIME_WINDOW).max(first), step);
    let step = Duration::seconds(step.num_seconds().max(1));
    let mut steps = Vec::new();
    let mut t = floor;
    while t <= newest {
        steps.push(t);
        t += step;
    }
    steps
}

/// The frame instant a request draws, the #507 cache-key authority: an
/// explicit TIME floors to `step`; no TIME is the latest advertised step.
/// `None` only without TIME and without data, where the render must fail
/// (an error caches nothing).
pub fn resolve_frame_end(
    advertised: &[DateTime<Utc>],
    time: Option<DateTime<Utc>>,
    step: Duration,
) -> Option<DateTime<Utc>> {
    match time {
        Some(t) => Some(quantize_time(t, step)),
        None => advertised.last().copied(),
    }
}

/// Each station's latest non-null, finite value in a frame's window, by its
/// place in the station list: the reduction a stream of observation rows
/// goes through when they do not arrive newest first.
#[derive(Debug, Clone, Default)]
pub struct LatestValues {
    latest: Vec<Option<(DateTime<Utc>, f64)>>,
}

impl LatestValues {
    pub fn new(stations: usize) -> Self {
        Self {
            latest: vec![None; stations],
        }
    }

    /// One row of station `at`. A null or non-finite value is no value; of
    /// two values the later time wins, and of two at one time the later row,
    /// as an EDR series keeps it.
    pub fn push(&mut self, at: usize, time: DateTime<Utc>, value: Option<f64>) {
        let Some(value) = value.filter(|v| v.is_finite()) else {
            return;
        };
        let slot = &mut self.latest[at];
        if slot.is_none_or(|(t, _)| time >= t) {
            *slot = Some((time, value));
        }
    }

    pub fn value(&self, at: usize) -> Option<f64> {
        self.latest[at].map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// The latest value in the window wins whatever order rows arrive in,
    /// and a later null does not erase an earlier value. The window is
    /// `[end - max_age, end]`.
    #[test]
    fn the_latest_value_in_the_window_wins() {
        let mut latest = LatestValues::new(1);
        latest.push(0, ts("2026-10-18T05:50:00Z"), Some(3.0));
        latest.push(0, ts("2026-10-18T05:30:00Z"), Some(1.0));
        latest.push(0, ts("2026-10-18T06:00:00Z"), None);
        latest.push(0, ts("2026-10-18T06:00:00Z"), Some(f64::NAN));
        assert_eq!(latest.value(0), Some(3.0));
        // Of two values at one time, the later row.
        latest.push(0, ts("2026-10-18T05:50:00Z"), Some(4.0));
        assert_eq!(latest.value(0), Some(4.0));
        assert_eq!(
            frame_window(ts("2026-10-18T06:00:00Z"), Duration::hours(1)),
            (ts("2026-10-18T05:00:00Z"), ts("2026-10-18T06:00:00Z"))
        );
    }

    /// The default 10-minute step advertises 37 steps over the 6 h window;
    /// a TIME floors to its step, the instant `resolve_frame_end` resolves,
    /// and no TIME is the latest advertised step.
    #[test]
    fn frames_step_on_the_time_step_over_six_hours() {
        let step = Duration::minutes(10);
        let times = advertised_times(
            Some((ts("2026-07-01T00:00:00Z"), ts("2026-07-11T20:27:37Z"))),
            step,
        );
        assert_eq!(times.first(), Some(&ts("2026-07-11T14:20:00Z")));
        assert_eq!(times.last(), Some(&ts("2026-07-11T20:20:00Z")));
        assert_eq!(times.len(), 37);
        assert!(times.windows(2).all(|w| w[1] - w[0] == step));
        assert_eq!(
            resolve_frame_end(&times, Some(ts("2026-07-11T20:29:59Z")), step),
            Some(ts("2026-07-11T20:20:00Z"))
        );
        assert_eq!(
            resolve_frame_end(&times, None, step),
            Some(ts("2026-07-11T20:20:00Z"))
        );
        assert_eq!(resolve_frame_end(&[], None, step), None);
        // A short extent starts at its first observation's step.
        let short = advertised_times(
            Some((ts("2026-07-11T20:00:30Z"), ts("2026-07-11T20:10:00Z"))),
            Duration::minutes(1),
        );
        assert_eq!(short.first(), Some(&ts("2026-07-11T20:00:00Z")));
        assert_eq!(short.len(), 11);
        assert!(advertised_times(None, step).is_empty());
    }

    /// A section resolves field by field over the defaults (PT1H, 6 px,
    /// stations without a value left out, PT10M); out-of-range values name
    /// their field.
    #[test]
    fn settings_resolve_over_the_defaults() {
        assert_eq!(
            StationMapSettings::resolve(None).unwrap(),
            StationMapSettings::default()
        );
        let resolved = StationMapSettings::resolve(Some(&StationMapConfig {
            max_age: Some("PT30M".into()),
            marker_size: None,
            draw_missing: Some(true),
            time_step: Some("PT5M".into()),
        }))
        .unwrap();
        assert_eq!(
            resolved,
            StationMapSettings {
                max_age: Duration::minutes(30),
                marker_size: 6,
                draw_missing: true,
                time_step: Duration::minutes(5),
            }
        );
        for (bad, expected) in [
            (
                StationMapConfig {
                    max_age: Some("P8D".into()),
                    ..Default::default()
                },
                "max_age 'P8D'",
            ),
            (
                StationMapConfig {
                    marker_size: Some(0),
                    ..Default::default()
                },
                "marker_size 0",
            ),
            (
                StationMapConfig {
                    time_step: Some("PT30S".into()),
                    ..Default::default()
                },
                "time_step 'PT30S'",
            ),
        ] {
            assert_eq!(
                StationMapSettings::resolve(Some(&bad)).unwrap_err(),
                expected
            );
        }
    }
}
