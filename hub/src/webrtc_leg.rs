//! The in-hub WebRTC phone leg's network task (intercom#42, owner decision B 20.11.2026; design
//! comment 7041415746). The hub itself is the phones' WebRTC endpoint: no Janus, no audiobridge.
//!
//! One tokio task owns ONE UDP socket and every phone's [`str0m::Rtc`] (sans-IO ICE + DTLS +
//! SRTP). It:
//!
//! - answers the HTTP signalling ([`WebrtcHandle`]: offer -> answer with every host candidate,
//!   the display name, leave);
//! - demultiplexes each datagram to the phone whose ICE credentials or nominated address accept it
//!   (`Rtc::accepts`), after the Dante guard (intercom#25) dropped what arrived on the Dante leg;
//! - decodes each phone's Opus (`janus_codec::RxDecoder`: FEC for the frame right before a packet,
//!   PLC for earlier ones) into that phone's mic ring ([`RoomSlots`]);
//! - writes the packets the phones-room thread encoded ([`crate::webrtc_room`]) to their phone;
//! - frees a phone's slot when its ICE disconnects, it never connects, its `Rtc` dies, or it
//!   leaves; and publishes the `/api/state` `webrtc` facet once a second.
//!
//! Phase 3: with `[webrtc].turn = "cloudflare"` the task also owns the hub's own TURN allocation
//! ([`crate::webrtc_turn::TurnRelay`], credentials from the box's [`TurnMinter`]), each allocation
//! on its OWN ephemeral UDP socket (0.0.0.0:0, one per relay epoch, `TurnSock`): a new allocation
//! is a new 5-tuple, so it never meets an allocation the server still holds (437) after a restart,
//! a loss or a rotation. Datagrams from the TURN server are unwrapped and fed to
//! str0m from the peer to the relayed address; str0m's transmits from the relayed address are
//! wrapped and sent to the TURN server from that socket. Every answer then carries the relayed candidate next to the host ones, and each offer's
//! plausible addresses get a TURN permission while its phone lives. ICE-lite is unchanged: the hub
//! only answers checks. Logging: one line per session start / end, ICE state change, TURN state
//! change and error, never one per packet.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::config::CryptoProvider;
use str0m::format::Codec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::sync::{mpsc, oneshot};

use crate::dante_guard::DanteGuard;
use crate::janus_codec::RxDecoder;
use crate::janus_pacing::{IntervalStats, FRAME_48K};
use crate::janus_rtp::JanusCodec;
use crate::turn::{refresh_wait_s, TurnMinter, TURN_REFRESH_RETRY_S, TURN_TTL_S};
use crate::vban_jitter::NetworkFillStats;
use crate::webrtc_room::{RoomSlots, TxPacket};
use crate::webrtc_signal::{
    filter_offer_candidates, offer_candidate_ips, pending_cap, OfferRequest, PhoneId, RefusalLog,
    SignalError, SlotTable,
};
use crate::webrtc_turn::{
    advertise_relayed, turn_udp_server, TurnCreds, TurnFacet, TurnOptions, TurnRelay,
};

/// An offer's slot is held this long for the phone's first valid STUN binding request (the
/// phone checks, the ICE-lite hub answers); no check, no slot (intercom#42 review: anonymous offers
/// must not lock real phones out).
pub const STUN_DEADLINE: Duration = Duration::from_secs(3);
/// After its first STUN check a phone has this long to finish ICE + DTLS.
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// A reserved slot whose answer never came back (the HTTP request went away) is freed after this.
pub const RESERVATION_DEADLINE: Duration = Duration::from_secs(5);
/// A phone that was connected may reconnect with its old id (`replaces`) for this long.
pub const RECENT_WINDOW: Duration = Duration::from_secs(120);
/// At most this many recently connected ids are remembered.
pub const RECENT_MAX: usize = 256;
/// How many PENDING (not yet connected) fresh offers one client key may hold: half the hub-wide
/// pending cap, at least 3; 429 beyond (review rounds 2-4). Connected phones never count: a whole
/// venue's Wi-Fi reaches the hub through cloudflared under ONE key. The hub-wide pending cap is the
/// real flood guard; this one keeps one key from taking all of it on a hub of 8 or more slots. On a
/// smaller hub (pending cap ≤ 3) the share is the whole hub-wide cap: harmless, a venue is one key.
pub fn pending_cap_per_client(max_phones: usize) -> usize {
    (pending_cap(max_phones) / 2).max(3)
}
/// The end reason of a phone a reconnect replaced: its id is not a reconnect claim again.
const REPLACED: &str = "replaced by its reconnect";
/// How long the pending-commands queue is.
pub const COMMAND_QUEUE: usize = 64;
/// A mic counts as active while a packet with sound came within this long.
pub const MIC_ACTIVE_WINDOW: Duration = Duration::from_secs(1);
/// The peak a decoded mic packet must reach to count as sound (about -60 dBFS): a phone whose
/// track is disabled sends digital silence.
pub const MIC_ACTIVE_PEAK: i16 = 33;
/// The largest datagram read.
const RECV_BUF: usize = 2048;

/// The shared counters behind the `/api/state` `webrtc` facet. The room thread and the network
/// task write atomics; the per-phone list is replaced once a second by the network task. Read only
/// when a snapshot is serialized (off the hub-mix thread, which only clones the `Arc`).
#[derive(Debug, Default)]
pub struct WebrtcShared {
    pub udp_bind: String,
    pub host_candidates: Vec<String>,
    pub max_phones: usize,
    pub sessions_started: AtomicU64,
    pub sessions_ended: AtomicU64,
    pub offers_refused: AtomicU64,
    pub offers_rate_limited: AtomicU64,
    pub phones_pending: AtomicU64,
    pub sessions_never_connected: AtomicU64,
    pub send_errors: AtomicU64,
    pub rx_refused: AtomicU64,
    pub room_ticks: AtomicU64,
    pub room_packets: AtomicU64,
    pub room_dropped: AtomicU64,
    pub room_encode_errors: AtomicU64,
    bus_underflows: AtomicU64,
    bus_trims: AtomicU64,
    bus_rate_ppm: AtomicU64,
    bus_asrc_phase_frames: AtomicU64,
    tx_interval_sd_us: AtomicU64,
    tx_interval_max_us: AtomicU64,
    phones: Mutex<Vec<PhoneFacet>>,
    turn: Mutex<Option<TurnFacet>>,
}

/// One phone on `/api/state`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PhoneFacet {
    /// The first 8 hex characters of the phone's id (the full id is its credential).
    pub id: String,
    pub display: String,
    pub slot: usize,
    /// `new` / `checking` / `connected` / `completed` / `disconnected`.
    pub ice_state: &'static str,
    /// ICE and DTLS are up (media can flow).
    pub connected: bool,
    pub age_s: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    /// Received frames lost on the wire and concealed (FEC / PLC).
    pub rx_lost_frames: u64,
    /// A packet with sound arrived within the last second.
    pub mic_active: bool,
    /// The phone's mic ring: its resampler's ratio in ppm off 1 (the phone's clock against the hub's).
    pub mic_rate_ppm: f64,
    /// The hub's side of the pair its last datagram took: `host` (a host candidate) or `relay`
    /// (the hub's TURN relay, phase 3); `null` before the first.
    pub pair: Option<&'static str>,
}

/// The `/api/state` `webrtc` facet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WebrtcFacet {
    pub udp_bind: String,
    pub host_candidates: Vec<String>,
    pub max_phones: usize,
    pub phones_connected: usize,
    pub sessions_started: u64,
    pub sessions_ended: u64,
    /// Offers refused (any reason: rate limit, full, too large, not JSON, not an offer); a
    /// warning names the count at most every 10 s.
    pub offers_refused: u64,
    /// Of those, refused by the per-client rate limit (429).
    pub offers_rate_limited: u64,
    /// Slots held by offers whose phone has not connected yet (at most half of `max_phones`).
    pub phones_pending: u64,
    /// Accepted offers that ended without their phone ever connecting (logged as a summary).
    pub sessions_never_connected: u64,
    /// Sends the kernel refused (not a full buffer): a warning per change of state, not per packet.
    pub send_errors: u64,
    /// Datagrams no phone accepted (stray, late, or for a phone that left).
    pub rx_refused: u64,
    /// Room ticks (one per 20 ms), the packets they handed to the network task, and the ones the
    /// full queue dropped.
    pub room_ticks: u64,
    pub room_packets: u64,
    pub room_dropped: u64,
    pub room_encode_errors: u64,
    /// The phones bus ring the hub-mix thread feeds (the Janus sender's ring, same numbers).
    pub bus_underflows: u64,
    pub bus_overflow_trims: u64,
    pub bus_rate_ppm: f64,
    pub bus_asrc_phase_frames: f64,
    /// The room tick spacing over the last 5 s (paced: sd well under 0.5 ms).
    pub tx_interval_ms_sd: f64,
    pub tx_interval_ms_max: f64,
    pub phones: Vec<PhoneFacet>,
    /// The hub-side TURN relay (phase 3); omitted with `[webrtc].turn` off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnFacet>,
}

fn ms_to_us(ms: f64) -> u64 {
    (ms * 1000.0).round().max(0.0) as u64
}

impl WebrtcShared {
    /// Counters for a leg bound to `udp_bind`, advertising `host_candidates`.
    pub fn new(udp_bind: String, host_candidates: Vec<String>, max_phones: usize) -> Self {
        WebrtcShared {
            udp_bind,
            host_candidates,
            max_phones,
            ..Default::default()
        }
    }

    /// The facet as `/api/state` shows it.
    pub fn snapshot(&self) -> WebrtcFacet {
        let phones = self
            .phones
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let turn = self.turn.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let ld = |a: &AtomicU64| a.load(Ordering::Relaxed);
        WebrtcFacet {
            udp_bind: self.udp_bind.clone(),
            host_candidates: self.host_candidates.clone(),
            max_phones: self.max_phones,
            phones_connected: phones.iter().filter(|p| p.connected).count(),
            sessions_started: ld(&self.sessions_started),
            sessions_ended: ld(&self.sessions_ended),
            offers_refused: ld(&self.offers_refused),
            offers_rate_limited: ld(&self.offers_rate_limited),
            phones_pending: ld(&self.phones_pending),
            sessions_never_connected: ld(&self.sessions_never_connected),
            send_errors: ld(&self.send_errors),
            rx_refused: ld(&self.rx_refused),
            room_ticks: ld(&self.room_ticks),
            room_packets: ld(&self.room_packets),
            room_dropped: ld(&self.room_dropped),
            room_encode_errors: ld(&self.room_encode_errors),
            bus_underflows: ld(&self.bus_underflows),
            bus_overflow_trims: ld(&self.bus_trims),
            bus_rate_ppm: f64::from_bits(ld(&self.bus_rate_ppm)),
            bus_asrc_phase_frames: f64::from_bits(ld(&self.bus_asrc_phase_frames)),
            tx_interval_ms_sd: ld(&self.tx_interval_sd_us) as f64 / 1000.0,
            tx_interval_ms_max: ld(&self.tx_interval_max_us) as f64 / 1000.0,
            phones,
            turn,
        }
    }

    /// The phones bus ring's numbers (the room thread, every tick).
    pub fn publish_bus(&self, underflows: u64, trims: u64, ring: &NetworkFillStats) {
        self.bus_underflows.store(underflows, Ordering::Relaxed);
        self.bus_trims.store(trims, Ordering::Relaxed);
        self.bus_rate_ppm
            .store(ring.rate_ppm.to_bits(), Ordering::Relaxed);
        self.bus_asrc_phase_frames
            .store(ring.asrc_phase_frames.to_bits(), Ordering::Relaxed);
    }

    /// The room tick spacing (the room thread, every tick).
    pub fn publish_intervals(&self, intervals: &IntervalStats) {
        self.tx_interval_sd_us
            .store(ms_to_us(intervals.sd_ms()), Ordering::Relaxed);
        self.tx_interval_max_us
            .store(ms_to_us(intervals.max_ms()), Ordering::Relaxed);
    }

    fn publish_phones(&self, phones: Vec<PhoneFacet>) {
        *self.phones.lock().unwrap_or_else(|e| e.into_inner()) = phones;
    }

    fn publish_turn(&self, turn: Option<TurnFacet>) {
        *self.turn.lock().unwrap_or_else(|e| e.into_inner()) = turn;
    }
}

/// The `webrtc` facet on [`crate::state::HubState`]: the shared counters, serialized when the
/// snapshot is (the hub-mix thread only clones the `Arc`, issue 1401).
#[derive(Debug, Clone)]
pub struct WebrtcStateRef(pub Arc<WebrtcShared>);

impl Serialize for WebrtcStateRef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.snapshot().serialize(s)
    }
}

/// An answered offer.
#[derive(Debug, Clone)]
pub struct Answered {
    pub id: PhoneId,
    pub sdp: String,
}

/// A slot the network task set aside for an offer while its `Rtc` is built off the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    pub slot: usize,
    pub generation: u64,
}

/// The hub's relayed address while its TURN allocation lives.
type RelayCandidate = Arc<Mutex<Option<SocketAddr>>>;

/// What builds a phone's `Rtc` and its answer, OFF the media task (the SDP parse and the DTLS
/// certificate are the slow part): the crypto provider, the hub's host candidates and, while the
/// TURN allocation lives, its relayed candidate (the network task keeps it current).
#[derive(Debug, Clone)]
pub struct RtcFactory {
    crypto: Arc<CryptoProvider>,
    candidates: Vec<SocketAddr>,
    relay: RelayCandidate,
}

impl RtcFactory {
    /// A factory for `candidates` with the build's crypto provider.
    pub fn new(candidates: Vec<SocketAddr>) -> Self {
        RtcFactory {
            crypto: Arc::new(str0m::crypto::from_feature_flags()),
            candidates,
            relay: Arc::new(Mutex::new(None)),
        }
    }

    /// The hub's host candidates.
    pub fn candidates(&self) -> &[SocketAddr] {
        &self.candidates
    }

    /// The hub's relayed address, if its TURN allocation lives.
    pub fn relay_candidate(&self) -> Option<SocketAddr> {
        *self.relay.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// An ICE-lite, Opus-only `Rtc` answering `sdp` (the offer's implausible candidates dropped
    /// first): the hub never sends a connectivity check, it only answers the phone's. The answer
    /// carries every host candidate and the relayed one while the TURN allocation lives. Also
    /// returns the IPs of the offer's kept candidates (the TURN permissions this phone needs).
    pub fn answer(&self, sdp: &str) -> Result<(Rtc, String, Vec<IpAddr>), SignalError> {
        let relay = self.relay_candidate();
        // Only the HOST addresses count as the hub's own: a phone's relayed candidate often has
        // the hub relay's IP (the same TURN service; Cloudflare relays from shared anycast
        // addresses), and dropping it would leave that phone without a permission.
        let (sdp, _dropped) = filter_offer_candidates(sdp, &self.candidates);
        let peers = offer_candidate_ips(&sdp);
        let offer =
            SdpOffer::from_sdp_string(&sdp).map_err(|e| SignalError::BadOffer(e.to_string()))?;
        let mut rtc = RtcConfig::new()
            .set_crypto_provider(self.crypto.clone())
            .set_ice_lite(true)
            .clear_codecs()
            .enable_opus(true, false)
            .build(Instant::now());
        for addr in &self.candidates {
            match Candidate::host(*addr, "udp") {
                Ok(c) => {
                    rtc.add_local_candidate(c);
                }
                Err(e) => tracing::warn!(%addr, error = %e, "webrtc: host candidate refused"),
            }
        }
        // The relayed address joins as a HOST candidate: str0m's ICE-lite agent refuses any other
        // kind (`is` 0.12), and a lite agent only needs to know the address a check arrives on
        // and answer from it. The answer's line is then typed `relay` (`advertise_relayed`).
        let mut relayed_added = None;
        if let Some(relayed) = relay {
            match Candidate::host(relayed, "udp") {
                Ok(c) => {
                    if rtc.add_local_candidate(c).is_some() {
                        relayed_added = Some(relayed);
                    }
                }
                Err(e) => tracing::warn!(%relayed, error = %e, "webrtc: relayed candidate refused"),
            }
        }
        let answer: SdpAnswer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| SignalError::BadOffer(e.to_string()))?;
        let mut sdp = answer.to_sdp_string();
        if let Some(relayed) = relayed_added {
            sdp = advertise_relayed(&sdp, relayed);
        }
        Ok((rtc, sdp, peers))
    }
}

enum Command {
    Reserve {
        replaces: Option<PhoneId>,
        client: String,
        /// The client's rate bucket has a token (only a fresh offer needs one).
        has_token: bool,
        /// The reservation and whether it is a reconnect (a valid claim: no token spent).
        reply: oneshot::Sender<Result<(Reservation, bool), SignalError>>,
    },
    Adopt {
        reservation: Reservation,
        rtc: Box<Rtc>,
        display: String,
        /// The offer's plausible addresses (the TURN permissions the phone needs).
        peers: Vec<IpAddr>,
        reply: oneshot::Sender<Result<PhoneId, SignalError>>,
    },
    Release {
        reservation: Reservation,
    },
    Display {
        id: PhoneId,
        display: String,
        reply: oneshot::Sender<Result<(), SignalError>>,
    },
    Close {
        id: PhoneId,
        reply: oneshot::Sender<Result<(), SignalError>>,
    },
    /// The relay's current credential (the minter's, or a test's).
    TurnCredentials(TurnCreds),
    /// No credential to be had (no TURN key, a failed mint): the reason.
    TurnUnavailable(String),
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Command::Reserve { .. } => "Reserve",
            Command::Adopt { .. } => "Adopt",
            Command::Release { .. } => "Release",
            Command::Display { .. } => "Display",
            Command::Close { .. } => "Close",
            Command::TurnCredentials(_) => "TurnCredentials",
            Command::TurnUnavailable(_) => "TurnUnavailable",
        })
    }
}

/// The HTTP side's handle on the network task.
#[derive(Debug, Clone)]
pub struct WebrtcHandle {
    cmd: mpsc::Sender<Command>,
    pub shared: Arc<WebrtcShared>,
    pub factory: Arc<RtcFactory>,
}

impl WebrtcHandle {
    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, SignalError>>) -> Command,
    ) -> Result<T, SignalError> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(make(tx))
            .await
            .map_err(|_| SignalError::Unavailable)?;
        rx.await.map_err(|_| SignalError::Unavailable)?
    }

    /// A phone's offer from `client`: the capacity first (on the media task, a few µs), the SDP
    /// parse, the certificate and the answer on the blocking pool, then the ready `Rtc` goes to the
    /// media task. `has_token` says whether the client's rate bucket has a token; `charge` spends
    /// it, and is called only when a FRESH offer (not a reconnect) got a slot. Returns the phone's
    /// id and the hub's answer.
    pub async fn offer(
        &self,
        req: OfferRequest,
        client: String,
        has_token: bool,
        charge: impl FnOnce(),
    ) -> Result<Answered, SignalError> {
        let replaces = req.replaces;
        let (reservation, reconnect) = self
            .ask(|reply| Command::Reserve {
                replaces,
                client,
                has_token,
                reply,
            })
            .await?;
        if !reconnect {
            charge();
        }
        let factory = self.factory.clone();
        let sdp = req.sdp;
        let built = tokio::task::spawn_blocking(move || factory.answer(&sdp))
            .await
            .unwrap_or(Err(SignalError::Unavailable));
        let (rtc, answer, peers) = match built {
            Ok(b) => b,
            Err(e) => {
                let _ = self.cmd.send(Command::Release { reservation }).await;
                return Err(e);
            }
        };
        let display = req.display;
        let rtc = Box::new(rtc);
        let id = self
            .ask(|reply| Command::Adopt {
                reservation,
                rtc,
                display,
                peers,
                reply,
            })
            .await?;
        Ok(Answered { id, sdp: answer })
    }

    /// A phone's new display name.
    pub async fn set_display(&self, id: PhoneId, display: String) -> Result<(), SignalError> {
        self.ask(|reply| Command::Display { id, display, reply })
            .await
    }

    /// A phone leaves. An id the hub no longer has is not an error (it already left).
    pub async fn close(&self, id: PhoneId) -> Result<(), SignalError> {
        self.ask(|reply| Command::Close { id, reply }).await
    }

    /// Hand the TURN relay a credential (the minter's feed does this; tests call it directly with
    /// [`TurnFeed::Manual`]). `Err` when the network task is gone. Ignored with TURN off.
    pub async fn set_turn_credentials(&self, creds: TurnCreds) -> Result<(), SignalError> {
        self.cmd
            .send(Command::TurnCredentials(creds))
            .await
            .map_err(|_| SignalError::Unavailable)
    }
}

/// Where the hub-side TURN relay gets its credential (phase 3).
pub enum TurnFeed {
    /// No relay: host candidates only.
    Off,
    /// The box's Cloudflare TURN key: the network task's feeder asks the minter (the same cache
    /// `/api/ice` serves), resolves its UDP `turn:` url and hands every new credential over.
    Cloudflare(Arc<TurnMinter>),
    /// A relay whose credentials come through [`WebrtcHandle::set_turn_credentials`] (tests).
    Manual,
}

/// What [`start_webrtc`] needs.
pub struct WebrtcLegConfig {
    /// The bound socket (bound by the caller, so a bind failure stops the hub at start).
    pub socket: std::net::UdpSocket,
    /// The host candidate IPs; the socket's port is added.
    pub host_ips: Vec<IpAddr>,
    pub max_phones: usize,
    pub slots: Arc<RoomSlots>,
    pub shared: Arc<WebrtcShared>,
    pub dante_guard: Arc<DanteGuard>,
}

/// Start the network task on the current tokio runtime, host candidates only. Returns the HTTP
/// handle and the sender the phones-room thread hands its packets to.
pub fn start_webrtc(
    cfg: WebrtcLegConfig,
) -> anyhow::Result<(WebrtcHandle, mpsc::Sender<TxPacket>)> {
    start_webrtc_turn(cfg, TurnFeed::Off, TurnOptions::for_phones(1))
}

/// [`start_webrtc`] with the hub-side TURN relay fed by `feed`, bounded by `opts`.
pub fn start_webrtc_turn(
    cfg: WebrtcLegConfig,
    feed: TurnFeed,
    opts: TurnOptions,
) -> anyhow::Result<(WebrtcHandle, mpsc::Sender<TxPacket>)> {
    cfg.socket.set_nonblocking(true)?;
    let local = cfg.socket.local_addr()?;
    let socket = tokio::net::UdpSocket::from_std(cfg.socket)?;
    // Each datagram carries its destination (str0m pairs it with a local candidate) and its
    // ingress interface (the Dante guard drops what arrived on the Dante leg, intercom#25); each
    // send names its source (`pktinfo::try_send_from`).
    crate::pktinfo::enable_guarded(&socket, &cfg.dante_guard, "webrtc socket")?;
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE);
    let (pkt_tx, pkt_rx) = mpsc::channel(4 * cfg.max_phones.max(1));
    let candidates: Vec<SocketAddr> = cfg
        .host_ips
        .iter()
        .map(|ip| SocketAddr::new(*ip, local.port()))
        .collect();
    let factory = Arc::new(RtcFactory::new(candidates.clone()));
    let relay_candidate = factory.relay.clone();
    let turn = match &feed {
        TurnFeed::Off => None,
        TurnFeed::Cloudflare(_) | TurnFeed::Manual => {
            Some(TurnRelay::new(opts).map_err(anyhow::Error::msg)?)
        }
    };
    // Every relay socket's reader hands its datagrams to the task through this queue.
    let (turn_in_tx, turn_in_rx) = mpsc::channel(TURN_IN_QUEUE);
    if let TurnFeed::Cloudflare(minter) = feed {
        tokio::spawn(feed_turn_credentials(minter, cmd_tx.clone()));
    }
    let handle = WebrtcHandle {
        cmd: cmd_tx,
        shared: cfg.shared.clone(),
        factory,
    };
    let task = NetTask {
        socket,
        local,
        candidates,
        phones: Vec::new(),
        reservations: Vec::new(),
        recent: VecDeque::new(),
        unconnected_log: RefusalLog::default(),
        table: SlotTable::new(cfg.max_phones),
        slots: cfg.slots,
        shared: cfg.shared,
        guard: cfg.dante_guard,
        turn,
        turn_socks: HashMap::new(),
        turn_in: turn_in_tx,
        turn_send_fail: SendFailures::default(),
        relay_candidate,
        relay_published: None,
    };
    task.publish();
    tokio::spawn(task.run(cmd_rx, pkt_rx, turn_in_rx));
    Ok((handle, pkt_tx))
}

/// Where a str0m transmit goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxRoute {
    /// From a host candidate: the phones' socket, source-addressed.
    Host,
    /// From the live relayed address: wrapped for the TURN server.
    Relay,
    /// From anything else (an OLD relayed address during or after a re-allocation): dropped and
    /// counted on the relay's `tx_refused`. Never the host socket: a non-local source address
    /// there is EINVAL and a false `send_errors` warning (review of phase 3).
    Drop,
}

/// The route of a transmit from `source`, given the host candidates and the live relayed address.
pub fn transmit_route(
    source: SocketAddr,
    hosts: &[SocketAddr],
    relayed: Option<SocketAddr>,
) -> TxRoute {
    if relayed == Some(source) {
        TxRoute::Relay
    } else if hosts.contains(&source) {
        TxRoute::Host
    } else {
        TxRoute::Drop
    }
}

/// A connected phone counts as on the relay (a credential rotation waits) while a datagram came
/// through the relay within this long. `/api/state` `phones[].pair` is the path of its LAST
/// datagram (str0m exposes the nominated pair only through its stats events, not cheaply).
pub const RELAY_BUSY_WINDOW: Duration = Duration::from_secs(30);

/// Whether a phone's last relayed datagram (`last`) is inside [`RELAY_BUSY_WINDOW`].
pub fn relay_recent(last: Option<Instant>, now: Instant) -> bool {
    last.is_some_and(|t| now.saturating_duration_since(t) <= RELAY_BUSY_WINDOW)
}

/// How many relay datagrams may wait for the network task.
const TURN_IN_QUEUE: usize = 256;

/// One relay allocation's socket and its reader task.
struct TurnSock {
    sock: Arc<tokio::net::UdpSocket>,
    local: SocketAddr,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for TurnSock {
    /// Its reader goes with it (a closed allocation, or the network task ending): the socket is
    /// closed once the last `Arc` (the reader's) is gone.
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// A relay allocation's own socket: IPv4, an EPHEMERAL port, every interface, one per allocation
/// (epoch), so every allocation is a new 5-tuple at the TURN server. The kernel picks the source
/// address by route (the TURN server is on the internet, so the default route's address); that
/// stays the allocation's 5-tuple while the route holds. A reader task forwards what arrives
/// (`epoch`, source, bytes) to the network task; datagrams arriving on the Dante leg are dropped
/// first, like on the phones' socket (intercom#25).
fn bind_turn_socket(
    epoch: u32,
    guard: &Arc<DanteGuard>,
    to_task: &mpsc::Sender<(u32, SocketAddr, Vec<u8>)>,
) -> anyhow::Result<TurnSock> {
    let std_sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    std_sock.set_nonblocking(true)?;
    let sock = Arc::new(tokio::net::UdpSocket::from_std(std_sock)?);
    crate::pktinfo::enable_guarded(&*sock, guard, "webrtc turn socket")?;
    let local = sock.local_addr()?;
    let (rsock, guard, tx) = (sock.clone(), guard.clone(), to_task.clone());
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; RECV_BUF];
        loop {
            match crate::pktinfo::recv_from_with_info(&rsock, &mut buf).await {
                Ok((n, from, info)) => {
                    if guard.check_datagram(local.port(), info, from) {
                        continue;
                    }
                    if tx.send((epoch, from, buf[..n].to_vec())).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(%local, error = %e, "webrtc turn: socket receive failed");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    });
    tracing::debug!(epoch, %local, "webrtc turn: a new socket for a new allocation");
    Ok(TurnSock {
        sock,
        local,
        reader,
    })
}

/// The TURN server a minted credential set names: its UDP `turn:` url, resolved to IPv4.
async fn resolve_turn_server(urls: &[String]) -> Result<SocketAddr, String> {
    let (host, port) = turn_udp_server(urls)
        .ok_or_else(|| "the minted credential has no UDP turn: url".to_string())?;
    let found = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| format!("cannot resolve the TURN server {host}: {e}"))?;
    let mut v4 = found.filter(SocketAddr::is_ipv4);
    v4.next()
        .ok_or_else(|| format!("the TURN server {host} has no IPv4 address"))
}

/// The Cloudflare credential feed for the hub's own relay: ask the minter (the cache `/api/ice`
/// serves, renewed by its own refresher) and hand the relay every NEW credential (another username
/// or server) with its expiry; a failure is handed over as the reason, once per change. Sleeps
/// until the cache's 2/3 point, or `TURN_REFRESH_RETRY_S` after a failure. Ends with the task.
async fn feed_turn_credentials(minter: Arc<TurnMinter>, cmd: mpsc::Sender<Command>) {
    let mut last: Option<(String, SocketAddr)> = None;
    let mut last_err: Option<String> = None;
    loop {
        let got = match minter.current().await {
            Ok((creds, left)) => match resolve_turn_server(&creds.urls).await {
                Ok(server) => Ok((creds, left, server)),
                Err(e) => Err(e),
            },
            Err(e) => Err(e.to_string()),
        };
        let wait = match got {
            Ok((creds, left, server)) => {
                let key = (creds.username.clone(), server);
                if last.as_ref() != Some(&key) {
                    let c = TurnCreds {
                        server,
                        username: creds.username.clone(),
                        password: creds.credential.clone(),
                        expires_at: Instant::now() + Duration::from_secs(left),
                    };
                    if cmd.send(Command::TurnCredentials(c)).await.is_err() {
                        return;
                    }
                    last = Some(key);
                }
                last_err = None;
                refresh_wait_s(left, TURN_TTL_S)
            }
            Err(e) => {
                if last_err.as_deref() != Some(e.as_str()) {
                    tracing::warn!(error = %e, "webrtc turn: no credential for the hub's relay");
                    if cmd.send(Command::TurnUnavailable(e.clone())).await.is_err() {
                        return;
                    }
                    last_err = Some(e);
                }
                TURN_REFRESH_RETRY_S
            }
        };
        tokio::time::sleep(Duration::from_secs(wait.max(1))).await;
    }
}

/// Every IPv4 address of an interface that is up (`getifaddrs`), with the interface's name: the
/// caller ([`crate::webrtc_signal::WebrtcConfig::candidate_ips`]) keeps the LAN ones. Empty when
/// the call fails.
pub fn local_ipv4_addrs() -> Vec<(String, IpAddr)> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list freed below with freeifaddrs.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        tracing::warn!(error = %std::io::Error::last_os_error(), "webrtc: getifaddrs failed");
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list getifaddrs returned and is not freed yet.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null()
            || ifa.ifa_name.is_null()
            || ifa.ifa_flags & (libc::IFF_UP as libc::c_uint) == 0
        {
            continue;
        }
        // SAFETY: ifa_addr points at a sockaddr whose family field is valid to read.
        if i32::from(unsafe { (*ifa.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: AF_INET: a sockaddr_in; the name is NUL-terminated and owned by the list.
        let (a, name) = unsafe {
            (
                &*(ifa.ifa_addr as *const libc::sockaddr_in),
                std::ffi::CStr::from_ptr(ifa.ifa_name)
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        found.push((
            name,
            IpAddr::V4(std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr))),
        ));
    }
    // SAFETY: the list getifaddrs allocated, freed once.
    unsafe { libc::freeifaddrs(head) };
    found
}

struct Phone {
    id: PhoneId,
    slot: usize,
    generation: u64,
    rtc: Rtc,
    mid: Option<Mid>,
    pt: Option<Pt>,
    display: String,
    decoder: RxDecoder,
    ice: IceConnectionState,
    connected: bool,
    created: Instant,
    /// The first valid STUN binding request from the phone (ICE-lite: the phone checks, the hub
    /// answers). Until it comes the slot is held only [`STUN_DEADLINE`].
    stun_at: Option<Instant>,
    /// The room tick of the first packet written to this phone: its RTP time is (tick - first) x
    /// 960, so a packet the full queue dropped still advances it.
    first_tick: Option<u64>,
    rx_packets: u64,
    tx_packets: u64,
    rx_lost_frames: u64,
    last_sound: Option<Instant>,
    /// Why the phone is being dropped (set once; the sweep removes it).
    ending: Option<&'static str>,
    /// Its ICE state or connection changed since the facet was last published.
    changed: bool,
    /// The client key its offer came from (`webrtc_signal::client_key`).
    client: String,
    /// The reconnect claim its offer spent, given back if it ends without connecting.
    claim: Option<(PhoneId, Instant)>,
    /// Its own sends' health: one phone failing never flips the state of another (review round 3).
    send_fail: SendFailures,
    /// The IPs its offer named (the TURN permissions it needs, phase 3).
    peers: Vec<IpAddr>,
    /// Whether its last accepted datagram came through the hub's TURN relay (`None` before one).
    via_relay: Option<bool>,
    /// When its last datagram came through the relay.
    last_relay_rx: Option<Instant>,
}

struct PendingReservation {
    r: Reservation,
    deadline: Instant,
    /// The reconnect claim this offer spent (the old id and the time it became a claim): exempt
    /// from the pending caps and the rate bucket; given back when the offer never connects.
    claim: Option<(PhoneId, Instant)>,
    client: String,
}

/// The source-addressed sends' health: a warning when sending starts to fail, an info line when
/// it works again, each with the count, never a line per packet (review round 2 🔵5).
#[derive(Debug, Default)]
pub struct SendFailures {
    failing: bool,
    count: u64,
}

impl SendFailures {
    /// A send failed: `Some(failures so far)` when this starts a failing run (warn now).
    pub fn fail(&mut self) -> Option<u64> {
        self.count += 1;
        if self.failing {
            None
        } else {
            self.failing = true;
            Some(self.count)
        }
    }

    /// A send went out: `Some(failures so far)` when this ends a failing run (log the recovery).
    pub fn ok(&mut self) -> Option<u64> {
        if self.failing {
            self.failing = false;
            Some(self.count)
        } else {
            None
        }
    }
}

struct NetTask {
    socket: tokio::net::UdpSocket,
    local: SocketAddr,
    candidates: Vec<SocketAddr>,
    phones: Vec<Phone>,
    reservations: Vec<PendingReservation>,
    /// Phones that were connected and left within [`RECENT_WINDOW`]: their id still counts as a
    /// reconnect claim.
    recent: VecDeque<(PhoneId, Instant)>,
    table: SlotTable,
    slots: Arc<RoomSlots>,
    shared: Arc<WebrtcShared>,
    guard: Arc<DanteGuard>,
    /// Offers that ended without connecting: one summary line at most every 10 s.
    unconnected_log: RefusalLog,
    /// The hub's own TURN relay (phase 3), `None` with `[webrtc].turn` off.
    turn: Option<TurnRelay>,
    /// Each live relay allocation's own socket, by epoch (see [`bind_turn_socket`]).
    turn_socks: HashMap<u32, TurnSock>,
    /// The readers' way in.
    turn_in: mpsc::Sender<(u32, SocketAddr, Vec<u8>)>,
    /// The sends to the TURN server: a warning when they start to fail, a line when they work.
    turn_send_fail: SendFailures,
    /// The relayed candidate the answers carry (shared with the [`RtcFactory`]).
    relay_candidate: RelayCandidate,
    relay_published: Option<SocketAddr>,
}

fn ice_name(s: IceConnectionState) -> &'static str {
    match s {
        IceConnectionState::New => "new",
        IceConnectionState::Checking => "checking",
        IceConnectionState::Connected => "connected",
        IceConnectionState::Completed => "completed",
        IceConnectionState::Disconnected => "disconnected",
    }
}

/// A STUN message (RFC 5389): the first two bits 0 and the magic cookie at bytes 4..8.
pub fn is_stun(data: &[u8]) -> bool {
    data.len() >= 20 && data[0] < 4 && data[4..8] == [0x21, 0x12, 0xA4, 0x42]
}

impl NetTask {
    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<Command>,
        mut pkt_rx: mpsc::Receiver<TxPacket>,
        mut turn_rx: mpsc::Receiver<(u32, SocketAddr, Vec<u8>)>,
    ) {
        let mut buf = vec![0u8; RECV_BUF];
        let mut publish = tokio::time::interval(Duration::from_secs(1));
        publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            bind = %self.local,
            candidates = ?self.candidates,
            max_phones = self.table.capacity(),
            pending_cap = pending_cap(self.table.capacity()),
            turn = self.turn.is_some(),
            "webrtc: phone leg listening (ICE-lite; with turn the hub's own relayed candidate joins the host ones)"
        );
        loop {
            let next = self.drive(Instant::now());
            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(next));
            tokio::select! {
                r = crate::pktinfo::recv_from_with_info(&self.socket, &mut buf) => match r {
                    Ok((n, from, info)) => {
                        if self.guard.check_datagram(self.local.port(), info, from) {
                            continue;
                        }
                        let dst_ip = info.map(|i| IpAddr::V4(i.dst));
                        self.receive(&buf[..n], from, dst_ip);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "webrtc: socket receive failed");
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                },
                Some((epoch, from, data)) = turn_rx.recv() => self.receive_turn(epoch, &data, from),
                Some(cmd) = cmd_rx.recv() => self.command(cmd),
                Some(pkt) = pkt_rx.recv() => self.write_packet(&pkt),
                _ = sleep => {}
                _ = publish.tick() => self.publish(),
            }
        }
    }

    /// The destination a datagram was sent to: the IP_PKTINFO destination, else the bound IP, else
    /// the first candidate (str0m pairs it with a local candidate).
    fn destination(&self, dst_ip: Option<IpAddr>) -> SocketAddr {
        let ip = dst_ip
            .or_else(|| (!self.local.ip().is_unspecified()).then_some(self.local.ip()))
            .or_else(|| self.candidates.first().map(|c| c.ip()))
            .unwrap_or(self.local.ip());
        SocketAddr::new(ip, self.local.port())
    }

    fn receive(&mut self, data: &[u8], from: SocketAddr, dst_ip: Option<IpAddr>) {
        let destination = self.destination(dst_ip);
        self.deliver(data, from, destination, false, Instant::now());
    }

    /// A datagram on a relay allocation's socket (`epoch`): the relay takes only its TURN
    /// server's exact address on a live allocation's socket. A response moves the relay; a phone's
    /// datagram comes out unwrapped and str0m sees it from the phone to the hub's relayed address.
    fn receive_turn(&mut self, epoch: u32, data: &[u8], from: SocketAddr) {
        let now = Instant::now();
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if !turn.accepts(epoch, from) {
            // Anyone but the TURN server, or a late answer on a retiring socket.
            self.shared.rx_refused.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let got = turn.handle_datagram(epoch, from, data, now);
        let relayed = turn.relayed();
        if let (Some((peer, payload)), Some(relayed)) = (got, relayed) {
            self.deliver(&payload, peer, relayed, true, now);
        }
        self.after_turn();
    }

    /// One datagram for the phones: `from` the phone (or the peer the relay unwrapped) to
    /// `destination` (a host candidate, or the relayed address).
    fn deliver(
        &mut self,
        data: &[u8],
        from: SocketAddr,
        destination: SocketAddr,
        via_relay: bool,
        now: Instant,
    ) {
        let stun = is_stun(data);
        let Ok(contents) = data.try_into() else {
            self.shared.rx_refused.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let input = Input::Receive(
            now,
            Receive {
                proto: Protocol::Udp,
                source: from,
                destination,
                contents,
            },
        );
        let Some(phone) = self.phones.iter_mut().find(|p| p.rtc.accepts(&input)) else {
            self.shared.rx_refused.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if stun && phone.stun_at.is_none() {
            // accepts() matched the STUN message on this phone's ICE credentials.
            phone.stun_at = Some(now);
        }
        if via_relay {
            phone.last_relay_rx = Some(now);
        }
        if phone.via_relay != Some(via_relay) {
            phone.via_relay = Some(via_relay);
            phone.changed = true;
        }
        if let Err(e) = phone.rtc.handle_input(input) {
            tracing::warn!(phone = %phone.id.short(), error = %e, "webrtc: input refused — dropping the phone");
            phone.ending.get_or_insert("rtc error");
        }
    }

    /// Unconnected phones and open reservations, the reconnects not counted.
    fn pending(&self) -> usize {
        self.phones
            .iter()
            // A reconnecting phone (it spent a claim) is not a fresh offer: a crew-wide reconnect
            // never holds the cap against a new phone (review round 4).
            .filter(|p| !p.connected && p.ending.is_none() && p.claim.is_none())
            .count()
            + self
                .reservations
                .iter()
                .filter(|r| r.claim.is_none())
                .count()
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Reserve {
                replaces,
                client,
                has_token,
                reply,
            } => {
                let r = self.reserve(replaces, client, has_token);
                let _ = reply.send(r);
            }
            Command::Adopt {
                reservation,
                rtc,
                display,
                peers,
                reply,
            } => {
                let r = self.adopt(reservation, *rtc, display, peers);
                let _ = reply.send(r);
            }
            Command::TurnCredentials(creds) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.set_credentials(creds, Instant::now());
                }
                self.after_turn();
                self.publish();
            }
            Command::TurnUnavailable(reason) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.set_unavailable(&reason);
                }
                self.publish();
            }
            Command::Release { reservation } => self.release(reservation),
            Command::Display {
                id,
                display: name,
                reply,
            } => {
                let r = match self.phones.iter_mut().find(|p| p.id == id) {
                    Some(p) => {
                        tracing::info!(phone = %id.short(), old = %p.display, new = %name, "webrtc: phone renamed");
                        p.display = name;
                        Ok(())
                    }
                    None => Err(SignalError::UnknownPhone),
                };
                if r.is_ok() {
                    self.publish(); // the new name shows at once, not at the next second
                }
                let _ = reply.send(r);
            }
            Command::Close { id, reply } => {
                if let Some(p) = self.phones.iter_mut().find(|p| p.id == id) {
                    p.ending.get_or_insert("left");
                }
                // Idempotent: a phone the hub already dropped has left too.
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Set a slot aside for an offer from `client`. A `replaces` naming a live phone drops that
    /// phone; one naming a recently connected phone takes that claim. Either makes the offer a
    /// reconnect: exempt from the hub-wide pending cap, the per-client pending cap and the rate
    /// bucket (review round 3: a venue's whole crew comes back at once under one key after a
    /// restart). One open reservation per claimed id. A fresh offer needs a token and may be one
    /// of at most [`pending_cap_per_client`] pending ones of its client. Returns the reservation
    /// and whether it is a reconnect.
    fn reserve(
        &mut self,
        replaces: Option<PhoneId>,
        client: String,
        has_token: bool,
    ) -> Result<(Reservation, bool), SignalError> {
        let now = Instant::now();
        let mut claim = None;
        if let Some(old) = replaces {
            if self
                .reservations
                .iter()
                .any(|r| r.claim.is_some_and(|(id, _)| id == old))
            {
                // A second offer for the same id while the first is open: no reconnect.
            } else if let Some(i) = self
                .phones
                .iter()
                // Only a phone that really connected is a claim (review round 5): a pending phone's
                // id is in its own answer, so an attacker could name it.
                .position(|p| p.id == old && p.ending.is_none() && p.connected)
            {
                self.phones[i].ending = Some(REPLACED);
                self.sweep(now);
                claim = Some((old, now));
            } else if let Some(i) = self.recent.iter().position(|(id, _)| *id == old) {
                claim = self.recent.remove(i);
            }
        }
        if claim.is_none() {
            if !has_token {
                return Err(SignalError::RateLimited);
            }
            let pending_of_client = self
                .phones
                .iter()
                .filter(|p| {
                    p.ending.is_none() && !p.connected && p.claim.is_none() && p.client == client
                })
                .count()
                + self
                    .reservations
                    .iter()
                    .filter(|r| r.claim.is_none() && r.client == client)
                    .count();
            if pending_of_client >= pending_cap_per_client(self.table.capacity()) {
                return Err(SignalError::RateLimited);
            }
            if self.pending() >= pending_cap(self.table.capacity()) {
                return Err(SignalError::Full);
            }
        }
        let Some((slot, generation)) = self.table.allocate() else {
            self.give_back(claim, now);
            return Err(SignalError::Full);
        };
        let r = Reservation { slot, generation };
        self.reservations.push(PendingReservation {
            r,
            deadline: now + RESERVATION_DEADLINE,
            claim,
            client,
        });
        Ok((r, claim.is_some()))
    }

    /// A reconnect claim whose offer never connected goes back (review round 3): the phone's page
    /// already names its new id, so the old one must stay valid for its next try, still bounded
    /// by [`RECENT_WINDOW`] from when it became a claim.
    fn give_back(&mut self, claim: Option<(PhoneId, Instant)>, now: Instant) {
        if let Some((id, since)) = claim {
            if now.saturating_duration_since(since) <= RECENT_WINDOW
                && !self.recent.iter().any(|(r, _)| *r == id)
            {
                self.recent.push_back((id, since));
            }
        }
    }

    fn release(&mut self, r: Reservation) {
        if let Some(i) = self.reservations.iter().position(|p| p.r == r) {
            let claim = self.reservations.swap_remove(i).claim;
            self.table.free(r.slot, r.generation);
            self.give_back(claim, Instant::now());
        }
    }

    fn adopt(
        &mut self,
        r: Reservation,
        rtc: Rtc,
        name: String,
        peers: Vec<IpAddr>,
    ) -> Result<PhoneId, SignalError> {
        let Some(i) = self.reservations.iter().position(|p| p.r == r) else {
            // The reservation expired while the answer was built.
            return Err(SignalError::Unavailable);
        };
        let PendingReservation { claim, client, .. } = self.reservations.swap_remove(i);
        let id = match PhoneId::random() {
            Ok(id) => id,
            Err(e) => {
                self.table.free(r.slot, r.generation);
                self.give_back(claim, Instant::now());
                tracing::error!(error = %e, "webrtc: no random id for a phone");
                return Err(SignalError::Unavailable);
            }
        };
        let decoder = match RxDecoder::new(JanusCodec::Opus) {
            Ok(d) => d,
            Err(e) => {
                self.table.free(r.slot, r.generation);
                self.give_back(claim, Instant::now());
                tracing::error!(error = %e, "webrtc: opus decoder create failed");
                return Err(SignalError::Unavailable);
            }
        };
        self.slots.open(r.slot, r.generation);
        self.shared.sessions_started.fetch_add(1, Ordering::Relaxed);
        // The session is logged at info once the phone connects (review round 2 🔵4): an offer
        // from the public route that never connects is only a debug line and a summary count.
        tracing::debug!(
            phone = %id.short(),
            display = %name,
            slot = r.slot,
            "webrtc: offer accepted, waiting for the phone's ICE check"
        );
        self.phones.push(Phone {
            id,
            slot: r.slot,
            generation: r.generation,
            rtc,
            mid: None,
            pt: None,
            display: name,
            decoder,
            ice: IceConnectionState::New,
            connected: false,
            created: Instant::now(),
            stun_at: None,
            first_tick: None,
            rx_packets: 0,
            tx_packets: 0,
            rx_lost_frames: 0,
            last_sound: None,
            ending: None,
            changed: false,
            client,
            claim,
            send_fail: SendFailures::default(),
            peers,
            via_relay: None,
            last_relay_rx: None,
        });
        // Its offer's addresses get a TURN permission now, before its first check comes.
        self.update_turn_wanted(Instant::now());
        self.publish();
        Ok(id)
    }

    fn write_packet(&mut self, pkt: &TxPacket) {
        let Some(phone) = self
            .phones
            .iter_mut()
            .find(|p| p.slot == pkt.slot && p.generation == pkt.generation)
        else {
            return;
        };
        let (Some(mid), Some(pt)) = (phone.mid, phone.pt) else {
            return;
        };
        if !phone.connected || phone.ending.is_some() {
            return;
        }
        let first = *phone.first_tick.get_or_insert(pkt.tick);
        let rtp_time = rtp_time_for(first, pkt.tick);
        let Some(writer) = phone.rtc.writer(mid) else {
            return;
        };
        match writer.write(pt, Instant::now(), rtp_time, pkt.payload().to_vec()) {
            Ok(()) => phone.tx_packets += 1,
            Err(e) => {
                tracing::warn!(phone = %phone.id.short(), error = %e, "webrtc: media write failed — dropping the phone");
                phone.ending.get_or_insert("write error");
            }
        }
    }

    /// Feed time to every phone, send what they want to send, handle their events, drop the
    /// phones that ended and the reservations that expired. Returns when the next timeout is due.
    fn drive(&mut self, now: Instant) -> Instant {
        let mut next = now + Duration::from_millis(100);
        // A phone on the relay holds off a credential rotation (never past the old one's expiry).
        let busy = self
            .phones
            .iter()
            .any(|p| p.connected && p.ending.is_none() && relay_recent(p.last_relay_rx, now));
        if let Some(turn) = self.turn.as_mut() {
            turn.handle_timeout(now, busy);
        }
        for phone in &mut self.phones {
            if phone.ending.is_none() {
                if let Err(e) = phone.rtc.handle_input(Input::Timeout(now)) {
                    tracing::warn!(phone = %phone.id.short(), error = %e, "webrtc: timeout handling failed — dropping the phone");
                    phone.ending.get_or_insert("rtc error");
                }
            }
            if let Some(t) = poll_phone(
                phone,
                &self.socket,
                &self.candidates,
                self.turn.as_mut(),
                &self.slots,
                &self.shared,
            ) {
                next = next.min(t);
            }
            if phone.ending.is_none() && !phone.connected {
                match phone.stun_at {
                    None if now.duration_since(phone.created) > STUN_DEADLINE => {
                        phone.ending = Some("no ICE check from the phone");
                    }
                    Some(t) if now.duration_since(t) > CONNECT_DEADLINE => {
                        phone.ending = Some("never connected");
                    }
                    _ => {}
                }
            }
            if phone.ending.is_none() && !phone.rtc.is_alive() {
                phone.ending = Some("closed");
            }
            if phone.ending.is_none() {
                let deadline = match phone.stun_at {
                    None => phone.created + STUN_DEADLINE,
                    Some(t) => t + CONNECT_DEADLINE,
                };
                if !phone.connected {
                    next = next.min(deadline);
                }
            }
        }
        // What the relay has to send (its own requests, the phones' wrapped datagrams).
        self.after_turn();
        if let Some(t) = self.turn.as_ref().and_then(TurnRelay::poll_timeout) {
            next = next.min(t);
        }
        // Reservations whose answer never came back (the HTTP handler went away).
        let before = self.reservations.len();
        let mut expired = Vec::new();
        self.reservations.retain(|p| {
            let keep = p.deadline > now;
            if !keep {
                expired.push((p.r, p.claim));
            }
            keep
        });
        for (r, claim) in expired {
            self.table.free(r.slot, r.generation);
            self.give_back(claim, now);
        }
        if self.reservations.len() != before {
            tracing::debug!(
                expired = before - self.reservations.len(),
                "webrtc: offer reservations expired"
            );
        }
        // An ICE change or a connect shows on /api/state at once, not at the next second.
        let mut changed = false;
        for p in &mut self.phones {
            // every flag is cleared, so no short-circuit
            changed |= std::mem::take(&mut p.changed);
        }
        if !self.sweep(now) && changed {
            self.publish();
        }
        next.max(now + Duration::from_millis(1))
    }

    /// Drop every phone that ended; true when any was (the facet was then republished).
    fn sweep(&mut self, now: Instant) -> bool {
        if !self.phones.iter().any(|p| p.ending.is_some()) {
            return false;
        }
        let mut kept = Vec::with_capacity(self.phones.len());
        for mut phone in std::mem::take(&mut self.phones) {
            match phone.ending {
                None => kept.push(phone),
                Some(why) => {
                    phone.rtc.disconnect();
                    self.slots.close(phone.slot, phone.generation);
                    self.table.free(phone.slot, phone.generation);
                    self.shared.sessions_ended.fetch_add(1, Ordering::Relaxed);
                    if phone.connected {
                        // A replaced phone's id was its reconnect's claim: spent.
                        if why != REPLACED {
                            self.recent.push_back((phone.id, now));
                        }
                        tracing::info!(
                            phone = %phone.id.short(),
                            display = %phone.display,
                            why,
                            age_s = phone.created.elapsed().as_secs(),
                            rx_packets = phone.rx_packets,
                            tx_packets = phone.tx_packets,
                            rx_lost_frames = phone.rx_lost_frames,
                            "webrtc: phone session ended"
                        );
                    } else {
                        // Its reconnect claim stays valid for the phone's next try.
                        self.give_back(phone.claim, now);
                        self.shared
                            .sessions_never_connected
                            .fetch_add(1, Ordering::Relaxed);
                        if let Some(count) = self.unconnected_log.note(now) {
                            tracing::info!(
                                ended_since_last_line = count,
                                last_reason = why,
                                "webrtc: offers ended without their phone connecting (counted on /api/state webrtc.sessions_never_connected)"
                            );
                        }
                    }
                }
            }
        }
        // A claim given back keeps its original time, so the list is not in time order.
        self.recent
            .retain(|(_, t)| now.saturating_duration_since(*t) <= RECENT_WINDOW);
        while self.recent.len() > RECENT_MAX {
            self.recent.pop_front();
        }
        self.phones = kept;
        self.update_turn_wanted(now);
        self.publish();
        true
    }

    /// The TURN permissions the live phones need: their offers' addresses.
    fn update_turn_wanted(&mut self, now: Instant) {
        if let Some(turn) = self.turn.as_mut() {
            let ips: Vec<IpAddr> = self
                .phones
                .iter()
                .filter(|p| p.ending.is_none())
                .flat_map(|p| p.peers.iter().copied())
                .collect();
            turn.set_wanted(ips, now);
        }
        self.after_turn();
    }

    /// Send what the relay queued, each datagram from its allocation's own socket (bound on its
    /// first datagram), close the sockets of the allocations the relay no longer uses, and keep
    /// the answers' relayed candidate current.
    fn after_turn(&mut self) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        while let Some(tx) = turn.poll_transmit() {
            if !self.turn_socks.contains_key(&tx.epoch) {
                match bind_turn_socket(tx.epoch, &self.guard, &self.turn_in) {
                    Ok(s) => {
                        self.turn_socks.insert(tx.epoch, s);
                    }
                    Err(e) => {
                        // The request goes unanswered, so the relay takes it for failed and
                        // tries again on a new socket.
                        self.shared.send_errors.fetch_add(1, Ordering::Relaxed);
                        if let Some(count) = self.turn_send_fail.fail() {
                            tracing::warn!(error = %e, failures = count, "webrtc turn: cannot bind a relay socket");
                        }
                        continue;
                    }
                }
            }
            let Some(ts) = self.turn_socks.get(&tx.epoch) else {
                continue;
            };
            let server = tx.to;
            match ts.sock.try_send_to(&tx.data, server) {
                Ok(_) => {
                    if let Some(count) = self.turn_send_fail.ok() {
                        tracing::info!(
                            failures = count,
                            "webrtc turn: sending to the TURN server works again"
                        );
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    self.shared.send_errors.fetch_add(1, Ordering::Relaxed);
                    if let Some(count) = self.turn_send_fail.fail() {
                        tracing::warn!(%server, error = %e, failures = count, "webrtc turn: sending to the TURN server failed (logged again when it works)");
                    }
                }
            }
        }
        let keep = turn.live_epochs();
        self.turn_socks.retain(|epoch, s| {
            let kept = keep.contains(epoch);
            if !kept {
                // Dropping it aborts its reader (`Drop for TurnSock`).
                tracing::debug!(epoch, local = %s.local, "webrtc turn: an old allocation's socket closed");
            }
            kept
        });
        let relayed = turn.relayed();
        if relayed != self.relay_published {
            *self
                .relay_candidate
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = relayed;
            self.relay_published = relayed;
        }
    }

    fn publish(&self) {
        let now = Instant::now();
        let phones = self
            .phones
            .iter()
            .map(|p| PhoneFacet {
                id: p.id.short(),
                display: p.display.clone(),
                slot: p.slot,
                ice_state: ice_name(p.ice),
                connected: p.connected,
                age_s: p.created.elapsed().as_secs(),
                rx_packets: p.rx_packets,
                tx_packets: p.tx_packets,
                rx_lost_frames: p.rx_lost_frames,
                mic_active: p
                    .last_sound
                    .is_some_and(|t| now.saturating_duration_since(t) < MIC_ACTIVE_WINDOW),
                mic_rate_ppm: self.slots.mic_rate_ppm(p.slot),
                pair: p.via_relay.map(|r| if r { "relay" } else { "host" }),
            })
            .collect();
        self.shared
            .phones_pending
            .store(self.pending() as u64, Ordering::Relaxed);
        self.shared.publish_phones(phones);
        self.shared.publish_turn(self.turn.as_ref().map(|t| {
            let mut f = t.facet(now);
            f.local = t
                .live_epoch()
                .and_then(|e| self.turn_socks.get(&e))
                .map(|s| s.local.to_string());
            f
        }));
    }
}

/// A phone's RTP time for the room tick `tick`, its first packet at `first`: 960 per tick, so a
/// packet that never reached the phone still moves the clock on.
pub fn rtp_time_for(first: u64, tick: u64) -> MediaTime {
    MediaTime::new(
        tick.saturating_sub(first) * FRAME_48K as u64,
        Frequency::FORTY_EIGHT_KHZ,
    )
}

/// Poll one phone until it wants to wait: send its datagrams (from the local candidate str0m
/// chose), handle its events. The next timeout, or `None` once it is closed.
fn poll_phone(
    phone: &mut Phone,
    socket: &tokio::net::UdpSocket,
    hosts: &[SocketAddr],
    mut turn: Option<&mut TurnRelay>,
    slots: &RoomSlots,
    shared: &WebrtcShared,
) -> Option<Instant> {
    loop {
        if !phone.rtc.is_alive() {
            return None;
        }
        let out = match phone.rtc.poll_output() {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(phone = %phone.id.short(), error = %e, "webrtc: poll failed — dropping the phone");
                phone.ending.get_or_insert("rtc error");
                return None;
            }
        };
        match out {
            Output::Timeout(t) => return Some(t),
            Output::Transmit(t) => {
                // From the relayed candidate: wrapped for the TURN server (the relay refuses a
                // peer its permissions do not cover). str0m only ever answers a check or sends
                // on a pair the phone validated (ICE-lite).
                let relayed = turn.as_deref().and_then(TurnRelay::relayed);
                match transmit_route(t.source, hosts, relayed) {
                    TxRoute::Host => {}
                    TxRoute::Relay => {
                        if let Some(relay) = turn.as_deref_mut() {
                            // Refused (no permission): counted on `tx_refused`, no line per packet.
                            let _ = relay.send_to(t.destination, &t.contents, Instant::now());
                        }
                        continue;
                    }
                    TxRoute::Drop => {
                        if let Some(relay) = turn.as_deref_mut() {
                            relay.refuse();
                        }
                        continue;
                    }
                }
                // Non-blocking: a full send buffer drops this datagram (WebRTC retransmits what it
                // must), it never stalls the task. The source is the candidate the phone talks
                // to, so a 0.0.0.0 bind with several host IPs answers from the right one.
                let src = match t.source.ip() {
                    IpAddr::V4(v4) if !v4.is_unspecified() => Some(v4),
                    _ => None,
                };
                match crate::pktinfo::try_send_from(socket, &t.contents, t.destination, src) {
                    Ok(_) => {
                        if let Some(count) = phone.send_fail.ok() {
                            tracing::info!(
                                phone = %phone.id.short(),
                                failures = count,
                                "webrtc: sending to the phone works again"
                            );
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        shared.send_errors.fetch_add(1, Ordering::Relaxed);
                        if let Some(count) = phone.send_fail.fail() {
                            tracing::warn!(
                                phone = %phone.id.short(),
                                dest = %t.destination,
                                source = ?src,
                                error = %e,
                                failures = count,
                                "webrtc: sending failed (logged again when it works; counted on /api/state webrtc.send_errors)"
                            );
                        }
                    }
                }
            }
            Output::Event(ev) => handle_event(phone, ev, slots),
        }
    }
}

fn handle_event(phone: &mut Phone, ev: Event, slots: &RoomSlots) {
    match ev {
        Event::IceConnectionStateChange(s) => {
            tracing::info!(phone = %phone.id.short(), display = %phone.display, from = ice_name(phone.ice), to = ice_name(s), "webrtc: ICE state");
            phone.ice = s;
            phone.changed = true;
            if s == IceConnectionState::Disconnected {
                phone.ending.get_or_insert("ICE disconnected");
            }
        }
        Event::Connected => {
            phone.connected = true;
            phone.changed = true;
            tracing::info!(
                phone = %phone.id.short(),
                display = %phone.display,
                slot = phone.slot,
                "webrtc: phone session started (ICE + DTLS up)"
            );
        }
        Event::MediaAdded(m) => {
            if m.kind == MediaKind::Audio && phone.mid.is_none() {
                phone.mid = Some(m.mid);
                phone.pt = phone.rtc.writer(m.mid).and_then(|w| {
                    w.payload_params()
                        .find(|p| p.spec().codec == Codec::Opus)
                        .map(|p| p.pt())
                });
                if phone.pt.is_none() {
                    tracing::warn!(phone = %phone.id.short(), "webrtc: the phone's audio has no Opus — nothing will be sent to it");
                }
            }
        }
        Event::MediaData(d) => {
            if Some(d.mid) != phone.mid {
                return;
            }
            phone.rx_packets += 1;
            let arrived = d.network_time;
            let seq = (**d.seq_range.start() & 0xffff) as u16;
            match phone.decoder.decode(seq, &d.data) {
                Ok(dec) => {
                    phone.rx_lost_frames += u64::from(dec.concealed_frames);
                    if dec.samples.is_empty() {
                        return;
                    }
                    let peak = dec
                        .samples
                        .iter()
                        .map(|s| s.unsigned_abs())
                        .max()
                        .unwrap_or(0);
                    if peak >= MIC_ACTIVE_PEAK.unsigned_abs() {
                        phone.last_sound = Some(arrived);
                    }
                    slots.push_mic(phone.slot, phone.generation, &dec.samples, arrived);
                }
                Err(e) => {
                    tracing::debug!(phone = %phone.id.short(), error = %e, "webrtc: undecodable packet dropped");
                }
            }
        }
        _ => {}
    }
}
