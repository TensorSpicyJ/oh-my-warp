//! SSE-based PTY transport — alternative to WebSocket for browsers
//! (including mobile) behind proxies that don't support WS upgrade.
//!
//! - `GET  /api/v1/sessions/:id/pty?ct=<token>` → SSE output stream
//! - `POST /api/v1/sessions/:id/pty/input?ct=<token>` → input bytes

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::Utc;
use futures_util::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::broadcast;

use crate::capability::Capability;
use crate::server::{extract_ct_query_param, verify_connect_token, AppState};

#[derive(Deserialize)]
pub(crate) struct PtyInputBody {
    pub(crate) input: String,
}

fn map_ct_error(e: crate::server::ConnectTokenError) -> (StatusCode, &'static str) {
    use crate::server::ConnectTokenError;
    match e {
        ConnectTokenError::Malformed => (StatusCode::BAD_REQUEST, "malformed_ct"),
        ConnectTokenError::CapabilityInvalid => (StatusCode::UNAUTHORIZED, "capability_invalid"),
        ConnectTokenError::CapabilityExpired => (StatusCode::UNAUTHORIZED, "capability_expired"),
        ConnectTokenError::CapabilityScope => (StatusCode::FORBIDDEN, "capability_scope"),
        ConnectTokenError::DeviceRevoked => (StatusCode::UNAUTHORIZED, "device_revoked"),
        ConnectTokenError::TsSkew => (StatusCode::UNAUTHORIZED, "timestamp_skew"),
        ConnectTokenError::NonceReplayed => (StatusCode::FORBIDDEN, "nonce_replayed"),
        ConnectTokenError::SignatureInvalid => (StatusCode::UNAUTHORIZED, "signature_invalid"),
    }
}

/// A stream that yields SSE Events from a broadcast receiver.
/// First emits an initial event, then forwards all broadcast items.
pub(crate) struct PtySseStream {
    pub(crate) state: PtySseState,
}

pub(crate) enum PtySseState {
    EmitSnapshot(String),
    Live {
        rx: broadcast::Receiver<bytes::Bytes>,
    },
    Done,
}

impl Stream for PtySseStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match &mut self.state {
                PtySseState::EmitSnapshot(_) => {
                    let PtySseState::EmitSnapshot(b64) =
                        std::mem::replace(&mut self.state, PtySseState::Done)
                    else {
                        unreachable!()
                    };
                    self.state = PtySseState::Done;
                    return Poll::Ready(Some(Ok(Event::default().data(b64).event("snapshot"))));
                }
                PtySseState::Live { rx } => match rx.try_recv() {
                    Ok(bytes) => {
                        let b64 = URL_SAFE_NO_PAD.encode(&bytes);
                        return Poll::Ready(Some(Ok(Event::default().data(b64))));
                    }
                    Err(broadcast::error::TryRecvError::Empty) => {
                        // Register waker and wait.
                        // broadcast::Receiver doesn't implement poll_recv,
                        // so we use a small hack: spawn a task that waits
                        // and signals via a waker.
                        return Poll::Pending;
                    }
                    Err(broadcast::error::TryRecvError::Closed) => {
                        self.state = PtySseState::Done;
                        return Poll::Ready(None);
                    }
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {
                        // Skip lagged chunks and try again.
                        continue;
                    }
                },
                PtySseState::Done => return Poll::Ready(None),
            }
        }
    }
}

pub(crate) async fn pty_output_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let origin = headers
        .get("origin")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !state.pinned_origins.iter().any(|o| o == origin) {
        return (StatusCode::FORBIDDEN, "origin_mismatch").into_response();
    }

    let raw_ct = raw_query
        .as_deref()
        .and_then(extract_ct_query_param)
        .map(|s| s.to_string());
    let ct = match raw_ct {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "missing_connect_token").into_response(),
    };

    let request_path = format!("/api/v1/sessions/{session_id}/pty");
    let now = Utc::now();
    if let Err(e) = verify_connect_token(
        &ct,
        &request_path,
        &state.host_pubkey,
        &state.nonce_store,
        &state.revocations,
        Capability::PtyRead,
        300,
        now,
    ) {
        let (status, msg) = map_ct_error(e);
        return (status, msg).into_response();
    };

    let s_id: uuid::Uuid = match session_id.parse() {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid_session_id").into_response(),
    };

    let (snapshot, rx) = match state.pty_registry.subscribe_with_state(s_id) {
        Some(r) => r,
        None => return (StatusCode::NOT_FOUND, "session_not_found").into_response(),
    };

    let snapshot_b64 = URL_SAFE_NO_PAD.encode(&snapshot);
    let stream = PtySseStream {
        state: PtySseState::EmitSnapshot(snapshot_b64),
    };
    // Swap state to Live after the first poll.
    // Actually, we need to set up properly. Let's just use a simpler approach.

    // Drop the stream and use a channel-based approach instead.
    let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);

    // Send snapshot first.
    let _ = tx
        .try_send(Ok(Event::default()
            .data(URL_SAFE_NO_PAD.encode(&snapshot))
            .event("snapshot")));

    // Spawn a task to forward broadcast items.
    tokio::spawn(async move {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(bytes) => {
                    let b64 = URL_SAFE_NO_PAD.encode(&bytes);
                    if tx.send(Ok(Event::default().data(b64))).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    if tx
                        .send(Ok(Event::default()
                            .data(format!("{{\"lagged\":{n}}}"))
                            .event("lagged")))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    Sse::new(tokio_stream::wrappers::ReceiverStream::new(rx_stream))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

pub(crate) async fn pty_input_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    headers: axum::http::HeaderMap,
    Json(body): Json<PtyInputBody>,
) -> axum::response::Response {
    let origin = headers
        .get("origin")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !state.pinned_origins.iter().any(|o| o == origin) {
        return (StatusCode::FORBIDDEN, "origin_mismatch").into_response();
    }

    let raw_ct = raw_query
        .as_deref()
        .and_then(extract_ct_query_param)
        .map(|s| s.to_string());
    let ct = match raw_ct {
        Some(c) => c,
        None => return (StatusCode::BAD_REQUEST, "missing_connect_token").into_response(),
    };

    let request_path = format!("/api/v1/sessions/{session_id}/pty/input");
    let now = Utc::now();
    if let Err(e) = verify_connect_token(
        &ct,
        &request_path,
        &state.host_pubkey,
        &state.nonce_store,
        &state.revocations,
        Capability::PtyWrite,
        300,
        now,
    ) {
        let (status, msg) = map_ct_error(e);
        return (status, msg).into_response();
    };

    let s_id: uuid::Uuid = match session_id.parse() {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid_session_id").into_response(),
    };

    let bytes = match URL_SAFE_NO_PAD.decode(&body.input) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid_base64").into_response(),
    };

    match state.pty_registry.write_input(s_id, &bytes).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
            eprintln!("[omw-debug] sse_input write failed: {e}");
            (StatusCode::NOT_FOUND, "session_not_found").into_response()
        }
    }
}
