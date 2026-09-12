//! WIS2 push source: CAP alerts accumulated from Global Broker notifications.
//!
//! Unlike the directory and feed sources, which re-list everything on every
//! poll, WIS2 only ever tells us about *new* documents. The source therefore
//! keeps an in-memory accumulator of every alert seen, keyed by CAP
//! `<identifier>`, and hands the catalog a snapshot of it. Two things the
//! pull sources get for free have to be explicit here:
//!
//! - **eviction** — an alert leaves the accumulator once every info's
//!   validity end (`<expires>`, else onset + `default_ttl`, else receipt +
//!   [`FALLBACK_LIFETIME`]) is more than `retention_grace` in the past, or when
//!   a `rel=deletion` notification names its `data_id`; a hard `max_alerts`
//!   cap evicts the oldest-received first;
//! - **geometry** — MeteoAlarm's CAP documents are geocode-only, but its
//!   notifications carry a `rel=geometry` link to the exact zone polygon
//!   (one notification per alert × info × area, with 0-based `indexInfo` /
//!   `indexArea`). With `geometry_links` the polygon is fetched and attached
//!   to that area as a [`CapAreaHint`]; `bbox_fallback` uses the
//!   notification's own bbox for areas that still have none.
//!
//! Update / Cancel resolution is NOT done here — `supersede::resolve_references`
//! runs on every catalog rebuild for all source modes. The accumulator only
//! keeps the newest document per identifier and remembers tombstones so a
//! late duplicate cannot resurrect a deleted alert.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use ds_core::feature::Geometry;
use ds_wis2::{Fetcher, Notification, Payload, Resolved};

use crate::catalog::build_window;
use crate::parser::{parse_document, CapAlert, CapAreaHint};

/// Alerts with no computable validity end are kept this long after receipt.
pub const FALLBACK_LIFETIME: Duration = Duration::days(7);
/// Largest `rel=geometry` document fetched (a single zone polygon).
const MAX_GEOMETRY_BYTES: usize = 4 * 1024 * 1024;
/// Notification `geometry` is a bbox; the fetched polygon must fit inside it
/// (with this tolerance in degrees) or the hint is discarded — guards against
/// mis-keyed `indexInfo`/`indexArea` on the producer side.
const HINT_BBOX_TOLERANCE_DEG: f64 = 0.05;

#[derive(Debug, Clone)]
pub struct Wis2SourceConfig {
    pub retention_grace: Duration,
    pub max_alerts: usize,
    pub geometry_links: bool,
    pub bbox_fallback: bool,
    pub default_ttl: Option<Duration>,
}

#[derive(Debug, Clone)]
struct Entry {
    alert: CapAlert,
    received: DateTime<Utc>,
    pubtime: DateTime<Utc>,
    /// `data_id`s whose notifications contributed to this alert (a deletion
    /// of any of them withdraws the alert).
    data_ids: Vec<String>,
}

#[derive(Debug, Default)]
struct Accumulator {
    alerts: HashMap<String, Entry>,
    /// identifier → when it was deleted (a later duplicate must not resurrect it).
    tombstones: HashMap<String, DateTime<Utc>>,
    data_id_index: HashMap<String, String>,
}

/// Ingest-side counters surfaced through `/metrics`.
#[derive(Debug, Default)]
pub struct Wis2SourceStats {
    pub documents_ingested: AtomicU64,
    pub documents_rejected: AtomicU64,
    pub deletions: AtomicU64,
    pub hints_attached: AtomicU64,
    pub hints_rejected: AtomicU64,
    pub evicted: AtomicU64,
}

pub struct Wis2CapSource {
    cfg: Wis2SourceConfig,
    acc: Mutex<Accumulator>,
    dirty: AtomicBool,
    pub stats: Wis2SourceStats,
}

impl Wis2CapSource {
    pub fn new(cfg: Wis2SourceConfig) -> Self {
        Wis2CapSource {
            cfg,
            acc: Mutex::new(Accumulator::default()),
            dirty: AtomicBool::new(false),
            stats: Wis2SourceStats::default(),
        }
    }

    /// Whether anything changed since the last [`Self::take_dirty`].
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// Alerts currently held (before eviction).
    pub fn len(&self) -> usize {
        self.acc
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .alerts
            .len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Apply one resolved notification. `fetcher` is used for the optional
    /// `rel=geometry` hint download; `now` is injectable for tests.
    pub async fn apply(&self, r: Resolved, fetcher: &Fetcher, label: &str) {
        self.apply_at(r, fetcher, label, Utc::now()).await;
    }

    pub async fn apply_at(&self, r: Resolved, fetcher: &Fetcher, label: &str, now: DateTime<Utc>) {
        let hint = if self.cfg.geometry_links && r.payload.is_some() {
            self.fetch_hint(&r.notification, fetcher, label).await
        } else {
            None
        };
        self.apply_with_hint(r, hint, label, now);
    }

    /// [`Self::apply_at`] with the `rel=geometry` hint already resolved
    /// (`(indexInfo, indexArea, hint)`); the network-free core, also used by
    /// tests.
    pub fn apply_with_hint(
        &self,
        r: Resolved,
        hint: Option<(usize, usize, CapAreaHint)>,
        label: &str,
        now: DateTime<Utc>,
    ) {
        let n = r.notification;
        let Some(payload) = r.payload else {
            self.delete(&n.data_id, now);
            return;
        };
        let alerts = match parse_payload(&payload, &n.data_id) {
            Ok(a) => a,
            Err(e) => {
                self.stats
                    .documents_rejected
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!("[{label}] cap/wis2: {} rejected: {e}", n.data_id);
                return;
            }
        };
        // Which (info, area) the notification is about, when it says.
        let bbox_scope = match (n.extra_u64("indexInfo"), n.extra_u64("indexArea")) {
            (Some(i), Some(a)) => Some((i as usize, a as usize)),
            _ => None,
        };
        let bbox_hint = if self.cfg.bbox_fallback {
            n.geometry
                .as_ref()
                .and_then(|g| g.bbox())
                .map(|b| CapAreaHint {
                    geometry: Arc::new(bbox_polygon(b)),
                    source: "bbox",
                })
        } else {
            None
        };

        let mut acc = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        for mut alert in alerts {
            let identifier = alert.identifier.clone();
            if let Some(&deleted_at) = acc.tombstones.get(&alert.identifier) {
                // A document published before the deletion is stale; a newer
                // one (re-issue after a withdrawal) revives the identifier.
                if n.pubtime <= deleted_at {
                    continue;
                }
                acc.tombstones.remove(&alert.identifier);
            }
            // Attach geometry hints. The exact-polygon hint is keyed to one
            // (info, area); the bbox fallback applies to every area lacking
            // geometry.
            if let Some((info_idx, area_idx, h)) = &hint {
                if let Some(area) = alert
                    .infos
                    .get_mut(*info_idx)
                    .and_then(|i| i.areas.get_mut(*area_idx))
                {
                    area.hint_geometry = Some(h.clone());
                    self.stats.hints_attached.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.stats.hints_rejected.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(
                        "[{label}] cap/wis2: {} hint index ({info_idx},{area_idx}) out of range",
                        alert.identifier
                    );
                }
            }
            match acc.alerts.get_mut(&alert.identifier) {
                Some(existing) if existing.pubtime > n.pubtime => {
                    // Older revision arriving late: keep the newer document but
                    // still merge any per-area hints it carried.
                    merge_hints(&mut existing.alert, &alert);
                    if !existing.data_ids.contains(&n.data_id) {
                        existing.data_ids.push(n.data_id.clone());
                    }
                }
                Some(existing) => {
                    // Same or newer revision (MeteoAlarm sends one notification
                    // per area for the same XML): merge hints from the stored
                    // copy into the new one so earlier areas keep theirs.
                    merge_hints(&mut alert, &existing.alert);
                    let mut data_ids = std::mem::take(&mut existing.data_ids);
                    if !data_ids.contains(&n.data_id) {
                        data_ids.push(n.data_id.clone());
                    }
                    *existing = Entry {
                        alert,
                        received: existing.received,
                        pubtime: n.pubtime,
                        data_ids,
                    };
                }
                None => {
                    acc.alerts.insert(
                        alert.identifier.clone(),
                        Entry {
                            alert,
                            received: now,
                            pubtime: n.pubtime,
                            data_ids: vec![n.data_id.clone()],
                        },
                    );
                }
            }
            // Bbox fallback last, so it never shadows an exact polygon that
            // arrived on another notification for the same document — and
            // scoped to the one area the notification describes when it says
            // which (MeteoAlarm's indexInfo/indexArea); a notification for a
            // whole document may fill every geometry-less area.
            if let Some(b) = &bbox_hint {
                if let Some(entry) = acc.alerts.get_mut(&identifier) {
                    let mut targets: Vec<&mut crate::parser::CapArea> = match bbox_scope {
                        Some((i, a)) => entry
                            .alert
                            .infos
                            .get_mut(i)
                            .and_then(|info| info.areas.get_mut(a))
                            .into_iter()
                            .collect(),
                        None => entry
                            .alert
                            .infos
                            .iter_mut()
                            .flat_map(|info| info.areas.iter_mut())
                            .collect(),
                    };
                    for area in targets.iter_mut() {
                        if area.hint_geometry.is_none() {
                            area.hint_geometry = Some(b.clone());
                        }
                    }
                }
            }
            acc.data_id_index.insert(n.data_id.clone(), identifier);
            self.stats
                .documents_ingested
                .fetch_add(1, Ordering::Relaxed);
        }
        drop(acc);
        self.dirty.store(true, Ordering::Release);
    }

    fn delete(&self, data_id: &str, now: DateTime<Utc>) {
        let mut acc = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(identifier) = acc.data_id_index.remove(data_id) {
            if acc.alerts.remove(&identifier).is_some() {
                acc.tombstones.insert(identifier, now);
                self.stats.deletions.fetch_add(1, Ordering::Relaxed);
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    /// Fetch the `rel=geometry` polygon named by a MeteoAlarm-style
    /// notification and return it keyed to `(indexInfo, indexArea)`. The
    /// hub's links are pre-signed and expire about an hour after
    /// publication, so this runs on arrival, never on a later rebuild.
    async fn fetch_hint(
        &self,
        n: &Notification,
        fetcher: &Fetcher,
        label: &str,
    ) -> Option<(usize, usize, CapAreaHint)> {
        let (info_idx, area_idx, href) = hint_key(n)?;
        let bytes = match fetcher.download(href).await {
            Ok(b) => b,
            Err(e) => {
                self.stats.hints_rejected.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    "[{label}] cap/wis2: geometry link for {} failed: {e}",
                    n.data_id
                );
                return None;
            }
        };
        match hint_from_bytes(n, &bytes) {
            Ok(h) => Some((info_idx, area_idx, h)),
            Err(why) => {
                self.stats.hints_rejected.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "[{label}] cap/wis2: geometry hint for {} ignored: {why}",
                    n.data_id
                );
                None
            }
        }
    }

    /// The alerts currently in force, after eviction. Called by
    /// `Source::load` on every catalog rebuild.
    pub fn snapshot(&self, now: DateTime<Utc>) -> Vec<CapAlert> {
        let mut acc = self.acc.lock().unwrap_or_else(|e| e.into_inner());
        let grace = self.cfg.retention_grace;
        let default_ttl = self.cfg.default_ttl;
        let before = acc.alerts.len();
        acc.alerts.retain(|_, e| {
            let end = validity_end(&e.alert, default_ttl).unwrap_or(e.received + FALLBACK_LIFETIME);
            end + grace >= now
        });
        // Hard cap: oldest-received first.
        if acc.alerts.len() > self.cfg.max_alerts {
            let mut by_age: Vec<(DateTime<Utc>, String)> = acc
                .alerts
                .iter()
                .map(|(id, e)| (e.received, id.clone()))
                .collect();
            by_age.sort();
            let excess = acc.alerts.len() - self.cfg.max_alerts;
            for (_, id) in by_age.into_iter().take(excess) {
                acc.alerts.remove(&id);
            }
        }
        let evicted = before - acc.alerts.len();
        if evicted > 0 {
            self.stats
                .evicted
                .fetch_add(evicted as u64, Ordering::Relaxed);
            let live: std::collections::HashSet<String> = acc.alerts.keys().cloned().collect();
            acc.data_id_index.retain(|_, ident| live.contains(ident));
        }
        acc.tombstones.retain(|_, t| *t + grace >= now);
        acc.alerts.values().map(|e| e.alert.clone()).collect()
    }
}

/// The `(indexInfo, indexArea, href)` a MeteoAlarm-style notification keys
/// its `rel=geometry` link to; `None` when the notification carries no usable
/// hint.
pub fn hint_key(n: &Notification) -> Option<(usize, usize, &str)> {
    let link = n.link("geometry")?;
    if !link
        .media_type
        .as_deref()
        .map(|t| t.contains("json"))
        .unwrap_or(true)
    {
        return None;
    }
    let info_idx = n.extra_u64("indexInfo")? as usize;
    let area_idx = n.extra_u64("indexArea")? as usize;
    Some((info_idx, area_idx, link.href.as_str()))
}

/// Turn a downloaded `rel=geometry` document into a hint, rejecting anything
/// that is not a polygon or that lies outside the notification's own bbox
/// (a mis-keyed `indexInfo`/`indexArea` on the producer side would otherwise
/// paint the wrong zone).
pub fn hint_from_bytes(n: &Notification, bytes: &[u8]) -> Result<CapAreaHint, &'static str> {
    if bytes.len() > MAX_GEOMETRY_BYTES {
        return Err("geometry document over the size cap");
    }
    let geometry = parse_geometry_document(bytes).ok_or("not a GeoJSON polygon")?;
    if let (Some(nb), Some(gb)) = (n.geometry.as_ref().and_then(|g| g.bbox()), geometry.bbox()) {
        let t = HINT_BBOX_TOLERANCE_DEG;
        if gb[0] < nb[0] - t || gb[1] < nb[1] - t || gb[2] > nb[2] + t || gb[3] > nb[3] + t {
            return Err("polygon lies outside the notification bbox");
        }
    }
    Ok(CapAreaHint {
        geometry: Arc::new(geometry),
        source: "notification",
    })
}

/// Carry per-area hints from `from` into `into` where `into` has none or
/// only a bbox fallback (an exact `notification` polygon always wins).
fn merge_hints(into: &mut CapAlert, from: &CapAlert) {
    for (i, info) in into.infos.iter_mut().enumerate() {
        for (a, area) in info.areas.iter_mut().enumerate() {
            let replaceable = match &area.hint_geometry {
                None => true,
                Some(h) => h.source == "bbox",
            };
            if !replaceable {
                continue;
            }
            if let Some(h) = from
                .infos
                .get(i)
                .and_then(|fi| fi.areas.get(a))
                .and_then(|fa| fa.hint_geometry.clone())
            {
                if area.hint_geometry.is_none() || h.source != "bbox" {
                    area.hint_geometry = Some(h);
                }
            }
        }
    }
}

/// Latest validity end over an alert's infos (`None` when no info has one).
fn validity_end(alert: &CapAlert, default_ttl: Option<Duration>) -> Option<DateTime<Utc>> {
    alert
        .infos
        .iter()
        .filter_map(|info| build_window(alert, info, default_ttl).end)
        .max()
}

fn parse_payload(payload: &Payload, data_id: &str) -> Result<Vec<CapAlert>, String> {
    let xml = match std::str::from_utf8(&payload.bytes) {
        Ok(s) => std::borrow::Cow::Borrowed(s),
        Err(e) => {
            tracing::warn!("cap/wis2: '{data_id}' is not valid UTF-8 ({e}) — parsing lossily");
            String::from_utf8_lossy(&payload.bytes)
        }
    };
    let alerts = parse_document(&xml).map_err(|e| e.to_string())?;
    if alerts.is_empty() {
        return Err("no <alert> element".into());
    }
    Ok(alerts)
}

/// Parse a GeoJSON `Feature` / bare geometry / one-feature `FeatureCollection`
/// into a polygonal [`Geometry`].
fn parse_geometry_document(bytes: &[u8]) -> Option<Geometry> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let g = match v.get("type").and_then(|t| t.as_str())? {
        "Feature" => v.get("geometry")?,
        "FeatureCollection" => v.get("features")?.as_array()?.first()?.get("geometry")?,
        _ => &v,
    };
    let geom = crate::geocode::parse_geometry(g)?;
    matches!(
        geom,
        Geometry::Polygon { .. } | Geometry::MultiPolygon { .. }
    )
    .then_some(geom)
}

fn bbox_polygon(b: [f64; 4]) -> Geometry {
    Geometry::Polygon {
        exterior: vec![
            [b[0], b[1]],
            [b[2], b[1]],
            [b[2], b[3]],
            [b[0], b[3]],
            [b[0], b[1]],
        ],
        holes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ds_wis2::{DownloadPolicy, PayloadSource, Status};

    fn fetcher() -> Fetcher {
        Fetcher::new(
            DownloadPolicy::new(vec![]),
            1 << 20,
            Arc::new(Status::new()),
        )
        .unwrap()
    }

    fn cfg() -> Wis2SourceConfig {
        Wis2SourceConfig {
            retention_grace: Duration::hours(1),
            max_alerts: 100,
            geometry_links: false,
            bbox_fallback: false,
            default_ttl: None,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_789_200_000 + secs, 0).unwrap()
    }

    fn cap_xml(identifier: &str, msg_type: &str, refs: &str, expires: &str) -> String {
        let refs = if refs.is_empty() {
            String::new()
        } else {
            format!("<references>{refs}</references>")
        };
        format!(
            r#"<?xml version="1.0"?><alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
<identifier>{identifier}</identifier><sender>t@x</sender><sent>2026-09-12T10:00:00+00:00</sent>
<status>Actual</status><msgType>{msg_type}</msgType><scope>Public</scope>{refs}
<info><language>en-GB</language><category>Met</category><event>Rain</event><urgency>Immediate</urgency>
<severity>Moderate</severity><certainty>Likely</certainty><expires>{expires}</expires>
<area><areaDesc>Zone</areaDesc><geocode><valueName>NUTS3</valueName><value>MK006</value></geocode></area></info></alert>"#
        )
    }

    fn resolved(data_id: &str, pub_secs: i64, xml: Option<String>) -> Resolved {
        let n = Notification {
            topic: "cache/a/wis2/eu-eumetnet-warnings/data/core/weather/advisories-warnings".into(),
            centre_id: Some("eu-eumetnet-warnings".into()),
            id: format!("id-{data_id}-{pub_secs}"),
            data_id: data_id.into(),
            pubtime: at(pub_secs),
            datetime: None,
            start_datetime: None,
            end_datetime: None,
            geometry: None,
            integrity: None,
            content: None,
            links: if xml.is_none() {
                vec![ds_wis2::Link {
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
        };
        Resolved {
            notification: n,
            payload: xml.map(|x| Payload {
                bytes: x.into_bytes().into(),
                source: PayloadSource::Inline,
                media_type: None,
                verified: None,
            }),
        }
    }

    #[tokio::test]
    async fn accumulates_newest_per_identifier_and_evicts_after_expiry() {
        let src = Wis2CapSource::new(cfg());
        let f = fetcher();
        let far = "2026-09-13T00:00:00+00:00";
        src.apply_at(
            resolved("d1", 0, Some(cap_xml("A", "Alert", "", far))),
            &f,
            "t",
            at(0),
        )
        .await;
        src.apply_at(
            resolved(
                "d2",
                5,
                Some(cap_xml("B", "Alert", "", "2026-09-12T11:00:00+00:00")),
            ),
            &f,
            "t",
            at(5),
        )
        .await;
        assert!(src.take_dirty());
        assert!(!src.take_dirty());
        assert_eq!(src.len(), 2);

        // At 10:30Z both are live; at 12:30Z B is past expiry + 1 h grace.
        let t1030 = Utc.with_ymd_and_hms(2026, 9, 12, 10, 30, 0).unwrap();
        let t1230 = Utc.with_ymd_and_hms(2026, 9, 12, 12, 30, 0).unwrap();
        assert_eq!(src.snapshot(t1030).len(), 2);
        let snap = src.snapshot(t1230);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].identifier, "A");
        assert_eq!(src.stats.evicted.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn deletion_tombstones_and_stale_duplicate_cannot_resurrect() {
        let src = Wis2CapSource::new(cfg());
        let f = fetcher();
        let far = "2026-09-13T00:00:00+00:00";
        src.apply_at(
            resolved("d1", 0, Some(cap_xml("A", "Alert", "", far))),
            &f,
            "t",
            at(0),
        )
        .await;
        src.apply_at(resolved("d1", 10, None), &f, "t", at(10))
            .await;
        assert_eq!(src.len(), 0);
        assert_eq!(src.stats.deletions.load(Ordering::Relaxed), 1);
        // A copy from another Global Cache with the pre-deletion pubtime.
        src.apply_at(
            resolved("d1", 0, Some(cap_xml("A", "Alert", "", far))),
            &f,
            "t",
            at(11),
        )
        .await;
        assert_eq!(src.len(), 0);
        // A genuinely newer re-issue revives it.
        src.apply_at(
            resolved("d9", 20, Some(cap_xml("A", "Alert", "", far))),
            &f,
            "t",
            at(20),
        )
        .await;
        assert_eq!(src.len(), 1);
    }

    #[tokio::test]
    async fn max_alerts_evicts_oldest_received() {
        let mut c = cfg();
        c.max_alerts = 2;
        let src = Wis2CapSource::new(c);
        let f = fetcher();
        let far = "2026-09-13T00:00:00+00:00";
        for (i, id) in ["A", "B", "C"].iter().enumerate() {
            src.apply_at(
                resolved(
                    &format!("d{i}"),
                    i as i64,
                    Some(cap_xml(id, "Alert", "", far)),
                ),
                &f,
                "t",
                at(i as i64),
            )
            .await;
        }
        let mut ids: Vec<String> = src
            .snapshot(at(100))
            .into_iter()
            .map(|a| a.identifier)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["B", "C"]);
    }

    #[tokio::test]
    async fn bad_document_is_rejected_and_counted() {
        let src = Wis2CapSource::new(cfg());
        let f = fetcher();
        src.apply_at(resolved("d1", 0, Some("<not-cap/>".into())), &f, "t", at(0))
            .await;
        assert_eq!(src.len(), 0);
        assert_eq!(src.stats.documents_rejected.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn bbox_fallback_attaches_polygon_to_geocode_only_area() {
        let mut c = cfg();
        c.bbox_fallback = true;
        let src = Wis2CapSource::new(c);
        let f = fetcher();
        let mut r = resolved(
            "d1",
            0,
            Some(cap_xml("A", "Alert", "", "2026-09-13T00:00:00+00:00")),
        );
        r.notification.geometry = Some(ds_wis2::Geometry::Polygon(vec![
            [20.5, 41.4],
            [20.5, 42.2],
            [21.2, 42.2],
            [21.2, 41.4],
            [20.5, 41.4],
        ]));
        src.apply_at(r, &f, "t", at(0)).await;
        let snap = src.snapshot(at(1));
        let hint = snap[0].infos[0].areas[0].hint_geometry.as_ref().unwrap();
        assert_eq!(hint.source, "bbox");
        assert_eq!(hint.geometry.bbox(), Some([20.5, 41.4, 21.2, 42.2]));
    }

    #[test]
    fn geometry_document_shapes() {
        let feature = br#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}}"#;
        assert!(matches!(
            parse_geometry_document(feature),
            Some(Geometry::Polygon { .. })
        ));
        let bare = br#"{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]]]}"#;
        assert!(matches!(
            parse_geometry_document(bare),
            Some(Geometry::MultiPolygon { .. })
        ));
        let point = br#"{"type":"Feature","geometry":{"type":"Point","coordinates":[1,2]}}"#;
        assert!(parse_geometry_document(point).is_none());
        assert!(parse_geometry_document(b"nope").is_none());
    }
}
