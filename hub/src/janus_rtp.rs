//! The Janus audiobridge plain-RTP participant adapter (issue 1345 M3a).
//!
//! Three layers, split so the wire-format + protocol logic verifies pure (Tier-0 #557: CI is the
//! first compile, but the RTP header bytes + the Janus JSON shapes are pinned by a rustc `--test`
//! replica locally):
//!
//! (1) RTP — a 12-byte-header packetizer/depacketizer (a fixed SSRC + payload type per run, seq +
//! timestamp continuity, the marker bit only on the first packet after silence) and the
//! [`JanusCodec`] choice (Opus with in-band FEC by default, PCMU selectable; issue 1345, 25.9.2026).
//! (2) Janus HTTP API messages — pure `serde_json` builders (`create` session, `attach`
//! `janus.plugin.audiobridge`, `join` as a plain-RTP participant, `configure`, `keepalive`, `leave`)
//! plus parsers for the `success` id, the audiobridge `joined` reply (Janus's own RTP ip/port and
//! payload type — where we send + receive) and the transport/plugin `error` shapes. Field names are
//! pinned to the Janus AudioBridge docs' plain-RTP participant section.
//! (3) The adapter runtime — [`run_janus_participant`] drives the HTTP long-poll session (`reqwest`,
//! rustls, NO native openssl), re-creating the session on ANY error with a bounded backoff. It binds
//! the RTP socket and starts the paced sender thread ([`crate::janus_sender`]), which sends the
//! phones' N-1 mix on its own steady 20 ms clock. It decodes the received room mix
//! ([`crate::janus_codec`]) into the phones participant's [`JitterBuffer`] the engine pops like any
//! input.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::janus_codec::{RxDecoder, TxEncoder};
use crate::janus_pacing::{IntervalStats, PacedRing};
use crate::janus_sender::{spawn_paced_sender, PacedSenderConfig, PacedSenderShared, TxTarget};
use crate::vban_io::{DecodedAudio, JitterBuffer};

// ---------------------------------------------------------------------------------------------
// 1. RTP (PCMU + Opus)
// ---------------------------------------------------------------------------------------------

/// The fixed RTP header length (no CSRC, no extension).
pub const RTP_HEADER_LEN: usize = 12;
/// The RTP payload type for PCMU (G.711 µ-law), per RFC 3551.
pub const PCMU_PAYLOAD_TYPE: u8 = 0;
/// Samples (= µ-law bytes) in one 20 ms PCMU packet at 8 kHz.
pub const PCMU_SAMPLES_PER_PACKET: usize = 160;
/// The dynamic RTP payload type the hub asks Janus to use for Opus (issue 1345, 25.9.2026). Janus
/// echoes the payload type it will send the room mix with in its `joined` reply.
pub const OPUS_PAYLOAD_TYPE: u8 = 111;

/// The phones leg's codec (`[janus].codec`). Opus is the default (issue 1345, 25.9.2026: the
/// narrowband PCMU leg sounded like a telephone on top of the pacing beat). PCMU stays selectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JanusCodec {
    /// Opus 48 kHz mono, 20 ms, in-band FEC.
    #[default]
    Opus,
    /// G.711 µ-law, 8 kHz.
    Pcmu,
}

impl JanusCodec {
    /// The name Janus takes in the join's top-level `codec` field (and `/api/state` shows).
    pub fn as_str(self) -> &'static str {
        match self {
            JanusCodec::Opus => "opus",
            JanusCodec::Pcmu => "pcmu",
        }
    }

    /// The RTP payload type the hub sends (and asks Janus to send) for this codec.
    pub fn payload_type(self) -> u8 {
        match self {
            JanusCodec::Opus => OPUS_PAYLOAD_TYPE,
            JanusCodec::Pcmu => PCMU_PAYLOAD_TYPE,
        }
    }

    /// One 20 ms frame in RTP clock units: Opus always uses a 48 kHz RTP clock (RFC 7587), PCMU
    /// 8 kHz.
    pub fn rtp_samples_per_frame(self) -> u32 {
        match self {
            JanusCodec::Opus => 960,
            JanusCodec::Pcmu => PCMU_SAMPLES_PER_PACKET as u32,
        }
    }
}

/// An RTP packetizer: a fixed SSRC and payload type per run, a 16-bit sequence number and a 32-bit
/// timestamp that advance by the sample count of each packet, and the marker bit set only on the
/// first packet emitted after (re)start or a silence gap.
#[derive(Debug, Clone)]
pub struct RtpPacketizer {
    ssrc: u32,
    payload_type: u8,
    seq: u16,
    timestamp: u32,
    marker_next: bool,
}

impl RtpPacketizer {
    /// A PCMU packetizer with a fixed SSRC; the first packet it emits carries the marker bit (the
    /// first packet after start-up silence).
    pub fn new(ssrc: u32) -> Self {
        Self::with_payload_type(ssrc, PCMU_PAYLOAD_TYPE)
    }

    /// A packetizer for any payload type (Opus: [`OPUS_PAYLOAD_TYPE`]).
    pub fn with_payload_type(ssrc: u32, payload_type: u8) -> Self {
        RtpPacketizer {
            ssrc,
            payload_type: payload_type & 0x7F,
            seq: 0,
            timestamp: 0,
            marker_next: true,
        }
    }

    /// Arm the marker bit for the next packet (call after a silence gap so the receiver resyncs).
    pub fn mark_silence(&mut self) {
        self.marker_next = true;
    }

    /// Build one PCMU packet: the timestamp advances by the payload length (1 byte = 1 sample).
    pub fn packetize(&mut self, payload: &[u8]) -> Vec<u8> {
        self.packetize_samples(payload, payload.len() as u32)
    }

    /// Build one RTP packet (12-byte header + `payload`), advancing the sequence number by one and
    /// the timestamp by `samples` RTP clock units (an Opus packet always advances by the frame
    /// duration, whatever its byte size). The marker bit is set only on the first packet after
    /// start-up / [`mark_silence`](Self::mark_silence).
    pub fn packetize_samples(&mut self, payload: &[u8], samples: u32) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
        pkt.push(0x80); // V=2, P=0, X=0, CC=0
        let marker = if self.marker_next { 0x80 } else { 0x00 };
        pkt.push(marker | self.payload_type);
        pkt.extend_from_slice(&self.seq.to_be_bytes());
        pkt.extend_from_slice(&self.timestamp.to_be_bytes());
        pkt.extend_from_slice(&self.ssrc.to_be_bytes());
        pkt.extend_from_slice(payload);

        self.marker_next = false;
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(samples);
        pkt
    }

    /// Account for a frame that is not sent (it could not be encoded): the sequence number and the
    /// timestamp move on, so the receiver sees one lost packet and RTP time keeps pace with the
    /// wall clock. The marker bit is not re-armed — this is not a new talkspurt.
    pub fn skip(&mut self, samples: u32) {
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(samples);
    }
}

/// Whether a datagram from `from` belongs to the current session's Janus endpoint `peer`. An old
/// session's leftovers (another port) and any other host are ignored, so they cannot feed or lock
/// out the room mix. When Janus advertised an unspecified address only the port is compared.
pub fn is_session_peer(from: SocketAddr, peer: SocketAddr) -> bool {
    from.port() == peer.port() && (peer.ip().is_unspecified() || from.ip() == peer.ip())
}

/// A parsed RTP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpParsed {
    pub marker: bool,
    pub payload_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

/// Parse an RTP packet's fixed 12-byte header + payload. `None` for a runt (< 12 bytes) or a version
/// other than 2 — a foreign/garbage datagram is ignored, exactly like the VBAN receiver.
pub fn rtp_depacketize(data: &[u8]) -> Option<RtpParsed> {
    if data.len() < RTP_HEADER_LEN {
        return None;
    }
    if data[0] >> 6 != 2 {
        return None; // not RTP version 2
    }
    let marker = data[1] & 0x80 != 0;
    let payload_type = data[1] & 0x7F;
    let seq = u16::from_be_bytes([data[2], data[3]]);
    let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    Some(RtpParsed {
        marker,
        payload_type,
        seq,
        timestamp,
        ssrc,
        payload: data[RTP_HEADER_LEN..].to_vec(),
    })
}

// ---------------------------------------------------------------------------------------------
// 2. Janus HTTP API messages (pure serde_json)
// ---------------------------------------------------------------------------------------------

/// The audiobridge plugin package name.
pub const AUDIOBRIDGE_PLUGIN: &str = "janus.plugin.audiobridge";

/// `create` a Janus session.
pub fn build_create(transaction: &str) -> Value {
    json!({ "janus": "create", "transaction": transaction })
}

/// `attach` the audiobridge plugin (POSTed to the session URL).
pub fn build_attach(transaction: &str) -> Value {
    json!({ "janus": "attach", "plugin": AUDIOBRIDGE_PLUGIN, "transaction": transaction })
}

/// What the hub asks for when it joins the audiobridge room as a plain-RTP participant.
#[derive(Debug, Clone, Copy)]
pub struct JoinSpec<'a> {
    pub room: u64,
    /// The `display` name shown in the room.
    pub display: &'a str,
    /// Our RTP address, advertised to Janus.
    pub local_ip: &'a str,
    pub local_port: u16,
    pub codec: JanusCodec,
    /// The room secret; NEVER logged.
    pub secret: Option<&'a str>,
    pub pin: Option<&'a str>,
}

/// `join` an audiobridge room as a plain-RTP participant. Janus answers with its OWN rtp ip/port
/// (where we then send our audio + receive the room mix minus ourselves). `secret`/`pin` are added
/// to the body only when present.
pub fn build_join(transaction: &str, spec: &JoinSpec<'_>) -> Value {
    let mut rtp = json!({
        "ip": spec.local_ip,
        "port": spec.local_port,
        "payload_type": spec.codec.payload_type(),
    });
    if spec.codec == JanusCodec::Opus {
        // Ask Janus to put in-band FEC into the room mix it sends us too.
        rtp["fec"] = json!(true);
    }
    let mut body = json!({
        "request": "join",
        "room": spec.room,
        "display": spec.display,
        // The plain-RTP leg's codec is selected HERE (top-level `codec`), not by `rtp.payload_type`:
        // without it Janus 1.1.2 defaults the participant to Opus and discards PCMU (live strih-lx
        // finding, 19.9.2026).
        "codec": spec.codec.as_str(),
        "rtp": rtp,
    });
    if let Some(s) = spec.secret {
        body["secret"] = json!(s);
    }
    if let Some(p) = spec.pin {
        body["pin"] = json!(p);
    }
    json!({ "janus": "message", "transaction": transaction, "body": body })
}

/// `configure` the participant (unmuted for talkback; the phone leg mixes into the room).
pub fn build_configure(transaction: &str, muted: bool) -> Value {
    json!({
        "janus": "message",
        "transaction": transaction,
        "body": { "request": "configure", "muted": muted },
    })
}

/// A session-level `keepalive` (POSTed to the session URL, < 60 s apart or Janus drops the session).
pub fn build_keepalive(transaction: &str) -> Value {
    json!({ "janus": "keepalive", "transaction": transaction })
}

/// `destroy` the session (POSTed to the session URL): Janus detaches its handles, so the old
/// participant leaves the room and stops sending its mix to our port.
pub fn build_destroy(transaction: &str) -> Value {
    json!({ "janus": "destroy", "transaction": transaction })
}

/// `leave` the room (POSTed to the handle URL).
pub fn build_leave(transaction: &str) -> Value {
    json!({
        "janus": "message",
        "transaction": transaction,
        "body": { "request": "leave" },
    })
}

/// The `id` from a `{"janus":"success","data":{"id":N}}` create/attach reply.
pub fn parse_success_id(v: &Value) -> Option<u64> {
    if v.get("janus")?.as_str()? != "success" {
        return None;
    }
    v.get("data")?.get("id")?.as_u64()
}

/// Janus's own RTP endpoint from an audiobridge `joined` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedInfo {
    pub room: u64,
    pub id: u64,
    pub rtp_ip: String,
    pub rtp_port: u16,
    /// The payload type Janus will send the room mix with, when the reply names one.
    pub payload_type: Option<u8>,
}

/// Parse the audiobridge `joined` event: `plugindata.data.audiobridge == "joined"` carrying the
/// plugin's own `rtp` ip/port (the address to send our audio to + receive the room mix from), its
/// payload type, the participant `id` and the `room`. `None` for any other event shape.
pub fn parse_joined(v: &Value) -> Option<JoinedInfo> {
    let data = v.get("plugindata")?.get("data")?;
    if data.get("audiobridge")?.as_str()? != "joined" {
        return None;
    }
    let rtp = data.get("rtp")?;
    Some(JoinedInfo {
        room: data.get("room")?.as_u64()?,
        id: data.get("id")?.as_u64()?,
        rtp_ip: rtp.get("ip")?.as_str()?.to_string(),
        rtp_port: u16::try_from(rtp.get("port")?.as_u64()?).ok()?,
        payload_type: rtp
            .get("payload_type")
            .and_then(Value::as_u64)
            .and_then(|pt| u8::try_from(pt).ok()),
    })
}

/// A Janus error (transport-level `janus:"error"` or the audiobridge plugin's `error_code`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JanusApiError {
    pub code: i64,
    pub reason: String,
}

/// Parse either error shape: the transport `{"janus":"error","error":{"code","reason"}}` or the
/// audiobridge plugin `{"plugindata":{"data":{"audiobridge":"event","error_code":N,"error":"…"}}}`.
pub fn parse_error(v: &Value) -> Option<JanusApiError> {
    // Transport-level error.
    if v.get("janus").and_then(|j| j.as_str()) == Some("error") {
        let e = v.get("error")?;
        return Some(JanusApiError {
            code: e.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
            reason: e
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    // Plugin-level error inside a plugindata event.
    if let Some(data) = v.get("plugindata").and_then(|p| p.get("data")) {
        if let Some(code) = data.get("error_code").and_then(|c| c.as_i64()) {
            return Some(JanusApiError {
                code,
                reason: data
                    .get("error")
                    .and_then(|r| r.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// 3. The adapter runtime
// ---------------------------------------------------------------------------------------------

/// Resolved runtime parameters for the Janus adapter (built from `[janus]` in the matrix + the
/// secret read from its 0600 file).
#[derive(Debug, Clone)]
pub struct JanusRuntimeConfig {
    pub api_url: String,
    pub room: u64,
    /// The room secret (read from `room_secret_file`); NEVER logged.
    pub secret: Option<String>,
    /// The local UDP address our audio is sent from + the room mix is received on.
    pub rtp_bind: SocketAddr,
    /// The `display` name shown in the room.
    pub display: String,
    /// The phones leg's codec (`[janus].codec`).
    pub codec: JanusCodec,
}

/// A live-established Janus session: the ids, the plugin's RTP endpoint (where we send / receive)
/// and the payload type the room mix arrives with.
#[derive(Debug, Clone)]
pub struct JanusSession {
    pub session_id: u64,
    pub handle_id: u64,
    pub janus_rtp_addr: SocketAddr,
    pub payload_type: u8,
}

/// Per-participant Janus counters, shared with the `/api/state` snapshot (atomics so the paced
/// sender thread, the recv task and the HTTP layer read them without a lock).
#[derive(Debug, Default)]
pub struct JanusSharedStats {
    codec: JanusCodec,
    joined: AtomicBool,
    rejoin_count: AtomicU64,
    rx_packets: AtomicU64,
    tx_packets: AtomicU64,
    /// The spacing of the last sends, in µs (the pacing proof).
    tx_interval_sd_us: AtomicU64,
    tx_interval_max_us: AtomicU64,
    tx_underflows: AtomicU64,
    tx_overflow_trims: AtomicU64,
    rx_lost_frames: AtomicU64,
    /// Wall-clock start of the current session, for `session_age_s` (None = not joined).
    session_started: Mutex<Option<Instant>>,
}

/// The serialized Janus facet added to `/api/state` for the janus participant.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct JanusStats {
    pub joined: bool,
    pub session_age_s: u64,
    pub rejoin_count: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    /// The phones leg's codec: `opus` or `pcmu`.
    pub codec: &'static str,
    /// The standard deviation of the last 5 s of send intervals (paced: well under 0.5 ms). It
    /// reads 0 while no session is joined (nothing is sent), so read it together with `joined`.
    pub tx_interval_ms_sd: f64,
    /// The longest send interval in the last 5 s (paced: just over 20 ms; 0 while not joined).
    pub tx_interval_ms_max: f64,
    /// Ticks bridged with a silent frame because the ring ran dry mid-stream.
    pub tx_underflows: u64,
    /// Times the ring overflowed and was trimmed back to its target.
    pub tx_overflow_trims: u64,
    /// Received frames that were lost on the wire and concealed (FEC/PLC).
    pub rx_lost_frames: u64,
}

fn ms_to_us(ms: f64) -> u64 {
    (ms * 1000.0).round().max(0.0) as u64
}

impl JanusSharedStats {
    /// Counters for a leg running `codec`.
    pub fn new(codec: JanusCodec) -> Self {
        JanusSharedStats {
            codec,
            ..Default::default()
        }
    }

    /// A point-in-time snapshot for `/api/state`.
    pub fn snapshot(&self) -> JanusStats {
        let session_age_s = self
            .session_started
            .lock()
            .ok()
            .and_then(|g| *g)
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        JanusStats {
            joined: self.joined.load(Ordering::Relaxed),
            session_age_s,
            rejoin_count: self.rejoin_count.load(Ordering::Relaxed),
            rx_packets: self.rx_packets.load(Ordering::Relaxed),
            tx_packets: self.tx_packets.load(Ordering::Relaxed),
            codec: self.codec.as_str(),
            tx_interval_ms_sd: self.tx_interval_sd_us.load(Ordering::Relaxed) as f64 / 1000.0,
            tx_interval_ms_max: self.tx_interval_max_us.load(Ordering::Relaxed) as f64 / 1000.0,
            tx_underflows: self.tx_underflows.load(Ordering::Relaxed),
            tx_overflow_trims: self.tx_overflow_trims.load(Ordering::Relaxed),
            rx_lost_frames: self.rx_lost_frames.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn count_tx(&self) {
        self.tx_packets.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn publish_ring(&self, underflows: u64, trims: u64) {
        self.tx_underflows.store(underflows, Ordering::Relaxed);
        self.tx_overflow_trims.store(trims, Ordering::Relaxed);
    }

    pub(crate) fn publish_intervals(&self, intervals: &IntervalStats) {
        self.tx_interval_sd_us
            .store(ms_to_us(intervals.sd_ms()), Ordering::Relaxed);
        self.tx_interval_max_us
            .store(ms_to_us(intervals.max_ms()), Ordering::Relaxed);
    }

    fn mark_joined(&self) {
        self.joined.store(true, Ordering::Relaxed);
        if let Ok(mut g) = self.session_started.lock() {
            *g = Some(Instant::now());
        }
    }

    fn mark_left(&self) {
        self.joined.store(false, Ordering::Relaxed);
        if let Ok(mut g) = self.session_started.lock() {
            *g = None;
        }
    }
}

/// The minimum + maximum re-establish backoff (1 s → 30 s) after any session error.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Send one keepalive well under Janus's 60 s session timeout.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(30);
/// How often the session task looks at the paced sender's send-failed flag.
const SEND_FAILED_CHECK_EVERY: Duration = Duration::from_secs(1);

/// Establish a Janus audiobridge session as a plain-RTP participant: `create` → `attach` → `join`
/// (advertising our address + codec) → parse the plugin's own rtp ip/port + payload type. Errors
/// bubble up so the caller backs off and re-establishes.
pub async fn establish_session(
    client: &reqwest::Client,
    api_url: &str,
    spec: &JoinSpec<'_>,
) -> Result<JanusSession> {
    // create
    let created: Value = client
        .post(api_url)
        .json(&build_create("create"))
        .send()
        .await
        .context("janus create POST")?
        .json()
        .await
        .context("janus create decode")?;
    if let Some(e) = parse_error(&created) {
        return Err(anyhow!("janus create error {}: {}", e.code, e.reason));
    }
    let session_id =
        parse_success_id(&created).ok_or_else(|| anyhow!("janus create: no session id"))?;

    // attach
    let session_url = format!("{}/{}", api_url.trim_end_matches('/'), session_id);
    let attached: Value = client
        .post(&session_url)
        .json(&build_attach("attach"))
        .send()
        .await
        .context("janus attach POST")?
        .json()
        .await
        .context("janus attach decode")?;
    if let Some(e) = parse_error(&attached) {
        return Err(anyhow!("janus attach error {}: {}", e.code, e.reason));
    }
    let handle_id =
        parse_success_id(&attached).ok_or_else(|| anyhow!("janus attach: no handle id"))?;

    // join (plain-RTP participant)
    let handle_url = format!("{session_url}/{handle_id}");
    let join_resp: Value = client
        .post(&handle_url)
        .json(&build_join("join", spec))
        .send()
        .await
        .context("janus join POST")?
        .json()
        .await
        .context("janus join decode")?;
    if let Some(e) = parse_error(&join_resp) {
        return Err(anyhow!("janus join error {}: {}", e.code, e.reason));
    }
    // The join reply may inline the `joined` event, or Janus may `ack` and deliver it on the session
    // long-poll. Try the inline reply first, then one bounded long-poll GET.
    let joined = match parse_joined(&join_resp) {
        Some(j) => j,
        None => poll_for_joined(client, &session_url).await?,
    };

    let janus_rtp_addr: SocketAddr = format!("{}:{}", joined.rtp_ip, joined.rtp_port)
        .parse()
        .with_context(|| format!("janus rtp addr {}:{}", joined.rtp_ip, joined.rtp_port))?;
    Ok(JanusSession {
        session_id,
        handle_id,
        janus_rtp_addr,
        payload_type: joined
            .payload_type
            .unwrap_or_else(|| spec.codec.payload_type()),
    })
}

/// Long-poll the session URL (bounded) until a `joined` event arrives.
async fn poll_for_joined(client: &reqwest::Client, session_url: &str) -> Result<JoinedInfo> {
    for _ in 0..10 {
        let ev: Value = client
            .get(session_url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("janus long-poll GET")?
            .json()
            .await
            .context("janus long-poll decode")?;
        if let Some(e) = parse_error(&ev) {
            return Err(anyhow!("janus join event error {}: {}", e.code, e.reason));
        }
        if let Some(j) = parse_joined(&ev) {
            return Ok(j);
        }
    }
    Err(anyhow!("janus: no joined event after long-poll"))
}

/// Keep the session alive until Janus reports it gone. A keepalive that returns a Janus error body
/// (e.g. "No such session", HTTP 200) is how a Janus-side teardown is detected within one period.
/// Runs as its own task, so a slow POST never holds up the receive side.
async fn keepalive_until_error(client: reqwest::Client, session_url: String) -> anyhow::Error {
    let mut every = tokio::time::interval(KEEPALIVE_EVERY);
    every.tick().await; // the first tick fires immediately
    loop {
        every.tick().await;
        let resp = match client
            .post(&session_url)
            .json(&build_keepalive("keepalive"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return anyhow!("keepalive POST: {e}"),
        };
        match resp.json::<Value>().await {
            Ok(v) => {
                if let Some(e) = parse_error(&v) {
                    return anyhow!("keepalive error {}: {}", e.code, e.reason);
                }
            }
            Err(e) => return anyhow!("keepalive decode: {e}"),
        }
    }
}

/// POST `configure` (unmuted). A transport error OR a Janus error in the response body (Janus can
/// answer HTTP 200 with `{"janus":"error",…}`) is an error, so the caller re-establishes WITH the
/// bounded backoff.
async fn configure_unmuted(client: &reqwest::Client, handle_url: &str) -> Result<()> {
    let resp = client
        .post(handle_url)
        .json(&build_configure("configure", false))
        .send()
        .await
        .context("configure POST")?;
    let v: Value = resp.json().await.context("configure decode")?;
    match parse_error(&v) {
        Some(e) => Err(anyhow!("configure error {}: {}", e.code, e.reason)),
        None => Ok(()),
    }
}

/// How long the best-effort `destroy` of an abandoned session may take.
const DESTROY_TIMEOUT: Duration = Duration::from_secs(2);

/// Best-effort `destroy` of a session the hub is abandoning, so its participant leaves the room
/// and stops sending the old room mix to our port. A failure is only logged (Janus drops an
/// abandoned session on its own after 60 s without a keepalive).
async fn destroy_session(client: &reqwest::Client, session_url: &str) {
    let sent = client
        .post(session_url)
        .timeout(DESTROY_TIMEOUT)
        .json(&build_destroy("destroy"))
        .send()
        .await;
    if let Err(e) = sent {
        tracing::debug!(error = %e, "janus: destroy of the old session failed (Janus times it out)");
    }
}

/// Everything the adapter task needs besides its config (grouped to keep the spawn short).
pub struct JanusAdapterIo {
    /// The ring the block loop feeds with the phones' N-1 mix (48 kHz mono).
    pub ring: Arc<Mutex<PacedRing>>,
    /// The jitter buffers; the room mix is pushed into `phones_id`'s.
    pub jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    pub phones_id: usize,
    pub stats: Arc<JanusSharedStats>,
}

/// The re-establishing adapter task. It binds the RTP socket, starts the paced sender thread
/// ([`crate::janus_sender`]) on a clone of it, then keeps a Janus session alive: it points the
/// sender at each new session, decodes the received room mix into the phones jitter buffer, and
/// re-joins (with a bounded backoff) on any error.
pub async fn run_janus_participant(cfg: JanusRuntimeConfig, ssrc: u32, io: JanusAdapterIo) {
    let JanusAdapterIo {
        ring,
        jitter,
        phones_id,
        stats,
    } = io;
    let (socket, send_socket) = match bind_rtp_sockets(cfg.rtp_bind) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(bind = %cfg.rtp_bind, error = %e, "janus: cannot bind the RTP socket — adapter disabled");
            return;
        }
    };
    let local_ip = cfg.rtp_bind.ip().to_string();
    let local_port = socket
        .local_addr()
        .map(|a| a.port())
        .unwrap_or_else(|_| cfg.rtp_bind.port());
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(35))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "janus: cannot build the HTTP client — adapter disabled");
            return;
        }
    };
    let mut decoder = match RxDecoder::new(cfg.codec) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(codec = cfg.codec.as_str(), error = %e, "janus: cannot create the decoder — adapter disabled");
            return;
        }
    };
    let encoder = match TxEncoder::new(cfg.codec) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(codec = cfg.codec.as_str(), error = %e, "janus: cannot create the encoder — adapter disabled");
            return;
        }
    };
    let shared = Arc::new(PacedSenderShared::default());
    if let Err(e) = spawn_paced_sender(
        PacedSenderConfig {
            socket: send_socket,
            codec: cfg.codec,
            ssrc,
            encoder,
        },
        ring,
        shared.clone(),
        stats.clone(),
    ) {
        tracing::error!(error = %e, "janus: cannot start the paced sender thread — adapter disabled");
        return;
    }

    let spec = JoinSpec {
        room: cfg.room,
        display: &cfg.display,
        local_ip: &local_ip,
        local_port,
        codec: cfg.codec,
        secret: cfg.secret.as_deref(),
        pin: None,
    };
    let mut backoff = BACKOFF_MIN;
    let mut session_number: u64 = 0;
    let mut recv_buf = vec![0u8; 4096];

    loop {
        let session = match establish_session(&client, &cfg.api_url, &spec).await {
            Ok(s) => s,
            Err(e) => {
                stats.mark_left();
                stats.rejoin_count.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(room = cfg.room, error = %e, backoff_s = backoff.as_secs(), "janus: session establish failed — backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        let session_url = format!(
            "{}/{}",
            cfg.api_url.trim_end_matches('/'),
            session.session_id
        );
        let handle_url = format!("{session_url}/{}", session.handle_id);
        // A bare `continue` on a failed configure would bypass the backoff and, if configure alone
        // keeps failing while create/attach/join succeed, spin at RTT and orphan a session each pass.
        if let Err(e) = configure_unmuted(&client, &handle_url).await {
            tracing::warn!(error = %e, backoff_s = backoff.as_secs(), "janus: configure failed — backing off");
            destroy_session(&client, &session_url).await;
            stats.mark_left();
            stats.rejoin_count.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
            continue;
        }

        tracing::info!(room = cfg.room, session = session.session_id, janus_rtp = %session.janus_rtp_addr, codec = cfg.codec.as_str(), payload_type = session.payload_type, "janus: joined the audiobridge room");
        stats.mark_joined();
        backoff = BACKOFF_MIN;
        session_number += 1;
        if let Err(e) = decoder.reset() {
            tracing::warn!(error = %e, "janus: decoder reset failed");
        }
        shared.set_target(Some(TxTarget {
            addr: session.janus_rtp_addr,
            session: session_number,
        }));
        let mut keepalive =
            tokio::spawn(keepalive_until_error(client.clone(), session_url.clone()));
        let mut send_check = tokio::time::interval(SEND_FAILED_CHECK_EVERY);

        // Inner I/O loop until any error forces a re-establish.
        loop {
            tokio::select! {
                // The room mix minus ourselves: decode + push to the phones jitter buffer.
                recvd = socket.recv_from(&mut recv_buf) => {
                    match recvd {
                        // Only this session's Janus endpoint: an abandoned session's leftovers or a
                        // stray datagram must not feed (or lock out) the room mix.
                        Ok((len, from)) if is_session_peer(from, session.janus_rtp_addr) => {
                            if let Some(rtp) = rtp_depacketize(&recv_buf[..len]) {
                                if rtp.payload_type == session.payload_type && !rtp.payload.is_empty() {
                                    push_room_mix(&mut decoder, &rtp, &jitter, phones_id, &stats);
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => { tracing::warn!(error = %e, "janus: RTP recv failed — re-establishing"); break; }
                    }
                }
                ended = &mut keepalive => {
                    match ended {
                        Ok(e) => tracing::warn!(error = %e, "janus: keepalive failed — re-establishing"),
                        Err(e) => tracing::warn!(error = %e, "janus: keepalive task ended — re-establishing"),
                    }
                    break;
                }
                _ = send_check.tick() => {
                    if shared.take_send_failed() {
                        break;
                    }
                }
            }
        }

        keepalive.abort();
        shared.set_target(None);
        destroy_session(&client, &session_url).await;
        stats.mark_left();
        stats.rejoin_count.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Bind the RTP socket and clone it: the async receiver gets a non-blocking tokio socket, the paced
/// sender thread gets the clone, so Janus sees ONE address for both directions.
fn bind_rtp_sockets(bind: SocketAddr) -> std::io::Result<(tokio::net::UdpSocket, UdpSocket)> {
    let std_socket = UdpSocket::bind(bind)?;
    let send_socket = std_socket.try_clone()?;
    std_socket.set_nonblocking(true)?;
    Ok((tokio::net::UdpSocket::from_std(std_socket)?, send_socket))
}

/// Decode one received packet and push the audio. It is mono: the phones jitter buffer (built with
/// `with_min_channels(2)`) fans it out to both input channels.
fn push_room_mix(
    decoder: &mut RxDecoder,
    rtp: &RtpParsed,
    jitter: &Mutex<Vec<JitterBuffer>>,
    phones_id: usize,
    stats: &JanusSharedStats,
) {
    decoder.observe_ssrc(rtp.ssrc);
    let decoded = match decoder.decode(rtp.seq, &rtp.payload) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(seq = rtp.seq, error = %e, "janus: undecodable packet dropped");
            return;
        }
    };
    if decoded.samples.is_empty() {
        return; // a duplicate or late packet
    }
    let frames = decoded.samples.len();
    let audio = DecodedAudio {
        stream_name: "janus-phones".to_string(),
        channels: vec![decoded.samples],
        frames,
    };
    if let Ok(mut jb) = jitter.lock() {
        if let Some(b) = jb.get_mut(phones_id) {
            b.push(&audio);
        }
    }
    stats.rx_packets.fetch_add(1, Ordering::Relaxed);
    stats
        .rx_lost_frames
        .fetch_add(u64::from(decoded.concealed_frames), Ordering::Relaxed);
}
