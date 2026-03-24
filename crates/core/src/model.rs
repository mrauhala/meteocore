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
pub struct DomainDescription {
    pub domain_type: String,
    pub axes_x: f64,
    pub axes_y: f64,
    pub axes_t: Vec<DateTime<Utc>>,
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
