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
use std::time::{Duration, Instant};

use anyhow::Result;
use intercom_vban::{VbanCodec, VbanHeader, VBAN_HEADER_SIZE};

use crate::adaptive_target::AdaptiveTarget;
use crate::vban_jitter::{stretch_block, NetworkFill, NetworkFillStats, PopPlan};
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
/// Three fill policies share the one type:
///
/// * [`JitterBuffer::vban_leg`] — the VBAN network legs (the FOH program feed, the camboxes'
///   talkback, issue 1401): a target fill with prefill, one whole silent block per underrun and a
///   bounded drift servo. The policy itself is [`crate::vban_jitter::NetworkFill`]. Its underrun
///   counts only when the stream continues; a stream back after going stale counts one STALL and
///   starts over without its old tail (a muted cambox stops sending; that is not a dropout). A
///   program feed's target follows its sender's gaps ([`JitterBuffer::with_adaptive_target`]).
/// * [`JitterBuffer::local_capture`] — the local PipeWire capture (the MiniFuse talkback, issue 1345)
///   and the Janus ingress. `pw-cat` hands the hub >= 1024-frame bursts (the graph runs at quantum
///   1024) that the block loop pops as 256-frame blocks, so the buffer holds a TARGET fill (about 2x
///   the burst): it prefills to the target before the first pop, after an underrun it outputs WHOLE
///   silent blocks until it has refilled to the target (never a zero-spliced partial block
///   mid-voice), and on overrun it drops back down to the target (not just to the cap, which would
///   overrun again at once).
/// * [`JitterBuffer::new`] — the plain policy, for a participant with no ingress (an
///   `adapter = "none"` slot, the `program_out` sink): pop whatever is queued, zero-pad a short
///   block, drop the OLDEST samples beyond the cap.
#[derive(Debug)]
pub struct JitterBuffer {
    channels: Vec<VecDeque<i16>>,
    cap_frames: usize,
    policy: FillPolicy,
    /// A mono packet is fanned ch1 -> ch2 when the participant declares >= 2 input channels.
    min_channels: usize,
    pub rx_packets: u64,
    pub underruns: u64,
    pub overruns: u64,
    /// VBAN-leg policy: a ran-dry pop waiting to be judged by the next packet — counted when the
    /// stream continues within [`STALE_STREAM_MS`], dropped when it had stopped.
    pending_underrun: bool,
    last_rx: Option<Instant>,
    last_peak: i16,
    /// A program feed's adaptive target (issue 1401), `None` for a leg with a fixed target.
    adaptive: Option<AdaptiveLeg>,
    /// The last target change, waiting for the caller to take and log it off the lock.
    target_change: Option<TargetChange>,
}

/// A program feed's target change (issue 1401), handed to the caller to log once it has released
/// the buffer's lock: the receive task holds the jitter lock while it pushes, and the real-time
/// hub-mix thread takes that same lock every block, so no journal write may happen under it.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetChange {
    /// The leg's participant name.
    pub leg: String,
    pub from_frames: usize,
    pub to_frames: usize,
    /// The largest gap of the last 10 min the new target follows.
    pub max_gap_10min: Duration,
}

impl TargetChange {
    /// Log the change as one info line. Call it with no lock held.
    pub fn log(&self) {
        tracing::info!(
            leg = %self.leg,
            from_frames = self.from_frames,
            to_frames = self.to_frames,
            max_gap_ms_10min = self.max_gap_10min.as_secs_f64() * 1000.0,
            "intercom-hub: program feed target {} (the largest gap of the last 10 min)",
            if self.to_frames > self.from_frames {
                "raised"
            } else {
                "lowered"
            }
        );
    }
}

/// A VBAN leg whose target follows its sender's gaps, and the leg's name for its log line.
#[derive(Debug)]
struct AdaptiveLeg {
    target: AdaptiveTarget,
    name: String,
}

/// Which fill policy a [`JitterBuffer`] runs (see the type doc), as [`JitterBuffer::kind`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferKind {
    /// [`JitterBuffer::new`].
    Plain,
    /// [`JitterBuffer::local_capture`].
    LocalCapture,
    /// [`JitterBuffer::vban_leg`].
    VbanLeg,
}

/// The fill policy a [`JitterBuffer`] runs, with its state (see the type doc).
#[derive(Debug)]
enum FillPolicy {
    /// [`JitterBuffer::new`]: pop whatever is queued, zero-pad, drop down to the cap.
    Plain,
    /// [`JitterBuffer::local_capture`]: `priming` = waiting to (re)fill to `target` before audio flows.
    LocalCapture { target: usize, priming: bool },
    /// [`JitterBuffer::vban_leg`]: the VBAN network leg's target fill + drift servo (issue 1401).
    Network(NetworkFill),
}

impl JitterBuffer {
    /// A PLAIN buffer sized for `cap_frames` per channel (older samples beyond that are dropped as an
    /// overrun), for a participant with no ingress. Channels grow on demand as packets arrive.
    pub fn new(cap_frames: usize) -> Self {
        JitterBuffer {
            channels: Vec::new(),
            cap_frames: cap_frames.max(1),
            policy: FillPolicy::Plain,
            min_channels: 1,
            rx_packets: 0,
            underruns: 0,
            overruns: 0,
            pending_underrun: false,
            last_rx: None,
            last_peak: 0,
            adaptive: None,
            target_change: None,
        }
    }

    /// A LOCAL-CAPTURE buffer (see the type doc): holds `target_frames` of audio, prefills to it
    /// before the first pop and after every underrun, and drops back to it when it would exceed
    /// `cap_frames`. The target is clamped into `1..=cap_frames`.
    pub fn local_capture(cap_frames: usize, target_frames: usize) -> Self {
        let cap = cap_frames.max(1);
        JitterBuffer {
            policy: FillPolicy::LocalCapture {
                target: target_frames.clamp(1, cap),
                priming: true,
            },
            ..JitterBuffer::new(cap)
        }
    }

    /// A VBAN NETWORK-LEG buffer (issue 1401, see [`crate::vban_jitter`]): prefills to
    /// `target_frames` before the first pop, outputs ONE whole silent block per underrun and
    /// re-primes, keeps the fill near the target with a single-frame drift servo (<= 1 ms/s), and
    /// drops the oldest audio back down to the target above `cap_frames`. The target is clamped
    /// into `1..=cap_frames`.
    pub fn vban_leg(cap_frames: usize, target_frames: usize) -> Self {
        let cap = cap_frames.max(1);
        JitterBuffer {
            policy: FillPolicy::Network(NetworkFill::new(target_frames, cap)),
            ..JitterBuffer::new(cap)
        }
    }

    /// Let a VBAN leg's target follow its sender (issue 1401, design 5980775411: the program feeds).
    /// It starts at the floor [`crate::vban_jitter::VBAN_PROGRAM_TARGET_BLOCKS`]; every packet
    /// inside a running stream reports its gap to [`AdaptiveTarget::observe`], and a change goes to
    /// the leg's setpoint ([`NetworkFill::set_target`]), so the servo walks the fill there. `name`
    /// labels the leg's log line. The other policies ignore it.
    pub fn with_adaptive_target(
        mut self,
        name: &str,
        block_frames: usize,
        sample_rate: u32,
    ) -> Self {
        if let FillPolicy::Network(fill) = &mut self.policy {
            let target = AdaptiveTarget::new(block_frames, sample_rate);
            fill.set_target(target.target_frames());
            self.adaptive = Some(AdaptiveLeg {
                target,
                name: name.to_string(),
            });
        }
        self
    }

    /// Take a program feed's last target change, if any, to log it once the caller has released
    /// the buffer's lock ([`TargetChange::log`]). The receive task takes it after every push.
    pub fn take_target_change(&mut self) -> Option<TargetChange> {
        self.target_change.take()
    }

    /// The VBAN leg's live fill numbers (target, depth, servo corrections, stalls, and a program
    /// feed's largest gap of the last 10 min) for `/api/state`; `None` for the other policies.
    pub fn network_stats(&self) -> Option<NetworkFillStats> {
        match &self.policy {
            FillPolicy::Network(fill) => Some(NetworkFillStats {
                max_gap_us_10min: self.adaptive.as_ref().map(|a| {
                    u64::try_from(a.target.max_gap_10min().as_micros()).unwrap_or(u64::MAX)
                }),
                ..fill.stats()
            }),
            _ => None,
        }
    }

    /// Which fill policy this buffer runs.
    pub fn kind(&self) -> BufferKind {
        match self.policy {
            FillPolicy::Plain => BufferKind::Plain,
            FillPolicy::LocalCapture { .. } => BufferKind::LocalCapture,
            FillPolicy::Network(_) => BufferKind::VbanLeg,
        }
    }

    /// The hub's block loop LOST `missed` ticks before the next pop: the part of a late wake beyond
    /// the ticks it runs late ([`crate::block_clock::CATCHUP_MAX_BLOCKS`]). A VBAN leg drops as many
    /// blocks of its oldest audio, never going more than half a block under its target (see
    /// [`NetworkFill::discard_for_missed_ticks`]); the other policies are unchanged.
    pub fn skip_missed(&mut self, missed: u64, frames: usize) {
        let buffered = self.buffered_frames();
        if let FillPolicy::Network(fill) = &self.policy {
            let drop = fill.discard_for_missed_ticks(buffered, frames, missed);
            if drop > 0 {
                for q in &mut self.channels {
                    q.drain(0..drop);
                }
            }
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
    /// overrun. On overrun the plain policy drops the oldest samples down to the cap; the
    /// local-capture and VBAN-leg policies drop them down to the target.
    ///
    /// VBAN-leg policy: a packet after more than [`STALE_STREAM_MS`] of silence (a muted cambox
    /// unmuted, a restarted sender) counts one stall and starts the leg over — the tail left from
    /// before is dropped, never played ahead of the fresh audio, and a ran-dry pop before the
    /// silence is no underrun. A packet that continues the stream counts that pending underrun. A
    /// packet with a different channel count (a sender reconfigured) also starts the leg over, so a
    /// channel that stopped arriving can never hold every other one at an empty minimum. A program
    /// feed's adaptive target sees the gap since the previous packet, never across a stall.
    pub fn push_at(&mut self, audio: &DecodedAudio, now: Instant) {
        let stale = self.is_stale_at(now);
        let gap = self.last_rx.map(|t| now.saturating_duration_since(t));
        let fan_mono = audio.channels.len() == 1 && self.min_channels >= 2;
        let n_ch = if fan_mono { 2 } else { audio.channels.len() };
        let reshaped = !self.channels.is_empty() && self.channels.len() != n_ch;
        if let FillPolicy::Network(fill) = &mut self.policy {
            if stale {
                self.channels.clear();
                fill.restart_after_stall();
            } else if reshaped {
                self.channels.clear();
                fill.restart();
            } else if self.pending_underrun {
                self.underruns += 1;
            }
            self.pending_underrun = false;
            if let (Some(leg), Some(gap), false) = (&mut self.adaptive, gap, stale) {
                if let Some(target) = leg.target.observe(gap, now) {
                    let from = fill.target();
                    fill.set_target(target);
                    // Recorded, never logged here: the caller logs it off the lock.
                    self.target_change = Some(TargetChange {
                        leg: leg.name.clone(),
                        from_frames: from,
                        to_frames: target,
                        max_gap_10min: leg.target.max_gap_10min(),
                    });
                }
            }
        }
        self.ensure_channels(n_ch);
        for (c, q) in self.channels.iter_mut().enumerate().take(n_ch) {
            let src = if fan_mono { 0 } else { c };
            q.extend(audio.channels[src].iter().copied());
        }
        let mut overran = false;
        if let FillPolicy::Network(fill) = &mut self.policy {
            // The controller decides (and restarts its servo window); every channel is trimmed to
            // the same depth so the planar queues stay aligned.
            let longest = self.channels.iter().map(VecDeque::len).max().unwrap_or(0);
            if let Some(keep) = fill.overrun_keep(longest) {
                for q in &mut self.channels {
                    if q.len() > keep {
                        let drop = q.len() - keep;
                        q.drain(0..drop);
                    }
                }
                overran = true;
            }
        } else {
            let keep = match self.policy {
                FillPolicy::LocalCapture { target, .. } => target,
                _ => self.cap_frames,
            };
            for q in &mut self.channels {
                if q.len() > self.cap_frames {
                    let drop = q.len() - keep;
                    q.drain(0..drop);
                    overran = true;
                }
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
    /// Plain policy: pad any short channel with silence. Local-capture policy: while priming, or
    /// when fewer than `frames` are queued, output a WHOLE silent block and consume nothing (the
    /// queued voice is kept for when the buffer has refilled to the target). VBAN-leg policy: the
    /// same whole-silent-block rule (one block per underrun, then re-prime), plus the servo's
    /// single-frame drop/repeat spread across the block ([`stretch_block`]).
    ///
    /// ONE underrun is counted when a stream runs short — only a PREVIOUSLY-LIVE (at least one packet)
    /// and NOT STALE ([`STALE_STREAM_MS`]) stream; a local-capture or VBAN-leg buffer counts it once
    /// on entering the refill wait, not once per silent block. A VBAN leg counts it at its next
    /// packet, and only if that packet continues the stream (see [`JitterBuffer::push_at`]).
    pub fn pop_block_at(&mut self, frames: usize, now: Instant) -> Vec<Vec<i16>> {
        let n_ch = self.channels.len().max(1);
        let countable = self.last_rx.is_some() && !self.is_stale_at(now);
        let buffered = self.buffered_frames();

        if let FillPolicy::Network(fill) = &mut self.policy {
            return match fill.plan_pop(buffered, frames) {
                PopPlan::Silent { ran_dry } => {
                    if ran_dry && countable {
                        self.pending_underrun = true;
                    }
                    vec![vec![0i16; frames]; n_ch]
                }
                PopPlan::Audio { skip, take } => self
                    .channels
                    .iter_mut()
                    .map(|q| {
                        q.drain(0..skip);
                        let block: Vec<i16> = q.drain(0..take).collect();
                        if take == frames {
                            block
                        } else {
                            stretch_block(&block, frames)
                        }
                    })
                    .collect(),
            };
        }

        if let FillPolicy::LocalCapture { target, priming } = &mut self.policy {
            let target = *target;
            // Refill to the target — and to at least one whole block, so a target below the block
            // size can never flap between "refilled" and "ran dry" on the same pop.
            if *priming && buffered >= target.max(frames) {
                *priming = false;
            }
            if !*priming && buffered < frames {
                // Ran dry: one underrun, then wait to refill to the target.
                *priming = true;
                if countable {
                    self.underruns += 1;
                }
            }
            if *priming {
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

/// A UDP sender for ONE VBAN output destination (issue 1401): the block loop binds one per output
/// slot, and every socket is NON-BLOCKING.
///
/// Packets to a cambox that is off wait in its unresolved ARP neighbour queue, charged to the
/// socket that sent them. With one shared blocking socket the queues of several off camboxes
/// together filled its buffer, and `send_to` blocked the whole block loop (16929 missed ticks
/// live, 4.10.2026). The kernel caps each neighbour's queue (`unres_qlen_bytes`, oldest discarded)
/// at no more than one socket's default send buffer (both 212992 by default), so a per-slot socket
/// never fills on an off cambox: its packets are discarded by the kernel (`unresolved_discards` in
/// `/proc/net/stat/arp_cache`) and its `tx_packets` keeps counting. A socket that really backs up
/// (a NIC or queue stall) drops and counts the packet ([`SendOutcome::Dropped`]) instead of
/// blocking. One shared non-blocking socket was rejected: a buffer full of dead neighbours'
/// packets would drop the LIVE camboxes' packets too (reproduced on dev1: one dead neighbour per
/// socket 0 EAGAIN, two on one socket ~90 %).
pub struct VbanSender {
    socket: UdpSocket,
}

/// What one [`VbanSender::send_block`] did with the packet.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Handed to the kernel.
    Sent,
    /// The destination's socket was full (`WouldBlock`): this one packet is dropped.
    Dropped,
}

/// Classify a non-blocking `send_to` result: `WouldBlock` (a full send buffer) is a dropped packet,
/// any other error stays an error.
pub fn classify_send(sent: io::Result<usize>) -> io::Result<SendOutcome> {
    match sent {
        Ok(_) => Ok(SendOutcome::Sent),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(SendOutcome::Dropped),
        Err(e) => Err(e),
    }
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
    /// Bind an ephemeral, non-blocking local UDP socket to send from.
    pub fn bind_ephemeral() -> io::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_nonblocking(true)?;
        Ok(VbanSender { socket })
    }

    /// Send one interleaved PCM16 [`OutBlock`] as a VBAN packet to `addr`, never waiting: a full
    /// socket drops the packet ([`SendOutcome::Dropped`]). Pass a RESOLVED [`SocketAddr`] on the
    /// block hot path (see [`resolve_vban_addr`]); a host string here is a per-block DNS lookup.
    pub fn send_block<A: ToSocketAddrs>(
        &self,
        addr: A,
        block: &OutBlock<'_>,
    ) -> Result<SendOutcome> {
        let packet = encode_packet(block)?;
        Ok(classify_send(self.socket.send_to(&packet, addr))?)
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
    fn a_sender_socket_never_blocks() {
        // Issue 1401 (4.10.2026): one blocking socket for every cambox stalled the block loop once
        // the unresolved-neighbour queues of several off camboxes together filled its send buffer
        // (16929 missed ticks).
        // Every sender socket is non-blocking: with nothing to read, recv_from returns WouldBlock
        // at once. A blocking socket would wait out the 2 s timeout first.
        let sender = VbanSender::bind_ephemeral().unwrap();
        sender
            .socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0u8; 64];
        let started = Instant::now();
        let err = sender
            .socket
            .recv_from(&mut buf)
            .expect_err("nothing was sent to it");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "returned at once, not after the timeout: {:?}",
            started.elapsed()
        );
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
        assert_eq!(
            sender.send_block(addr, &known_block).unwrap(),
            SendOutcome::Sent
        );
        assert_eq!(
            sender
                .send_block(
                    addr,
                    &OutBlock {
                        stream_name: "cam9",
                        ..known_block
                    },
                )
                .unwrap(),
            SendOutcome::Sent
        );

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
