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
//! Host candidates only in this phase; the hub-side TURN relay is phase 3. Logging: one line per
//! session start / end, ICE state change and error, never one per packet.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
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
use crate::vban_jitter::NetworkFillStats;
use crate::webrtc_room::{RoomSlots, TxPacket};
use crate::webrtc_signal::{
    filter_offer_candidates, pending_cap, OfferRequest, PhoneId, SignalError, SlotTable,
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

/// What builds a phone's `Rtc` and its answer, OFF the media task (the SDP parse and the DTLS
/// certificate are the slow part): the crypto provider and the hub's host candidates.
#[derive(Debug, Clone)]
pub struct RtcFactory {
    crypto: Arc<CryptoProvider>,
    candidates: Vec<SocketAddr>,
}

impl RtcFactory {
    /// A factory for `candidates` with the build's crypto provider.
    pub fn new(candidates: Vec<SocketAddr>) -> Self {
        RtcFactory {
            crypto: Arc::new(str0m::crypto::from_feature_flags()),
            candidates,
        }
    }

    /// The hub's host candidates.
    pub fn candidates(&self) -> &[SocketAddr] {
        &self.candidates
    }

    /// An ICE-lite, Opus-only `Rtc` answering `sdp` (the offer's implausible candidates dropped
    /// first): the hub never sends a connectivity check, it only answers the phone's.
    pub fn answer(&self, sdp: &str) -> Result<(Rtc, String), SignalError> {
        let (sdp, _dropped) = filter_offer_candidates(sdp, &self.candidates);
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
        let answer: SdpAnswer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| SignalError::BadOffer(e.to_string()))?;
        Ok((rtc, answer.to_sdp_string()))
    }
}

enum Command {
    Reserve {
        replaces: Option<PhoneId>,
        reply: oneshot::Sender<Result<Reservation, SignalError>>,
    },
    Adopt {
        reservation: Reservation,
        rtc: Box<Rtc>,
        display: String,
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
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Command::Reserve { .. } => "Reserve",
            Command::Adopt { .. } => "Adopt",
            Command::Release { .. } => "Release",
            Command::Display { .. } => "Display",
            Command::Close { .. } => "Close",
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

    /// A phone's offer: the capacity first (on the media task, a few µs), the SDP parse, the
    /// certificate and the answer on the blocking pool, then the ready `Rtc` goes to the media
    /// task. Returns the phone's id and the hub's answer.
    pub async fn offer(&self, req: OfferRequest) -> Result<Answered, SignalError> {
        let replaces = req.replaces;
        let reservation = self
            .ask(|reply| Command::Reserve { replaces, reply })
            .await?;
        let factory = self.factory.clone();
        let sdp = req.sdp;
        let built = tokio::task::spawn_blocking(move || factory.answer(&sdp))
            .await
            .unwrap_or(Err(SignalError::Unavailable));
        let (rtc, answer) = match built {
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

/// Start the network task on the current tokio runtime. Returns the HTTP handle and the sender the
/// phones-room thread hands its packets to.
pub fn start_webrtc(
    cfg: WebrtcLegConfig,
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
        table: SlotTable::new(cfg.max_phones),
        slots: cfg.slots,
        shared: cfg.shared,
        guard: cfg.dante_guard,
    };
    tokio::spawn(task.run(cmd_rx, pkt_rx));
    Ok((handle, pkt_tx))
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
}

struct PendingReservation {
    r: Reservation,
    deadline: Instant,
    /// A reconnecting phone's offer (`replaces` matched): not held to the pending cap.
    reconnect: bool,
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
    ) {
        let mut buf = vec![0u8; RECV_BUF];
        let mut publish = tokio::time::interval(Duration::from_secs(1));
        publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            bind = %self.local,
            candidates = ?self.candidates,
            max_phones = self.table.capacity(),
            pending_cap = pending_cap(self.table.capacity()),
            "webrtc: phone leg listening (ICE-lite, host candidates only; TURN is phase 3)"
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
        let now = Instant::now();
        let destination = self.destination(dst_ip);
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
        if let Err(e) = phone.rtc.handle_input(input) {
            tracing::warn!(phone = %phone.id.short(), error = %e, "webrtc: input refused — dropping the phone");
            phone.ending.get_or_insert("rtc error");
        }
    }

    /// Unconnected phones and open reservations, the reconnects not counted.
    fn pending(&self) -> usize {
        self.phones
            .iter()
            .filter(|p| !p.connected && p.ending.is_none())
            .count()
            + self.reservations.iter().filter(|r| !r.reconnect).count()
    }

    fn command(&mut self, cmd: Command) {
        match cmd {
            Command::Reserve { replaces, reply } => {
                let r = self.reserve(replaces);
                let _ = reply.send(r);
            }
            Command::Adopt {
                reservation,
                rtc,
                display,
                reply,
            } => {
                let r = self.adopt(reservation, *rtc, display);
                let _ = reply.send(r);
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

    /// Set a slot aside for an offer. A `replaces` naming a live phone drops that phone first; one
    /// naming a live or recently connected phone is a reconnect, never held to the pending cap.
    fn reserve(&mut self, replaces: Option<PhoneId>) -> Result<Reservation, SignalError> {
        let now = Instant::now();
        let mut reconnect = false;
        if let Some(old) = replaces {
            if let Some(i) = self.phones.iter().position(|p| p.id == old) {
                self.phones[i].ending = Some("replaced by its reconnect");
                self.sweep(now);
                reconnect = true;
            } else {
                reconnect = self.recent.iter().any(|(id, _)| *id == old);
            }
        }
        if !reconnect && self.pending() >= pending_cap(self.table.capacity()) {
            return Err(SignalError::Full);
        }
        let Some((slot, generation)) = self.table.allocate() else {
            return Err(SignalError::Full);
        };
        let r = Reservation { slot, generation };
        self.reservations.push(PendingReservation {
            r,
            deadline: now + RESERVATION_DEADLINE,
            reconnect,
        });
        Ok(r)
    }

    fn release(&mut self, r: Reservation) {
        if let Some(i) = self.reservations.iter().position(|p| p.r == r) {
            self.reservations.swap_remove(i);
            self.table.free(r.slot, r.generation);
        }
    }

    fn adopt(&mut self, r: Reservation, rtc: Rtc, name: String) -> Result<PhoneId, SignalError> {
        let Some(i) = self.reservations.iter().position(|p| p.r == r) else {
            // The reservation expired while the answer was built.
            return Err(SignalError::Unavailable);
        };
        self.reservations.swap_remove(i);
        let id = match PhoneId::random() {
            Ok(id) => id,
            Err(e) => {
                self.table.free(r.slot, r.generation);
                tracing::error!(error = %e, "webrtc: no random id for a phone");
                return Err(SignalError::Unavailable);
            }
        };
        let decoder = match RxDecoder::new(JanusCodec::Opus) {
            Ok(d) => d,
            Err(e) => {
                self.table.free(r.slot, r.generation);
                tracing::error!(error = %e, "webrtc: opus decoder create failed");
                return Err(SignalError::Unavailable);
            }
        };
        self.slots.open(r.slot, r.generation);
        self.shared.sessions_started.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            phone = %id.short(),
            display = %name,
            slot = r.slot,
            phones = self.table.in_use(),
            "webrtc: phone session started"
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
        });
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
        for phone in &mut self.phones {
            if phone.ending.is_none() {
                if let Err(e) = phone.rtc.handle_input(Input::Timeout(now)) {
                    tracing::warn!(phone = %phone.id.short(), error = %e, "webrtc: timeout handling failed — dropping the phone");
                    phone.ending.get_or_insert("rtc error");
                }
            }
            if let Some(t) = poll_phone(phone, &self.socket, &self.slots) {
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
        // Reservations whose answer never came back (the HTTP handler went away).
        let before = self.reservations.len();
        let mut expired = Vec::new();
        self.reservations.retain(|p| {
            let keep = p.deadline > now;
            if !keep {
                expired.push(p.r);
            }
            keep
        });
        for r in expired {
            self.table.free(r.slot, r.generation);
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
                }
            }
        }
        while self
            .recent
            .front()
            .is_some_and(|(_, t)| now.saturating_duration_since(*t) > RECENT_WINDOW)
            || self.recent.len() > RECENT_MAX
        {
            self.recent.pop_front();
        }
        self.phones = kept;
        self.publish();
        true
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
            })
            .collect();
        self.shared
            .phones_pending
            .store(self.pending() as u64, Ordering::Relaxed);
        self.shared.publish_phones(phones);
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
    slots: &RoomSlots,
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
                // Non-blocking: a full send buffer drops this datagram (WebRTC retransmits what it
                // must), it never stalls the task. The source is the candidate the phone talks
                // to, so a 0.0.0.0 bind with several host IPs answers from the right one.
                let src = match t.source.ip() {
                    IpAddr::V4(v4) if !v4.is_unspecified() => Some(v4),
                    _ => None,
                };
                if let Err(e) =
                    crate::pktinfo::try_send_from(socket, &t.contents, t.destination, src)
                {
                    if e.kind() != std::io::ErrorKind::WouldBlock {
                        tracing::debug!(phone = %phone.id.short(), dest = %t.destination, error = %e, "webrtc: send failed");
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
            tracing::info!(phone = %phone.id.short(), display = %phone.display, "webrtc: phone connected (ICE + DTLS up)");
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
