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

use ds_core::error::DataServerError;
use ds_core::model::{
    CoverageResponse, DomainDescription, ParameterDescription, QueryResult, VerticalCoord,
};
use ds_render::{Panel, Series};

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
}
