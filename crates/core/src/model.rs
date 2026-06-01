use chrono::{DateTime, Utc};
use std::collections::HashMap;

use crate::vertical::VerticalKind;

#[derive(Debug, Clone)]
pub struct Location {
    pub id: String,
    pub label: String,
    pub latitude: f64,
    pub longitude: f64,
}

/// A vertical coordinate carried by a domain: the kind of level plus the
/// concrete level values selected for this coverage.
#[derive(Debug, Clone)]
pub struct VerticalCoord {
    pub kind: VerticalKind,
    pub values: Vec<f64>,
}

#[derive(Debug, Clone)]
pub enum DomainDescription {
    /// A time series at a single point (x, y fixed, t varies). `z`, when
    /// present, pins the series to a single vertical level.
    PointSeries {
        x: f64,
        y: f64,
        t: Vec<DateTime<Utc>>,
        z: Option<VerticalCoord>,
    },
    /// A regular grid, optionally with time and vertical dimensions.
    Grid {
        x: Vec<f64>,
        y: Vec<f64>,
        t: Option<Vec<DateTime<Utc>>>,
        z: Option<VerticalCoord>,
    },
    /// A vertical profile at a single point and time (x, y, t fixed, z varies).
    VerticalProfile {
        x: f64,
        y: f64,
        t: Option<DateTime<Utc>>,
        z: VerticalCoord,
    },
    /// A vertical cross-section along a path: the CoverageJSON `Section`
    /// domain. `nodes` carries one `(time, longitude, latitude)` tuple
    /// per along-path output column and is serialised as the mandatory
    /// composite axis with `coordinates: ["t", "x", "y"]`; `z` carries
    /// the vertical levels. The ndarray range shape is `[N_nodes, N_z]`.
    Section {
        nodes: Vec<(DateTime<Utc>, f64, f64)>,
        z: VerticalCoord,
    },
}

#[derive(Debug, Clone)]
pub struct ParameterDescription {
    pub label: String,
    pub unit: String,
    pub observed_property: String,
}

#[derive(Debug, Clone)]
pub struct NdArray {
    pub shape: Vec<usize>,
    pub axis_names: Vec<String>,
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub domain: DomainDescription,
    pub parameters: HashMap<String, ParameterDescription>,
    pub ranges: HashMap<String, NdArray>,
}

/// Result of an EDR position / area / location query — either a single
/// coverage (e.g. a Grid) or a collection of coverages (e.g. one PointSeries
/// per station, or one VerticalProfile per timestep).
#[derive(Debug, Clone)]
pub enum CoverageResponse {
    /// A single coverage.
    Single(QueryResult),
    /// Multiple coverages, serialised as a CoverageJSON `CoverageCollection`.
    Collection(Vec<QueryResult>),
}
