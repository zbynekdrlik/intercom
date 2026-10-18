//! TURN credentials for the phone page and for Janus (issue 1453).
//!
//! A phone on mobile data reaches the intercom page through the strih box's outbound Cloudflare
//! Tunnel, but the WebRTC media needs a relay both ends can reach: the venue uplink is CGNAT, so
//! Janus has only LAN host candidates. Both sides therefore get Cloudflare Realtime TURN
//! credentials, minted here from the box's TURN key:
//!
//! - `GET /api/ice` (the PWA, before it opens the Janus session) -> `{"turn":"on","iceServers":[..]}`.
//!   It ALWAYS answers 200: without a key it is `{"turn":"off","reason":..,"iceServers":[]}`, after
//!   a failed mint `{"turn":"error",..}`. Chromium logs every 4xx/5xx resource load as a console
//!   error, and the page keeps a clean console, so "no TURN" is a 200 the page reads as "use the
//!   default", never an error status.
//! - `GET /api/turn-rest` (Janus's `nat.turn_rest_api`, called per PeerConnection on loopback) ->
//!   the draft-uberti JSON `{"username","password","ttl","uris"}`; 404 with the reason when the box
//!   has no key, 502 when the mint failed.
//!
//! The key (Cloudflare's key id + key API token) lives in a 0600 TOML file, by default
//! `/etc/intercom-hub/turn.toml`. The provisioned hub runs as the operator (setup-strih step 12) and
//! cannot read a root 0600 file, so setup-strih step 13 hands it over as the systemd credential
//! `turn.toml` (only when the file exists); `resolve_turn_path` prefers that copy. An absent,
//! unreadable, group/world-readable or malformed file leaves TURN off with a named reason; the rest
//! of the hub is unchanged. The token never reaches a log line, a Debug print or a response.
//!
//! Credentials are minted for `TURN_TTL_S` and reused until 2/3 of it has passed, so a phone always
//! gets at least a third of the lifetime. Concurrent callers wait on one mint. When a mint fails, a
//! cached set that has not expired yet is served (with a warning) instead of an error.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// The default TURN key file (0600, placed by the supervisor; issue 1453 runbook).
pub const TURN_CONFIG_DEFAULT: &str = "/etc/intercom-hub/turn.toml";
/// The systemd credential id the unit drop-in loads the key file under (`LoadCredential=turn.toml:..`).
pub const TURN_CONFIG_CRED: &str = "turn.toml";
/// Cloudflare Realtime TURN's key API: `<base>/<key id>/credentials/generate-ice-servers`.
pub const CLOUDFLARE_TURN_API: &str = "https://rtc.live.cloudflare.com/v1/turn/keys";
/// The User-Agent every mint request carries. Cloudflare answers 403 to a request WITHOUT one
/// (its bot filter; the supervisor's live finding of 18.10.2026), so it is never left to a default.
pub const TURN_USER_AGENT: &str = "camera-box-intercom/1";
/// The lifetime asked for every credential set, seconds (24 h).
pub const TURN_TTL_S: u64 = 86_400;
/// Never serve a cached set within this many seconds of its expiry.
pub const TURN_EXPIRY_MARGIN_S: u64 = 60;
/// The mint request's own bound (connect + response).
pub const TURN_MINT_TIMEOUT: Duration = Duration::from_secs(5);

/// The box's Cloudflare TURN key. `Debug` never prints the token.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnKey {
    pub key_id: String,
    key_api_token: String,
}

impl TurnKey {
    /// A key from its two parts, checked like the file's values.
    pub fn new(key_id: &str, key_api_token: &str) -> Result<Self, String> {
        let key_id = key_id.trim();
        let key_api_token = key_api_token.trim();
        if key_id.is_empty() {
            return Err("key_id is empty".into());
        }
        if !key_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("key_id may only hold letters, digits, '-' and '_'".into());
        }
        if key_api_token.is_empty() {
            return Err("key_api_token is empty".into());
        }
        if !key_api_token.chars().all(|c| c.is_ascii_graphic()) {
            return Err("key_api_token holds a space or a control character".into());
        }
        Ok(Self {
            key_id: key_id.to_string(),
            key_api_token: key_api_token.to_string(),
        })
    }

    /// The first 8 characters of the key id, for log lines.
    pub fn id_hint(&self) -> String {
        self.key_id.chars().take(8).collect()
    }
}

impl fmt::Debug for TurnKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnKey")
            .field("key_id", &self.key_id)
            .field("key_api_token", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnKeyFile {
    key_id: String,
    key_api_token: String,
}

/// Parse the key file's TOML (`key_id = ".."`, `key_api_token = ".."`). The error never quotes the
/// file's text.
pub fn parse_turn_key(text: &str) -> Result<TurnKey, String> {
    let file: TurnKeyFile = toml::from_str(text).map_err(|e| {
        format!(
            "not a TURN key file (want key_id + key_api_token): {}",
            e.message()
        )
    })?;
    TurnKey::new(&file.key_id, &file.key_api_token)
}

/// The key file to read: the systemd credential copy when the unit loaded one, else the configured
/// path.
pub fn resolve_turn_path(credentials_dir: Option<&str>, configured: &Path) -> PathBuf {
    if let Some(dir) = credentials_dir.map(str::trim).filter(|d| !d.is_empty()) {
        let cred = Path::new(dir).join(TURN_CONFIG_CRED);
        if cred.is_file() {
            return cred;
        }
    }
    configured.to_path_buf()
}

/// Read + check the key file. `Err` is the reason TURN stays off (named, never the token).
pub fn load_turn_key(path: &Path) -> Result<TurnKey, String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("no TURN key file at {}", path.display()))
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is readable by group/others (mode {mode:o}) -- chmod 600 it",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse_turn_key(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// One minted credential set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceCredentials {
    /// Every usable url Cloudflare returned (stun + turn + turns), in its order.
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

/// A url on port 53: browsers refuse it (Cloudflare's own docs say to drop those).
pub fn is_port_53(url: &str) -> bool {
    let host_port = url.split('?').next().unwrap_or(url);
    host_port.rsplit(':').next() == Some("53")
}

/// A `turn:` / `turns:` url (Janus's TURN REST parser takes only those).
pub fn is_turn_url(url: &str) -> bool {
    url.starts_with("turn:") || url.starts_with("turns:")
}

fn urls_of(entry: &Value) -> Vec<String> {
    match entry.get("urls") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|u| u.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse Cloudflare's `generate-ice-servers` answer:
/// `{"iceServers":[{"urls":[..],"username":..,"credential":..}]}` (a list; a single object is taken
/// too). Port-53 urls are dropped. `Err` when no turn url or no username/credential came back.
pub fn parse_generate_ice_servers(body: &Value) -> Result<IceCredentials, String> {
    let entries: Vec<&Value> = match body.get("iceServers") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(o @ Value::Object(_)) => vec![o],
        _ => return Err("the answer has no iceServers".into()),
    };
    let mut urls = Vec::new();
    let mut username = None;
    let mut credential = None;
    for e in entries {
        for u in urls_of(e) {
            if !is_port_53(&u) && !urls.contains(&u) {
                urls.push(u);
            }
        }
        if let (Some(u), Some(c)) = (
            e.get("username").and_then(Value::as_str),
            e.get("credential").and_then(Value::as_str),
        ) {
            if username.is_none() && !u.is_empty() && !c.is_empty() {
                username = Some(u.to_string());
                credential = Some(c.to_string());
            }
        }
    }
    if !urls.iter().any(|u| is_turn_url(u)) {
        return Err("the answer has no turn: / turns: url".into());
    }
    match (username, credential) {
        (Some(username), Some(credential)) => Ok(IceCredentials {
            urls,
            username,
            credential,
        }),
        _ => Err("the answer has no username + credential".into()),
    }
}

/// The PWA's `/api/ice` body for a minted set: one RTCIceServer with every url (a browser ignores
/// the credential on a stun url).
pub fn ice_servers_json(c: &IceCredentials) -> Value {
    json!({
        "turn": "on",
        "iceServers": [{
            "urls": c.urls,
            "username": c.username,
            "credential": c.credential,
        }],
    })
}

/// The PWA's `/api/ice` body without TURN (`state` = `off` | `error`): the page then keeps its
/// default ICE servers.
pub fn ice_servers_unavailable_json(state: &str, reason: &str) -> Value {
    json!({ "turn": state, "reason": reason, "iceServers": [] })
}

/// Janus's TURN REST answer (draft-uberti-behave-turn-rest): only the turn/turns urls.
pub fn turn_rest_json(c: &IceCredentials, ttl_left_s: u64) -> Value {
    let uris: Vec<&String> = c.urls.iter().filter(|u| is_turn_url(u)).collect();
    json!({
        "username": c.username,
        "password": c.credential,
        "ttl": ttl_left_s,
        "uris": uris,
    })
}

/// A cached set minted `elapsed_ms` ago is reused until 2/3 of its `ttl_s`.
pub fn cache_fresh(elapsed_ms: u64, ttl_s: u64) -> bool {
    u128::from(elapsed_ms) * 3 < u128::from(ttl_s) * 2000
}

/// Seconds a set minted `elapsed_ms` ago still lives (0 once expired).
pub fn ttl_left_s(elapsed_ms: u64, ttl_s: u64) -> u64 {
    ttl_s.saturating_sub(elapsed_ms / 1000)
}

/// Why `/api/ice` / `/api/turn-rest` have no credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnError {
    /// The box has no usable TURN key (the reason names the file).
    NotConfigured(String),
    /// The Cloudflare mint failed (HTTP status / transport / answer shape).
    Upstream(String),
}

impl fmt::Display for TurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TurnError::NotConfigured(r) => write!(f, "TURN not configured: {r}"),
            TurnError::Upstream(r) => write!(f, "TURN mint failed: {r}"),
        }
    }
}

struct Cached {
    creds: Arc<IceCredentials>,
    at: Instant,
}

enum Source {
    Off(String),
    On {
        key: TurnKey,
        endpoint: String,
        client: reqwest::Client,
    },
}

/// Mints and caches the box's TURN credentials.
pub struct TurnMinter {
    source: Source,
    ttl_s: u64,
    cache: Mutex<Option<Cached>>,
}

impl TurnMinter {
    /// TURN off, with the reason the endpoints report.
    pub fn off(reason: impl Into<String>) -> Self {
        Self {
            source: Source::Off(reason.into()),
            ttl_s: TURN_TTL_S,
            cache: Mutex::new(None),
        }
    }

    /// TURN on with `key` against `api_base` (`CLOUDFLARE_TURN_API`, or a test server).
    pub fn new(key: TurnKey, api_base: &str, ttl_s: u64) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .user_agent(TURN_USER_AGENT)
            .timeout(TURN_MINT_TIMEOUT)
            .connect_timeout(TURN_MINT_TIMEOUT)
            .build()
            .map_err(|e| format!("cannot build the TURN http client: {e}"))?;
        let endpoint = format!(
            "{}/{}/credentials/generate-ice-servers",
            api_base.trim_end_matches('/'),
            key.key_id
        );
        Ok(Self {
            source: Source::On {
                key,
                endpoint,
                client,
            },
            ttl_s: ttl_s.max(1),
            cache: Mutex::new(None),
        })
    }

    /// The hub's start-up path: the key from the credential copy or `configured`, TURN off (one
    /// log line naming why) when it is not usable. Never fails the hub.
    pub fn load(credentials_dir: Option<&str>, configured: &Path, api_base: &str) -> Self {
        let path = resolve_turn_path(credentials_dir, configured);
        let minter = load_turn_key(&path).and_then(|key| {
            let hint = key.id_hint();
            Self::new(key, api_base, TURN_TTL_S).map(|m| (m, hint))
        });
        match minter {
            Ok((m, hint)) => {
                tracing::info!(path = %path.display(), key = %hint, "turn: Cloudflare TURN key loaded -- /api/ice + /api/turn-rest mint credentials");
                m
            }
            Err(reason) => {
                tracing::info!(%reason, "turn: off -- /api/ice answers turn=off, /api/turn-rest 404 (issue 1453)");
                Self::off(reason)
            }
        }
    }

    /// The reason TURN is off, `None` when a key is loaded.
    pub fn off_reason(&self) -> Option<&str> {
        match &self.source {
            Source::Off(r) => Some(r),
            Source::On { .. } => None,
        }
    }

    /// The current credential set and its remaining lifetime (seconds).
    pub async fn current(&self) -> Result<(Arc<IceCredentials>, u64), TurnError> {
        let (key, endpoint, client) = match &self.source {
            Source::Off(r) => return Err(TurnError::NotConfigured(r.clone())),
            Source::On {
                key,
                endpoint,
                client,
            } => (key, endpoint, client),
        };
        // Held across the mint on purpose: concurrent callers share one request.
        let mut cache = self.cache.lock().await;
        if let Some(c) = cache.as_ref() {
            let elapsed = elapsed_ms(c.at);
            if cache_fresh(elapsed, self.ttl_s) {
                return Ok((c.creds.clone(), ttl_left_s(elapsed, self.ttl_s)));
            }
        }
        match mint(client, endpoint, key, self.ttl_s).await {
            Ok(creds) => {
                tracing::info!(
                    ttl_s = self.ttl_s,
                    urls = creds.urls.len(),
                    "turn: minted Cloudflare TURN credentials"
                );
                let creds = Arc::new(creds);
                *cache = Some(Cached {
                    creds: creds.clone(),
                    at: Instant::now(),
                });
                Ok((creds, self.ttl_s))
            }
            Err(e) => {
                if let Some(c) = cache.as_ref() {
                    let left = ttl_left_s(elapsed_ms(c.at), self.ttl_s);
                    if left > TURN_EXPIRY_MARGIN_S {
                        tracing::warn!(error = %e, ttl_left_s = left, "turn: mint failed -- serving the cached credentials");
                        return Ok((c.creds.clone(), left));
                    }
                }
                tracing::warn!(error = %e, "turn: mint failed and no usable cached credentials");
                Err(TurnError::Upstream(e))
            }
        }
    }
}

fn elapsed_ms(at: Instant) -> u64 {
    u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

async fn mint(
    client: &reqwest::Client,
    endpoint: &str,
    key: &TurnKey,
    ttl_s: u64,
) -> Result<IceCredentials, String> {
    let resp = client
        .post(endpoint)
        .bearer_auth(&key.key_api_token)
        .json(&json!({ "ttl": ttl_s }))
        .send()
        .await
        .map_err(|e| format!("request failed: {}", e.without_url()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("reading the answer failed: {}", e.without_url()))?;
    if !status.is_success() {
        let snippet: String = text.chars().take(200).collect();
        return Err(format!("HTTP {}: {}", status.as_u16(), snippet.trim()));
    }
    let body: Value = serde_json::from_str(&text)
        .map_err(|e| format!("HTTP {} with a non-JSON answer: {e}", status.as_u16()))?;
    parse_generate_ice_servers(&body)
}
