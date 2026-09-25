//! `establish_session` against a FAKE Janus HTTP server (issue 1345 M3a): create → attach → join,
//! and the plugin's own rtp ip/port parsed out of the `joined` reply. A tokio + axum stand-in
//! implements the three POST endpoints so the adapter's session handshake is exercised end-to-end
//! without a real Janus. CI is the first compile (Tier-0 #557).

use axum::extract::Path;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use intercom_hub::janus_rtp::{establish_session, JanusCodec, JoinSpec, OPUS_PAYLOAD_TYPE};

async fn fake_create(Json(_body): Json<Value>) -> Json<Value> {
    Json(json!({ "janus": "success", "transaction": "create", "data": { "id": 100 } }))
}

async fn fake_session(Path(_sid): Path<u64>, Json(_body): Json<Value>) -> Json<Value> {
    // attach -> a handle id.
    Json(json!({ "janus": "success", "transaction": "attach", "data": { "id": 200 } }))
}

async fn fake_handle(Path((_sid, _hid)): Path<(u64, u64)>, Json(body): Json<Value>) -> Json<Value> {
    // The join request advertises OUR rtp; reply with the plugin's own rtp endpoint (inline joined).
    assert_eq!(body["body"]["request"], "join");
    // The codec and its payload type travel together (issue 1345: Opus with FEC, or PCMU PT 0).
    match body["body"]["codec"].as_str() {
        Some("opus") => {
            assert_eq!(body["body"]["rtp"]["payload_type"], 111);
            assert_eq!(body["body"]["rtp"]["fec"], true);
        }
        Some("pcmu") => assert_eq!(body["body"]["rtp"]["payload_type"], 0),
        other => panic!("unexpected join codec {other:?}"),
    }
    // Janus echoes the payload type it will send the room mix with.
    let pt = body["body"]["rtp"]["payload_type"].clone();
    Json(json!({
        "janus": "event",
        "transaction": "join",
        "plugindata": {
            "plugin": "janus.plugin.audiobridge",
            "data": {
                "audiobridge": "joined",
                "room": body["body"]["room"],
                "id": 4242,
                "rtp": { "ip": "127.0.0.1", "port": 5005, "payload_type": pt },
                "participants": []
            }
        }
    }))
}

async fn spawn_fake_janus(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/janus")
}

#[tokio::test]
async fn establish_session_handshakes_and_parses_the_plugin_rtp_endpoint() {
    let app = Router::new()
        .route("/janus", post(fake_create))
        .route("/janus/:sid", post(fake_session))
        .route("/janus/:sid/:hid", post(fake_handle));
    let api_url = spawn_fake_janus(app).await;

    let client = reqwest::Client::new();
    let session = establish_session(&client, &api_url, &spec(JanusCodec::Pcmu))
        .await
        .expect("the handshake must succeed against the fake Janus");

    assert_eq!(session.session_id, 100);
    assert_eq!(session.handle_id, 200);
    assert_eq!(session.janus_rtp_addr.ip().to_string(), "127.0.0.1");
    assert_eq!(session.janus_rtp_addr.port(), 5005);
    assert_eq!(session.payload_type, 0);
}

#[tokio::test]
async fn establish_session_joins_as_opus_and_keeps_the_answered_payload_type() {
    let app = Router::new()
        .route("/janus", post(fake_create))
        .route("/janus/:sid", post(fake_session))
        .route("/janus/:sid/:hid", post(fake_handle));
    let api_url = spawn_fake_janus(app).await;

    let client = reqwest::Client::new();
    let session = establish_session(&client, &api_url, &spec(JanusCodec::Opus))
        .await
        .expect("the Opus handshake must succeed against the fake Janus");
    // The room mix comes back with the payload type Janus answered, which the receiver filters on.
    assert_eq!(session.payload_type, OPUS_PAYLOAD_TYPE);
    assert_eq!(session.janus_rtp_addr.port(), 5005);
}

fn spec(codec: JanusCodec) -> JoinSpec<'static> {
    JoinSpec {
        room: 1000,
        display: "strih-lx-hub",
        local_ip: "127.0.0.1",
        local_port: 6990,
        codec,
        secret: None,
        pin: None,
    }
}

async fn err_create(Json(_b): Json<Value>) -> Json<Value> {
    Json(
        json!({ "janus": "error", "transaction": "create", "error": { "code": 490, "reason": "unauthorized" } }),
    )
}

#[tokio::test]
async fn establish_session_surfaces_a_transport_error() {
    let app = Router::new().route("/janus", post(err_create));
    let api_url = spawn_fake_janus(app).await;

    let client = reqwest::Client::new();
    let err = establish_session(&client, &api_url, &spec(JanusCodec::Opus))
        .await
        .expect_err("a Janus error reply must bubble up");
    let msg = err.to_string();
    assert!(
        msg.contains("490") && msg.contains("unauthorized"),
        "got: {msg}"
    );
}
