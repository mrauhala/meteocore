//! How late each collection's data arrives (#1389): the
//! `collection_expected_delay_seconds` gauge.
//!
//! The data age is now minus the newest data a collection serves
//! (`admin::collection_data_ages`), so it includes the feed's delivery
//! delay: a scan published 30 minutes after its timestamp is 30 minutes old
//! the moment the server serves it. `CollectionStale` adds the delay
//! measured here to its threshold.
//!
//! [`DeliveryDelays`] watches the newest timestep of every collection whose
//! engine reports a data age, read through the same accessor the age is
//! ([`DataTimeline::newest`]). An arrival is a newest timestep later than
//! any its engine held before; its delay is the time the server first saw
//! it minus its timestamp, in whole seconds, negative for data that arrives
//! ahead of its timestamp (a forecast's valid time). The gauge is the
//! median of a collection's newest [`WINDOW`] delays, the larger middle one
//! of an even count, as `ds_core::health::expected_interval` takes it.
//!
//! - **Resolution.** The age is read per scrape. This tracker reads the
//!   same timestamps on its own [`TICK`] on the poll runtime
//!   (`admin::delivery_delay_loop`), so a delay is exact to within the
//!   tick whether or not anything scrapes, and a scrape only reads the
//!   medians it published. It counts from when the engine's poll published
//!   the timestep: the poll interval is in the delay as it is in the age.
//! - **Baseline.** What an engine holds the first time it is seen with
//!   data is a baseline, not an arrival: at boot the server did not see it
//!   arrive, an engine that loads in its first poll (satellite) held
//!   nothing before, and a rebuilt engine (a reload changed its collection)
//!   scanned what its predecessor already served. A collection has no
//!   series until its first arrival.
//! - **A rebuilt engine keeps its collection's delays:** they describe the
//!   same feed, and dropping them on each config edit would leave the alert
//!   without a delay until the next arrival. A collection that leaves the
//!   registry loses them.
//! - **An outage freezes it.** No arrival, no delay: the median stays what
//!   it was, so a stopped feed's age outgrows the threshold and the alert
//!   fires. Late arrivals, a backlog trickling in after an outage, leave the
//!   median while they are fewer than half the window.
//! - **Memory:** at most [`WINDOW`] delays per collection; an observation
//!   is O(collections × WINDOW), a scrape one map lookup per collection.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};

/// How many of a collection's newest arrivals the delay is the median of:
/// two hours of a 5-minute radar, four of a 10-minute scan, six days of a
/// 6-hourly model. Up to 11 late arrivals among them leave the median.
pub(crate) const WINDOW: usize = 24;

/// How often the server reads the newest timesteps
/// ([`DeliveryDelays::observe`]): the resolution of a delay.
pub(crate) const TICK: Duration = Duration::from_secs(10);

/// A collection whose engine reports a data age
/// (`ds_core::config::DATA_AGE_ENGINES`), as the data age gauges and the
/// delay read it: snapshot reads, no I/O.
pub(crate) trait DataTimeline: Send + Sync {
    fn id(&self) -> &str;
    /// The newest data the collection serves; its data age is now minus it.
    fn newest(&self) -> Option<DateTime<Utc>>;
    /// The spacing of that timeline (#1375): `ds_core::health::expected_interval`.
    fn interval(&self) -> Option<chrono::Duration>;
}

/// The delays of every collection, and their published medians.
#[derive(Default)]
pub(crate) struct DeliveryDelays {
    tracks: Mutex<HashMap<String, Track>>,
    /// Each collection's median, as the last [`Self::observe`] left it:
    /// what a scrape reads.
    medians: ArcSwap<HashMap<String, i64>>,
}

/// One collection's arrivals.
struct Track {
    /// The engine serving it, by identity: a rebuilt engine is another.
    /// Weak, so a replaced engine is freed; its allocation, which a
    /// successor could otherwise reuse and pass for it, is kept until the
    /// next observation replaces this.
    engine: Weak<dyn DataTimeline>,
    /// The newest timestep `engine` has held; `None` until it held data.
    newest: Option<DateTime<Utc>>,
    /// Delays in seconds, oldest first, at most [`WINDOW`].
    delays: VecDeque<i64>,
}

impl DeliveryDelays {
    /// Read every collection's newest timestep at `now`: one later than any
    /// its engine held adds its delay, a new engine's first data is its
    /// baseline, a collection not in `timelines` is forgotten. Then publish
    /// the medians.
    pub(crate) fn observe(&self, now: DateTime<Utc>, timelines: &[Arc<dyn DataTimeline>]) {
        let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        let mut observed = HashMap::with_capacity(timelines.len());
        for timeline in timelines {
            let engine = Arc::downgrade(timeline);
            let mut track = tracks.remove(timeline.id()).unwrap_or_else(|| Track {
                engine: engine.clone(),
                newest: None,
                delays: VecDeque::with_capacity(WINDOW),
            });
            if !Weak::ptr_eq(&track.engine, &engine) {
                // Rebuilt: what the new engine holds was not seen arriving.
                track.engine = engine;
                track.newest = None;
            }
            match (track.newest, timeline.newest()) {
                (None, newest) => track.newest = newest,
                (Some(held), Some(newest)) if newest > held => {
                    if track.delays.len() == WINDOW {
                        track.delays.pop_front();
                    }
                    track.delays.push_back((now - newest).num_seconds());
                    track.newest = Some(newest);
                }
                // Nothing newer: the same data, an older timestep after
                // the newest left, or none at all.
                (Some(_), _) => {}
            }
            observed.insert(timeline.id().to_owned(), track);
        }
        *tracks = observed;
        let medians = tracks
            .iter()
            .filter_map(|(id, track)| Some((id.clone(), median(&track.delays)?)))
            .collect();
        self.medians.store(Arc::new(medians));
    }

    /// The median delay of every collection with an arrival, as the last
    /// observation published it.
    pub(crate) fn medians(&self) -> Arc<HashMap<String, i64>> {
        self.medians.load_full()
    }
}

/// The median of `delays`, the larger middle one of an even count; `None`
/// without any.
fn median(delays: &VecDeque<i64>) -> Option<i64> {
    let mut sorted: Vec<i64> = delays.iter().copied().collect();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied()
}

#[cfg(test)]
mod tests {
    use super::{DataTimeline, DeliveryDelays, WINDOW};
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use std::sync::{Arc, Mutex};

    /// An engine whose newest timestep the test sets.
    struct Feed {
        id: &'static str,
        newest: Mutex<Option<DateTime<Utc>>>,
    }

    impl DataTimeline for Feed {
        fn id(&self) -> &str {
            self.id
        }
        fn newest(&self) -> Option<DateTime<Utc>> {
            *self.newest.lock().unwrap()
        }
        fn interval(&self) -> Option<chrono::Duration> {
            None
        }
    }

    fn feed(id: &'static str, newest: Option<DateTime<Utc>>) -> Arc<Feed> {
        Arc::new(Feed {
            id,
            newest: Mutex::new(newest),
        })
    }

    /// `minutes` after midnight.
    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap() + Duration::minutes(minutes)
    }

    fn timelines(feeds: &[&Arc<Feed>]) -> Vec<Arc<dyn DataTimeline>> {
        feeds
            .iter()
            .map(|f| Arc::clone(f) as Arc<dyn DataTimeline>)
            .collect()
    }

    /// `feed` publishes the timestep of minute `timestep`; the server sees
    /// it at minute `seen`.
    fn arrive(delays: &DeliveryDelays, feed: &Arc<Feed>, timestep: i64, seen: i64) {
        *feed.newest.lock().unwrap() = Some(at(timestep));
        delays.observe(at(seen), &timelines(&[feed]));
    }

    fn delay(delays: &DeliveryDelays, id: &str) -> Option<i64> {
        delays.medians().get(id).copied()
    }

    /// What an engine holds when first seen is a baseline: at boot, and
    /// when it first holds data, the arrival was not observed. The first
    /// newer timestep is the first delay.
    #[test]
    fn data_held_when_first_seen_is_a_baseline() {
        let delays = DeliveryDelays::default();
        let radar = feed("radar", Some(at(0)));
        // Held at boot, an hour before the server saw it.
        delays.observe(at(60), &timelines(&[&radar]));
        assert_eq!(delay(&delays, "radar"), None);
        delays.observe(at(61), &timelines(&[&radar]));
        assert_eq!(delay(&delays, "radar"), None, "the same timestep again");

        // An engine that loads in its first poll: empty, then its backlog.
        let satellite = feed("satellite", None);
        delays.observe(at(60), &timelines(&[&radar, &satellite]));
        *satellite.newest.lock().unwrap() = Some(at(50));
        delays.observe(at(61), &timelines(&[&radar, &satellite]));
        assert_eq!(delay(&delays, "satellite"), None);

        // Observed arrivals: 12 and 31 minutes late.
        *radar.newest.lock().unwrap() = Some(at(5));
        delays.observe(at(17), &timelines(&[&radar, &satellite]));
        assert_eq!(delay(&delays, "radar"), Some(12 * 60));
        assert_eq!(delay(&delays, "satellite"), None);
        *satellite.newest.lock().unwrap() = Some(at(60));
        delays.observe(at(91), &timelines(&[&radar, &satellite]));
        assert_eq!(delay(&delays, "satellite"), Some(31 * 60));
        assert_eq!(delay(&delays, "radar"), Some(12 * 60));
    }

    /// The median of the newest arrivals, the larger middle one of an even
    /// count: one late delivery does not move it, a forecast ahead of its
    /// timestamp is negative.
    #[test]
    fn the_delay_is_the_median_of_the_arrivals() {
        let delays = DeliveryDelays::default();
        let scan = feed("scan", Some(at(0)));
        delays.observe(at(30), &timelines(&[&scan]));
        // A 10-minute scan delivered 30, 32, 50 and 31 minutes late.
        for (timestep, late) in [(10, 30), (20, 32), (30, 50)] {
            arrive(&delays, &scan, timestep, timestep + late);
        }
        assert_eq!(delay(&delays, "scan"), Some(32 * 60));
        arrive(&delays, &scan, 40, 71);
        assert_eq!(delay(&delays, "scan"), Some(32 * 60), "30 31 32 50");

        let forecast = feed("forecast", Some(at(600)));
        delays.observe(at(0), &timelines(&[&forecast]));
        arrive(&delays, &forecast, 960, 360);
        assert_eq!(delay(&delays, "forecast"), Some(-10 * 3600));
    }

    /// Only the newest [`WINDOW`] arrivals count: a feed that became
    /// prompter is measured by its new delay once those fill the window.
    #[test]
    fn only_the_newest_window_of_arrivals_counts() {
        let delays = DeliveryDelays::default();
        let radar = feed("radar", Some(at(0)));
        delays.observe(at(0), &timelines(&[&radar]));
        let mut timestep = 0;
        for late in [60; WINDOW].into_iter().chain([5; WINDOW]) {
            timestep += 5;
            arrive(&delays, &radar, timestep, timestep + late);
        }
        // Over every arrival the larger middle one would be an hour.
        assert_eq!(delay(&delays, "radar"), Some(5 * 60));
        let held = delays.tracks.lock().unwrap()["radar"].delays.len();
        assert_eq!(held, WINDOW);
    }

    /// A rebuilt engine (a reload changed the collection) is a new
    /// baseline: the newer timestep its own scan found is not an arrival.
    /// The collection keeps its delays, and the new engine's arrivals add
    /// to them.
    #[test]
    fn a_rebuilt_engine_is_a_new_baseline() {
        let delays = DeliveryDelays::default();
        let old = feed("radar", Some(at(0)));
        delays.observe(at(10), &timelines(&[&old]));
        arrive(&delays, &old, 5, 15);
        assert_eq!(delay(&delays, "radar"), Some(10 * 60));

        // The rebuilt engine scanned a timestep that arrived meanwhile.
        let rebuilt = feed("radar", Some(at(10)));
        drop(old);
        delays.observe(at(60), &timelines(&[&rebuilt]));
        assert_eq!(delay(&delays, "radar"), Some(10 * 60), "not 50 minutes");

        arrive(&delays, &rebuilt, 15, 35);
        assert_eq!(delay(&delays, "radar"), Some(20 * 60), "10 20");
    }

    /// A newest timestep that leaves and comes back, or data that ages out,
    /// is no arrival; data newer than any held is, even after the
    /// collection held none.
    #[test]
    fn only_a_timestep_newer_than_any_held_is_an_arrival() {
        let delays = DeliveryDelays::default();
        let radar = feed("radar", Some(at(0)));
        delays.observe(at(0), &timelines(&[&radar]));
        arrive(&delays, &radar, 5, 10);
        arrive(&delays, &radar, 0, 11);
        arrive(&delays, &radar, 5, 12);
        assert_eq!(delay(&delays, "radar"), Some(5 * 60));

        *radar.newest.lock().unwrap() = None;
        delays.observe(at(20), &timelines(&[&radar]));
        assert_eq!(delay(&delays, "radar"), Some(5 * 60), "kept while empty");
        arrive(&delays, &radar, 30, 37);
        arrive(&delays, &radar, 35, 42);
        assert_eq!(delay(&delays, "radar"), Some(7 * 60), "5 7 7");
    }

    /// A collection that leaves the registry loses its delays: back under
    /// the same id it starts from a baseline.
    #[test]
    fn a_removed_collection_is_forgotten() {
        let delays = DeliveryDelays::default();
        let (radar, model) = (feed("radar", Some(at(0))), feed("model", Some(at(0))));
        delays.observe(at(0), &timelines(&[&radar, &model]));
        *radar.newest.lock().unwrap() = Some(at(5));
        *model.newest.lock().unwrap() = Some(at(360));
        delays.observe(at(400), &timelines(&[&radar, &model]));
        assert_eq!(delays.medians().len(), 2);

        delays.observe(at(401), &timelines(&[&model]));
        assert_eq!(delay(&delays, "radar"), None);
        assert_eq!(delay(&delays, "model"), Some(40 * 60));
        *radar.newest.lock().unwrap() = Some(at(400));
        delays.observe(at(402), &timelines(&[&radar, &model]));
        assert_eq!(delay(&delays, "radar"), None, "a baseline again");
    }
}
