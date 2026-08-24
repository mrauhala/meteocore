//! Point-event sources (#549): lightning strikes and similar timestamped
//! point observations, joinable onto other engines' domain objects (e.g.
//! nowcast cell tracks). Framework-free like the rest of ds-core — the
//! trait is the seam that lets engine-nowcast consume engine-postgis
//! events without depending on it.

use chrono::{DateTime, Utc};

use crate::error::DataServerError;

/// One point event in WGS84.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EventPoint {
    pub time: DateTime<Utc>,
    pub lon: f64,
    pub lat: f64,
    /// Optional per-event scalars, populated only when the source declares
    /// the columns they come from.
    pub attrs: EventAttrs,
}

/// Per-event scalars a detection network may report alongside position.
///
/// **Flat and `Copy` on purpose.** A consumer fetches up to
/// `MAX_JOIN_STRIKES` (200k) events per cycle, so anything with a heap
/// allocation per event — a map, a `Vec` — would allocate 200k times per
/// generation on the poll runtime. This is 12 bytes and never allocates.
///
/// The fields are named for lightning because lightning is what reports
/// them. Inventing neutral names (`kind`, `magnitude`) would buy a
/// generality nothing is asking for while making both sides harder to read;
/// a source with nothing to say leaves them `None`. Rename if a second kind
/// of event source ever appears.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EventAttrs {
    /// 0 = cloud-to-ground, 1 = intra-cloud.
    pub cloud_indicator: Option<i16>,
    /// Signed peak current in kA — **polarity is its sign**. A positive
    /// cloud-to-ground flash is the severe-storm signal, so the sign carries
    /// more meaning here than the magnitude.
    pub peak_current_ka: Option<f32>,
}

impl EventAttrs {
    /// Cloud-to-ground, if known.
    pub fn is_cloud_to_ground(&self) -> Option<bool> {
        self.cloud_indicator.map(|c| c == 0)
    }

    /// Positive polarity, if known. `None` for a zero current, which carries
    /// no polarity rather than a positive one.
    pub fn is_positive(&self) -> Option<bool> {
        self.peak_current_ka
            .and_then(|c| match c.partial_cmp(&0.0) {
                Some(std::cmp::Ordering::Greater) => Some(true),
                Some(std::cmp::Ordering::Less) => Some(false),
                _ => None,
            })
    }
}

/// A source of recent point events.
///
/// Contract:
/// - `recent_events` returns every event in the HALF-OPEN window
///   `(start, end]` across the source's whole extent, ascending by time,
///   capped at `limit` — when capped, the NEWEST events are kept (the cap
///   is a safety valve; callers treat `len() == limit` as possible
///   truncation and may log it).
/// - One bounded call per consumer cycle (e.g. per nowcast generation),
///   never per object — implementations may hit a database.
/// - Implementations are sync bridges over async I/O in practice
///   (engine-postgis): call this from a MULTI-THREAD runtime worker (the
///   background poll runtime), never from `spawn_blocking` and never from
///   a request-handler task — the same rules as `ds-storage`
///   (root CLAUDE.md rule 7).
pub trait EventSource: Send + Sync {
    fn recent_events(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<EventPoint>, DataServerError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_attrs_stays_small_and_allocation_free() {
        // The whole design rationale is "flat and Copy so 200k events per
        // generation cost nothing to carry". A field that reintroduced a heap
        // allocation — a String, a Vec, a HashMap — would break that silently,
        // since it still compiles and still passes every behavioural test.
        assert_eq!(std::mem::size_of::<EventAttrs>(), 12);
        fn assert_copy<T: Copy>() {}
        assert_copy::<EventAttrs>();
        assert_copy::<EventPoint>();
    }

    #[test]
    fn polarity_comes_from_the_sign_and_zero_carries_none() {
        let at = |c: f32| EventAttrs {
            cloud_indicator: None,
            peak_current_ka: Some(c),
        };
        assert_eq!(at(30.0).is_positive(), Some(true));
        assert_eq!(at(-30.0).is_positive(), Some(false));
        // Zero current has no polarity — reporting it as positive would
        // inflate the severe-storm signal from a non-measurement.
        assert_eq!(at(0.0).is_positive(), None);
        assert_eq!(at(f32::NAN).is_positive(), None);
        assert_eq!(EventAttrs::default().is_positive(), None);
    }

    #[test]
    fn cloud_indicator_maps_zero_to_cloud_to_ground() {
        let at = |c: i16| EventAttrs {
            cloud_indicator: Some(c),
            peak_current_ka: None,
        };
        assert_eq!(at(0).is_cloud_to_ground(), Some(true));
        assert_eq!(at(1).is_cloud_to_ground(), Some(false));
        assert_eq!(EventAttrs::default().is_cloud_to_ground(), None);
    }
}
