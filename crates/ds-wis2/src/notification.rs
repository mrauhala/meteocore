//! WIS2 Notification Message (WNM) parsing.
//!
//! A notification is a GeoJSON `Feature` (spec: wmo-im/wis2-notification-message,
//! conformance `http://wis.wmo.int/spec/wnm/1/conf/core`). Only the fields a
//! data consumer needs are typed here; every other `properties` member is kept
//! verbatim in [`Notification::extra`] so producer-specific hints (MeteoAlarm's
//! `alertId` / `indexInfo` / `indexArea`, `gts`, …) stay reachable without
//! this crate knowing about them.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::Wis2Error;

/// Upper bound on a raw notification payload. The spec caps inline content at
/// 4096 bytes and the rest of the message is small; anything bigger is not a
/// WNM and is dropped before JSON parsing.
pub const MAX_NOTIFICATION_BYTES: usize = 256 * 1024;

/// Spatial hint carried by the notification. Points are `[lon, lat, (elev)]`.
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    Point {
        lon: f64,
        lat: f64,
        elevation: Option<f64>,
    },
    /// Exterior ring as `[lon, lat]` pairs (holes ignored — the notification
    /// geometry is a coarse extent, never the product geometry itself).
    Polygon(Vec<[f64; 2]>),
    /// Any other / malformed geometry (kept opaque; never an error).
    Other,
}

impl Geometry {
    /// `[west, south, east, north]` of the geometry, if it has an extent.
    pub fn bbox(&self) -> Option<[f64; 4]> {
        match self {
            Geometry::Point { lon, lat, .. } => Some([*lon, *lat, *lon, *lat]),
            Geometry::Polygon(ring) if !ring.is_empty() => {
                let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
                for [x, y] in ring {
                    b[0] = b[0].min(*x);
                    b[1] = b[1].min(*y);
                    b[2] = b[2].max(*x);
                    b[3] = b[3].max(*y);
                }
                Some(b)
            }
            _ => None,
        }
    }
}

/// Checksum method + raw digest bytes (the wire form is base64).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    pub method: String,
    pub digest: Vec<u8>,
}

/// Inline payload (`properties.content`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    /// `utf-8`, `base64` or `gzip` (base64 of gzip).
    pub encoding: String,
    /// Declared size of the encoded value.
    pub size: u64,
    pub value: String,
}

/// One entry of the `links` array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub rel: String,
    pub href: String,
    pub media_type: Option<String>,
    pub length: Option<u64>,
}

/// A parsed WIS2 notification.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// MQTT topic the message arrived on.
    pub topic: String,
    /// Centre id — the fourth topic segment (`cache/a/wis2/<centre-id>/…`),
    /// when the topic has the standard shape.
    pub centre_id: Option<String>,
    /// Message id (UUID in the spec; kept as a string, never validated).
    pub id: String,
    /// Producer-assigned data identifier — the dedup / update / deletion key.
    pub data_id: String,
    /// Publication time (RFC 3339).
    pub pubtime: DateTime<Utc>,
    /// Data validity time (`properties.datetime`), when given.
    pub datetime: Option<DateTime<Utc>>,
    pub start_datetime: Option<DateTime<Utc>>,
    pub end_datetime: Option<DateTime<Utc>>,
    pub geometry: Option<Geometry>,
    pub integrity: Option<Integrity>,
    pub content: Option<Content>,
    pub links: Vec<Link>,
    pub metadata_id: Option<String>,
    pub wigos_station_identifier: Option<String>,
    /// Which Global Cache republished this message (absent on `origin/`).
    pub global_cache: Option<String>,
    /// Every `properties` member not typed above, verbatim.
    pub extra: serde_json::Map<String, Value>,
}

impl Notification {
    /// The download link for the data: `rel=canonical`, else `rel=update`.
    pub fn canonical_link(&self) -> Option<&Link> {
        self.links
            .iter()
            .find(|l| l.rel == "canonical")
            .or_else(|| self.links.iter().find(|l| l.rel == "update"))
    }

    /// Whether the message announces the removal of `data_id`.
    pub fn is_deletion(&self) -> bool {
        self.links.iter().any(|l| l.rel == "deletion")
    }

    /// First link with the given `rel`.
    pub fn link(&self, rel: &str) -> Option<&Link> {
        self.links.iter().find(|l| l.rel == rel)
    }

    /// Integer-valued extra property (MeteoAlarm's `indexInfo` etc.).
    pub fn extra_u64(&self, key: &str) -> Option<u64> {
        self.extra.get(key).and_then(Value::as_u64)
    }

    /// String-valued extra property.
    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }
}

// ---- wire shape --------------------------------------------------------

#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type")]
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, rename = "conformsTo")]
    conforms_to: Option<Vec<String>>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    geometry: Option<Value>,
    properties: serde_json::Map<String, Value>,
    #[serde(default)]
    links: Vec<WireLink>,
}

#[derive(Deserialize)]
struct WireLink {
    #[serde(default)]
    rel: Option<String>,
    #[serde(default)]
    href: Option<String>,
    #[serde(default, rename = "type")]
    media_type: Option<String>,
    #[serde(default)]
    length: Option<Value>,
}

const TYPED_PROPS: &[&str] = &[
    "data_id",
    "pubtime",
    "datetime",
    "start_datetime",
    "end_datetime",
    "integrity",
    "content",
    "metadata_id",
    "wigos_station_identifier",
    "global-cache",
];

/// Parse a raw MQTT payload received on `topic`.
///
/// Accepts both the current `conformsTo` form and the legacy `version: "v04"`
/// form (still emitted by some wis2box 1.x nodes). Malformed timestamps in
/// optional fields are ignored; a malformed `pubtime` is an error because the
/// dedup ring orders on it.
pub fn parse_notification(topic: &str, payload: &[u8]) -> Result<Notification, Wis2Error> {
    if payload.len() > MAX_NOTIFICATION_BYTES {
        return Err(Wis2Error::Parse(format!(
            "notification of {} bytes exceeds the {MAX_NOTIFICATION_BYTES}-byte cap",
            payload.len()
        )));
    }
    let wire: Wire =
        serde_json::from_slice(payload).map_err(|e| Wis2Error::Parse(format!("json: {e}")))?;
    if wire.kind.as_deref() != Some("Feature") {
        return Err(Wis2Error::Parse("not a GeoJSON Feature".into()));
    }
    let conforms = wire
        .conforms_to
        .as_ref()
        .map(|c| c.iter().any(|s| s.contains("wis.wmo.int/spec/wnm/1")))
        .unwrap_or(false);
    if !conforms && wire.version.as_deref() != Some("v04") {
        return Err(Wis2Error::Parse(
            "missing conformsTo (wnm/1) and legacy version marker".into(),
        ));
    }
    let id = wire
        .id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Wis2Error::Parse("missing id".into()))?;

    let props = wire.properties;
    let data_id = props
        .get("data_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Wis2Error::Parse("missing properties.data_id".into()))?
        .to_string();
    let pubtime = props
        .get("pubtime")
        .and_then(Value::as_str)
        .ok_or_else(|| Wis2Error::Parse("missing properties.pubtime".into()))
        .and_then(|s| {
            parse_rfc3339(s).ok_or_else(|| Wis2Error::Parse(format!("bad pubtime '{s}'")))
        })?;

    let opt_time = |key: &str| {
        props
            .get(key)
            .and_then(Value::as_str)
            .and_then(parse_rfc3339)
    };
    let opt_str = |key: &str| {
        props
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let integrity = props.get("integrity").and_then(|v| {
        let method = v.get("method")?.as_str()?.trim().to_ascii_lowercase();
        let value = v.get("value")?.as_str()?;
        let digest = decode_base64(value)?;
        Some(Integrity { method, digest })
    });
    let content = props.get("content").and_then(|v| {
        let encoding = v.get("encoding")?.as_str()?.trim().to_ascii_lowercase();
        let value = v.get("value")?.as_str()?.to_string();
        let size = v
            .get("size")
            .and_then(Value::as_u64)
            .unwrap_or(value.len() as u64);
        Some(Content {
            encoding,
            size,
            value,
        })
    });

    let links = wire
        .links
        .into_iter()
        .filter_map(|l| {
            let href = l.href?.trim().to_string();
            if href.is_empty() {
                return None;
            }
            Some(Link {
                rel: l.rel.unwrap_or_default().trim().to_ascii_lowercase(),
                href,
                media_type: l.media_type.map(|t| t.trim().to_ascii_lowercase()),
                length: l.length.and_then(|v| match v {
                    Value::Number(n) => n.as_u64(),
                    Value::String(s) => s.parse().ok(),
                    _ => None,
                }),
            })
        })
        .collect();

    let mut extra = serde_json::Map::new();
    for (k, v) in props.iter() {
        if !TYPED_PROPS.contains(&k.as_str()) {
            extra.insert(k.clone(), v.clone());
        }
    }

    Ok(Notification {
        topic: topic.to_string(),
        centre_id: centre_id_of(topic),
        id,
        data_id,
        pubtime,
        datetime: opt_time("datetime"),
        start_datetime: opt_time("start_datetime"),
        end_datetime: opt_time("end_datetime"),
        geometry: wire.geometry.as_ref().and_then(parse_geometry),
        integrity,
        content,
        links,
        metadata_id: opt_str("metadata_id"),
        wigos_station_identifier: opt_str("wigos_station_identifier"),
        global_cache: opt_str("global-cache"),
        extra,
    })
}

/// `cache/a/wis2/<centre-id>/…` → `<centre-id>`.
pub fn centre_id_of(topic: &str) -> Option<String> {
    let mut it = topic.split('/');
    let root = it.next()?;
    if root != "cache" && root != "origin" {
        return None;
    }
    let _version = it.next()?;
    if it.next()? != "wis2" {
        return None;
    }
    let centre = it.next()?;
    (!centre.is_empty()).then(|| centre.to_string())
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

pub(crate) fn decode_base64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(&s)
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(&s)
                .ok()
        })
}

fn parse_geometry(v: &Value) -> Option<Geometry> {
    if v.is_null() {
        return None;
    }
    let kind = v.get("type")?.as_str()?;
    let coords = v.get("coordinates")?;
    match kind {
        "Point" => {
            let arr = coords.as_array()?;
            let lon = arr.first()?.as_f64()?;
            let lat = arr.get(1)?.as_f64()?;
            if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
                return Some(Geometry::Other);
            }
            Some(Geometry::Point {
                lon,
                lat,
                elevation: arr.get(2).and_then(Value::as_f64),
            })
        }
        "Polygon" => {
            let ring = coords.as_array()?.first()?.as_array()?;
            let mut out = Vec::with_capacity(ring.len());
            for p in ring {
                let p = p.as_array()?;
                let x = p.first()?.as_f64()?;
                let y = p.get(1)?.as_f64()?;
                if !x.is_finite() || !y.is_finite() {
                    return Some(Geometry::Other);
                }
                out.push([x, y]);
            }
            if out.len() < 4 {
                return Some(Geometry::Other);
            }
            Some(Geometry::Polygon(out))
        }
        _ => Some(Geometry::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"{"id":"m1","type":"Feature","conformsTo":["http://wis.wmo.int/spec/wnm/1/conf/core"],
        "geometry":null,"properties":{"data_id":"x:y/z","pubtime":"2026-09-12T08:20:02Z"},
        "links":[{"rel":"canonical","href":"https://gc.example/x.bufr","type":"application/bufr","length":380}]}"#;

    #[test]
    fn parses_minimal_and_derives_centre() {
        let n = parse_notification(
            "cache/a/wis2/se-smhi/data/core/weather/surface-based-observations/synop",
            MIN.as_bytes(),
        )
        .unwrap();
        assert_eq!(n.id, "m1");
        assert_eq!(n.data_id, "x:y/z");
        assert_eq!(n.centre_id.as_deref(), Some("se-smhi"));
        assert_eq!(n.pubtime.to_rfc3339(), "2026-09-12T08:20:02+00:00");
        assert!(n.geometry.is_none());
        assert_eq!(
            n.canonical_link().unwrap().href,
            "https://gc.example/x.bufr"
        );
        assert_eq!(n.canonical_link().unwrap().length, Some(380));
        assert!(!n.is_deletion());
        assert!(n.extra.is_empty());
    }

    #[test]
    fn legacy_version_marker_accepted_and_typed_fields_extracted() {
        let raw = r#"{"id":"08666d4c","version":"v04","type":"Feature",
          "geometry":{"type":"Point","coordinates":[18.983913,57.440752]},
          "properties":{"metadata_id":"urn:wmo:md:se-smhi:core","pubtime":"2026-09-12T08:20:07Z",
            "datetime":"2026-09-12T08:00:00Z","data_id":"origin/a/wis2/se-smhi/x",
            "integrity":{"method":"sha512","value":"AAECAw=="},
            "content":{"encoding":"base64","value":"QlVGUg==","size":4},
            "global-cache":"cn-cma-global-cache","gts":{"ttaaii":"ISNC02"}},
          "links":[{"href":"https://gc.wis.cma.cn/x","rel":"canonical","type":"application/bufr","length":"257"}]}"#;
        let n = parse_notification("cache/a/wis2/se-smhi/x", raw.as_bytes()).unwrap();
        assert_eq!(
            n.geometry,
            Some(Geometry::Point {
                lon: 18.983913,
                lat: 57.440752,
                elevation: None
            })
        );
        assert_eq!(
            n.datetime.unwrap().to_rfc3339(),
            "2026-09-12T08:00:00+00:00"
        );
        let i = n.integrity.unwrap();
        assert_eq!(i.method, "sha512");
        assert_eq!(i.digest, vec![0, 1, 2, 3]);
        let c = n.content.unwrap();
        assert_eq!(c.encoding, "base64");
        assert_eq!(c.size, 4);
        assert_eq!(n.global_cache.as_deref(), Some("cn-cma-global-cache"));
        assert_eq!(n.links[0].length, Some(257));
        assert!(n.extra.contains_key("gts"));
        assert!(!n.extra.contains_key("data_id"));
    }

    #[test]
    fn rejects_non_feature_missing_marker_and_missing_keys() {
        let bad = |s: &str| parse_notification("cache/a/wis2/x", s.as_bytes()).is_err();
        assert!(bad(
            r#"{"id":"a","type":"FeatureCollection","conformsTo":["http://wis.wmo.int/spec/wnm/1/conf/core"],"properties":{"data_id":"d","pubtime":"2026-01-01T00:00:00Z"},"links":[]}"#
        ));
        assert!(bad(
            r#"{"id":"a","type":"Feature","properties":{"data_id":"d","pubtime":"2026-01-01T00:00:00Z"},"links":[]}"#
        ));
        assert!(bad(
            r#"{"id":"a","type":"Feature","version":"v04","properties":{"pubtime":"2026-01-01T00:00:00Z"},"links":[]}"#
        ));
        assert!(bad(
            r#"{"id":"a","type":"Feature","version":"v04","properties":{"data_id":"d","pubtime":"yesterday"},"links":[]}"#
        ));
        assert!(bad(
            r#"{"type":"Feature","version":"v04","properties":{"data_id":"d","pubtime":"2026-01-01T00:00:00Z"},"links":[]}"#
        ));
        assert!(bad("not json"));
    }

    #[test]
    fn oversized_payload_rejected_before_parsing() {
        let big = vec![b' '; MAX_NOTIFICATION_BYTES + 1];
        assert!(matches!(
            parse_notification("cache/a/wis2/x", &big),
            Err(Wis2Error::Parse(_))
        ));
    }

    #[test]
    fn polygon_geometry_and_bbox() {
        let raw = r#"{"id":"a","type":"Feature","version":"v04",
          "geometry":{"type":"Polygon","coordinates":[[[20.5,41.4],[20.5,42.2],[21.2,42.2],[21.2,41.4],[20.5,41.4]]]},
          "properties":{"data_id":"d","pubtime":"2026-01-01T00:00:00Z"},
          "links":[{"rel":"deletion","href":"https://x/y"}]}"#;
        let n = parse_notification("origin/a/wis2/eu-eumetnet-warnings/x", raw.as_bytes()).unwrap();
        assert_eq!(
            n.geometry.as_ref().unwrap().bbox(),
            Some([20.5, 41.4, 21.2, 42.2])
        );
        assert!(n.is_deletion());
        assert!(n.canonical_link().is_none());
    }

    #[test]
    fn centre_id_requires_standard_topic_shape() {
        assert_eq!(
            centre_id_of("cache/a/wis2/de-dwd/data").as_deref(),
            Some("de-dwd")
        );
        assert_eq!(
            centre_id_of("origin/a/wis2/fi-fmi").as_deref(),
            Some("fi-fmi")
        );
        assert!(centre_id_of("cache/a/wis2//data").is_none());
        assert!(centre_id_of("ORD/fi/x").is_none());
        assert!(centre_id_of("cache/a/wis3/x").is_none());
    }
}
