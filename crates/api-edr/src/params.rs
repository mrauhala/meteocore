use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct LocationQueryParams {
    pub datetime: Option<String>,
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PositionQueryParams {
    pub coords: String,
    pub datetime: Option<String>,
    #[serde(rename = "parameter-name")]
    pub parameter_name: Option<String>,
}
