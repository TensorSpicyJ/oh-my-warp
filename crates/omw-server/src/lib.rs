//! `omw-server` — local-loopback backend shim.
//!
//! Phase C of v0.4-thin. Provides:
//! - An axum [`Router`] factory exposing the internal session registry on a
//!   `127.0.0.1` HTTP loopback (no auth — assumes in-process trust).
//! - A [`SessionRegistry`] tracking live PTY sessions: register, list, look up
//!   by id, write input, subscribe to output, kill on drop.
//! - [`serve`] — bind+run the router on a given [`SocketAddr`], blocking until
//!   the server exits.
//!
//! The registry is in-memory only; there is no persistence in v0.4-thin.
//!
//! See [PRD §8.2](../../../PRD.md#82-components) and
//! [PRD §9.1](../../../PRD.md#91-omw-server-loopback-only).

pub mod error;
pub mod handlers;
pub mod registry;

pub use error::{Error, Result};
pub use registry::{
    ExternalSessionSpec, Session, SessionId, SessionMeta, SessionRegistry, SessionSpec,
};

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

/// Build the axum [`Router`] for the internal session registry surface.
///
/// Routes (all under `/internal/v1`):
/// - `POST   /sessions`            — register a new session, spawning a PTY.
/// - `GET    /sessions`            — list active sessions.
/// - `GET    /sessions/:id`        — get one session's metadata, or 404.
/// - `POST   /sessions/:id/input`  — write base64-encoded input bytes.
/// - `GET    /sessions/:id/pty`    — WebSocket bidirectional PTY frames.
/// - `DELETE /sessions/:id`        — kill a session.
pub fn router(registry: Arc<SessionRegistry>) -> Router {
    Router::new()
        // Session registry routes (with state).
        .route(
            "/internal/v1/sessions",
            post(handlers::sessions::create).get(handlers::sessions::list),
        )
        .route(
            "/internal/v1/sessions/:id",
            get(handlers::sessions::get).delete(handlers::sessions::delete),
        )
        .route(
            "/internal/v1/sessions/:id/input",
            post(handlers::input::write),
        )
        .route(
            "/internal/v1/sessions/:id/pty",
            get(handlers::ws_pty::ws_handler),
        )
        // Agent routes (no state needed).
        .route("/api/v1/providers", get(handlers::agent::list_providers))
        .route("/api/v1/agent/ask", post(handlers::agent::ask))
        .with_state(registry)
}

/// Bind `addr` and serve the [`router`] until the server exits or the listener
/// fails. Blocks the calling task; intended to run on a dedicated runtime thread.
pub async fn serve(registry: Arc<SessionRegistry>, bind: SocketAddr) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let app = router(registry);
    axum::serve(listener, app.into_make_service()).await
}
