//! WIS2 push source: station reports pushed as Global Broker notifications.
//!
//! Most SYNOP notifications carry the whole BUFR message inline
//! (`properties.content`, a few hundred bytes), so the common path never
//! touches HTTP; link-only producers are downloaded by the `ds-wis2`
//! pipeline under its policy. Every accepted payload is decoded and
//! ingested exactly like a scanned file; the `(station, time)` rows each
//! `data_id` produced are remembered for a bounded time so a `rel=deletion`
//! notification withdraws exactly those rows.
//!
//! Runs from `BufrEngine::poll_loop` on the background runtime — the
//! pipeline is started here, never in the constructor.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use ds_core::config::Wis2Config;
use ds_core::health::LiveStatus;
use ds_poll::{FirstTick, Shutdown};
use ds_wis2::{Resolved, Status, StatusSnapshot};

use crate::engine::BufrEngine;
use crate::health::Health;

/// `data_id` → rows it produced, remembered for deletions. Bounded by
/// count (a global feed is ~10 messages/s; 200 k ≈ 5 h).
const MAX_REMEMBERED: usize = 200_000;

pub struct Wis2Source {
    config: Wis2Config,
    stale_after: chrono::Duration,
    degrade_after: Duration,
    status: ArcSwap<Option<Arc<Status>>>,
    produced: Mutex<Produced>,
    /// `(centre, reason)` pairs already logged at WARN — every further
    /// failure of that kind is counted only (a centre with an unsupported
    /// template would otherwise WARN on every report).
    warned: Mutex<HashSet<(String, &'static str)>>,
}

#[derive(Default)]
struct Produced {
    by_data_id: HashMap<String, Vec<(String, DateTime<Utc>)>>,
    order: VecDeque<String>,
}

impl Wis2Source {
    pub fn new(config: Wis2Config, stale_after: chrono::Duration) -> Self {
        let degrade_after = Duration::from_secs(config.degrade_after_secs.max(1));
        Wis2Source {
            config,
            stale_after,
            degrade_after,
            status: ArcSwap::from_pointee(None),
            produced: Mutex::new(Produced::default()),
            warned: Mutex::new(HashSet::new()),
        }
    }

    pub fn config(&self) -> &Wis2Config {
        &self.config
    }

    pub fn status_snapshot(&self) -> Option<StatusSnapshot> {
        let guard = self.status.load();
        guard.as_ref().as_ref().map(|s| s.snapshot())
    }

    /// Ready once subscribed, the session is not down for longer than
    /// `degrade_after_secs`, and a notification has been accepted within
    /// `stale_after` (a quiet observation feed is not healthy — unlike a
    /// quiet warning feed).
    pub fn live_status(&self, health: &Health) -> LiveStatus {
        let guard = self.status.load();
        let Some(status) = guard.as_ref().as_ref() else {
            return LiveStatus::Degraded {
                reason: "connecting to WIS2 broker",
            };
        };
        let snap = status.snapshot();
        if !snap.connected {
            return match snap.disconnected_for_secs {
                Some(secs) if secs >= self.degrade_after.as_secs() => LiveStatus::Degraded {
                    reason: "WIS2 broker disconnected",
                },
                Some(_) if health.is_probed() => LiveStatus::Ready,
                _ => LiveStatus::Degraded {
                    reason: "connecting to WIS2 broker",
                },
            };
        }
        if !snap.subscribed {
            return LiveStatus::Degraded {
                reason: "WIS2 subscription not acknowledged",
            };
        }
        match snap.last_message_age_secs {
            None => {
                if health.is_probed() {
                    LiveStatus::Ready
                } else {
                    LiveStatus::Degraded {
                        reason: "waiting for first WIS2 notification",
                    }
                }
            }
            Some(age) if age as i64 > self.stale_after.num_seconds() => LiveStatus::Degraded {
                reason: "no WIS2 notification within stale_after",
            },
            // Fresh notifications alone are not health: a topic whose every
            // payload fails to decode would otherwise stay green with nothing
            // served. `probed` flips on the first successfully decoded report.
            Some(_) if health.is_probed() => LiveStatus::Ready,
            Some(_) => LiveStatus::Degraded {
                reason: "no BUFR reports decoded from WIS2 yet",
            },
        }
    }

    /// Drive the subscription until `shutdown` fires. Called from
    /// `BufrEngine::poll_loop` (background runtime).
    pub async fn run(&self, engine: &BufrEngine, shutdown: &Shutdown) {
        let label = engine.collection_id().to_string();
        let pipeline_shutdown = Arc::new(Shutdown::new());
        let mut pipeline =
            match ds_wis2::spawn_pipeline(&self.config, &label, pipeline_shutdown.clone()) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!("[{label}] bufr/wis2: cannot start subscription: {e}");
                    return;
                }
            };
        self.status.store(Arc::new(Some(pipeline.status.clone())));
        let mut snap = shutdown.ticker(BufrEngine::SNAPSHOT_INTERVAL, FirstTick::Skip);
        let mut prune = shutdown.ticker(BufrEngine::PRUNE_INTERVAL, FirstTick::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.wait() => break,
                r = pipeline.receiver.recv() => match r {
                    Some(r) => self.apply(engine, r),
                    None => {
                        tracing::warn!("[{label}] bufr/wis2: pipeline ended");
                        break;
                    }
                },
                _ = prune.tick() => engine.prune_now(),
                _ = snap.tick() => engine.snapshot_if_dirty(),
            }
        }
        // The pipeline's own Shutdown is private to this loop so a reload can
        // never leave a subscriber behind.
        pipeline_shutdown.shutdown();
    }

    fn apply(&self, engine: &BufrEngine, r: Resolved) {
        let n = r.notification;
        let Some(payload) = r.payload else {
            self.delete(engine, &n.data_id);
            return;
        };
        engine.health.files_total.fetch_add(1, Ordering::Relaxed);
        let keys = match engine.ingest_bytes_keyed(&payload.bytes, &n.data_id, Utc::now()) {
            Ok(k) => k,
            Err(e) => {
                let centre = n.centre_id.clone().unwrap_or_default();
                let kind = match &e {
                    crate::decode::DecodeError::Unsupported(_) => "unsupported",
                    _ => "error",
                };
                let first = self
                    .warned
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert((centre.clone(), kind));
                if first {
                    tracing::warn!(
                        "[{}] bufr/wis2: {centre} payload {} not decoded ({kind}: {e}) — \
                         further {kind} failures from {centre} are counted only",
                        engine.collection_id(),
                        n.data_id
                    );
                }
                return;
            }
        };
        engine.health.mark_probed();
        if keys.is_empty() {
            return;
        }
        let mut p = self.produced.lock().unwrap_or_else(|e| e.into_inner());
        if p.by_data_id.insert(n.data_id.clone(), keys).is_none() {
            p.order.push_back(n.data_id);
            while p.order.len() > MAX_REMEMBERED {
                if let Some(old) = p.order.pop_front() {
                    p.by_data_id.remove(&old);
                }
            }
        }
    }

    fn delete(&self, engine: &BufrEngine, data_id: &str) {
        let keys = {
            let mut p = self.produced.lock().unwrap_or_else(|e| e.into_inner());
            p.by_data_id.remove(data_id)
        };
        if let Some(keys) = keys {
            let n = engine.remove_reports(&keys);
            tracing::debug!(
                "[{}] bufr/wis2: deletion of {data_id} removed {n} report(s)",
                engine.collection_id()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Source;
    use ds_core::config::BufrConfig;
    use ds_core::edr_engine::EdrEngine;
    use ds_wis2::{Link, Notification, Payload, PayloadSource};

    fn engine() -> BufrEngine {
        BufrEngine::new(
            &BufrConfig {
                data_path: None,
                wis2: Some(Wis2Config {
                    topics: vec![
                        "cache/a/wis2/se-smhi/data/core/weather/surface-based-observations/synop"
                            .into(),
                    ],
                    ..Wis2Config::default()
                }),
                poll_interval_secs: 60,
                retention: "P36500D".into(),
                max_stations: 100,
                stale_after: "PT2H".into(),
                position_radius_km: 25.0,
                builtin_parameters: true,
                parameters: vec![],
            },
            "obs-wis2-test",
        )
        .unwrap()
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/bufr-synop/").to_string() + name,
        )
        .unwrap()
    }

    fn resolved(data_id: &str, bytes: Option<Vec<u8>>) -> Resolved {
        Resolved {
            notification: Notification {
                topic: "cache/a/wis2/se-smhi/data/core/weather/surface-based-observations/synop"
                    .into(),
                centre_id: Some("se-smhi".into()),
                id: format!("id-{data_id}"),
                data_id: data_id.into(),
                pubtime: Utc::now(),
                datetime: None,
                start_datetime: None,
                end_datetime: None,
                geometry: None,
                integrity: None,
                content: None,
                links: if bytes.is_none() {
                    vec![Link {
                        rel: "deletion".into(),
                        href: "https://x/y".into(),
                        media_type: None,
                        length: None,
                    }]
                } else {
                    vec![]
                },
                metadata_id: None,
                wigos_station_identifier: None,
                global_cache: None,
                extra: Default::default(),
            },
            payload: bytes.map(|b| Payload {
                bytes: b.into(),
                source: PayloadSource::Inline,
                media_type: None,
                verified: Some(true),
            }),
        }
    }

    #[test]
    fn constructor_is_offline_and_reports_connecting() {
        let e = engine();
        assert!(e.is_wis2());
        assert!(!e.is_loaded());
        assert!(e.wis2_status().is_none());
        assert_eq!(
            e.live_health(),
            Some(LiveStatus::Degraded {
                reason: "connecting to WIS2 broker"
            })
        );
        assert!(e.wis2_config().is_some());
        assert_eq!(e.get_locations().unwrap().len(), 0);
    }

    #[test]
    fn apply_ingests_and_deletion_withdraws_exactly_that_report() {
        let e = engine();
        let Source::Wis2(src) = e.source() else {
            panic!()
        };
        src.apply(
            &e,
            resolved(
                "se-smhi/a",
                Some(fixture("synop_se-smhi_20260912T0800Z.bufr")),
            ),
        );
        src.apply(
            &e,
            resolved(
                "za/b",
                Some(fixture("synop_za-weathersa_20260912T0800Z.bufr")),
            ),
        );
        assert!(e.is_loaded());
        e.snapshot_if_dirty();
        assert_eq!(e.get_locations().unwrap().len(), 2);
        assert_eq!(e.health.files_total.load(Ordering::Relaxed), 2);

        // A garbage payload is counted, not fatal.
        src.apply(&e, resolved("junk", Some(b"not bufr".to_vec())));
        assert_eq!(e.health.decode_failures_total.load(Ordering::Relaxed), 1);

        // Deleting the SMHI data_id removes only its row.
        src.apply(&e, resolved("se-smhi/a", None));
        e.snapshot_if_dirty();
        let ids: Vec<String> = e
            .get_locations()
            .unwrap()
            .into_iter()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids, vec!["0-20000-0-68155"]);
        // Unknown / repeated deletion is a no-op.
        src.apply(&e, resolved("se-smhi/a", None));
        src.apply(&e, resolved("never-seen", None));
        e.snapshot_if_dirty();
        assert_eq!(e.get_locations().unwrap().len(), 1);
    }

    #[test]
    fn live_status_state_machine() {
        let e = engine();
        let Source::Wis2(src) = e.source() else {
            panic!()
        };
        let status = Arc::new(Status::new());
        src.status.store(Arc::new(Some(status.clone())));
        // Connected but no SUBACK yet.
        status.set_connected();
        assert_eq!(
            src.live_status(&e.health),
            LiveStatus::Degraded {
                reason: "WIS2 subscription not acknowledged"
            }
        );
        status.set_subscribed();
        assert_eq!(
            src.live_status(&e.health),
            LiveStatus::Degraded {
                reason: "waiting for first WIS2 notification"
            }
        );
        // Notifications flowing but nothing decoded yet is not Ready.
        status.record_accepted(1);
        assert_eq!(
            src.live_status(&e.health),
            LiveStatus::Degraded {
                reason: "no BUFR reports decoded from WIS2 yet"
            }
        );
        e.health.mark_probed();
        assert_eq!(src.live_status(&e.health), LiveStatus::Ready);
        // A short disconnect keeps serving; a long one degrades.
        status.set_disconnected();
        assert_eq!(src.live_status(&e.health), LiveStatus::Ready);
        let long = Wis2Source::new(
            Wis2Config {
                degrade_after_secs: 1,
                ..src.config.clone()
            },
            chrono::Duration::hours(2),
        );
        long.status.store(Arc::new(Some(status.clone())));
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(
            long.live_status(&e.health),
            LiveStatus::Degraded {
                reason: "WIS2 broker disconnected"
            }
        );
        // Stale feed: reconnect, but the last message is older than stale_after.
        status.set_connected();
        status.set_subscribed();
        let stale = Wis2Source::new(src.config.clone(), chrono::Duration::seconds(0));
        stale.status.store(Arc::new(Some(status.clone())));
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(
            stale.live_status(&e.health),
            LiveStatus::Degraded {
                reason: "no WIS2 notification within stale_after"
            }
        );
    }
}
