//! Card Intake NAS service. Runs in Docker on the Synology, next to Immich.
//!
//! Settings come from environment variables:
//! - `CARD_INTAKE_PHOTOS`: the photo share inside the container (default `/photos`)
//! - `PORT`: where to listen (default `8787`)

use axum::{Router, routing::get};
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let photos = std::env::var("CARD_INTAKE_PHOTOS").unwrap_or_else(|_| "/photos".into());
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787u16);

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/version", get(|| async { env!("CARGO_PKG_VERSION") }));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("can't listen on port {port}: {e}"));
    tracing::info!(%addr, %photos, "Card Intake server listening");
    axum::serve(listener, app).await.expect("server stopped");
}
