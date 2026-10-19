//! The WIS2 accumulator snapshot format (#1000).
//!
//! A WIS2 CAP collection holds its alert set only in the accumulator, and
//! nothing replays it after a restart: the MQTT client id is random per
//! process and the broker session expires, so standing warnings return only
//! when the hub republishes them — and their pre-signed `rel=geometry` links
//! expire about an hour after publication, so a late replay could not
//! restore the polygons anyway. The engine therefore snapshots the
//! accumulator into the server's `ds_core::state::StateStore` under the key
//! `<collection id>.cap` ([`KIND`]; the store, the atomic replace and the
//! write policy are `ds_core::state`'s) and restores it when the collection
//! is built. This module is only the bytes: one whole blob per snapshot.
//!
//! The format is JSON: on a live deployment 2,460 alert areas were about
//! 3 MB of Features JSON, so even the observed peak of about 8,000 areas
//! (every `<info>` language held, not just the served one) stays in the
//! tens of MB, written at most every five minutes. A compact binary format would
//! need a new dependency for no operational gain. The one thing worth
//! compacting is geometry: MeteoAlarm sends the same zone polygon once per
//! `<info>` language, so the hint polygons go into one `geometries` table,
//! each distinct polygon once, and areas refer to it by index — which also
//! lets the restored areas share one allocation per polygon.
//!
//! Alerts are stored through the parser types' serde derives (see
//! `CapAlert`); a change there that is not additive bumps [`VERSION`]. A
//! snapshot whose `format`, `version` or `collection` does not match, or
//! that is malformed in any way, is rejected whole: the engine logs it and
//! starts cold. Output is sorted, so the same state always encodes to the
//! same bytes. `written_at` is when the blob was encoded: the engine
//! rewrites an unchanged state now and then and flushes at shutdown, so it
//! also says how long the server was down when it is restored.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use ds_core::feature::Geometry;
use ds_core::health::WarmupCause;
use serde::{Deserialize, Serialize};

use crate::catalog::geometry_fingerprint;
use crate::parser::{CapAlert, CapAreaHint, HintPart};
use crate::supersede::{AlertKey, MessageKey};
use crate::wis2::{AccumulatorState, Entry, Tombstone};

/// The store key's kind: `<collection id>.cap` (`ds_core::state::collection_key`).
pub(crate) const KIND: &str = "cap";
const FORMAT: &str = "meteocore/cap-wis2-accumulator";
/// Bump on any non-additive change to this file's types or the parser
/// structs they embed.
pub(crate) const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    format: String,
    version: u32,
    collection: String,
    written_at: DateTime<Utc>,
    filling_since: Option<DateTime<Utc>>,
    #[serde(default)]
    warmup_cause: CauseRepr,
    geometries: Vec<GeometryRepr>,
    alerts: Vec<EntryRepr>,
    tombstones: Vec<(MessageKey, Tombstone)>,
    data_tombstones: Vec<(String, Tombstone)>,
}

/// Read first, so a snapshot from another version or collection is
/// reported as that rather than as whatever field fails to parse.
#[derive(Deserialize)]
struct Header {
    format: String,
    version: u32,
    collection: String,
}

#[derive(Serialize, Deserialize)]
struct EntryRepr {
    alert: CapAlert,
    received: DateTime<Utc>,
    pubtime: DateTime<Utc>,
    data_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hints: Vec<HintRepr>,
}

/// One area's `hint_geometry`: `(info, area)` position in the alert, its
/// provenance, and its parts as `(part key, index into geometries)`.
#[derive(Serialize, Deserialize)]
struct HintRepr {
    info: usize,
    area: usize,
    source: String,
    parts: Vec<(PartRepr, usize)>,
}

/// [`WarmupCause`], kept apart so the health type is not the file format.
#[derive(Serialize, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
enum CauseRepr {
    #[default]
    ColdStart,
    LongOutage,
}

impl From<WarmupCause> for CauseRepr {
    fn from(c: WarmupCause) -> Self {
        match c {
            WarmupCause::ColdStart => CauseRepr::ColdStart,
            WarmupCause::LongOutage => CauseRepr::LongOutage,
        }
    }
}

impl From<CauseRepr> for WarmupCause {
    fn from(c: CauseRepr) -> Self {
        match c {
            CauseRepr::ColdStart => WarmupCause::ColdStart,
            CauseRepr::LongOutage => WarmupCause::LongOutage,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy)]
enum PartRepr {
    Feature(u64),
    Content(u64),
}

#[derive(Serialize, Deserialize, PartialEq)]
#[allow(clippy::type_complexity)]
enum GeometryRepr {
    Point {
        x: f64,
        y: f64,
    },
    Polygon {
        exterior: Vec<[f64; 2]>,
        holes: Vec<Vec<[f64; 2]>>,
    },
    MultiPolygon {
        polygons: Vec<(Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>)>,
    },
    Null,
}

impl From<&Geometry> for GeometryRepr {
    fn from(g: &Geometry) -> Self {
        match g {
            Geometry::Point { x, y } => GeometryRepr::Point { x: *x, y: *y },
            Geometry::Polygon { exterior, holes } => GeometryRepr::Polygon {
                exterior: exterior.clone(),
                holes: holes.clone(),
            },
            Geometry::MultiPolygon { polygons } => GeometryRepr::MultiPolygon {
                polygons: polygons.clone(),
            },
            Geometry::Null => GeometryRepr::Null,
        }
    }
}

impl From<GeometryRepr> for Geometry {
    fn from(g: GeometryRepr) -> Self {
        match g {
            GeometryRepr::Point { x, y } => Geometry::Point { x, y },
            GeometryRepr::Polygon { exterior, holes } => Geometry::Polygon { exterior, holes },
            GeometryRepr::MultiPolygon { polygons } => Geometry::MultiPolygon { polygons },
            GeometryRepr::Null => Geometry::Null,
        }
    }
}

/// Each distinct hint polygon once: by allocation first (parts shared
/// between revisions), then by content (the same zone downloaded once per
/// `<info>` language).
#[derive(Default)]
struct GeometryTable {
    reprs: Vec<GeometryRepr>,
    by_ptr: HashMap<*const Geometry, usize>,
    by_content: HashMap<u64, Vec<usize>>,
}

impl GeometryTable {
    fn intern(&mut self, g: &Arc<Geometry>) -> usize {
        let ptr = Arc::as_ptr(g);
        if let Some(&i) = self.by_ptr.get(&ptr) {
            return i;
        }
        let repr = GeometryRepr::from(g.as_ref());
        let same = self.by_content.entry(geometry_fingerprint(g)).or_default();
        let i = match same.iter().copied().find(|&i| self.reprs[i] == repr) {
            Some(i) => i,
            None => {
                self.reprs.push(repr);
                same.push(self.reprs.len() - 1);
                self.reprs.len() - 1
            }
        };
        self.by_ptr.insert(ptr, i);
        i
    }
}

/// `state` as one snapshot blob.
pub(crate) fn encode(
    collection_id: &str,
    state: AccumulatorState,
    written_at: DateTime<Utc>,
) -> Result<Vec<u8>, serde_json::Error> {
    let AccumulatorState {
        mut entries,
        mut tombstones,
        mut data_tombstones,
        filling_since,
        warmup_cause,
    } = state;
    entries.sort_by_cached_key(|e| AlertKey::of(&e.alert));
    tombstones.sort_by(|a, b| a.0.cmp(&b.0));
    data_tombstones.sort_by(|a, b| a.0.cmp(&b.0));
    let mut table = GeometryTable::default();
    let alerts = entries
        .into_iter()
        .map(|e| {
            let mut hints = Vec::new();
            for (i, info) in e.alert.infos.iter().enumerate() {
                for (a, area) in info.areas.iter().enumerate() {
                    // Serialized here, skipped by the alert's own derive.
                    if let Some(h) = &area.hint_geometry {
                        hints.push(HintRepr {
                            info: i,
                            area: a,
                            source: h.source.to_string(),
                            parts: h
                                .parts
                                .iter()
                                .map(|(k, g)| {
                                    let key = match *k {
                                        HintPart::Feature(f) => PartRepr::Feature(f),
                                        HintPart::Content(c) => PartRepr::Content(c),
                                    };
                                    (key, table.intern(g))
                                })
                                .collect(),
                        });
                    }
                }
            }
            EntryRepr {
                alert: e.alert,
                received: e.received,
                pubtime: e.pubtime,
                data_id: e.current_data_id,
                hints,
            }
        })
        .collect();
    let snapshot = Snapshot {
        format: FORMAT.to_string(),
        version: VERSION,
        collection: collection_id.to_string(),
        written_at,
        filling_since,
        warmup_cause: warmup_cause.into(),
        geometries: table.reprs,
        alerts,
        tombstones,
        data_tombstones,
    };
    serde_json::to_vec(&snapshot)
}

/// A decoded snapshot: the accumulator state and when it was written.
pub(crate) struct Decoded {
    pub(crate) state: AccumulatorState,
    pub(crate) written_at: DateTime<Utc>,
}

/// Parse and check a snapshot written for `collection_id`. Any problem
/// rejects the whole snapshot: half a warning set restored as if it were
/// complete would be worse than a cold start, which at least says so.
pub(crate) fn decode(bytes: &[u8], collection_id: &str) -> Result<Decoded, String> {
    let header: Header =
        serde_json::from_slice(bytes).map_err(|e| format!("not a snapshot: {e}"))?;
    if header.format != FORMAT {
        return Err(format!("unknown format '{}'", header.format));
    }
    if header.version != VERSION {
        return Err(format!(
            "snapshot version {} (this build reads {VERSION})",
            header.version
        ));
    }
    if header.collection != collection_id {
        return Err(format!(
            "snapshot of collection '{}', not '{collection_id}'",
            header.collection
        ));
    }
    let snapshot: Snapshot = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let geometries: Vec<Arc<Geometry>> = snapshot
        .geometries
        .into_iter()
        .map(|g| Arc::new(Geometry::from(g)))
        .collect();
    let mut entries = Vec::with_capacity(snapshot.alerts.len());
    for repr in snapshot.alerts {
        let mut alert = repr.alert;
        for hint in repr.hints {
            let source = match hint.source.as_str() {
                "notification" => "notification",
                "bbox" => "bbox",
                other => {
                    return Err(format!(
                        "alert '{}': unknown hint source '{other}'",
                        alert.identifier
                    ))
                }
            };
            let mut parts = std::collections::BTreeMap::new();
            for (key, index) in hint.parts {
                let geometry = geometries.get(index).ok_or_else(|| {
                    format!(
                        "alert '{}': geometry index {index} out of range",
                        alert.identifier
                    )
                })?;
                let key = match key {
                    PartRepr::Feature(f) => HintPart::Feature(f),
                    PartRepr::Content(c) => HintPart::Content(c),
                };
                parts.insert(key, Arc::clone(geometry));
            }
            if parts.is_empty() {
                return Err(format!("alert '{}': hint with no parts", alert.identifier));
            }
            let identifier = alert.identifier.clone();
            let area = alert
                .infos
                .get_mut(hint.info)
                .and_then(|info| info.areas.get_mut(hint.area))
                .ok_or_else(|| {
                    format!(
                        "alert '{identifier}': hint for info {} area {} out of range",
                        hint.info, hint.area
                    )
                })?;
            area.hint_geometry = Some(CapAreaHint { parts, source });
        }
        entries.push(Entry {
            alert,
            received: repr.received,
            pubtime: repr.pubtime,
            current_data_id: repr.data_id,
        });
    }
    Ok(Decoded {
        state: AccumulatorState {
            entries,
            tombstones: snapshot.tombstones,
            data_tombstones: snapshot.data_tombstones,
            filling_since: snapshot.filling_since,
            warmup_cause: snapshot.warmup_cause.into(),
        },
        written_at: snapshot.written_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wis2::test_support::{at, resolved};
    use crate::wis2::{Wis2CapSource, Wis2SourceConfig};
    use chrono::{Duration, TimeZone};

    fn cfg(bbox_fallback: bool) -> Wis2SourceConfig {
        Wis2SourceConfig {
            label: "test".into(),
            status_filter: vec!["Actual".into()],
            retention_grace: Duration::hours(1),
            max_alerts: 100,
            geometry_links: true,
            bbox_fallback,
            default_ttl: None,
        }
    }

    /// Two `<info>` languages, each with one geocode-only area: MeteoAlarm's
    /// shape, where the same zone polygon arrives once per info.
    fn cap_xml(identifier: &str, msg_type: &str, refs: &str, expires: &str) -> String {
        let refs = if refs.is_empty() {
            String::new()
        } else {
            format!("<references>{refs}</references>")
        };
        let info = |lang: &str| {
            format!(
                "<info><language>{lang}</language><category>Met</category><event>Rain &amp; \
                 wind</event><urgency>Immediate</urgency><severity>Severe</severity>\
                 <certainty>Likely</certainty><expires>{expires}</expires>\
                 <parameter><valueName>awareness_level</valueName><value>3; orange; \
                 Severe</value></parameter><area><areaDesc>Zone</areaDesc><geocode>\
                 <valueName>EMMA_ID</valueName><value>FI810</value></geocode></area></info>"
            )
        };
        format!(
            r#"<?xml version="1.0"?><alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
<identifier>{identifier}</identifier><sender>t@x</sender><sent>2026-09-12T08:00:00+00:00</sent>
<status>Actual</status><msgType>{msg_type}</msgType><scope>Public</scope>{refs}{}{}</alert>"#,
            info("en-GB"),
            info("fi-FI")
        )
    }

    fn zone(west: f64) -> Geometry {
        Geometry::Polygon {
            exterior: vec![
                [west, 63.5],
                [west + 0.7712, 63.5],
                [west + 0.7712, 63.8],
                [west, 63.5],
            ],
            holes: Vec::new(),
        }
    }

    const FAR: &str = "2026-09-20T00:00:00+00:00";

    /// An accumulator with hints (exact, several parts, the same polygon on
    /// both languages; and a bbox), a deletion tombstone, a Cancel
    /// tombstone and a running warm-up clock.
    fn populated(bbox_fallback: bool) -> Wis2CapSource {
        let src = Wis2CapSource::new(cfg(bbox_fallback));
        for (info, feature, west) in [(0, 0, 22.0), (0, 1, 23.0), (1, 0, 22.0), (1, 1, 23.0)] {
            let hint = CapAreaHint::single(HintPart::Feature(feature), zone(west), "notification");
            src.apply_with_hint(
                resolved("d-a", 0, Some(cap_xml("A", "Alert", "", FAR))),
                Some((info, 0, hint)),
                "t",
                at(0),
            );
        }
        let bbox = CapAreaHint::single(HintPart::Content(7), zone(30.0), "bbox");
        src.apply_with_hint(
            resolved("d-b", 5, Some(cap_xml("B", "Alert", "", FAR))),
            Some((0, 0, bbox)),
            "t",
            at(5),
        );
        src.apply_with_hint(
            resolved("d-c", 6, Some(cap_xml("C", "Alert", "", FAR))),
            None,
            "t",
            at(6),
        );
        src.apply_with_hint(
            resolved(
                "d-x",
                7,
                Some(cap_xml(
                    "X",
                    "Cancel",
                    "t@x,C,2026-09-12T08:00:00+00:00",
                    FAR,
                )),
            ),
            None,
            "t",
            at(7),
        );
        src.apply_with_hint(resolved("d-gone", 8, None), None, "t", at(8));
        src.mark_filling(at(1));
        src
    }

    fn encoded(src: &Wis2CapSource, written_at: DateTime<Utc>) -> Vec<u8> {
        encode("coll", src.export(), written_at).unwrap()
    }

    #[test]
    fn round_trip_restores_the_accumulator_exactly() {
        let src = populated(true);
        assert_eq!(src.len(), 2, "A and B held; C cancelled");
        let bytes = encoded(&src, at(10));
        let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // The same zone on two languages is one table entry: 2 zones + bbox.
        assert_eq!(snapshot["geometries"].as_array().unwrap().len(), 3);
        assert_eq!(snapshot["tombstones"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["data_tombstones"].as_array().unwrap().len(), 1);

        let decoded = decode(&bytes, "coll").unwrap();
        assert_eq!(decoded.written_at, at(10));
        let restored = Wis2CapSource::new(cfg(true));
        let summary = restored.restore(decoded.state, at(20));
        assert_eq!(
            summary,
            crate::wis2::RestoreSummary {
                alerts: 2,
                tombstones: 2,
                ..Default::default()
            }
        );
        assert_eq!(restored.len(), 2);
        assert_eq!(restored.filling_since(), Some(at(1)));
        assert_eq!(restored.warmup_cause(), WarmupCause::ColdStart);
        restored.assert_index_consistent();
        // The warm-up cause travels with the clock.
        let outage = AccumulatorState {
            warmup_cause: WarmupCause::LongOutage,
            ..src.export()
        };
        let decoded = decode(&encode("coll", outage, at(10)).unwrap(), "coll").unwrap();
        assert_eq!(decoded.state.warmup_cause, WarmupCause::LongOutage);
        // Same state ⇒ same bytes (the encoding is sorted).
        assert_eq!(encoded(&restored, at(10)), bytes);

        // Restored geometry: both languages share one allocation per zone,
        // parts keep their keys, and the bbox hint keeps its provenance.
        let alerts = restored.snapshot(at(20));
        let a = alerts.iter().find(|a| a.identifier == "A").unwrap();
        let en = a.infos[0].areas[0].hint_geometry.as_ref().unwrap();
        let fi = a.infos[1].areas[0].hint_geometry.as_ref().unwrap();
        assert_eq!(en.source, "notification");
        assert_eq!(
            en.parts.keys().copied().collect::<Vec<_>>(),
            vec![HintPart::Feature(0), HintPart::Feature(1)]
        );
        for (p, q) in en.parts.values().zip(fi.parts.values()) {
            assert!(Arc::ptr_eq(p, q));
        }
        let b = alerts.iter().find(|a| a.identifier == "B").unwrap();
        assert_eq!(
            b.infos[0].areas[0].hint_geometry.as_ref().unwrap().source,
            "bbox"
        );
        assert_eq!(a.infos[0].event.as_deref(), Some("Rain & wind"));
        assert_eq!(
            a.infos[0].parameters,
            vec![(
                "awareness_level".to_string(),
                "3; orange; Severe".to_string()
            )]
        );

        // The restored tombstones still work: the cancelled C and the
        // deleted data_id cannot come back as a stale copy.
        restored.apply_with_hint(
            resolved("d-c", 6, Some(cap_xml("C", "Alert", "", FAR))),
            None,
            "t",
            at(30),
        );
        restored.apply_with_hint(
            resolved("d-gone", 2, Some(cap_xml("G", "Alert", "", FAR))),
            None,
            "t",
            at(30),
        );
        assert_eq!(restored.len(), 2);
        // A deletion naming A's current data_id still withdraws it.
        restored.apply_with_hint(resolved("d-a", 40, None), None, "t", at(40));
        assert_eq!(restored.len(), 1);
        restored.assert_index_consistent();
    }

    #[test]
    fn restore_drops_what_expired_while_down_and_old_tombstones() {
        let src = populated(true);
        let short = Wis2CapSource::new(cfg(true));
        short.apply_with_hint(
            resolved(
                "d-s",
                0,
                Some(cap_xml("S", "Alert", "", "2026-09-12T09:00:00+00:00")),
            ),
            None,
            "t",
            at(0),
        );
        let mut state = src.export();
        state.entries.extend(short.export().entries);
        let bytes = encode("coll", state, at(10)).unwrap();

        // Tombstones live an hour past receipt (07:40Z): both still there
        // at 08:40Z, gone at 09:59Z.
        let restored = Wis2CapSource::new(cfg(true));
        let s = restored.restore(decode(&bytes, "coll").unwrap().state, at(3600));
        assert_eq!((s.alerts, s.expired, s.tombstones), (3, 0, 2));
        // S's 09:00Z expiry + 1 h grace: still held at 09:59Z, gone at 10:01Z.
        let restored = Wis2CapSource::new(cfg(true));
        let t0959 = Utc.with_ymd_and_hms(2026, 9, 12, 9, 59, 0).unwrap();
        let s = restored.restore(decode(&bytes, "coll").unwrap().state, t0959);
        assert_eq!((s.alerts, s.expired, s.tombstones), (3, 0, 0));
        let restored = Wis2CapSource::new(cfg(true));
        let t1001 = Utc.with_ymd_and_hms(2026, 9, 12, 10, 1, 0).unwrap();
        let s = restored.restore(decode(&bytes, "coll").unwrap().state, t1001);
        assert_eq!((s.alerts, s.expired), (2, 1));
        assert!(restored.snapshot(t1001).iter().all(|a| a.identifier != "S"));
        restored.assert_index_consistent();
        // Days later: alerts past their expiry and every tombstone are gone.
        let restored = Wis2CapSource::new(cfg(true));
        let s = restored.restore(
            decode(&bytes, "coll").unwrap().state,
            Utc.with_ymd_and_hms(2026, 9, 25, 0, 0, 0).unwrap(),
        );
        assert_eq!((s.alerts, s.expired, s.tombstones), (0, 3, 0));
        assert_eq!(restored.len(), 0);
    }

    #[test]
    fn restore_applies_the_current_config() {
        let bytes = encoded(&populated(true), at(10));
        // bbox_fallback switched off since the snapshot: B's bbox goes.
        let restored = Wis2CapSource::new(cfg(false));
        let s = restored.restore(decode(&bytes, "coll").unwrap().state, at(20));
        assert_eq!((s.alerts, s.hints_dropped), (2, 1));
        let b = restored.snapshot(at(20));
        let b = b.iter().find(|a| a.identifier == "B").unwrap();
        assert!(b.infos[0].areas[0].hint_geometry.is_none());
        // A status_filter that no longer accepts Actual drops everything.
        let restored = Wis2CapSource::new(Wis2SourceConfig {
            status_filter: vec!["Exercise".into()],
            ..cfg(true)
        });
        let s = restored.restore(decode(&bytes, "coll").unwrap().state, at(20));
        assert_eq!((s.alerts, s.filtered), (0, 2));
    }

    /// Full-precision coordinates (17 significant digits, as a reprojected
    /// or computed outline carries) read back bit for bit: serde_json
    /// guarantees that only with its `float_roundtrip` feature, which this
    /// crate enables. An inexact read would change `data_version` on every
    /// restore.
    #[test]
    fn full_precision_coordinates_round_trip_bit_exactly() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = |scale: f64, offset: f64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1u64 << 53) as f64 * scale + offset
        };
        let mut exterior: Vec<[f64; 2]> = (0..5000)
            .map(|_| [next(360.0, -180.0), next(180.0, -90.0)])
            .collect();
        exterior.push(exterior[0]);
        let ring = Geometry::Polygon {
            exterior: exterior.clone(),
            holes: Vec::new(),
        };
        let src = Wis2CapSource::new(cfg(true));
        src.apply_with_hint(
            resolved("d-p", 0, Some(cap_xml("P", "Alert", "", FAR))),
            Some((
                0,
                0,
                CapAreaHint::single(HintPart::Feature(0), ring, "notification"),
            )),
            "t",
            at(0),
        );
        let decoded = decode(&encoded(&src, at(1)), "coll").unwrap();
        let hint = decoded.state.entries[0].alert.infos[0].areas[0]
            .hint_geometry
            .clone()
            .unwrap();
        let Geometry::Polygon { exterior: back, .. } = hint.parts[&HintPart::Feature(0)].as_ref()
        else {
            panic!("not a polygon");
        };
        assert_eq!(back.len(), exterior.len());
        let inexact = back
            .iter()
            .flatten()
            .zip(exterior.iter().flatten())
            .filter(|(a, b)| a.to_bits() != b.to_bits())
            .count();
        assert_eq!(inexact, 0, "coordinates not read back bit for bit");
    }

    #[test]
    fn malformed_snapshots_are_rejected_whole() {
        let bytes = encoded(&populated(true), at(10));
        let text = String::from_utf8(bytes.clone()).unwrap();
        let err = |b: &[u8]| decode(b, "coll").err().expect("must be rejected");

        assert!(err(b"").starts_with("not a snapshot"));
        assert!(err(b"{\"format\":").starts_with("not a snapshot"));
        assert!(err(&bytes[..bytes.len() / 2]).starts_with("not a snapshot"));
        assert!(err(b"[1,2,3]").starts_with("not a snapshot"));
        assert_eq!(
            decode(&bytes, "other").err().unwrap(),
            "snapshot of collection 'coll', not 'other'"
        );
        assert!(
            err(text.replace("\"version\":1", "\"version\":99").as_bytes())
                .contains("snapshot version 99")
        );
        assert!(err(text.replace(FORMAT, "something-else").as_bytes()).contains("unknown format"));
        assert!(err(text
            .replace("\"source\":\"bbox\"", "\"source\":\"guess\"")
            .as_bytes())
        .contains("unknown hint source"));
        // Point a hint at a geometry that does not exist, and at an area
        // that does not exist.
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let mut bad = v.clone();
        bad["alerts"][0]["hints"][0]["parts"][0][1] = 99.into();
        assert!(err(&serde_json::to_vec(&bad).unwrap()).contains("out of range"));
        let mut bad = v.clone();
        bad["alerts"][0]["hints"][0]["area"] = 5.into();
        assert!(err(&serde_json::to_vec(&bad).unwrap()).contains("out of range"));
        let mut bad = v;
        bad["alerts"][0]["received"] = "yesterday".into();
        assert!(decode(&serde_json::to_vec(&bad).unwrap(), "coll").is_err());
    }
}
