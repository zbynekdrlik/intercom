//! `establish_session` against a FAKE Janus HTTP server (issue 1345 M3a): create → attach → join,
//! and the plugin's own rtp ip/port parsed out of the `joined` reply. A tokio + axum stand-in
//! implements the three POST endpoints so the adapter's session handshake is exercised end-to-end
//! without a real Janus. CI is the first compile (Tier-0 #557).

use axum::extract::Path;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use intercom_hub::janus_rtp::establish_session;

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
    assert_eq!(body["body"]["rtp"]["payload_type"], 0);
    Json(json!({
        "janus": "event",
        "transaction": "join",
        "plugindata": {
            "plugin": "janus.plugin.audiobridge",
            "data": {
                "audiobridge": "joined",
                "room": body["body"]["room"],
                "id": 4242,
                "rtp": { "ip": "127.0.0.1", "port": 5005, "payload_type": 0 },
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
    let session = establish_session(
        &client,
        &api_url,
        1000,
        "strih-lx-hub",
        "127.0.0.1",
        6990,
        None,
    )
    .await
    .expect("the handshake must succeed against the fake Janus");

    assert_eq!(session.session_id, 100);
    assert_eq!(session.handle_id, 200);
    assert_eq!(session.janus_rtp_addr.ip().to_string(), "127.0.0.1");
    assert_eq!(session.janus_rtp_addr.port(), 5005);
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
    let err = establish_session(&client, &api_url, 1000, "hub", "127.0.0.1", 6990, None)
        .await
        .expect_err("a Janus error reply must bubble up");
    let msg = err.to_string();
    assert!(
        msg.contains("490") && msg.contains("unauthorized"),
        "got: {msg}"
    );
}
