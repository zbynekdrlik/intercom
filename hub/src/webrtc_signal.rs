//! The in-hub WebRTC phone leg's pure parts (intercom#42, owner decision B 20.11.2026; design
//! comment 7041415746): the `[webrtc]` config, the phone ids, the signalling request limits and the
//! slot table. No socket, no thread, no clock: everything here is decided from its inputs, so the
//! limits are pinned by plain unit tests (`hub/tests/webrtc_signal_42.rs`).
//!
//! Signalling is WHEP-like over the hub's own HTTP port (no WebSocket, no new nginx location):
//!
//! - `POST /api/phone/offer {sdp, display}` -> `{id, sdp}`: the phone's complete offer (sent after
//!   ICE gathering) and the hub's answer, which carries every hub candidate;
//! - `POST /api/phone/<id>/display {display}`: the name the phone shows;
//! - `DELETE /api/phone/<id>`: the phone leaves;
//! - `GET /api/phone` -> `{"transport": "webrtc" | "janus"}`: which leg the page talks to.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use serde::Deserialize;

/// The `[webrtc]` table's default UDP bind: one socket for every phone's ICE, DTLS and SRTP.
pub const DEFAULT_UDP_BIND: &str = "0.0.0.0:6992";
/// The `[webrtc]` table's default phone cap: one slot (a mic ring and an encoder) per phone.
pub const DEFAULT_MAX_PHONES: usize = 16;
/// The largest `max_phones` the config accepts: the room thread mixes N x N frames every 20 ms.
pub const MAX_PHONES_LIMIT: usize = 64;
/// The largest SDP offer the hub parses (a browser's audio-only offer is ~3-5 KiB).
pub const MAX_SDP_BYTES: usize = 16 * 1024;
/// The largest request body on the signalling routes: the SDP plus its JSON wrapping and the name.
pub const MAX_SIGNAL_BODY_BYTES: usize = MAX_SDP_BYTES + 1024;
/// The longest display name kept, in characters (a longer one is cut, never refused).
pub const MAX_DISPLAY_CHARS: usize = 48;
/// The name a phone gets when it sends none.
pub const DEFAULT_DISPLAY: &str = "Kameraman";

/// The phones' transport as `GET /api/phone` names it.
pub const TRANSPORT_WEBRTC: &str = "webrtc";
pub const TRANSPORT_JANUS: &str = "janus";

/// The optional `[webrtc]` table: the hub itself is the phones' WebRTC endpoint.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebrtcConfig {
    /// The UDP `host:port` the one WebRTC socket binds (never a Dante address).
    #[serde(default = "default_udp_bind")]
    pub udp_bind: String,
    /// The host candidates the answer offers. Empty = the bind's IP when it is a specific one, else
    /// every up IPv4 address of the box that is not loopback and not on the Dante leg.
    #[serde(default)]
    pub host_ips: Vec<IpAddr>,
    /// How many phones may be connected at once; one more gets 503.
    #[serde(default = "default_max_phones")]
    pub max_phones: usize,
    /// Reserved for the hub-side TURN relay (intercom#42 phase 3). Any value fails the load until
    /// that phase lands, so a config never believes a relay is running when it is not.
    #[serde(default)]
    pub turn: Option<toml::Value>,
}

fn default_udp_bind() -> String {
    DEFAULT_UDP_BIND.to_string()
}
fn default_max_phones() -> usize {
    DEFAULT_MAX_PHONES
}

impl Default for WebrtcConfig {
    fn default() -> Self {
        WebrtcConfig {
            udp_bind: default_udp_bind(),
            host_ips: Vec::new(),
            max_phones: default_max_phones(),
            turn: None,
        }
    }
}

impl WebrtcConfig {
    /// Check the table at load; the message names the key.
    pub fn validate(&self) -> Result<SocketAddr, String> {
        let bind: SocketAddr = self
            .udp_bind
            .parse()
            .map_err(|e| format!("webrtc.udp_bind '{}': {e}", self.udp_bind))?;
        if bind.port() == 0 {
            return Err(format!(
                "webrtc.udp_bind '{}' needs a fixed port: the answer advertises it",
                self.udp_bind
            ));
        }
        if !(1..=MAX_PHONES_LIMIT).contains(&self.max_phones) {
            return Err(format!(
                "webrtc.max_phones must be 1..={MAX_PHONES_LIMIT}, got {}",
                self.max_phones
            ));
        }
        if let Some(ip) = self.host_ips.iter().find(|ip| ip.is_unspecified()) {
            return Err(format!(
                "webrtc.host_ips: {ip} is not an address a phone can send to"
            ));
        }
        if self.turn.is_some() {
            return Err(
                "webrtc.turn is reserved for the hub-side TURN relay (intercom#42 phase 3) and not supported by this hub yet"
                    .to_string(),
            );
        }
        Ok(bind)
    }

    /// The host candidate IPs: the configured ones, else the bind's own IP, else `discovered`
    /// (the box's addresses) minus loopback, link-local and whatever `is_excluded` refuses (the
    /// Dante leg).
    pub fn candidate_ips(
        &self,
        bind: SocketAddr,
        discovered: &[IpAddr],
        is_excluded: impl Fn(IpAddr) -> bool,
    ) -> Vec<IpAddr> {
        if !self.host_ips.is_empty() {
            return self.host_ips.clone();
        }
        if !bind.ip().is_unspecified() {
            return vec![bind.ip()];
        }
        let mut out: Vec<IpAddr> = discovered
            .iter()
            .copied()
            .filter(|ip| ip.is_ipv4() && !ip.is_loopback() && !ip.is_unspecified())
            .filter(|ip| !matches!(ip, IpAddr::V4(v4) if v4.is_link_local()))
            .filter(|ip| !is_excluded(*ip))
            .collect();
        out.dedup();
        out
    }
}

/// A phone's id: 128 random bits, 32 lowercase hex characters. It is the phone's only credential
/// for its own `display` / `DELETE`, so it never appears in `/api/state` (that shows [`PhoneId::short`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhoneId([u8; 16]);

impl PhoneId {
    /// An id from 16 random bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        PhoneId(bytes)
    }

    /// A fresh id from the kernel's random source.
    pub fn random() -> std::io::Result<Self> {
        use std::io::Read;
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(PhoneId(bytes))
    }

    /// Parse the 32-hex form; anything else is `None` (a 404, never a parse error to the caller).
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() != 32
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let mut bytes = [0u8; 16];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(PhoneId(bytes))
    }

    /// The first 8 hex characters: enough to tell phones apart in a log or `/api/state`, not
    /// enough to act as the phone.
    pub fn short(&self) -> String {
        self.to_string()[..8].to_string()
    }
}

impl fmt::Display for PhoneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Why a signalling request was refused, with the HTTP status it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalError {
    /// The body is not the expected JSON (400).
    BadRequest(String),
    /// The body or the SDP is over its limit (413).
    TooLarge,
    /// The SDP is not an offer the hub can answer (400).
    BadOffer(String),
    /// Every phone slot is taken (503).
    Full,
    /// No phone with that id (404).
    UnknownPhone,
    /// The WebRTC leg is not running (503).
    Unavailable,
}

impl SignalError {
    /// The HTTP status code of the refusal.
    pub fn status(&self) -> u16 {
        match self {
            SignalError::BadRequest(_) | SignalError::BadOffer(_) => 400,
            SignalError::TooLarge => 413,
            SignalError::Full | SignalError::Unavailable => 503,
            SignalError::UnknownPhone => 404,
        }
    }
}

impl fmt::Display for SignalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignalError::BadRequest(why) => write!(f, "bad request: {why}"),
            SignalError::TooLarge => {
                write!(f, "request too large (SDP limit {MAX_SDP_BYTES} bytes)")
            }
            SignalError::BadOffer(why) => write!(f, "offer not accepted: {why}"),
            SignalError::Full => write!(f, "every phone slot is taken"),
            SignalError::UnknownPhone => write!(f, "no such phone"),
            SignalError::Unavailable => write!(f, "the WebRTC phone leg is not running"),
        }
    }
}

/// A parsed `POST /api/phone/offer` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferRequest {
    pub sdp: String,
    pub display: String,
}

#[derive(Deserialize)]
struct OfferBody {
    sdp: String,
    #[serde(default)]
    display: Option<String>,
}

#[derive(Deserialize)]
struct DisplayBody {
    display: String,
}

/// Parse and check an offer body: the size limits first (before any JSON is parsed), then the shape.
pub fn parse_offer_body(body: &[u8]) -> Result<OfferRequest, SignalError> {
    if body.len() > MAX_SIGNAL_BODY_BYTES {
        return Err(SignalError::TooLarge);
    }
    let b: OfferBody =
        serde_json::from_slice(body).map_err(|e| SignalError::BadRequest(e.to_string()))?;
    if b.sdp.len() > MAX_SDP_BYTES {
        return Err(SignalError::TooLarge);
    }
    if !b.sdp.starts_with("v=0") {
        return Err(SignalError::BadOffer(
            "the sdp does not start with v=0".into(),
        ));
    }
    Ok(OfferRequest {
        sdp: b.sdp,
        display: clean_display(b.display.as_deref().unwrap_or("")),
    })
}

/// Parse a `POST /api/phone/<id>/display` body.
pub fn parse_display_body(body: &[u8]) -> Result<String, SignalError> {
    if body.len() > MAX_SIGNAL_BODY_BYTES {
        return Err(SignalError::TooLarge);
    }
    let b: DisplayBody =
        serde_json::from_slice(body).map_err(|e| SignalError::BadRequest(e.to_string()))?;
    Ok(clean_display(&b.display))
}

/// A display name as the hub keeps it: control characters dropped, whitespace collapsed, at most
/// [`MAX_DISPLAY_CHARS`] characters, [`DEFAULT_DISPLAY`] when nothing is left.
pub fn clean_display(raw: &str) -> String {
    let words: Vec<&str> = raw.split_whitespace().collect();
    let joined: String = words
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DISPLAY_CHARS)
        .collect();
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        DEFAULT_DISPLAY.to_string()
    } else {
        trimmed.to_string()
    }
}

/// The phone slots: a fixed number of places, each with a generation that changes on every
/// allocation, so a packet or a frame meant for a phone that left never reaches the next one.
#[derive(Debug, Clone)]
pub struct SlotTable {
    used: Vec<Option<u64>>,
    next_generation: u64,
}

impl SlotTable {
    /// `max` free slots.
    pub fn new(max: usize) -> Self {
        SlotTable {
            used: vec![None; max],
            next_generation: 1,
        }
    }

    /// Take the lowest free slot: `(slot, generation)`, or `None` when every slot is taken.
    pub fn allocate(&mut self) -> Option<(usize, u64)> {
        let slot = self.used.iter().position(Option::is_none)?;
        let generation = self.next_generation;
        self.next_generation += 1;
        self.used[slot] = Some(generation);
        Some((slot, generation))
    }

    /// Free `slot` if it still holds `generation` (a stale free is ignored). True when freed.
    pub fn free(&mut self, slot: usize, generation: u64) -> bool {
        match self.used.get_mut(slot) {
            Some(g) if *g == Some(generation) => {
                *g = None;
                true
            }
            _ => false,
        }
    }

    /// The generation `slot` holds now (`None` = free or out of range).
    pub fn generation(&self, slot: usize) -> Option<u64> {
        self.used.get(slot).copied().flatten()
    }

    /// Slots in use.
    pub fn in_use(&self) -> usize {
        self.used.iter().filter(|g| g.is_some()).count()
    }

    /// Every slot, used or not.
    pub fn capacity(&self) -> usize {
        self.used.len()
    }
}
