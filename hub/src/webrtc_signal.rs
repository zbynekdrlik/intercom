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
    /// The hub-side TURN relay (intercom#42 phase 3): `"cloudflare"` allocates the hub's own
    /// relayed candidate from the box's Cloudflare TURN key (the `/api/ice` minter), `"off"` (the
    /// default) offers host candidates only. Anything else fails the load.
    #[serde(default)]
    pub turn: Option<toml::Value>,
}

/// Where the hub's own relayed candidate comes from (`[webrtc].turn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnMode {
    /// Host candidates only.
    Off,
    /// One allocation on Cloudflare's TURN service, credentials from the box's TURN key.
    Cloudflare,
}

/// The `[webrtc].turn` values.
pub const TURN_MODE_CLOUDFLARE: &str = "cloudflare";
pub const TURN_MODE_OFF: &str = "off";

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
        if !bind.is_ipv4() {
            // [::] would deliver v4-mapped sources and no IP_PKTINFO destination (review round 2).
            return Err(format!(
                "webrtc.udp_bind '{}' must be an IPv4 address",
                self.udp_bind
            ));
        }
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
        self.parse_turn_mode()?;
        Ok(bind)
    }

    fn parse_turn_mode(&self) -> Result<TurnMode, String> {
        match &self.turn {
            None => Ok(TurnMode::Off),
            Some(toml::Value::String(s)) if s == TURN_MODE_OFF => Ok(TurnMode::Off),
            Some(toml::Value::String(s)) if s == TURN_MODE_CLOUDFLARE => Ok(TurnMode::Cloudflare),
            Some(other) => Err(format!(
                "webrtc.turn must be \"{TURN_MODE_CLOUDFLARE}\" or \"{TURN_MODE_OFF}\", got {other}"
            )),
        }
    }

    /// The relay's credential source (a table that passed [`WebrtcConfig::validate`]; an invalid
    /// value reads as off).
    pub fn turn_mode(&self) -> TurnMode {
        self.parse_turn_mode().unwrap_or(TurnMode::Off)
    }

    /// The host candidate IPs: the configured ones, else the bind's own IP, else the box's LAN
    /// addresses from `discovered` (`(interface, address)`, see [`is_lan_candidate`]) minus
    /// whatever `is_excluded` refuses (the Dante leg). The answer goes to anonymous POSTers, so the
    /// default never names an internal address (a container bridge, the tailnet, link-local).
    pub fn candidate_ips(
        &self,
        bind: SocketAddr,
        discovered: &[(String, IpAddr)],
        is_excluded: impl Fn(IpAddr) -> bool,
    ) -> Vec<IpAddr> {
        if !self.host_ips.is_empty() {
            return self.host_ips.clone();
        }
        if !bind.ip().is_unspecified() {
            return vec![bind.ip()];
        }
        let mut out: Vec<IpAddr> = Vec::new();
        for (iface, ip) in discovered {
            if is_lan_candidate(iface, *ip) && !is_excluded(*ip) && !out.contains(ip) {
                out.push(*ip);
            }
        }
        out
    }
}

/// Interface name prefixes that are never a phone's way in: container and VM bridges, VPNs.
pub const VIRTUAL_IFACE_PREFIXES: &[&str] = &[
    "lo",
    "docker",
    "br-",
    "veth",
    "virbr",
    "vnet",
    "tailscale",
    "ts",
    "tun",
    "tap",
    "wg",
    "zt",
    "cni",
    "flannel",
    "lxc",
    "lxd",
    "podman",
];

/// A default host candidate: an RFC 1918 IPv4 address on a real interface. Loopback, link-local,
/// the CGNAT / tailnet range 100.64/10, public addresses and virtual interfaces are left out (set
/// `[webrtc].host_ips` to advertise anything else).
pub fn is_lan_candidate(iface: &str, ip: IpAddr) -> bool {
    let IpAddr::V4(v4) = ip else {
        return false;
    };
    if VIRTUAL_IFACE_PREFIXES.iter().any(|p| iface.starts_with(p)) {
        return false;
    }
    v4.is_private()
}

/// Offers without a connected phone behind them may hold at most this many slots (half, at least
/// one), so a burst of anonymous offers never locks a reconnecting phone out.
pub fn pending_cap(max_phones: usize) -> usize {
    (max_phones / 2).max(1)
}

/// The rate-limit and per-client key of a request (review round 2 🟡1). The hub listens on all
/// addresses, so a LAN client can reach it without the box's nginx and send any header it likes:
///
/// - a forwarding header counts ONLY when the TCP peer is loopback, i.e. the box's own nginx,
///   which sets `X-Forwarded-For $remote_addr` (one address: the last hop is it);
/// - `CF-Connecting-IP` counts only when that nginx-set address is itself loopback, i.e. the
///   request came from cloudflared on the box (every phone through the tunnel shares that peer);
/// - anything else, or a header that is not one IP address, keys on the TCP peer.
///
/// An IPv6 address is keyed by its /64 (a client rotates freely inside its own prefix).
pub fn client_key(cf: Option<&str>, xff: Option<&str>, peer: Option<IpAddr>) -> String {
    let Some(peer) = peer else {
        return "unknown".to_string();
    };
    // Canonical first, so `::ffff:127.0.0.1` is the loopback it is (review round 3).
    let parse = |s: Option<&str>| -> Option<IpAddr> {
        s?.trim().parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
    };
    let peer = peer.to_canonical();
    let mut ip = peer;
    if peer.is_loopback() {
        if let Some(hop) = parse(xff.and_then(|x| x.rsplit(',').next())) {
            ip = hop;
            if hop.is_loopback() {
                if let Some(c) = parse(cf) {
                    ip = c;
                }
            }
        }
    }
    key_of(ip)
}

/// One client: an IPv4 address, or an IPv6 /64.
fn key_of(ip: IpAddr) -> String {
    let ip = match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                return format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3]);
            }
        },
        v4 => v4,
    };
    ip.to_string()
}

/// A token bucket per client key: `burst` offers at once, then one every `1 / per_s` seconds.
/// At most `max_keys` buckets are kept; a new key evicts the least recently used one, so every
/// client always has a bucket of its own (never a shared overflow bucket an attacker could drain).
#[derive(Debug)]
pub struct RateLimiter {
    buckets: std::collections::HashMap<String, (f64, std::time::Instant)>,
    burst: f64,
    per_s: f64,
    max_keys: usize,
}

/// The offer rate limit for a hub of `max_phones` slots: `(burst, per second)`. A whole crew may
/// come in at once from ONE key (the venue's Wi-Fi reaches the hub through cloudflared with one
/// public address, review round 3), so the burst is `max_phones` (at least 5) and the refill one
/// per second; the hub-wide pending cap and the 3 s STUN deadline are the flood guard, and a
/// reconnect (a valid `replaces`) spends no token at all.
pub fn offer_rate(max_phones: usize) -> (f64, f64) {
    (max_phones.max(5) as f64, 1.0)
}
/// The rate limiter's default key capacity.
pub const RATE_LIMIT_KEYS: usize = 4096;

impl RateLimiter {
    pub fn new(burst: f64, per_s: f64) -> Self {
        Self::with_capacity(burst, per_s, RATE_LIMIT_KEYS)
    }

    /// A limiter keeping at most `max_keys` buckets (at least one).
    pub fn with_capacity(burst: f64, per_s: f64, max_keys: usize) -> Self {
        RateLimiter {
            buckets: std::collections::HashMap::new(),
            burst,
            per_s,
            max_keys: max_keys.max(1),
        }
    }

    /// Buckets kept now.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// No bucket yet.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Does `key` have a token at `now`? Nothing is spent (see [`RateLimiter::spend`]).
    pub fn peek(&mut self, key: &str, now: std::time::Instant) -> bool {
        self.refill(key, now) >= 1.0
    }

    /// Spend one of `key`'s tokens at `now` (no-op on an empty bucket).
    pub fn spend(&mut self, key: &str, now: std::time::Instant) {
        if self.refill(key, now) >= 1.0 {
            if let Some((tokens, _)) = self.buckets.get_mut(key) {
                *tokens -= 1.0;
            }
        }
    }

    /// May `key` make one more request at `now`? A yes spends a token.
    pub fn allow(&mut self, key: &str, now: std::time::Instant) -> bool {
        if self.peek(key, now) {
            self.spend(key, now);
            true
        } else {
            false
        }
    }

    /// Bring `key`'s bucket up to `now` (creating it, evicting the least recently used one when
    /// full) and return its tokens.
    fn refill(&mut self, key: &str, now: std::time::Instant) -> f64 {
        if !self.buckets.contains_key(key) && self.buckets.len() >= self.max_keys {
            // Evict the least recently used bucket (an O(n) scan, bounded by max_keys and only
            // on a new key, which the rate of new TCP peers bounds in turn).
            if let Some(lru) = self
                .buckets
                .iter()
                .min_by_key(|(_, (_, last))| *last)
                .map(|(k, _)| k.clone())
            {
                self.buckets.remove(&lru);
            }
        }
        let (tokens, last) = self
            .buckets
            .entry(key.to_string())
            .or_insert((self.burst, now));
        let refill = now.saturating_duration_since(*last).as_secs_f64() * self.per_s;
        *tokens = (*tokens + refill).min(self.burst);
        *last = now;
        *tokens
    }
}

/// A warning about refused offers at most once every [`RefusalLog::EVERY`], carrying the count
/// since the last line, so a flood on the public route never floods the journal.
#[derive(Debug, Default)]
pub struct RefusalLog {
    last: Option<std::time::Instant>,
    since: u64,
}

impl RefusalLog {
    pub const EVERY: std::time::Duration = std::time::Duration::from_secs(10);

    /// One refusal at `now`: `Some(count)` when a line is due (the refusals since the last one,
    /// this one included).
    pub fn note(&mut self, now: std::time::Instant) -> Option<u64> {
        self.since += 1;
        if self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) < Self::EVERY)
        {
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.since))
    }
}

/// The offer's `a=candidate` lines the hub keeps: UDP, a port of 1024 or more, a unicast address
/// that is not one of the hub's own, at most [`MAX_OFFER_CANDIDATES`]; `.local` mDNS names are
/// kept (str0m ignores them). The hub runs ICE-lite, so it never sends a check to any of them;
/// they are kept for the phase-3 TURN permissions only. Returns the SDP and how many were dropped.
pub fn filter_offer_candidates(sdp: &str, own: &[SocketAddr]) -> (String, usize) {
    let mut kept = 0usize;
    let mut dropped = 0usize;
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        if let Some(cand) = body.strip_prefix("a=candidate:") {
            if kept < MAX_OFFER_CANDIDATES && plausible_candidate(cand, own) {
                kept += 1;
            } else {
                dropped += 1;
                continue;
            }
        }
        out.push_str(line);
    }
    (out, dropped)
}

/// The IP addresses of an (already filtered) offer's `a=candidate:` lines, each once, in order;
/// `.local` mDNS names are skipped. The hub's TURN relay asks a permission for each (phase 3), so
/// the phone's checks can come through the relay.
pub fn offer_candidate_ips(sdp: &str) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for line in sdp.lines() {
        let Some(cand) = line.trim_end().strip_prefix("a=candidate:") else {
            continue;
        };
        if let Some(ip) = cand
            .split_whitespace()
            .nth(4)
            .and_then(|a| a.parse::<IpAddr>().ok())
        {
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

/// The most offer candidates the hub keeps.
pub const MAX_OFFER_CANDIDATES: usize = 8;

fn plausible_candidate(cand: &str, own: &[SocketAddr]) -> bool {
    // foundation component transport priority address port "typ" type ...
    let f: Vec<&str> = cand.split_whitespace().collect();
    if f.len() < 8 || !f[2].eq_ignore_ascii_case("udp") || f[6] != "typ" {
        return false;
    }
    let Ok(port) = f[5].parse::<u16>() else {
        return false;
    };
    if port < 1024 {
        return false;
    }
    if f[4].ends_with(".local") {
        return true;
    }
    let Ok(ip) = f[4].parse::<IpAddr>() else {
        return false;
    };
    let unicast = match ip {
        IpAddr::V4(v4) => !(v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast()),
        IpAddr::V6(v6) => !(v6.is_unspecified() || v6.is_multicast()),
    };
    unicast && !own.iter().any(|o| o.ip() == ip)
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
    /// Too many offers from one client (429).
    RateLimited,
    /// The body is not `application/json` (415): a page elsewhere cannot fire a CORS "simple"
    /// POST at the hub, it must preflight, and the hub answers no preflight.
    UnsupportedMediaType,
}

impl SignalError {
    /// The HTTP status code of the refusal.
    pub fn status(&self) -> u16 {
        match self {
            SignalError::BadRequest(_) | SignalError::BadOffer(_) => 400,
            SignalError::TooLarge => 413,
            SignalError::Full | SignalError::Unavailable => 503,
            SignalError::UnknownPhone => 404,
            SignalError::RateLimited => 429,
            SignalError::UnsupportedMediaType => 415,
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
            SignalError::RateLimited => {
                write!(f, "too many offers from this client, retry shortly")
            }
            SignalError::UnsupportedMediaType => write!(f, "the body must be application/json"),
        }
    }
}

/// `Content-Type: application/json` (parameters such as `; charset=utf-8` allowed).
pub fn is_json_content_type(value: Option<&str>) -> bool {
    value
        .and_then(|v| v.split(';').next())
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
}

/// A parsed `POST /api/phone/offer` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferRequest {
    pub sdp: String,
    pub display: String,
    /// The id this phone had before a reconnect (see [`OfferBody`]).
    pub replaces: Option<PhoneId>,
}

#[derive(Deserialize)]
struct OfferBody {
    sdp: String,
    #[serde(default)]
    display: Option<String>,
    /// The id this phone had before a reconnect: an offer naming a live or recently connected
    /// phone of this hub is not held to the pending cap (and replaces a live one).
    #[serde(default)]
    replaces: Option<String>,
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
        // A malformed or unknown id is simply no claim: the offer is then a new phone.
        replaces: b.replaces.as_deref().and_then(PhoneId::parse),
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
