//! The VBAN adapter (issue 1345 M1): packet encode/decode, per-participant jitter buffers, the
//! stream-name demux, and the UDP sender.
//!
//! The hub speaks byte-identical VBAN to the camboxes (the shared `intercom-vban` codec): it
//! RECEIVES on one socket, demultiplexes packets by their VBAN stream name into the matching
//! participant's [`JitterBuffer`] (unknown names ignored, exactly like `src/intercom.rs`), and
//! SENDS each cambox's mixed output back as a `camN` stereo PCM16 stream to `camN.lan:6980`.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Instant;

use anyhow::Result;
use intercom_vban::{VbanCodec, VbanHeader, VBAN_HEADER_SIZE};

use crate::vban_rate::VbanRateConverter;

/// One decoded VBAN packet's audio, deinterleaved into planar channels.
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// The VBAN stream name the packet carried.
    pub stream_name: String,
    /// Planar channels: `channels[ch][sample]`.
    pub channels: Vec<Vec<i16>>,
    /// Frames actually present in the payload.
    pub frames: usize,
}

impl DecodedAudio {
    /// Rebuild a packet from planar channels (e.g. after rate conversion). `frames` is the shortest
    /// channel's length, `0` for no channels.
    pub fn from_planar(stream_name: String, channels: Vec<Vec<i16>>) -> Self {
        let frames = channels.iter().map(Vec::len).min().unwrap_or(0);
        DecodedAudio {
            stream_name,
            channels,
            frames,
        }
    }

    /// Peak absolute sample across all channels (for a level meter). `0` for an empty packet.
    pub fn peak(&self) -> i16 {
        self.channels
            .iter()
            .flat_map(|c| c.iter())
            .map(|s| s.unsigned_abs())
            .max()
            .map(|u| u.min(i16::MAX as u16) as i16)
            .unwrap_or(0)
    }
}

/// Encode an interleaved PCM16 block as a VBAN packet, byte-identical to what `src/intercom.rs`
/// puts on the wire (header via the shared codec + little-endian i16 payload).
/// One outgoing VBAN audio block: the header fields plus the interleaved PCM16 payload (`frames`
/// frames × `channels`). Borrowed, so a per-tick send allocates nothing beyond the packet itself.
#[derive(Debug, Clone, Copy)]
pub struct OutBlock<'a> {
    pub stream_name: &'a str,
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_counter: u32,
    pub interleaved: &'a [i16],
    pub frames: usize,
}

pub fn encode_packet(block: &OutBlock<'_>) -> Result<Vec<u8>> {
    let mut header = VbanHeader::new(
        block.stream_name,
        block.sample_rate,
        block.channels,
        VbanCodec::Pcm16,
    )?;
    header.frame_counter = block.frame_counter;
    let mut packet = header.encode(block.frames).to_vec();
    packet.reserve(block.interleaved.len() * 2);
    for &s in block.interleaved {
        packet.extend_from_slice(&s.to_le_bytes());
    }
    Ok(packet)
}

/// Decode a VBAN packet into planar PCM16 (`Pcm16` + `Float32` payloads supported; both are what
/// the cambox may put on the wire) plus the header's sample rate in Hz (`0` for a reserved rate
/// index — no rate, so the converter rejects it). The number of frames is derived from the actual
/// payload length, so a short/long packet never panics. The samples are at the HEADER rate — the
/// receive path runs them through [`crate::vban_rate::VbanRateConverter`] before they reach a
/// 48 kHz jitter ring (issue 1345: a 96 kHz FOH stream overran the ring).
pub fn decode_packet(data: &[u8]) -> Result<(DecodedAudio, u32)> {
    let header = VbanHeader::decode(data)?;
    let n_ch = header.num_channels() as usize;
    let payload = &data[VBAN_HEADER_SIZE.min(data.len())..];

    let interleaved: Vec<i16> = match header.codec {
        c if c == VbanCodec::Pcm16 as u8 => payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect(),
        c if c == VbanCodec::Float32 as u8 => payload
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| {
                let f = f32::from_le_bytes(*c);
                (f * 32767.0).clamp(-32768.0, 32767.0) as i16
            })
            .collect(),
        other => anyhow::bail!("unsupported VBAN codec 0x{other:02x} (only PCM16/Float32)"),
    };

    let frames = interleaved.len().checked_div(n_ch).unwrap_or(0);
    let mut channels = vec![Vec::with_capacity(frames); n_ch];
    for (i, &s) in interleaved.iter().enumerate() {
        if n_ch == 0 {
            break;
        }
        channels[i % n_ch].push(s);
    }
    // Trim any trailing partial frame so every channel has exactly `frames` samples.
    for ch in &mut channels {
        ch.truncate(frames);
    }

    Ok((
        DecodedAudio {
            stream_name: header.stream_name_str().to_string(),
            channels,
            frames,
        },
        header.sample_rate_checked().unwrap_or(0),
    ))
}

/// Decode + demux one packet: `Some((participant_id, audio, sample_rate))` if the stream name maps
/// to a known participant, `None` if it decodes to an unknown name OR fails to decode (a
/// foreign/garbage packet is silently ignored, exactly like the cambox receiver).
pub fn route_packet(
    known_streams: &HashMap<String, usize>,
    data: &[u8],
) -> Option<(usize, DecodedAudio, u32)> {
    let (audio, sample_rate) = decode_packet(data).ok()?;
    let id = *known_streams.get(&audio.stream_name)?;
    Some((id, audio, sample_rate))
}

/// Bring one routed packet, sampled at `rate`, to the hub rate through its stream's `conv`
/// (issue 1345: a 96 kHz FOH stream pushed raw into the 48 kHz ring overran it on ~60 % of packets).
/// A rate transition is logged ONCE: an info naming the rate, or a warn when the rate is rejected.
/// `None` = the packet was dropped (unsupported rate — counted in the converter's `rate_rejects`).
pub fn to_hub_rate(
    conv: &mut VbanRateConverter,
    audio: DecodedAudio,
    rate: u32,
) -> Option<DecodedAudio> {
    let hub_rate = conv.out_rate();
    let DecodedAudio {
        stream_name,
        channels,
        ..
    } = audio;
    let step = conv.process(rate, channels);
    if step.rate_changed {
        if step.channels.is_some() {
            tracing::info!(stream = %stream_name, rate, hub_rate, "VBAN stream sample rate");
        } else {
            tracing::warn!(
                stream = %stream_name,
                rate,
                hub_rate,
                "VBAN stream at an unsupported sample rate: its packets are DROPPED (only 1x/2x/4x the hub rate is accepted)"
            );
        }
    }
    step.channels
        .map(|channels| DecodedAudio::from_planar(stream_name, channels))
}

/// A stream whose last packet is older than this is STALE (issue 1345, 24.9.2026): a muted or
/// stopped cambox keeps its buffer "previously live" forever. A stale stream no longer counts an
/// underrun per block (the old code counted 187 underruns/s for a muted cam, which made the counter
/// useless) and its level reads as silence (-120 dBFS) instead of the last packet's level forever.
pub const STALE_STREAM_MS: u64 = 500;

/// A per-participant jitter buffer: planar sample queues with underrun/overrun accounting and the
/// age of the last received packet.
///
/// Two fill policies share the one type:
///
/// * [`JitterBuffer::new`] — the VBAN/Janus network legs: pop whatever is queued, zero-pad a short
///   block, drop the OLDEST samples beyond the cap.
/// * [`JitterBuffer::local_capture`] — the local PipeWire capture (the MiniFuse talkback, issue 1345).
///   `pw-cat` hands the hub >= 1024-frame bursts (the graph runs at quantum 1024) that the block loop
///   pops as 256-frame blocks, so the buffer holds a TARGET fill (about 2x the burst): it prefills
///   to the target before the first pop, after an underrun it outputs WHOLE silent blocks until it
///   has refilled to the target (never a zero-spliced partial block mid-voice), and on overrun it
///   drops back down to the target (not just to the cap, which would overrun again at once).
#[derive(Debug)]
pub struct JitterBuffer {
    channels: Vec<VecDeque<i16>>,
    cap_frames: usize,
    /// `Some(target)` = the local-capture fill policy; `None` = the network-leg policy.
    target_frames: Option<usize>,
    /// Local-capture policy only: waiting to (re)fill to the target before audio flows.
    priming: bool,
    /// A mono packet is fanned ch1 -> ch2 when the participant declares >= 2 input channels.
    min_channels: usize,
    pub rx_packets: u64,
    pub underruns: u64,
    pub overruns: u64,
    last_rx: Option<Instant>,
    last_peak: i16,
}

impl JitterBuffer {
    /// A network-leg buffer sized for `cap_frames` per channel (older samples beyond that are dropped
    /// as an overrun). Channels grow on demand as packets arrive.
    pub fn new(cap_frames: usize) -> Self {
        JitterBuffer {
            channels: Vec::new(),
            cap_frames: cap_frames.max(1),
            target_frames: None,
            priming: false,
            min_channels: 1,
            rx_packets: 0,
            underruns: 0,
            overruns: 0,
            last_rx: None,
            last_peak: 0,
        }
    }

    /// A LOCAL-CAPTURE buffer (see the type doc): holds `target_frames` of audio, prefills to it
    /// before the first pop and after every underrun, and drops back to it when it would exceed
    /// `cap_frames`. The target is clamped into `1..=cap_frames`.
    pub fn local_capture(cap_frames: usize, target_frames: usize) -> Self {
        let cap = cap_frames.max(1);
        JitterBuffer {
            target_frames: Some(target_frames.clamp(1, cap)),
            priming: true,
            ..JitterBuffer::new(cap)
        }
    }

    /// Declare the participant's input channel count. A MONO packet for a participant with >= 2 input
    /// channels is copied into ch2 as well: a cambox sends mono VBAN, and the matrix routes ch1 -> out1
    /// and ch2 -> out2, so without the fan-out a cam came out left-only (and 6 dB quieter after the
    /// phones' stereo -> mono average). Only ch2 is filled — never beyond, so an 8-channel participant
    /// is not padded with silent queues that would count as short.
    pub fn with_min_channels(mut self, in_channels: usize) -> Self {
        self.min_channels = in_channels.max(1);
        self
    }

    fn ensure_channels(&mut self, n: usize) {
        while self.channels.len() < n {
            self.channels.push(VecDeque::new());
        }
    }

    /// Frames queued (the shortest channel; `0` before any packet).
    pub fn buffered_frames(&self) -> usize {
        self.channels.iter().map(|q| q.len()).min().unwrap_or(0)
    }

    /// Push a decoded packet's planar audio now. See [`JitterBuffer::push_at`].
    pub fn push(&mut self, audio: &DecodedAudio) {
        self.push_at(audio, Instant::now());
    }

    /// Push a decoded packet's planar audio received at `now`, counting one packet and at most one
    /// overrun. On overrun the network policy drops the oldest samples down to the cap; the
    /// local-capture policy drops them down to the target.
    pub fn push_at(&mut self, audio: &DecodedAudio, now: Instant) {
        let fan_mono = audio.channels.len() == 1 && self.min_channels >= 2;
        let n_ch = if fan_mono { 2 } else { audio.channels.len() };
        self.ensure_channels(n_ch);
        for (c, q) in self.channels.iter_mut().enumerate().take(n_ch) {
            let src = if fan_mono { 0 } else { c };
            q.extend(audio.channels[src].iter().copied());
        }
        let mut overran = false;
        let keep = self.target_frames.unwrap_or(self.cap_frames);
        for q in &mut self.channels {
            if q.len() > self.cap_frames {
                let drop = q.len() - keep;
                q.drain(0..drop);
                overran = true;
            }
        }
        if overran {
            self.overruns += 1;
        }
        self.rx_packets += 1;
        self.last_rx = Some(now);
        self.last_peak = audio.peak();
    }

    /// Whether the stream went quiet for longer than [`STALE_STREAM_MS`] as of `now`. A buffer that
    /// never received a packet is not stale (it is "never live" — see [`JitterBuffer::pop_block_at`]).
    fn is_stale_at(&self, now: Instant) -> bool {
        self.last_rx.is_some_and(|t| {
            now.saturating_duration_since(t) > std::time::Duration::from_millis(STALE_STREAM_MS)
        })
    }

    /// Pop one block now. See [`JitterBuffer::pop_block_at`].
    pub fn pop_block(&mut self, frames: usize) -> Vec<Vec<i16>> {
        self.pop_block_at(frames, Instant::now())
    }

    /// Pop one block of `frames` frames as planar channels at `now`.
    ///
    /// Network policy: pad any short channel with silence. Local-capture policy: while priming, or
    /// when fewer than `frames` are queued, output a WHOLE silent block and consume nothing (the
    /// queued voice is kept for when the buffer has refilled to the target).
    ///
    /// ONE underrun is counted when a stream runs short — only a PREVIOUSLY-LIVE (at least one packet)
    /// and NOT STALE ([`STALE_STREAM_MS`]) stream; a local-capture buffer counts it once on entering
    /// the refill wait, not once per silent block.
    pub fn pop_block_at(&mut self, frames: usize, now: Instant) -> Vec<Vec<i16>> {
        let n_ch = self.channels.len().max(1);
        let countable = self.last_rx.is_some() && !self.is_stale_at(now);

        if let Some(target) = self.target_frames {
            let buffered = self.buffered_frames();
            // Refill to the target — and to at least one whole block, so a target below the block
            // size can never flap between "refilled" and "ran dry" on the same pop.
            if self.priming && buffered >= target.max(frames) {
                self.priming = false;
            }
            if !self.priming && buffered < frames {
                // Ran dry: one underrun, then wait to refill to the target.
                self.priming = true;
                if countable {
                    self.underruns += 1;
                }
            }
            if self.priming {
                return vec![vec![0i16; frames]; n_ch];
            }
            return self
                .channels
                .iter_mut()
                .map(|q| q.drain(0..frames).collect())
                .collect();
        }

        let mut out = Vec::with_capacity(n_ch);
        let mut short = self.channels.is_empty();
        if self.channels.is_empty() {
            out.push(vec![0i16; frames]);
        }
        for q in &mut self.channels {
            let mut ch = Vec::with_capacity(frames);
            for _ in 0..frames {
                match q.pop_front() {
                    Some(s) => ch.push(s),
                    None => {
                        ch.push(0);
                        short = true;
                    }
                }
            }
            out.push(ch);
        }
        // Count an underrun ONLY for a stream that was previously LIVE and ran dry — never for a
        // buffer that has NEVER received a packet (a not-yet-connected vban leg, or an M1
        // `adapter="none"` participant that never receives VBAN at all), and never for a STALE stream
        // (a muted/stopped cambox). Otherwise such a participant would emit an underrun every block
        // and swamp the watchdog status line.
        if short && countable {
            self.underruns += 1;
        }
        out
    }

    /// Milliseconds since the last received packet, or `None` if none received yet.
    pub fn last_rx_age_ms(&self) -> Option<u64> {
        self.last_rx.map(|t| t.elapsed().as_millis() as u64)
    }

    /// Peak dBFS of the last received packet now. See [`JitterBuffer::last_level_dbfs_at`].
    pub fn last_level_dbfs(&self) -> f32 {
        self.last_level_dbfs_at(Instant::now())
    }

    /// Peak dBFS of the last received packet as of `now`: `-120.0` for silence, no packet, or a STALE
    /// stream ([`STALE_STREAM_MS`]).
    pub fn last_level_dbfs_at(&self, now: Instant) -> f32 {
        if self.last_rx.is_none() || self.is_stale_at(now) {
            return -120.0;
        }
        peak_dbfs(self.last_peak)
    }
}

/// Peak dBFS for an absolute PCM16 peak value (full scale = 32768). Silence → `-120.0`.
pub fn peak_dbfs(peak_abs: i16) -> f32 {
    let p = peak_abs.unsigned_abs() as f32;
    if p <= 0.0 {
        -120.0
    } else {
        (20.0 * (p / 32768.0).log10()).max(-120.0)
    }
}

/// A UDP sender for the per-participant VBAN outputs. One socket sends every cambox's stream to its
/// own `host:6980`.
pub struct VbanSender {
    socket: UdpSocket,
}

/// Resolve one VBAN destination (`host`, `port`) to its first address — a ONE-SHOT lookup the send
/// loop performs at startup and refreshes off the hot path. NEVER hand a host STRING to the
/// per-block send: `ToSocketAddrs` on a string is a synchronous getaddrinfo per call (~20 ms via
/// systemd-resolved on strih-lx), which at 187.5 blocks/s halved the hub's send rate and overran
/// every jitter buffer in the M1b live loopback (19.9.2026). `None` = unresolvable right now (the
/// caller keeps its last-known-good address).
pub fn resolve_vban_addr(host: &str, port: u16) -> Option<SocketAddr> {
    if host.trim().is_empty() {
        return None;
    }
    (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
}

impl VbanSender {
    /// Bind an ephemeral local UDP socket to send from.
    pub fn bind_ephemeral() -> io::Result<Self> {
        Ok(VbanSender {
            socket: UdpSocket::bind("0.0.0.0:0")?,
        })
    }

    /// Send one interleaved PCM16 [`OutBlock`] as a VBAN packet to `addr`. Pass a RESOLVED
    /// [`SocketAddr`] on the block hot path (see [`resolve_vban_addr`]); a host string here is a
    /// per-block DNS lookup.
    pub fn send_block<A: ToSocketAddrs>(&self, addr: A, block: &OutBlock<'_>) -> Result<()> {
        let packet = encode_packet(block)?;
        self.socket.send_to(&packet, addr)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn resolve_vban_addr_is_a_one_shot_lookup_with_the_vban_port() {
        // M1b live finding (19.9.2026): a `ToSocketAddrs` STRING on the per-block send path meant one
        // synchronous getaddrinfo per 5.33 ms block (~20 ms via systemd-resolved on strih-lx), which
        // halved the hub's send rate and overran every jitter buffer. Destinations are resolved once,
        // off the hot path, into a SocketAddr.
        let a = resolve_vban_addr("127.0.0.1", intercom_vban::VBAN_PORT).expect("literal resolves");
        assert_eq!(a.ip().to_string(), "127.0.0.1");
        assert_eq!(a.port(), intercom_vban::VBAN_PORT);
        // An unresolvable/empty host is None (the caller keeps its last-known-good address), never a panic.
        assert!(resolve_vban_addr("", intercom_vban::VBAN_PORT).is_none());
    }

    #[test]
    fn encode_decode_roundtrip_planar() {
        // Stereo interleaved L,R per frame.
        let interleaved: Vec<i16> = vec![1, -1, 2, -2, 3, -3, 4, -4];
        let frames = 4;
        let pkt = encode_packet(&OutBlock {
            stream_name: "cam5",
            sample_rate: 48000,
            channels: 2,
            frame_counter: 7,
            interleaved: &interleaved,
            frames,
        })
        .unwrap();
        let (audio, rate) = decode_packet(&pkt).unwrap();
        assert_eq!(rate, 48000, "the decoder carries the header rate");
        assert_eq!(audio.stream_name, "cam5");
        assert_eq!(audio.frames, 4);
        assert_eq!(audio.channels.len(), 2);
        assert_eq!(audio.channels[0], vec![1, 2, 3, 4]); // L
        assert_eq!(audio.channels[1], vec![-1, -2, -3, -4]); // R
    }

    #[test]
    fn jitter_underrun_accounting() {
        let mut jb = JitterBuffer::new(4096);
        // Push only 100 mono samples, then pop a 256-frame block → 100 real + 156 silence, 1 underrun.
        let short: Vec<i16> = (0..100).map(|i| i as i16).collect();
        jb.push(&DecodedAudio {
            stream_name: "cam1".into(),
            channels: vec![short.clone()],
            frames: 100,
        });
        let block = jb.pop_block(256);
        assert_eq!(block.len(), 1);
        assert_eq!(block[0].len(), 256);
        assert_eq!(&block[0][..100], &short[..]);
        assert!(block[0][100..].iter().all(|&s| s == 0));
        assert_eq!(jb.underruns, 1);
        assert_eq!(jb.rx_packets, 1);

        // Now push a full block; popping it is NOT an underrun.
        let full: Vec<i16> = (0..256).map(|i| i as i16).collect();
        jb.push(&DecodedAudio {
            stream_name: "cam1".into(),
            channels: vec![full.clone()],
            frames: 256,
        });
        let block2 = jb.pop_block(256);
        assert_eq!(block2[0], full);
        assert_eq!(jb.underruns, 1); // unchanged
    }

    #[test]
    fn jitter_never_received_pops_silence_without_underrun() {
        // A participant that never gets a packet (an M1 adapter="none" leg, or a not-yet-connected
        // vban leg) must NOT accrue an underrun every block — else it swamps the watchdog line.
        let mut jb = JitterBuffer::new(4096);
        for _ in 0..5 {
            let block = jb.pop_block(256);
            assert_eq!(block.len(), 1);
            assert!(block[0].iter().all(|&s| s == 0));
        }
        assert_eq!(
            jb.underruns, 0,
            "a never-received buffer must not count underruns"
        );
        assert_eq!(jb.rx_packets, 0);
        assert_eq!(jb.last_rx_age_ms(), None);
    }

    #[test]
    fn jitter_overrun_drops_oldest() {
        let mut jb = JitterBuffer::new(256); // cap 256 frames / channel
        let big: Vec<i16> = (0..400).map(|i| i as i16).collect();
        jb.push(&DecodedAudio {
            stream_name: "cam1".into(),
            channels: vec![big],
            frames: 400,
        });
        assert_eq!(jb.overruns, 1);
        // Buffer trimmed to the newest 256; the first popped sample is 400-256 = 144.
        let block = jb.pop_block(1);
        assert_eq!(block[0][0], 144);
    }

    #[test]
    fn peak_dbfs_scale() {
        assert!((peak_dbfs(32767) - 0.0).abs() < 0.01);
        assert_eq!(peak_dbfs(0), -120.0);
        // Half scale ≈ -6 dBFS.
        assert!((peak_dbfs(16384) - (-6.02)).abs() < 0.1);
    }

    #[test]
    fn vban_loopback_demux_and_foreign_ignored() {
        // Our sender → the receiver socket on a RANDOM UDP port → stream-name demux; a foreign
        // name is ignored (returns None), exactly like the cambox receiver.
        let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
        recv.set_read_timeout(Some(Duration::from_millis(1000)))
            .unwrap();
        let addr = recv.local_addr().unwrap();

        let mut known: HashMap<String, usize> = HashMap::new();
        known.insert("cam1".to_string(), 0);

        let sender = VbanSender::bind_ephemeral().unwrap();
        let interleaved: Vec<i16> = (0..256 * 2).map(|i| i as i16).collect();
        let known_block = OutBlock {
            stream_name: "cam1",
            sample_rate: 48000,
            channels: 2,
            frame_counter: 0,
            interleaved: &interleaved,
            frames: 256,
        };
        sender.send_block(addr, &known_block).unwrap();
        sender
            .send_block(
                addr,
                &OutBlock {
                    stream_name: "cam9",
                    ..known_block
                },
            )
            .unwrap();

        let mut buf = [0u8; 8192];
        let mut got_known = false;
        let mut got_foreign = false;
        for _ in 0..2 {
            let (n, _) = recv.recv_from(&mut buf).unwrap();
            match route_packet(&known, &buf[..n]) {
                Some((id, audio, rate)) => {
                    assert_eq!(id, 0);
                    assert_eq!(rate, 48000);
                    assert_eq!(audio.stream_name, "cam1");
                    assert_eq!(audio.frames, 256);
                    got_known = true;
                }
                None => got_foreign = true,
            }
        }
        assert!(got_known, "the cam1 packet must route to participant 0");
        assert!(got_foreign, "the cam9 packet must be ignored");
    }
}
