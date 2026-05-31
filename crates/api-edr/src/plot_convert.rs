//! Map an EDR [`CoverageResponse`] onto stacked plot [`Panel`]s for PNG output.
//!
//! - A `VerticalProfile` domain plots the parameter value on x against the
//!   vertical coordinate on y (oriented per [`VerticalKind::direction`]).
//! - A `PointSeries` domain plots time on x against the value on y.
//! - A `Grid` domain (area queries) is not plottable and is rejected.
//!
//! One panel is produced per parameter; each coverage in a collection becomes
//! one overlaid series (timestep for profiles, level/station for time series).

use chrono::{DateTime, Utc};

use ds_core::config::WmsConfig;
use ds_core::error::DataServerError;
use ds_core::model::{
    CoverageResponse, DomainDescription, ParameterDescription, QueryResult, VerticalCoord,
};
use ds_render::{ColorMap, Heatmap, Panel, Series};

/// Build one panel per parameter from an EDR coverage response.
pub fn coverage_response_to_panels(resp: &CoverageResponse) -> Result<Vec<Panel>, DataServerError> {
    let coverages: &[QueryResult] = match resp {
        CoverageResponse::Single(q) => std::slice::from_ref(q),
        CoverageResponse::Collection(v) => v.as_slice(),
    };
    if coverages.is_empty() {
        return Err(DataServerError::InvalidParameter("no data to plot".into()));
    }

    // Reject heterogeneous-domain collections explicitly: `build_panel`
    // dispatches on the first coverage's domain, so a mixed-kind collection
    // would silently drop the non-matching coverages instead of producing
    // partial output. Engines emit homogeneous responses today; this guard
    // is a defensive contract check.
    let first_kind = domain_kind(&coverages[0].domain);
    if coverages
        .iter()
        .any(|q| domain_kind(&q.domain) != first_kind)
    {
        return Err(DataServerError::InvalidParameter(
            "PNG output requires every coverage to share a domain type".into(),
        ));
    }

    // Stable, de-duplicated parameter order across every coverage.
    let mut params: Vec<String> = Vec::new();
    for q in coverages {
        let mut keys: Vec<&String> = q.ranges.keys().collect();
        keys.sort();
        for k in keys {
            if !params.iter().any(|p| p == k) {
                params.push(k.clone());
            }
        }
    }
    if params.is_empty() {
        return Err(DataServerError::InvalidParameter(
            "no parameters to plot".into(),
        ));
    }

    params.iter().map(|p| build_panel(p, coverages)).collect()
}

fn build_panel(param: &str, coverages: &[QueryResult]) -> Result<Panel, DataServerError> {
    let title = param_desc(param, coverages)
        .map(|d| d.label.clone())
        .unwrap_or_else(|| param.to_string());
    let value_caption = value_caption(param, coverages);

    match &coverages[0].domain {
        DomainDescription::VerticalProfile { z, .. } => {
            let y_label = axis_caption(z.kind.default_label(), z.kind.default_unit());
            let y_invert = z.kind.direction() == "down";
            let mut series = Vec::new();
            for q in coverages {
                let DomainDescription::VerticalProfile { z, t, .. } = &q.domain else {
                    continue;
                };
                let Some(nd) = q.ranges.get(param) else {
                    continue;
                };
                // Profile: x = value (nullable), y = level (always present).
                let points = z
                    .values
                    .iter()
                    .zip(nd.values.iter())
                    .map(|(&level, &value)| (value, Some(level)))
                    .collect();
                series.push(Series {
                    label: t.map(fmt_time).unwrap_or_default(),
                    points,
                });
            }
            Ok(Panel {
                title,
                x_label: value_caption,
                y_label,
                y_invert,
                x_is_time: false,
                series,
            })
        }
        DomainDescription::PointSeries { .. } => {
            let mut series = Vec::new();
            for (idx, q) in coverages.iter().enumerate() {
                let DomainDescription::PointSeries { t, z, .. } = &q.domain else {
                    continue;
                };
                let Some(nd) = q.ranges.get(param) else {
                    continue;
                };
                // Time series: x = time (always present), y = value (nullable).
                let points = t
                    .iter()
                    .zip(nd.values.iter())
                    .map(|(dt, &value)| (Some(dt.timestamp() as f64), value))
                    .collect();
                series.push(Series {
                    label: pointseries_label(z.as_ref(), idx, coverages.len()),
                    points,
                });
            }
            Ok(Panel {
                title,
                x_label: "Time (UTC)".to_string(),
                y_label: value_caption,
                y_invert: false,
                x_is_time: true,
                series,
            })
        }
        DomainDescription::Grid { .. } => Err(DataServerError::InvalidParameter(
            "PNG output is not available for gridded (area) responses".into(),
        )),
        DomainDescription::Section { .. } => Err(DataServerError::InvalidParameter(
            "PNG output is not available for cross-section (trajectory) responses".into(),
        )),
    }
}

/// Classify a domain so a heterogeneous-domain collection can be rejected.
fn domain_kind(d: &DomainDescription) -> &'static str {
    match d {
        DomainDescription::VerticalProfile { .. } => "VerticalProfile",
        DomainDescription::PointSeries { .. } => "PointSeries",
        DomainDescription::Grid { .. } => "Grid",
        DomainDescription::Section { .. } => "Section",
    }
}

/// First parameter descriptor for `param` across the coverages.
fn param_desc<'a>(param: &str, coverages: &'a [QueryResult]) -> Option<&'a ParameterDescription> {
    coverages.iter().find_map(|q| q.parameters.get(param))
}

/// `"Label (unit)"`, or just `"Label"` when the unit is blank.
fn value_caption(param: &str, coverages: &[QueryResult]) -> String {
    match param_desc(param, coverages) {
        Some(d) => axis_caption(&d.label, &d.unit),
        None => param.to_string(),
    }
}

fn axis_caption(label: &str, unit: &str) -> String {
    if unit.trim().is_empty() {
        label.to_string()
    } else {
        format!("{label} ({unit})")
    }
}

/// Legend label for a time-series coverage: the vertical level if pinned,
/// otherwise a 1-based index when overlaying several, else blank.
fn pointseries_label(z: Option<&VerticalCoord>, idx: usize, total: usize) -> String {
    if let Some(z) = z {
        if let Some(&level) = z.values.first() {
            let unit = z.kind.default_unit();
            return if unit == "1" {
                format!("{level}")
            } else {
                format!("{level} {unit}")
            };
        }
    }
    if total > 1 {
        format!("#{}", idx + 1)
    } else {
        String::new()
    }
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.format("%H:%M").to_string()
}

// ---------------------------------------------------------------------------
// Section (cross-section) → heatmap, for PNG output
// ---------------------------------------------------------------------------

const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// Great-circle distance (km) between two WGS84 points — a local
/// haversine so `plot_convert` needn't pull in an engine's geo helper.
fn haversine_km(lon0: f64, lat0: f64, lon1: f64, lat1: f64) -> f64 {
    let dlat = (lat1 - lat0).to_radians();
    let dlon = (lon1 - lon0).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat0.to_radians().cos() * lat1.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().clamp(0.0, 1.0).asin();
    EARTH_RADIUS_M * c / 1000.0
}

/// Build the colormap (and its value range) for a cross-section PNG from
/// the collection's optional `[wms]` style config. Mirrors the server's
/// `build_colormap_from_wms_config` priority — inline `color_stops`, then
/// a built-in `colormap` name, then a fallback — using only `ds_render`
/// public APIs.
///
/// The fallback is viridis stretched to `data_range` (the finite min/max
/// of the values being rendered) so a collection with no usable colormap
/// config still produces a readable image rather than a flat one clamped
/// to 0..1.
fn section_colormap(
    wms: Option<&WmsConfig>,
    data_range: (f64, f64),
) -> (Box<dyn ColorMap>, f64, f64) {
    if let Some(w) = wms {
        // Inline custom stops win.
        if !w.color_stops.is_empty() {
            let stops: Vec<ds_render::ColorStop> = w
                .color_stops
                .iter()
                .filter_map(|s| {
                    ds_render::parse_hex_color(&s.color)
                        .ok()
                        .map(|color| ds_render::ColorStop {
                            value: s.value,
                            color,
                        })
                })
                .collect();
            if !stops.is_empty() {
                let min = w
                    .min
                    .unwrap_or_else(|| stops.first().map(|s| s.value).unwrap_or(0.0));
                let max = w
                    .max
                    .unwrap_or_else(|| stops.last().map(|s| s.value).unwrap_or(1.0));
                return (Box::new(ds_render::LinearColorMap::new(stops)), min, max);
            }
        }
        // Then a named built-in.
        if let Some(name) = w.colormap.as_deref() {
            if let Some(builtin) = ds_render::colormap::resolve_builtin(name) {
                let stops = ds_render::colormap::builtin_stops(&builtin);
                let min = w
                    .min
                    .unwrap_or_else(|| stops.first().map(|s| s.value).unwrap_or(0.0));
                let max = w
                    .max
                    .unwrap_or_else(|| stops.last().map(|s| s.value).unwrap_or(1.0));
                return (
                    Box::new(ds_render::LutColorMap::from_builtin(builtin, min, max)),
                    min,
                    max,
                );
            }
        }
    }
    // Fallback: viridis stretched to the data's own finite range.
    let (mut min, mut max) = data_range;
    if !(min.is_finite() && max.is_finite()) || max <= min {
        min = 0.0;
        max = 1.0;
    }
    (
        Box::new(ds_render::LutColorMap::from_builtin(
            ds_render::BuiltinColormap::Viridis,
            min,
            max,
        )),
        min,
        max,
    )
}

/// Convert a cross-section (`Section`) coverage response into stacked
/// heatmaps plus the shared colormap, ready for [`ds_render::render_heatmap`].
///
/// One heatmap is produced per quantity in the **latest** coverage (a
/// multi-timestep request renders its most recent section — a PNG is a
/// single snapshot, and the newest scan is the right default for radar
/// imagery). The x axis is cumulative great-circle distance (km) along
/// the path; the y axis is the vertical coordinate (height above antenna,
/// m). All quantities share the collection colormap; for a correctly-
/// scaled single panel, filter with `parameter-name`.
pub fn section_response_to_heatmaps(
    resp: &CoverageResponse,
    wms: Option<&WmsConfig>,
) -> Result<(Vec<Heatmap>, Box<dyn ColorMap>), DataServerError> {
    let qr = match resp {
        CoverageResponse::Single(q) => q,
        // The engine returns the per-timestep sections oldest-first
        // (`by_site` is time-ascending), so the newest is `last`.
        CoverageResponse::Collection(v) => v.last().ok_or_else(|| {
            DataServerError::InvalidParameter("no cross-section data to plot".into())
        })?,
    };
    let DomainDescription::Section { nodes, z } = &qr.domain else {
        return Err(DataServerError::InvalidParameter(
            "PNG cross-section requires a Section domain".into(),
        ));
    };
    if nodes.len() < 2 || z.values.is_empty() {
        return Err(DataServerError::InvalidParameter(
            "cross-section has too few nodes or levels to plot".into(),
        ));
    }

    // Cumulative along-path distance (km) per node.
    let mut x_values = Vec::with_capacity(nodes.len());
    let mut acc = 0.0;
    x_values.push(0.0);
    for w in nodes.windows(2) {
        let (_, lon0, lat0) = w[0];
        let (_, lon1, lat1) = w[1];
        acc += haversine_km(lon0, lat0, lon1, lat1);
        x_values.push(acc);
    }

    // Quantities in stable order.
    let mut params: Vec<&String> = qr.ranges.keys().collect();
    params.sort();
    if params.is_empty() {
        return Err(DataServerError::InvalidParameter(
            "cross-section carries no parameters to plot".into(),
        ));
    }

    // Global finite data range across the rendered quantities — only used
    // as the colormap fallback when the collection has no colormap config.
    let mut dmin = f64::INFINITY;
    let mut dmax = f64::NEG_INFINITY;
    for p in &params {
        if let Some(nd) = qr.ranges.get(*p) {
            for v in nd.values.iter().flatten() {
                if v.is_finite() {
                    dmin = dmin.min(*v);
                    dmax = dmax.max(*v);
                }
            }
        }
    }

    let (colormap, vmin, vmax) = section_colormap(wms, (dmin, dmax));

    let y_label = axis_caption(z.kind.default_label(), z.kind.default_unit());
    let mut heatmaps = Vec::with_capacity(params.len());
    for p in params {
        let nd = match qr.ranges.get(p) {
            Some(nd) => nd,
            None => continue,
        };
        // Section ndarray is row-major [n_nodes, n_z] — exactly the
        // order `render_heatmap` expects.
        let desc = qr.parameters.get(p);
        let unit = desc.map(|d| d.unit.clone()).unwrap_or_default();
        let label = desc.map(|d| d.label.clone()).unwrap_or_else(|| p.clone());
        heatmaps.push(Heatmap {
            title: label,
            x_label: "Distance along path (km)".to_string(),
            y_label: y_label.clone(),
            value_label: unit,
            x_values: x_values.clone(),
            y_values: z.values.clone(),
            values: nd.values.clone(),
            value_min: vmin,
            value_max: vmax,
        });
    }

    Ok((heatmaps, colormap))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ds_core::model::NdArray;
    use ds_core::vertical::VerticalKind;

    use super::*;

    fn pdesc(label: &str, unit: &str) -> ParameterDescription {
        ParameterDescription {
            label: label.into(),
            unit: unit.into(),
            observed_property: label.into(),
        }
    }

    fn profile(kind: VerticalKind, levels: Vec<f64>, vals: Vec<Option<f64>>) -> QueryResult {
        let mut parameters = HashMap::new();
        parameters.insert("DBZH".to_string(), pdesc("Reflectivity", "dBZ"));
        let mut ranges = HashMap::new();
        ranges.insert(
            "DBZH".to_string(),
            NdArray {
                shape: vec![levels.len()],
                axis_names: vec!["z".into()],
                values: vals,
            },
        );
        QueryResult {
            domain: DomainDescription::VerticalProfile {
                x: 25.0,
                y: 60.0,
                t: Some(DateTime::from_timestamp(1_778_889_600, 0).unwrap()),
                z: VerticalCoord {
                    kind,
                    values: levels,
                },
            },
            parameters,
            ranges,
        }
    }

    #[test]
    fn vertical_profile_maps_value_x_level_y() {
        let q = profile(
            VerticalKind::ElevationAngle,
            vec![0.5, 2.0, 5.0],
            vec![Some(10.0), Some(20.0), None],
        );
        let panels = coverage_response_to_panels(&CoverageResponse::Single(q)).unwrap();
        assert_eq!(panels.len(), 1);
        let p = &panels[0];
        assert_eq!(p.title, "Reflectivity");
        assert_eq!(p.x_label, "Reflectivity (dBZ)");
        assert_eq!(p.y_label, "Elevation angle (deg)");
        assert!(!p.y_invert, "elevation angle grows up");
        assert_eq!(p.series.len(), 1);
        // x = value (None for the third), y = level.
        assert_eq!(
            p.series[0].points,
            vec![
                (Some(10.0), Some(0.5)),
                (Some(20.0), Some(2.0)),
                (None, Some(5.0)),
            ]
        );
    }

    #[test]
    fn pressure_profile_inverts_y() {
        let q = profile(
            VerticalKind::Pressure,
            vec![1000.0, 500.0],
            vec![Some(1.0), Some(2.0)],
        );
        let panels = coverage_response_to_panels(&CoverageResponse::Single(q)).unwrap();
        assert!(panels[0].y_invert, "pressure grows down");
        assert_eq!(panels[0].y_label, "Pressure (hPa)");
    }

    #[test]
    fn collection_overlays_one_series_per_coverage() {
        let a = profile(
            VerticalKind::ElevationAngle,
            vec![0.5, 2.0],
            vec![Some(1.0), Some(2.0)],
        );
        let b = profile(
            VerticalKind::ElevationAngle,
            vec![0.5, 2.0],
            vec![Some(3.0), Some(4.0)],
        );
        let panels =
            coverage_response_to_panels(&CoverageResponse::Collection(vec![a, b])).unwrap();
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].series.len(), 2);
    }

    #[test]
    fn point_series_is_time_axis() {
        let mut parameters = HashMap::new();
        parameters.insert("t2m".to_string(), pdesc("Temperature", "K"));
        let mut ranges = HashMap::new();
        ranges.insert(
            "t2m".to_string(),
            NdArray {
                shape: vec![2],
                axis_names: vec!["t".into()],
                values: vec![Some(280.0), None],
            },
        );
        let q = QueryResult {
            domain: DomainDescription::PointSeries {
                x: 25.0,
                y: 60.0,
                t: vec![
                    DateTime::from_timestamp(1_778_889_600, 0).unwrap(),
                    DateTime::from_timestamp(1_778_893_200, 0).unwrap(),
                ],
                z: None,
            },
            parameters,
            ranges,
        };
        let panels = coverage_response_to_panels(&CoverageResponse::Single(q)).unwrap();
        let p = &panels[0];
        assert!(p.x_is_time);
        assert_eq!(p.x_label, "Time (UTC)");
        assert_eq!(p.y_label, "Temperature (K)");
        assert_eq!(p.series.len(), 1);
        assert_eq!(
            p.series[0].points,
            vec![
                (Some(1_778_889_600.0), Some(280.0)),
                (Some(1_778_893_200.0), None)
            ]
        );
    }

    #[test]
    fn grid_domain_is_rejected() {
        let q = QueryResult {
            domain: DomainDescription::Grid {
                x: vec![1.0],
                y: vec![1.0],
                t: None,
                z: None,
            },
            parameters: HashMap::new(),
            ranges: {
                let mut m = HashMap::new();
                m.insert(
                    "p".to_string(),
                    NdArray {
                        shape: vec![1, 1],
                        axis_names: vec!["y".into(), "x".into()],
                        values: vec![Some(1.0)],
                    },
                );
                m
            },
        };
        assert!(coverage_response_to_panels(&CoverageResponse::Single(q)).is_err());
    }

    /// A collection that mixes domain types is rejected explicitly — the
    /// per-panel dispatch would otherwise silently drop the non-matching
    /// coverages and produce partial output.
    #[test]
    fn heterogeneous_collection_is_rejected() {
        let p = profile(VerticalKind::ElevationAngle, vec![0.5], vec![Some(1.0)]);
        let mut parameters = HashMap::new();
        parameters.insert("t2m".to_string(), pdesc("Temperature", "K"));
        let mut ranges = HashMap::new();
        ranges.insert(
            "t2m".to_string(),
            NdArray {
                shape: vec![1],
                axis_names: vec!["t".into()],
                values: vec![Some(280.0)],
            },
        );
        let ts = QueryResult {
            domain: DomainDescription::PointSeries {
                x: 25.0,
                y: 60.0,
                t: vec![DateTime::from_timestamp(1_778_889_600, 0).unwrap()],
                z: None,
            },
            parameters,
            ranges,
        };
        assert!(coverage_response_to_panels(&CoverageResponse::Collection(vec![p, ts])).is_err());
    }

    // -- Section → heatmap --

    fn section(quantity: &str, unit: &str) -> QueryResult {
        // 3 nodes × 2 levels, row-major [node][level].
        let t = DateTime::from_timestamp(1_778_889_600, 0).unwrap();
        let nodes = vec![(t, 25.0, 60.0), (t, 25.2, 60.1), (t, 25.4, 60.2)];
        let mut parameters = HashMap::new();
        parameters.insert(quantity.to_string(), pdesc("Reflectivity", unit));
        let mut ranges = HashMap::new();
        ranges.insert(
            quantity.to_string(),
            NdArray {
                shape: vec![3, 2],
                axis_names: vec!["composite".into(), "z".into()],
                values: vec![
                    Some(10.0),
                    Some(20.0),
                    Some(15.0),
                    None,
                    Some(5.0),
                    Some(25.0),
                ],
            },
        );
        QueryResult {
            domain: DomainDescription::Section {
                nodes,
                z: VerticalCoord {
                    kind: VerticalKind::HeightAboveAntenna,
                    values: vec![0.0, 1000.0],
                },
            },
            parameters,
            ranges,
        }
    }

    #[test]
    fn section_to_heatmap_builds_distance_axis_and_shape() {
        let qr = section("DBZH", "dBZ");
        let (heatmaps, _cmap) =
            section_response_to_heatmaps(&CoverageResponse::Single(qr), None).unwrap();
        assert_eq!(heatmaps.len(), 1);
        let hm = &heatmaps[0];
        // x axis: 3 cumulative distances, monotonic ascending from 0.
        assert_eq!(hm.x_values.len(), 3);
        assert_eq!(hm.x_values[0], 0.0);
        assert!(hm.x_values[1] > 0.0 && hm.x_values[2] > hm.x_values[1]);
        // y axis: the two heights.
        assert_eq!(hm.y_values, vec![0.0, 1000.0]);
        // values are the row-major ndarray verbatim.
        assert_eq!(hm.values.len(), 6);
        assert_eq!(hm.value_label, "dBZ");
    }

    #[test]
    fn section_colormap_uses_builtin_when_configured() {
        use ds_core::config::WmsConfig;
        let wms = WmsConfig {
            style_bundle: None,
            colormap: Some("radar_dbz".into()),
            color_stops: vec![],
            min: Some(-32.0),
            max: Some(95.0),
            styles: vec![],
            parameters: vec![],
            rendered_cache_mb: 0,
        };
        let qr = section("DBZH", "dBZ");
        let (heatmaps, _cmap) =
            section_response_to_heatmaps(&CoverageResponse::Single(qr), Some(&wms)).unwrap();
        // Config min/max drive the colour-bar bounds.
        assert_eq!(heatmaps[0].value_min, -32.0);
        assert_eq!(heatmaps[0].value_max, 95.0);
    }

    #[test]
    fn section_colormap_falls_back_to_data_range() {
        // No WMS config → viridis stretched to the data's finite range
        // (5..25 in the fixture).
        let qr = section("DBZH", "dBZ");
        let (heatmaps, _cmap) =
            section_response_to_heatmaps(&CoverageResponse::Single(qr), None).unwrap();
        assert_eq!(heatmaps[0].value_min, 5.0);
        assert_eq!(heatmaps[0].value_max, 25.0);
    }

    #[test]
    fn section_heatmap_rejects_non_section() {
        let p = profile(VerticalKind::ElevationAngle, vec![0.5], vec![Some(1.0)]);
        assert!(section_response_to_heatmaps(&CoverageResponse::Single(p), None).is_err());
    }
}
