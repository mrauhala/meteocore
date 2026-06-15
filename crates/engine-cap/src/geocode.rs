//! Optional geocode → geometry resolution.
//!
//! Many CAP feeds (MeteoAlarm in particular) describe an alert `<area>` only by
//! a `<geocode>` zone code (EMMA_ID, UGC, FIPS, …) with **no** `<polygon>` /
//! `<circle>` — so the area has no renderable geometry on its own. This module
//! loads an operator-supplied GeoJSON of zone polygons (code → geometry) so those
//! geocode-only areas resolve to real geometry for both Features and the map.
//!
//! The lookup file is a GeoJSON `FeatureCollection`; each feature's
//! `properties[<geocode_property>]` is the zone code (e.g. `"FI801"`) and its
//! geometry is the zone `Polygon`/`MultiPolygon` (GeoJSON `[lon, lat]`, no swap).

use std::collections::HashMap;
use std::sync::Arc;

use ds_core::error::DataServerError;
use ds_core::feature::Geometry;

/// Cap on the geocode-lookup GeoJSON file size (trusted reference data, but
/// bounded so a misconfigured path can't OOM the loader).
const MAX_LOOKUP_BYTES: u64 = 256 * 1024 * 1024;
/// Cap on coordinates per zone geometry (geometry-bomb guard).
const MAX_COORDS_PER_GEOMETRY: usize = 1_000_000;

/// A loaded zone-code → geometry table.
#[derive(Debug)]
pub struct GeocodeLookup {
    by_code: HashMap<String, Arc<Geometry>>,
    /// Only resolve CAP `<geocode>` entries whose `<valueName>` matches this
    /// (case-insensitive); `None` = resolve against any geocode value.
    value_name: Option<String>,
}

impl GeocodeLookup {
    /// Load a GeoJSON `FeatureCollection` of zone polygons. `property` is the
    /// feature property holding the zone code; `value_name` optionally restricts
    /// which CAP `<geocode>` `valueName` is resolved (e.g. `"EMMA_ID"`).
    pub fn load(
        path: &str,
        property: &str,
        value_name: Option<&str>,
    ) -> Result<Self, DataServerError> {
        let meta = std::fs::metadata(path)
            .map_err(|e| DataServerError::Config(format!("cap geocode_geometry '{path}': {e}")))?;
        if meta.len() > MAX_LOOKUP_BYTES {
            return Err(DataServerError::Config(format!(
                "cap geocode_geometry '{path}' is {} bytes — exceeds the {MAX_LOOKUP_BYTES}-byte limit",
                meta.len()
            )));
        }
        let bytes = std::fs::read(path)
            .map_err(|e| DataServerError::Config(format!("cap geocode_geometry '{path}': {e}")))?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            DataServerError::Config(format!(
                "cap geocode_geometry '{path}' is not valid JSON: {e}"
            ))
        })?;
        let features = json
            .get("features")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                DataServerError::Config(format!(
                "cap geocode_geometry '{path}' is not a GeoJSON FeatureCollection (no 'features')"
            ))
            })?;

        let mut by_code: HashMap<String, Arc<Geometry>> = HashMap::new();
        for feat in features {
            let code = feat
                .get("properties")
                .and_then(|p| p.get(property))
                .and_then(value_as_string);
            let geom = feat.get("geometry").and_then(parse_geometry);
            if let (Some(code), Some(geom)) = (code, geom) {
                by_code.insert(code, Arc::new(geom));
            }
        }
        if by_code.is_empty() {
            return Err(DataServerError::Config(format!(
                "cap geocode_geometry '{path}': no features with a '{property}' code and a \
                 Polygon/MultiPolygon geometry were found"
            )));
        }
        Ok(Self {
            by_code,
            value_name: value_name.map(str::to_string),
        })
    }

    /// Number of loaded zones (for logging).
    pub fn len(&self) -> usize {
        self.by_code.len()
    }

    /// Resolve an area's `(valueName, value)` geocodes to zone geometries. A code
    /// may resolve to a `Polygon` or `MultiPolygon`; the caller merges the
    /// results into the area's geometry.
    pub fn resolve(&self, geocodes: &[(String, String)]) -> Vec<Arc<Geometry>> {
        geocodes
            .iter()
            .filter(|(vn, _)| {
                self.value_name
                    .as_deref()
                    .is_none_or(|want| want.eq_ignore_ascii_case(vn))
            })
            .filter_map(|(_, value)| self.by_code.get(value).cloned())
            .collect()
    }
}

/// A GeoJSON property value as a code string (string verbatim; an integer number
/// rendered without a decimal point so `123` matches `"123"`).
fn value_as_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i.to_string())
            } else {
                Some(n.to_string())
            }
        }
        _ => None,
    }
}

/// Parse a GeoJSON `Polygon`/`MultiPolygon` geometry into [`Geometry`]
/// (`[lon, lat]`, no swap — GeoJSON is already lon-first). Other types → `None`.
fn parse_geometry(g: &serde_json::Value) -> Option<Geometry> {
    let mut coord_budget = MAX_COORDS_PER_GEOMETRY;
    match g.get("type").and_then(|t| t.as_str())? {
        "Polygon" => {
            let rings = g.get("coordinates")?.as_array()?;
            let (exterior, holes) = parse_rings(rings, &mut coord_budget)?;
            Some(Geometry::Polygon { exterior, holes })
        }
        "MultiPolygon" => {
            let polys_raw = g.get("coordinates")?.as_array()?;
            let mut polygons = Vec::with_capacity(polys_raw.len());
            for poly in polys_raw {
                let rings = poly.as_array()?;
                polygons.push(parse_rings(rings, &mut coord_budget)?);
            }
            Some(Geometry::MultiPolygon { polygons })
        }
        _ => None,
    }
}

#[allow(clippy::type_complexity)]
fn parse_rings(
    rings: &[serde_json::Value],
    budget: &mut usize,
) -> Option<(Vec<[f64; 2]>, Vec<Vec<[f64; 2]>>)> {
    let exterior = parse_ring(rings.first()?, budget)?;
    let mut holes = Vec::new();
    for ring in &rings[1..] {
        holes.push(parse_ring(ring, budget)?);
    }
    Some((exterior, holes))
}

fn parse_ring(ring: &serde_json::Value, budget: &mut usize) -> Option<Vec<[f64; 2]>> {
    let coords = ring.as_array()?;
    let mut out = Vec::with_capacity(coords.len());
    for pair in coords {
        *budget = budget.checked_sub(1)?; // bail if the geometry is too large
        let p = pair.as_array()?;
        let lon = p.first()?.as_f64()?;
        let lat = p.get(1)?.as_f64()?;
        if !lon.is_finite() || !lat.is_finite() {
            return None;
        }
        out.push([lon, lat]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(json: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    const FC: &str = r#"{
      "type": "FeatureCollection",
      "features": [
        {"type":"Feature","properties":{"code":"FI801","type":"EMMA_ID"},
         "geometry":{"type":"Polygon","coordinates":[[[24.0,60.0],[25.0,60.0],[25.0,61.0],[24.0,61.0],[24.0,60.0]]]}},
        {"type":"Feature","properties":{"code":"FI802","type":"EMMA_ID"},
         "geometry":{"type":"MultiPolygon","coordinates":[[[[20.0,59.0],[21.0,59.0],[21.0,60.0],[20.0,60.0],[20.0,59.0]]]]}}
      ]
    }"#;

    #[test]
    fn loads_and_resolves_by_code() {
        let f = write(FC);
        let lk = GeocodeLookup::load(f.path().to_str().unwrap(), "code", Some("EMMA_ID")).unwrap();
        assert_eq!(lk.len(), 2);
        // EMMA_ID geocode resolves to the polygon.
        let g = lk.resolve(&[("EMMA_ID".into(), "FI801".into())]);
        assert_eq!(g.len(), 1);
        assert!(matches!(&*g[0], Geometry::Polygon { .. }));
        // A non-matching valueName is skipped even if the value exists.
        assert!(lk.resolve(&[("UGC".into(), "FI801".into())]).is_empty());
        // An unknown code resolves to nothing.
        assert!(lk.resolve(&[("EMMA_ID".into(), "ZZ999".into())]).is_empty());
        // Multiple geocodes → multiple geometries.
        assert_eq!(
            lk.resolve(&[
                ("EMMA_ID".into(), "FI801".into()),
                ("EMMA_ID".into(), "FI802".into())
            ])
            .len(),
            2
        );
    }

    #[test]
    fn value_name_none_matches_any() {
        let f = write(FC);
        let lk = GeocodeLookup::load(f.path().to_str().unwrap(), "code", None).unwrap();
        assert_eq!(lk.resolve(&[("WHATEVER".into(), "FI801".into())]).len(), 1);
    }

    #[test]
    fn rejects_empty_or_geometryless() {
        let f = write(r#"{"type":"FeatureCollection","features":[]}"#);
        assert!(GeocodeLookup::load(f.path().to_str().unwrap(), "code", None).is_err());
        let f2 = write(r#"{"type":"Feature"}"#);
        assert!(GeocodeLookup::load(f2.path().to_str().unwrap(), "code", None).is_err());
    }
}
