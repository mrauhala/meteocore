pub mod capabilities;
pub mod error;
pub mod handlers;
pub mod params;

use axum::routing::get;
use axum::Router;

pub use handlers::{AppState, WmsState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::wms_handler))
        .with_state(state)
}
