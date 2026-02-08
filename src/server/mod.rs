pub mod webhook;

use std::net::SocketAddr;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::error::PrAgentError;

/// Start the webhook server.
///
/// Listens on port 3000 by default (overridable via PORT env var).
pub async fn start_server() -> Result<(), PrAgentError> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);

    let app = Router::new()
        .route("/", get(health_check))
        .route(
            "/api/v1/github_webhooks",
            post(webhook::handle_github_webhook),
        )
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024)); // 2 MB

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "starting webhook server");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| PrAgentError::Other(format!("failed to bind to {addr}: {e}")))?;

    axum::serve(listener, app)
        .await
        .map_err(|e| PrAgentError::Other(format!("server error: {e}")))?;

    Ok(())
}

/// Health check endpoint: GET /
async fn health_check() -> impl IntoResponse {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "ok"})),
    )
}
