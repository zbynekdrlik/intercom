//! The Janus leg's paced sender (issue 1345, 25.9.2026: the phone voice sounds robotic).
//!
//! A dedicated OS thread with its own monotonic clock. Every [`crate::janus_pacing::TICK`] (20 ms)
//! it pops exactly one frame from the [`PacedRing`] the block loop feeds, encodes it
//! ([`TxEncoder`]) and sends one RTP packet to the current Janus session. So packets leave on a
//! steady 20 ms grid whatever the 5.33 ms mix-block phase is, and one packet per tick keeps the RTP
//! timestamps contiguous.
//!
//! It is an OS thread, not a tokio task: `std::thread::sleep` to an absolute deadline wakes within
//! tens of µs, while the tokio timer wheel rounds to whole milliseconds. The async session task
//! ([`crate::janus_rtp::run_janus_participant`]) owns the Janus HTTP session and the receive side.
//! It tells this thread where to send through [`PacedSenderShared::set_target`]. The thread reports
//! a failed send back through a flag, which makes the session task re-establish the session.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crate::janus_codec::TxEncoder;
use crate::janus_pacing::{IntervalStats, PaceSchedule, PacedRing};
use crate::janus_rtp::{JanusCodec, JanusSharedStats, RtpPacketizer};

/// The send-spacing window behind `tx_interval_ms_sd` / `tx_interval_ms_max`: the last 5 s.
pub const INTERVAL_WINDOW: usize = 250;

/// Where the sender sends: the Janus RTP address of one established session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxTarget {
    pub addr: SocketAddr,
    /// A number that changes with every new session (resets the encoder, re-arms the marker bit).
    pub session: u64,
}

/// The state the session task and the sender thread share.
#[derive(Debug, Default)]
pub struct PacedSenderShared {
    target: Mutex<Option<TxTarget>>,
    send_failed: AtomicBool,
}

impl PacedSenderShared {
    /// Point the sender at a session (`None` = no session: keep ticking, send nothing).
    pub fn set_target(&self, target: Option<TxTarget>) {
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = target;
        self.send_failed.store(false, Ordering::Relaxed);
    }

    fn target(&self) -> Option<TxTarget> {
        *self.target.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True once after a send failed since the last check (the session task re-establishes).
    pub fn take_send_failed(&self) -> bool {
        self.send_failed.swap(false, Ordering::Relaxed)
    }
}

/// What the sender thread needs besides the shared state.
pub struct PacedSenderConfig {
    /// The RTP socket (a clone of the one the session task receives on, so Janus sees one address).
    pub socket: UdpSocket,
    pub codec: JanusCodec,
    pub ssrc: u32,
    /// The encoder for `codec`, created by the caller so a failure disables the adapter up front
    /// instead of silently killing this thread.
    pub encoder: TxEncoder,
}

/// Start the paced sender thread. It runs for the life of the process.
pub fn spawn_paced_sender(
    cfg: PacedSenderConfig,
    ring: Arc<Mutex<PacedRing>>,
    shared: Arc<PacedSenderShared>,
    stats: Arc<JanusSharedStats>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("janus-paced-tx".into())
        .spawn(move || paced_sender_loop(cfg, &ring, &shared, &stats))
}

fn paced_sender_loop(
    cfg: PacedSenderConfig,
    ring: &Mutex<PacedRing>,
    shared: &PacedSenderShared,
    stats: &JanusSharedStats,
) {
    let PacedSenderConfig {
        socket,
        codec,
        ssrc,
        mut encoder,
    } = cfg;
    let mut packetizer = RtpPacketizer::with_payload_type(ssrc, codec.payload_type());
    let rtp_step = codec.rtp_samples_per_frame();
    let origin = Instant::now();
    let mut schedule = PaceSchedule::new();
    let mut intervals = IntervalStats::new(INTERVAL_WINDOW);
    let mut session_seen: Option<u64> = None;
    let mut warned_this_session = false;
    let mut resyncs_seen = 0;
    tracing::info!(
        codec = codec.as_str(),
        "janus: paced sender running on its own 20 ms clock"
    );

    loop {
        let wait = schedule.wait(origin.elapsed());
        if !wait.is_zero() {
            thread::sleep(wait);
        }

        // Always pop, joined or not: the ring stays drained and the grid never pauses.
        let (frame, underflows, trims) = {
            let mut r = ring.lock().unwrap_or_else(|e| e.into_inner());
            let (frame, _) = r.pop_frame();
            (frame, r.underflows(), r.trims())
        };
        stats.publish_ring(underflows, trims);

        match shared.target() {
            None => {
                if session_seen.take().is_some() {
                    intervals.reset();
                    stats.publish_intervals(&intervals);
                }
            }
            Some(target) => {
                if session_seen != Some(target.session) {
                    session_seen = Some(target.session);
                    warned_this_session = false;
                    if let Err(e) = encoder.reset() {
                        tracing::warn!(error = %e, "janus: encoder reset failed");
                    }
                    packetizer.mark_silence();
                    intervals.reset();
                }
                match encoder.encode(&frame) {
                    Ok(payload) => {
                        let pkt = packetizer.packetize_samples(&payload, rtp_step);
                        match socket.send_to(&pkt, target.addr) {
                            Ok(_) => {
                                intervals.record(origin.elapsed());
                                stats.count_tx();
                                stats.publish_intervals(&intervals);
                            }
                            // The socket is non-blocking (shared with the async receiver): a full
                            // send buffer drops this one packet, it is not a dead session.
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                tracing::debug!("janus: RTP send buffer full — one packet dropped");
                            }
                            Err(e) => {
                                if !warned_this_session {
                                    warned_this_session = true;
                                    tracing::warn!(dest = %target.addr, error = %e, "janus: RTP send failed — re-establishing");
                                }
                                shared.send_failed.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(e) => {
                        // RTP time still moves on: Janus sees one lost packet, not a stream that
                        // falls 20 ms behind the wall clock.
                        packetizer.skip(rtp_step);
                        if !warned_this_session {
                            warned_this_session = true;
                            tracing::warn!(error = %e, "janus: encode failed — frame dropped");
                        }
                    }
                }
            }
        }

        schedule.advance(origin.elapsed());
        if schedule.resyncs() != resyncs_seen {
            resyncs_seen = schedule.resyncs();
            tracing::warn!(
                resyncs = resyncs_seen,
                "janus: paced sender stalled for more than 100 ms — restarted its 20 ms grid"
            );
        }
    }
}
