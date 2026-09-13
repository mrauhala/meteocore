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
use ds_wis2::{Notification, Resolved, Status, StatusSnapshot};

use crate::engine::BufrEngine;
use crate::health::Health;

/// `data_id` → rows it produced, remembered for deletions. Bounded by
/// count (a global feed is ~10 messages/s; 200 k ≈ 5 h).
const MAX_REMEMBERED: usize = 200_000;
/// Back-off between attempts to (re)start the broker pipeline after it
/// failed to start or ended on its own.
const RESPAWN_DELAY: Duration = Duration::from_secs(30);

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

/// Which `(station, time)` rows each `data_id` is responsible for, so a
/// `rel=deletion` withdraws exactly those. The store keys rows by
/// `(station, time)` alone and a second `data_id` re-producing a key (an
/// overlapping bulletin, a correction re-issued under a new id) replaces
/// the row — so ownership is tracked per key and moves to the latest
/// producer: deleting the earlier, now-stale `data_id` leaves the live row
/// alone. Invariant: `owner[k] == d` ⇔ `k ∈ by_data_id[d]`.
#[derive(Default)]
struct Produced {
    by_data_id: HashMap<String, Vec<(String, DateTime<Utc>)>>,
    owner: HashMap<(String, DateTime<Utc>), String>,
    order: VecDeque<String>,
}

impl Produced {
    /// Record that `data_id` now holds `keys`, taking each key over from
    /// whichever `data_id` produced it before.
    fn record(&mut self, data_id: String, keys: Vec<(String, DateTime<Utc>)>) {
        for k in &keys {
            if let Some(prev) = self.owner.insert(k.clone(), data_id.clone()) {
                if prev != data_id {
                    if let Some(list) = self.by_data_id.get_mut(&prev) {
                        list.retain(|x| x != k);
                    }
                }
            }
        }
        match self.by_data_id.get_mut(&data_id) {
            Some(list) => {
                for k in keys {
                    if !list.contains(&k) {
                        list.push(k);
                    }
                }
            }
            None => {
                self.by_data_id.insert(data_id.clone(), keys);
                self.order.push_back(data_id);
                while self.order.len() > MAX_REMEMBERED {
                    if let Some(old) = self.order.pop_front() {
                        self.forget(&old);
                    }
                }
            }
        }
    }

    /// Drop `data_id`'s bookkeeping, returning the keys it still owns.
    fn forget(&mut self, data_id: &str) -> Vec<(String, DateTime<Utc>)> {
        let keys = self.by_data_id.remove(data_id).unwrap_or_default();
        for k in &keys {
            if self.owner.get(k).is_some_and(|d| d == data_id) {
                self.owner.remove(k);
            }
        }
        keys
    }
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
    /// `BufrEngine::poll_loop` (background runtime). If the pipeline cannot
    /// be started, or ends on its own (a non-transient subscriber error, a
    /// closed channel), it is respawned after [`RESPAWN_DELAY`] rather than
    /// leaving the collection frozen — an unchanged-config reload reuses
    /// this engine, so nothing else would restart it short of a process
    /// restart (the engine-cap `wis2_loop` pattern).
    pub async fn run(&self, engine: &BufrEngine, shutdown: &Shutdown) {
        let label = engine.collection_id().to_string();
        let mut snap = shutdown.ticker(BufrEngine::SNAPSHOT_INTERVAL, FirstTick::Skip);
        let mut prune = shutdown.ticker(BufrEngine::PRUNE_INTERVAL, FirstTick::Skip);
        'session: loop {
            let pipeline_shutdown = Arc::new(Shutdown::new());
            let mut pipeline =
                match ds_wis2::spawn_pipeline(&self.config, &label, pipeline_shutdown.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::error!(
                            "[{label}] bufr/wis2: cannot start subscription: {e} — retrying in {}s",
                            RESPAWN_DELAY.as_secs()
                        );
                        if !shutdown.sleep(RESPAWN_DELAY).await {
                            break 'session;
                        }
                        continue 'session;
                    }
                };
            self.status.store(Arc::new(Some(pipeline.status.clone())));
            loop {
                // `biased` with the tickers ahead of the message arm: a QoS-1
                // backlog replay (channel continuously ready) must not starve
                // pruning and snapshots.
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => {
                        pipeline_shutdown.shutdown();
                        break 'session;
                    }
                    _ = prune.tick() => engine.prune_now(),
                    _ = snap.tick() => engine.snapshot_if_dirty(),
                    r = pipeline.receiver.recv() => match r {
                        Some(r) => self.apply(engine, r),
                        None => {
                            tracing::warn!(
                                "[{label}] bufr/wis2: pipeline ended — restarting in {}s",
                                RESPAWN_DELAY.as_secs()
                            );
                            // Mark the session down so /health degrades while
                            // we wait, then respawn.
                            pipeline.status.set_disconnected();
                            pipeline_shutdown.shutdown();
                            if !shutdown.sleep(RESPAWN_DELAY).await {
                                break 'session;
                            }
                            continue 'session;
                        }
                    },
                }
            }
        }
        // The pipeline's own Shutdown is private to this loop so a reload can
        // never leave a subscriber behind.
    }

    fn apply(&self, engine: &BufrEngine, r: Resolved) {
        let n = r.notification;
        let Some(payload) = r.payload else {
            self.delete(engine, &n.data_id);
            return;
        };
        engine.health.files_total.fetch_add(1, Ordering::Relaxed);
        let outcome = match engine.ingest_bytes_keyed(&payload.bytes, &n.data_id, Utc::now()) {
            Ok(o) => o,
            Err(e) => {
                self.warn_once(engine, &n, &e);
                return;
            }
        };
        for e in &outcome.failed {
            self.warn_once(engine, &n, e);
        }
        // Ready only once this feed has produced a report the decoder
        // understood — a payload that failed (or a bulletin whose every
        // message failed) proves nothing about the pipeline.
        if outcome.decoded_reports > 0 {
            engine.health.mark_probed();
        }
        let keys = outcome.keys;
        if keys.is_empty() {
            return;
        }
        self.produced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(n.data_id, keys);
    }

    /// WARN once per (centre, failure kind) — a centre whose payloads hit a
    /// decoder gap (#693) would otherwise log every message; later ones are
    /// counted only (`bufr_decode_failures_total`).
    fn warn_once(&self, engine: &BufrEngine, n: &Notification, e: &crate::decode::DecodeError) {
        let centre = n.centre_id.clone().unwrap_or_default();
        let kind = match e {
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
    }

    fn delete(&self, engine: &BufrEngine, data_id: &str) {
        let keys = self
            .produced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .forget(data_id);
        if !keys.is_empty() {
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
    fn deletion_of_a_superseded_data_id_leaves_the_replacing_row_alone() {
        // Two data_ids produce the same (station, time): the second replaces
        // the row and takes ownership; deleting the first must not remove it.
        let e = engine();
        let Source::Wis2(src) = e.source() else {
            panic!()
        };
        let smhi = fixture("synop_se-smhi_20260912T0800Z.bufr");
        src.apply(&e, resolved("se-smhi/first", Some(smhi.clone())));
        src.apply(&e, resolved("se-smhi/second", Some(smhi.clone())));
        e.snapshot_if_dirty();
        assert_eq!(e.get_locations().unwrap().len(), 1);
        {
            let p = src.produced.lock().unwrap();
            assert!(p.by_data_id["se-smhi/first"].is_empty());
            assert_eq!(p.by_data_id["se-smhi/second"].len(), 1);
        }
        src.apply(&e, resolved("se-smhi/first", None));
        e.snapshot_if_dirty();
        assert_eq!(
            e.get_locations().unwrap().len(),
            1,
            "the live row belongs to 'second'"
        );
        src.apply(&e, resolved("se-smhi/second", None));
        e.snapshot_if_dirty();
        assert_eq!(e.get_locations().unwrap().len(), 0);
        assert!(src.produced.lock().unwrap().owner.is_empty());

        // The other order: deleting the owner removes the row; the stale
        // data_id's deletion is then a no-op.
        src.apply(&e, resolved("a", Some(smhi.clone())));
        src.apply(&e, resolved("b", Some(smhi)));
        src.apply(&e, resolved("b", None));
        e.snapshot_if_dirty();
        assert_eq!(e.get_locations().unwrap().len(), 0);
        src.apply(&e, resolved("a", None));
        assert!(src.produced.lock().unwrap().by_data_id.is_empty());
    }

    #[test]
    fn undecodable_payload_does_not_probe_and_warns_once_per_centre() {
        // A valid BUFR message whose first descriptor is the unassigned
        // 3-63-255: `decode()` yields one per-message failure, no reports.
        let mut bad = fixture("synop_se-smhi_20260912T0800Z.bufr");
        let s3 = 8 + u32::from_be_bytes([0, bad[8], bad[9], bad[10]]) as usize;
        bad[s3 + 7] = 0xFF;
        bad[s3 + 8] = 0xFF;
        let e = engine();
        let Source::Wis2(src) = e.source() else {
            panic!()
        };
        src.apply(&e, resolved("se-smhi/bad1", Some(bad.clone())));
        src.apply(&e, resolved("se-smhi/bad2", Some(bad)));
        src.apply(&e, resolved("se-smhi/junk", Some(b"not bufr".to_vec())));
        assert!(
            !e.is_loaded(),
            "failures alone must not mark the feed probed"
        );
        assert_eq!(e.health.files_total.load(Ordering::Relaxed), 3);
        assert_eq!(e.health.decode_failures_total.load(Ordering::Relaxed), 3);
        // Three failures of one kind ("error") from one centre: one warning.
        assert_eq!(src.warned.lock().unwrap().len(), 1);
        // One decoded report flips it.
        src.apply(
            &e,
            resolved(
                "se-smhi/good",
                Some(fixture("synop_se-smhi_20260912T0800Z.bufr")),
            ),
        );
        assert!(e.is_loaded());
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
