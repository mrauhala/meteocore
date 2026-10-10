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
    /// Serving, but the engine's in-memory store is still refilling from a
    /// push feed that only republishes what it holds now and then (#1000):
    /// it started empty, or was restored from a snapshot older than the
    /// warm-up, so whatever the feed published meanwhile is missing.
    /// Reported as degraded, with how much has arrived, until the engine's
    /// warm-up period is over. `items` names what is counted (`"alerts"`).
    WarmingUp {
        received: u64,
        items: &'static str,
        cause: WarmupCause,
    },
}

/// Why a [`LiveStatus::WarmingUp`] engine is warming up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WarmupCause {
    /// Nothing to restore: no state store, no snapshot, or an unusable one.
    #[default]
    ColdStart,
    /// A snapshot was restored, but the server was down longer than the
    /// warm-up: the restored items are served, the gap refills.
    LongOutage,
}

impl LiveStatus {
    pub fn is_ready(self) -> bool {
        matches!(self, LiveStatus::Ready)
    }

    /// The operator-facing `/health` message of a non-ready status.
    pub fn degraded_reason(self) -> Option<String> {
        match self {
            LiveStatus::Ready => None,
            LiveStatus::Degraded { reason } => Some(reason.to_string()),
            LiveStatus::WarmingUp {
                received,
                items,
                cause,
            } => {
                let after = match cause {
                    WarmupCause::ColdStart => "cold start",
                    WarmupCause::LongOutage => "a long outage",
                };
                Some(format!(
                    "warming up after {after}: {received} {items} received"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LiveStatus, WarmupCause};

    #[test]
    fn degraded_reason_names_the_warm_up_count() {
        assert_eq!(LiveStatus::Ready.degraded_reason(), None);
        assert_eq!(
            LiveStatus::Degraded {
                reason: "broker down"
            }
            .degraded_reason(),
            Some("broker down".into())
        );
        let warming = |cause| LiveStatus::WarmingUp {
            received: 42,
            items: "alerts",
            cause,
        };
        assert!(!warming(WarmupCause::ColdStart).is_ready());
        assert_eq!(
            warming(WarmupCause::ColdStart).degraded_reason(),
            Some("warming up after cold start: 42 alerts received".into())
        );
        assert_eq!(
            warming(WarmupCause::LongOutage).degraded_reason(),
            Some("warming up after a long outage: 42 alerts received".into())
        );
    }
}
