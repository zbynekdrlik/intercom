//! The hub's own TURN relay for the in-hub WebRTC phone leg (intercom#42 phase 3, design comment
//! 7041415746). The venue uplink is CGNAT, so the hub has only LAN host candidates and a phone on
//! mobile data cannot reach any of them, not even through its own TURN relay. The hub therefore
//! allocates ITS OWN relayed address on the same Cloudflare TURN service (credentials from
//! [`crate::turn::TurnMinter`]) and offers it in every answer next to the host candidates.
//!
//! [`TurnRelay`] is sans-IO, owned by the network task ([`crate::webrtc_leg`]), and talks to the
//! TURN server over its OWN ephemeral UDP socket (`webrtc_leg::bind_turn_socket`: a new 5-tuple
//! per hub process, so a restarted hub never meets its predecessor's unreleased allocation):
//!
//! - **One allocation** (UDP over UDP, the long-term credential: an unsigned Allocate gets 401 with
//!   REALM + NONCE, the signed one the relayed address), refreshed at half its lifetime (or
//!   `refresh_cap`), a 438 stale nonce retried once with the new nonce, kept alive by a STUN
//!   Binding request whenever nothing went to the server for [`TURN_KEEPALIVE`] (the venue NAT's
//!   mapping). A refused or unanswered Allocate backs off 1 s -> 60 s. A LOST allocation (a 437 or
//!   no answer on any other request, an expiry) is allocated again at once (`lost`), or refreshed
//!   back when the server turns out to still hold it (`Readopt`). One log line per state change,
//!   never per packet.
//! - **A credential rotation** (a new username from the minter) RELEASES the allocation with the
//!   OLD credential (RFC 5766 §6.2: a request on an allocation signed by another username is 441;
//!   the same 5-tuple cannot hold a second allocation, 437) and allocates with the new one. While a
//!   phone is on the relay that waits, but never past [`TURN_ROTATE_MARGIN`] before the old
//!   credential expires.
//! - **Permissions per IP** (`set_wanted`: the plausible addresses of the live phones' offers),
//!   global IPv4 only, at most `max_permissions`, refreshed every [`TURN_PERMISSION_REFRESH`] only
//!   while wanted; the server lets a permission lapse after 300 s once the hub stops refreshing it.
//! - **Sends** (`send_to`): a Send indication, and once a ChannelBind for that peer succeeded,
//!   ChannelData (4 bytes of header instead of 36), at most `max_channels` live; a departed peer's
//!   channel is remembered [`TURN_CHANNEL_REMEMBER`] and reused. Never to a peer without
//!   a permitted IP. **Receives** (`handle_datagram`): Data indications and ChannelData unwrap to
//!   (peer, payload) for str0m; responses drive the state machine; anything else is dropped.
//!
//! ICE-lite is unchanged: the relay is a transport, never a reason to send. str0m still only
//! ANSWERS the phone's checks; a TURN permission is not a check. Everything this relay emits is
//! addressed to the TURN server. The password never reaches a log line, a Debug print or the facet.
//!
//! Responses are matched by their 96-bit random transaction id; their MESSAGE-INTEGRITY is not
//! checked (they only move this client's own state, and only the server's address is accepted).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rtc_stun::attributes::{ATTR_NONCE, ATTR_REALM, ATTR_USERNAME};
use rtc_stun::error_code::ErrorCodeAttribute;
use rtc_stun::fingerprint::FINGERPRINT;
use rtc_stun::integrity::MessageIntegrity;
use rtc_stun::message::{
    is_stun_message, Getter, Message, MessageType, Setter, TransactionId, BINDING_REQUEST,
    CLASS_ERROR_RESPONSE, CLASS_INDICATION, CLASS_REQUEST, CLASS_SUCCESS_RESPONSE, METHOD_ALLOCATE,
    METHOD_CHANNEL_BIND, METHOD_CREATE_PERMISSION, METHOD_DATA, METHOD_REFRESH, METHOD_SEND,
};
use rtc_stun::textattrs::TextAttribute;
use rtc_turn::crypto::RTCCryptoProvider;
use rtc_turn::proto::chandata::ChannelData;
use rtc_turn::proto::channum::{ChannelNumber, MIN_CHANNEL_NUMBER};
use rtc_turn::proto::data::Data;
use rtc_turn::proto::lifetime::Lifetime;
use rtc_turn::proto::peeraddr::PeerAddress;
use rtc_turn::proto::relayaddr::RelayedAddress;
use rtc_turn::proto::reqtrans::RequestedTransport;
use rtc_turn::proto::PROTO_UDP;
use serde::Serialize;

/// The lifetime the hub asks for (RFC 5766's default; the server may grant less or more).
pub const TURN_REQUEST_LIFETIME: Duration = Duration::from_secs(600);
/// The first wait after a failure; each further failure doubles it up to [`TURN_BACKOFF_MAX`].
pub const TURN_BACKOFF_MIN: Duration = Duration::from_secs(1);
/// The longest wait between two allocation attempts.
pub const TURN_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A rotation waits for the relay to be idle, but never closer than this to the old credential's
/// expiry.
pub const TURN_ROTATE_MARGIN: Duration = Duration::from_secs(600);
/// A permission lives 300 s on the server; the hub refreshes a wanted one after this.
pub const TURN_PERMISSION_REFRESH: Duration = Duration::from_secs(240);
/// A channel binding lives 600 s; one in use is bound again after this.
pub const TURN_CHANNEL_REFRESH: Duration = Duration::from_secs(300);
/// A request unanswered this long has failed (sent at 0, 0.5, 1.5 and 3.5 s).
pub const TURN_TX_TIMEOUT: Duration = Duration::from_millis(7500);
/// When the retransmissions go out, from the first send.
const TX_SENDS_MS: [u64; 4] = [0, 500, 1500, 3500];
/// A refused permission is asked again after this (no tight loop on a 403).
const PERMISSION_RETRY: Duration = Duration::from_secs(60);
/// After a release, a 437 (the server still holds the old allocation: coturn drops it on its next
/// timer pass, 1-3 s on dev1) is the release settling, not a failure, for this long.
pub const TURN_RELEASE_SETTLE: Duration = Duration::from_secs(10);
/// The Allocate is tried again this soon while a release settles.
const RELEASE_RETRY: Duration = Duration::from_millis(500);
/// How many 438 (stale nonce) answers one request may get before it fails.
const MAX_NONCE_RETRIES: u8 = 2;
/// The default TURN port (RFC 5766).
pub const TURN_DEFAULT_PORT: u16 = 3478;
/// While nothing else goes to the TURN server for this long, a STUN Binding request does (review
/// of phase 3): the venue's CGNAT / conntrack drops an idle UDP mapping in 30-180 s, and the only
/// other idle traffic is the Refresh every 300 s. Its answer (or its absence) also proves the
/// allocation's 5-tuple still reaches the server.
pub const TURN_KEEPALIVE: Duration = Duration::from_secs(15);
/// The last channel number (RFC 8656 §12: 0x4000 through 0x4FFF; 0x5000-0x7FFF are reserved).
pub const TURN_MAX_CHANNEL_NUMBER: u16 = 0x4FFF;
/// A departed peer's channel is remembered this long after its last bind: the server keeps the
/// binding 600 s and refuses its number / peer to another binding for 300 s more (RFC 8656 §12).
pub const TURN_CHANNEL_REMEMBER: Duration = Duration::from_secs(900);
/// A refused ChannelBind is tried again after this (Send indications meanwhile).
pub const TURN_CHANNEL_RETRY: Duration = Duration::from_secs(30);
/// A lost allocation is allocated again at once, unless another was lost this recently (then the
/// back-off applies: no tight loop against a server that keeps losing it).
pub const TURN_LOSS_AT_ONCE_WINDOW: Duration = Duration::from_secs(60);

/// One credential set for the hub's allocation. `Debug` never prints the password.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnCreds {
    /// The TURN server's UDP address (resolved, IPv4).
    pub server: SocketAddr,
    pub username: String,
    pub password: String,
    /// When the credential stops being accepted (the minter's ttl).
    pub expires_at: Instant,
}

impl fmt::Debug for TurnCreds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnCreds")
            .field("server", &self.server)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The relay's bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOptions {
    /// The most peer IPs the hub holds a permission for.
    pub max_permissions: usize,
    /// The most channel bindings (the rest of the peers get Send indications).
    pub max_channels: usize,
    /// Refresh the allocation at least this often (tests; production refreshes at half the
    /// lifetime).
    pub refresh_cap: Option<Duration>,
    /// Ask permissions for non-global IPv4 peers too (RFC 1918, 100.64/10, link-local, loopback).
    /// TESTS ONLY (coturn and its peers on 127.0.0.x): a production hub never asks for them, a
    /// public TURN service refuses them (403) and a phone on the LAN reaches a host candidate.
    pub allow_private_peers: bool,
}

impl TurnOptions {
    /// The bounds for a hub of `max_phones`: up to 8 plausible addresses per offer
    /// ([`crate::webrtc_signal::MAX_OFFER_CANDIDATES`]) for every phone, two channels per phone.
    pub fn for_phones(max_phones: usize) -> Self {
        let n = max_phones.max(1);
        TurnOptions {
            max_permissions: (n * crate::webrtc_signal::MAX_OFFER_CANDIDATES).clamp(8, 512),
            max_channels: (n * 2).clamp(2, 128),
            refresh_cap: None,
            allow_private_peers: false,
        }
    }
}

/// Where the relay is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPhase {
    /// No credential source (no TURN key): nothing is sent.
    Off,
    /// Waiting for the first credential.
    Waiting,
    /// An Allocate is out.
    Allocating,
    /// The relayed address is live.
    Allocated,
    /// The old allocation is being released for a rotated credential.
    Releasing,
    /// The last attempt failed; the next one waits for the back-off.
    Backoff,
}

impl TurnPhase {
    /// The `/api/state` name.
    pub fn name(self) -> &'static str {
        match self {
            TurnPhase::Off => "off",
            TurnPhase::Waiting => "waiting",
            TurnPhase::Allocating => "allocating",
            TurnPhase::Allocated => "allocated",
            TurnPhase::Releasing => "releasing",
            TurnPhase::Backoff => "backoff",
        }
    }
}

/// The `/api/state` `webrtc.turn` facet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnFacet {
    /// `off` / `waiting` / `allocating` / `allocated` / `releasing` / `backoff`.
    pub state: &'static str,
    /// The TURN server the hub talks to.
    pub server: Option<String>,
    /// The hub's relayed address (its `typ relay` candidate) while allocated.
    pub relayed: Option<String>,
    /// Seconds until the allocation expires unless refreshed.
    pub lifetime_left_s: Option<u64>,
    /// Allocations made, refreshes answered, failures (each one starts a back-off).
    pub allocations: u64,
    pub refreshes: u64,
    pub failures: u64,
    /// The last failure or refusal, for the operator (never a credential).
    pub last_error: Option<String>,
    /// Peer IPs with a live permission.
    pub permissions: usize,
    /// Peer IPs that did not fit under `max_permissions`.
    pub permissions_refused: u64,
    /// Channel bindings (bound or being bound).
    pub channels: usize,
    /// Datagrams unwrapped from / wrapped to the relay.
    pub rx_packets: u64,
    pub tx_packets: u64,
    /// Sends refused: no allocation, or no permission for the peer.
    pub tx_refused: u64,
}

/// The `turn:` url a hub can use: UDP (no `transport` or `transport=udp`), `(host, port)`. The
/// first such url in the list wins; `turns:` and `transport=tcp` are skipped.
pub fn turn_udp_server(urls: &[String]) -> Option<(String, u16)> {
    urls.iter().find_map(|u| {
        let rest = u.strip_prefix("turn:")?;
        let (hostport, query) = match rest.split_once('?') {
            Some((h, q)) => (h, Some(q)),
            None => (rest, None),
        };
        if let Some(q) = query {
            let udp = q
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .all(|(k, v)| {
                    !k.eq_ignore_ascii_case("transport") || v.eq_ignore_ascii_case("udp")
                });
            if !udp {
                return None;
            }
        }
        if hostport.is_empty() || hostport.starts_with('[') {
            return None; // an IPv6 literal: the hub's socket is IPv4
        }
        match hostport.rsplit_once(':') {
            Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
            None => Some((hostport.to_string(), TURN_DEFAULT_PORT)),
        }
    })
}

/// The ICE priority the answer gives the relayed candidate: type preference 0 (relayed, RFC 8445
/// §5.1.2.2), local preference 65535, component 1. Lower than any host candidate, so a phone that
/// can reach the hub directly (the venue Wi-Fi) nominates the host pair, never the relay.
pub const RELAY_CANDIDATE_PRIORITY: u32 = (65_535 << 8) | 255;

/// The answer's line for the relayed address, typed as what it is.
///
/// str0m's ICE-lite agent takes HOST candidates only (`is` 0.12 `agent.rs` `add_local_candidate`:
/// "Reject non-host candidate due to ice-lite mode"), so the hub adds its relayed address as a
/// host candidate: str0m then pairs a datagram whose destination is the relayed address with it
/// and answers from it, which is all a lite agent does. The SDP line str0m writes for it says
/// `typ host` with a host priority, though, and a phone on the venue Wi-Fi that reaches both would
/// then prefer the relay. This rewrites exactly that line to `typ relay` with
/// [`RELAY_CANDIDATE_PRIORITY`] (and the `raddr`/`rport` a non-host candidate needs); every other
/// line is unchanged.
pub fn advertise_relayed(sdp: &str, relayed: SocketAddr) -> String {
    let ip = relayed.ip().to_string();
    let port = relayed.port().to_string();
    let mut out = String::with_capacity(sdp.len() + 64);
    for line in sdp.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let eol = &line[body.len()..];
        if let Some(cand) = body.strip_prefix("a=candidate:") {
            let f: Vec<&str> = cand.split_whitespace().collect();
            if f.len() >= 8 && f[4] == ip && f[5] == port && f[6] == "typ" && f[7] == "host" {
                let rest = f[8..].join(" ");
                out.push_str(&format!(
                    "a=candidate:{} {} {} {RELAY_CANDIDATE_PRIORITY} {ip} {port} typ relay raddr 0.0.0.0 rport 0{}{}{eol}",
                    f[0],
                    f[1],
                    f[2],
                    if rest.is_empty() { "" } else { " " },
                    rest
                ));
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

#[derive(Debug, Clone)]
enum TxKind {
    Allocate {
        signed: bool,
    },
    /// LIFETIME 0 = the release before a rotation.
    Refresh {
        lifetime: Duration,
    },
    Permission(IpAddr),
    ChannelBind(SocketAddr, u16),
    /// A STUN Binding request: the idle relay's NAT keepalive.
    Keepalive,
    /// A Refresh of an allocation the hub took for lost but the server still holds (its Allocate
    /// got 437 right after the loss): the relayed address it had.
    Readopt(SocketAddr),
}

struct Tx {
    kind: TxKind,
    raw: Vec<u8>,
    started: Instant,
    sends: usize,
    nonce_retries: u8,
}

struct Allocation {
    relayed: SocketAddr,
    lifetime: Duration,
    expires_at: Instant,
    refresh_at: Instant,
    refreshing: bool,
}

struct Perm {
    permitted: bool,
    in_flight: bool,
    /// When to ask (again): the refresh, or the retry after a refusal.
    next_at: Instant,
}

struct Chan {
    number: u16,
    ready: bool,
    in_flight: bool,
    /// A refused bind: Send indications until then, then the bind is tried again.
    failed_until: Option<Instant>,
    bound_at: Instant,
    last_used: Instant,
    /// Its peer's IP is no longer wanted: kept (number and all) for [`TURN_CHANNEL_REMEMBER`].
    departed: bool,
}

/// A global unicast IPv4 address: not RFC 1918, 100.64/10, link-local, loopback, unspecified,
/// broadcast or multicast.
fn is_global_v4(ip: Ipv4Addr) -> bool {
    let shared = ip.octets()[0] == 100 && (ip.octets()[1] & 0xC0) == 64;
    !(ip.is_private()
        || shared
        || ip.is_link_local()
        || ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast())
}

/// The hub's TURN client (see the module doc).
pub struct TurnRelay {
    crypto: Arc<dyn RTCCryptoProvider>,
    opts: TurnOptions,
    creds: Option<TurnCreds>,
    /// A rotated credential waiting for the release of the current allocation.
    next: Option<TurnCreds>,
    phase: TurnPhase,
    realm: String,
    nonce: String,
    /// MD5(username:realm:password) of `creds`, once the realm is known.
    key: Option<Vec<u8>>,
    alloc: Option<Allocation>,
    txs: HashMap<TransactionId, Tx>,
    wanted: HashSet<IpAddr>,
    perms: HashMap<IpAddr, Perm>,
    chans: HashMap<SocketAddr, Chan>,
    /// number -> peer, for the inbound ChannelData.
    chan_peer: HashMap<u16, SocketAddr>,
    next_channel: u16,
    out: VecDeque<Vec<u8>>,
    /// When the last datagram went to the TURN server (the keepalive's clock).
    last_sent: Instant,
    /// When an allocation was last lost, and its relayed address.
    last_loss: Option<Instant>,
    lost_relayed: Option<SocketAddr>,
    backoff: Duration,
    retry_at: Option<Instant>,
    /// When the last release (a rotation) was answered.
    released_at: Option<Instant>,
    allocations: u64,
    refreshes: u64,
    failures: u64,
    permissions_refused: u64,
    rx_packets: u64,
    tx_packets: u64,
    tx_refused: u64,
    last_error: Option<String>,
}

fn error_code(m: &Message) -> (u16, String) {
    let mut e = ErrorCodeAttribute::default();
    match e.get_from(m) {
        Ok(()) => (e.code.0, e.to_string()),
        Err(_) => (0, "an error answer without ERROR-CODE".to_string()),
    }
}

impl TurnRelay {
    /// A relay waiting for its first credential.
    pub fn new(opts: TurnOptions) -> Result<Self, String> {
        let crypto = rtc_turn::crypto::default_provider()
            .map_err(|e| format!("no crypto provider for the TURN client: {e}"))?;
        Ok(TurnRelay {
            crypto,
            opts,
            creds: None,
            next: None,
            phase: TurnPhase::Waiting,
            realm: String::new(),
            nonce: String::new(),
            key: None,
            alloc: None,
            txs: HashMap::new(),
            wanted: HashSet::new(),
            perms: HashMap::new(),
            chans: HashMap::new(),
            chan_peer: HashMap::new(),
            next_channel: MIN_CHANNEL_NUMBER,
            out: VecDeque::new(),
            last_sent: Instant::now(),
            last_loss: None,
            lost_relayed: None,
            backoff: TURN_BACKOFF_MIN,
            retry_at: None,
            released_at: None,
            allocations: 0,
            refreshes: 0,
            failures: 0,
            permissions_refused: 0,
            rx_packets: 0,
            tx_packets: 0,
            tx_refused: 0,
            last_error: None,
        })
    }

    /// Where the relay is.
    pub fn phase(&self) -> TurnPhase {
        self.phase
    }

    /// The relayed address while allocated.
    pub fn relayed(&self) -> Option<SocketAddr> {
        match self.phase {
            TurnPhase::Allocated => self.alloc.as_ref().map(|a| a.relayed),
            _ => None,
        }
    }

    /// The TURN server this relay talks to.
    pub fn server(&self) -> Option<SocketAddr> {
        self.creds.as_ref().map(|c| c.server)
    }

    /// A datagram from `from` belongs to the relay (the server's exact address).
    pub fn is_server(&self, from: SocketAddr) -> bool {
        self.server() == Some(from)
    }

    fn enter(&mut self, phase: TurnPhase) {
        if self.phase == phase {
            return;
        }
        let from = self.phase;
        self.phase = phase;
        let server = self.server().map(|s| s.to_string()).unwrap_or_default();
        match phase {
            TurnPhase::Allocated => {
                let (relayed, lifetime) = self
                    .alloc
                    .as_ref()
                    .map(|a| (a.relayed.to_string(), a.lifetime.as_secs()))
                    .unwrap_or_default();
                tracing::info!(from = from.name(), %server, %relayed, lifetime_s = lifetime, "webrtc turn: relay allocated");
            }
            TurnPhase::Backoff => {
                tracing::warn!(
                    from = from.name(),
                    %server,
                    error = self.last_error.as_deref().unwrap_or(""),
                    retry_in_s = self.backoff.as_secs_f64(),
                    "webrtc turn: relay failed, backing off (counted on /api/state webrtc.turn.failures)"
                );
            }
            TurnPhase::Releasing if from == TurnPhase::Allocated => {
                tracing::info!(%server, "webrtc turn: credential rotated, releasing the allocation to allocate again");
            }
            TurnPhase::Releasing => {
                tracing::debug!(%server, "webrtc turn: waiting for the server to drop the released allocation");
            }
            TurnPhase::Off => {
                tracing::info!(
                    reason = self.last_error.as_deref().unwrap_or(""),
                    "webrtc turn: off"
                );
            }
            TurnPhase::Waiting | TurnPhase::Allocating => {
                tracing::debug!(from = from.name(), to = phase.name(), %server, "webrtc turn: state");
            }
        }
    }

    /// No credential source: the reason shows on the facet; an allocation that exists keeps
    /// running until it fails (a mint error does not end a live credential).
    pub fn set_unavailable(&mut self, reason: &str) {
        self.last_error = Some(reason.to_string());
        if self.creds.is_none() {
            self.enter(TurnPhase::Off);
        }
    }

    /// The minter's current credential. The same username and server again only moves the expiry;
    /// another one is a rotation (see the module doc).
    pub fn set_credentials(&mut self, creds: TurnCreds, now: Instant) {
        match &self.creds {
            Some(c) if c.username == creds.username && c.server == creds.server => {
                if let Some(c) = self.creds.as_mut() {
                    c.expires_at = creds.expires_at;
                    c.password = creds.password;
                }
                return;
            }
            None => {
                self.creds = Some(creds);
                self.start_allocate(now);
                return;
            }
            Some(_) => {}
        }
        match self.phase {
            TurnPhase::Allocated | TurnPhase::Releasing => {
                tracing::info!(
                    username_changed = true,
                    "webrtc turn: a new credential is waiting for the rotation"
                );
                self.next = Some(creds);
            }
            _ => {
                // Nothing to release: start over with the new credential at once.
                self.creds = Some(creds);
                self.next = None;
                self.backoff = TURN_BACKOFF_MIN;
                self.start_allocate(now);
            }
        }
    }

    /// The peer IPs the live phones may send from. Global IPv4 only (non-global ones only with
    /// the test-only `allow_private_peers`), at most `max_permissions`; the rest is counted in
    /// `permissions_refused`. IPs no longer wanted lose their permission here (the hub stops
    /// refreshing it); their channels are kept as departed for [`TURN_CHANNEL_REMEMBER`].
    pub fn set_wanted(&mut self, ips: impl IntoIterator<Item = IpAddr>, now: Instant) {
        let mut wanted = HashSet::new();
        for ip in ips {
            let IpAddr::V4(v4) = ip else { continue };
            if wanted.contains(&ip) || (!self.opts.allow_private_peers && !is_global_v4(v4)) {
                continue;
            }
            if wanted.len() >= self.opts.max_permissions {
                self.permissions_refused += 1;
                continue;
            }
            wanted.insert(ip);
        }
        if self.permissions_refused > 0 && wanted.len() == self.opts.max_permissions {
            self.last_error = Some(format!(
                "permission table full ({} peer IPs)",
                self.opts.max_permissions
            ));
        }
        self.perms.retain(|ip, _| wanted.contains(ip));
        for (peer, c) in &mut self.chans {
            if !wanted.contains(&peer.ip()) {
                c.departed = true;
            }
        }
        self.wanted = wanted;
        self.prune_channels(now);
        self.ask_permissions(now);
    }

    /// Forget the departed channels the server no longer holds (or never bound), and keep the
    /// table bounded: at most 4 x `max_channels` entries, the oldest departed go first.
    fn prune_channels(&mut self, now: Instant) {
        let before = self.chans.len();
        self.chans.retain(|_, c| {
            !c.departed
                || ((c.ready || c.in_flight)
                    && now.saturating_duration_since(c.bound_at) < TURN_CHANNEL_REMEMBER)
        });
        let cap = self.opts.max_channels.saturating_mul(4).max(1);
        while self.chans.len() > cap {
            let oldest = self
                .chans
                .iter()
                .filter(|(_, c)| c.departed)
                .min_by_key(|(_, c)| c.bound_at)
                .map(|(p, _)| *p);
            match oldest {
                Some(p) => {
                    self.chans.remove(&p);
                }
                None => break,
            }
        }
        if self.chans.len() != before {
            let live: HashSet<u16> = self.chans.values().map(|c| c.number).collect();
            self.chan_peer.retain(|n, _| live.contains(n));
            tracing::debug!(
                forgotten = before - self.chans.len(),
                "webrtc turn: departed channels forgotten"
            );
        }
    }

    /// A channel number no remembered channel holds, in 0x4000..=0x4FFF.
    fn free_channel_number(&mut self) -> Option<u16> {
        let span = usize::from(TURN_MAX_CHANNEL_NUMBER - MIN_CHANNEL_NUMBER) + 1;
        for _ in 0..span {
            let n = self.next_channel;
            self.next_channel = if n >= TURN_MAX_CHANNEL_NUMBER {
                MIN_CHANNEL_NUMBER
            } else {
                n + 1
            };
            if !self.chan_peer.contains_key(&n) {
                return Some(n);
            }
        }
        None
    }

    /// A datagram for the TURN server.
    fn emit(&mut self, d: Vec<u8>, now: Instant) {
        self.out.push_back(d);
        self.last_sent = now;
    }

    fn start_allocate(&mut self, now: Instant) {
        self.alloc = None;
        self.retry_at = None;
        self.txs.clear();
        for p in self.perms.values_mut() {
            p.permitted = false;
            p.in_flight = false;
            p.next_at = now;
        }
        self.chans.clear();
        self.chan_peer.clear();
        self.next_channel = MIN_CHANNEL_NUMBER;
        if self.creds.is_none() {
            self.enter(TurnPhase::Waiting);
            return;
        }
        // Unsigned first: the answer carries the realm and the nonce (a cached nonce may be stale).
        self.request(TxKind::Allocate { signed: false }, now);
        self.enter(TurnPhase::Allocating);
    }

    fn fail(&mut self, reason: String, now: Instant) {
        self.failures += 1;
        self.last_error = Some(reason);
        self.alloc = None;
        self.txs.clear();
        for p in self.perms.values_mut() {
            p.permitted = false;
            p.in_flight = false;
        }
        self.chans.clear();
        self.chan_peer.clear();
        self.retry_at = Some(now + self.backoff);
        self.enter(TurnPhase::Backoff);
        self.backoff = (self.backoff * 2).min(TURN_BACKOFF_MAX);
    }

    /// The allocation is gone (a 437, no answer, an expiry, a refused refresh): allocate again AT
    /// ONCE, unless one was already lost within [`TURN_LOSS_AT_ONCE_WINDOW`] (then back off).
    fn lost(&mut self, reason: String, now: Instant) {
        let recent = self
            .last_loss
            .is_some_and(|l| now.saturating_duration_since(l) < TURN_LOSS_AT_ONCE_WINDOW);
        self.last_loss = Some(now);
        self.lost_relayed = self.alloc.as_ref().map(|a| a.relayed);
        if recent {
            self.fail(reason, now);
            return;
        }
        self.failures += 1;
        tracing::warn!(error = %reason, "webrtc turn: the allocation is lost, allocating again at once (counted on /api/state webrtc.turn.failures)");
        self.last_error = Some(reason);
        self.start_allocate(now);
    }

    fn build(&self, kind: &TxKind) -> Result<Message, String> {
        let mut setters: Vec<Box<dyn Setter + '_>> = vec![Box::new(TransactionId::new())];
        let signed = match kind {
            TxKind::Allocate { signed } => {
                setters.push(Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)));
                setters.push(Box::new(RequestedTransport {
                    protocol: PROTO_UDP,
                }));
                setters.push(Box::new(Lifetime(TURN_REQUEST_LIFETIME)));
                *signed
            }
            TxKind::Refresh { lifetime } => {
                setters.push(Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)));
                setters.push(Box::new(Lifetime(*lifetime)));
                true
            }
            TxKind::Readopt(_) => {
                setters.push(Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)));
                setters.push(Box::new(Lifetime(TURN_REQUEST_LIFETIME)));
                true
            }
            TxKind::Permission(ip) => {
                setters.push(Box::new(MessageType::new(
                    METHOD_CREATE_PERMISSION,
                    CLASS_REQUEST,
                )));
                setters.push(Box::new(PeerAddress { ip: *ip, port: 0 }));
                true
            }
            TxKind::ChannelBind(peer, number) => {
                setters.push(Box::new(MessageType::new(
                    METHOD_CHANNEL_BIND,
                    CLASS_REQUEST,
                )));
                setters.push(Box::new(PeerAddress {
                    ip: peer.ip(),
                    port: peer.port(),
                }));
                setters.push(Box::new(ChannelNumber(*number)));
                true
            }
            TxKind::Keepalive => {
                setters.push(Box::new(BINDING_REQUEST));
                false
            }
        };
        let crypto = self.crypto.crypto();
        if signed {
            let (Some(creds), Some(key)) = (self.creds.as_ref(), self.key.as_ref()) else {
                return Err("no credential to sign the request with".into());
            };
            setters.push(Box::new(TextAttribute::new(
                ATTR_USERNAME,
                creds.username.clone(),
            )));
            setters.push(Box::new(TextAttribute::new(ATTR_REALM, self.realm.clone())));
            setters.push(Box::new(TextAttribute::new(ATTR_NONCE, self.nonce.clone())));
            setters.push(Box::new(MessageIntegrity::new_raw_integrity_with_provider(
                key.clone(),
                crypto,
            )));
        }
        setters.push(Box::new(FINGERPRINT));
        let mut m = Message::new();
        m.build(&setters)
            .map_err(|e| format!("cannot encode a TURN request: {e}"))?;
        Ok(m)
    }

    fn request(&mut self, kind: TxKind, now: Instant) {
        self.request_retry(kind, now, 0);
    }

    fn request_retry(&mut self, kind: TxKind, now: Instant, nonce_retries: u8) {
        match self.build(&kind) {
            Ok(m) => {
                self.emit(m.raw.clone(), now);
                self.txs.insert(
                    m.transaction_id,
                    Tx {
                        kind,
                        raw: m.raw,
                        started: now,
                        sends: 1,
                        nonce_retries,
                    },
                );
            }
            Err(e) => self.fail(e, now),
        }
    }

    /// The datagrams for the TURN server, in order.
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    /// The earliest time [`TurnRelay::handle_timeout`] has work.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = None;
        let mut at = |x: Instant| t = Some(t.map_or(x, |y| y.min(x)));
        for tx in self.txs.values() {
            match TX_SENDS_MS.get(tx.sends) {
                Some(ms) => at(tx.started + Duration::from_millis(*ms)),
                None => at(tx.started + TURN_TX_TIMEOUT),
            }
        }
        if let Some(r) = self.retry_at {
            at(r);
        }
        if let Some(a) = &self.alloc {
            if self.phase == TurnPhase::Allocated {
                if !a.refreshing {
                    at(a.refresh_at);
                }
                at(a.expires_at);
                at(self.last_sent + TURN_KEEPALIVE);
                for p in self.perms.values() {
                    if !p.in_flight {
                        at(p.next_at);
                    }
                }
            }
        }
        if let (Some(c), true) = (&self.creds, self.next.is_some()) {
            at(c.expires_at
                .checked_sub(TURN_ROTATE_MARGIN)
                .unwrap_or(c.expires_at));
        }
        t
    }

    /// Time moves on: retransmissions and timeouts, the back-off, the refresh, a rotation, the
    /// permissions. `busy` = a phone is on the relay right now (a rotation then waits).
    pub fn handle_timeout(&mut self, now: Instant, busy: bool) {
        // Retransmit, or give up on, the requests in flight.
        let mut timed_out = Vec::new();
        let mut resent = Vec::new();
        for (id, tx) in &mut self.txs {
            if let Some(ms) = TX_SENDS_MS.get(tx.sends) {
                if now >= tx.started + Duration::from_millis(*ms) {
                    resent.push(tx.raw.clone());
                    tx.sends += 1;
                }
            } else if now >= tx.started + TURN_TX_TIMEOUT {
                timed_out.push(*id);
            }
        }
        for d in resent {
            self.emit(d, now);
        }
        for id in timed_out {
            if let Some(tx) = self.txs.remove(&id) {
                self.timed_out(tx.kind, now);
            }
        }
        let retry_due = self.retry_at.is_some_and(|r| now >= r);
        if retry_due && matches!(self.phase, TurnPhase::Backoff | TurnPhase::Releasing) {
            self.start_allocate(now);
        }
        if self.phase == TurnPhase::Allocated {
            self.rotate_if_due(now, busy);
        }
        if self.phase == TurnPhase::Allocated {
            let (expired, refresh) = match &self.alloc {
                Some(a) => (now >= a.expires_at, !a.refreshing && now >= a.refresh_at),
                None => (false, false),
            };
            if expired {
                self.lost("the allocation expired without a refresh".into(), now);
                return;
            }
            if refresh {
                if let Some(a) = self.alloc.as_mut() {
                    a.refreshing = true;
                }
                self.request(
                    TxKind::Refresh {
                        lifetime: TURN_REQUEST_LIFETIME,
                    },
                    now,
                );
            }
            self.ask_permissions(now);
            self.refresh_channels(now);
            self.prune_channels(now);
            let keepalive_out = self
                .txs
                .values()
                .any(|t| matches!(t.kind, TxKind::Keepalive));
            if !keepalive_out && now >= self.last_sent + TURN_KEEPALIVE {
                self.request(TxKind::Keepalive, now);
            }
        }
    }

    fn rotate_if_due(&mut self, now: Instant, busy: bool) {
        let Some(old) = &self.creds else { return };
        if self.next.is_none() {
            return;
        }
        let near_expiry = old.expires_at.saturating_duration_since(now) <= TURN_ROTATE_MARGIN;
        if busy && !near_expiry {
            return;
        }
        self.txs.clear();
        self.request(
            TxKind::Refresh {
                lifetime: Duration::ZERO,
            },
            now,
        );
        self.enter(TurnPhase::Releasing);
    }

    /// The release is over (answered, refused or timed out): allocate with the new credential.
    fn released(&mut self, now: Instant) {
        if let Some(next) = self.next.take() {
            self.creds = Some(next);
        }
        self.key = None;
        self.backoff = TURN_BACKOFF_MIN;
        self.released_at = Some(now);
        self.start_allocate(now);
    }

    fn timed_out(&mut self, kind: TxKind, now: Instant) {
        match kind {
            TxKind::Allocate { .. } => {
                self.fail("allocate: no answer from the TURN server".into(), now);
            }
            TxKind::Refresh { lifetime } if lifetime.is_zero() => self.released(now),
            // Any other request unanswered after its retransmissions: the server no longer
            // hears this 5-tuple (a dropped NAT mapping, a lost allocation). Allocate again.
            TxKind::Refresh { .. } => {
                self.lost("refresh: no answer from the TURN server".into(), now);
            }
            TxKind::Permission(_) => {
                self.lost("permission: no answer from the TURN server".into(), now);
            }
            TxKind::ChannelBind(..) => {
                self.lost("channel bind: no answer from the TURN server".into(), now);
            }
            TxKind::Keepalive => {
                self.lost("keepalive: no answer from the TURN server".into(), now);
            }
            TxKind::Readopt(_) => {
                self.fail("refresh of the held allocation: no answer".into(), now);
            }
        }
    }

    fn ask_permissions(&mut self, now: Instant) {
        if self.phase != TurnPhase::Allocated {
            return;
        }
        let mut due = Vec::new();
        for ip in &self.wanted {
            match self.perms.get(ip) {
                None => due.push(*ip),
                Some(p) if !p.in_flight && now >= p.next_at => due.push(*ip),
                _ => {}
            }
        }
        for ip in due {
            let p = self.perms.entry(ip).or_insert(Perm {
                permitted: false,
                in_flight: false,
                next_at: now,
            });
            p.in_flight = true;
            self.request(TxKind::Permission(ip), now);
        }
    }

    fn refresh_channels(&mut self, now: Instant) {
        let mut due = Vec::new();
        for (peer, c) in &mut self.chans {
            let used = now.saturating_duration_since(c.last_used) < TURN_CHANNEL_REFRESH;
            if c.ready && !c.in_flight && used && now >= c.bound_at + TURN_CHANNEL_REFRESH {
                c.in_flight = true;
                due.push((*peer, c.number));
            }
        }
        for (peer, n) in due {
            self.request(TxKind::ChannelBind(peer, n), now);
        }
    }

    /// One datagram from the TURN server. Data indications and ChannelData come back as
    /// `(peer, payload)` (feed it to str0m from `peer` to the relayed address); responses move the
    /// state machine and return `None`, as does anything malformed or unexpected.
    pub fn handle_datagram(&mut self, data: &[u8], now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        if !is_stun_message(data) {
            if !ChannelData::is_channel_data(data) {
                return None;
            }
            let mut cd = ChannelData {
                raw: data.to_vec(),
                ..Default::default()
            };
            cd.decode().ok()?;
            // Any channel the hub bound, a departed peer's too (the server still delivers it).
            let peer = *self.chan_peer.get(&cd.number.0)?;
            self.rx_packets += 1;
            return Some((peer, cd.data));
        }
        let mut m = Message::new();
        m.raw = data.to_vec();
        m.decode().ok()?;
        if m.typ.class == CLASS_INDICATION {
            if m.typ.method != METHOD_DATA || self.phase != TurnPhase::Allocated {
                return None;
            }
            let mut peer = PeerAddress::default();
            let mut d = Data::default();
            peer.get_from(&m).ok()?;
            d.get_from(&m).ok()?;
            if !self.perms.contains_key(&peer.ip) {
                return None;
            }
            self.rx_packets += 1;
            return Some((SocketAddr::new(peer.ip, peer.port), d.0));
        }
        if m.typ.class != CLASS_SUCCESS_RESPONSE && m.typ.class != CLASS_ERROR_RESPONSE {
            return None;
        }
        let tx = self.txs.remove(&m.transaction_id)?;
        self.answered(tx, &m, now);
        None
    }

    fn answered(&mut self, tx: Tx, m: &Message, now: Instant) {
        let ok = m.typ.class == CLASS_SUCCESS_RESPONSE;
        let (code, reason) = if ok {
            (0, String::new())
        } else {
            error_code(m)
        };
        // A stale nonce (438) on a signed request: take the new nonce, ask again.
        if code == 438 && !matches!(tx.kind, TxKind::Allocate { signed: false }) {
            if let Ok(n) = TextAttribute::get_from_as(m, ATTR_NONCE) {
                self.nonce = n.text;
            }
            if tx.nonce_retries < MAX_NONCE_RETRIES {
                self.request_retry(tx.kind, now, tx.nonce_retries + 1);
                return;
            }
        }
        let settling = self
            .released_at
            .is_some_and(|r| now.saturating_duration_since(r) < TURN_RELEASE_SETTLE);
        if code == 437 && settling && matches!(tx.kind, TxKind::Allocate { .. }) {
            // The released allocation is still on the server (signed or not, the 5-tuple is
            // taken): wait for it to go, no failure.
            tracing::debug!("webrtc turn: the released allocation is still on the server, allocating again shortly");
            self.txs.clear();
            self.retry_at = Some(now + RELEASE_RETRY);
            self.enter(TurnPhase::Releasing);
            return;
        }
        // Our Allocate got 437 right after we took the allocation for lost (a request went
        // unanswered for a few seconds, the server did not lose it): the server still holds it
        // for this 5-tuple. Refresh it and carry on with its relayed address, instead of waiting
        // out its lifetime (~10 min) in back-off.
        let readopt = self.lost_relayed.filter(|_| {
            self.last_loss
                .is_some_and(|l| now.saturating_duration_since(l) < TURN_LOSS_AT_ONCE_WINDOW)
        });
        if let (437, TxKind::Allocate { .. }, Some(relayed), true) =
            (code, &tx.kind, readopt, self.key.is_some())
        {
            tracing::info!(%relayed, "webrtc turn: the server still holds the allocation, refreshing it");
            self.request(TxKind::Readopt(relayed), now);
            return;
        }
        match tx.kind {
            TxKind::Readopt(relayed) => {
                if !ok {
                    self.fail(
                        format!("refresh of the held allocation refused: {reason}"),
                        now,
                    );
                    return;
                }
                let mut l = Lifetime::default();
                let lifetime = if l.get_from(m).is_ok() && !l.0.is_zero() {
                    l.0
                } else {
                    TURN_REQUEST_LIFETIME
                };
                self.lost_relayed = None;
                self.alloc = Some(Allocation {
                    relayed,
                    lifetime,
                    expires_at: now + lifetime,
                    refresh_at: now + self.refresh_interval(lifetime),
                    refreshing: false,
                });
                self.refreshes += 1;
                self.backoff = TURN_BACKOFF_MIN;
                self.retry_at = None;
                self.enter(TurnPhase::Allocated);
                self.ask_permissions(now);
            }
            TxKind::Allocate { signed: false } => {
                if code == 401 {
                    let realm = TextAttribute::get_from_as(m, ATTR_REALM).map(|t| t.text);
                    let nonce = TextAttribute::get_from_as(m, ATTR_NONCE).map(|t| t.text);
                    let (Ok(realm), Ok(nonce)) = (realm, nonce) else {
                        self.fail("allocate: 401 without REALM / NONCE".into(), now);
                        return;
                    };
                    let Some(creds) = self.creds.as_ref() else {
                        self.fail("allocate: no credential".into(), now);
                        return;
                    };
                    match MessageIntegrity::long_term_integrity_key(
                        creds.username.clone(),
                        realm.clone(),
                        creds.password.clone(),
                        self.crypto.crypto(),
                    ) {
                        Ok(key) => {
                            self.realm = realm;
                            self.nonce = nonce;
                            self.key = Some(key);
                            self.request(TxKind::Allocate { signed: true }, now);
                        }
                        Err(e) => self.fail(format!("allocate: no key: {e}"), now),
                    }
                } else if ok {
                    // A server without authentication: take the allocation as it is.
                    self.allocated(m, now);
                } else {
                    self.fail(format!("allocate refused: {reason}"), now);
                }
            }
            TxKind::Allocate { signed: true } => {
                if ok {
                    self.allocated(m, now);
                } else {
                    self.fail(format!("allocate refused: {reason}"), now);
                }
            }
            TxKind::Refresh { lifetime } if lifetime.is_zero() => self.released(now),
            TxKind::Refresh { .. } => {
                if !ok {
                    self.lost(format!("refresh refused: {reason}"), now);
                    return;
                }
                let mut l = Lifetime::default();
                let granted = if l.get_from(m).is_ok() {
                    l.0
                } else {
                    TURN_REQUEST_LIFETIME
                };
                if granted.is_zero() {
                    self.lost("refresh: the server ended the allocation".into(), now);
                    return;
                }
                let refresh_at = now + self.refresh_interval(granted);
                if let Some(a) = self.alloc.as_mut() {
                    a.lifetime = granted;
                    a.expires_at = now + granted;
                    a.refresh_at = refresh_at;
                    a.refreshing = false;
                }
                self.refreshes += 1;
            }
            TxKind::Permission(ip) if code == 437 => {
                self.lost(format!("permission for {ip}: {reason}"), now);
            }
            TxKind::ChannelBind(peer, _) if code == 437 => {
                self.lost(format!("channel bind for {peer}: {reason}"), now);
            }
            TxKind::Keepalive => {}
            TxKind::Permission(ip) => {
                let Some(p) = self.perms.get_mut(&ip) else {
                    return; // no longer wanted
                };
                p.in_flight = false;
                if ok {
                    if !p.permitted {
                        tracing::debug!(peer = %ip, "webrtc turn: permission granted");
                    }
                    p.permitted = true;
                    p.next_at = now + TURN_PERMISSION_REFRESH;
                } else {
                    p.permitted = false;
                    p.next_at = now + PERMISSION_RETRY;
                    tracing::debug!(peer = %ip, %reason, "webrtc turn: permission refused");
                    self.last_error = Some(format!("permission for {ip} refused: {reason}"));
                }
            }
            TxKind::ChannelBind(peer, _) => {
                if let Some(c) = self.chans.get_mut(&peer) {
                    c.in_flight = false;
                    if ok {
                        c.ready = true;
                        c.failed_until = None;
                        c.bound_at = now;
                    } else {
                        c.ready = false;
                        c.failed_until = Some(now + TURN_CHANNEL_RETRY);
                        tracing::debug!(%peer, %reason, "webrtc turn: channel bind refused, Send indications until it is tried again");
                    }
                }
            }
        }
    }

    fn refresh_interval(&self, lifetime: Duration) -> Duration {
        let half = lifetime / 2;
        self.opts
            .refresh_cap
            .map_or(half, |c| half.min(c))
            .max(Duration::from_secs(1))
    }

    fn allocated(&mut self, m: &Message, now: Instant) {
        let mut relayed = RelayedAddress::default();
        if relayed.get_from(m).is_err() {
            self.fail(
                "allocate: the answer has no XOR-RELAYED-ADDRESS".into(),
                now,
            );
            return;
        }
        let mut l = Lifetime::default();
        let lifetime = if l.get_from(m).is_ok() && !l.0.is_zero() {
            l.0
        } else {
            TURN_REQUEST_LIFETIME
        };
        self.alloc = Some(Allocation {
            relayed: SocketAddr::new(relayed.ip, relayed.port),
            lifetime,
            expires_at: now + lifetime,
            refresh_at: now + self.refresh_interval(lifetime),
            refreshing: false,
        });
        self.allocations += 1;
        self.lost_relayed = None;
        self.backoff = TURN_BACKOFF_MIN;
        self.retry_at = None;
        self.enter(TurnPhase::Allocated);
        self.ask_permissions(now);
    }

    /// Wrap `payload` for `peer` (a Send indication, or ChannelData once bound). Refused without
    /// an allocation or a permission for the peer's IP: the hub never relays to an address the
    /// phone's offer did not name.
    pub fn send_to(
        &mut self,
        peer: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) -> Result<(), &'static str> {
        if self.phase != TurnPhase::Allocated {
            self.tx_refused += 1;
            return Err("no TURN allocation");
        }
        if !self.perms.get(&peer.ip()).is_some_and(|p| p.permitted) {
            self.tx_refused += 1;
            return Err("no TURN permission for the peer");
        }
        let live = self.chans.values().filter(|c| !c.departed).count();
        let mut bind = None;
        if let Some(c) = self.chans.get_mut(&peer) {
            // A returning peer takes its remembered channel back (the server still holds it).
            c.departed = false;
            c.last_used = now;
            if c.ready {
                let mut cd = ChannelData {
                    data: payload.to_vec(),
                    number: ChannelNumber(c.number),
                    ..Default::default()
                };
                cd.encode();
                self.emit(cd.raw, now);
                self.tx_packets += 1;
                return Ok(());
            }
            if !c.in_flight && c.failed_until.is_none_or(|u| now >= u) {
                c.in_flight = true;
                c.failed_until = None;
                bind = Some(c.number);
            }
        } else if live < self.opts.max_channels {
            if let Some(number) = self.free_channel_number() {
                self.chans.insert(
                    peer,
                    Chan {
                        number,
                        ready: false,
                        in_flight: true,
                        failed_until: None,
                        bound_at: now,
                        last_used: now,
                        departed: false,
                    },
                );
                self.chan_peer.insert(number, peer);
                bind = Some(number);
            }
        }
        if let Some(number) = bind {
            self.request(TxKind::ChannelBind(peer, number), now);
        }
        let mut m = Message::new();
        let built = m.build(&[
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_SEND, CLASS_INDICATION)),
            Box::new(PeerAddress {
                ip: peer.ip(),
                port: peer.port(),
            }),
            Box::new(Data(payload.to_vec())),
        ]);
        if built.is_err() {
            self.tx_refused += 1;
            return Err("cannot encode a Send indication");
        }
        self.emit(m.raw, now);
        self.tx_packets += 1;
        Ok(())
    }

    /// A transmit str0m wanted to send from a relayed address that is not (or no longer) the
    /// live one: dropped, counted here (`tx_refused`).
    pub fn refuse(&mut self) {
        self.tx_refused += 1;
    }

    /// The `/api/state` facet.
    pub fn facet(&self, now: Instant) -> TurnFacet {
        let alloc = self
            .alloc
            .as_ref()
            .filter(|_| self.phase == TurnPhase::Allocated);
        TurnFacet {
            state: self.phase.name(),
            server: self.server().map(|s| s.to_string()),
            relayed: alloc.map(|a| a.relayed.to_string()),
            lifetime_left_s: alloc.map(|a| a.expires_at.saturating_duration_since(now).as_secs()),
            allocations: self.allocations,
            refreshes: self.refreshes,
            failures: self.failures,
            last_error: self.last_error.clone(),
            permissions: self.perms.values().filter(|p| p.permitted).count(),
            permissions_refused: self.permissions_refused,
            channels: self
                .chans
                .values()
                .filter(|c| !c.departed && c.failed_until.is_none())
                .count(),
            rx_packets: self.rx_packets,
            tx_packets: self.tx_packets,
            tx_refused: self.tx_refused,
        }
    }
}
