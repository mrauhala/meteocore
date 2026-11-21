pub mod capabilities;
pub mod capabilities_cache;
pub mod error;
pub mod feature_info;
pub mod handlers;
pub mod params;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;

pub use capabilities_cache::CAPABILITIES_TTL;
pub use handlers::{AppState, ErrorTile, WmsState, ERROR_TILE_REASONS};

/// The WMS service over `state`, serving each GetCapabilities document for
/// [`CAPABILITIES_TTL`] (#1482).
pub fn router(state: AppState) -> Router {
    router_with_capabilities_ttl(state, CAPABILITIES_TTL)
}

/// [`router`] serving each GetCapabilities document for `ttl`;
/// `Duration::ZERO` builds one per request, for a test that changes an
/// engine's layers and asks again at once.
pub fn router_with_capabilities_ttl(state: AppState, ttl: Duration) -> Router {
    Router::new()
        .route("/", get(handlers::wms_handler))
        .with_state(handlers::Services {
            state,
            capabilities: Arc::new(capabilities_cache::CapabilitiesCache::new(ttl)),
        })
}
