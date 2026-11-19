//! The cutters' level controls (intercom#34): a second HTTP listener, loopback only.
//!
//! The box's nginx and the public phone front proxy only the main listener (`:8790`), so this one
//! (`[hub] control_bind`, default `127.0.0.1:8791`) is reachable only from the strih notebook
//! itself, where the cutter's cans hang on the card: the cutter opens `http://127.0.0.1:8791/`.
//! It is served by the same accept loop as the main listener ([`crate::http_serve::serve`], the
//! Dante guard included).
//!
//! Routes: `GET /` (the page, `web/levels.html`, version substituted), `GET /levels.js`,
//! `GET /levels.css`, `GET /favicon.svg`, `GET /api/version`, `GET /api/levels` (every slider +
//! the trimmed outputs' meters), `PUT /api/levels/{name}` with `{"db": <f32>, "muted": <bool>}`.
//! A change takes effect on the next mix block (an atomic store) and is written to `levels_file`
//! atomically off the async runtime.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use std::future::Future;

use anyhow::Context as _;
use axum::{
    extract::{ConnectInfo, Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::dante_guard::{check_bind, DanteGuard};
use crate::levels::{is_loopback_host, LevelState, Levels};
use crate::matrix::Matrix;
use crate::state::HubState;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const LEVELS_HTML: &str = include_str!("../../web/levels.html");
const LEVELS_JS: &str = include_str!("../../web/levels.js");
const LEVELS_CSS: &str = include_str!("../../web/levels.css");
const FAVICON_SVG: &str = include_str!("../../web/favicon.svg");

pub const LEVELS_JS_CONTENT_TYPE: &str = "text/javascript; charset=utf-8";
pub const LEVELS_CSS_CONTENT_TYPE: &str = "text/css; charset=utf-8";
pub const SVG_CONTENT_TYPE: &str = "image/svg+xml";

/// The page with the hub version substituted (read from the DOM after a deploy).
pub fn levels_index() -> String {
    LEVELS_HTML.replace("{{VERSION}}", VERSION)
}
/// The page's script.
pub fn levels_js_asset() -> &'static str {
    LEVELS_JS
}
/// The page's stylesheet.
pub fn levels_css_asset() -> &'static str {
    LEVELS_CSS
}

/// What the control routes share.
#[derive(Clone)]
pub struct ControlState {
    pub levels: Levels,
    /// The latest hub snapshot (the outputs' `out_level_dbfs` meters).
    pub live: watch::Receiver<Arc<HubState>>,
    /// Where every change is saved.
    pub levels_file: PathBuf,
}

/// One trimmed output's meter.
#[derive(Debug, Serialize)]
pub struct OutputMeter {
    pub participant: String,
    /// Peak dBFS of its mix over the last second, null before the first second.
    pub out_level_dbfs: Option<f32>,
}

/// `GET /api/levels`.
#[derive(Debug, Serialize)]
pub struct LevelsBody {
    pub version: &'static str,
    pub levels: Vec<LevelState>,
    pub outputs: Vec<OutputMeter>,
}

/// `PUT /api/levels/{name}` body. Both fields, nothing else.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutLevel {
    pub db: f32,
    pub muted: bool,
}

/// `PUT` answer: the slider as it now is, and whether the state file took it.
#[derive(Debug, Serialize)]
pub struct PutAnswer {
    #[serde(flatten)]
    pub level: LevelState,
    pub saved: bool,
}

pub fn control_router(state: ControlState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/levels.js", get(levels_js))
        .route("/levels.css", get(levels_css))
        .route("/favicon.svg", get(favicon))
        .route("/api/version", get(version))
        .route("/api/levels", get(get_levels))
        .route("/api/levels/:name", axum::routing::put(put_level))
        .layer(middleware::from_fn(loopback_host_only))
        .with_state(state)
}

/// Refuse (403) a request whose `Host` / authority, or `Origin` when it has one, is not the
/// loopback itself. Loopback binding keeps other machines out; this keeps out a web page in the
/// notebook's own browser that re-binds its DNS name to 127.0.0.1 (DNS rebinding) and would
/// otherwise be same-origin with the sliders.
async fn loopback_host_only(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()));
    let host_ok = host.as_deref().is_some_and(is_loopback_host);
    let origin_ok = match req.headers().get(header::ORIGIN) {
        None => true,
        Some(o) => o
            .to_str()
            .ok()
            .and_then(|o| o.strip_prefix("http://"))
            .is_some_and(is_loopback_host),
    };
    if host_ok && origin_ok {
        return next.run(req).await;
    }
    tracing::warn!(
        host = host.as_deref().unwrap_or("-"),
        origin = ?req.headers().get(header::ORIGIN),
        path = %req.uri().path(),
        "level controls: refused a request not addressed to the loopback"
    );
    (
        StatusCode::FORBIDDEN,
        "the level controls answer only http://127.0.0.1 / localhost",
    )
        .into_response()
}

/// Start the loopback level controls. A wrong `control_bind` (the Dante guard refuses it) is a
/// config error and stops the hub at start, like every other bind's; a port another process holds
/// is logged and the hub runs on without sliders: the sliders are a convenience, the audio is not
/// (review finding, intercom#34). `Ok(Some(addr))` serving, `Ok(None)` not started.
pub async fn start_level_controls(
    matrix: &Matrix,
    dante_guard: &Arc<DanteGuard>,
    live: watch::Receiver<Arc<HubState>>,
    levels_file: PathBuf,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<Option<SocketAddr>> {
    let addr = crate::levels::check_control_bind(&matrix.hub.control_bind)?;
    check_bind(dante_guard, "control_bind", addr)?;
    match spawn_level_controls(addr, matrix, dante_guard, live, levels_file, shutdown).await {
        Ok(()) => Ok(Some(addr)),
        Err(e) => {
            tracing::error!(
                error = format!("{e:#}"),
                %addr,
                "level controls NOT started: the hub runs on without the cutters' sliders"
            );
            Ok(None)
        }
    }
}

/// Bind `addr` and serve the loopback level controls beside the main listener, through the same
/// Dante-guarded accept loop. [`start_level_controls`] checks the address first and keeps the hub
/// running when the bind fails.
async fn spawn_level_controls(
    addr: SocketAddr,
    matrix: &Matrix,
    dante_guard: &Arc<DanteGuard>,
    live: watch::Receiver<Arc<HubState>>,
    levels_file: PathBuf,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind the level controls {addr}"))?;
    tracing::info!(
        %addr,
        sliders = matrix.levels.len(),
        levels_file = %levels_file.display(),
        "level controls listening (loopback only)"
    );
    let router = control_router(ControlState {
        levels: matrix.levels.clone(),
        live,
        levels_file,
    });
    let guard = dante_guard.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::http_serve::serve(listener, router, guard, shutdown).await {
            tracing::warn!(error = %e, "level controls: the listener ended");
        }
    });
    Ok(())
}

async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(levels_index()))
}

async fn levels_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, LEVELS_JS_CONTENT_TYPE)], LEVELS_JS)
}

async fn levels_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, LEVELS_CSS_CONTENT_TYPE)],
        LEVELS_CSS,
    )
}

async fn favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, SVG_CONTENT_TYPE)], FAVICON_SVG)
}

async fn version() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": VERSION }))
}

/// The body of `GET /api/levels` for the current values and snapshot.
pub fn levels_body(levels: &Levels, snapshot: &HubState) -> LevelsBody {
    let outputs = levels
        .specs()
        .iter()
        .map(|s| s.dst_name.clone())
        .fold(Vec::<String>::new(), |mut acc, n| {
            if !acc.contains(&n) {
                acc.push(n);
            }
            acc
        })
        .into_iter()
        .map(|participant| {
            let out_level_dbfs = snapshot
                .participants
                .iter()
                .find(|p| p.name == participant)
                .and_then(|p| p.out_level_dbfs);
            OutputMeter {
                participant,
                out_level_dbfs,
            }
        })
        .collect();
    LevelsBody {
        version: VERSION,
        levels: levels.states(),
        outputs,
    }
}

async fn get_levels(State(state): State<ControlState>) -> Response {
    let snapshot = state.live.borrow().clone();
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(levels_body(&state.levels, &snapshot)),
    )
        .into_response()
}

async fn put_level(
    State(state): State<ControlState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    Path(name): Path<String>,
    body: Result<Json<PutLevel>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let peer = peer.map_or_else(|| "unknown".to_string(), |ConnectInfo(a)| a.to_string());
    let Some(i) = state.levels.index_of(&name) else {
        tracing::warn!(level = %name, %peer, "level change refused: no such slider");
        return (StatusCode::NOT_FOUND, format!("no level slider '{name}'")).into_response();
    };
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(level = %name, %peer, error = %e.body_text(), "level change refused: bad body");
            return (
                StatusCode::BAD_REQUEST,
                format!(
                    "level '{name}': body must be {{\"db\": <number>, \"muted\": <bool>}}: {}",
                    e.body_text()
                ),
            )
                .into_response();
        }
    };
    let levels = state.levels.clone();
    let path = state.levels_file.clone();
    // The store and its file write happen under the writer lock, off the async runtime: two
    // changes land in the file in the order they took effect.
    let done = tokio::task::spawn_blocking(move || {
        let _w = levels.writer();
        let change = levels.set(i, req.db, req.muted)?;
        let saved = match levels.save_file(&path) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "levels: the state file write failed; the value holds until the hub restarts");
                false
            }
        };
        let level = levels.states().swap_remove(i);
        anyhow::Ok((change, level, saved))
    })
    .await;
    match done {
        Ok(Ok((change, level, saved))) => {
            tracing::info!(
                "level {name} {:+.1} -> {:+.1} dB (muted={}) from {peer}",
                change.old_db,
                change.db,
                change.muted
            );
            Json(PutAnswer { level, saved }).into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(level = %name, %peer, error = %e, "level change refused");
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Err(e) => {
            tracing::warn!(level = %name, error = %e, "level change task failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "level change failed").into_response()
        }
    }
}
