use chrono::{DateTime, Utc};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Location {
    pub id: String,
    pub label: String,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone)]
pub enum DomainDescription {
    /// A time series at a single point (x, y fixed, t varies).
    PointSeries {
        x: f64,
        y: f64,
        t: Vec<DateTime<Utc>>,
    },
    /// A regular grid, optionally with a time dimension.
    Grid {
        x: Vec<f64>,
        y: Vec<f64>,
        t: Option<Vec<DateTime<Utc>>>,
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

/// Result of an area query — either a single coverage (grid) or a collection of coverages (stations).
#[derive(Debug, Clone)]
pub enum AreaQueryResult {
    /// A single coverage, e.g. a Grid from GeoTIFF.
    Single(QueryResult),
    /// Multiple coverages (one per location), e.g. PointSeries from CSV stations.
    Collection(Vec<QueryResult>),
}
