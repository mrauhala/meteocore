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
/// Host headers the transport should accept.
///
/// rmcp defaults to loopback only as DNS-rebinding protection, which means a
/// server behind ANY reverse proxy 403s every request until its public host
/// is added. Loopback stays in the list so a local smoke test still works —
/// and note that testing only via loopback is precisely what hides this.
pub fn allowed_hosts(base_url: &str, extra: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    // The authority from base_url, with and without an explicit port: the
    // Host header carries a port only when it is non-default for the scheme.
    if let Some(authority) = base_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .filter(|a| !a.is_empty())
    {
        hosts.push(authority.to_string());
        if let Some((host, _port)) = authority.rsplit_once(':') {
            if !host.is_empty() {
                hosts.push(host.to_string());
            }
        }
    }
    hosts.extend(extra.iter().cloned());
    hosts.sort();
    hosts.dedup();
    hosts
}

pub fn router(state: AppState, auth: Arc<McpAuth>, allowed_hosts: Vec<String>) -> Router {
    let service = StreamableHttpService::new(
        {
            let state = state.clone();
            move || Ok(MeteoCoreMcp::new(state.clone()))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_json_response(true)
            .with_allowed_hosts(allowed_hosts),
    );

    Router::new()
        .fallback_service(service)
        .layer(axum::middleware::from_fn_with_state(auth, auth::guard))
}
