//! The phones' room (intercom#42, design comment 7041415746 "Per-phone mixing"): one OS thread,
//! "phones-room", on its own 20 ms clock (the `janus_sender` [`PaceSchedule`] pattern), that mixes
//! every phone's own N-1 and encodes it.
//!
//! Each tick:
//!
//! 1. take one 960-frame frame of the phones bus H from the ring the hub-mix thread feeds (the same
//!    [`PacedRing`] the Janus sender pops; the hub-mix thread is unchanged);
//! 2. take one frame of every connected phone's mic mᵢ from its own resampler ring
//!    ([`RoomSlots`], fed by the network task with the decoded Opus, the
//!    `JitterBuffer::janus_capture` / `NetworkFill` pattern: a phone's clock is a foreign clock);
//! 3. push Σmᵢ into the `phones` participant's input (the jitter buffer the engine pops), exactly
//!    where the Janus room mix went;
//! 4. build outᵢ = H + Σ_{j≠i} mⱼ with a skip-self loop ([`mix_minus`]), so a phone never hears
//!    itself by construction;
//! 5. encode each outᵢ (Opus 48 kHz mono, in-band FEC, no DTX: a packet every 20 ms keeps the
//!    page's media watchdog meaningful) into a fixed buffer and hand it to the network task with
//!    `try_send` (a full queue drops the packet and counts it, the thread never waits).
//!
//! The hot path allocates nothing after the first ticks: the frames, the sums and the packet
//! buffers are preallocated per slot; each slot's lock is held for a frame copy only.

use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::janus_codec::{TxEncoder, OPUS_MAX_PACKET};
use crate::janus_pacing::{IntervalStats, PaceSchedule, PacedRing, PoppedFrame, FRAME_48K};
use crate::janus_rtp::JanusCodec;
use crate::janus_sender::INTERVAL_WINDOW;
use crate::vban_io::{DecodedAudio, JitterBuffer};
use crate::webrtc_leg::WebrtcShared;

/// A phone's mic ring target: two 20 ms packets plus a little margin (the Janus ingress default,
/// `LOCAL_CAPTURE_TARGET_FRAMES`), so one late packet on mobile data does not run it dry.
pub const PHONE_MIC_TARGET_FRAMES: usize = 2048;
/// Above this the mic ring resyncs back to the target (120 ms).
pub const PHONE_MIC_CAP_FRAMES: usize = 6 * FRAME_48K;
/// A mic that sent nothing for this long starts over (the phone's sender stopped: a track that
/// was removed, a network outage); its old tail is dropped and the ring re-primes.
pub const PHONE_MIC_STALE: Duration = Duration::from_millis(500);

/// One encoded packet for one phone.
#[derive(Clone)]
pub struct TxPacket {
    pub slot: usize,
    pub generation: u64,
    pub len: usize,
    pub data: [u8; OPUS_MAX_PACKET],
}

impl TxPacket {
    /// The payload.
    pub fn payload(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

/// One slot's mic: the ring and the generation of the phone it belongs to (0 = free).
#[derive(Debug)]
struct PhoneMic {
    generation: u64,
    ring: PacedRing,
    last_push: Option<Instant>,
}

/// Every phone's mic ring, by slot. The network task pushes, the room thread takes.
#[derive(Debug)]
pub struct RoomSlots {
    slots: Vec<Mutex<PhoneMic>>,
}

impl RoomSlots {
    /// `max` free slots.
    pub fn new(max: usize) -> Self {
        RoomSlots {
            slots: (0..max)
                .map(|_| {
                    Mutex::new(PhoneMic {
                        generation: 0,
                        ring: PacedRing::new(PHONE_MIC_TARGET_FRAMES, PHONE_MIC_CAP_FRAMES),
                        last_push: None,
                    })
                })
                .collect(),
        }
    }

    /// Slots, used or not.
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    fn lock(&self, slot: usize) -> Option<std::sync::MutexGuard<'_, PhoneMic>> {
        self.slots
            .get(slot)
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// A phone takes `slot`: its ring starts empty.
    pub fn open(&self, slot: usize, generation: u64) {
        if let Some(mut m) = self.lock(slot) {
            m.generation = generation;
            m.ring.reset();
            m.last_push = None;
        }
    }

    /// The phone of `generation` left `slot` (a stale close is ignored).
    pub fn close(&self, slot: usize, generation: u64) {
        if let Some(mut m) = self.lock(slot) {
            if m.generation == generation {
                m.generation = 0;
                m.ring.reset();
                m.last_push = None;
            }
        }
    }

    /// The generation `slot` holds (0 = free).
    pub fn generation(&self, slot: usize) -> u64 {
        self.lock(slot).map_or(0, |m| m.generation)
    }

    /// Decoded mic audio of the phone of `generation`, received at `at` (stamped before the lock).
    /// False when the slot belongs to another phone now.
    pub fn push_mic(&self, slot: usize, generation: u64, samples: &[i16], at: Instant) -> bool {
        let Some(mut m) = self.lock(slot) else {
            return false;
        };
        if m.generation != generation || generation == 0 {
            return false;
        }
        if m.last_push
            .is_some_and(|t| at.saturating_duration_since(t) > PHONE_MIC_STALE)
        {
            m.ring.reset();
        }
        m.last_push = Some(at);
        m.ring.push_at(samples, at);
        true
    }

    /// `slot`'s mic resampler ratio in ppm off 1 (0 for a free slot).
    pub fn mic_rate_ppm(&self, slot: usize) -> f64 {
        self.lock(slot)
            .filter(|m| m.generation != 0)
            .map_or(0.0, |m| m.ring.stats().rate_ppm)
    }

    /// Take one frame of `slot`'s mic into `frame`: its generation (0 = free, the frame is then
    /// silent).
    fn take(&self, slot: usize, frame: &mut PoppedFrame) -> u64 {
        let Some(mut m) = self.lock(slot) else {
            return 0;
        };
        if m.generation == 0 {
            return 0;
        }
        m.ring.take_frame_into(frame);
        m.generation
    }
}

/// outᵢ = `bus` + Σ_{j≠i} `mics[j]` for every active i, saturated to i16. The own mic is skipped by
/// the loop itself (never added and subtracted), so N-1 is structural. An inactive slot's output is
/// left untouched and an inactive mic never counts.
pub fn mix_minus(bus: &[i16], mics: &[Vec<i16>], active: &[bool], outs: &mut [Vec<i16>]) {
    for (i, out) in outs.iter_mut().enumerate() {
        if !active.get(i).copied().unwrap_or(false) {
            continue;
        }
        for (n, o) in out.iter_mut().enumerate() {
            let mut acc = i32::from(bus.get(n).copied().unwrap_or(0));
            for (j, mic) in mics.iter().enumerate() {
                if j != i && active.get(j).copied().unwrap_or(false) {
                    acc += i32::from(mic.get(n).copied().unwrap_or(0));
                }
            }
            *o = acc.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        }
    }
}

/// Σ of every active mic, saturated: what the hub's mix hears from the phones.
pub fn sum_mics(mics: &[Vec<i16>], active: &[bool], out: &mut [i16]) {
    for (n, o) in out.iter_mut().enumerate() {
        let mut acc = 0i32;
        for (j, mic) in mics.iter().enumerate() {
            if active.get(j).copied().unwrap_or(false) {
                acc += i32::from(mic.get(n).copied().unwrap_or(0));
            }
        }
        *o = acc.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
}

/// Where the room's packets go.
pub trait PacketSink {
    /// Hand one packet on; false = dropped (the queue is full or gone).
    fn offer(&mut self, packet: TxPacket) -> bool;
}

impl PacketSink for tokio::sync::mpsc::Sender<TxPacket> {
    fn offer(&mut self, packet: TxPacket) -> bool {
        self.try_send(packet).is_ok()
    }
}

/// Everything the room needs from the rest of the hub.
pub struct RoomIo<S: PacketSink> {
    /// The phones bus H the hub-mix thread pushes (mono, the `phones` participant's output).
    pub bus: Arc<Mutex<PacedRing>>,
    /// The phones' mic rings.
    pub slots: Arc<RoomSlots>,
    /// The hub's input buffers; Σ mics goes into `phones_id`'s.
    pub jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    pub phones_id: usize,
    /// The encoded packets' way to the network task.
    pub sink: S,
    pub stats: Arc<WebrtcShared>,
}

/// One tick's worth of state, preallocated per slot. The thread owns it; the tests drive
/// [`PhonesRoom::tick`] directly.
pub struct PhonesRoom<S: PacketSink> {
    io: RoomIo<S>,
    bus_frame: PoppedFrame,
    bus: Vec<i16>,
    popped: Vec<PoppedFrame>,
    mics: Vec<Vec<i16>>,
    generations: Vec<u64>,
    active: Vec<bool>,
    outs: Vec<Vec<i16>>,
    encoders: Vec<(u64, TxEncoder)>,
    sum: DecodedAudio,
    packet: TxPacket,
}

impl<S: PacketSink> PhonesRoom<S> {
    /// A room for `io.slots.capacity()` phones (one Opus encoder per slot, made here so a failure
    /// stops the leg at start instead of killing the thread).
    pub fn new(io: RoomIo<S>) -> anyhow::Result<Self> {
        let n = io.slots.capacity();
        let mut encoders = Vec::with_capacity(n);
        for _ in 0..n {
            encoders.push((0, TxEncoder::new(JanusCodec::Opus)?));
        }
        Ok(PhonesRoom {
            io,
            bus_frame: PoppedFrame::empty(),
            bus: vec![0; FRAME_48K],
            popped: (0..n).map(|_| PoppedFrame::empty()).collect(),
            mics: vec![vec![0; FRAME_48K]; n],
            generations: vec![0; n],
            active: vec![false; n],
            outs: vec![vec![0; FRAME_48K]; n],
            encoders,
            sum: DecodedAudio {
                stream_name: "webrtc-phones".to_string(),
                channels: vec![Vec::with_capacity(FRAME_48K)],
                frames: FRAME_48K,
            },
            packet: TxPacket {
                slot: 0,
                generation: 0,
                len: 0,
                data: [0; OPUS_MAX_PACKET],
            },
        })
    }

    /// One 20 ms tick at `now`: take H and every mic, feed Σ mics to the hub, mix, encode, send.
    pub fn tick(&mut self, now: Instant) {
        let stats = &self.io.stats;
        {
            let mut ring = self.io.bus.lock().unwrap_or_else(|e| e.into_inner());
            ring.take_frame_into(&mut self.bus_frame);
            stats.publish_bus(ring.underflows(), ring.trims(), &ring.stats());
        }
        self.bus_frame.render_into(&mut self.bus);

        let mut any = false;
        for slot in 0..self.popped.len() {
            let generation = self.io.slots.take(slot, &mut self.popped[slot]);
            self.generations[slot] = generation;
            self.active[slot] = generation != 0;
            any |= generation != 0;
            if generation != 0 {
                self.popped[slot].render_into(&mut self.mics[slot]);
            } else {
                self.mics[slot].fill(0);
            }
        }
        stats.room_ticks.fetch_add(1, Ordering::Relaxed);
        if !any {
            return;
        }

        // Σ mics into the phones participant, stamped before the jitter lock like every ingress.
        let ch = &mut self.sum.channels[0];
        ch.resize(FRAME_48K, 0);
        sum_mics(&self.mics, &self.active, ch);
        if let Ok(mut jb) = self.io.jitter.lock() {
            if let Some(b) = jb.get_mut(self.io.phones_id) {
                b.push_at(&self.sum, now);
            }
        }

        mix_minus(&self.bus, &self.mics, &self.active, &mut self.outs);
        for slot in 0..self.outs.len() {
            if !self.active[slot] {
                continue;
            }
            let generation = self.generations[slot];
            let (enc_gen, enc) = &mut self.encoders[slot];
            if *enc_gen != generation {
                // A new phone in this slot: a clean encoder (its first packet is a fresh start).
                let _ = enc.reset();
                *enc_gen = generation;
            }
            match enc.encode_into(&self.outs[slot], &mut self.packet.data) {
                Ok(len) => {
                    self.packet.slot = slot;
                    self.packet.generation = generation;
                    self.packet.len = len;
                    if self.io.sink.offer(self.packet.clone()) {
                        stats.room_packets.fetch_add(1, Ordering::Relaxed);
                    } else {
                        stats.room_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(_) => {
                    stats.room_encode_errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// Start the "phones-room" thread. It runs for the life of the process.
pub fn spawn_phones_room<S: PacketSink + Send + 'static>(
    room: PhonesRoom<S>,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("phones-room".into())
        .spawn(move || room_loop(room))
}

fn room_loop<S: PacketSink>(mut room: PhonesRoom<S>) {
    let origin = Instant::now();
    let mut schedule = PaceSchedule::new();
    let mut intervals = IntervalStats::new(INTERVAL_WINDOW);
    let mut resyncs_seen = 0;
    tracing::info!(
        slots = room.popped.len(),
        "webrtc: phones-room running on its own 20 ms clock (per-phone N-1, Opus FEC, no DTX)"
    );
    loop {
        let wait = schedule.wait(origin.elapsed());
        if !wait.is_zero() {
            thread::sleep(wait);
        }
        room.tick(Instant::now());
        intervals.record(origin.elapsed());
        room.io.stats.publish_intervals(&intervals);
        schedule.advance(origin.elapsed());
        if schedule.resyncs() != resyncs_seen {
            resyncs_seen = schedule.resyncs();
            tracing::warn!(
                resyncs = resyncs_seen,
                "webrtc: phones-room stalled for more than 100 ms — restarted its 20 ms grid"
            );
        }
    }
}
