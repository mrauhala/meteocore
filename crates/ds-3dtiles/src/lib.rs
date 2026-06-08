//! OGC 3D Tiles encoder: turns a [`ds_core::volume::VolumePointCloud`] into a
//! `.pnts` point-cloud tile plus a `tileset.json`. Framework-free (only
//! `ds-core` + `ds-render`), mirroring how `ds-render` encodes PNG and `ds-mvt`
//! encodes vector tiles — engines produce the domain type, this crate turns it
//! into bytes.
//!
//! The hard-won invariants from the Phase-1 demo (#347) are enforced here so
//! every caller gets them for free:
//!
//! - The tileset's top-level `geometricError` is **> 0** — otherwise CesiumJS
//!   never refines to the root and never even requests the content tile.
//! - `.pnts` `POSITION` is **ECEF-native** (offsets from `RTC_CENTER`); there
//!   is no glTF Y-up→Z-up flip.
//! - The `region` bounding volume is geodetic (EPSG:4979) and ignores any tile
//!   transform.
//! - `u32` header fields are bounds-checked, and **non-finite geometry is
//!   rejected** rather than silently emitted as `NaN`/`Infinity` (invalid JSON
//!   / a corrupt tile that loads as nothing).

use ds_core::volume::VolumePointCloud;
use ds_render::ColorMap;
use serde_json::json;

/// Top-level tileset geometric error. Must be > 0 (see module docs); the value
/// only needs to exceed the root's so CesiumJS refines to the content.
const TILESET_GEOMETRIC_ERROR: f64 = 1.0e5;
/// Leaf (root content) geometric error. Also > 0.
const ROOT_GEOMETRIC_ERROR: f64 = 1.0e3;

/// Errors from encoding a [`VolumePointCloud`].
#[derive(Debug, thiserror::Error)]
pub enum Tiles3dError {
    /// A coordinate that must be finite (an RTC-center component, a point
    /// offset, or a region bound) was `NaN`/±∞. Encoding refuses rather than
    /// emit a corrupt tile / invalid JSON.
    #[error("non-finite value in {0}")]
    NonFinite(&'static str),
    /// A `.pnts` header field (each a `u32`) would overflow — the tile exceeds
    /// 4 GiB. Fail loudly instead of truncating to a corrupt file.
    #[error("pnts {0} exceeds u32 range (4 GiB)")]
    TooLarge(&'static str),
    /// The point cloud is empty. The 3D Tiles 1.0 Point Cloud spec requires
    /// `POINTS_LENGTH >= 1`; `VolumeEngine` maps "no data" to a 404 before
    /// encoding, so an empty cloud reaching here is a caller bug.
    #[error("cannot encode an empty point cloud (POINTS_LENGTH must be >= 1)")]
    Empty,
}

/// Encode a point cloud as a 3D Tiles **`.pnts`** tile (the 3D Tiles 1.0 Point
/// Cloud format, still rendered by CesiumJS in 1.1 — the path where per-point
/// RGB and `pointSize` styling actually work).
///
/// `colormap` maps each point's physical value to RGB (alpha is ignored;
/// `.pnts` `RGB` is opaque). Returns the complete tile bytes.
///
/// `cloud.points` must be non-empty — the 3D Tiles 1.0 Point Cloud format
/// requires `POINTS_LENGTH >= 1`. An empty cloud is rejected with
/// [`Tiles3dError::Empty`]; `VolumeEngine` callers already map "no data" to a
/// 404 before reaching here.
pub fn encode_pnts(
    cloud: &VolumePointCloud,
    colormap: &dyn ColorMap,
) -> Result<Vec<u8>, Tiles3dError> {
    if cloud.rtc_center.iter().any(|c| !c.is_finite()) {
        return Err(Tiles3dError::NonFinite("rtc_center"));
    }

    let count = cloud.points.len();
    if count == 0 {
        return Err(Tiles3dError::Empty);
    }

    // Feature-table binary: POSITION (count * 3 * f32) then RGB (count * 3 * u8).
    let pos_bytes = count * 12;
    let rgb_off = pos_bytes;
    let mut body = Vec::with_capacity(pos_bytes + count * 3);
    for p in &cloud.points {
        for v in p.offset {
            if !v.is_finite() {
                return Err(Tiles3dError::NonFinite("point offset"));
            }
            body.extend_from_slice(&v.to_le_bytes());
        }
    }
    for p in &cloud.points {
        let c = colormap.color(Some(p.value));
        body.extend_from_slice(&[c[0], c[1], c[2]]);
    }
    // The feature-table binary must end on an 8-byte boundary.
    while !body.len().is_multiple_of(8) {
        body.push(0);
    }

    // Feature-table JSON. rtc_center is finite (checked above); serde_json
    // would otherwise serialize a non-finite f64 as `null` silently.
    let [cx, cy, cz] = cloud.rtc_center;
    let ft = json!({
        "POINTS_LENGTH": count,
        "RTC_CENTER": [cx, cy, cz],
        "POSITION": { "byteOffset": 0 },
        "RGB": { "byteOffset": rgb_off },
    });
    let mut ft_json = serde_json::to_vec(&ft).expect("feature table serializes");
    while !ft_json.len().is_multiple_of(8) {
        ft_json.push(b' ');
    }

    const HEADER_LEN: usize = 28;
    let total = HEADER_LEN + ft_json.len() + body.len();
    let total = u32::try_from(total).map_err(|_| Tiles3dError::TooLarge("byteLength"))?;
    let ft_json_len =
        u32::try_from(ft_json.len()).map_err(|_| Tiles3dError::TooLarge("featureTableJSON"))?;
    let body_len =
        u32::try_from(body.len()).map_err(|_| Tiles3dError::TooLarge("featureTableBinary"))?;

    let mut pnts = Vec::with_capacity(total as usize);
    pnts.extend_from_slice(b"pnts");
    pnts.extend_from_slice(&1u32.to_le_bytes()); // version
    pnts.extend_from_slice(&total.to_le_bytes());
    pnts.extend_from_slice(&ft_json_len.to_le_bytes());
    pnts.extend_from_slice(&body_len.to_le_bytes());
    pnts.extend_from_slice(&0u32.to_le_bytes()); // batch-table JSON length
    pnts.extend_from_slice(&0u32.to_le_bytes()); // batch-table binary length
    pnts.extend_from_slice(&ft_json);
    pnts.extend_from_slice(&body);
    Ok(pnts)
}

/// Build the `tileset.json` for a point cloud. `content_uri` is the relative
/// URI of the `.pnts` tile (e.g. `"content.pnts"`).
///
/// The `region` bounding volume is geodetic, so no tile `transform` is needed
/// (the `.pnts` `RTC_CENTER` already places points in ECEF). The top-level
/// `geometricError` is non-zero (load-bearing — see module docs).
pub fn tileset_json(cloud: &VolumePointCloud, content_uri: &str) -> Result<String, Tiles3dError> {
    if cloud.region.iter().any(|v| !v.is_finite()) {
        return Err(Tiles3dError::NonFinite("region"));
    }
    let region: Vec<f64> = cloud.region.to_vec();
    // `asset.version` is intentionally "1.1" even though `.pnts` is a 1.0
    // content type: CesiumJS keys its tileset loader off this version, and a
    // 1.1 tileset happily references legacy `.pnts` content. Do not "fix" this
    // to "1.0" — it would change which loader path CesiumJS takes.
    let tileset = json!({
        "asset": { "version": "1.1" },
        "geometricError": TILESET_GEOMETRIC_ERROR,
        "root": {
            "boundingVolume": { "region": region },
            "geometricError": ROOT_GEOMETRIC_ERROR,
            "refine": "ADD",
            "content": { "uri": content_uri },
        }
    });
    Ok(serde_json::to_string_pretty(&tileset).expect("tileset serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::volume::{VolumePoint, VolumePointCloud};
    use ds_render::{BuiltinColormap, LutColorMap};

    fn sample_cloud() -> VolumePointCloud {
        VolumePointCloud {
            rtc_center: [3_000_000.0, 1_000_000.0, 5_000_000.0],
            region: [0.42, 1.05, 0.44, 1.07, 100.0, 12_000.0],
            points: vec![
                VolumePoint {
                    offset: [0.0, 0.0, 0.0],
                    value: 10.0,
                },
                VolumePoint {
                    offset: [100.0, -50.0, 250.0],
                    value: 45.0,
                },
                VolumePoint {
                    offset: [-200.0, 75.0, 1000.0],
                    value: 60.0,
                },
            ],
            quantity: "DBZH".into(),
            unit: "dBZ".into(),
        }
    }

    fn dbz_map() -> LutColorMap {
        LutColorMap::from_builtin(BuiltinColormap::RadarDbz, -32.0, 95.0)
    }

    #[test]
    fn pnts_header_is_wellformed() {
        let cloud = sample_cloud();
        let bytes = encode_pnts(&cloud, &dbz_map()).expect("encode");

        assert_eq!(&bytes[0..4], b"pnts", "magic");
        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        assert_eq!(u32_at(4), 1, "version");
        assert_eq!(
            u32_at(8) as usize,
            bytes.len(),
            "byteLength == actual length"
        );
        let ft_json_len = u32_at(12) as usize;
        let ft_bin_len = u32_at(16) as usize;
        assert_eq!(u32_at(20), 0, "no batch-table JSON");
        assert_eq!(u32_at(24), 0, "no batch-table binary");
        // Sections are 8-byte aligned, and the three lengths tile the file.
        assert_eq!(ft_json_len % 8, 0);
        assert_eq!(ft_bin_len % 8, 0);
        assert_eq!(28 + ft_json_len + ft_bin_len, bytes.len());

        // Feature-table JSON parses and carries the right point count + RTC.
        let ft: serde_json::Value =
            serde_json::from_slice(&bytes[28..28 + ft_json_len]).expect("FT JSON parses");
        assert_eq!(ft["POINTS_LENGTH"], 3);
        assert_eq!(ft["RTC_CENTER"][0], 3_000_000.0);
        // RGB starts after the 3 positions (3 * 12 = 36 bytes).
        assert_eq!(ft["RGB"]["byteOffset"], 36);
    }

    #[test]
    fn tileset_has_positive_geometric_error_and_region() {
        let cloud = sample_cloud();
        let json: serde_json::Value =
            serde_json::from_str(&tileset_json(&cloud, "content.pnts").expect("tileset")).unwrap();
        assert!(
            json["geometricError"].as_f64().unwrap() > 0.0,
            "load-bearing"
        );
        assert!(json["root"]["geometricError"].as_f64().unwrap() > 0.0);
        assert_eq!(json["root"]["content"]["uri"], "content.pnts");
        let region = json["root"]["boundingVolume"]["region"].as_array().unwrap();
        assert_eq!(region.len(), 6);
        assert_eq!(region[0].as_f64().unwrap(), 0.42);
    }

    #[test]
    fn non_finite_geometry_is_rejected() {
        let mut cloud = sample_cloud();
        cloud.rtc_center[1] = f64::NAN;
        assert!(matches!(
            encode_pnts(&cloud, &dbz_map()),
            Err(Tiles3dError::NonFinite("rtc_center"))
        ));

        let mut cloud = sample_cloud();
        cloud.region[3] = f64::INFINITY;
        assert!(matches!(
            tileset_json(&cloud, "content.pnts"),
            Err(Tiles3dError::NonFinite("region"))
        ));
    }

    #[test]
    fn empty_cloud_is_rejected() {
        // The 3D Tiles 1.0 Point Cloud spec requires POINTS_LENGTH >= 1.
        let mut cloud = sample_cloud();
        cloud.points.clear();
        assert!(matches!(
            encode_pnts(&cloud, &dbz_map()),
            Err(Tiles3dError::Empty)
        ));
    }
}
