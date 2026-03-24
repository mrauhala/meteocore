use std::sync::Arc;

use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config = ds_core::config::ServerConfig::from_file("config.toml")
        .expect("Failed to load config.toml");

    let collection = &config.collections[0];
    info!("Loading collection '{}' from {}", collection.id, collection.data_path);

    let store = engine_csv::CsvDataStore::load(&collection.data_path)
        .expect("Failed to load CSV data");

    info!(
        "Loaded {} rows, {} locations, {} parameters",
        store.rows.len(),
        store.location_index.len(),
        store.parameter_names.len()
    );

    let engine = Arc::new(engine_csv::CsvEngine::new(store)) as Arc<dyn ds_core::engine::Engine>;
    let app = api_edr::router(engine);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind");

    axum::serve(listener, app).await.expect("Server error");
}
