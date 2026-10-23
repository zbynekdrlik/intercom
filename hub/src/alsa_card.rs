//! issue 1438: the hub opens the cutters' USB card (strih-snv's MiniFuse 4, strih-pp's Yamaha MG-XU)
//! directly through ALSA, with no PipeWire node in between. This module is the pure, std-only half
//! of that leg ([`crate::alsa_leg`] runs the supervised `aplay` / `arecord` children):
//!
//! - the card is named by its ALSA card ID (`/proc/asound/cards`: `M4`, `MGXU`), never by its
//!   index, which moves when USB re-enumerates; the PCM is `hw:CARD=<id>,DEV=<n>`;
//! - a missing card is the leg DOWN, never a fallback: the old `pw-cat` children carried no
//!   `node.dont-fallback`, and when the MG-XU left USB on strih-pp WirePlumber re-routed the
//!   cutters' playback into the program sink (issue 1438 comment 6337554988);
//! - `hw:` converts nothing, so the hub matches the card itself: the channel count is the CARD's
//!   (the MiniFuse opens only with 6 channels both ways, the cutters use 4 out / 2 in), the
//!   participant's channels are mapped onto hw channels ([`spread_to_hw`] / [`pick_from_hw`]), and
//!   the sample format is picked per direction from the card's own USB stream description
//!   (`/proc/asound/<id>/stream0`, [`parse_stream_formats`] + [`choose_format`]): S16_LE when it is
//!   offered, else S32_LE, else S24_3LE, converted in the hub ([`PcmFormat`]). The MiniFuse capture
//!   offers S32_LE only (live read 23.10.2026).
//!
//! The `read_*` / [`resolve_direction`] glue reads `/proc`; everything else is pure.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

/// The default ALSA period of a card leg: one hub block (5.33 ms at 48 kHz), like the HDMI leg.
pub const ALSA_CARD_DEFAULT_PERIOD_FRAMES: usize = 256;

/// The ALSA buffer of a card leg in periods (aplay keeps it full on playback): 4, like the HDMI leg.
pub const ALSA_CARD_PERIODS: usize = 4;

/// The allowed `alsa_period_frames`. The playback pipe must also fit a Linux pipe at its trim mark
/// (9 periods of the card's channels at up to 4 bytes a sample), checked at load by
/// [`crate::alsa_spec::playback_pipe_trim_bytes`]: the MiniFuse's 6 channels fit up to 303 frames.
pub const ALSA_CARD_PERIOD_RANGE: std::ops::RangeInclusive<usize> = 64..=512;

/// The most hw channels a card leg opens (a USB class-compliant interface).
pub const ALSA_CARD_MAX_CHANNELS: usize = 32;

/// The longest ALSA card ID (the kernel keeps 15 characters + NUL).
pub const ALSA_CARD_ID_MAX: usize = 15;

/// Where the kernel lists the cards by ID (`/proc/asound/<id>` is a symlink to `cardN`).
pub const PROC_ASOUND: &str = "/proc/asound";

/// One hw sample format the leg speaks with a card, picked from what the card offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmFormat {
    /// Signed 16-bit little-endian: the hub's own sample type, no conversion.
    S16Le,
    /// Signed 32-bit little-endian (a 24-bit USB card in a 32-bit container).
    S32Le,
    /// Signed 24-bit little-endian packed in 3 bytes.
    S24Packed,
}

/// The order the leg prefers the formats a card offers: no conversion first.
pub const FORMAT_PREFERENCE: [PcmFormat; 3] =
    [PcmFormat::S16Le, PcmFormat::S32Le, PcmFormat::S24Packed];

impl PcmFormat {
    /// The name aplay / arecord take with `-f` and `/proc/asound/<id>/stream0` prints.
    pub fn alsa_name(self) -> &'static str {
        match self {
            PcmFormat::S16Le => "S16_LE",
            PcmFormat::S32Le => "S32_LE",
            PcmFormat::S24Packed => "S24_3LE",
        }
    }

    /// The format with this ALSA name, if the leg speaks it.
    pub fn from_alsa_name(name: &str) -> Option<Self> {
        FORMAT_PREFERENCE
            .into_iter()
            .find(|f| f.alsa_name() == name.trim())
    }

    /// Bytes per sample on the wire.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            PcmFormat::S16Le => 2,
            PcmFormat::S32Le => 4,
            PcmFormat::S24Packed => 3,
        }
    }

    /// Hub samples -> this format's little-endian bytes. A 16-bit sample is the high bits of the
    /// wider word, so the level is the same.
    pub fn encode(self, samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * self.bytes_per_sample());
        for &s in samples {
            let [lo, hi] = s.to_le_bytes();
            match self {
                PcmFormat::S16Le => out.extend_from_slice(&[lo, hi]),
                PcmFormat::S32Le => out.extend_from_slice(&[0, 0, lo, hi]),
                PcmFormat::S24Packed => out.extend_from_slice(&[0, lo, hi]),
            }
        }
        out
    }

    /// This format's little-endian bytes -> hub samples (the top 16 bits of each sample). A
    /// trailing partial sample is dropped, so a short read never panics.
    pub fn decode(self, bytes: &[u8]) -> Vec<i16> {
        match self {
            PcmFormat::S16Le => bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes(*c))
                .collect(),
            PcmFormat::S32Le => bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes([c[2], c[3]]))
                .collect(),
            PcmFormat::S24Packed => bytes
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes([c[1], c[2]]))
                .collect(),
        }
    }

    /// Hub little-endian s16 bytes (what the pipe writer builds) -> this format's bytes. S16_LE
    /// hands the bytes back untouched.
    pub fn from_s16le_bytes(self, bytes: Vec<u8>) -> Vec<u8> {
        if self == PcmFormat::S16Le {
            return bytes;
        }
        let samples: Vec<i16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        self.encode(&samples)
    }
}

/// Which side of the card a leg opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Playback,
    Capture,
}

impl Direction {
    fn header(self) -> &'static str {
        match self {
            Direction::Playback => "Playback:",
            Direction::Capture => "Capture:",
        }
    }

    /// The tool that opens this side (`aplay` / `arecord`).
    pub fn tool(self) -> &'static str {
        match self {
            Direction::Playback => "aplay",
            Direction::Capture => "arecord",
        }
    }
}

/// One alternate setting a USB card offers on one side, as `stream0` prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamAltset {
    /// The format name (`S32_LE`), kept as text: a format the leg does not speak is still listed.
    pub format: String,
    pub channels: usize,
    /// The `Rates:` text (`44100, 48000, 96000` or `8000 - 48000 (continuous)`).
    pub rates: String,
}

/// Every altset the card's `stream0` text lists for `dir` (`Format:` starts one, `Channels:` and
/// `Rates:` complete it). The `Status:` block above them carries no `Format:` line.
pub fn parse_stream_formats(text: &str, dir: Direction) -> Vec<StreamAltset> {
    let mut out: Vec<StreamAltset> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t == "Playback:" || t == "Capture:" {
            inside = t == dir.header();
            continue;
        }
        if !inside {
            continue;
        }
        if let Some(v) = t.strip_prefix("Format:") {
            out.push(StreamAltset {
                format: v.trim().to_string(),
                channels: 0,
                rates: String::new(),
            });
        } else if let Some(v) = t.strip_prefix("Channels:") {
            if let Some(a) = out.last_mut() {
                a.channels = v.trim().parse().unwrap_or(0);
            }
        } else if let Some(v) = t.strip_prefix("Rates:") {
            if let Some(a) = out.last_mut() {
                a.rates = v.trim().to_string();
            }
        }
    }
    out
}

/// Whether a `Rates:` text offers `rate`: a listed value, or a `low - high` range.
pub fn rates_offer(rates: &str, rate: u32) -> bool {
    if let Some((lo, hi)) = rates.split_once(" - ") {
        let lo: Option<u32> = lo.trim().parse().ok();
        let hi: Option<u32> = hi.split_whitespace().next().and_then(|h| h.parse().ok());
        if let (Some(lo), Some(hi)) = (lo, hi) {
            return (lo..=hi).contains(&rate);
        }
    }
    rates
        .split(',')
        .any(|r| r.trim().parse::<u32>().ok() == Some(rate))
}

/// The format the leg opens the card with: the first of [`FORMAT_PREFERENCE`] that an altset offers
/// at exactly `channels` channels and `rate`.
pub fn choose_format(altsets: &[StreamAltset], channels: usize, rate: u32) -> Option<PcmFormat> {
    FORMAT_PREFERENCE.into_iter().find(|f| {
        altsets.iter().any(|a| {
            a.format == f.alsa_name() && a.channels == channels && rates_offer(&a.rates, rate)
        })
    })
}

/// Why a card leg is down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlsaDown {
    /// `/proc/asound/<id>` does not exist: the card is not on the box (unplugged).
    CardMissing,
    /// The card is there but offers no format the leg speaks at the hub rate and the declared
    /// channel count.
    NoFormat,
}

impl AlsaDown {
    pub fn reason(self) -> &'static str {
        match self {
            AlsaDown::CardMissing => "card not present",
            AlsaDown::NoFormat => {
                "the card offers no S16_LE/S32_LE/S24_3LE at this rate and channel count"
            }
        }
    }
}

/// The decision before every spawn, from what `/proc` shows: `present` = `/proc/asound/<id>`
/// exists, `stream0` = its USB stream description (`None` for a card without one: then S16_LE,
/// and the tool's own open error says if the card refuses it).
pub fn decide_direction(
    present: bool,
    stream0: Option<&str>,
    dir: Direction,
    channels: usize,
    rate: u32,
) -> Result<PcmFormat, AlsaDown> {
    if !present {
        return Err(AlsaDown::CardMissing);
    }
    match stream0 {
        None => Ok(PcmFormat::S16Le),
        Some(text) => choose_format(&parse_stream_formats(text, dir), channels, rate)
            .ok_or(AlsaDown::NoFormat),
    }
}

/// [`decide_direction`] on the live `/proc/asound` (`root` is that directory; a test passes a
/// fixture tree).
pub fn resolve_direction(
    root: &Path,
    card: &str,
    dir: Direction,
    channels: usize,
    rate: u32,
) -> Result<PcmFormat, AlsaDown> {
    let dir_path = root.join(card);
    let present = dir_path.exists();
    let stream0 = std::fs::read_to_string(dir_path.join("stream0")).ok();
    decide_direction(present, stream0.as_deref(), dir, channels, rate)
}

/// An ALSA card ID the matrix may name: 1..=15 of `[A-Za-z0-9_]`, and not all digits (a bare
/// number is a card INDEX to ALSA, which moves).
pub fn card_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ALSA_CARD_ID_MAX
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !id.chars().all(|c| c.is_ascii_digit())
}

/// `hw:CARD=<id>,DEV=<n>`: the card by ID, nothing converts or mixes.
pub fn pcm_name(card: &str, device: u32) -> String {
    format!("hw:CARD={card},DEV={device}")
}

/// The aplay / arecord argv for one side of the card: raw PCM on stdin / stdout, `-N` so a PCM
/// another process holds fails the open at once, one period of `period` frames and a buffer of
/// [`ALSA_CARD_PERIODS`] periods.
pub fn card_argv(
    dir: Direction,
    card: &str,
    device: u32,
    format: PcmFormat,
    rate: u32,
    channels: usize,
    period: usize,
) -> Vec<String> {
    vec![
        dir.tool().into(),
        "-N".into(),
        "-D".into(),
        pcm_name(card, device),
        "-t".into(),
        "raw".into(),
        "-f".into(),
        format.alsa_name().into(),
        "-r".into(),
        rate.to_string(),
        "-c".into(),
        channels.to_string(),
        format!("--period-size={period}"),
        format!("--buffer-size={}", period * ALSA_CARD_PERIODS),
        "-".into(),
    ]
}

/// Whether an arecord stderr line reports an xrun (`overrun!!! (at least N ms long)`).
pub fn is_capture_xrun_line(line: &str) -> bool {
    line.contains("overrun!!!")
}

/// Whether an arecord stderr line is its start banner (`Recording raw data 'stdout' : ...`).
pub fn is_capture_banner_line(line: &str) -> bool {
    line.starts_with("Recording ")
}

/// A channel map from the matrix: comma-separated 1-based hw channel numbers, one per participant
/// channel. Returns the 0-based hw indices. `unique` refuses a hw channel named twice (playback:
/// two participant channels cannot share one output).
pub fn parse_channel_map(
    map: &str,
    participant_channels: usize,
    hw_channels: usize,
    unique: bool,
) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in map.split(',') {
        let n: usize = part
            .trim()
            .parse()
            .map_err(|_| format!("'{}' is not a channel number", part.trim()))?;
        if !(1..=hw_channels).contains(&n) {
            return Err(format!(
                "channel {n} is outside the card's 1..={hw_channels}"
            ));
        }
        if unique && out.contains(&(n - 1)) {
            return Err(format!("channel {n} is named twice"));
        }
        out.push(n - 1);
    }
    if out.len() != participant_channels {
        return Err(format!(
            "names {} channels, the participant has {participant_channels}",
            out.len()
        ));
    }
    Ok(out)
}

/// The identity map: participant channel n on hw channel n.
pub fn identity_map(channels: usize) -> Vec<usize> {
    (0..channels).collect()
}

/// A participant's interleaved block (`src_channels` wide) -> the card's interleaved block
/// (`hw_channels` wide): participant channel i lands on hw channel `map[i]`, every other hw
/// channel plays silence. A trailing partial frame is dropped.
pub fn spread_to_hw(
    block: &[i16],
    src_channels: usize,
    map: &[usize],
    hw_channels: usize,
) -> Vec<i16> {
    if src_channels == 0 || hw_channels == 0 {
        return Vec::new();
    }
    let frames = block.len() / src_channels;
    let mut out = vec![0i16; frames * hw_channels];
    for (src, dst) in block
        .chunks_exact(src_channels)
        .zip(out.chunks_exact_mut(hw_channels))
    {
        for (&s, &hw) in src.iter().zip(map) {
            if let Some(d) = dst.get_mut(hw) {
                *d = s;
            }
        }
    }
    out
}

/// The card's interleaved capture block (`hw_channels` wide) -> the participant's planar channels:
/// participant channel i reads hw channel `map[i]`. A trailing partial frame is dropped.
pub fn pick_from_hw(hw_block: &[i16], hw_channels: usize, map: &[usize]) -> Vec<Vec<i16>> {
    if hw_channels == 0 {
        return vec![Vec::new(); map.len()];
    }
    let frames = hw_block.len() / hw_channels;
    let mut out: Vec<Vec<i16>> = map.iter().map(|_| Vec::with_capacity(frames)).collect();
    for frame in hw_block.chunks_exact(hw_channels) {
        for (ch, &hw) in out.iter_mut().zip(map) {
            ch.push(frame.get(hw).copied().unwrap_or(0));
        }
    }
    out
}

/// Everything the hub needs to run one participant's card leg, resolved from the matrix
/// ([`crate::matrix::Matrix::alsa_cards`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlsaCardSpec {
    pub participant: usize,
    /// The ALSA card ID (`M4`, `MGXU`).
    pub card: String,
    pub device: u32,
    pub period_frames: usize,
    /// The participant's output channels (0 = no playback) and the card's playback channel count.
    pub out_channels: usize,
    pub hw_out_channels: usize,
    /// 0-based hw playback channel of each participant output channel.
    pub out_map: Vec<usize>,
    /// The participant's input channels (0 = no capture) and the card's capture channel count.
    pub in_channels: usize,
    pub hw_in_channels: usize,
    /// 0-based hw capture channel of each participant input channel.
    pub in_map: Vec<usize>,
}

/// One side's state as the leg last decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlsaDirState {
    /// The leg has not looked at the card yet.
    #[default]
    NotStarted,
    /// The card is there; the child is (re)spawned with this format.
    Ready(PcmFormat),
    /// The leg is down and why.
    Down(AlsaDown),
}

impl AlsaDirState {
    fn code(self) -> u8 {
        match self {
            AlsaDirState::NotStarted => 0,
            AlsaDirState::Ready(PcmFormat::S16Le) => 1,
            AlsaDirState::Ready(PcmFormat::S32Le) => 2,
            AlsaDirState::Ready(PcmFormat::S24Packed) => 3,
            AlsaDirState::Down(AlsaDown::CardMissing) => 10,
            AlsaDirState::Down(AlsaDown::NoFormat) => 11,
        }
    }

    fn from_code(c: u8) -> Self {
        match c {
            1 => AlsaDirState::Ready(PcmFormat::S16Le),
            2 => AlsaDirState::Ready(PcmFormat::S32Le),
            3 => AlsaDirState::Ready(PcmFormat::S24Packed),
            10 => AlsaDirState::Down(AlsaDown::CardMissing),
            11 => AlsaDirState::Down(AlsaDown::NoFormat),
            _ => AlsaDirState::NotStarted,
        }
    }

    /// The named state for `/api/state`.
    pub fn name(self) -> &'static str {
        match self {
            AlsaDirState::NotStarted => "not started yet",
            AlsaDirState::Ready(_) => "ready",
            AlsaDirState::Down(d) => d.reason(),
        }
    }

    /// The format, once ready.
    pub fn format(self) -> Option<PcmFormat> {
        match self {
            AlsaDirState::Ready(f) => Some(f),
            _ => None,
        }
    }
}

/// One side of a card leg, as `/api/state` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AlsaDirFacet {
    pub state: AlsaDirState,
    /// The child is moving audio: aplay has read from its pipe / arecord has delivered a block
    /// since its spawn.
    pub running: bool,
    /// The child's xruns since the hub started (aplay `underrun!!!` / arecord `overrun!!!`).
    pub xruns: u64,
    /// Child spawns since the hub started.
    pub spawns: u64,
}

/// Shared, atomic counters of one side of a card leg.
#[derive(Debug, Default)]
pub struct AlsaDirStats {
    state: AtomicU8,
    running: AtomicBool,
    xruns: AtomicU64,
    spawns: AtomicU64,
}

impl AlsaDirStats {
    pub fn set_state(&self, state: AlsaDirState) {
        self.state.store(state.code(), Ordering::Relaxed);
    }

    pub fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::Relaxed);
    }

    pub fn record_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_spawn(&self) {
        self.spawns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> AlsaDirFacet {
        AlsaDirFacet {
            state: AlsaDirState::from_code(self.state.load(Ordering::Relaxed)),
            running: self.running.load(Ordering::Relaxed),
            xruns: self.xruns.load(Ordering::Relaxed),
            spawns: self.spawns.load(Ordering::Relaxed),
        }
    }
}

/// Both sides of one participant's card leg.
#[derive(Debug, Default)]
pub struct AlsaLegStats {
    pub playback: AlsaDirStats,
    pub capture: AlsaDirStats,
}

/// A point-in-time snapshot of both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AlsaLegFacet {
    pub playback: AlsaDirFacet,
    pub capture: AlsaDirFacet,
}

impl AlsaLegStats {
    pub fn side(&self, dir: Direction) -> &AlsaDirStats {
        match dir {
            Direction::Playback => &self.playback,
            Direction::Capture => &self.capture,
        }
    }

    pub fn snapshot(&self) -> AlsaLegFacet {
        AlsaLegFacet {
            playback: self.playback.snapshot(),
            capture: self.capture.snapshot(),
        }
    }
}

impl AlsaLegFacet {
    /// The card was on the box at the last look of either side.
    pub fn card_present(&self) -> bool {
        let seen = |s: AlsaDirState| {
            matches!(
                s,
                AlsaDirState::Ready(_) | AlsaDirState::Down(AlsaDown::NoFormat)
            )
        };
        seen(self.playback.state) || seen(self.capture.state)
    }
}
