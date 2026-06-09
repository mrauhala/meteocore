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

/// Isosurface meshing of a [`ds_core::volume::VoxelGrid`] into glTF `.glb`
/// triangle-mesh content (#357) — the verifiable, any-client 3-D path next to
/// the `.pnts` point cloud.
pub mod isosurface;
pub use isosurface::{encode_isosurface_glb, tileset_json_glb};

/// Echo-top-height (ETH) draped-surface meshing of a
/// [`ds_core::volume::VoxelGrid`] into a height-coloured glTF `.glb` (#362).
pub mod echo_top;
pub use echo_top::{encode_echo_top_columns_glb, encode_echo_top_glb};

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
    /// `content_uri` is not a safe server-relative path (empty, absolute,
    /// scheme-qualified, or containing a `..` segment). CesiumJS fetches
    /// whatever URL the tileset names, so the encoder rejects anything that
    /// could traverse or redirect — defence-in-depth for a `pub` API.
    #[error("invalid content URI: {0:?}")]
    InvalidUri(String),
    /// The geodetic `region` has inverted/degenerate bounds (`south > north`
    /// or `min_h > max_h`) — valid JSON that CesiumJS silently refuses to load.
    /// (`west > east` is *not* inverted — it's an antimeridian-crossing region.)
    #[error("invalid region (inverted/degenerate bounds): {0:?}")]
    InvalidRegion([f64; 6]),
    /// The isosurface `background` (the value `NaN`/clear-air cells are sealed
    /// to) is not strictly below the `threshold`. With `background >= threshold`,
    /// unmeasured cells would be treated as *inside* the surface, inverting it
    /// into something with no physical meaning. Rejected rather than silently
    /// returning a bogus mesh.
    #[error("isosurface background {background} must be < threshold {threshold}")]
    BackgroundNotBelowThreshold { background: f64, threshold: f64 },
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
    // `POINTS_LENGTH` is a u32 in the feature table; reject before the body
    // build (the byte-length guards below would only catch this at ~4 GiB).
    let points_len = u32::try_from(count).map_err(|_| Tiles3dError::TooLarge("POINTS_LENGTH"))?;

    // Front-load the byte-budget check too: reject an oversized cloud *before*
    // allocating (POSITION 12 B + RGB 3 B + batch-table value 4 B per point, plus
    // the fixed overhead), so a caller bypassing the engine's MAX_POINTS cap can't
    // force a multi-GB allocation that only errors afterward. The `+128` covers
    // the fixed overhead conservatively (HEADER 28 + the two JSON blobs + ≤4
    // section alignments); the authoritative guard is the `u32::try_from(total)`
    // on the assembled size below, so this only needs to be in the ballpark.
    if count
        .checked_mul(19)
        .and_then(|n| n.checked_add(128))
        .is_none_or(|n| u32::try_from(n).is_err())
    {
        // Distinct label from the post-build check below so the two guards are
        // distinguishable in logs.
        return Err(Tiles3dError::TooLarge("point data size"));
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
        "POINTS_LENGTH": points_len,
        "RTC_CENTER": [cx, cy, cz],
        "POSITION": { "byteOffset": 0 },
        "RGB": { "byteOffset": rgb_off },
    });
    let mut ft_json = serde_json::to_vec(&ft).expect("feature table serializes");
    while !ft_json.len().is_multiple_of(8) {
        ft_json.push(b' ');
    }

    // Batch table: one per-point `value` (the physical value, e.g. dBZ). No
    // `BATCH_ID` in the feature table ⇒ the batch table holds POINTS_LENGTH
    // per-point properties (one per point), which CesiumJS exposes to the style
    // engine as `${value}` — driving client-side point sizing AND filtering.
    // (A `BATCH_ID` would make points pickable features but disables per-point
    // `pointSize` styling in CesiumJS's point-cloud pipeline — not worth it.)
    let bt = json!({
        "value": { "byteOffset": 0, "componentType": "FLOAT", "type": "SCALAR" },
    });
    let mut bt_json = serde_json::to_vec(&bt).expect("batch table serializes");
    while !bt_json.len().is_multiple_of(8) {
        bt_json.push(b' ');
    }
    let mut bt_bin = Vec::with_capacity(count * 4);
    for p in &cloud.points {
        // Points come from real (finite) echoes; guard anyway so a stray NaN
        // can't reach the buffer as a non-finite property.
        let v = if p.value.is_finite() {
            p.value as f32
        } else {
            0.0
        };
        bt_bin.extend_from_slice(&v.to_le_bytes());
    }
    // The batch-table binary must also end on an 8-byte boundary.
    while !bt_bin.len().is_multiple_of(8) {
        bt_bin.push(0);
    }

    const HEADER_LEN: usize = 28;
    let total = HEADER_LEN + ft_json.len() + body.len() + bt_json.len() + bt_bin.len();
    let total = u32::try_from(total).map_err(|_| Tiles3dError::TooLarge("byteLength"))?;
    let ft_json_len =
        u32::try_from(ft_json.len()).map_err(|_| Tiles3dError::TooLarge("featureTableJSON"))?;
    let body_len =
        u32::try_from(body.len()).map_err(|_| Tiles3dError::TooLarge("featureTableBinary"))?;
    let bt_json_len =
        u32::try_from(bt_json.len()).map_err(|_| Tiles3dError::TooLarge("batchTableJSON"))?;
    let bt_bin_len =
        u32::try_from(bt_bin.len()).map_err(|_| Tiles3dError::TooLarge("batchTableBinary"))?;

    let mut pnts = Vec::with_capacity(total as usize);
    pnts.extend_from_slice(b"pnts");
    pnts.extend_from_slice(&1u32.to_le_bytes()); // version
    pnts.extend_from_slice(&total.to_le_bytes());
    pnts.extend_from_slice(&ft_json_len.to_le_bytes());
    pnts.extend_from_slice(&body_len.to_le_bytes());
    pnts.extend_from_slice(&bt_json_len.to_le_bytes());
    pnts.extend_from_slice(&bt_bin_len.to_le_bytes());
    pnts.extend_from_slice(&ft_json);
    pnts.extend_from_slice(&body);
    pnts.extend_from_slice(&bt_json);
    pnts.extend_from_slice(&bt_bin);
    Ok(pnts)
}

/// Build the `tileset.json` for a point cloud. `content_uri` is the relative
/// URI of the `.pnts` tile (e.g. `"content.pnts"`). CesiumJS fetches whatever
/// URL it resolves to, so the encoder **validates** it as a safe server-relative
/// path — rejecting empty, absolute (`/…`), scheme-qualified (`…://…`), and
/// `..`-traversing values with [`Tiles3dError::InvalidUri`] — rather than
/// relying on the caller (defence-in-depth for a `pub` API).
///
/// The `region` bounding volume is geodetic, so no tile `transform` is needed
/// (the `.pnts` `RTC_CENTER` already places points in ECEF). The top-level
/// `geometricError` is non-zero (load-bearing — see module docs).
pub fn tileset_json(cloud: &VolumePointCloud, content_uri: &str) -> Result<String, Tiles3dError> {
    tileset_json_for_region(cloud.region, content_uri)
}

/// Build the `tileset.json` from a geodetic bounding `region`
/// (`[west, south, east, north, min_h, max_h]`, lon/lat radians + metres)
/// directly — for the API layer, which has the collection's coverage region
/// from `VolumeInfo` and need not sample the whole volume just to emit the
/// tileset. The region must merely *contain* the content the `.pnts` will hold.
/// Same `content_uri` validation and invariants as [`tileset_json`].
pub fn tileset_json_for_region(
    region: [f64; 6],
    content_uri: &str,
) -> Result<String, Tiles3dError> {
    let tileset = tileset_value_for_region(region, content_uri)?;
    Ok(serde_json::to_string_pretty(&tileset).expect("tileset serializes"))
}

/// Build the `tileset.json` as a [`serde_json::Value`] (the validation +
/// construction shared by [`tileset_json_for_region`] and the glTF variant
/// `isosurface::tileset_json_glb`, which injects a `transform` — so it builds
/// on this `Value` directly instead of re-parsing a serialized string).
pub(crate) fn tileset_value_for_region(
    region: [f64; 6],
    content_uri: &str,
) -> Result<serde_json::Value, Tiles3dError> {
    if content_uri.is_empty()
        || content_uri.starts_with('/')
        || content_uri.contains("://")
        || content_uri.split('/').any(|s| s == "..")
    {
        return Err(Tiles3dError::InvalidUri(content_uri.to_string()));
    }
    if region.iter().any(|v| !v.is_finite()) {
        return Err(Tiles3dError::NonFinite("region"));
    }
    // Reject truly inverted/degenerate bounds (south > north, min_h > max_h) —
    // valid JSON that CesiumJS silently refuses to load. Note `west > east` is
    // NOT inverted: 3D Tiles 1.1 uses it for antimeridian-crossing regions.
    if region[1] > region[3] || region[4] > region[5] {
        return Err(Tiles3dError::InvalidRegion(region));
    }
    // `[f64; 6]` serializes directly to a JSON array — no `Vec` needed.
    // `asset.version` is intentionally "1.1" even though `.pnts` is a 1.0
    // content type: CesiumJS keys its tileset loader off this version, and a
    // 1.1 tileset happily references legacy `.pnts` content. Do not "fix" this
    // to "1.0" — it would change which loader path CesiumJS takes.
    Ok(json!({
        "asset": { "version": "1.1" },
        "geometricError": TILESET_GEOMETRIC_ERROR,
        "root": {
            "boundingVolume": { "region": region },
            "geometricError": ROOT_GEOMETRIC_ERROR,
            "refine": "ADD",
            "content": { "uri": content_uri },
        }
    }))
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
        let bt_json_len = u32_at(20) as usize;
        let bt_bin_len = u32_at(24) as usize;
        // A per-point `value` batch table is present (for client point sizing):
        // 3 f32 values = 12 B, padded to the next 8-byte boundary → 16 B.
        assert!(bt_json_len > 0, "batch-table JSON present");
        assert_eq!(bt_bin_len, 16, "3 f32 values (12 B) padded to 16");
        // All four sections are 8-byte aligned and tile the file exactly.
        assert_eq!(ft_json_len % 8, 0);
        assert_eq!(ft_bin_len % 8, 0);
        assert_eq!(bt_json_len % 8, 0);
        assert_eq!(bt_bin_len % 8, 0);
        assert_eq!(
            28 + ft_json_len + ft_bin_len + bt_json_len + bt_bin_len,
            bytes.len()
        );

        // Feature-table JSON parses and carries the right point count + RTC.
        let ft: serde_json::Value =
            serde_json::from_slice(&bytes[28..28 + ft_json_len]).expect("FT JSON parses");
        assert_eq!(ft["POINTS_LENGTH"], 3);
        assert_eq!(ft["RTC_CENTER"][0], 3_000_000.0);
        // RGB starts after the 3 positions (3 * 12 = 36 bytes).
        assert_eq!(ft["RGB"]["byteOffset"], 36);
        // No BATCH_ID: per-point `value` reaches the style engine (sizing +
        // filtering) without promoting points to features (which would disable
        // per-point pointSize in CesiumJS).
        assert!(
            ft.get("BATCH_ID").is_none(),
            "no BATCH_ID (keeps pointSize)"
        );

        // Batch table declares the per-point `value` (FLOAT SCALAR), and its
        // binary carries the three sample values (10, 45, 60).
        let bt_start = 28 + ft_json_len + ft_bin_len;
        let bt: serde_json::Value =
            serde_json::from_slice(&bytes[bt_start..bt_start + bt_json_len])
                .expect("BT JSON parses");
        assert_eq!(bt["value"]["componentType"], "FLOAT");
        let vbin = &bytes[bt_start + bt_json_len..bt_start + bt_json_len + bt_bin_len];
        let v0 = f32::from_le_bytes(vbin[0..4].try_into().unwrap());
        let v1 = f32::from_le_bytes(vbin[4..8].try_into().unwrap());
        let v2 = f32::from_le_bytes(vbin[8..12].try_into().unwrap());
        assert_eq!(v0, 10.0);
        assert_eq!(v1, 45.0);
        assert_eq!(v2, 60.0); // all three written — a truncated loop would fail here
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
    fn inverted_region_is_rejected() {
        // south > north is inverted.
        let region = [0.42, 1.07, 0.44, 1.05, 100.0, 12_000.0];
        assert!(matches!(
            tileset_json_for_region(region, "content.pnts"),
            Err(Tiles3dError::InvalidRegion(_))
        ));
        // min_h > max_h is inverted.
        let region = [0.42, 1.05, 0.44, 1.07, 12_000.0, 100.0];
        assert!(matches!(
            tileset_json_for_region(region, "content.pnts"),
            Err(Tiles3dError::InvalidRegion(_))
        ));
        // west > east is VALID — an antimeridian-crossing region (not inverted).
        let region = [3.0, 1.05, -3.0, 1.07, 100.0, 12_000.0];
        assert!(tileset_json_for_region(region, "content.pnts").is_ok());
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

        // A non-finite point offset is caught in the encode loop.
        let mut cloud = sample_cloud();
        cloud.points[1].offset[2] = f32::NAN;
        assert!(matches!(
            encode_pnts(&cloud, &dbz_map()),
            Err(Tiles3dError::NonFinite("point offset"))
        ));
    }

    #[test]
    fn unsafe_content_uri_is_rejected() {
        let cloud = sample_cloud();
        for bad in [
            "",
            "/abs.pnts",
            "http://evil/x.pnts",
            "../../secret",
            "a/../b.pnts",
        ] {
            assert!(
                matches!(tileset_json(&cloud, bad), Err(Tiles3dError::InvalidUri(_))),
                "expected InvalidUri for {bad:?}"
            );
        }
        // A plain relative path is accepted.
        assert!(tileset_json(&cloud, "content.pnts").is_ok());
        assert!(tileset_json(&cloud, "tiles/0/content.pnts").is_ok());
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
