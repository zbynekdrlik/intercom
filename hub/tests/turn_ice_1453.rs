//! TURN credentials for the phone page and Janus (issue 1453): the key file, the Cloudflare
//! `generate-ice-servers` answer, the cache, and `/api/ice` + `/api/turn-rest` served by the REAL hub
//! router against a FAKE Cloudflare (a tokio + axum stand-in, the `janus_session.rs` pattern). The
//! fake answers 403 to a request without a User-Agent, exactly like the real API (the supervisor's
//! live finding of 18.10.2026), so the minter's explicit User-Agent is pinned. CI is the first
//! compile (Tier-0 #557).

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::watch;

use intercom_hub::http::{router, AppState};
use intercom_hub::matrix::Matrix;
use intercom_hub::state::{HubState, RuntimeStats};
use intercom_hub::turn::{
    backoff_active, cache_fresh, expiry_margin_s, ice_servers_json, is_port_53, load_turn_key,
    parse_generate_ice_servers, parse_turn_key, refresh_wait_s, resolve_turn_path, ttl_left_s,
    turn_rest_json, TurnError, TurnKey, TurnMinter, TURN_CONFIG_CRED, TURN_EXPIRY_MARGIN_S,
    TURN_REFRESH_RETRY_S, TURN_USER_AGENT,
};

const KEY_ID: &str = "fake-key-id-1453";
const KEY_TOKEN: &str = "fake-turn-api-token-of-the-test";

/// The answer shape read live on 18.10.2026 (201, one entry, `urls` a list).
fn live_answer() -> Value {
    json!({"iceServers": [{
        "urls": [
            "stun:stun.cloudflare.com:3478",
            "turn:turn.cloudflare.com:3478?transport=udp",
            "turn:turn.cloudflare.com:3478?transport=tcp",
            "turns:turn.cloudflare.com:5349?transport=tcp",
            "turn:turn.cloudflare.com:443?transport=udp",
            "turn:turn.cloudflare.com:80?transport=tcp",
            "turns:turn.cloudflare.com:443?transport=tcp"
        ],
        "username": "u-minted",
        "credential": "c-minted"
    }]})
}

/// A private scratch dir, removed when the test ends (never a pid+timestamp path).
fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("turn-1453-")
        .tempdir()
        .unwrap()
}

fn write_key_file(path: &std::path::Path, text: &str, mode: u32) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn key_text() -> String {
    format!("key_id = \"{KEY_ID}\"\nkey_api_token = \"{KEY_TOKEN}\"\n")
}

// ---- the key file ---------------------------------------------------------------------------

#[test]
fn a_key_file_parses_and_debug_never_prints_the_token() {
    let key = parse_turn_key(&key_text()).expect("a valid key file");
    assert_eq!(key.key_id, KEY_ID);
    let dbg = format!("{key:?}");
    assert!(!dbg.contains(KEY_TOKEN), "Debug leaks the token: {dbg}");
    assert!(dbg.contains("redacted"));
    assert_eq!(key.id_hint(), "fake-key");
}

#[test]
fn a_bad_key_file_is_refused_without_quoting_its_text() {
    for bad in [
        "key_id = \"\"\nkey_api_token = \"x\"\n",
        "key_id = \"a/b\"\nkey_api_token = \"x\"\n",
        "key_id = \"abc\"\nkey_api_token = \"has space\"\n",
        "key_id = \"abc\"\n",
        "key_id = \"abc\"\nkey_api_token = \"x\"\nextra = 1\n",
        "not toml at all = = =",
    ] {
        let err = parse_turn_key(bad).expect_err(bad);
        assert!(
            !err.contains("has space"),
            "the error quotes the file: {err}"
        );
    }
    assert!(TurnKey::new("abc", "tok").is_ok());
}

#[test]
fn an_absent_or_open_key_file_leaves_turn_off_with_a_named_reason() {
    let tmp = scratch();
    let d = tmp.path();
    let missing = d.join("turn.toml");
    let err = load_turn_key(&missing).expect_err("absent");
    assert!(err.contains("no TURN key file at"), "{err}");
    assert!(err.contains(missing.to_str().unwrap()), "{err}");

    write_key_file(&missing, &key_text(), 0o644);
    let err = load_turn_key(&missing).expect_err("group/world readable");
    assert!(
        err.contains("mode 644") && err.contains("chmod 600"),
        "{err}"
    );
    assert!(!err.contains(KEY_TOKEN));

    write_key_file(&missing, &key_text(), 0o600);
    assert_eq!(load_turn_key(&missing).unwrap().key_id, KEY_ID);
    // systemd's credential copy is 0400.
    write_key_file(&missing, &key_text(), 0o400);
    assert!(load_turn_key(&missing).is_ok());
}

#[test]
fn the_systemd_credential_copy_wins_only_when_it_exists() {
    let tmp = scratch();
    let d = tmp.path();
    let configured = d.join("etc-turn.toml");
    let creds = d.join("creds");
    std::fs::create_dir_all(&creds).unwrap();
    // No credential loaded (the box has no key file): the configured path.
    assert_eq!(resolve_turn_path(None, &configured), configured);
    assert_eq!(resolve_turn_path(Some(""), &configured), configured);
    assert_eq!(
        resolve_turn_path(Some(creds.to_str().unwrap()), &configured),
        configured,
        "a credentials dir without turn.toml (the Janus secret only) falls back"
    );
    std::fs::write(creds.join(TURN_CONFIG_CRED), "x").unwrap();
    assert_eq!(
        resolve_turn_path(Some(creds.to_str().unwrap()), &configured),
        creds.join(TURN_CONFIG_CRED)
    );
}

// ---- the Cloudflare answer -----------------------------------------------------------------

#[test]
fn the_live_answer_shape_parses_and_janus_gets_only_the_turn_urls() {
    let c = parse_generate_ice_servers(&live_answer()).expect("the live shape");
    assert_eq!(c.username, "u-minted");
    assert_eq!(c.credential, "c-minted");
    assert_eq!(c.urls.len(), 7);

    let ice = ice_servers_json(&c);
    assert_eq!(ice["turn"], "on");
    assert_eq!(ice["iceServers"][0]["urls"].as_array().unwrap().len(), 7);
    assert_eq!(ice["iceServers"][0]["username"], "u-minted");
    assert_eq!(ice["iceServers"][0]["credential"], "c-minted");

    let rest = turn_rest_json(&c, 1234);
    assert_eq!(rest["username"], "u-minted");
    assert_eq!(rest["password"], "c-minted");
    assert_eq!(rest["ttl"], 1234);
    let uris: Vec<&str> = rest["uris"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u.as_str().unwrap())
        .collect();
    assert_eq!(uris.len(), 6, "the stun url is not a TURN uri: {uris:?}");
    assert!(uris
        .iter()
        .all(|u| u.starts_with("turn:") || u.starts_with("turns:")));
}

#[test]
fn port_53_urls_are_dropped_and_a_single_object_answer_is_taken() {
    assert!(is_port_53("turn:turn.cloudflare.com:53?transport=udp"));
    assert!(is_port_53("stun:stun.cloudflare.com:53"));
    assert!(!is_port_53("turn:turn.cloudflare.com:5349?transport=tcp"));
    assert!(!is_port_53("turn:host53"));
    let c = parse_generate_ice_servers(&json!({"iceServers": {
        "urls": ["stun:stun.cloudflare.com:53", "turn:turn.cloudflare.com:53?transport=udp",
                 "turn:turn.cloudflare.com:3478?transport=udp"],
        "username": "u", "credential": "c"
    }}))
    .unwrap();
    assert_eq!(c.urls, vec!["turn:turn.cloudflare.com:3478?transport=udp"]);
    // A single url string.
    let c = parse_generate_ice_servers(&json!({"iceServers": [
        {"urls": "stun:stun.cloudflare.com:3478"},
        {"urls": "turns:turn.cloudflare.com:443?transport=tcp", "username": "u", "credential": "c"}
    ]}))
    .unwrap();
    assert_eq!(c.urls.len(), 2);
    assert_eq!(c.username, "u");
}

#[test]
fn an_answer_without_a_turn_url_or_credentials_is_an_error() {
    assert!(parse_generate_ice_servers(&json!({})).is_err());
    assert!(parse_generate_ice_servers(&json!({"iceServers": [
        {"urls": ["stun:stun.cloudflare.com:3478"], "username": "u", "credential": "c"}
    ]}))
    .is_err());
    assert!(parse_generate_ice_servers(&json!({"iceServers": [
        {"urls": ["turn:turn.cloudflare.com:3478?transport=udp"]}
    ]}))
    .is_err());
}

#[test]
fn the_margin_the_backoff_and_the_refresh_wait() {
    assert_eq!(expiry_margin_s(86_400), TURN_EXPIRY_MARGIN_S);
    assert_eq!(expiry_margin_s(300), 30, "at most a tenth of the ttl");
    assert_eq!(expiry_margin_s(3), 0);
    assert!(backoff_active(0, 15));
    assert!(backoff_active(14_999, 15));
    assert!(!backoff_active(15_000, 15));
    assert!(
        !backoff_active(0, 0),
        "a zero back-off never holds a mint back"
    );
    // A fresh 24 h set: renew just past the 16 h point.
    assert_eq!(refresh_wait_s(86_400, 86_400), 57_601);
    assert_eq!(refresh_wait_s(28_801, 86_400), 2);
    // Already past the 2/3 point (a failed mint, the cached set served): retry soon.
    assert_eq!(refresh_wait_s(28_800, 86_400), TURN_REFRESH_RETRY_S);
    assert_eq!(refresh_wait_s(0, 86_400), TURN_REFRESH_RETRY_S);
}

#[test]
fn the_cache_lasts_two_thirds_of_the_ttl() {
    let ttl = 86_400;
    assert!(cache_fresh(0, ttl));
    assert!(cache_fresh(57_599_999, ttl));
    assert!(!cache_fresh(57_600_000, ttl), "2/3 of 24 h = 16 h");
    assert!(!cache_fresh(u64::MAX, ttl));
    assert_eq!(ttl_left_s(0, ttl), ttl);
    assert_eq!(ttl_left_s(57_600_000, ttl), 28_800);
    assert_eq!(ttl_left_s(u64::MAX, ttl), 0);
}

// ---- a fake Cloudflare + the real hub router ------------------------------------------------

#[derive(Clone)]
struct Fake {
    hits: Arc<AtomicUsize>,
    /// The status a request with a User-Agent gets (201 = the live answer), switchable mid-test.
    status: Arc<AtomicU16>,
    /// The ttl of the last request body.
    ttl: Arc<AtomicU64>,
}

async fn fake_generate(
    State(fake): State<Fake>,
    Path(key): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    fake.hits.fetch_add(1, Ordering::SeqCst);
    // The real API's bot filter: no User-Agent = 403.
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ua.is_empty() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "no user agent"})),
        );
    }
    assert_eq!(ua, TURN_USER_AGENT, "the minter's own User-Agent");
    assert_eq!(key, KEY_ID, "the key id is in the path");
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some(format!("Bearer {KEY_TOKEN}").as_str())
    );
    fake.ttl
        .store(body["ttl"].as_u64().unwrap_or(0), Ordering::SeqCst);
    let status = StatusCode::from_u16(fake.status.load(Ordering::SeqCst)).unwrap();
    if status != StatusCode::CREATED {
        return (status, Json(json!({"error": "denied"})));
    }
    (StatusCode::CREATED, Json(live_answer()))
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn fake_cloudflare(status: StatusCode) -> (String, Fake) {
    let fake = Fake {
        hits: Arc::new(AtomicUsize::new(0)),
        status: Arc::new(AtomicU16::new(status.as_u16())),
        ttl: Arc::new(AtomicU64::new(0)),
    };
    let app = Router::new()
        .route(
            "/keys/:key/credentials/generate-ice-servers",
            post(fake_generate),
        )
        .with_state(fake.clone());
    (format!("{}/keys", spawn(app).await), fake)
}

fn hits(fake: &Fake) -> usize {
    fake.hits.load(Ordering::SeqCst)
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

async fn spawn_hub(turn: TurnMinter) -> String {
    // These tests never read /api/state; a receiver keeps its last value after the sender drops.
    let (_tx, rx) = watch::channel(Arc::new(hub_state()));
    spawn(router(AppState {
        live: rx,
        video: None,
        turn: Arc::new(turn),
        janus_ws: None,
    }))
    .await
}

fn key() -> TurnKey {
    TurnKey::new(KEY_ID, KEY_TOKEN).unwrap()
}

#[tokio::test]
async fn the_fake_refuses_a_request_without_a_user_agent_like_cloudflare() {
    let (base, fake) = fake_cloudflare(StatusCode::CREATED).await;
    let r = reqwest::Client::builder()
        .build()
        .unwrap()
        .post(format!("{base}/{KEY_ID}/credentials/generate-ice-servers"))
        .bearer_auth(KEY_TOKEN)
        .json(&json!({"ttl": 86_400}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(hits(&fake), 1);
}

#[tokio::test]
async fn api_ice_and_turn_rest_mint_once_and_share_the_cached_set() {
    let (base, fake) = fake_cloudflare(StatusCode::CREATED).await;
    let hub = spawn_hub(TurnMinter::new(key(), &base, 86_400).unwrap()).await;
    let c = reqwest::Client::new();

    let r = c.get(format!("{hub}/api/ice")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-store");
    let ice: Value = r.json().await.unwrap();
    assert_eq!(ice["turn"], "on");
    assert_eq!(ice["iceServers"][0]["username"], "u-minted");
    assert_eq!(ice["iceServers"][0]["credential"], "c-minted");
    assert_eq!(ice["iceServers"][0]["urls"].as_array().unwrap().len(), 7);

    // Janus calls with its own query parameters.
    let r = c
        .get(format!("{hub}/api/turn-rest?service=turn&username=janus-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let rest: Value = r.json().await.unwrap();
    assert_eq!(rest["username"], "u-minted");
    assert_eq!(rest["password"], "c-minted");
    assert_eq!(rest["uris"].as_array().unwrap().len(), 6);
    let ttl = rest["ttl"].as_u64().unwrap();
    assert!(ttl > 86_000 && ttl <= 86_400, "ttl {ttl}");

    assert_eq!(hits(&fake), 1, "one mint serves both");
    assert_eq!(
        fake.ttl.load(Ordering::SeqCst),
        86_400,
        "the minter asks its own ttl"
    );
    // A burst of page loads still mints nothing new.
    for _ in 0..5 {
        c.get(format!("{hub}/api/ice")).send().await.unwrap();
    }
    assert_eq!(hits(&fake), 1);
}

#[tokio::test]
async fn without_a_key_api_ice_is_a_200_turn_off_and_turn_rest_is_a_named_404() {
    let tmp = scratch();
    let d = tmp.path();
    let path = d.join("turn.toml");
    let hub = spawn_hub(TurnMinter::load(None, &path, "http://127.0.0.1:9/keys")).await;
    let c = reqwest::Client::new();

    let r = c.get(format!("{hub}/api/ice")).send().await.unwrap();
    assert_eq!(r.status(), 200, "never an error status for the page");
    let ice: Value = r.json().await.unwrap();
    assert_eq!(ice["turn"], "off");
    assert_eq!(ice["iceServers"], json!([]));
    assert!(ice["reason"].as_str().unwrap().contains("no TURN key file"));

    let r = c.get(format!("{hub}/api/turn-rest")).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let text = r.text().await.unwrap();
    assert!(
        text.contains("TURN not configured") && text.contains("no TURN key file"),
        "{text}"
    );
}

#[tokio::test]
async fn a_refused_mint_is_turn_error_on_the_page_and_502_for_janus() {
    let (base, fake) = fake_cloudflare(StatusCode::FORBIDDEN).await;
    let hub = spawn_hub(TurnMinter::new(key(), &base, 86_400).unwrap()).await;
    let c = reqwest::Client::new();

    let ice: Value = c
        .get(format!("{hub}/api/ice"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ice["turn"], "error");
    assert_eq!(ice["iceServers"], json!([]));
    assert!(
        ice["reason"].as_str().unwrap().contains("HTTP 403"),
        "{ice}"
    );
    assert!(!ice.to_string().contains(KEY_TOKEN));

    let r = c.get(format!("{hub}/api/turn-rest")).send().await.unwrap();
    assert_eq!(r.status(), 502);
    assert!(r
        .text()
        .await
        .unwrap()
        .contains("TURN mint failed: HTTP 403"));
    assert_eq!(
        hits(&fake),
        1,
        "a failed mint is remembered for the back-off: one attempt, not one per request"
    );
}

#[tokio::test]
async fn without_a_back_off_every_request_tries_again() {
    let (base, fake) = fake_cloudflare(StatusCode::FORBIDDEN).await;
    let m = TurnMinter::new(key(), &base, 86_400)
        .unwrap()
        .with_failure_backoff(0);
    assert!(m.current().await.is_err());
    assert!(m.current().await.is_err());
    assert_eq!(hits(&fake), 2);
}

#[tokio::test]
async fn a_failed_renewal_serves_the_unexpired_set_and_backs_off() {
    // ttl 6 s: the set counts as stale after 4 s, expires at 6 s (margin 0 for so short a ttl);
    // the ~1.9 s between the first sleep and the expiry leaves a slow CI runner room.
    let (base, fake) = fake_cloudflare(StatusCode::CREATED).await;
    let m = TurnMinter::new(key(), &base, 6).unwrap();
    let (first, _) = m.current().await.unwrap();
    assert_eq!(hits(&fake), 1);
    tokio::time::sleep(Duration::from_millis(4_100)).await;
    fake.status
        .store(StatusCode::FORBIDDEN.as_u16(), Ordering::SeqCst);
    let (second, left) = m.current().await.expect("the cached set while it lives");
    assert_eq!(hits(&fake), 2, "the stale set was renewed, and that failed");
    assert_eq!(second, first);
    assert!(left <= 2, "left {left}");
    let (third, _) = m.current().await.expect("still the cached set");
    assert_eq!(third, first);
    assert_eq!(hits(&fake), 2, "within the back-off no new attempt");
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    assert_eq!(
        m.current().await.unwrap_err(),
        TurnError::Upstream("HTTP 403: {\"error\":\"denied\"}".into()),
        "an expired set is never served"
    );
}

#[tokio::test]
async fn the_refresher_warms_the_cache_before_any_request() {
    let (base, fake) = fake_cloudflare(StatusCode::CREATED).await;
    let m = Arc::new(TurnMinter::new(key(), &base, 86_400).unwrap());
    m.spawn_refresher();
    for _ in 0..50 {
        if hits(&fake) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(hits(&fake), 1, "the refresher minted at start");
    m.current().await.unwrap();
    assert_eq!(hits(&fake), 1, "a request finds the warm cache");
    // TURN off: nothing is spawned, nothing is minted.
    let off = Arc::new(TurnMinter::off("no key"));
    off.spawn_refresher();
}

#[tokio::test]
async fn a_loaded_key_file_mints_through_the_minter() {
    let (base, fake) = fake_cloudflare(StatusCode::CREATED).await;
    let tmp = scratch();
    let d = tmp.path();
    let path = d.join("turn.toml");
    write_key_file(&path, &key_text(), 0o600);
    let m = TurnMinter::load(None, &path, &base);
    assert_eq!(m.off_reason(), None);
    let (c, ttl) = m.current().await.expect("minted");
    assert_eq!(c.username, "u-minted");
    assert_eq!(ttl, 86_400);
    assert_eq!(hits(&fake), 1);
    let off = TurnMinter::off("because");
    assert_eq!(
        off.current().await.unwrap_err(),
        TurnError::NotConfigured("because".into())
    );
}
