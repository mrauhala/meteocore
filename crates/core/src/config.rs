use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub server: ServerSettings,
    pub collections: Vec<CollectionConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct CollectionConfig {
    pub id: String,
    pub title: String,
    pub description: String,
    pub data_path: String,
}

impl ServerConfig {
    pub fn from_file(path: &str) -> Result<Self, crate::error::DataServerError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::error::DataServerError::Config(format!("Failed to read {path}: {e}")))?;
        toml::from_str(&content)
            .map_err(|e| crate::error::DataServerError::Config(format!("Failed to parse config: {e}")))
    }
}
