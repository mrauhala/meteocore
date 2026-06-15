//! OASIS Common Alerting Protocol (CAP) v1.2 XML parser.
//!
//! Parses a CAP document (`<alert>`) into the domain structs the catalog turns
//! into per-area Features and map fills. Built with `quick-xml`'s streaming
//! reader as a small stack machine over **local** element names (the CAP
//! namespace prefix is ignored), so it tolerates the namespace-prefixed and
//! default-namespace forms feeds use interchangeably.
//!
//! Spec: <https://docs.oasis-open.org/emergency/cap/v1.2/CAP-v1.2-os.html>
//!
//! **Load-bearing coordinate gotcha:** CAP coordinates are `lat,lon` (spec
//! §3.3.4), but `ds_core::Geometry` polygons are `[lon, lat]`. This parser
//! **swaps on ingest** — see [`parse_polygon`] / [`parse_circle`]. Getting this
//! wrong silently places alerts in the wrong hemisphere.

use chrono::{DateTime, Utc};
use quick_xml::events::Event;
use quick_xml::Reader;

use ds_core::error::DataServerError;

/// Cap on coordinate pairs in a single `<polygon>` (geometry-bomb guard).
const MAX_POLYGON_VERTICES: usize = 50_000;
/// Cap on `<alert>` elements parsed from one document.
const MAX_ALERTS_PER_DOC: usize = 10_000;

/// A parsed CAP `<alert>` (one emergency message).
#[derive(Debug, Clone, Default)]
pub struct CapAlert {
    pub identifier: String,
    pub sender: Option<String>,
    pub sent: Option<DateTime<Utc>>,
    pub status: Option<String>,
    pub msg_type: Option<String>,
    pub scope: Option<String>,
    pub references: Option<String>,
    pub infos: Vec<CapInfo>,
}

/// A parsed `<info>` block (one language variant of an alert).
#[derive(Debug, Clone, Default)]
pub struct CapInfo {
    pub language: Option<String>,
    pub categories: Vec<String>,
    pub event: Option<String>,
    pub response_types: Vec<String>,
    pub urgency: Option<String>,
    pub severity: Option<String>,
    pub certainty: Option<String>,
    pub effective: Option<DateTime<Utc>>,
    pub onset: Option<DateTime<Utc>>,
    pub expires: Option<DateTime<Utc>>,
    pub sender_name: Option<String>,
    pub headline: Option<String>,
    pub description: Option<String>,
    pub instruction: Option<String>,
    pub web: Option<String>,
    pub areas: Vec<CapArea>,
}

/// A parsed `<area>` (one affected region of an info block).
#[derive(Debug, Clone, Default)]
pub struct CapArea {
    pub area_desc: Option<String>,
    /// Each polygon is a closed ring of `[lon, lat]` (swapped from CAP `lat,lon`).
    pub polygons: Vec<Vec<[f64; 2]>>,
    pub circles: Vec<CapCircle>,
    /// `(valueName, value)` geocode pairs (no renderable geometry).
    pub geocodes: Vec<(String, String)>,
}

/// A parsed `<circle>`: centre (`[lon, lat]`) + radius in kilometres.
#[derive(Debug, Clone, Copy)]
pub struct CapCircle {
    pub lon: f64,
    pub lat: f64,
    pub radius_km: f64,
}

/// Parse a CAP document into its `<alert>` elements.
///
/// Returns an empty vector for a document with no `<alert>` (e.g. an Atom feed
/// fed here by mistake) rather than erroring, so one malformed file can't sink a
/// whole directory scan. A hard XML error (truncated/invalid markup) is returned
/// as `Err`.
pub fn parse_document(xml: &str) -> Result<Vec<CapAlert>, DataServerError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut alerts: Vec<CapAlert> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();

    // In-progress containers.
    let mut alert: Option<CapAlert> = None;
    let mut info: Option<CapInfo> = None;
    let mut area: Option<CapArea> = None;
    let mut geocode_name: Option<String> = None;
    let mut geocode_value: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = decode_name(e.local_name().as_ref());
                match name.as_str() {
                    "alert" => alert = Some(CapAlert::default()),
                    "info" => info = Some(CapInfo::default()),
                    "area" => area = Some(CapArea::default()),
                    "geocode" => {
                        geocode_name = None;
                        geocode_value = None;
                    }
                    _ => {}
                }
                path.push(name);
                text.clear();
            }
            Ok(Event::Text(e)) => {
                let chunk = e
                    .unescape()
                    .map_err(|err| DataServerError::Engine(format!("CAP XML text error: {err}")))?;
                text.push_str(&chunk);
            }
            Ok(Event::CData(e)) => {
                // CAP `<description>`/`<instruction>` are sometimes CDATA-wrapped.
                text.push_str(&String::from_utf8_lossy(e.as_ref()));
            }
            Ok(Event::End(e)) => {
                let name = decode_name(e.local_name().as_ref());
                let parent = if path.len() >= 2 {
                    path[path.len() - 2].as_str()
                } else {
                    ""
                };
                let leaf = text.trim().to_string();

                match (parent, name.as_str()) {
                    // ----- alert-level leaves -----
                    ("alert", "identifier") => set(&mut alert, |a| a.identifier = leaf.clone()),
                    ("alert", "sender") => set(&mut alert, |a| a.sender = some(&leaf)),
                    ("alert", "sent") => set(&mut alert, |a| a.sent = parse_time(&leaf)),
                    ("alert", "status") => set(&mut alert, |a| a.status = some(&leaf)),
                    ("alert", "msgType") => set(&mut alert, |a| a.msg_type = some(&leaf)),
                    ("alert", "scope") => set(&mut alert, |a| a.scope = some(&leaf)),
                    ("alert", "references") => set(&mut alert, |a| a.references = some(&leaf)),
                    // ----- info-level leaves -----
                    ("info", "language") => set(&mut info, |i| i.language = some(&leaf)),
                    ("info", "category") => push_if(&mut info, &leaf, |i, v| i.categories.push(v)),
                    ("info", "event") => set(&mut info, |i| i.event = some(&leaf)),
                    ("info", "responseType") => {
                        push_if(&mut info, &leaf, |i, v| i.response_types.push(v))
                    }
                    ("info", "urgency") => set(&mut info, |i| i.urgency = some(&leaf)),
                    ("info", "severity") => set(&mut info, |i| i.severity = some(&leaf)),
                    ("info", "certainty") => set(&mut info, |i| i.certainty = some(&leaf)),
                    ("info", "effective") => set(&mut info, |i| i.effective = parse_time(&leaf)),
                    ("info", "onset") => set(&mut info, |i| i.onset = parse_time(&leaf)),
                    ("info", "expires") => set(&mut info, |i| i.expires = parse_time(&leaf)),
                    ("info", "senderName") => set(&mut info, |i| i.sender_name = some(&leaf)),
                    ("info", "headline") => set(&mut info, |i| i.headline = some(&leaf)),
                    ("info", "description") => set(&mut info, |i| i.description = some(&leaf)),
                    ("info", "instruction") => set(&mut info, |i| i.instruction = some(&leaf)),
                    ("info", "web") => set(&mut info, |i| i.web = some(&leaf)),
                    // ----- area-level leaves -----
                    ("area", "areaDesc") => set(&mut area, |a| a.area_desc = some(&leaf)),
                    ("area", "polygon") => {
                        if let (Some(a), Some(ring)) = (area.as_mut(), parse_polygon(&leaf)) {
                            a.polygons.push(ring);
                        }
                    }
                    ("area", "circle") => {
                        if let (Some(a), Some(c)) = (area.as_mut(), parse_circle(&leaf)) {
                            a.circles.push(c);
                        }
                    }
                    // ----- geocode children -----
                    ("geocode", "valueName") => geocode_name = some(&leaf),
                    ("geocode", "value") => geocode_value = some(&leaf),
                    _ => {}
                }

                // ----- close containers -----
                match name.as_str() {
                    "geocode" => {
                        if let (Some(a), Some(n)) = (area.as_mut(), geocode_name.take()) {
                            a.geocodes
                                .push((n, geocode_value.take().unwrap_or_default()));
                        }
                    }
                    "area" => {
                        if let (Some(i), Some(a)) = (info.as_mut(), area.take()) {
                            i.areas.push(a);
                        }
                    }
                    "info" => {
                        if let (Some(al), Some(i)) = (alert.as_mut(), info.take()) {
                            al.infos.push(i);
                        }
                    }
                    "alert" => {
                        if let Some(a) = alert.take() {
                            if !a.identifier.is_empty() {
                                alerts.push(a);
                                if alerts.len() >= MAX_ALERTS_PER_DOC {
                                    tracing::warn!(
                                        "cap: document hit the {MAX_ALERTS_PER_DOC}-alert cap — \
                                         remaining <alert> elements ignored"
                                    );
                                    break;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                path.pop();
                text.clear();
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(DataServerError::Engine(format!(
                    "CAP XML parse error at byte {}: {e}",
                    reader.buffer_position()
                )))
            }
            _ => {}
        }
    }

    Ok(alerts)
}

/// Lossily decode an already-namespace-stripped element name (`quick_xml`'s
/// `local_name()` removes the prefix; this only turns the bytes into a `String`).
fn decode_name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Apply `f` to the in-progress container if present.
fn set<T>(slot: &mut Option<T>, f: impl FnOnce(&mut T)) {
    if let Some(v) = slot.as_mut() {
        f(v);
    }
}

/// Push a non-empty trimmed value into a list field of the container.
fn push_if<T>(slot: &mut Option<T>, leaf: &str, f: impl FnOnce(&mut T, String)) {
    if let (Some(v), Some(s)) = (slot.as_mut(), some(leaf)) {
        f(v, s);
    }
}

/// `Some(trimmed)` for non-empty text, else `None`.
fn some(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Parse a CAP ISO 8601 timestamp (with timezone) to UTC.
fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(t)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Parse a CAP `<polygon>` (`lat,lon lat,lon …`) into a closed `[lon, lat]`
/// ring. Returns `None` if fewer than 3 distinct vertices survive validation
/// (degenerate ring), or if the vertex count exceeds [`MAX_POLYGON_VERTICES`] —
/// **rejected, not truncated**, since a truncated ring is a different (wrong)
/// polygon. The ring is closed defensively (first == last appended if missing),
/// matching `ds_core::Geometry::Polygon`'s closed-ring expectation.
pub fn parse_polygon(s: &str) -> Option<Vec<[f64; 2]>> {
    // Single pass: reject (don't truncate into a wrong shape) once the count of
    // *valid* vertices would exceed the cap.
    let mut ring: Vec<[f64; 2]> = Vec::new();
    for pair in s.split_whitespace() {
        if let Some(p) = parse_lat_lon(pair) {
            if ring.len() >= MAX_POLYGON_VERTICES {
                tracing::warn!(
                    "cap: <polygon> exceeds {MAX_POLYGON_VERTICES} vertices — dropped (not truncated)"
                );
                return None;
            }
            ring.push(p);
        }
    }
    // Need ≥3 distinct vertices for an area; close defensively.
    if ring.len() < 3 {
        return None;
    }
    if ring.first() != ring.last() {
        ring.push(ring[0]);
    }
    // After closing, a valid ring has ≥4 points (3 distinct + repeat).
    if ring.len() < 4 {
        return None;
    }
    Some(ring)
}

/// Parse a CAP `<circle>` (`lat,lon radius`, radius in km) into centre + radius.
pub fn parse_circle(s: &str) -> Option<CapCircle> {
    let mut it = s.split_whitespace();
    let centre = it.next()?;
    let radius = it.next()?;
    let [lon, lat] = parse_lat_lon(centre)?;
    let radius_km: f64 = radius.parse().ok()?;
    if !radius_km.is_finite() || radius_km <= 0.0 {
        return None;
    }
    Some(CapCircle {
        lon,
        lat,
        radius_km,
    })
}

/// Parse one CAP `lat,lon` token into a validated `[lon, lat]` pair (**swapped**).
fn parse_lat_lon(token: &str) -> Option<[f64; 2]> {
    let (lat_s, lon_s) = token.split_once(',')?;
    let lat: f64 = lat_s.trim().parse().ok()?;
    let lon: f64 = lon_s.trim().parse().ok()?;
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some([lon, lat])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
  <identifier>URN:TEST:1</identifier>
  <sender>test@example.org</sender>
  <sent>2026-06-15T10:00:00-00:00</sent>
  <status>Actual</status>
  <msgType>Alert</msgType>
  <scope>Public</scope>
  <info>
    <language>en-US</language>
    <category>Met</category>
    <event>Flood Warning</event>
    <responseType>Prepare</responseType>
    <urgency>Expected</urgency>
    <severity>Severe</severity>
    <certainty>Likely</certainty>
    <effective>2026-06-15T10:00:00-00:00</effective>
    <onset>2026-06-15T11:00:00-00:00</onset>
    <expires>2026-06-15T18:00:00-00:00</expires>
    <senderName>Test Weather Service</senderName>
    <headline>Flood Warning issued</headline>
    <description>Heavy rain.</description>
    <instruction>Move to higher ground.</instruction>
    <web>https://example.org/alert/1</web>
    <area>
      <areaDesc>Test County</areaDesc>
      <polygon>60.0,24.0 60.0,25.0 61.0,25.0 61.0,24.0 60.0,24.0</polygon>
      <geocode>
        <valueName>UGC</valueName>
        <value>FIC001</value>
      </geocode>
    </area>
  </info>
</alert>"#;

    #[test]
    fn parses_full_alert() {
        let alerts = parse_document(SAMPLE).unwrap();
        assert_eq!(alerts.len(), 1);
        let a = &alerts[0];
        assert_eq!(a.identifier, "URN:TEST:1");
        assert_eq!(a.status.as_deref(), Some("Actual"));
        assert_eq!(a.msg_type.as_deref(), Some("Alert"));
        assert_eq!(a.infos.len(), 1);
        let i = &a.infos[0];
        assert_eq!(i.language.as_deref(), Some("en-US"));
        assert_eq!(i.event.as_deref(), Some("Flood Warning"));
        assert_eq!(i.severity.as_deref(), Some("Severe"));
        assert_eq!(i.categories, vec!["Met".to_string()]);
        assert_eq!(i.response_types, vec!["Prepare".to_string()]);
        assert!(i.onset.is_some() && i.expires.is_some());
        assert_eq!(i.areas.len(), 1);
        let ar = &i.areas[0];
        assert_eq!(ar.area_desc.as_deref(), Some("Test County"));
        assert_eq!(ar.polygons.len(), 1);
        assert_eq!(ar.geocodes, vec![("UGC".to_string(), "FIC001".to_string())]);
    }

    #[test]
    fn polygon_swaps_lat_lon_and_closes() {
        // CAP lat,lon 60,24 → ds-core [lon,lat] [24,60].
        let ring = parse_polygon("60,24 60,25 61,25 61,24").unwrap();
        assert_eq!(ring[0], [24.0, 60.0]);
        assert_eq!(ring[1], [25.0, 60.0]);
        // Closed defensively (last == first even though input wasn't closed).
        assert_eq!(ring.first(), ring.last());
    }

    #[test]
    fn polygon_rejects_out_of_range_and_degenerate() {
        // lat 200 is out of range → that pair is dropped, leaving < 3 → None.
        assert!(parse_polygon("200,24 60,25").is_none());
        assert!(parse_polygon("60,24 60,25").is_none()); // only 2 vertices
    }

    #[test]
    fn circle_parses_and_swaps() {
        let c = parse_circle("60.5,24.5 12.5").unwrap();
        assert_eq!(c.lon, 24.5);
        assert_eq!(c.lat, 60.5);
        assert_eq!(c.radius_km, 12.5);
        assert!(parse_circle("60.5,24.5 0").is_none()); // zero radius
        assert!(parse_circle("60.5,24.5").is_none()); // missing radius
    }

    #[test]
    fn handles_prefixed_namespace() {
        let xml = r#"<cap:alert xmlns:cap="urn:oasis:names:tc:emergency:cap:1.2">
          <cap:identifier>P1</cap:identifier>
          <cap:status>Actual</cap:status>
          <cap:info><cap:event>Storm</cap:event></cap:info>
        </cap:alert>"#;
        let alerts = parse_document(xml).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].identifier, "P1");
        assert_eq!(alerts[0].infos[0].event.as_deref(), Some("Storm"));
    }

    #[test]
    fn multiple_infos_and_areas() {
        let xml = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
          <identifier>M1</identifier><status>Actual</status>
          <info><language>en</language><event>Heat</event>
            <area><areaDesc>A</areaDesc><polygon>0,0 0,1 1,1 1,0 0,0</polygon></area>
            <area><areaDesc>B</areaDesc><polygon>2,2 2,3 3,3 3,2 2,2</polygon></area>
          </info>
          <info><language>fr</language><event>Chaleur</event>
            <area><areaDesc>A</areaDesc><polygon>0,0 0,1 1,1 1,0 0,0</polygon></area>
          </info>
        </alert>"#;
        let alerts = parse_document(xml).unwrap();
        assert_eq!(alerts[0].infos.len(), 2);
        assert_eq!(alerts[0].infos[0].areas.len(), 2);
        assert_eq!(alerts[0].infos[1].language.as_deref(), Some("fr"));
    }

    #[test]
    fn non_cap_document_yields_no_alerts() {
        let atom =
            r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>x</title></entry></feed>"#;
        assert!(parse_document(atom).unwrap().is_empty());
    }
}
