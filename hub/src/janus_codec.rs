//! The Janus leg's codecs (issue 1345, 25.9.2026): Opus 48 kHz mono 20 ms with in-band FEC by
//! default, G.711 PCMU still selectable (`[janus].codec`).
//!
//! [`TxEncoder`] turns exactly one 20 ms frame of 48 kHz mono (one [`FRAME_48K`] pop of the paced
//! ring) into one RTP payload. [`RxDecoder`] turns each received payload back into 48 kHz mono.
//! For Opus it also conceals a short loss, using the sequence-gap plan from
//! [`crate::janus_pacing::rx_gap`]:
//!
//! - the frame right before the packet is rebuilt from the packet's in-band FEC;
//! - earlier missing frames (up to [`MAX_CONCEAL_FRAMES`]) use the decoder's packet-loss
//!   concealment;
//! - duplicates and late packets are dropped.
//!
//! libopus comes from the `opus` crate, whose `opusic-sys` builds the bundled libopus source with
//! cmake. No system libopus is needed at build time or at run time.

use anyhow::{anyhow, bail, Context, Result};

use crate::janus_pacing::{rx_gap, RxGap, FRAME_48K, MAX_CONCEAL_FRAMES};
use crate::janus_rtp::JanusCodec;
use crate::mulaw;

/// The Opus sample rate: the hub's own rate, so nothing is resampled on this leg.
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
/// The Opus bitrate: wideband-to-fullband speech with enough room for the in-band FEC copy.
pub const OPUS_BITRATE_BPS: i32 = 32_000;
/// The packet loss the encoder plans for. In-band FEC is only produced when this is above zero.
pub const OPUS_EXPECTED_LOSS_PERC: i32 = 10;
/// The largest Opus packet (RFC 6716: 1275 bytes for one frame).
const OPUS_MAX_PACKET: usize = 1275;
/// The largest Opus frame a decode can return: 120 ms at 48 kHz.
const OPUS_MAX_FRAME: usize = 5760;

/// One Janus payload per 20 ms frame, in the configured codec.
pub enum TxEncoder {
    /// Opus 48 kHz mono, VoIP mode, in-band FEC on.
    Opus(Box<opus::Encoder>),
    /// G.711 µ-law: the 241-tap anti-alias decimator to 8 kHz, then µ-law.
    Pcmu(mulaw::Decimator48kTo8k),
}

impl TxEncoder {
    /// An encoder for `codec`, configured for the Janus leg.
    pub fn new(codec: JanusCodec) -> Result<Self> {
        match codec {
            JanusCodec::Opus => {
                let mut enc = opus::Encoder::new(
                    OPUS_SAMPLE_RATE,
                    opus::Channels::Mono,
                    opus::Application::Voip,
                )
                .context("opus encoder create")?;
                enc.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE_BPS))
                    .context("opus set bitrate")?;
                enc.set_inband_fec(true).context("opus enable FEC")?;
                enc.set_packet_loss_perc(OPUS_EXPECTED_LOSS_PERC)
                    .context("opus set expected loss")?;
                Ok(TxEncoder::Opus(Box::new(enc)))
            }
            JanusCodec::Pcmu => Ok(TxEncoder::Pcmu(mulaw::Decimator48kTo8k::new())),
        }
    }

    /// Encode exactly one 20 ms frame ([`FRAME_48K`] samples of 48 kHz mono).
    pub fn encode(&mut self, frame: &[i16]) -> Result<Vec<u8>> {
        if frame.len() != FRAME_48K {
            bail!(
                "janus encoder takes one 20 ms frame ({FRAME_48K} samples), got {}",
                frame.len()
            );
        }
        match self {
            TxEncoder::Opus(enc) => {
                let mut out = vec![0u8; OPUS_MAX_PACKET];
                let n = enc.encode(frame, &mut out).context("opus encode")?;
                out.truncate(n);
                Ok(out)
            }
            TxEncoder::Pcmu(decimator) => Ok(mulaw::ulaw_encode_block(&decimator.process(frame))),
        }
    }

    /// Start from a clean state (a new Janus session).
    pub fn reset(&mut self) -> Result<()> {
        match self {
            TxEncoder::Opus(enc) => enc.reset_state().context("opus encoder reset"),
            TxEncoder::Pcmu(decimator) => {
                decimator.reset();
                Ok(())
            }
        }
    }
}

/// One received payload, decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RxDecoded {
    /// 48 kHz mono samples: the concealed frames (if any) followed by the packet's own audio.
    /// Empty for a dropped duplicate or late packet.
    pub samples: Vec<i16>,
    /// Frames rebuilt by FEC or PLC because packets before this one were lost.
    pub concealed_frames: u32,
}

/// The receive side of the Janus leg: payload → 48 kHz mono, with Opus loss concealment.
pub struct RxDecoder {
    kind: RxKind,
    last_seq: Option<u16>,
    last_ssrc: Option<u32>,
}

enum RxKind {
    Opus(Box<opus::Decoder>),
    Pcmu,
}

impl RxDecoder {
    /// A decoder for `codec`.
    pub fn new(codec: JanusCodec) -> Result<Self> {
        let kind = match codec {
            JanusCodec::Opus => RxKind::Opus(Box::new(
                opus::Decoder::new(OPUS_SAMPLE_RATE, opus::Channels::Mono)
                    .context("opus decoder create")?,
            )),
            JanusCodec::Pcmu => RxKind::Pcmu,
        };
        Ok(RxDecoder {
            kind,
            last_seq: None,
            last_ssrc: None,
        })
    }

    /// Start from a clean state (a new Janus session).
    pub fn reset(&mut self) -> Result<()> {
        self.last_seq = None;
        self.last_ssrc = None;
        match &mut self.kind {
            RxKind::Opus(dec) => dec.reset_state().context("opus decoder reset"),
            RxKind::Pcmu => Ok(()),
        }
    }

    /// Note the SSRC of the packet about to be decoded. A new SSRC is a new sender with its own
    /// sequence numbers, so the sequence plan and the codec state start over.
    pub fn observe_ssrc(&mut self, ssrc: u32) {
        if self.last_ssrc != Some(ssrc) {
            if self.last_ssrc.is_some() {
                self.last_seq = None;
                if let RxKind::Opus(dec) = &mut self.kind {
                    if let Err(e) = dec.reset_state() {
                        tracing::warn!(error = %e, "janus: decoder reset on a new SSRC failed");
                    }
                }
            }
            self.last_ssrc = Some(ssrc);
        }
    }

    /// Decode the payload of the RTP packet with sequence number `seq`.
    pub fn decode(&mut self, seq: u16, payload: &[u8]) -> Result<RxDecoded> {
        let gap = rx_gap(self.last_seq, seq);
        if gap == RxGap::Stale {
            return Ok(RxDecoded::default());
        }
        if payload.is_empty() {
            return Err(anyhow!("janus: empty RTP payload (seq {seq})"));
        }
        self.last_seq = Some(seq);
        match &mut self.kind {
            RxKind::Pcmu => Ok(RxDecoded {
                samples: mulaw::upsample_8k_to_48k(&mulaw::ulaw_decode_block(payload)),
                concealed_frames: 0,
            }),
            RxKind::Opus(dec) => {
                let lost = match gap {
                    RxGap::Lost(n) => n.min(MAX_CONCEAL_FRAMES),
                    _ => 0,
                };
                let mut samples = Vec::with_capacity(FRAME_48K * (lost as usize + 1));
                let mut frame = vec![0i16; FRAME_48K];
                // Frames lost before the one right ahead of this packet: packet-loss concealment.
                for _ in 1..lost {
                    let n = dec.decode(&[], &mut frame, false).context("opus PLC")?;
                    samples.extend_from_slice(&frame[..n]);
                }
                // The frame right before this packet: rebuilt from this packet's in-band FEC.
                if lost > 0 {
                    let n = dec.decode(payload, &mut frame, true).context("opus FEC")?;
                    samples.extend_from_slice(&frame[..n]);
                }
                let mut own = vec![0i16; OPUS_MAX_FRAME];
                let n = dec
                    .decode(payload, &mut own, false)
                    .context("opus decode")?;
                samples.extend_from_slice(&own[..n]);
                Ok(RxDecoded {
                    samples,
                    concealed_frames: u32::from(lost),
                })
            }
        }
    }
}
