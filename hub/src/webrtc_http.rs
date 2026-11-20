//! The phone signalling routes of the in-hub WebRTC leg (intercom#42), merged into the hub's
//! router next to `crate::http::router`:
//!
//! - `GET /api/phone` -> `{"transport": "webrtc" | "janus"}`: the page's transport seam. A hub
//!   without the webrtc leg says `janus` (the page then behaves exactly as before);
//! - `POST /api/phone/offer {sdp, display, replaces?}` -> `{id, sdp}` (415 not JSON, 429 rate
//!   limited, 413 too large, 400 bad body / offer, 503 no slot or no webrtc leg);
//! - `POST /api/phone/<id>/display {display}` -> 204 (415, 404 unknown id);
//! - `DELETE /api/phone/<id>` -> 204, also for an id the hub already dropped (the page never logs
//!   a 404 for a phone that left by itself).
//!
//! The route is public (cloudflared, no credential), so an offer is checked in the cheapest order:
//! the content type (a page elsewhere cannot send JSON without a CORS preflight, and the hub
//! answers none), the per-client rate limit, the body limit, the slot (on the media task), and only
//! then the SDP parse, the DTLS certificate and the answer, on the blocking pool. Every refusal is
//! counted on `/api/state`; a warning names the count at most every 10 s.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Extension, Router,
};

use crate::webrtc_leg::WebrtcHandle;
use crate::webrtc_signal::{
    client_key, is_json_content_type, parse_display_body, parse_offer_body, PhoneId, RateLimiter,
    RefusalLog, SignalError, MAX_SIGNAL_BODY_BYTES, OFFER_BURST, OFFER_PER_S, TRANSPORT_JANUS,
    TRANSPORT_WEBRTC,
};

/// The offer route's guards: the per-client rate limit and the refusal log.
#[derive(Debug)]
pub struct OfferGuard {
    pub rate: RateLimiter,
    pub log: RefusalLog,
}

/// The routes' state: the network task's handle (`None` on a hub without the webrtc leg) and the
/// offer guards.
#[derive(Clone, Debug)]
pub struct PhoneApi {
    pub leg: Option<WebrtcHandle>,
    pub guard: Arc<Mutex<OfferGuard>>,
}

impl PhoneApi {
    /// The routes for `leg`, with the default offer rate limit.
    pub fn new(leg: Option<WebrtcHandle>) -> Self {
        Self::with_rate(leg, RateLimiter::new(OFFER_BURST, OFFER_PER_S))
    }

    /// The routes with a given rate limiter (the tests use their own).
    pub fn with_rate(leg: Option<WebrtcHandle>, rate: RateLimiter) -> Self {
        PhoneApi {
            leg,
            guard: Arc::new(Mutex::new(OfferGuard {
                rate,
                log: RefusalLog::default(),
            })),
        }
    }
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

/// Count a refused offer; at most every 10 s one warning with the count since the last.
fn count_refusal(api: &PhoneApi, leg: &WebrtcHandle, e: &SignalError, client: &str) {
    leg.shared.offers_refused.fetch_add(1, Ordering::Relaxed);
    if *e == SignalError::RateLimited {
        leg.shared
            .offers_rate_limited
            .fetch_add(1, Ordering::Relaxed);
    }
    let due = api
        .guard
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .log
        .note(Instant::now());
    if let Some(count) = due {
        tracing::warn!(
            refused_since_last_line = count,
            last_reason = %e,
            last_client = %client,
            "webrtc: phone offers refused (counted on /api/state webrtc.offers_refused)"
        );
    }
}

async fn offer(
    State(api): State<PhoneApi>,
    headers: HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: Bytes,
) -> Response {
    let Some(leg) = api.leg.clone() else {
        return refused(SignalError::Unavailable);
    };
    let header_str = |name: header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    let client = client_key(
        headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok()),
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        peer.map(|Extension(ConnectInfo(a))| a.ip()),
    );
    let checked = (|| {
        if !is_json_content_type(header_str(header::CONTENT_TYPE)) {
            return Err(SignalError::UnsupportedMediaType);
        }
        let allowed = api
            .guard
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .rate
            .allow(&client, Instant::now());
        if !allowed {
            return Err(SignalError::RateLimited);
        }
        parse_offer_body(&body)
    })();
    let result = match checked {
        Ok(req) => leg.offer(req).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(a) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({ "id": a.id.to_string(), "sdp": a.sdp })),
        )
            .into_response(),
        Err(e) => {
            count_refusal(&api, &leg, &e, &client);
            refused(e)
        }
    }
}

async fn display(
    State(api): State<PhoneApi>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(leg) = api.leg else {
        return refused(SignalError::Unavailable);
    };
    if !is_json_content_type(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
    ) {
        return refused(SignalError::UnsupportedMediaType);
    }
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
    // Not an id at all: nothing to leave, and nothing worth an error either.
    let Some(id) = PhoneId::parse(&id) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    match leg.close(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => refused(e),
    }
}
