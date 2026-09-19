//! The Janus audiobridge plain-RTP participant adapter (issue 1345 M3a).
//!
//! Three layers, split so the wire-format + protocol logic verifies pure (Tier-0 #557: CI is the
//! first compile, but the RTP header bytes + the Janus JSON shapes are pinned by a rustc `--test`
//! replica locally):
//!
//! (1) RTP PCMU — a 12-byte-header packetizer/depacketizer (PT 0, 160 samples / 20 ms, a fixed
//! SSRC per run, seq + timestamp continuity, the marker bit only on the first packet after silence).
//! (2) Janus HTTP API messages — pure `serde_json` builders (`create` session, `attach`
//! `janus.plugin.audiobridge`, `join` as a plain-RTP participant, `configure`, `keepalive`, `leave`)
//! plus parsers for the `success` id, the audiobridge `joined` reply (Janus's own RTP ip/port — where
//! we send + receive) and the transport/plugin `error` shapes. Field names are pinned to the Janus
//! AudioBridge docs' plain-RTP participant section.
//! (3) The adapter runtime — [`run_janus_participant`] drives the HTTP long-poll session (`reqwest`,
//! rustls, NO native openssl), re-creating the session on ANY error with a bounded backoff, binds a
//! UDP socket, sends the phones' N-1 mix as PCMU (via [`crate::mulaw`]) every 20 ms and pushes the
//! received room mix into the phones participant's [`JitterBuffer`] the engine pops like any input.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::mulaw;
use crate::vban_io::{DecodedAudio, JitterBuffer};

// ---------------------------------------------------------------------------------------------
// 1. RTP PCMU
// ---------------------------------------------------------------------------------------------

/// The fixed RTP header length (no CSRC, no extension).
pub const RTP_HEADER_LEN: usize = 12;
/// The RTP payload type for PCMU (G.711 µ-law), per RFC 3551.
pub const PCMU_PAYLOAD_TYPE: u8 = 0;
/// Samples (= µ-law bytes) in one 20 ms PCMU packet at 8 kHz.
pub const PCMU_SAMPLES_PER_PACKET: usize = 160;

/// An RTP PCMU packetizer: a fixed SSRC per run, a 16-bit sequence number and a 32-bit timestamp
/// that advance by the sample count of each packet, and the marker bit set only on the first packet
/// emitted after (re)start or a silence gap.
#[derive(Debug, Clone)]
pub struct RtpPacketizer {
    ssrc: u32,
    seq: u16,
    timestamp: u32,
    marker_next: bool,
}

impl RtpPacketizer {
    /// A packetizer with a fixed SSRC; the first packet it emits carries the marker bit (the first
    /// packet after start-up silence).
    pub fn new(ssrc: u32) -> Self {
        RtpPacketizer {
            ssrc,
            seq: 0,
            timestamp: 0,
            marker_next: true,
        }
    }

    /// Arm the marker bit for the next packet (call after a silence gap so the receiver resyncs).
    pub fn mark_silence(&mut self) {
        self.marker_next = true;
    }

    /// Build one RTP packet (12-byte header + `payload`), advancing the sequence number by one and
    /// the timestamp by the payload's sample count (1 byte = 1 sample for PCMU). The marker bit is
    /// set only on the first packet after start-up / [`mark_silence`](Self::mark_silence).
    pub fn packetize(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
        pkt.push(0x80); // V=2, P=0, X=0, CC=0
        let marker = if self.marker_next { 0x80 } else { 0x00 };
        pkt.push(marker | (PCMU_PAYLOAD_TYPE & 0x7F));
        pkt.extend_from_slice(&self.seq.to_be_bytes());
        pkt.extend_from_slice(&self.timestamp.to_be_bytes());
        pkt.extend_from_slice(&self.ssrc.to_be_bytes());
        pkt.extend_from_slice(payload);

        self.marker_next = false;
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(payload.len() as u32);
        pkt
    }
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

/// `join` an audiobridge room as a plain-RTP participant. Janus answers with its OWN rtp ip/port
/// (where we then send our µ-law + receive the room mix minus ourselves). `secret`/`pin` are added
/// to the body only when present.
pub fn build_join(
    transaction: &str,
    room: u64,
    display: &str,
    local_ip: &str,
    local_port: u16,
    secret: Option<&str>,
    pin: Option<&str>,
) -> Value {
    let mut body = json!({
        "request": "join",
        "room": room,
        "display": display,
        "rtp": {
            "ip": local_ip,
            "port": local_port,
            "payload_type": PCMU_PAYLOAD_TYPE,
        },
    });
    if let Some(s) = secret {
        body["secret"] = json!(s);
    }
    if let Some(p) = pin {
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
}

/// Parse the audiobridge `joined` event: `plugindata.data.audiobridge == "joined"` carrying the
/// plugin's own `rtp` ip/port (the address to send our PCMU to + receive the room mix from), the
/// participant `id` and the `room`. `None` for any other event shape.
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
    /// The local UDP address our PCMU is sent from + the room mix is received on.
    pub rtp_bind: SocketAddr,
    /// The `display` name shown in the room.
    pub display: String,
}

/// A live-established Janus session: the ids + the plugin's RTP endpoint (where we send / receive).
#[derive(Debug, Clone)]
pub struct JanusSession {
    pub session_id: u64,
    pub handle_id: u64,
    pub janus_rtp_addr: SocketAddr,
}

/// Per-participant Janus counters, shared with the `/api/state` snapshot (atomics so the block loop,
/// the recv task and the HTTP layer read them without a lock).
#[derive(Debug, Default)]
pub struct JanusSharedStats {
    joined: AtomicBool,
    rejoin_count: AtomicU64,
    rx_packets: AtomicU64,
    tx_packets: AtomicU64,
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
}

impl JanusSharedStats {
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
        }
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
/// 20 ms of 48 kHz mono samples = one PCMU packet's worth before down-sampling.
const MIX_SAMPLES_PER_PACKET_48K: usize = 960;

/// Establish a Janus audiobridge session as a plain-RTP participant: `create` → `attach` → `join`
/// (advertising our `local_ip:local_port`) → parse the plugin's own rtp ip/port. Returns the session
/// ids + Janus's RTP address. Errors bubble up so the caller backs off and re-establishes.
pub async fn establish_session(
    client: &reqwest::Client,
    api_url: &str,
    room: u64,
    display: &str,
    local_ip: &str,
    local_port: u16,
    secret: Option<&str>,
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
        .json(&build_join(
            "join", room, display, local_ip, local_port, secret, None,
        ))
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

/// The re-establishing adapter task: bind the UDP socket, establish the session, then send the
/// phones' N-1 mix as PCMU every 20 ms and push the received room mix into the phones jitter buffer,
/// keeping the session alive and re-joining (with a bounded backoff) on any error.
///
/// `mix_rx` carries the phones participant's mixed INTERLEAVED STEREO 48 kHz output, one block per
/// hub tick; the task down-mixes + accumulates to 20 ms PCMU packets.
pub async fn run_janus_participant(
    cfg: JanusRuntimeConfig,
    ssrc: u32,
    mut mix_rx: tokio::sync::mpsc::Receiver<Vec<i16>>,
    jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    phones_id: usize,
    stats: Arc<JanusSharedStats>,
) {
    let socket = match tokio::net::UdpSocket::bind(cfg.rtp_bind).await {
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

    let mut backoff = BACKOFF_MIN;
    let mut packetizer = RtpPacketizer::new(ssrc);
    let mut pending_48k: Vec<i16> = Vec::with_capacity(MIX_SAMPLES_PER_PACKET_48K * 2);
    let mut recv_buf = vec![0u8; 4096];

    loop {
        let session = match establish_session(
            &client,
            &cfg.api_url,
            cfg.room,
            &cfg.display,
            &local_ip,
            local_port,
            cfg.secret.as_deref(),
        )
        .await
        {
            Ok(s) => {
                tracing::info!(room = cfg.room, session = s.session_id, janus_rtp = %s.janus_rtp_addr, "janus: joined the audiobridge room");
                stats.mark_joined();
                backoff = BACKOFF_MIN;
                s
            }
            Err(e) => {
                stats.mark_left();
                stats.rejoin_count.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(room = cfg.room, error = %e, backoff_s = backoff.as_secs(), "janus: session establish failed — backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };

        // configure (unmuted) — best-effort; a failure just re-establishes.
        let session_url = format!(
            "{}/{}",
            cfg.api_url.trim_end_matches('/'),
            session.session_id
        );
        let handle_url = format!("{session_url}/{}", session.handle_id);
        if let Err(e) = client
            .post(&handle_url)
            .json(&build_configure("configure", false))
            .send()
            .await
        {
            tracing::warn!(error = %e, "janus: configure failed — re-establishing");
            stats.mark_left();
            continue;
        }

        packetizer.mark_silence();
        pending_48k.clear();
        let mut keepalive = tokio::time::interval(KEEPALIVE_EVERY);
        keepalive.tick().await; // consume the immediate first tick

        // Inner I/O loop until any error forces a re-establish. Labeled so a send failure inside the
        // inner drain `while` can break the whole I/O loop (a `while` cannot break with a value).
        let session_ok = 'io: loop {
            tokio::select! {
                // The phones' mixed output — accumulate + send as 20 ms PCMU.
                mixed = mix_rx.recv() => {
                    let Some(block) = mixed else { break 'io true; }; // channel closed = shutdown
                    pending_48k.extend(mulaw::stereo_to_mono(&block));
                    while pending_48k.len() >= MIX_SAMPLES_PER_PACKET_48K {
                        let chunk: Vec<i16> = pending_48k.drain(0..MIX_SAMPLES_PER_PACKET_48K).collect();
                        let pcm8k = mulaw::downsample_48k_to_8k(&chunk);
                        let ulaw = mulaw::ulaw_encode_block(&pcm8k);
                        let pkt = packetizer.packetize(&ulaw);
                        match socket.send_to(&pkt, session.janus_rtp_addr).await {
                            Ok(_) => { stats.tx_packets.fetch_add(1, Ordering::Relaxed); }
                            Err(e) => { tracing::warn!(error = %e, "janus: RTP send failed — re-establishing"); break 'io false; }
                        }
                    }
                }
                // The room mix minus ourselves — decode + push to the phones jitter buffer.
                recvd = socket.recv_from(&mut recv_buf) => {
                    match recvd {
                        Ok((len, _from)) => {
                            if let Some(rtp) = rtp_depacketize(&recv_buf[..len]) {
                                if rtp.payload_type == PCMU_PAYLOAD_TYPE && !rtp.payload.is_empty() {
                                    let mono8k = mulaw::ulaw_decode_block(&rtp.payload);
                                    let mono48k = mulaw::upsample_8k_to_48k(&mono8k);
                                    let frames = mono48k.len();
                                    let audio = DecodedAudio {
                                        stream_name: "janus-phones".to_string(),
                                        channels: vec![mono48k.clone(), mono48k],
                                        frames,
                                    };
                                    if let Ok(mut jb) = jitter.lock() {
                                        if let Some(b) = jb.get_mut(phones_id) {
                                            b.push(&audio);
                                        }
                                    }
                                    stats.rx_packets.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(e) => { tracing::warn!(error = %e, "janus: RTP recv failed — re-establishing"); break 'io false; }
                    }
                }
                _ = keepalive.tick() => {
                    if let Err(e) = client.post(&session_url).json(&build_keepalive("keepalive")).send().await {
                        tracing::warn!(error = %e, "janus: keepalive failed — re-establishing");
                        break 'io false;
                    }
                }
            }
        };

        stats.mark_left();
        if session_ok {
            // Clean shutdown (the mix channel closed).
            let _ = client
                .post(&handle_url)
                .json(&build_leave("leave"))
                .send()
                .await;
            tracing::info!("janus: mix channel closed — leaving the room");
            return;
        }
        stats.rejoin_count.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}
