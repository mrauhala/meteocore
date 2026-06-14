//! The CAP catalog: the parsed-alert snapshot that backs both the `FeatureEngine`
//! (one feature per alert area) and the `MapEngine` (severity-shaded polygon
//! fills). Built once per poll/refresh and swapped atomically; both trait
//! surfaces read from the same immutable snapshot.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{DateTime, Duration, Utc};
use rstar::{RTree, RTreeObject, AABB};

use ds_core::feature::{Bbox, Geometry, PropertyValue};
use ds_core::geo::destination_point;
use ds_core::map_engine::RasterInfo;

use crate::parser::{CapAlert, CapArea, CapCircle, CapInfo};

/// Cap on advertised TIME-dimension values (keeps WMS GetCapabilities bounded).
const MAX_TIME_VALUES: usize = 256;

/// Resolved engine knobs passed into [`Catalog::build`].
#[derive(Debug, Clone)]
pub struct BuildConfig {
    /// Preferred `<info>` language (case-insensitive primary-subtag match);
    /// `None` exposes every `<info>`.
    pub language: Option<String>,
    /// Lowercased `<status>` allowlist; empty serves every status.
    pub status_filter: Vec<String>,
    /// Validity added to onset/effective when `<expires>` is absent; `None`
    /// leaves such alerts open-ended.
    pub default_ttl: Option<Duration>,
    /// Circle → N-gon vertex count.
    pub circle_segments: u32,
}

/// The active-validity window of an alert area: `[start, end]` with open bounds.
/// `start = None` ⇒ active since forever; `end = None` ⇒ active until superseded.
#[derive(Debug, Clone, Copy)]
pub struct ActiveWindow {
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

impl ActiveWindow {
    /// Whether the area is active at instant `t`.
    pub fn active_at(&self, t: DateTime<Utc>) -> bool {
        self.start.is_none_or(|s| s <= t) && self.end.is_none_or(|e| t <= e)
    }

    /// Whether the window overlaps the (possibly open) query interval
    /// `[qs, qe]`. Open query bounds are ±∞.
    pub fn intersects(&self, qs: Option<DateTime<Utc>>, qe: Option<DateTime<Utc>>) -> bool {
        // qs <= end  (else the query starts after the window closed)
        let after_start = match (self.end, qs) {
            (Some(e), Some(qs)) => qs <= e,
            _ => true,
        };
        // start <= qe  (else the window opens after the query ends)
        let before_end = match (self.start, qe) {
            (Some(s), Some(qe)) => s <= qe,
            _ => true,
        };
        after_start && before_end
    }
}

/// One renderable/queryable alert area = one OGC Feature.
pub struct AreaRecord {
    pub id: String,
    pub geometry: Arc<Geometry>,
    pub properties: Arc<HashMap<String, PropertyValue>>,
    /// Geometry bbox `[w, s, e, n]`; `None` for geocode-only (null-geometry) areas.
    pub bbox: Option<[f64; 4]>,
    pub window: ActiveWindow,
    /// CAP severity code 0–4 (Unknown..Extreme), the value filled into the map raster.
    pub severity_code: f64,
}

/// rstar index entry over an area's bbox.
struct IndexedArea {
    index: usize,
    envelope: AABB<[f64; 2]>,
}

impl RTreeObject for IndexedArea {
    type Envelope = AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

/// An immutable snapshot of the parsed alert set.
pub struct Catalog {
    pub records: Vec<AreaRecord>,
    id_index: HashMap<String, usize>,
    tree: RTree<IndexedArea>,
    pub spatial_extent: Option<[f64; 4]>,
    /// Prebuilt metadata snapshot so `raster_info()` is O(1) (#211).
    pub info: Arc<RasterInfo>,
    /// The moment this snapshot reflects ("now" for un-pinned TIME selection).
    pub as_of: DateTime<Utc>,
    /// Opaque content hash for Feature ETags (stable across restarts).
    pub data_version: u64,
    /// Areas dropped from the map for having only geocodes (no geometry).
    pub geocode_only_count: usize,
}

impl Catalog {
    /// An empty snapshot (before the first successful load).
    pub fn empty(collection_id: &str, parameter: &str, as_of: DateTime<Utc>) -> Self {
        Catalog {
            records: Vec::new(),
            id_index: HashMap::new(),
            tree: RTree::new(),
            spatial_extent: None,
            info: Arc::new(base_raster_info(collection_id, parameter, None, Vec::new())),
            as_of,
            data_version: 0,
            geocode_only_count: 0,
        }
    }

    /// Build a snapshot from parsed alerts.
    pub fn build(
        alerts: &[CapAlert],
        cfg: &BuildConfig,
        collection_id: &str,
        parameter: &str,
        as_of: DateTime<Utc>,
    ) -> Self {
        let mut records: Vec<AreaRecord> = Vec::new();
        let mut geocode_only_count = 0usize;

        for alert in alerts {
            if !status_allowed(alert, &cfg.status_filter) {
                continue;
            }
            for (info_idx, info) in select_infos(alert, cfg.language.as_deref()) {
                for (area_idx, area) in info.areas.iter().enumerate() {
                    let geometry = build_geometry(area, cfg.circle_segments);
                    if matches!(geometry, Geometry::Null) && area.has_geocode_only() {
                        geocode_only_count += 1;
                    }
                    let bbox = geometry.bbox();
                    let id = format!("{}.{}.{}", alert.identifier, info_idx, area_idx);
                    let window = build_window(alert, info, cfg.default_ttl);
                    let severity_code = severity_code(info.severity.as_deref());
                    let properties =
                        build_properties(alert, info, area, window, geometry_kind_radius(area));
                    records.push(AreaRecord {
                        id,
                        geometry: Arc::new(geometry),
                        properties: Arc::new(properties),
                        bbox,
                        window,
                        severity_code,
                    });
                }
            }
        }

        // Deterministic order for stable pagination.
        records.sort_by(|a, b| a.id.cmp(&b.id));

        let id_index = records
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id.clone(), i))
            .collect();

        let tree = RTree::bulk_load(
            records
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    r.bbox.map(|b| IndexedArea {
                        index: i,
                        envelope: AABB::from_corners([b[0], b[1]], [b[2], b[3]]),
                    })
                })
                .collect(),
        );

        let spatial_extent = union_extent(&records);
        let times = build_times(&records, as_of);
        let data_version = compute_version(&records);
        let info = Arc::new(base_raster_info(
            collection_id,
            parameter,
            spatial_extent,
            times,
        ));

        Catalog {
            records,
            id_index,
            tree,
            spatial_extent,
            info,
            as_of,
            data_version,
            geocode_only_count,
        }
    }

    /// Record indices whose bbox intersects `bbox` (geometry areas only).
    pub fn query_bbox(&self, bbox: &Bbox) -> Vec<usize> {
        let aabb = AABB::from_corners([bbox.west, bbox.south], [bbox.east, bbox.north]);
        let mut idx: Vec<usize> = self
            .tree
            .locate_in_envelope_intersecting(&aabb)
            .map(|e| e.index)
            .collect();
        idx.sort_unstable();
        idx
    }

    /// Lookup a record by feature id.
    pub fn get(&self, id: &str) -> Option<&AreaRecord> {
        self.id_index.get(id).map(|&i| &self.records[i])
    }
}

/// The engine's live snapshot holder.
pub type CatalogStore = ArcSwap<Catalog>;

// ---------------------------------------------------------------------------
// Build helpers
// ---------------------------------------------------------------------------

/// Map a CAP `<severity>` to its numeric code 0–4 (Unknown..Extreme).
pub fn severity_code(severity: Option<&str>) -> f64 {
    match severity.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("extreme") => 4.0,
        Some("severe") => 3.0,
        Some("moderate") => 2.0,
        Some("minor") => 1.0,
        // "Unknown", anything unrecognised, or absent → 0.
        _ => 0.0,
    }
}

fn status_allowed(alert: &CapAlert, filter: &[String]) -> bool {
    if filter.is_empty() {
        return true;
    }
    match &alert.status {
        Some(s) => filter.contains(&s.trim().to_ascii_lowercase()),
        None => false,
    }
}

/// Select the `<info>` blocks to expose, paired with their original index (so
/// feature ids stay stable). `None` language ⇒ every info. A configured
/// language keeps infos whose primary subtag matches (case-insensitive); if no
/// info matches, the first info is kept so the alert isn't silently dropped.
fn select_infos<'a>(alert: &'a CapAlert, language: Option<&str>) -> Vec<(usize, &'a CapInfo)> {
    let all: Vec<(usize, &CapInfo)> = alert.infos.iter().enumerate().collect();
    let Some(lang) = language else {
        return all;
    };
    let want = primary_subtag(lang);
    let matched: Vec<(usize, &CapInfo)> = all
        .iter()
        .filter(|(_, i)| {
            i.language
                .as_deref()
                .map(|l| primary_subtag(l) == want)
                .unwrap_or(false)
        })
        .copied()
        .collect();
    if matched.is_empty() {
        // No translation in the requested language: fall back to the first info.
        all.into_iter().take(1).collect()
    } else {
        matched
    }
}

/// Lowercased primary language subtag (`"en-US"` → `"en"`).
fn primary_subtag(tag: &str) -> String {
    tag.split(['-', '_'])
        .next()
        .unwrap_or(tag)
        .to_ascii_lowercase()
}

/// Build the active window: start = onset ∨ effective ∨ sent;
/// end = expires ∨ (start + default_ttl) ∨ open.
fn build_window(alert: &CapAlert, info: &CapInfo, default_ttl: Option<Duration>) -> ActiveWindow {
    let start = info.onset.or(info.effective).or(alert.sent);
    let end = info.expires.or_else(|| match (start, default_ttl) {
        (Some(s), Some(ttl)) => Some(s + ttl),
        _ => None,
    });
    ActiveWindow { start, end }
}

/// `Some(radius_km)` when the area is exactly one circle with no polygons (the
/// only case where a single `radius_km` property is unambiguous).
fn geometry_kind_radius(area: &CapArea) -> Option<f64> {
    if area.polygons.is_empty() && area.circles.len() == 1 {
        Some(area.circles[0].radius_km)
    } else {
        None
    }
}

/// Assemble an area's geometry from its polygons + circles (each circle → an
/// N-gon). 0 shapes ⇒ `Null`, 1 ⇒ `Polygon`, >1 ⇒ `MultiPolygon`.
fn build_geometry(area: &CapArea, segments: u32) -> Geometry {
    #[allow(clippy::type_complexity)]
    let mut polys: Vec<(Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>)> = Vec::new();
    for ring in &area.polygons {
        polys.push((ring.clone(), Vec::new()));
    }
    for c in &area.circles {
        polys.push((circle_ring(c, segments), Vec::new()));
    }
    match polys.len() {
        0 => Geometry::Null,
        1 => {
            let (exterior, holes) = polys.pop().unwrap();
            Geometry::Polygon { exterior, holes }
        }
        _ => Geometry::MultiPolygon { polygons: polys },
    }
}

/// Approximate a CAP circle as a closed `[lon, lat]` ring on the geodesic.
fn circle_ring(c: &CapCircle, segments: u32) -> Vec<[f64; 2]> {
    let n = segments.max(3);
    let radius_m = c.radius_km * 1000.0;
    let mut ring = Vec::with_capacity(n as usize + 1);
    for i in 0..n {
        let bearing = 360.0 * (i as f64) / (n as f64);
        let (lon, lat) = destination_point(c.lon, c.lat, radius_m, bearing);
        ring.push([lon, lat]);
    }
    ring.push(ring[0]); // close
    ring
}

fn build_properties(
    alert: &CapAlert,
    info: &CapInfo,
    area: &CapArea,
    window: ActiveWindow,
    radius_km: Option<f64>,
) -> HashMap<String, PropertyValue> {
    let mut p: HashMap<String, PropertyValue> = HashMap::new();
    // alert-level
    p.insert(
        "identifier".into(),
        PropertyValue::String(alert.identifier.clone()),
    );
    put_str(&mut p, "sender", &alert.sender);
    put_str(&mut p, "status", &alert.status);
    put_str(&mut p, "msgType", &alert.msg_type);
    put_str(&mut p, "scope", &alert.scope);
    // info-level
    put_str(&mut p, "language", &info.language);
    put_str(&mut p, "event", &info.event);
    put_str(&mut p, "urgency", &info.urgency);
    put_str(&mut p, "severity", &info.severity);
    put_str(&mut p, "certainty", &info.certainty);
    put_str(&mut p, "senderName", &info.sender_name);
    put_str(&mut p, "headline", &info.headline);
    put_str(&mut p, "description", &info.description);
    put_str(&mut p, "instruction", &info.instruction);
    put_str(&mut p, "web", &info.web);
    // area-level
    put_str(&mut p, "areaDesc", &area.area_desc);

    // List-valued fields.
    if !info.categories.is_empty() {
        p.insert("category".into(), str_list(&info.categories));
    }
    if !info.response_types.is_empty() {
        p.insert("responseType".into(), str_list(&info.response_types));
    }

    // Times as RFC 3339 strings.
    insert_time(&mut p, "sent", alert.sent);
    insert_time(&mut p, "effective", info.effective);
    insert_time(&mut p, "onset", info.onset);
    insert_time(&mut p, "expires", info.expires);
    // The resolved validity end (after default_ttl), useful when <expires> was absent.
    insert_time(&mut p, "active_until", window.end);

    if let Some(r) = radius_km {
        p.insert("radius_km".into(), PropertyValue::Float(r));
    }
    p
}

fn put_str(p: &mut HashMap<String, PropertyValue>, key: &str, v: &Option<String>) {
    if let Some(s) = v {
        p.insert(key.to_string(), PropertyValue::String(s.clone()));
    }
}

fn str_list(items: &[String]) -> PropertyValue {
    PropertyValue::List(items.iter().cloned().map(PropertyValue::String).collect())
}

fn insert_time(p: &mut HashMap<String, PropertyValue>, key: &str, t: Option<DateTime<Utc>>) {
    if let Some(t) = t {
        p.insert(key.to_string(), PropertyValue::String(t.to_rfc3339()));
    }
}

fn union_extent(records: &[AreaRecord]) -> Option<[f64; 4]> {
    let mut ext = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    let mut any = false;
    for r in records {
        if let Some(b) = r.bbox {
            any = true;
            ext[0] = ext[0].min(b[0]);
            ext[1] = ext[1].min(b[1]);
            ext[2] = ext[2].max(b[2]);
            ext[3] = ext[3].max(b[3]);
        }
    }
    any.then_some(ext)
}

/// Distinct window boundaries at/below `as_of`, plus `as_of` itself (always the
/// max, so a WMS TIME-less request — which the handler resolves to `times.last()`
/// — renders the set active *now*). Capped to the most recent [`MAX_TIME_VALUES`].
fn build_times(records: &[AreaRecord], as_of: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let mut times: Vec<DateTime<Utc>> = Vec::new();
    for r in records {
        for b in [r.window.start, r.window.end].into_iter().flatten() {
            if b <= as_of {
                times.push(b);
            }
        }
    }
    times.push(as_of);
    times.sort_unstable();
    times.dedup();
    if times.len() > MAX_TIME_VALUES {
        times.drain(0..times.len() - MAX_TIME_VALUES);
    }
    times
}

fn compute_version(records: &[AreaRecord]) -> u64 {
    let mut hasher = DefaultHasher::new();
    records.len().hash(&mut hasher);
    for r in records {
        r.id.hash(&mut hasher);
        r.severity_code.to_bits().hash(&mut hasher);
        r.window.start.map(|t| t.timestamp()).hash(&mut hasher);
        r.window.end.map(|t| t.timestamp()).hash(&mut hasher);
    }
    hasher.finish()
}

fn base_raster_info(
    collection_id: &str,
    parameter: &str,
    spatial_extent: Option<[f64; 4]>,
    times: Vec<DateTime<Utc>>,
) -> RasterInfo {
    let _ = collection_id;
    RasterInfo {
        native_crs: "CRS:84".to_string(),
        spatial_extent,
        times,
        parameter: parameter.to_string(),
        unit: String::new(),
        parameters: Vec::new(),
        vertical: None,
        grid_size: None,
        layer_subtitle: None,
        reference_times: Vec::new(),
    }
}

impl CapArea {
    /// True when the area has geocodes but no renderable polygon/circle.
    fn has_geocode_only(&self) -> bool {
        self.polygons.is_empty() && self.circles.is_empty() && !self.geocodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_document;

    fn cfg() -> BuildConfig {
        BuildConfig {
            language: None,
            status_filter: vec!["actual".to_string()],
            default_ttl: None,
            circle_segments: 16,
        }
    }

    fn at(y: i64, mo: u32, d: u32, h: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(y as i32, mo, d, h, 0, 0).unwrap()
    }

    const DOC: &str = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
      <identifier>A1</identifier><status>Actual</status><sent>2026-06-15T09:00:00Z</sent>
      <info><language>en</language><event>Flood</event><severity>Severe</severity>
        <onset>2026-06-15T10:00:00Z</onset><expires>2026-06-15T16:00:00Z</expires>
        <area><areaDesc>County</areaDesc><polygon>60,24 60,25 61,25 61,24 60,24</polygon></area>
      </info></alert>"#;

    #[test]
    fn builds_record_with_swapped_geometry() {
        let alerts = parse_document(DOC).unwrap();
        let cat = Catalog::build(&alerts, &cfg(), "cap", "severity", at(2026, 6, 15, 12));
        assert_eq!(cat.records.len(), 1);
        let r = &cat.records[0];
        assert_eq!(r.id, "A1.0.0");
        assert_eq!(r.severity_code, 3.0);
        // Geometry is in [lon, lat]: bbox west≈24, south≈60.
        let b = r.bbox.unwrap();
        assert!((b[0] - 24.0).abs() < 1e-9 && (b[1] - 60.0).abs() < 1e-9);
        assert!(r.window.active_at(at(2026, 6, 15, 12)));
        assert!(!r.window.active_at(at(2026, 6, 15, 17)));
    }

    #[test]
    fn status_filter_drops_test_alerts() {
        let doc = DOC.replace("<status>Actual</status>", "<status>Test</status>");
        let alerts = parse_document(&doc).unwrap();
        let cat = Catalog::build(&alerts, &cfg(), "cap", "severity", at(2026, 6, 15, 12));
        assert!(cat.records.is_empty());
    }

    #[test]
    fn language_filter_selects_one_info() {
        let doc = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
          <identifier>L1</identifier><status>Actual</status>
          <info><language>en-US</language><event>Heat</event>
            <area><areaDesc>A</areaDesc><polygon>0,0 0,1 1,1 1,0 0,0</polygon></area></info>
          <info><language>fr-FR</language><event>Chaleur</event>
            <area><areaDesc>A</areaDesc><polygon>0,0 0,1 1,1 1,0 0,0</polygon></area></info>
        </alert>"#;
        let alerts = parse_document(doc).unwrap();
        let mut c = cfg();
        c.language = Some("en".to_string());
        let cat = Catalog::build(&alerts, &c, "cap", "severity", at(2026, 6, 15, 12));
        assert_eq!(cat.records.len(), 1);
        assert_eq!(cat.records[0].id, "L1.0.0"); // first (en) info index preserved
    }

    #[test]
    fn geocode_only_area_is_null_geometry_and_counted() {
        let doc = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
          <identifier>G1</identifier><status>Actual</status>
          <info><event>Wind</event>
            <area><areaDesc>Zone</areaDesc>
              <geocode><valueName>UGC</valueName><value>FIC001</value></geocode>
            </area></info></alert>"#;
        let alerts = parse_document(doc).unwrap();
        let cat = Catalog::build(&alerts, &cfg(), "cap", "severity", at(2026, 6, 15, 12));
        assert_eq!(cat.records.len(), 1);
        assert!(matches!(&*cat.records[0].geometry, Geometry::Null));
        assert!(cat.records[0].bbox.is_none());
        assert_eq!(cat.geocode_only_count, 1);
    }

    #[test]
    fn circle_becomes_polygon_with_radius_property() {
        let doc = r#"<alert xmlns="urn:oasis:names:tc:emergency:cap:1.2">
          <identifier>C1</identifier><status>Actual</status>
          <info><event>Ash</event><severity>Extreme</severity>
            <area><areaDesc>Ring</areaDesc><circle>60.0,24.0 10.0</circle></area></info></alert>"#;
        let alerts = parse_document(doc).unwrap();
        let cat = Catalog::build(&alerts, &cfg(), "cap", "severity", at(2026, 6, 15, 12));
        let r = &cat.records[0];
        assert!(matches!(&*r.geometry, Geometry::Polygon { .. }));
        assert_eq!(
            r.properties.get("radius_km"),
            Some(&PropertyValue::Float(10.0))
        );
        // Window has no times → open-ended → always active.
        assert!(r.window.active_at(at(2000, 1, 1, 0)));
    }

    #[test]
    fn default_ttl_closes_open_expiry() {
        let doc = DOC.replace("<expires>2026-06-15T16:00:00Z</expires>", "");
        let alerts = parse_document(&doc).unwrap();
        let mut c = cfg();
        c.default_ttl = Some(Duration::hours(2));
        let cat = Catalog::build(&alerts, &c, "cap", "severity", at(2026, 6, 15, 12));
        let r = &cat.records[0];
        // onset 10:00 + 2h = 12:00 end.
        assert!(r.window.active_at(at(2026, 6, 15, 11)));
        assert!(!r.window.active_at(at(2026, 6, 15, 13)));
    }

    #[test]
    fn times_end_at_as_of_for_now_default() {
        let alerts = parse_document(DOC).unwrap();
        let as_of = at(2026, 6, 15, 12);
        let cat = Catalog::build(&alerts, &cfg(), "cap", "severity", as_of);
        assert_eq!(cat.info.times.last(), Some(&as_of));
        // The future expiry (16:00) is excluded (> as_of) so as_of stays the max.
        assert!(cat.info.times.iter().all(|&t| t <= as_of));
    }
}
