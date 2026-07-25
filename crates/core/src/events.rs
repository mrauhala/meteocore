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
