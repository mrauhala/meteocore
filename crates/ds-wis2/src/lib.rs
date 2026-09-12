//! WMO WIS2 data-consumer client.
//!
//! WIS2 (WMO Information System 2.0) distributes real-time weather data as
//! **notification messages** on MQTT Global Brokers: a small GeoJSON message
//! per data object that either embeds the object inline (≤ 4 KiB) or links to
//! it on a Global Cache / the producer's server. This crate is the shared
//! consumer side used by every MeteoCore engine that ingests through WIS2:
//!
//! - [`Subscriber`] — one MQTT v5 session per collection with reconnect,
//!   session resumption and per-message parsing into [`Notification`].
//! - [`Dedup`] — the mandatory duplicate filter: every notification is
//!   republished once per Global Cache (six copies in practice), keyed by
//!   `data_id` with latest `pubtime` winning.
//! - [`Fetcher`] / [`DownloadPolicy`] — payload retrieval (inline content or
//!   an HTTPS download under an SSRF policy) with checksum verification.
//! - [`spawn_pipeline`] — the three stages wired together, yielding
//!   [`Resolved`] payloads on a channel.
//! - [`Status`] — atomics an engine reads for `/health` and `/metrics`.
//!
//! **Runtime rule:** everything here is async and must be spawned on the
//! server's background poll runtime (`poll_runtime()`), never on the
//! request-serving runtime — the pipeline is long-lived and does network I/O.

pub mod dedup;
pub mod fetch;
pub mod notification;
pub mod payload;
pub mod pipeline;
pub mod policy;
pub mod status;
pub mod subscriber;

pub use dedup::Dedup;
pub use fetch::Fetcher;
pub use notification::{parse_notification, Content, Geometry, Integrity, Link, Notification};
pub use payload::{Payload, PayloadSource};
pub use pipeline::{spawn_pipeline, Resolved};
pub use policy::DownloadPolicy;
pub use status::{DropReason, Status, StatusSnapshot};
pub use subscriber::Subscriber;

/// Errors raised by the WIS2 client. Messages may contain URLs and broker
/// details — log them, never return them to an HTTP client.
#[derive(Debug, thiserror::Error)]
pub enum Wis2Error {
    #[error("invalid WIS2 configuration: {0}")]
    Config(String),
    #[error("notification parse error: {0}")]
    Parse(String),
    #[error("download rejected by policy: {0}")]
    Policy(String),
    #[error("download failed: {0}")]
    Download(String),
    #[error("payload too large: {0}")]
    TooLarge(String),
    #[error("inline content decode error: {0}")]
    Decode(String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("broker error: {0}")]
    Broker(String),
}

impl From<Wis2Error> for ds_core::error::DataServerError {
    fn from(e: Wis2Error) -> Self {
        match e {
            Wis2Error::Config(m) => ds_core::error::DataServerError::Config(m),
            // Everything else is an ingest-side failure; the generic Engine
            // variant maps to a 500 with a fixed message (no detail leak).
            other => ds_core::error::DataServerError::Engine(other.to_string()),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    /// Load a captured broker message from `tests/fixtures/<name>` (the
    /// capture format wraps the raw message as `{"topic", "message"}`).
    pub fn fixture(name: &str) -> (String, Vec<u8>) {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
        let raw = std::fs::read(format!("{path}{name}")).expect("fixture exists");
        let v: serde_json::Value = serde_json::from_slice(&raw).expect("fixture is json");
        let topic = v["topic"].as_str().expect("topic").to_string();
        let payload = serde_json::to_vec(&v["message"]).expect("message");
        (topic, payload)
    }
}
