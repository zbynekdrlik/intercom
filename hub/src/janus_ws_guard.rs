//! The allowlist the hub applies to every frame a phone sends towards Janus (issue 1453).
//!
//! The interkom page is public (owner ruling 18.10.2026: anyone with the URL may join and talk), and
//! its Janus WebSocket now reaches the internet through the box's tunnel. Janus's audiobridge room
//! must keep `allow_rtp_participants = true` for the hub's OWN leg (`janus_rtp`, over Janus's
//! loopback HTTP API), and that switch is per ROOM, not per transport (`janus_audiobridge.c`: the
//! `join` refuses `rtp` only when the room's `allow_plainrtp` is off). So a WebSocket client could
//! join with `"rtp": {"ip", "port"}` and make Janus stream the room to any host it names (a UDP
//! reflection). The same plugin also lets an unauthenticated client `create` its own room with
//! `allow_rtp_participants`, `rtp_forward` the mix, `play_file`, or record to a path, and the box's
//! Janus loads other plugins too.
//!
//! The hub therefore relays the phone's WebSocket (`janus_relay`) and forwards a client frame ONLY
//! when it is one the phone page itself sends; everything else is answered with a Janus-shaped
//! error and never reaches Janus:
//! - a janus request in [`ALLOWED_JANUS_REQUESTS`] (what the vendored janus.js 1.1.2 sends);
//! - an `attach` only to [`AUDIOBRIDGE_PLUGIN`];
//! - a plugin `message` whose body is a `join` or a `configure` carrying only the keys the PWA sends
//!   ([`JOIN_KEYS`], [`CONFIGURE_KEYS`]: so never `rtp`, `record`, `filename`, ...), with `display`
//!   a string and `muted` a boolean, and a `join` only into the hub's own room.
//!
//! Frames from Janus to the phone are never inspected. A forwarded frame is the client's own text,
//! byte for byte. Pure (serde_json only), so the policy is tested without a socket.

use serde_json::{Map, Value};

/// The WebSocket subprotocol Janus speaks (janus.js asks for it; Janus's websockets transport
/// registers it).
pub const JANUS_WS_PROTOCOL: &str = "janus-protocol";
/// The one plugin a phone may attach to.
pub const AUDIOBRIDGE_PLUGIN: &str = "janus.plugin.audiobridge";

/// Janus core error codes (`src/apierror.h`) the refusal uses.
pub const JANUS_ERROR_UNAUTHORIZED: u16 = 403;
pub const JANUS_ERROR_INVALID_JSON: u16 = 454;
pub const JANUS_ERROR_INVALID_JSON_OBJECT: u16 = 455;

/// The janus requests the vendored janus.js 1.1.2 can send (`create`, `attach`, `message`,
/// `trickle`, `keepalive`, `detach`, `destroy`, `hangup`, `info`). Anything else (`claim`, the
/// admin requests, ...) is refused.
pub const ALLOWED_JANUS_REQUESTS: [&str; 9] = [
    "create",
    "attach",
    "message",
    "trickle",
    "keepalive",
    "detach",
    "destroy",
    "hangup",
    "info",
];

/// The keys of an audiobridge `join` the phone page sends (`intercom/web/app.js`).
pub const JOIN_KEYS: [&str; 4] = ["request", "room", "display", "muted"];
/// The keys of an audiobridge `configure` the phone page sends (mic on / off, a name change; the
/// renegotiation's SDP rides in the frame's `jsep`, outside the body).
pub const CONFIGURE_KEYS: [&str; 3] = ["request", "muted", "display"];

/// How many characters of a client-chosen name a refusal quotes (it lands in a log line and in the
/// reply).
const QUOTE_MAX_CHARS: usize = 40;

/// What the relay does with one client frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Send the frame to Janus unchanged.
    Forward,
    /// Answer the client with [`Refusal::reply`]; Janus never sees the frame.
    Refuse(Refusal),
}

/// A refused frame: the Janus error code, the reason, and the reply text for the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: u16,
    pub reason: String,
    /// `{"janus":"error","transaction":..,"session_id":..,"error":{"code":..,"reason":..}}`, the
    /// shape Janus's own core errors have, so janus.js routes it to the request's error callback.
    pub reply: String,
}

/// Decide one client text frame. `room` is the hub's audiobridge room (`[janus].room`).
pub fn check_client_frame(text: &str, room: u64) -> Verdict {
    let value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            return refuse(
                None,
                JANUS_ERROR_INVALID_JSON,
                "the frame is not JSON".to_string(),
            )
        }
    };
    let Some(request) = value.as_object() else {
        return refuse(
            None,
            JANUS_ERROR_INVALID_JSON_OBJECT,
            "the frame is not a JSON object".to_string(),
        );
    };
    match check_request(request, room) {
        Ok(()) => Verdict::Forward,
        Err(reason) => refuse(Some(request), JANUS_ERROR_UNAUTHORIZED, reason),
    }
}

fn check_request(request: &Map<String, Value>, room: u64) -> Result<(), String> {
    let Some(janus) = request.get("janus").and_then(Value::as_str) else {
        return Err("the frame names no janus request".to_string());
    };
    if !ALLOWED_JANUS_REQUESTS.contains(&janus) {
        return Err(format!("janus request {} is not allowed", quote(janus)));
    }
    match janus {
        "attach" => {
            let plugin = request.get("plugin").and_then(Value::as_str).unwrap_or("");
            if plugin == AUDIOBRIDGE_PLUGIN {
                Ok(())
            } else {
                Err(format!(
                    "attach is allowed only to {AUDIOBRIDGE_PLUGIN}, not {}",
                    quote(plugin)
                ))
            }
        }
        "message" => check_body(request.get("body"), room),
        _ => Ok(()),
    }
}

fn check_body(body: Option<&Value>, room: u64) -> Result<(), String> {
    let Some(body) = body.and_then(Value::as_object) else {
        return Err("a plugin message needs a body object".to_string());
    };
    let Some(request) = body.get("request").and_then(Value::as_str) else {
        return Err("the plugin message names no request".to_string());
    };
    let allowed: &[&str] = match request {
        "join" => &JOIN_KEYS,
        "configure" => &CONFIGURE_KEYS,
        other => {
            return Err(format!(
                "audiobridge request {} is not allowed",
                quote(other)
            ))
        }
    };
    if body.contains_key("rtp") {
        return Err(format!(
            "a plain-RTP {request} is refused (rtp is for the hub's own leg only)"
        ));
    }
    if let Some(key) = body.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!(
            "audiobridge {request} may not carry {}",
            quote(key)
        ));
    }
    if body.get("display").is_some_and(|v| !v.is_string()) {
        return Err(format!("audiobridge {request}: display must be a string"));
    }
    if body.get("muted").is_some_and(|v| !v.is_boolean()) {
        return Err(format!("audiobridge {request}: muted must be a boolean"));
    }
    if request == "join" && body.get("room").and_then(Value::as_u64) != Some(room) {
        return Err(format!("audiobridge join may enter room {room} only"));
    }
    Ok(())
}

/// A client-chosen name, cut to [`QUOTE_MAX_CHARS`] and Debug-quoted (control characters escaped,
/// so it can never break a log line).
fn quote(s: &str) -> String {
    let cut: String = s.chars().take(QUOTE_MAX_CHARS).collect();
    format!("{cut:?}")
}

fn refuse(request: Option<&Map<String, Value>>, code: u16, reason: String) -> Verdict {
    let mut reply = Map::new();
    reply.insert("janus".to_string(), Value::from("error"));
    if let Some(request) = request {
        if let Some(tx) = request.get("transaction").filter(|v| v.is_string()) {
            reply.insert("transaction".to_string(), tx.clone());
        }
        if let Some(sid) = request.get("session_id").filter(|v| v.is_u64()) {
            reply.insert("session_id".to_string(), sid.clone());
        }
    }
    let reason = format!("interkom: {reason}");
    let mut error = Map::new();
    error.insert("code".to_string(), Value::from(code));
    error.insert("reason".to_string(), Value::from(reason.clone()));
    reply.insert("error".to_string(), Value::Object(error));
    Verdict::Refuse(Refusal {
        code,
        reason,
        reply: Value::Object(reply).to_string(),
    })
}
