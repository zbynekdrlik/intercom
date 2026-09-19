//! The hub's observability HTTP layer (issue 1345 M1) — the bkshading service skeleton.
//!
//! `/api/state` returns the whole [`HubState`] JSON; `/ws` pushes a fresh snapshot every ~1 s from
//! a single background pump (all clients fan out from one `watch` channel); `/api/version` is the
//! version-on-dashboard surface; `/` is a plain-text liveness page. The phone PWA is M3 — M1 serves
//! JSON only.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::header,
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use tokio::sync::watch;

use crate::state::HubState;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
pub struct AppState {
    /// The latest hub state, published by the block-status pump. Keeping a receiver here means the
    /// pump's `send` never fails for "no receivers".
    pub live: watch::Receiver<Arc<HubState>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/version", get(version))
        .route("/api/state", get(api_state))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

async fn index(State(state): State<AppState>) -> impl IntoResponse {
    let snap = state.live.borrow().clone();
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!(
            "intercom-hub {} — {} participants, {} Hz, block {}\n(M1: VBAN N-1 mix-minus hub; /api/state for JSON)\n",
            snap.version,
            snap.participants.len(),
            snap.sample_rate,
            snap.block_frames
        ),
    )
}

async fn version() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": VERSION }))
}

async fn api_state(State(state): State<AppState>) -> Json<Arc<HubState>> {
    Json(state.live.borrow().clone())
}

async fn ws_upgrade(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| ws_push(socket, state.live.clone()))
}

/// Push the latest [`HubState`] to one client: the current state immediately, then a fresh state on
/// every pump update. Push-only (a browser panel sends nothing); the loop ends when the client goes
/// away or the pump is dropped (shutdown).
async fn ws_push(mut socket: WebSocket, mut rx: watch::Receiver<Arc<HubState>>) {
    loop {
        let snapshot = rx.borrow_and_update().clone();
        match ws_encode(&snapshot) {
            Ok(text) => {
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "ws: serialize hub state failed");
                break;
            }
        }
        if rx.changed().await.is_err() {
            break;
        }
    }
}

/// Serialize a hub state for the WS wire (the exact text a `/ws` frame carries).
pub fn ws_encode(state: &HubState) -> Result<String, serde_json::Error> {
    serde_json::to_string(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Matrix;
    use crate::state::{HubState, RuntimeStats};

    fn hub_state() -> HubState {
        let m = Matrix::from_toml(
            r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2
"#,
        )
        .unwrap();
        HubState::snapshot(&m, "1.7.0-test", &[RuntimeStats::default()])
    }

    #[test]
    fn ws_encode_is_the_state_json() {
        let hs = hub_state();
        let text = ws_encode(&hs).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], "1.7.0-test");
        assert_eq!(v["participants"][0]["name"], "cam1");
    }

    #[test]
    fn version_constant_is_populated() {
        assert!(!VERSION.is_empty());
    }
}
