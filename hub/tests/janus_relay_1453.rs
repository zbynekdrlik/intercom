//! The phone's Janus WebSocket through the REAL hub router (issue 1453): a FAKE Janus (an axum
//! WebSocket server speaking `janus-protocol`, the `janus_session.rs` pattern) records every frame it
//! receives and answers each with a fixed oddly-formatted text. Proven here:
//! - every frame shape the phone page sends (`intercom/tests/fixtures/janus-ws-pwa-frames.json`,
//!   checked against the page by phone.spec.js) reaches Janus byte-identically, in order, and Janus's
//!   answer reaches the phone byte-identically;
//! - a plain-RTP join (the UDP reflection the owner's ROZHODNUTÉ 18.10.2026 refuses), an attach to
//!   another plugin, a room `create`, an `rtp_forward` and a recording `configure` are answered with
//!   a Janus error and NEVER reach Janus, while the connection keeps relaying the page's frames;
//! - too many refusals or a binary frame close the connection; Janus not answering = 502 before
//!   the upgrade, no Janus room = 503.
//!
//! CI is the first compile (Tier-0 #557).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use intercom_hub::http::{router, AppState};
use intercom_hub::janus_relay::{
    phone_peer, JanusUpstream, JanusWsRelay, MAX_CONCURRENT_RELAYS, MAX_REFUSALS_PER_CONNECTION,
};
use intercom_hub::matrix::Matrix;
use intercom_hub::state::{HubState, RuntimeStats};
use intercom_hub::turn::TurnMinter;

const ROOM: u64 = 1000;
/// What the fake Janus answers to every frame: deliberately NOT serde_json's own formatting, so a
/// relay that re-serialized it would be caught.
const JANUS_REPLY: &str = "{ \"janus\" : \"ack\",  \"transaction\":\"from-fake-janus\" }";
const PWA_FRAMES: &str = include_str!("../../tests/fixtures/janus-ws-pwa-frames.json");

fn pwa_frames() -> Vec<String> {
    serde_json::from_str(PWA_FRAMES).expect("the sample is a JSON array of frame strings")
}

#[derive(Clone, Default)]
struct FakeJanus {
    frames: Arc<Mutex<Vec<String>>>,
    protocols: Arc<Mutex<Vec<String>>>,
}

impl FakeJanus {
    fn frames(&self) -> Vec<String> {
        self.frames.lock().unwrap().clone()
    }
}

async fn fake_upgrade(
    State(fake): State<FakeJanus>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let asked = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    fake.protocols.lock().unwrap().push(asked);
    ws.protocols(["janus-protocol"])
        .on_upgrade(move |socket| fake_session(socket, fake))
}

async fn fake_session(mut socket: WebSocket, fake: FakeJanus) {
    while let Some(Ok(msg)) = socket.recv().await {
        if let AxMessage::Text(text) = msg {
            fake.frames.lock().unwrap().push(text);
            if socket
                .send(AxMessage::Text(JANUS_REPLY.to_string()))
                .await
                .is_err()
            {
                break;
            }
        }
    }
}

async fn spawn(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn spawn_fake_janus() -> (String, FakeJanus) {
    let fake = FakeJanus::default();
    let app = Router::new()
        .route("/janus", get(fake_upgrade))
        .with_state(fake.clone());
    let addr = spawn(app).await;
    (format!("ws://{addr}/janus"), fake)
}

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

async fn spawn_hub(janus_ws: Option<Arc<JanusWsRelay>>) -> SocketAddr {
    let (_tx, rx) = watch::channel(Arc::new(hub_state()));
    spawn(router(AppState {
        live: rx,
        video: None,
        turn: Arc::new(TurnMinter::off("no TURN in this test")),
        janus_ws,
    }))
    .await
}

async fn hub_to(janus_url: &str) -> SocketAddr {
    spawn_hub(Some(Arc::new(JanusWsRelay::new(janus_url, ROOM)))).await
}

/// The HTTP status of a refused upgrade (the hub answered before 101).
async fn upgrade_status(hub: SocketAddr) -> u16 {
    match tokio_tungstenite::connect_async(phone_request(hub)).await {
        Err(WsError::Http(resp)) => resp.status().as_u16(),
        Err(other) => panic!("expected an HTTP refusal, got {other}"),
        Ok(_) => 101,
    }
}

fn phone_request(hub: SocketAddr) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = format!("ws://{hub}/janus").into_client_request().unwrap();
    req.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static("janus-protocol"),
    );
    req
}

/// The phone side, the way janus.js opens it (subprotocol `janus-protocol`).
async fn phone(hub: SocketAddr) -> JanusUpstream {
    let (ws, resp) = tokio_tungstenite::connect_async(phone_request(hub))
        .await
        .expect("the hub upgrades the phone's socket");
    assert_eq!(resp.headers()["sec-websocket-protocol"], "janus-protocol");
    ws
}

/// The next text frame the phone receives, within 5 s.
async fn next_text(ws: &mut JanusUpstream) -> String {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame within 5 s")
            .expect("the socket stays open")
            .expect("a valid frame");
        if let Message::Text(text) = msg {
            return text;
        }
    }
}

/// Whether the hub closes the phone's socket within 5 s (a close frame or the stream ending).
async fn closed_within_5s(ws: &mut JanusUpstream) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Err(_) => return false,
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return true,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

fn plugin_message(body: Value, tx: &str) -> String {
    json!({"janus": "message", "body": body, "transaction": tx, "session_id": 1001, "handle_id": 1002})
        .to_string()
}

fn error_reply(text: &str) -> Value {
    let v: Value = serde_json::from_str(text).expect("the refusal is JSON");
    assert_eq!(v["janus"], "error", "not a Janus error: {text}");
    v
}

#[tokio::test]
async fn every_page_frame_reaches_janus_byte_identically_and_janus_answers_reach_the_phone() {
    let (url, fake) = spawn_fake_janus().await;
    let hub = hub_to(&url).await;
    let mut ws = phone(hub).await;
    let frames = pwa_frames();
    for text in &frames {
        ws.send(Message::Text(text.clone())).await.unwrap();
        assert_eq!(
            next_text(&mut ws).await,
            JANUS_REPLY,
            "Janus's answer must reach the phone unchanged"
        );
    }
    assert_eq!(
        fake.frames(),
        frames,
        "Janus must receive the page's frames byte-identically, in order"
    );
    assert_eq!(
        *fake.protocols.lock().unwrap(),
        vec!["janus-protocol".to_string()],
        "the hub opens ONE Janus socket per phone, asking for janus-protocol"
    );
}

#[tokio::test]
async fn a_plain_rtp_join_is_refused_and_never_reaches_janus() {
    let (url, fake) = spawn_fake_janus().await;
    let hub = hub_to(&url).await;
    let mut ws = phone(hub).await;

    let webrtc_join = plugin_message(
        json!({"request": "join", "room": ROOM, "display": "Kamera 1", "muted": true}),
        "tx-webrtc",
    );
    ws.send(Message::Text(webrtc_join.clone())).await.unwrap();
    assert_eq!(next_text(&mut ws).await, JANUS_REPLY);

    let rtp_join = plugin_message(
        json!({"request": "join", "room": ROOM, "display": "x",
               "rtp": {"ip": "203.0.113.7", "port": 9}}),
        "tx-rtp",
    );
    ws.send(Message::Text(rtp_join)).await.unwrap();
    let refusal = error_reply(&next_text(&mut ws).await);
    assert_eq!(refusal["transaction"], "tx-rtp");
    assert_eq!(refusal["session_id"], 1001);
    assert_eq!(refusal["error"]["code"], 403);
    assert!(refusal["error"]["reason"]
        .as_str()
        .unwrap()
        .contains("plain-RTP"));

    // The connection keeps relaying the page's frames; Janus received the WebRTC join and the
    // follow-up only (Janus processes in order, so a forwarded rtp join would sit between them).
    let configure = plugin_message(json!({"request": "configure", "muted": false}), "tx-after");
    ws.send(Message::Text(configure.clone())).await.unwrap();
    assert_eq!(next_text(&mut ws).await, JANUS_REPLY);
    assert_eq!(fake.frames(), vec![webrtc_join, configure]);
    assert!(fake.frames().iter().all(|f| !f.contains("rtp")));
}

#[tokio::test]
async fn the_other_reflection_and_takeover_requests_never_reach_janus() {
    let (url, fake) = spawn_fake_janus().await;
    let hub = hub_to(&url).await;
    let mut ws = phone(hub).await;
    let attacks = [
        json!({"janus": "attach", "plugin": "janus.plugin.videoroom", "transaction": "a1", "session_id": 1001})
            .to_string(),
        plugin_message(
            json!({"request": "create", "room": 4242, "allow_rtp_participants": true}),
            "a2",
        ),
        plugin_message(
            json!({"request": "rtp_forward", "room": ROOM, "host": "203.0.113.7", "port": 9}),
            "a3",
        ),
        plugin_message(
            json!({"request": "configure", "record": true, "filename": "/tmp/x"}),
            "a4",
        ),
        plugin_message(
            json!({"request": "changeroom", "room": 4242, "rtp": {"ip": "203.0.113.7", "port": 9}}),
            "a5",
        ),
        plugin_message(json!({"request": "join", "room": 4242}), "a6"),
    ];
    for (i, attack) in attacks.iter().enumerate() {
        ws.send(Message::Text(attack.clone())).await.unwrap();
        let refusal = error_reply(&next_text(&mut ws).await);
        assert_eq!(refusal["transaction"], format!("a{}", i + 1));
        assert_eq!(refusal["error"]["code"], 403);
    }
    // Still a working relay, and Janus saw nothing of the above.
    let ok = pwa_frames()[0].clone();
    ws.send(Message::Text(ok.clone())).await.unwrap();
    assert_eq!(next_text(&mut ws).await, JANUS_REPLY);
    assert_eq!(fake.frames(), vec![ok]);
}

#[tokio::test]
async fn too_many_refusals_close_the_connection() {
    let (url, fake) = spawn_fake_janus().await;
    let hub = hub_to(&url).await;
    let mut ws = phone(hub).await;
    let bad = json!({"janus": "claim", "transaction": "c"}).to_string();
    for _ in 0..MAX_REFUSALS_PER_CONNECTION {
        ws.send(Message::Text(bad.clone())).await.unwrap();
        error_reply(&next_text(&mut ws).await);
    }
    assert!(
        closed_within_5s(&mut ws).await,
        "the hub must close after {MAX_REFUSALS_PER_CONNECTION} refusals"
    );
    assert!(fake.frames().is_empty());
}

#[tokio::test]
async fn a_binary_frame_closes_the_connection_and_never_reaches_janus() {
    let (url, fake) = spawn_fake_janus().await;
    let hub = hub_to(&url).await;
    let mut ws = phone(hub).await;
    ws.send(Message::Binary(b"{\"janus\":\"create\"}".to_vec()))
        .await
        .unwrap();
    assert!(closed_within_5s(&mut ws).await);
    assert!(fake.frames().is_empty());
}

#[tokio::test]
async fn janus_not_answering_is_a_502_before_the_upgrade() {
    // A port nothing listens on.
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = unused.local_addr().unwrap().port();
    drop(unused);
    let hub = hub_to(&format!("ws://127.0.0.1:{port}/janus")).await;
    match tokio_tungstenite::connect_async(phone_request(hub)).await {
        Err(WsError::Http(resp)) => assert_eq!(resp.status(), 502),
        Err(other) => panic!("expected an HTTP 502, got {other}"),
        Ok(_) => panic!("the hub upgraded a phone with Janus down"),
    }
}

#[tokio::test]
async fn a_hub_without_a_janus_room_answers_503() {
    let hub = spawn_hub(None).await;
    match tokio_tungstenite::connect_async(phone_request(hub)).await {
        Err(WsError::Http(resp)) => assert_eq!(resp.status(), 503),
        Err(other) => panic!("expected an HTTP 503, got {other}"),
        Ok(_) => panic!("the hub upgraded a phone without a Janus room"),
    }
}

#[tokio::test]
async fn the_janus_js_route_is_not_swallowed_by_the_relay() {
    // `/janus` is an exact route; the vendored library next to it is still the asset.
    let hub = spawn_hub(None).await;
    let r = reqwest::get(format!("http://{hub}/janus.js"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("Janus"));
}

#[test]
fn the_relay_keeps_its_room_and_url() {
    let relay = JanusWsRelay::new("ws://127.0.0.1:8188/janus", 7);
    assert_eq!(relay.room(), 7);
    assert_eq!(relay.ws_url(), "ws://127.0.0.1:8188/janus");
}

#[test]
fn the_default_janus_ws_url_is_janus_on_loopback() {
    let m = Matrix::from_toml(
        r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[janus]
room = 1000
"#,
    )
    .unwrap();
    assert_eq!(m.janus.unwrap().ws_url, "ws://127.0.0.1:8188/janus");
    let err = Matrix::from_toml(
        r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[janus]
ws_url = "http://127.0.0.1:8188/janus"
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("ws://"), "{err}");
}

#[tokio::test]
async fn the_relay_cap_answers_503_before_touching_janus_and_frees_the_slot_on_close() {
    let (url, fake) = spawn_fake_janus().await;
    let relay = JanusWsRelay::new(&url, ROOM).with_max_relays(1);
    let hub = spawn_hub(Some(Arc::new(relay))).await;

    let mut first = phone(hub).await;
    assert_eq!(
        upgrade_status(hub).await,
        503,
        "the second phone is over the cap"
    );
    assert_eq!(
        fake.protocols.lock().unwrap().len(),
        1,
        "a phone over the cap never opens a socket to Janus"
    );

    // The first phone leaves: its slot comes back once its relay ends.
    first.close(None).await.unwrap();
    drop(first);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if upgrade_status(hub).await == 101 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the slot was not freed after the phone left"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn a_relay_holds_at_most_its_slots_and_the_default_is_the_constant() {
    let relay = JanusWsRelay::new("ws://127.0.0.1:8188/janus", ROOM).with_max_relays(2);
    let a = relay.try_reserve().expect("slot 1");
    let _b = relay.try_reserve().expect("slot 2");
    assert!(
        relay.try_reserve().is_none(),
        "a third relay is over the cap"
    );
    drop(a);
    assert!(
        relay.try_reserve().is_some(),
        "a released slot is free again"
    );

    let default = JanusWsRelay::new("ws://127.0.0.1:8188/janus", ROOM);
    let held: Vec<_> = (0..MAX_CONCURRENT_RELAYS)
        .map(|_| default.try_reserve().expect("a slot under the default cap"))
        .collect();
    assert!(default.try_reserve().is_none());
    assert_eq!(held.len(), MAX_CONCURRENT_RELAYS);
    // Zero would refuse every phone: at least one slot.
    assert!(JanusWsRelay::new("ws://x", ROOM)
        .with_max_relays(0)
        .try_reserve()
        .is_some());
}

#[test]
fn the_phone_log_names_both_client_headers_quoted() {
    let mut h = HeaderMap::new();
    assert_eq!(phone_peer(&h), "xff=\"-\" cf=\"-\"");
    h.insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
    h.insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
    assert_eq!(phone_peer(&h), "xff=\"127.0.0.1\" cf=\"203.0.113.7\"");
    h.insert("cf-connecting-ip", "x".repeat(300).parse().unwrap());
    assert!(phone_peer(&h).len() < 120, "a long header is cut");
}

#[test]
fn the_cap_warning_fires_once_per_episode() {
    let relay = JanusWsRelay::new("ws://127.0.0.1:8188/janus", ROOM);
    assert!(!relay.mark_free(), "nothing was full yet");
    assert!(relay.mark_full(), "the first refusal starts an episode");
    assert!(
        !relay.mark_full(),
        "later refusals of the same episode stay quiet"
    );
    assert!(relay.mark_free(), "the next reservation ends it");
    assert!(!relay.mark_free());
    assert!(relay.mark_full(), "a new episode warns again");
}
