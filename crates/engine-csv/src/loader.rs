use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap};

use ds_core::error::DataServerError;

#[derive(Debug, Clone)]
pub struct CsvRow {
    pub location: String,
    pub latitude: f64,
    pub longitude: f64,
    pub time: DateTime<Utc>,
    pub values: HashMap<String, Option<f64>>,
}

#[derive(Debug)]
pub struct CsvDataStore {
    pub rows: Vec<CsvRow>,
    pub location_index: HashMap<String, Vec<usize>>,
    pub time_index: HashMap<String, BTreeMap<DateTime<Utc>, Vec<usize>>>,
    pub parameter_names: Vec<String>,
    pub parameter_units: HashMap<String, String>,
}

impl CsvDataStore {
    pub fn load(path: &str) -> Result<Self, DataServerError> {
        let mut reader = csv::Reader::from_path(path)
            .map_err(|e| DataServerError::Csv(format!("Failed to open {path}: {e}")))?;

        let headers: Vec<String> = reader
            .headers()
            .map_err(|e| DataServerError::Csv(format!("Failed to read headers: {e}")))?
            .iter()
            .map(|h| h.to_string())
            .collect();

        // Fixed columns: location, latitude, longitude, time
        // Everything else is a parameter
        let param_start = 4;
        let parameter_names: Vec<String> = headers[param_start..].to_vec();

        let parameter_units: HashMap<String, String> = parameter_names
            .iter()
            .map(|name| {
                let unit = match name.as_str() {
                    "temperature" => "°C",
                    "humidity" => "%",
                    "wind_speed" => "m/s",
                    "pressure" => "hPa",
                    "precipitation" => "mm",
                    _ => "",
                };
                (name.clone(), unit.to_string())
            })
            .collect();

        let mut rows = Vec::new();
        let mut location_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut time_index: HashMap<String, BTreeMap<DateTime<Utc>, Vec<usize>>> = HashMap::new();

        for result in reader.records() {
            let record =
                result.map_err(|e| DataServerError::Csv(format!("Failed to read row: {e}")))?;

            let location = record[0].to_string();
            let latitude: f64 = record[1]
                .parse()
                .map_err(|e| DataServerError::Csv(format!("Invalid latitude: {e}")))?;
            let longitude: f64 = record[2]
                .parse()
                .map_err(|e| DataServerError::Csv(format!("Invalid longitude: {e}")))?;
            let time: DateTime<Utc> = record[3]
                .parse()
                .map_err(|e| DataServerError::Csv(format!("Invalid time: {e}")))?;

            let mut values = HashMap::new();
            for (i, param_name) in parameter_names.iter().enumerate() {
                let val = record[param_start + i].parse::<f64>().ok();
                values.insert(param_name.clone(), val);
            }

            let idx = rows.len();
            location_index
                .entry(location.clone())
                .or_default()
                .push(idx);
            time_index
                .entry(location.clone())
                .or_default()
                .entry(time)
                .or_default()
                .push(idx);

            rows.push(CsvRow {
                location,
                latitude,
                longitude,
                time,
                values,
            });
        }

        Ok(CsvDataStore {
            rows,
            location_index,
            time_index,
            parameter_names,
            parameter_units,
        })
    }
}
