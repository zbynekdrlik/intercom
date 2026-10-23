//! What a received VBAN datagram is to the hub, read BEFORE its payload is decoded (issue 1467).
//!
//! 23.10.2026: the FOH console sent `fohabl-strih` as 24-bit PCM to a hub built before the PCM24
//! decode. The receive path decoded first and dropped the error with no counter and no log, so the
//! configured FOH stream read exactly like a sender that had stopped (the cutters lost FOH for
//! ~90 min). Here the stream name comes from the raw header, the matrix lookup decides known vs
//! foreign, and only then is the payload decoded ([`crate::vban_io::decode_packet`]):
//!
//! * a known stream that decodes is [`RoutedPacket::Audio`];
//! * a known stream that does not (an unsupported codec, a non-audio sub-protocol, a payload without
//!   one whole frame) is [`RoutedPacket::Undecodable`]: counted per participant in
//!   [`DecodeRejects`] and logged, never silent;
//! * anything else is [`RoutedPacket::Foreign`], ignored silently like the cambox receiver.
//!
//! The log is one warning per transition (the first reject, a changed reason) and one info line when
//! the stream decodes again, with at most one line per stream every [`DECODE_LOG_FLOOR`]: a sender
//! that mixes good and bad packets under one name never logs at packet rate (the suppressed
//! transitions are counted on the next line).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use intercom_vban::{VBAN_HEADER_SIZE, VBAN_MAGIC};

use crate::vban_io::{decode_packet, DecodedAudio};

/// At most one decode log line per stream in this span (issue 1467).
pub const DECODE_LOG_FLOOR: Duration = Duration::from_secs(10);

/// The VBAN stream name of a packet, read from the raw header bytes before anything else is
/// decoded: the magic + the 16-byte name field (cut at the first NUL, like
/// `VbanHeader::stream_name_str`), whatever the sub-protocol or codec. `None` for a datagram that is
/// not VBAN (too short, another magic) or whose name is not UTF-8 (never a matrix name).
pub fn vban_stream_name(data: &[u8]) -> Option<&str> {
    if data.len() < VBAN_HEADER_SIZE || &data[0..4] != VBAN_MAGIC {
        return None;
    }
    let field = &data[8..24];
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    std::str::from_utf8(&field[..end]).ok()
}

/// What one received datagram is to the hub.
#[derive(Debug)]
pub enum RoutedPacket {
    /// A known stream that decoded: its participant, its planar audio and the header rate in Hz
    /// (`0` for a reserved rate index, which the rate converter rejects and counts).
    Audio {
        id: usize,
        audio: DecodedAudio,
        sample_rate: u32,
    },
    /// A known stream (its name is in the matrix) whose packet cannot be decoded: an unsupported
    /// codec / data type, a non-audio sub-protocol, a payload without one whole frame. Counted per
    /// participant and logged, never dropped silently.
    Undecodable {
        id: usize,
        stream: String,
        reason: String,
    },
    /// Not VBAN, or a stream name the matrix does not know: foreign traffic, ignored silently.
    Foreign,
}

/// Classify one datagram: read the stream name from the header first, look it up, and only then
/// decode the payload, so a known sender in a format the hub cannot read is told apart from
/// foreign traffic.
pub fn classify_packet(known_streams: &HashMap<String, usize>, data: &[u8]) -> RoutedPacket {
    let Some(name) = vban_stream_name(data) else {
        return RoutedPacket::Foreign;
    };
    let Some(&id) = known_streams.get(name) else {
        return RoutedPacket::Foreign;
    };
    match decode_packet(data) {
        Ok((audio, sample_rate)) => RoutedPacket::Audio {
            id,
            audio,
            sample_rate,
        },
        Err(e) => RoutedPacket::Undecodable {
            id,
            stream: name.to_string(),
            reason: format!("{e:#}"),
        },
    }
}

/// Decode + demux one packet: `Some((participant_id, audio, sample_rate))` if the stream name maps
/// to a known participant and the packet decodes, `None` otherwise. The receive task uses
/// [`classify_packet`], which also reports a known stream that does not decode.
pub fn route_packet(
    known_streams: &HashMap<String, usize>,
    data: &[u8],
) -> Option<(usize, DecodedAudio, u32)> {
    match classify_packet(known_streams, data) {
        RoutedPacket::Audio {
            id,
            audio,
            sample_rate,
        } => Some((id, audio, sample_rate)),
        RoutedPacket::Undecodable { .. } | RoutedPacket::Foreign => None,
    }
}

/// A transition of a stream's decode state, logged once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeTransition {
    /// The stream started to be rejected, or the reason changed: a warning.
    Rejecting,
    /// The stream decodes again after rejects: an info line.
    Recovered,
}

/// One known VBAN stream's decode-reject count, its current reject reason and its log floor. It
/// lives in the receive task (no lock); the count is published through
/// [`crate::vban_rate::VbanRateStats::publish_decode_rejects`] for `/api/state` + the status line.
#[derive(Debug, Clone, Default)]
pub struct DecodeRejects {
    count: u64,
    rejecting: Option<String>,
    last_log: Option<Instant>,
    suppressed: u64,
    /// A transition was suppressed by the floor and its state is not in the journal yet: the next
    /// note after the floor logs the stream's CURRENT state (review round 2: otherwise a stream that
    /// flipped back inside the floor and stayed there left the opposite state as the last line).
    pending: bool,
}

impl DecodeRejects {
    /// Count one undecodable packet with `reason`. `Some(Rejecting)` when it is the first reject
    /// since the stream last decoded (or ever) or the reason changed.
    pub fn reject(&mut self, reason: &str) -> Option<DecodeTransition> {
        self.count += 1;
        if self.rejecting.as_deref() == Some(reason) {
            return None;
        }
        self.rejecting = Some(reason.to_string());
        Some(DecodeTransition::Rejecting)
    }

    /// A packet of the stream decoded. `Some(Recovered)` once after rejects; the count stays.
    pub fn accept(&mut self) -> Option<DecodeTransition> {
        self.rejecting.take().map(|_| DecodeTransition::Recovered)
    }

    /// Undecodable packets of this stream since the hub started.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Whether a transition at `now` may be logged: `Some(n)` when no line was logged within
    /// [`DECODE_LOG_FLOOR`] (`n` = the transitions suppressed since the last line, then reset),
    /// `None` when it is suppressed (counted, and its state left pending for [`Self::pending_due`]).
    pub fn log_due(&mut self, now: Instant) -> Option<u64> {
        if self.within_floor(now) {
            self.suppressed += 1;
            self.pending = true;
            return None;
        }
        Some(self.stamp(now))
    }

    /// A packet that is NOT a transition: `Some(n)` once the floor has passed since the last line
    /// while a suppressed transition is pending (log the current state, `n` suppressed), else `None`.
    /// It never counts: only transitions are `suppressed_transitions` (review round 3).
    pub fn pending_due(&mut self, now: Instant) -> Option<u64> {
        if !self.pending || self.within_floor(now) {
            return None;
        }
        Some(self.stamp(now))
    }

    /// A line was logged within [`DECODE_LOG_FLOOR`] before `now`.
    fn within_floor(&self, now: Instant) -> bool {
        self.last_log
            .is_some_and(|last| now.saturating_duration_since(last) < DECODE_LOG_FLOOR)
    }

    /// A line is logged at `now`: stamp it, clear the pending state, hand out the suppressed count.
    fn stamp(&mut self, now: Instant) -> u64 {
        self.last_log = Some(now);
        self.pending = false;
        std::mem::take(&mut self.suppressed)
    }

    /// Whether the transition `t` of this packet (or a pending earlier one) is to be logged now.
    fn due(&mut self, t: Option<DecodeTransition>, now: Instant) -> Option<u64> {
        match t {
            Some(_) => self.log_due(now),
            None => self.pending_due(now),
        }
    }
}

/// Count an undecodable packet of a known stream and log its state (stream, sender, reason) on a
/// transition, at most one line per [`DECODE_LOG_FLOOR`] (a suppressed one logged by the first note
/// after the floor), so a silent leg names its cause in the journal. `true` = a line was logged.
pub fn note_undecodable(
    rejects: &mut DecodeRejects,
    stream: &str,
    from: SocketAddr,
    reason: &str,
    now: Instant,
) -> bool {
    let transition = rejects.reject(reason);
    let Some(suppressed) = rejects.due(transition, now) else {
        return false;
    };
    tracing::warn!(
        stream = %stream,
        sender = %from,
        reason = %reason,
        decode_rejects = rejects.count(),
        suppressed_transitions = suppressed,
        "VBAN stream of a known participant cannot be decoded: its packets are DROPPED (issue 1467)"
    );
    true
}

/// A packet of a known stream decoded: log it when it follows rejects (under the same floor, a
/// suppressed recovery logged by the first note after it). `true` = a line was logged.
pub fn note_decoded(
    rejects: &mut DecodeRejects,
    stream: &str,
    from: SocketAddr,
    now: Instant,
) -> bool {
    let transition = rejects.accept();
    let Some(suppressed) = rejects.due(transition, now) else {
        return false;
    };
    tracing::info!(
        stream = %stream,
        sender = %from,
        decode_rejects = rejects.count(),
        suppressed_transitions = suppressed,
        "VBAN stream decodes again (issue 1467)"
    );
    true
}
