//! OGC-adjacent it is not: this crate speaks the Model Context Protocol, so a
//! model can ask about tracked storm cells directly instead of a client
//! re-implementing the ranking.
//!
//! Disabled unless `[mcp] enabled = true` and a token resolves — see
//! [`auth`] for why the token has no default.

pub mod auth;
pub mod tools;

use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::Router;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

pub use auth::McpAuth;
pub use tools::{McpState, MeteoCoreMcp};

pub type AppState = Arc<ArcSwap<McpState>>;

/// Build the `/mcp` service, guarded by bearer auth and a rate limit.
///
/// Returned as a `Router` for `nest`ing, rather than a bare service, because
/// the guard has to wrap it — mounting the transport directly would publish an
/// unauthenticated endpoint.
pub fn router(state: AppState, auth: Arc<McpAuth>) -> Router {
    let service = StreamableHttpService::new(
        {
            let state = state.clone();
            move || Ok(MeteoCoreMcp::new(state.clone()))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_json_response(true),
    );

    Router::new()
        .fallback_service(service)
        .layer(axum::middleware::from_fn_with_state(auth, auth::guard))
}
