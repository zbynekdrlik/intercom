//! The phone signalling routes of the in-hub WebRTC leg (intercom#42), merged into the hub's
//! router next to `crate::http::router`:
//!
//! - `GET /api/phone` -> `{"transport": "webrtc" | "janus"}`: the page's transport seam. A hub
//!   without the webrtc leg says `janus` (the page then behaves exactly as before);
//! - `POST /api/phone/offer {sdp, display}` -> `{id, sdp}` (400 bad body / offer, 413 too large,
//!   503 every slot taken or no webrtc leg);
//! - `POST /api/phone/<id>/display {display}` -> 204 (404 unknown id);
//! - `DELETE /api/phone/<id>` -> 204 (404 unknown id).
//!
//! The body is read with a hard limit ([`MAX_SIGNAL_BODY_BYTES`]) before any parsing.

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};

use crate::webrtc_leg::WebrtcHandle;
use crate::webrtc_signal::{
    parse_display_body, parse_offer_body, PhoneId, SignalError, MAX_SIGNAL_BODY_BYTES,
    TRANSPORT_JANUS, TRANSPORT_WEBRTC,
};

/// The routes' state: the network task's handle, `None` on a hub without the webrtc leg.
#[derive(Clone, Debug)]
pub struct PhoneApi {
    pub leg: Option<WebrtcHandle>,
}

/// The phone signalling routes.
pub fn phone_router(api: PhoneApi) -> Router {
    Router::new()
        .route("/api/phone", get(transport))
        .route("/api/phone/offer", post(offer))
        .route("/api/phone/:id/display", post(display))
        .route("/api/phone/:id", delete(leave))
        // One byte over the limit is enough for parse_* to answer 413 instead of axum's own.
        .layer(DefaultBodyLimit::max(MAX_SIGNAL_BODY_BYTES + 1))
        .with_state(api)
}

/// The JSON `GET /api/phone` answers.
pub fn transport_json(webrtc: bool) -> serde_json::Value {
    serde_json::json!({
        "transport": if webrtc { TRANSPORT_WEBRTC } else { TRANSPORT_JANUS }
    })
}

fn refused(e: SignalError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}

async fn transport(State(api): State<PhoneApi>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(transport_json(api.leg.is_some())),
    )
        .into_response()
}

async fn offer(State(api): State<PhoneApi>, body: Bytes) -> Response {
    let Some(leg) = api.leg else {
        return refused(SignalError::Unavailable);
    };
    let req = match parse_offer_body(&body) {
        Ok(r) => r,
        Err(e) => {
            leg.shared
                .offers_refused
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(error = %e, "webrtc: offer refused");
            return refused(e);
        }
    };
    match leg.offer(req).await {
        Ok(a) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({ "id": a.id.to_string(), "sdp": a.sdp })),
        )
            .into_response(),
        Err(e) => refused(e),
    }
}

async fn display(State(api): State<PhoneApi>, Path(id): Path<String>, body: Bytes) -> Response {
    let Some(leg) = api.leg else {
        return refused(SignalError::Unavailable);
    };
    let Some(id) = PhoneId::parse(&id) else {
        return refused(SignalError::UnknownPhone);
    };
    let name = match parse_display_body(&body) {
        Ok(n) => n,
        Err(e) => return refused(e),
    };
    match leg.set_display(id, name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => refused(e),
    }
}

async fn leave(State(api): State<PhoneApi>, Path(id): Path<String>) -> Response {
    let Some(leg) = api.leg else {
        return refused(SignalError::Unavailable);
    };
    let Some(id) = PhoneId::parse(&id) else {
        return refused(SignalError::UnknownPhone);
    };
    match leg.close(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => refused(e),
    }
}
