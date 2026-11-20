//! The hub's own TURN relay for the in-hub WebRTC phone leg (intercom#42 phase 3, design comment
//! 7041415746). The venue uplink is CGNAT, so the hub has only LAN host candidates and a phone on
//! mobile data cannot reach any of them, not even through its own TURN relay. The hub therefore
//! allocates ITS OWN relayed address on the same Cloudflare TURN service (credentials from
//! [`crate::turn::TurnMinter`]) and offers it in every answer next to the host candidates.
//!
//! [`TurnRelay`] is sans-IO, owned by the network task ([`crate::webrtc_leg`]). **Every
//! allocation talks from its OWN ephemeral UDP socket** (an `epoch`; the network task binds one
//! per epoch it sees in [`TurnRelay::poll_transmit`] and closes the ones
//! [`TurnRelay::live_epochs`] no longer names). A new allocation is a new 5-tuple, so it never meets
//! a 437 from an allocation the server still holds (a hub restart, a passing outage, a rotation),
//! and it starts with empty permission and channel tables by construction (coordinator,
//! 21.11.2026: the root fix after three layers of 437 handling).
//!
//! - **The allocation** (UDP over UDP, the long-term credential: an unsigned Allocate gets 401 with
//!   REALM + NONCE, the signed one the relayed address), refreshed at half its lifetime (or
//!   `refresh_cap`), a 438 stale nonce retried with the new nonce, kept alive by a STUN Binding
//!   request whenever nothing went to the server for [`TURN_KEEPALIVE`] (the venue NAT's mapping).
//!   A refused or unanswered Allocate backs off 1 s -> 60 s. A LOST allocation (a 437 or no answer
//!   on any other request, a refused refresh, an expiry) is ABANDONED (it expires on the server; no
//!   release on a dead path) and a new one allocated at once from a new socket; a second loss
//!   within [`TURN_LOSS_AT_ONCE_WINDOW`] backs off. One log line per state change, never per
//!   packet.
//! - **A credential rotation** (a new username from the minter) allocates with the NEW credential
//!   on a new socket while the old allocation keeps carrying the phones, switches the answers'
//!   relayed candidate over once it is up, then releases the old allocation with the OLD
//!   credential on the OLD socket (best effort) and closes that socket after
//!   [`TURN_RETIRE_GRACE`]. A phone still on the old relay loses it then and reconnects through the
//!   page's watchdog (~4 s). The rotation waits while a phone is on the relay, but never past
//!   [`TURN_ROTATE_MARGIN`] before the old credential expires.
//! - **Permissions per IP** (`set_wanted`: the plausible addresses of the live phones' offers),
//!   global IPv4 only, at most `max_permissions`, refreshed every [`TURN_PERMISSION_REFRESH`] only
//!   while wanted; the server lets a permission lapse after 300 s once the hub stops refreshing it.
//! - **Sends** (`send_to`): a Send indication, and once a ChannelBind for that peer succeeded,
//!   ChannelData (4 bytes of header instead of 36), at most `max_channels` live; a departed peer's
//!   channel is remembered [`TURN_CHANNEL_REMEMBER`] and reused (bound again first once the
//!   server's 600 s binding ran out). Never to a peer without a permitted IP. **Receives**
//!   (`handle_datagram`): Data indications and ChannelData unwrap to (peer, payload) for str0m;
//!   responses drive the state machine; anything else is dropped.
//!
//! ICE-lite is unchanged: the relay is a transport, never a reason to send. str0m still only
//! ANSWERS the phone's checks; a TURN permission is not a check. Everything this relay emits is
//! addressed to the TURN server. The password never reaches a log line, a Debug print or the facet.
//!
//! Responses are matched by their 96-bit random transaction id and their socket epoch; their
//! MESSAGE-INTEGRITY is not checked (they only move this client's own state, and only the server's
//! address on that epoch's socket is accepted).

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
/// A rotation whose new allocation failed is tried again after this (the old one carries on).
pub const TURN_ROTATE_RETRY: Duration = Duration::from_secs(30);
/// After a rotation's switch, the old allocation's socket stays open this long for its release.
pub const TURN_RETIRE_GRACE: Duration = Duration::from_secs(2);
/// A permission lives 300 s on the server; the hub refreshes a wanted one after this.
pub const TURN_PERMISSION_REFRESH: Duration = Duration::from_secs(240);
/// A channel binding lives 600 s; one in use is bound again after this.
pub const TURN_CHANNEL_REFRESH: Duration = Duration::from_secs(300);
/// The server keeps a channel binding this long after its last bind (RFC 8656 §12).
pub const TURN_CHANNEL_LIFETIME: Duration = Duration::from_secs(600);
/// A request unanswered this long has failed (sent at 0, 0.5, 1.5 and 3.5 s).
pub const TURN_TX_TIMEOUT: Duration = Duration::from_millis(7500);
/// When the retransmissions go out, from the first send.
const TX_SENDS_MS: [u64; 4] = [0, 500, 1500, 3500];
/// A refused permission is asked again after this (no tight loop on a 403).
const PERMISSION_RETRY: Duration = Duration::from_secs(60);
/// How many 438 (stale nonce) answers one request may get before it fails.
const MAX_NONCE_RETRIES: u8 = 2;
/// The default TURN port (RFC 5766).
pub const TURN_DEFAULT_PORT: u16 = 3478;
/// While nothing else goes to the TURN server for this long, a STUN Binding request does (review
/// of phase 3): the venue's CGNAT / conntrack drops an idle UDP mapping in 30-180 s, and the only
/// other idle traffic is the Refresh every 300 s. Needs a TURN server that answers STUN Binding
/// (Cloudflare and coturn do; a coturn with `--no-stun` would not, and every keepalive would then
/// read as a lost allocation).
pub const TURN_KEEPALIVE: Duration = Duration::from_secs(15);
/// The last channel number (RFC 8656 §12: 0x4000 through 0x4FFF; 0x5000-0x7FFF are reserved).
pub const TURN_MAX_CHANNEL_NUMBER: u16 = 0x4FFF;
/// A departed peer's channel is remembered this long after its last bind: the server keeps the
/// binding 600 s and refuses its number / peer to another binding for 300 s more (RFC 8656 §12).
pub const TURN_CHANNEL_REMEMBER: Duration = Duration::from_secs(900);
/// A refused ChannelBind is tried again after this (Send indications meanwhile).
pub const TURN_CHANNEL_RETRY: Duration = Duration::from_secs(30);
/// A lost allocation is replaced at once, unless another was lost this recently (then the
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
            TurnPhase::Backoff => "backoff",
        }
    }
}

/// The `/api/state` `webrtc.turn` facet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnFacet {
    /// `off` / `waiting` / `allocating` / `allocated` / `backoff`.
    pub state: &'static str,
    /// The TURN server the hub talks to.
    pub server: Option<String>,
    /// The local address of the live allocation's socket (filled by the network task).
    pub local: Option<String>,
    /// The hub's relayed address (its `typ relay` candidate) while allocated.
    pub relayed: Option<String>,
    /// Seconds until the allocation expires unless refreshed.
    pub lifetime_left_s: Option<u64>,
    /// A rotation's new allocation is being made next to the live one.
    pub rotating: bool,
    /// Allocations made, refreshes answered, failures (lost allocations and refused Allocates).
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
    /// Sends refused: no allocation, no permission for the peer, or an old relayed source.
    pub tx_refused: u64,
}

/// One datagram for the TURN server, from the socket of allocation `epoch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnTransmit {
    pub epoch: u32,
    pub to: SocketAddr,
    pub data: Vec<u8>,
}

/// A global unicast IPv4 address: not 0.0.0.0/8, RFC 1918, 100.64/10, 127/8, 169.254/16,
/// 192.0.0.0/24, 198.18.0.0/15, 224/4 or 240/4 (and not the broadcast).
fn is_global_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let shared = o[0] == 100 && (o[1] & 0xC0) == 64;
    let this_net = o[0] == 0;
    let ietf = o[0] == 192 && o[1] == 0 && o[2] == 0;
    let benchmark = o[0] == 198 && (o[1] & 0xFE) == 18;
    let reserved = o[0] >= 240;
    !(ip.is_private()
        || shared
        || this_net
        || ietf
        || benchmark
        || reserved
        || ip.is_link_local()
        || ip.is_loopback()
        || ip.is_multicast())
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
    Refresh,
    Permission(IpAddr),
    ChannelBind(SocketAddr, u16),
    /// A STUN Binding request: the idle relay's NAT keepalive.
    Keepalive,
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

/// What one allocation reports to the relay.
enum Ev {
    Data(SocketAddr, Vec<u8>),
    Allocated,
    Refreshed,
    /// The Allocate was refused or went unanswered.
    AllocFailed(String),
    /// The allocation is gone (a 437 or no answer on any later request, a refused refresh, an
    /// expiry).
    Lost(String),
    /// A refusal worth showing (a permission refused), not a failure of the allocation.
    Note(String),
}

fn error_code(m: &Message) -> (u16, String) {
    let mut e = ErrorCodeAttribute::default();
    match e.get_from(m) {
        Ok(()) => (e.code.0, e.to_string()),
        Err(_) => (0, "an error answer without ERROR-CODE".to_string()),
    }
}

/// ONE allocation on its own socket (`epoch`): its credential, transactions, permissions and
/// channels. A new allocation is a new `Session`, so nothing carries over.
struct Session {
    epoch: u32,
    creds: TurnCreds,
    crypto: Arc<dyn RTCCryptoProvider>,
    refresh_cap: Option<Duration>,
    realm: String,
    nonce: String,
    key: Option<Vec<u8>>,
    alloc: Option<Allocation>,
    txs: HashMap<TransactionId, Tx>,
    perms: HashMap<IpAddr, Perm>,
    chans: HashMap<SocketAddr, Chan>,
    /// number -> peer, for the inbound ChannelData.
    chan_peer: HashMap<u16, SocketAddr>,
    next_channel: u16,
    /// When the last datagram went to the TURN server (the keepalive's clock).
    last_sent: Instant,
    out: Vec<Vec<u8>>,
}

impl Session {
    /// A session on socket `epoch`, its unsigned Allocate already queued.
    fn start(
        epoch: u32,
        creds: TurnCreds,
        crypto: Arc<dyn RTCCryptoProvider>,
        refresh_cap: Option<Duration>,
        now: Instant,
    ) -> Result<Self, String> {
        let mut s = Session {
            epoch,
            creds,
            crypto,
            refresh_cap,
            realm: String::new(),
            nonce: String::new(),
            key: None,
            alloc: None,
            txs: HashMap::new(),
            perms: HashMap::new(),
            chans: HashMap::new(),
            chan_peer: HashMap::new(),
            next_channel: MIN_CHANNEL_NUMBER,
            last_sent: now,
            out: Vec::new(),
        };
        // Unsigned first: the answer carries the realm and the nonce.
        s.request(TxKind::Allocate { signed: false }, now, 0)?;
        Ok(s)
    }

    fn allocated(&self) -> bool {
        self.alloc.is_some()
    }

    fn emit(&mut self, d: Vec<u8>, now: Instant) {
        self.out.push(d);
        self.last_sent = now;
    }

    fn build(&self, kind: &TxKind, lifetime: Duration) -> Result<Message, String> {
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
            TxKind::Refresh => {
                setters.push(Box::new(MessageType::new(METHOD_REFRESH, CLASS_REQUEST)));
                setters.push(Box::new(Lifetime(lifetime)));
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
        if signed {
            let Some(key) = self.key.as_ref() else {
                return Err("no credential to sign the request with".into());
            };
            setters.push(Box::new(TextAttribute::new(
                ATTR_USERNAME,
                self.creds.username.clone(),
            )));
            setters.push(Box::new(TextAttribute::new(ATTR_REALM, self.realm.clone())));
            setters.push(Box::new(TextAttribute::new(ATTR_NONCE, self.nonce.clone())));
            setters.push(Box::new(MessageIntegrity::new_raw_integrity_with_provider(
                key.clone(),
                self.crypto.crypto(),
            )));
        }
        setters.push(Box::new(FINGERPRINT));
        let mut m = Message::new();
        m.build(&setters)
            .map_err(|e| format!("cannot encode a TURN request: {e}"))?;
        Ok(m)
    }

    fn request(&mut self, kind: TxKind, now: Instant, nonce_retries: u8) -> Result<(), String> {
        let m = self.build(&kind, TURN_REQUEST_LIFETIME)?;
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
        Ok(())
    }

    /// The best-effort release (Refresh LIFETIME 0, nothing waits for its answer).
    fn release(&mut self, now: Instant) {
        if let Ok(m) = self.build(&TxKind::Refresh, Duration::ZERO) {
            self.emit(m.raw, now);
        }
    }

    fn refresh_interval(&self, lifetime: Duration) -> Duration {
        let half = lifetime / 2;
        self.refresh_cap
            .map_or(half, |c| half.min(c))
            .max(Duration::from_secs(1))
    }

    fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = None;
        let mut at = |x: Instant| t = Some(t.map_or(x, |y| y.min(x)));
        for tx in self.txs.values() {
            match TX_SENDS_MS.get(tx.sends) {
                Some(ms) => at(tx.started + Duration::from_millis(*ms)),
                None => at(tx.started + TURN_TX_TIMEOUT),
            }
        }
        if let Some(a) = &self.alloc {
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
        t
    }

    /// Retransmissions, timeouts, the refresh, the permissions, the channels, the keepalive. The
    /// first fatal event is returned (the caller drops the session then).
    fn handle_timeout(
        &mut self,
        now: Instant,
        wanted: &HashSet<IpAddr>,
        max_channels: usize,
    ) -> Option<Ev> {
        let mut resent = Vec::new();
        let mut timed_out = Vec::new();
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
                let what = match tx.kind {
                    TxKind::Allocate { .. } => {
                        return Some(Ev::AllocFailed(
                            "allocate: no answer from the TURN server".into(),
                        ))
                    }
                    TxKind::Refresh => "refresh",
                    TxKind::Permission(_) => "permission",
                    TxKind::ChannelBind(..) => "channel bind",
                    TxKind::Keepalive => "keepalive",
                };
                // The server no longer hears this 5-tuple (a dropped NAT mapping, a lost
                // allocation).
                return Some(Ev::Lost(format!("{what}: no answer from the TURN server")));
            }
        }
        let a = self.alloc.as_ref()?;
        if now >= a.expires_at {
            return Some(Ev::Lost("the allocation expired without a refresh".into()));
        }
        if !a.refreshing && now >= a.refresh_at {
            if let Some(a) = self.alloc.as_mut() {
                a.refreshing = true;
            }
            if let Err(e) = self.request(TxKind::Refresh, now, 0) {
                return Some(Ev::Lost(e));
            }
        }
        self.ask_permissions(wanted, now);
        self.refresh_channels(now);
        self.prune_channels(now, max_channels);
        let keepalive_out = self
            .txs
            .values()
            .any(|t| matches!(t.kind, TxKind::Keepalive));
        if !keepalive_out && now >= self.last_sent + TURN_KEEPALIVE {
            if let Err(e) = self.request(TxKind::Keepalive, now, 0) {
                return Some(Ev::Lost(e));
            }
        }
        None
    }

    fn set_wanted(&mut self, wanted: &HashSet<IpAddr>, now: Instant, max_channels: usize) {
        self.perms.retain(|ip, _| wanted.contains(ip));
        for (peer, c) in &mut self.chans {
            if !wanted.contains(&peer.ip()) {
                c.departed = true;
            }
        }
        self.prune_channels(now, max_channels);
        self.ask_permissions(wanted, now);
    }

    fn ask_permissions(&mut self, wanted: &HashSet<IpAddr>, now: Instant) {
        if !self.allocated() {
            return;
        }
        let due: Vec<IpAddr> = wanted
            .iter()
            .filter(|ip| match self.perms.get(ip) {
                None => true,
                Some(p) => !p.in_flight && now >= p.next_at,
            })
            .copied()
            .collect();
        for ip in due {
            let p = self.perms.entry(ip).or_insert(Perm {
                permitted: false,
                in_flight: false,
                next_at: now,
            });
            p.in_flight = true;
            let _ = self.request(TxKind::Permission(ip), now, 0);
        }
    }

    fn refresh_channels(&mut self, now: Instant) {
        let mut due = Vec::new();
        for (peer, c) in &mut self.chans {
            let used = now.saturating_duration_since(c.last_used) < TURN_CHANNEL_REFRESH;
            if c.ready
                && !c.departed
                && !c.in_flight
                && used
                && now >= c.bound_at + TURN_CHANNEL_REFRESH
            {
                c.in_flight = true;
                due.push((*peer, c.number));
            }
        }
        for (peer, n) in due {
            let _ = self.request(TxKind::ChannelBind(peer, n), now, 0);
        }
    }

    /// Forget the departed channels the server no longer holds (or never bound), and keep the
    /// table bounded: at most 4 x `max_channels` entries, the oldest departed go first.
    fn prune_channels(&mut self, now: Instant, max_channels: usize) {
        let before = self.chans.len();
        self.chans.retain(|_, c| {
            !c.departed
                || ((c.ready || c.in_flight)
                    && now.saturating_duration_since(c.bound_at) < TURN_CHANNEL_REMEMBER)
        });
        let cap = max_channels.saturating_mul(4).max(1);
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

    fn datagram(&mut self, data: &[u8], now: Instant) -> Option<Ev> {
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
            return Some(Ev::Data(peer, cd.data));
        }
        let mut m = Message::new();
        m.raw = data.to_vec();
        m.decode().ok()?;
        if m.typ.class == CLASS_INDICATION {
            if m.typ.method != METHOD_DATA || !self.allocated() {
                return None;
            }
            let mut peer = PeerAddress::default();
            let mut d = Data::default();
            peer.get_from(&m).ok()?;
            d.get_from(&m).ok()?;
            if !self.perms.contains_key(&peer.ip) {
                return None;
            }
            return Some(Ev::Data(SocketAddr::new(peer.ip, peer.port), d.0));
        }
        if m.typ.class != CLASS_SUCCESS_RESPONSE && m.typ.class != CLASS_ERROR_RESPONSE {
            return None;
        }
        let tx = self.txs.remove(&m.transaction_id)?;
        self.answered(tx, &m, now)
    }

    fn answered(&mut self, tx: Tx, m: &Message, now: Instant) -> Option<Ev> {
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
                return match self.request(tx.kind, now, tx.nonce_retries + 1) {
                    Ok(()) => None,
                    Err(e) => Some(Ev::Lost(e)),
                };
            }
        }
        match tx.kind {
            TxKind::Allocate { signed: false } if code == 401 => {
                let realm = TextAttribute::get_from_as(m, ATTR_REALM).map(|t| t.text);
                let nonce = TextAttribute::get_from_as(m, ATTR_NONCE).map(|t| t.text);
                let (Ok(realm), Ok(nonce)) = (realm, nonce) else {
                    return Some(Ev::AllocFailed(
                        "allocate: 401 without REALM / NONCE".into(),
                    ));
                };
                match MessageIntegrity::long_term_integrity_key(
                    self.creds.username.clone(),
                    realm.clone(),
                    self.creds.password.clone(),
                    self.crypto.crypto(),
                ) {
                    Ok(key) => {
                        self.realm = realm;
                        self.nonce = nonce;
                        self.key = Some(key);
                        match self.request(TxKind::Allocate { signed: true }, now, 0) {
                            Ok(()) => None,
                            Err(e) => Some(Ev::AllocFailed(e)),
                        }
                    }
                    Err(e) => Some(Ev::AllocFailed(format!("allocate: no key: {e}"))),
                }
            }
            TxKind::Allocate { .. } => {
                if !ok {
                    return Some(Ev::AllocFailed(format!("allocate refused: {reason}")));
                }
                let mut relayed = RelayedAddress::default();
                if relayed.get_from(m).is_err() {
                    return Some(Ev::AllocFailed(
                        "allocate: the answer has no XOR-RELAYED-ADDRESS".into(),
                    ));
                }
                let mut l = Lifetime::default();
                let lifetime = if l.get_from(m).is_ok() && !l.0.is_zero() {
                    l.0
                } else {
                    TURN_REQUEST_LIFETIME
                };
                self.alloc = Some(Allocation {
                    relayed: SocketAddr::new(relayed.ip, relayed.port),
                    expires_at: now + lifetime,
                    refresh_at: now + self.refresh_interval(lifetime),
                    refreshing: false,
                });
                Some(Ev::Allocated)
            }
            TxKind::Refresh => {
                if !ok {
                    return Some(Ev::Lost(format!("refresh refused: {reason}")));
                }
                let mut l = Lifetime::default();
                let granted = if l.get_from(m).is_ok() {
                    l.0
                } else {
                    TURN_REQUEST_LIFETIME
                };
                if granted.is_zero() {
                    return Some(Ev::Lost("refresh: the server ended the allocation".into()));
                }
                let refresh_at = now + self.refresh_interval(granted);
                if let Some(a) = self.alloc.as_mut() {
                    a.expires_at = now + granted;
                    a.refresh_at = refresh_at;
                    a.refreshing = false;
                }
                Some(Ev::Refreshed)
            }
            TxKind::Permission(ip) if code == 437 => {
                Some(Ev::Lost(format!("permission for {ip}: {reason}")))
            }
            TxKind::ChannelBind(peer, _) if code == 437 => {
                Some(Ev::Lost(format!("channel bind for {peer}: {reason}")))
            }
            TxKind::Keepalive => None,
            TxKind::Permission(ip) => {
                let p = self.perms.get_mut(&ip)?; // gone = no longer wanted
                p.in_flight = false;
                if ok {
                    if !p.permitted {
                        tracing::debug!(peer = %ip, "webrtc turn: permission granted");
                    }
                    p.permitted = true;
                    p.next_at = now + TURN_PERMISSION_REFRESH;
                    None
                } else {
                    p.permitted = false;
                    p.next_at = now + PERMISSION_RETRY;
                    tracing::debug!(peer = %ip, %reason, "webrtc turn: permission refused");
                    Some(Ev::Note(format!("permission for {ip} refused: {reason}")))
                }
            }
            TxKind::ChannelBind(peer, _) => {
                let c = self.chans.get_mut(&peer)?;
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
                None
            }
        }
    }

    fn send_to(
        &mut self,
        peer: SocketAddr,
        payload: &[u8],
        now: Instant,
        max_channels: usize,
    ) -> Result<(), &'static str> {
        if !self.allocated() {
            return Err("no TURN allocation");
        }
        if !self.perms.get(&peer.ip()).is_some_and(|p| p.permitted) {
            return Err("no TURN permission for the peer");
        }
        let live = self.chans.values().filter(|c| !c.departed).count();
        let mut bind = None;
        if let Some(c) = self.chans.get_mut(&peer) {
            // A returning peer takes its remembered channel back.
            c.departed = false;
            c.last_used = now;
            if c.ready && now.saturating_duration_since(c.bound_at) < TURN_CHANNEL_LIFETIME {
                let mut cd = ChannelData {
                    data: payload.to_vec(),
                    number: ChannelNumber(c.number),
                    ..Default::default()
                };
                cd.encode();
                self.emit(cd.raw, now);
                return Ok(());
            }
            // Past the server's binding: bind the same number again before any ChannelData.
            c.ready = false;
            if !c.in_flight && c.failed_until.is_none_or(|u| now >= u) {
                c.in_flight = true;
                c.failed_until = None;
                bind = Some(c.number);
            }
        } else if live < max_channels {
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
            let _ = self.request(TxKind::ChannelBind(peer, number), now, 0);
        }
        let mut m = Message::new();
        m.build(&[
            Box::new(TransactionId::new()),
            Box::new(MessageType::new(METHOD_SEND, CLASS_INDICATION)),
            Box::new(PeerAddress {
                ip: peer.ip(),
                port: peer.port(),
            }),
            Box::new(Data(payload.to_vec())),
        ])
        .map_err(|_| "cannot encode a Send indication")?;
        self.emit(m.raw, now);
        Ok(())
    }
}

/// The hub's TURN client (see the module doc).
pub struct TurnRelay {
    crypto: Arc<dyn RTCCryptoProvider>,
    opts: TurnOptions,
    /// The newest credential.
    creds: Option<TurnCreds>,
    unavailable: bool,
    /// The allocation in use (or the one being made when none is up).
    live: Option<Session>,
    /// A rotation's new allocation, being made while `live` carries the phones.
    next: Option<Session>,
    /// Old allocations' sockets kept open until then (their release).
    retiring: Vec<(u32, Instant)>,
    next_epoch: u32,
    wanted: HashSet<IpAddr>,
    backoff: Duration,
    retry_at: Option<Instant>,
    rotate_after: Option<Instant>,
    last_loss: Option<Instant>,
    /// The last phase logged.
    shown: TurnPhase,
    out: VecDeque<TurnTransmit>,
    allocations: u64,
    refreshes: u64,
    failures: u64,
    permissions_refused: u64,
    rx_packets: u64,
    tx_packets: u64,
    tx_refused: u64,
    last_error: Option<String>,
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
            unavailable: false,
            live: None,
            next: None,
            retiring: Vec::new(),
            next_epoch: 0,
            wanted: HashSet::new(),
            backoff: TURN_BACKOFF_MIN,
            retry_at: None,
            rotate_after: None,
            last_loss: None,
            shown: TurnPhase::Waiting,
            out: VecDeque::new(),
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
        match &self.live {
            Some(s) if s.allocated() => TurnPhase::Allocated,
            Some(_) => TurnPhase::Allocating,
            None if self.retry_at.is_some() => TurnPhase::Backoff,
            None if self.unavailable => TurnPhase::Off,
            None => TurnPhase::Waiting,
        }
    }

    /// The relayed address while allocated.
    pub fn relayed(&self) -> Option<SocketAddr> {
        self.live.as_ref()?.alloc.as_ref().map(|a| a.relayed)
    }

    /// The TURN server this relay talks to.
    pub fn server(&self) -> Option<SocketAddr> {
        self.live
            .as_ref()
            .map(|s| s.creds.server)
            .or_else(|| self.creds.as_ref().map(|c| c.server))
    }

    /// The socket epoch of the allocation in use (or being made).
    pub fn live_epoch(&self) -> Option<u32> {
        self.live.as_ref().map(|s| s.epoch)
    }

    /// Every socket epoch still in use (the live and a rotation's allocation, the retiring
    /// ones), ascending: the network task closes every other.
    pub fn live_epochs(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self
            .live
            .iter()
            .chain(self.next.iter())
            .map(|s| s.epoch)
            .chain(self.retiring.iter().map(|(e, _)| *e))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn sync_phase(&mut self) {
        let phase = self.phase();
        if phase == self.shown {
            return;
        }
        let from = self.shown;
        self.shown = phase;
        let server = self.server().map(|s| s.to_string()).unwrap_or_default();
        match phase {
            TurnPhase::Allocated => {
                let relayed = self.relayed().map(|r| r.to_string()).unwrap_or_default();
                tracing::info!(from = from.name(), %server, %relayed, "webrtc turn: relay allocated");
            }
            TurnPhase::Backoff => {
                tracing::warn!(
                    from = from.name(),
                    %server,
                    error = self.last_error.as_deref().unwrap_or(""),
                    retry_in_s = self.backoff.as_secs_f64() / 2.0,
                    "webrtc turn: relay failed, backing off (counted on /api/state webrtc.turn.failures)"
                );
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

    /// Move every session's queued datagrams to the relay's queue, tagged with their socket.
    fn drain(&mut self) {
        for s in self.live.iter_mut().chain(self.next.iter_mut()) {
            for data in s.out.drain(..) {
                self.out.push_back(TurnTransmit {
                    epoch: s.epoch,
                    to: s.creds.server,
                    data,
                });
            }
        }
    }

    fn new_session(&mut self, creds: TurnCreds, now: Instant) -> Result<Session, String> {
        let epoch = self.next_epoch;
        self.next_epoch = self.next_epoch.wrapping_add(1);
        Session::start(
            epoch,
            creds,
            self.crypto.clone(),
            self.opts.refresh_cap,
            now,
        )
    }

    /// A new live allocation on a new socket with the newest credential.
    fn start_live(&mut self, now: Instant) {
        self.retry_at = None;
        let Some(creds) = self.creds.clone() else {
            self.live = None;
            return;
        };
        match self.new_session(creds, now) {
            Ok(s) => self.live = Some(s),
            Err(e) => self.fail(e, now),
        }
    }

    /// The live Allocate failed: back off.
    fn fail(&mut self, reason: String, now: Instant) {
        self.failures += 1;
        self.last_error = Some(reason);
        self.live = None;
        self.retry_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(TURN_BACKOFF_MAX);
    }

    /// The live allocation is gone: abandoned (it expires on the server), a new one at once on a
    /// new socket (a rotation's new allocation, when one is being made, takes over), unless one
    /// was lost within [`TURN_LOSS_AT_ONCE_WINDOW`] (then the back-off).
    fn lost(&mut self, reason: String, now: Instant) {
        let recent = self
            .last_loss
            .is_some_and(|l| now.saturating_duration_since(l) < TURN_LOSS_AT_ONCE_WINDOW);
        self.last_loss = Some(now);
        self.failures += 1;
        tracing::warn!(error = %reason, "webrtc turn: the allocation is lost, allocating again from a new socket (counted on /api/state webrtc.turn.failures)");
        self.last_error = Some(reason);
        self.live = self.next.take();
        if self.live.is_some() {
            return;
        }
        if recent {
            self.retry_at = Some(now + self.backoff);
            self.backoff = (self.backoff * 2).min(TURN_BACKOFF_MAX);
        } else {
            self.start_live(now);
        }
    }

    fn live_event(&mut self, ev: Ev, now: Instant) {
        match ev {
            Ev::Data(..) => {}
            Ev::Allocated => {
                self.allocations += 1;
                self.backoff = TURN_BACKOFF_MIN;
                self.retry_at = None;
                let max = self.opts.max_channels;
                if let Some(s) = self.live.as_mut() {
                    s.set_wanted(&self.wanted, now, max);
                }
            }
            Ev::Refreshed => self.refreshes += 1,
            Ev::AllocFailed(r) => self.fail(r, now),
            Ev::Lost(r) => self.lost(r, now),
            Ev::Note(r) => self.last_error = Some(r),
        }
    }

    /// A rotation's new allocation reports.
    fn next_event(&mut self, ev: Ev, now: Instant) {
        match ev {
            Ev::Data(..) | Ev::Refreshed => {}
            Ev::Allocated => {
                // The switch: the new allocation carries the answers from now on; the old one is
                // released on its own socket, which closes after a short grace.
                self.allocations += 1;
                let Some(mut new) = self.next.take() else {
                    return;
                };
                new.set_wanted(&self.wanted, now, self.opts.max_channels);
                if let Some(mut old) = self.live.replace(new) {
                    old.release(now);
                    for data in old.out.drain(..) {
                        self.out.push_back(TurnTransmit {
                            epoch: old.epoch,
                            to: old.creds.server,
                            data,
                        });
                    }
                    self.retiring.push((old.epoch, now + TURN_RETIRE_GRACE));
                }
                let relayed = self.relayed().map(|r| r.to_string()).unwrap_or_default();
                tracing::info!(%relayed, "webrtc turn: credential rotated, the new allocation carries the answers (phones on the old relay reconnect)");
            }
            Ev::AllocFailed(r) | Ev::Lost(r) => {
                self.failures += 1;
                tracing::warn!(error = %r, retry_in_s = TURN_ROTATE_RETRY.as_secs(), "webrtc turn: the rotation's new allocation failed, the old one carries on");
                self.last_error = Some(r);
                self.next = None;
                self.rotate_after = Some(now + TURN_ROTATE_RETRY);
            }
            Ev::Note(r) => self.last_error = Some(r),
        }
    }

    /// No credential source: the reason shows on the facet; an allocation that exists keeps
    /// running until it fails (a mint error does not end a live credential).
    pub fn set_unavailable(&mut self, reason: &str) {
        self.last_error = Some(reason.to_string());
        if self.creds.is_none() {
            self.unavailable = true;
        }
        self.sync_phase();
    }

    /// The minter's current credential. The same username and server again only moves the expiry;
    /// another one is a rotation (see the module doc).
    pub fn set_credentials(&mut self, creds: TurnCreds, now: Instant) {
        let same = |c: &TurnCreds| c.username == creds.username && c.server == creds.server;
        if self.creds.as_ref().is_some_and(same) {
            for s in self.live.iter_mut().chain(self.next.iter_mut()) {
                if same(&s.creds) {
                    s.creds.expires_at = creds.expires_at;
                }
            }
            self.creds = Some(creds);
            return;
        }
        self.creds = Some(creds);
        self.unavailable = false;
        self.rotate_after = None;
        self.next = None;
        match &self.live {
            Some(s) if s.allocated() => {
                tracing::info!("webrtc turn: a new credential, the rotation allocates with it next to the live relay");
            }
            _ => {
                // Nothing up to keep: start over with the new credential at once.
                self.backoff = TURN_BACKOFF_MIN;
                self.start_live(now);
            }
        }
        self.drain();
        self.sync_phase();
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
        self.wanted = wanted;
        let max = self.opts.max_channels;
        if let Some(s) = self.live.as_mut() {
            s.set_wanted(&self.wanted, now, max);
        }
        self.drain();
    }

    /// The datagrams for the TURN server, each with the socket (epoch) it leaves from.
    pub fn poll_transmit(&mut self) -> Option<TurnTransmit> {
        self.out.pop_front()
    }

    /// The earliest time [`TurnRelay::handle_timeout`] has work.
    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t: Option<Instant> = None;
        let mut at = |x: Instant| t = Some(t.map_or(x, |y| y.min(x)));
        for s in self.live.iter().chain(self.next.iter()) {
            if let Some(x) = s.poll_timeout() {
                at(x);
            }
        }
        if let Some(r) = self.retry_at {
            at(r);
        }
        for (_, until) in &self.retiring {
            at(*until);
        }
        if let (Some(live), Some(c), None) = (&self.live, &self.creds, &self.next) {
            if live.allocated() && live.creds.username != c.username {
                at(self.rotate_after.unwrap_or_else(|| {
                    live.creds
                        .expires_at
                        .checked_sub(TURN_ROTATE_MARGIN)
                        .unwrap_or(live.creds.expires_at)
                }));
            }
        }
        t
    }

    /// Time moves on: retransmissions and timeouts, the back-off, the refresh, a rotation, the
    /// permissions, the keepalive. `busy` = a phone is on the relay right now (a rotation then
    /// waits, never past the old credential's expiry margin).
    pub fn handle_timeout(&mut self, now: Instant, busy: bool) {
        self.retiring.retain(|(_, until)| *until > now);
        if self.live.is_none() && self.retry_at.is_some_and(|r| now >= r) {
            self.start_live(now);
        }
        let max = self.opts.max_channels;
        let ev = match self.live.as_mut() {
            Some(s) => s.handle_timeout(now, &self.wanted, max),
            None => None,
        };
        if let Some(ev) = ev {
            self.live_event(ev, now);
        }
        let ev = match self.next.as_mut() {
            Some(s) => s.handle_timeout(now, &self.wanted, max),
            None => None,
        };
        if let Some(ev) = ev {
            self.next_event(ev, now);
        }
        self.rotate_if_due(now, busy);
        self.drain();
        self.sync_phase();
    }

    fn rotate_if_due(&mut self, now: Instant, busy: bool) {
        let (Some(live), Some(creds)) = (&self.live, &self.creds) else {
            return;
        };
        let stale = live.creds.username != creds.username || live.creds.server != creds.server;
        if !live.allocated() || !stale || self.next.is_some() {
            return;
        }
        if self.rotate_after.is_some_and(|r| now < r) {
            return;
        }
        let near_expiry =
            live.creds.expires_at.saturating_duration_since(now) <= TURN_ROTATE_MARGIN;
        if busy && !near_expiry {
            return;
        }
        let creds = creds.clone();
        match self.new_session(creds, now) {
            Ok(s) => {
                tracing::info!(
                    "webrtc turn: credential rotated, allocating with it on a new socket"
                );
                self.next = Some(s);
            }
            Err(e) => self.next_event(Ev::AllocFailed(e), now),
        }
    }

    /// One datagram from `from` on the socket of allocation `epoch`. Only the TURN server's address
    /// on a live session's socket counts. Data indications and ChannelData come back as
    /// `(peer, payload)` (feed it to str0m from `peer` to the relayed address); responses move the
    /// state machine and return `None`, as does anything malformed or unexpected.
    pub fn handle_datagram(
        &mut self,
        epoch: u32,
        from: SocketAddr,
        data: &[u8],
        now: Instant,
    ) -> Option<(SocketAddr, Vec<u8>)> {
        let is = |s: &Option<Session>| {
            s.as_ref()
                .is_some_and(|s| s.epoch == epoch && s.creds.server == from)
        };
        let mut got = None;
        if is(&self.live) {
            let ev = self.live.as_mut().and_then(|s| s.datagram(data, now));
            match ev {
                Some(Ev::Data(peer, payload)) => {
                    self.rx_packets += 1;
                    got = Some((peer, payload));
                }
                Some(ev) => self.live_event(ev, now),
                None => {}
            }
        } else if is(&self.next) {
            if let Some(ev) = self.next.as_mut().and_then(|s| s.datagram(data, now)) {
                self.next_event(ev, now);
            }
        }
        self.drain();
        self.sync_phase();
        got
    }

    /// Wrap `payload` for `peer` (a Send indication, or ChannelData once bound) on the live
    /// allocation. Refused without an allocation or a permission for the peer's IP: the hub never
    /// relays to an address the phone's offer did not name.
    pub fn send_to(
        &mut self,
        peer: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) -> Result<(), &'static str> {
        let max = self.opts.max_channels;
        let r = match self.live.as_mut() {
            Some(s) => s.send_to(peer, payload, now, max),
            None => Err("no TURN allocation"),
        };
        match r {
            Ok(()) => self.tx_packets += 1,
            Err(_) => self.tx_refused += 1,
        }
        self.drain();
        r
    }

    /// A transmit str0m wanted to send from a relayed address that is not (or no longer) the
    /// live one: dropped, counted here (`tx_refused`).
    pub fn refuse(&mut self) {
        self.tx_refused += 1;
    }

    /// The `/api/state` facet (`local` is the network task's to fill).
    pub fn facet(&self, now: Instant) -> TurnFacet {
        let live = self.live.as_ref();
        let alloc = live.and_then(|s| s.alloc.as_ref());
        TurnFacet {
            state: self.phase().name(),
            server: self.server().map(|s| s.to_string()),
            local: None,
            relayed: alloc.map(|a| a.relayed.to_string()),
            lifetime_left_s: alloc.map(|a| a.expires_at.saturating_duration_since(now).as_secs()),
            rotating: self.next.is_some(),
            allocations: self.allocations,
            refreshes: self.refreshes,
            failures: self.failures,
            last_error: self.last_error.clone(),
            permissions: live.map_or(0, |s| s.perms.values().filter(|p| p.permitted).count()),
            permissions_refused: self.permissions_refused,
            channels: live.map_or(0, |s| {
                s.chans
                    .values()
                    .filter(|c| !c.departed && c.failed_until.is_none())
                    .count()
            }),
            rx_packets: self.rx_packets,
            tx_packets: self.tx_packets,
            tx_refused: self.tx_refused,
        }
    }
}
