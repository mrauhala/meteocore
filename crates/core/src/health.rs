//! Live (post-boot) health signal an engine can expose.
//!
//! `/health` reports each collection's boot status (loaded / degraded /
//! failed). An engine with a long-lived upstream — a database, a WIS2 broker
//! session — additionally reports whether that upstream is currently
//! reachable so the status flips at runtime. Engines return `None` from their
//! `live_health()` until they have actually probed the upstream, so a
//! boot-degraded collection keeps its boot status instead of briefly flashing
//! `ready` from an optimistic seed.

/// Runtime health of one collection's upstream connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveStatus {
    Ready,
    /// Serving from the last good snapshot; `reason` is a short operator-facing
    /// phrase (`"broker disconnected"`) — never an error string with hosts or
    /// credentials in it.
    Degraded {
        reason: &'static str,
    },
}

impl LiveStatus {
    pub fn is_ready(self) -> bool {
        matches!(self, LiveStatus::Ready)
    }
}
