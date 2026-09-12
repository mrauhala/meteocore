//! MQTT v5 subscription to a WIS2 Global Broker.
//!
//! One [`Subscriber`] = one broker session for one collection. It owns the
//! rumqttc event loop, re-subscribes after every fresh session, parses each
//! `PUBLISH` into a [`Notification`], filters duplicates and hands the rest to
//! a bounded channel. Reconnects with exponential backoff until the engine's
//! [`Shutdown`] fires.
//!
//! Session semantics (see `Wis2Config` docs): `clean_start = false` with a
//! bounded `session_expiry_interval`, so a short reconnect resumes the QoS-1
//! backlog the broker queued meanwhile, while a retired replica's session
//! expires on its own. The client id carries a per-process random suffix so
//! two replicas of the same collection never evict each other.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::Duration as ChronoDuration;
use ds_core::config::Wis2Config;
use ds_poll::Shutdown;
use rumqttc::v5::mqttbytes::v5::{ConnectReturnCode, Filter, Packet};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, ConnectionError, Event, MqttOptions};
use rumqttc::{TlsConfiguration, Transport};
use tokio::sync::mpsc;

use crate::dedup::{Dedup, Verdict};
use crate::status::{DropReason, Status};
use crate::{parse_notification, Notification, Wis2Error};

/// Channel depth between the broker task and the consumer. When full, the
/// event loop stops reading (the broker queues up to 2000 messages per
/// client) instead of dropping.
pub const CHANNEL_CAPACITY: usize = 1024;
/// Largest MQTT packet accepted (rumqttc defaults to 10 KiB, which would
/// reject a notification with 4 KiB of inline content once JSON overhead is
/// added).
const MAX_PACKET_SIZE: u32 = 1024 * 1024;
const CONNECTION_TIMEOUT_SECS: u64 = 10;
const RECEIVE_MAXIMUM: u16 = 64;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

pub struct Subscriber;

/// Resolved, validated connection parameters.
#[derive(Debug, Clone)]
pub struct SubscriberOptions {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub client_id: String,
    pub topics: Vec<String>,
    pub session_expiry_secs: u32,
    pub keep_alive: Duration,
    pub dedup_window: ChronoDuration,
}

impl SubscriberOptions {
    /// Build from a validated [`Wis2Config`]. `label` (the collection id)
    /// becomes part of the client id. Reads `password_env` here so a missing
    /// variable is a construction error, not a silent auth failure.
    pub fn from_config(cfg: &Wis2Config, label: &str) -> Result<Self, Wis2Error> {
        let (tls, host, port) = cfg
            .broker_parts()
            .ok_or_else(|| Wis2Error::Config(format!("invalid broker URL '{}'", cfg.broker)))?;
        let password = match &cfg.password_env {
            Some(var) => std::env::var(var).map_err(|_| {
                Wis2Error::Config(format!(
                    "password_env '{var}' is not set in the environment"
                ))
            })?,
            None => cfg.password.clone(),
        };
        let dedup_window = ds_core::datetime::parse_iso8601_duration(&cfg.dedup_window)
            .map_err(|e| Wis2Error::Config(format!("dedup_window: {e}")))?;
        let prefix = cfg.client_id_prefix.as_deref().unwrap_or("meteocore");
        let label_part: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .take(32)
            .collect();
        Ok(SubscriberOptions {
            tls,
            host,
            port,
            username: cfg.username.clone(),
            password,
            client_id: format!("{prefix}-{label_part}-{}", random_suffix()),
            topics: cfg.topics.iter().map(|t| t.trim().to_string()).collect(),
            session_expiry_secs: cfg.session_expiry_secs,
            keep_alive: Duration::from_secs(cfg.keep_alive_secs.max(1)),
            dedup_window,
        })
    }

    fn mqtt_options(&self) -> Result<MqttOptions, Wis2Error> {
        let mut opts = MqttOptions::new(self.client_id.clone(), self.host.clone(), self.port);
        opts.set_credentials(self.username.clone(), self.password.clone());
        opts.set_keep_alive(self.keep_alive);
        opts.set_connection_timeout(CONNECTION_TIMEOUT_SECS);
        opts.set_max_packet_size(Some(MAX_PACKET_SIZE));
        opts.set_receive_maximum(Some(RECEIVE_MAXIMUM));
        if self.session_expiry_secs == 0 {
            opts.set_clean_start(true);
        } else {
            opts.set_clean_start(false);
            opts.set_session_expiry_interval(Some(self.session_expiry_secs));
        }
        if self.tls {
            opts.set_transport(Transport::Tls(TlsConfiguration::Rustls(tls_config()?)));
        }
        Ok(opts)
    }
}

/// rustls client config with the Mozilla root set compiled in. Built
/// explicitly (rather than rumqttc's default) so a container without
/// `ca-certificates` still connects, and with the `ring` provider named so
/// the process-wide "which CryptoProvider" ambiguity (both `ring` and
/// `aws-lc-rs` are in the dependency graph) cannot panic at connect time.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, Wis2Error> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| Wis2Error::Config(format!("tls: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// 8 hex chars unique per process (hash of a random per-process seed, the
/// pid and the clock) — no extra dependency needed.
fn random_suffix() -> String {
    let mut h = RandomState::new().build_hasher();
    std::process::id().hash(&mut h);
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    format!("{:08x}", (h.finish() & 0xffff_ffff) as u32)
}

impl Subscriber {
    /// Spawn the broker task on the current runtime (must be the background
    /// poll runtime). Returns the notification receiver; the task ends when
    /// `shutdown` fires or the receiver is dropped.
    pub fn spawn(
        opts: SubscriberOptions,
        label: String,
        shutdown: Arc<Shutdown>,
        status: Arc<Status>,
    ) -> mpsc::Receiver<Notification> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        tokio::spawn(async move {
            run(opts, label, shutdown, status, tx).await;
        });
        rx
    }
}

async fn run(
    opts: SubscriberOptions,
    label: String,
    shutdown: Arc<Shutdown>,
    status: Arc<Status>,
    tx: mpsc::Sender<Notification>,
) {
    let mut dedup = Dedup::new(opts.dedup_window);
    let mut backoff = BACKOFF_MIN;
    let filters: Vec<Filter> = opts
        .topics
        .iter()
        .map(|t| Filter::new(t.clone(), QoS::AtLeastOnce))
        .collect();

    'connect: loop {
        if shutdown.is_shutdown() {
            break;
        }
        let mqtt_opts = match opts.mqtt_options() {
            Ok(o) => o,
            Err(e) => {
                tracing::error!("[{label}] wis2: cannot build MQTT options: {e}");
                break;
            }
        };
        let (client, mut eventloop) = AsyncClient::new(mqtt_opts, 64);
        tracing::info!(
            "[{label}] wis2: connecting to {}://{}:{} as '{}' ({} topic filter(s))",
            if opts.tls { "mqtts" } else { "mqtt" },
            opts.host,
            opts.port,
            opts.client_id,
            filters.len()
        );

        loop {
            let event = tokio::select! {
                biased;
                _ = shutdown.wait() => {
                    let _ = client.try_disconnect();
                    break 'connect;
                }
                ev = eventloop.poll() => ev,
            };
            match event {
                Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                    if ack.code != ConnectReturnCode::Success {
                        tracing::warn!("[{label}] wis2: broker refused connection: {:?}", ack.code);
                        status.set_disconnected();
                        break;
                    }
                    if status.set_connected() {
                        tracing::info!(
                            "[{label}] wis2: connected (session_present={})",
                            ack.session_present
                        );
                    }
                    backoff = BACKOFF_MIN;
                    if ack.session_present {
                        // Broker kept our subscriptions and the QoS-1 backlog.
                        status.set_subscribed();
                    } else {
                        // `try_*` (sync) — awaiting a client call from the task
                        // that drives the event loop can deadlock on a full
                        // request channel.
                        if let Err(e) = client.try_subscribe_many(filters.clone()) {
                            tracing::warn!("[{label}] wis2: subscribe request failed: {e}");
                            status.set_disconnected();
                            break;
                        }
                    }
                }
                Ok(Event::Incoming(Packet::SubAck(ack))) => {
                    let rejected: Vec<String> = ack
                        .return_codes
                        .iter()
                        .zip(filters.iter())
                        .filter(|(code, _)| {
                            !matches!(
                                code,
                                rumqttc::v5::mqttbytes::v5::SubscribeReasonCode::Success(_)
                            )
                        })
                        .map(|(code, f)| format!("{} ({code:?})", f.path))
                        .collect();
                    if rejected.is_empty() {
                        status.set_subscribed();
                        tracing::info!("[{label}] wis2: subscribed to {} filter(s)", filters.len());
                    } else {
                        tracing::warn!(
                            "[{label}] wis2: broker rejected subscription(s): {}",
                            rejected.join(", ")
                        );
                        if ack.return_codes.len() == rejected.len() {
                            status.set_disconnected();
                            break;
                        }
                        status.set_subscribed();
                    }
                }
                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    status.record_received();
                    let topic = String::from_utf8_lossy(&publish.topic).into_owned();
                    let n = match parse_notification(&topic, &publish.payload) {
                        Ok(n) => n,
                        Err(e) => {
                            status.record_dropped(DropReason::Parse);
                            tracing::debug!("[{label}] wis2: dropped message on '{topic}': {e}");
                            continue;
                        }
                    };
                    match dedup.accept(&n) {
                        Verdict::Accept => {}
                        Verdict::DuplicateId | Verdict::DuplicateData => {
                            status.record_dropped(DropReason::Duplicate);
                            continue;
                        }
                    }
                    status.record_accepted(n.pubtime.timestamp_millis().max(0) as u64);
                    if tx.send(n).await.is_err() {
                        // Consumer gone (engine dropped) — nothing left to do.
                        let _ = client.try_disconnect();
                        break 'connect;
                    }
                }
                Ok(Event::Incoming(Packet::Disconnect(d))) => {
                    tracing::warn!(
                        "[{label}] wis2: broker sent DISCONNECT: {:?}",
                        d.reason_code
                    );
                    status.set_disconnected();
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    let transition = status.set_disconnected();
                    match &e {
                        ConnectionError::ConnectionRefused(code) => {
                            tracing::warn!("[{label}] wis2: connection refused: {code:?}");
                        }
                        other if transition => {
                            tracing::warn!("[{label}] wis2: connection lost: {other}");
                        }
                        other => {
                            tracing::debug!("[{label}] wis2: connect attempt failed: {other}");
                        }
                    }
                    break;
                }
            }
        }

        // Backoff before the next session (with jitter from the random suffix
        // so a fleet does not reconnect in lockstep).
        let jitter = Duration::from_millis((random_suffix_u32() % 500) as u64);
        if !shutdown.sleep(backoff + jitter).await {
            break;
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
    status.set_disconnected();
    tracing::info!("[{label}] wis2: subscriber stopped");
}

fn random_suffix_u32() -> u32 {
    u32::from_str_radix(&random_suffix(), 16).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(topics: &[&str]) -> Wis2Config {
        Wis2Config {
            topics: topics.iter().map(|s| s.to_string()).collect(),
            ..Wis2Config::default()
        }
    }

    #[test]
    fn options_from_default_config() {
        let o =
            SubscriberOptions::from_config(&cfg(&["cache/a/wis2/se-smhi/#"]), "obs-se").unwrap();
        assert!(o.tls);
        assert_eq!(o.host, "globalbroker.meteo.fr");
        assert_eq!(o.port, 8883);
        assert_eq!(o.username, "everyone");
        assert_eq!(o.password, "everyone");
        assert!(o.client_id.starts_with("meteocore-obs-se-"));
        assert_eq!(o.client_id.len(), "meteocore-obs-se-".len() + 8);
        assert_eq!(o.session_expiry_secs, 900);
        assert_eq!(o.keep_alive, Duration::from_secs(30));
        assert_eq!(o.dedup_window, ChronoDuration::hours(1));
        // Two subscribers in one process get different ids.
        let o2 =
            SubscriberOptions::from_config(&cfg(&["cache/a/wis2/se-smhi/#"]), "obs-se").unwrap();
        assert_ne!(o.client_id, o2.client_id);
        assert!(o.mqtt_options().is_ok());
    }

    #[test]
    fn label_is_sanitised_and_password_env_must_exist() {
        let o = SubscriberOptions::from_config(&cfg(&["cache/a"]), "weird id/with:chars").unwrap();
        assert!(o.client_id.starts_with("meteocore-weird-id-with-chars-"));
        let mut c = cfg(&["cache/a"]);
        c.password_env = Some("DS_WIS2_TEST_UNSET_VAR_XYZ".into());
        assert!(matches!(
            SubscriberOptions::from_config(&c, "x"),
            Err(Wis2Error::Config(_))
        ));
    }

    #[test]
    fn tls_config_builds() {
        assert!(tls_config().is_ok());
    }
}
