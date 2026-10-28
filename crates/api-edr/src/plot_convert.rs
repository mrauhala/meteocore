//! Map an EDR [`CoverageResponse`] onto stacked plot [`Panel`]s for PNG output.
//!
//! - A `VerticalProfile` domain plots the parameter value on x against the
//!   vertical coordinate on y (oriented per [`VerticalKind::direction`]).
//! - A `PointSeries` domain plots time on x against the value on y.
//! - A `Grid` domain (area queries) is not plottable and is rejected.
//!
//! One panel is produced per parameter; each coverage in a collection becomes
//! one overlaid series (timestep for profiles, level/station for time series).

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use ds_core::error::DataServerError;
use ds_core::model::{
    CoverageResponse, DomainDescription, ParameterDescription, QueryResult, VerticalCoord,
};
use ds_render::{ColorMap, Heatmap, Panel, Series};

/// Build one panel per parameter from an EDR coverage response.
pub fn coverage_response_to_panels(resp: &CoverageResponse) -> Result<Vec<Panel>, DataServerError> {
    let coverages = plotted_coverages(resp)?;

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

    params.iter().map(|p| build_panel(p, coverages)).collect()
}

/// Whether [`coverage_response_to_panels`] plots `resp`, from the same
/// checks and without building a panel: an HTML data page asks before it
/// embeds the query's PNG (#1138).
pub fn is_plottable(resp: &CoverageResponse) -> bool {
    plotted_coverages(resp).is_ok()
}

/// The coverages of `resp` when they plot, else why not: at least one, one
/// domain type, a parameter, and a domain a panel draws.
fn plotted_coverages(resp: &CoverageResponse) -> Result<&[QueryResult], DataServerError> {
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
    if coverages.iter().all(|q| q.ranges.is_empty()) {
        return Err(DataServerError::InvalidParameter(
            "no parameters to plot".into(),
        ));
    }
    match unplottable(&coverages[0].domain) {
        Some(reason) => Err(reason),
        None => Ok(coverages),
    }
}

/// Why a domain has no panel; `None` for the two that do.
fn unplottable(domain: &DomainDescription) -> Option<DataServerError> {
    let reason = match domain {
        DomainDescription::VerticalProfile { .. } | DomainDescription::PointSeries { .. } => {
            return None
        }
        DomainDescription::Grid { .. } => {
            "PNG output is not available for gridded (area) responses"
        }
        DomainDescription::Section { .. } => {
            "PNG output is not available for cross-section (trajectory) responses"
        }
        DomainDescription::Point { .. } => "PNG output is not available for point-event responses",
        DomainDescription::Trajectory { .. } => {
            "PNG output is not available for along-path trajectory responses"
        }
    };
    Some(DataServerError::InvalidParameter(reason.into()))
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
        // `plotted_coverages` has already turned these away.
        other => Err(unplottable(other)
            .unwrap_or_else(|| DataServerError::InvalidParameter("no data to plot".into()))),
    }
}

/// Classify a domain so a heterogeneous-domain collection can be rejected.
fn domain_kind(d: &DomainDescription) -> &'static str {
    match d {
        DomainDescription::VerticalProfile { .. } => "VerticalProfile",
        DomainDescription::PointSeries { .. } => "PointSeries",
        DomainDescription::Grid { .. } => "Grid",
        DomainDescription::Section { .. } => "Section",
        DomainDescription::Point { .. } => "Point",
        DomainDescription::Trajectory { .. } => "Trajectory",
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

/// Convert a cross-section (`Section`) coverage response into stacked
/// heatmaps, ready for [`ds_render::render_heatmap`].
///
/// One heatmap is produced per quantity in the **latest** coverage (a
/// multi-timestep request renders its most recent section — a PNG is a
/// single snapshot, and the newest scan is the right default for radar
/// imagery). The x axis is cumulative great-circle distance (km) along
/// the path; the y axis is the vertical coordinate (height above antenna,
/// m).
///
/// Each quantity draws with its own `default` style, resolved from the
/// server's style layers `styles` as a map request naming that parameter
/// resolves it ([`ds_render::resolve_layer_style`], #1159): the
/// `{collection}/{parameter}` layer's style, which WMS, Maps and Tiles
/// draw the parameter with, else the collection's. A quantity with
/// neither gets viridis stretched to its own finite data range.
pub fn section_response_to_heatmaps(
    resp: &CoverageResponse,
    styles: &HashMap<String, HashMap<String, ds_render::StyleInfo>>,
    collection_id: &str,
) -> Result<Vec<Heatmap>, DataServerError> {
    let qr = match resp {
        CoverageResponse::Single(q) => q,
        // The engine returns the per-timestep sections oldest-first
        // (`by_site` is time-ascending), so the newest is `last`.
        CoverageResponse::Collection(v) => v.last().ok_or_else(|| {
            DataServerError::InvalidParameter("no cross-section data to plot".into())
        })?,
    };
    let DomainDescription::Section {
        nodes,
        z,
        coverage_floor,
    } = &qr.domain
    else {
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

    // Lowest-beam coverage floor (#514) as an overlay polyline on every
    // panel: x = along-path km, y = floor metres (raw — ds-render clips
    // the below-axis / above-ceiling reaches). The hatch below the line
    // marks the unobserved region as distinct from in-beam nodata.
    let overlays: Vec<ds_render::OverlayLine> = match coverage_floor {
        Some(floor) if floor.len() == nodes.len() => vec![ds_render::OverlayLine {
            label: "lowest beam".to_string(),
            color: [0x20, 0x20, 0x20],
            points: x_values
                .iter()
                .copied()
                .zip(floor.iter().copied())
                .collect(),
            hatch_below: Some([0xb4, 0xb4, 0xb4]),
        }],
        _ => vec![],
    };

    let y_label = axis_caption(z.kind.default_label(), z.kind.default_unit());
    let mut heatmaps = Vec::with_capacity(params.len());
    for p in params {
        let nd = match qr.ranges.get(p) {
            Some(nd) => nd,
            None => continue,
        };
        let style =
            ds_render::resolve_layer_style(styles, collection_id, Some(p), None, "default").ok();
        let (colormap, value_min, value_max): (Arc<dyn ColorMap>, f64, f64) = match style {
            Some(resolved) => (
                resolved.style.colormap.clone(),
                resolved.style.min,
                resolved.style.max,
            ),
            None => {
                // Fallback: viridis stretched to the quantity's own finite
                // range, so a collection with no style config still
                // produces a readable panel.
                let (min, max) = finite_range(&nd.values);
                (
                    Arc::new(ds_render::LutColorMap::from_builtin(
                        ds_render::BuiltinColormap::Viridis,
                        min,
                        max,
                    )),
                    min,
                    max,
                )
            }
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
            colormap,
            value_min,
            value_max,
            overlays: overlays.clone(),
        });
    }

    Ok(heatmaps)
}

/// The finite `(min, max)` of `values`, or `(0, 1)` when they hold no
/// finite range.
fn finite_range(values: &[Option<f64>]) -> (f64, f64) {
    let (min, max) = values
        .iter()
        .flatten()
        .filter(|v| v.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(min, max), &v| {
            (min.min(v), max.max(v))
        });
    if min.is_finite() && max.is_finite() && max > min {
        (min, max)
    } else {
        (0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ds_core::model::{NdArray, ValuePrecision};
    use ds_core::vertical::VerticalKind;

    use super::*;

    fn pdesc(label: &str, unit: &str) -> ParameterDescription {
        ParameterDescription {
            label: label.into(),
            unit: unit.into(),
            observed_property: label.into(),
            standard_name: None,
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
                precision: ValuePrecision::F64,
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
                precision: ValuePrecision::F64,
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
                        precision: ValuePrecision::F64,
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
                precision: ValuePrecision::F64,
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

    /// #1138: `is_plottable` answers what building the panels would, for
    /// each way a response plots or does not.
    #[test]
    fn is_plottable_agrees_with_the_panels() {
        let one_profile = || profile(VerticalKind::ElevationAngle, vec![0.5], vec![Some(1.0)]);
        let with_domain = |domain| QueryResult {
            domain,
            ..one_profile()
        };
        let series = || {
            with_domain(DomainDescription::PointSeries {
                x: 25.0,
                y: 60.0,
                t: vec![DateTime::from_timestamp(1_778_889_600, 0).unwrap()],
                z: None,
            })
        };
        let grid = with_domain(DomainDescription::Grid {
            x: vec![1.0],
            y: vec![1.0],
            t: None,
            z: None,
        });
        let mut no_parameters = one_profile();
        no_parameters.ranges.clear();
        let cases = [
            ("profile", CoverageResponse::Single(one_profile()), true),
            ("series", CoverageResponse::Single(series()), true),
            (
                "profiles",
                CoverageResponse::Collection(vec![one_profile(), one_profile()]),
                true,
            ),
            ("nothing", CoverageResponse::Collection(Vec::new()), false),
            (
                "no parameters",
                CoverageResponse::Single(no_parameters),
                false,
            ),
            ("grid", CoverageResponse::Single(grid), false),
            (
                "mixed",
                CoverageResponse::Collection(vec![one_profile(), series()]),
                false,
            ),
        ];
        for (name, response, plots) in &cases {
            assert_eq!(is_plottable(response), *plots, "{name}");
            assert_eq!(
                coverage_response_to_panels(response).is_ok(),
                *plots,
                "{name}"
            );
        }
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
                precision: ValuePrecision::F64,
            },
        );
        QueryResult {
            domain: DomainDescription::Section {
                nodes,
                z: VerticalCoord {
                    kind: VerticalKind::HeightAboveAntenna,
                    values: vec![0.0, 1000.0],
                },
                coverage_floor: Some(vec![50.0, 400.0, 900.0]),
            },
            parameters,
            ranges,
        }
    }

    /// A section of every quantity in `quantities`, each with the
    /// fixture's values.
    fn section_of(quantities: &[&str]) -> QueryResult {
        let mut qr = section(quantities[0], "dBZ");
        for quantity in &quantities[1..] {
            let more = section(quantity, "dBZ");
            qr.parameters.extend(more.parameters);
            qr.ranges.extend(more.ranges);
        }
        qr
    }

    /// The heatmaps of the section `qr` on the collection `site`, styled
    /// from `styles`.
    fn plot(
        qr: QueryResult,
        styles: &HashMap<String, HashMap<String, ds_render::StyleInfo>>,
    ) -> Vec<Heatmap> {
        section_response_to_heatmaps(&CoverageResponse::Single(qr), styles, "site").unwrap()
    }

    /// Style layers holding only the collection's own, with `style` as its
    /// default.
    fn collection_layer(
        style: ds_render::StyleInfo,
    ) -> HashMap<String, HashMap<String, ds_render::StyleInfo>> {
        HashMap::from([(
            "site".to_string(),
            HashMap::from([("default".to_string(), style)]),
        )])
    }

    #[test]
    fn section_to_heatmap_builds_distance_axis_and_shape() {
        let qr = section("DBZH", "dBZ");
        let heatmaps = plot(qr, &HashMap::new());
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

    /// Build a resolved `StyleInfo` through the same `StyleContext` path the
    /// server uses, so these tests pin the api-edr-facing contract.
    fn resolved_style(spec: &ds_render::StyleSpec) -> ds_render::StyleInfo {
        let ctx = ds_render::StyleContext::with_builtins();
        let r = ctx.build_colormap(spec).unwrap();
        ds_render::StyleInfo {
            name: "default".into(),
            title: "Default".into(),
            colormap: r.colormap,
            palette: r.palette,
            min: r.min,
            max: r.max,
            parameter: None,
        }
    }

    #[test]
    fn section_style_uses_builtin_when_configured() {
        let style = resolved_style(&ds_render::StyleSpec {
            colormap: Some("radar_dbz"),
            color_stops: &[],
            min: Some(-32.0),
            max: Some(95.0),
        });
        let qr = section("DBZH", "dBZ");
        let heatmaps = plot(qr, &collection_layer(style));
        // Style min/max drive the colour-bar bounds.
        assert_eq!(heatmaps[0].value_min, -32.0);
        assert_eq!(heatmaps[0].value_max, 95.0);
    }

    #[test]
    fn section_colormap_falls_back_to_data_range() {
        // No resolved style → viridis stretched to the data's finite range
        // (5..25 in the fixture).
        let qr = section("DBZH", "dBZ");
        let heatmaps = plot(qr, &HashMap::new());
        assert_eq!(heatmaps[0].value_min, 5.0);
        assert_eq!(heatmaps[0].value_max, 25.0);
    }

    /// A `default` style on `palette` over `min..max`, resolved as the
    /// server resolves a configured one.
    fn palette_style(palette: &str, min: f64, max: f64) -> ds_render::StyleInfo {
        resolved_style(&ds_render::StyleSpec {
            colormap: Some(palette),
            color_stops: &[],
            min: Some(min),
            max: Some(max),
        })
    }

    /// #1159: each panel draws with its own parameter's style layer, the
    /// one WMS `{collection}/{parameter}` and Maps/Tiles `parameter-name`
    /// draw that parameter with, not the collection's default.
    #[test]
    fn section_panels_draw_with_their_parameter_styles() {
        let mut styles = collection_layer(palette_style("temperature", -90.0, 50.0));
        for (layer, palette, min, max) in [
            ("site/DBZH", "radar_dbz", -32.0, 95.0),
            ("site/VRADH", "viridis", -30.0, 30.0),
        ] {
            styles.insert(
                layer.to_string(),
                HashMap::from([("default".to_string(), palette_style(palette, min, max))]),
            );
        }
        let heatmaps = plot(section_of(&["DBZH", "VRADH"]), &styles);
        assert_eq!(heatmaps.len(), 2);
        for (hm, layer) in heatmaps.iter().zip(["site/DBZH", "site/VRADH"]) {
            let style = &styles[layer]["default"];
            assert_eq!(
                (hm.value_min, hm.value_max),
                (style.min, style.max),
                "{layer}"
            );
            assert!(
                Arc::ptr_eq(&hm.colormap, &style.colormap),
                "{layer} draws with its own colormap"
            );
        }
    }

    /// A quantity without a style layer of its own (a moment that appeared
    /// after the server resolved the layers) draws with the collection's
    /// default, beside a quantity that has one.
    #[test]
    fn section_parameter_without_a_layer_uses_the_collection_style() {
        let mut styles = collection_layer(palette_style("temperature", -90.0, 50.0));
        styles.insert(
            "site/DBZH".to_string(),
            HashMap::from([(
                "default".to_string(),
                palette_style("radar_dbz", -32.0, 95.0),
            )]),
        );
        let heatmaps = plot(section_of(&["DBZH", "ZDR"]), &styles);
        let collection = &styles["site"]["default"];
        assert_eq!(
            (heatmaps[0].value_min, heatmaps[0].value_max),
            (-32.0, 95.0)
        );
        assert_eq!(
            (heatmaps[1].value_min, heatmaps[1].value_max),
            (collection.min, collection.max)
        );
        assert!(Arc::ptr_eq(&heatmaps[1].colormap, &collection.colormap));
    }

    /// Without any style, each quantity's viridis fallback spans its own
    /// finite values, not the union of every panel's.
    #[test]
    fn section_fallback_ranges_are_per_quantity() {
        let mut qr = section_of(&["DBZH", "ZDR"]);
        for v in qr
            .ranges
            .get_mut("ZDR")
            .unwrap()
            .values
            .iter_mut()
            .flatten()
        {
            *v /= 10.0;
        }
        let heatmaps = plot(qr, &HashMap::new());
        assert_eq!((heatmaps[0].value_min, heatmaps[0].value_max), (5.0, 25.0));
        assert_eq!((heatmaps[1].value_min, heatmaps[1].value_max), (0.5, 2.5));
    }

    /// The coverage floor becomes one overlay polyline per panel (#514):
    /// x = the cumulative along-path km axis, y = the raw floor metres,
    /// hatched below. A floor whose length mismatches the nodes is
    /// dropped rather than plotted misaligned.
    #[test]
    fn section_floor_maps_to_overlay() {
        let qr = section("DBZH", "dBZ");
        let heatmaps = plot(qr, &HashMap::new());
        let hm = &heatmaps[0];
        assert_eq!(hm.overlays.len(), 1);
        let ov = &hm.overlays[0];
        assert_eq!(ov.points.len(), 3);
        assert!(ov.hatch_below.is_some(), "unobserved region is hatched");
        // x from the distance axis, y the raw floor metres.
        for (i, &(x, y)) in ov.points.iter().enumerate() {
            assert_eq!(x, hm.x_values[i]);
            assert_eq!(y, [50.0, 400.0, 900.0][i]);
        }

        let mut qr = section("DBZH", "dBZ");
        if let DomainDescription::Section { coverage_floor, .. } = &mut qr.domain {
            *coverage_floor = Some(vec![1.0]); // wrong length
        }
        let heatmaps = plot(qr, &HashMap::new());
        assert!(heatmaps[0].overlays.is_empty());

        let mut qr = section("DBZH", "dBZ");
        if let DomainDescription::Section { coverage_floor, .. } = &mut qr.domain {
            *coverage_floor = None;
        }
        let heatmaps = plot(qr, &HashMap::new());
        assert!(heatmaps[0].overlays.is_empty());
    }

    #[test]
    fn section_heatmap_rejects_non_section() {
        let p = profile(VerticalKind::ElevationAngle, vec![0.5], vec![Some(1.0)]);
        assert!(section_response_to_heatmaps(
            &CoverageResponse::Single(p),
            &HashMap::new(),
            "site"
        )
        .is_err());
    }

    /// Descending-order `color_stops` (a natural radar-dBZ legend order)
    /// must still yield `value_min < value_max` — the colour bar would
    /// otherwise label max→min top-to-bottom against the gradient. The stop
    /// sorting itself now lives in `ds_render::StyleContext` (pinned there
    /// by `inline_stops_win_over_name_and_sort_descending_input`); this
    /// keeps the api-edr-level pin by resolving descending stops through
    /// the same path a config would take.
    #[test]
    fn section_style_from_descending_color_stops_sorts_range() {
        use ds_core::config::ColorStop;
        // Authored highest-value-first.
        let stops = vec![
            ColorStop {
                value: 60.0,
                color: "#ff0000".into(),
            },
            ColorStop {
                value: 30.0,
                color: "#00ff00".into(),
            },
            ColorStop {
                value: 0.0,
                color: "#0000ff".into(),
            },
        ];
        let style = resolved_style(&ds_render::StyleSpec {
            colormap: None,
            color_stops: &stops,
            min: None,
            max: None,
        });
        assert!(style.min < style.max, "resolved style must sort the range");
        let qr = section("DBZH", "dBZ");
        let heatmaps = plot(qr, &collection_layer(style));
        assert_eq!(heatmaps[0].value_min, 0.0, "min from the lowest stop");
        assert_eq!(heatmaps[0].value_max, 60.0, "max from the highest stop");
    }
}
