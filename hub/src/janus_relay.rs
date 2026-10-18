//! The phone's Janus WebSocket, relayed through the hub (issue 1453).
//!
//! nginx's `location = /janus` (the box's interkom site, `scripts/lib/strih-https.sh`) points at the
//! hub's `GET /janus`, and Janus's own WebSocket transport listens on loopback only (step 14 of
//! setup-strih), so every phone, on the venue LAN or through the tunnel, talks to Janus through this
//! relay.
//! Per connection:
//! 1. the hub first opens its own WebSocket to Janus ([`JanusWsRelay::connect`], subprotocol
//!    `janus-protocol`, bounded by [`JANUS_CONNECT_TIMEOUT`]); only then does it answer the phone's
//!    upgrade (101), so a 101 through nginx still means "Janus is reachable" (verify item 45);
//! 2. [`JanusWsRelay::run`] relays both ways: each client text frame goes through
//!    [`crate::janus_ws_guard::check_client_frame`] and is forwarded as the SAME string when allowed,
//!    or answered with the guard's Janus-shaped error and never sent to Janus; Janus's frames go to
//!    the phone unchanged;
//! 3. either side closing ends both (Janus then destroys the sessions that socket created).
//!
//! The path is public and unauthenticated, so the number of relays at once is capped
//! ([`MAX_CONCURRENT_RELAYS`]): each holds two sockets in the production audio daemon, and an
//! uncapped flood would exhaust its file descriptors and break the audio legs' respawns. A phone
//! over the cap gets 503 before the hub opens anything towards Janus.
//!
//! The hub's own audiobridge leg (`janus_rtp`) uses Janus's loopback HTTP API, never this path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::http::HeaderMap;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as JanusMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::janus_ws_guard::{check_client_frame, Verdict, JANUS_WS_PROTOCOL};

/// How long the hub waits for Janus's WebSocket handshake before it answers the phone 502.
pub const JANUS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The largest message the hub accepts from a phone (an SDP offer is a few kB).
pub const JANUS_WS_MAX_MESSAGE_BYTES: usize = 256 * 1024;
/// After this many refused frames on one connection the hub closes it (a client that keeps sending
/// what the page never sends is not the page; this also bounds the warning lines it can cause).
pub const MAX_REFUSALS_PER_CONNECTION: u32 = 8;
/// How many phone relays may run at once (a crew is ~10 phones; each relay holds two sockets).
pub const MAX_CONCURRENT_RELAYS: usize = 64;
/// How many characters of a client-sent header the log quotes.
const PEER_QUOTE_MAX_CHARS: usize = 64;

/// The hub's WebSocket to Janus.
pub type JanusUpstream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Where the relay connects, which room a phone may join (`[janus].ws_url` / `[janus].room`) and
/// the slots that cap how many relays run at once.
#[derive(Debug, Clone)]
pub struct JanusWsRelay {
    ws_url: String,
    room: u64,
    slots: Arc<Semaphore>,
    /// Whether the last reservation found every slot in use (the cap warning fires on a change only).
    full: Arc<AtomicBool>,
}

impl JanusWsRelay {
    /// A relay with [`MAX_CONCURRENT_RELAYS`] slots.
    pub fn new(ws_url: &str, room: u64) -> Self {
        Self {
            ws_url: ws_url.to_string(),
            room,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_RELAYS)),
            full: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Record that a phone found every slot in use. True only on the change from "a slot was
    /// free" (the caller warns once per episode, not once per refused phone).
    pub fn mark_full(&self) -> bool {
        !self.full.swap(true, Ordering::Relaxed)
    }

    /// Record that a phone got a slot. True only on the change from "every slot was in use".
    pub fn mark_free(&self) -> bool {
        self.full.swap(false, Ordering::Relaxed)
    }

    /// The same relay with `max` slots (at least one).
    pub fn with_max_relays(mut self, max: usize) -> Self {
        self.slots = Arc::new(Semaphore::new(max.max(1)));
        self
    }

    /// Take one relay slot, or `None` when every slot is in use. The slot is held until the permit
    /// is dropped (the end of [`JanusWsRelay::run`]).
    pub fn try_reserve(&self) -> Option<OwnedSemaphorePermit> {
        self.slots.clone().try_acquire_owned().ok()
    }

    pub fn room(&self) -> u64 {
        self.room
    }

    pub fn ws_url(&self) -> &str {
        &self.ws_url
    }

    /// Open the hub's WebSocket to Janus (subprotocol `janus-protocol`), bounded by
    /// [`JANUS_CONNECT_TIMEOUT`].
    pub async fn connect(&self) -> anyhow::Result<JanusUpstream> {
        let mut request = self.ws_url.as_str().into_client_request()?;
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static(JANUS_WS_PROTOCOL),
        );
        let connected = tokio::time::timeout(
            JANUS_CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "no answer from {} within {} s",
                self.ws_url,
                JANUS_CONNECT_TIMEOUT.as_secs()
            )
        })??;
        Ok(connected.0)
    }

    /// Relay one phone connection until either side closes. `peer` names the phone in the log;
    /// `_slot` is the relay slot this connection holds, released when the relay ends.
    pub async fn run(
        &self,
        mut client: WebSocket,
        mut janus: JanusUpstream,
        peer: String,
        _slot: OwnedSemaphorePermit,
    ) {
        let mut refusals: u32 = 0;
        tracing::info!(peer = %peer, "janus relay: phone connected");
        loop {
            tokio::select! {
                from_phone = client.recv() => {
                    let Some(Ok(msg)) = from_phone else { break };
                    match msg {
                        Message::Text(text) => match check_client_frame(&text, self.room) {
                            Verdict::Forward => {
                                if janus.send(JanusMessage::Text(text)).await.is_err() {
                                    break;
                                }
                            }
                            Verdict::Refuse(refusal) => {
                                refusals += 1;
                                tracing::warn!(
                                    peer = %peer,
                                    code = refusal.code,
                                    reason = %refusal.reason,
                                    "janus relay: refused a frame from the phone (never sent to Janus)"
                                );
                                if client.send(Message::Text(refusal.reply)).await.is_err()
                                    || refusals >= MAX_REFUSALS_PER_CONNECTION
                                {
                                    break;
                                }
                            }
                        },
                        Message::Binary(_) => {
                            refusals += 1;
                            tracing::warn!(
                                peer = %peer,
                                "janus relay: a binary frame from the phone -- closing (Janus speaks JSON text)"
                            );
                            break;
                        }
                        Message::Close(_) => break,
                        Message::Ping(_) | Message::Pong(_) => {}
                    }
                }
                from_janus = janus.next() => {
                    let Some(Ok(msg)) = from_janus else { break };
                    let out = match msg {
                        JanusMessage::Text(text) => Message::Text(text),
                        JanusMessage::Binary(bytes) => Message::Binary(bytes),
                        JanusMessage::Close(_) => break,
                        // Ping / pong: tungstenite answers Janus's pings itself.
                        _ => continue,
                    };
                    if client.send(out).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = janus.close(None).await;
        let _ = client.send(Message::Close(None)).await;
        tracing::info!(peer = %peer, refusals, "janus relay: phone disconnected");
    }
}

/// The phone's address for the log: nginx's `X-Forwarded-For` (the connecting address nginx saw,
/// 127.0.0.1 for the tunnel) and Cloudflare's `CF-Connecting-IP` (the phone behind the tunnel). Both
/// are headers a client can also send itself (a LAN client reaching :8790 directly), so the log
/// shows both, cut and Debug-quoted, and never treats either as proof.
pub fn phone_peer(headers: &HeaderMap) -> String {
    let field = |name: &str| -> String {
        let raw = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");
        let cut: String = raw.chars().take(PEER_QUOTE_MAX_CHARS).collect();
        format!("{cut:?}")
    };
    format!(
        "xff={} cf={}",
        field("x-forwarded-for"),
        field("cf-connecting-ip")
    )
}
