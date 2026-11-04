//! The hub's own Dante device (intercom#5): dstack's `dlink` embedded in this process.
//!
//! strih-snv receives dasio's Dante TX (the FOH master mix + the band's click/guide) as a Dante
//! device of its own. Since the cut-over (3.11.2026, FOH turned `fohabl-strih` off) it carries every
//! point the `fohabl` VBAN leg had; that leg stays declared and routed nowhere. dstack owns
//! the protocol, the subscriptions (kept in `state_file`), dlink's default 15 ms budget (the whole
//! path each way, split by dlink) at normal priority and the media clock: Dante PTP followed in process,
//! the dsync service as fallback (dstack#20, dstack#68, dstack#1). The hub only declares its wanted
//! subscriptions (`[[dlink.subscribe]]`); one Dante Controller sets wins.
//!
//! The crossing into the hub:
//!
//! - dlink's pump calls [`RxProcessor::process`] once per device block on the Dante clock (with
//!   `network_rate`, dlink's `Rated` hands it the hub-rate frames, about the device block x the
//!   rate ratio, one more or less when the ratio does not divide the block). That callback
//!   must not allocate, lock, make syscalls or log (dstack's rule I7), so it only copies the RX
//!   channels as i16 into a preallocated lock-free single-producer ring ([`SampleRing`]).
//! - The `dlink-rx` thread drains the ring in hub blocks and pushes each into the participant's
//!   `JitterBuffer` with its arrival time ([`spawn_dlink`]), exactly as the card leg's `arecord`
//!   does (`alsa_leg`). The card-capture resampler leg (issue 1461) then crosses the Dante clock
//!   into the hub's: no new clock or mixing code.
//! - The `dlink` supervisor thread starts the device, watches for its end, and restarts it after
//!   `restart_backoff`. The hub never exits because of the Dante device.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::local_audio::restart_backoff;
use crate::vban_io::{DecodedAudio, JitterBuffer};

/// The latency budget unless the TOML sets one: dlink's own default (`dlink::BUDGET_NS`, 15 ms since
/// v0.6.28), the hub's whole path each way, the work and the Dante latency together; dlink splits it
/// (dstack#68, dlink v0.6.22). Owner 4.11.2026 (dstack#1): robustness over low latency for dlink
/// apps, and no CPU pinning, so the pump's margin W has to absorb a loaded box's multi-ms preemption
/// (strih-snv at load ~11: 2.7-3.6 ms, which the old 10 ms / W 4 ms barely held). The hub is
/// RX-only, so since v0.6.32 W is the whole budget (`dlink::rx_work_margin`, 15 ms), not 2/5 of it.
pub const DEFAULT_LATENCY_MS: u32 = (dlink::BUDGET_NS / 1_000_000) as u32;
/// Where the device keeps what Dante Controller sets (its name, subscriptions, latency) unless the
/// TOML says otherwise: the unit's `StateDirectory=intercom-hub`.
pub const DEFAULT_STATE_FILE: &str = "/var/lib/intercom-hub/dlink.toml";
/// The ring between the pump and the `dlink-rx` thread, in frames: 16 hub blocks (85 ms), far more
/// than the whole 15 ms budget (the Dante latency is less) plus a stalled `dlink-rx` wake.
pub const RING_FRAMES: usize = 4096;
/// How often `dlink-rx` looks at the ring when it holds less than a block: well under one block.
pub const DRAIN_INTERVAL: Duration = Duration::from_millis(1);
/// How often the supervisor checks the device for its end or the hub's stop.
pub const WATCH_INTERVAL: Duration = Duration::from_millis(200);
/// At most one ring-drop warning per this interval.
pub const DROP_LOG_FLOOR: Duration = Duration::from_secs(10);
/// A run at least this long resets the restart backoff (as the card leg does).
pub const STABLE_RUN: Duration = Duration::from_secs(10);

/// The optional `[dlink]` table: the hub's own Dante device. Absent = no device (strih-pp).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DlinkConfig {
    /// The device name Dante Controller lists (1 to 31 of A-Z a-z 0-9 and '-').
    pub name: String,
    /// The Dante-facing interface (strih-snv: `enp7s0.70`); its one IPv4 address is used.
    pub iface: String,
    /// The RX channel names, in channel order: the `dlink` participant's inputs, 1-based.
    pub rx: Vec<String>,
    /// The latency budget in ms (dlink `Config::budget_ns`): the whole path each way. The Dante
    /// latency is what the work leaves (dlink `split`, at least 4 ms); a budget leaving less fails.
    #[serde(default = "default_latency_ms")]
    pub latency_ms: u32,
    /// Where dstack keeps the device state.
    #[serde(default = "default_state_file")]
    pub state_file: PathBuf,
    /// The ARC / CMC / flow-control ports (UDP). Absent = Dante's 4440 / 8800 / 4455. A second
    /// device on the same address (strih-snv: OBS's strih-obs) needs its own; all three or none.
    #[serde(default)]
    pub arc_port: Option<u16>,
    #[serde(default)]
    pub cmc_port: Option<u16>,
    #[serde(default)]
    pub flows_port: Option<u16>,
    /// The process id that tells this device apart from the others on its address (0 if absent).
    #[serde(default)]
    pub process_id: Option<u16>,
    /// The first of the 32 RX flow ports (base..=base+31), so camera-box's nft can name them.
    /// Absent = ports the OS picks.
    #[serde(default)]
    pub rx_port_base: Option<u16>,
    /// The network's sample rate, Hz, when it differs from the hub's (dlink, dstack #20 / #90):
    /// the device runs at it, since a Dante flow needs both ends at one rate, and dlink's `Rated`
    /// converts to the hub's rate on the device's clock. A Dante rate (44.1..192 kHz); absent =
    /// the hub's. strih-snv: 96000 (the FOH PC's Dante sources).
    #[serde(default)]
    pub network_rate: Option<u32>,
    /// Declared subscriptions (dlink v0.5.12): an RX channel the state file holds no subscription
    /// for takes its declared one at start; one Dante Controller sets is saved there and wins.
    /// Declared ones are never saved (dlink v0.5.12, the fix of intercom#5's report), so a TOML
    /// edit reaches its channel until Dante Controller changes that channel.
    #[serde(default)]
    pub subscribe: Vec<DlinkSubscribe>,
}

/// One `[[dlink.subscribe]]`: our RX channel `rx` (by its name in `rx`) hears `tx_channel` on the
/// Dante device `tx_device` (both Dante's names).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DlinkSubscribe {
    pub rx: String,
    pub tx_channel: String,
    pub tx_device: String,
}

fn default_latency_ms() -> u32 {
    DEFAULT_LATENCY_MS
}
fn default_state_file() -> PathBuf {
    PathBuf::from(DEFAULT_STATE_FILE)
}

impl DlinkConfig {
    /// The dlink config: the hub's `sample_rate` for the app, the device at `network_rate` if set.
    pub fn link_config(&self, sample_rate: u32) -> dlink::Config {
        let mut c = dlink::Config::new(self.name.clone(), self.state_file.clone())
            .rx(self.rx.iter().cloned())
            .sample_rate(sample_rate)
            .budget_ns(u64::from(self.latency_ms) * 1_000_000)
            .product("newlevel", "intercom-hub");
        if let (Some(arc), Some(cmc), Some(flows)) = (self.arc_port, self.cmc_port, self.flows_port)
        {
            c = c.ports(dlink::Ports { arc, cmc, flows });
        }
        if let Some(id) = self.process_id {
            c = c.process_id(id);
        }
        if let Some(base) = self.rx_port_base {
            c = c.rx_port_base(base);
        }
        if let Some(hz) = self.network_rate {
            c = c.network_rate(hz);
        }
        for sub in &self.subscribe {
            // An unknown name is listed by `problems`, so a loaded table never reaches here with one.
            if let Some(i) = self.rx.iter().position(|r| *r == sub.rx) {
                c = c.subscribe(i + 1, sub.tx_channel.clone(), sub.tx_device.clone());
            }
        }
        c
    }

    /// Every problem with the table, dstack's own device checks included (the name, the channel
    /// names, the rate, the latency), so a bad `[dlink]` fails the load, never the rig start.
    pub fn problems(&self, sample_rate: u32) -> Vec<String> {
        let mut out = Vec::new();
        if self.iface.is_empty() || self.iface.chars().any(char::is_whitespace) {
            out.push(format!("iface: an interface name, got '{}'", self.iface));
        }
        let ports = [self.arc_port, self.cmc_port, self.flows_port];
        if ports.iter().any(Option::is_some) && !ports.iter().all(Option::is_some) {
            out.push("arc_port, cmc_port, flows_port: all three or none".to_string());
        }
        if self.rx.is_empty() {
            out.push("rx: at least one RX channel".to_string());
        }
        if !self.state_file.is_absolute() {
            out.push(format!(
                "state_file: an absolute path, got '{}'",
                self.state_file.display()
            ));
        }
        for (n, sub) in self.subscribe.iter().enumerate() {
            if !self.rx.contains(&sub.rx) {
                out.push(format!(
                    "subscribe: rx '{}' is not one of the RX channels {:?}",
                    sub.rx, self.rx
                ));
            }
            if self.subscribe[..n].iter().any(|o| o.rx == sub.rx) {
                out.push(format!("subscribe: rx '{}' is declared twice", sub.rx));
            }
        }
        if let Err(p) = self.link_config(sample_rate).device() {
            out.extend(p);
        }
        out
    }
}

/// A float sample from the Dante engine as the hub's i16: rounded, saturating, NaN as 0.
pub fn to_i16(x: f64) -> i16 {
    // `as` saturates and maps NaN to 0.
    (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

/// A lock-free single-producer / single-consumer ring of interleaved i16 samples. The producer
/// (dlink's pump) and the consumer (`dlink-rx`) never wait on each other: a full ring drops the
/// newest frames and counts them, an empty one hands out nothing. Exactly one producer and one
/// consumer at a time: the supervisor starts a new device only after the old one stopped.
pub struct SampleRing {
    cells: Box<[AtomicI16]>,
    channels: usize,
    /// Samples written so far (the producer's count).
    written: AtomicUsize,
    /// Samples read so far (the consumer's count).
    read: AtomicUsize,
    /// Frames the producer dropped on a full ring.
    dropped_frames: AtomicU64,
}

impl std::fmt::Debug for SampleRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SampleRing")
            .field("channels", &self.channels)
            .field("frames_queued", &self.frames_queued())
            .field("dropped_frames", &self.dropped_frames())
            .finish()
    }
}

impl SampleRing {
    /// A ring of `frames` frames of `channels` channels (both at least 1).
    pub fn new(frames: usize, channels: usize) -> Self {
        let channels = channels.max(1);
        let len = frames.max(1) * channels;
        SampleRing {
            cells: (0..len).map(|_| AtomicI16::new(0)).collect(),
            channels,
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            dropped_frames: AtomicU64::new(0),
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Whole frames waiting for the consumer.
    pub fn frames_queued(&self) -> usize {
        // `read` first: `written` never trails `read`, so a `written` loaded after it cannot wrap
        // below it. Loaded the other way round, a third thread (the block loop's snapshot) saw the
        // consumer pass a stale `written` and read ~usize::MAX (review round 1). Both can still
        // move between the two loads, so a third thread's reading is clamped to the capacity; the
        // producer and the consumer each load their own counter and read it exactly.
        (self.queued_samples_unclamped() / self.channels).min(self.cells.len() / self.channels)
    }

    /// The samples between the two counters before the capacity clamp, `read` loaded first (see
    /// [`SampleRing::frames_queued`]). From a third thread it may overshoot the capacity a little,
    /// but it never wraps; public so a test can pin the load order the clamp would hide.
    pub fn queued_samples_unclamped(&self) -> usize {
        let r = self.read.load(Ordering::Acquire);
        let w = self.written.load(Ordering::Acquire);
        w.wrapping_sub(r)
    }

    /// Frames dropped on a full ring since the start.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    /// PRODUCER: write `frames` frames, sample `(f, ch)` from `sample(f, ch)`. Frames that do not
    /// fit are dropped and counted. No allocation, no lock.
    pub fn push_frames(&self, frames: usize, mut sample: impl FnMut(usize, usize) -> i16) {
        let len = self.cells.len();
        let w = self.written.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        let free_frames = (len - w.wrapping_sub(r)) / self.channels;
        let take = frames.min(free_frames);
        let mut at = w;
        for f in 0..take {
            for ch in 0..self.channels {
                self.cells[at % len].store(sample(f, ch), Ordering::Relaxed);
                at = at.wrapping_add(1);
            }
        }
        self.written.store(at, Ordering::Release);
        if take < frames {
            self.dropped_frames
                .fetch_add((frames - take) as u64, Ordering::Relaxed);
        }
    }

    /// CONSUMER: take exactly `frames` frames as planar channels, or `None` (nothing taken) while
    /// fewer are queued.
    pub fn pop_planar(&self, frames: usize) -> Option<Vec<Vec<i16>>> {
        if self.frames_queued() < frames {
            return None;
        }
        let len = self.cells.len();
        let mut at = self.read.load(Ordering::Relaxed);
        // One producer, one consumer: the consumer never sees more than the ring holds (a second
        // live pump after a bad restart would break this, and the clamp would hide it).
        debug_assert!(
            self.written.load(Ordering::Acquire).wrapping_sub(at) <= len,
            "SampleRing: more queued than the ring holds"
        );
        let mut out = vec![Vec::with_capacity(frames); self.channels];
        for _ in 0..frames {
            for ch_out in out.iter_mut() {
                ch_out.push(self.cells[at % len].load(Ordering::Relaxed));
                at = at.wrapping_add(1);
            }
        }
        self.read.store(at, Ordering::Release);
        Some(out)
    }
}

/// The most RX channels the callback reads (it keeps their slices in a fixed array, no allocation).
pub const MAX_RX_CHANNELS: usize = 16;

/// The device's processor: copies every RX channel into the ring, writes no TX. Runs on dlink's pump
/// thread under rule I7: no allocation, lock, syscall or log.
pub struct RxProcessor {
    ring: Arc<SampleRing>,
}

impl RxProcessor {
    pub fn new(ring: Arc<SampleRing>) -> Self {
        RxProcessor { ring }
    }
}

impl dlink::Process for RxProcessor {
    fn process(&mut self, block: &mut dlink::Block<'_>) {
        let channels = self.ring.channels().min(MAX_RX_CHANNELS);
        let mut inputs: [&[f64]; MAX_RX_CHANNELS] = [&[]; MAX_RX_CHANNELS];
        for (ch, slot) in inputs.iter_mut().enumerate().take(channels) {
            *slot = block.input(ch);
        }
        self.ring.push_frames(block.frames(), |f, ch| {
            inputs
                .get(ch)
                .and_then(|c| c.get(f))
                .map_or(0, |&x| to_i16(x))
        });
        block.zero_outputs();
    }
}

/// The device's counters, written by the supervisor and `dlink-rx`, read by the block loop.
#[derive(Debug, Default)]
pub struct DlinkStats {
    running: AtomicBool,
    starts: AtomicU64,
    ends: AtomicU64,
    blocks: AtomicU64,
    ring: Mutex<Option<Arc<SampleRing>>>,
    /// The device's RX channels as the supervisor last read them (empty while the device is down).
    rx: Mutex<Vec<DlinkRxFacet>>,
}

/// One RX channel on `/api/state` (dlink v0.6.3 `Link::rx_channels`): what it is subscribed to,
/// the subscription's Dante status, the flow it rides and how much arrived. `written` that stops
/// while `state` reads `connected` is a lost flow; `written` that grows over silence is a quiet
/// source (3.11.2026: a lost flow read as a quiet FOH).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DlinkRxFacet {
    /// The channel, from 1.
    pub rx: usize,
    /// Our name for it (`[dlink].rx`).
    pub name: String,
    pub tx_channel: Option<String>,
    pub tx_device: Option<String>,
    /// Dante's label for the subscription status (`Receiving`, `Tx fail`, ...); absent if none.
    pub status: Option<&'static str>,
    /// `connected` / `pending` / `error` / `idle`; absent if not subscribed.
    pub state: Option<&'static str>,
    /// The RX flow it rides, from 1.
    pub flow: Option<usize>,
    /// That flow's packets so far.
    pub packets: u64,
    /// This channel's samples written so far.
    pub written: u64,
}

fn state_name(s: dlink::SubState) -> &'static str {
    match s {
        dlink::SubState::Connected => "connected",
        dlink::SubState::Pending => "pending",
        dlink::SubState::Error => "error",
        dlink::SubState::Idle => "idle",
    }
}

/// The facet rows for dlink's RX channels, named by our `rx` list (by number, from 1).
pub fn rx_facets(names: &[String], channels: &[dlink::RxChannel]) -> Vec<DlinkRxFacet> {
    channels
        .iter()
        .map(|c| DlinkRxFacet {
            rx: c.rx,
            name: c
                .rx
                .checked_sub(1)
                .and_then(|i| names.get(i))
                .cloned()
                .unwrap_or_default(),
            tx_channel: c.subscription.as_ref().map(|s| s.tx_channel.clone()),
            tx_device: c.subscription.as_ref().map(|s| s.tx_device.clone()),
            status: c.status.map(|s| s.label()),
            state: c.status.map(|s| state_name(s.state())),
            flow: c.flow,
            packets: c.packets,
            written: c.written,
        })
        .collect()
}

/// How long a connected channel's `written` may stand still before it counts as a lost flow.
pub const RX_STALL: Duration = Duration::from_secs(1);

/// What changed on one RX channel between two reads, for the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RxEvent {
    /// The subscription's state or status label changed.
    State {
        rx: usize,
        from: Option<&'static str>,
        to: Option<&'static str>,
        status: Option<&'static str>,
    },
    /// Connected, yet no sample written for [`RX_STALL`]: the flow is gone.
    Stalled { rx: usize },
    /// Samples arrive again after a stall.
    Resumed { rx: usize },
}

/// Follows the RX channels between reads (pure: the caller passes the time).
#[derive(Debug, Default)]
pub struct RxWatch {
    last: Vec<(DlinkRxFacet, Instant, bool)>,
}

impl RxWatch {
    /// Compare a fresh read with the last one: state changes, a connected channel whose `written`
    /// stood still for [`RX_STALL`], and its resumption.
    pub fn observe(&mut self, now: Instant, rows: &[DlinkRxFacet]) -> Vec<RxEvent> {
        let mut events = Vec::new();
        let mut next = Vec::with_capacity(rows.len());
        for row in rows {
            let prev = self.last.iter().find(|(p, _, _)| p.rx == row.rx);
            let (since, mut stalled) = match prev {
                Some((p, since, stalled)) => {
                    let changed = p.state != row.state || p.status != row.status;
                    if changed {
                        events.push(RxEvent::State {
                            rx: row.rx,
                            from: p.state,
                            to: row.state,
                            status: row.status,
                        });
                    }
                    if row.written != p.written {
                        if *stalled {
                            events.push(RxEvent::Resumed { rx: row.rx });
                        }
                        (now, false)
                    } else if changed {
                        // A new state gets its own RX_STALL before it can count as a lost flow.
                        (now, *stalled)
                    } else {
                        (*since, *stalled)
                    }
                }
                None => {
                    if row.state.is_some() {
                        events.push(RxEvent::State {
                            rx: row.rx,
                            from: None,
                            to: row.state,
                            status: row.status,
                        });
                    }
                    (now, false)
                }
            };
            if !stalled
                && row.state == Some("connected")
                && now.saturating_duration_since(since) >= RX_STALL
            {
                events.push(RxEvent::Stalled { rx: row.rx });
                stalled = true;
            }
            next.push((row.clone(), since, stalled));
        }
        self.last = next;
        events
    }
}

/// The `/api/state` `dlink` facet of the `dlink` participant (intercom#5). Additive only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DlinkFacet {
    /// The device is up (started and not ended).
    pub running: bool,
    /// Device starts since the hub started.
    pub starts: u64,
    /// Device ends (a start failure, or the device ending by itself) since the hub started.
    pub ends: u64,
    /// Hub blocks pushed into the participant's buffer.
    pub blocks: u64,
    /// Frames waiting in the ring right now.
    pub ring_frames: usize,
    /// Frames dropped on a full ring (`dlink-rx` fell behind).
    pub ring_dropped_frames: u64,
    /// The RX channels as last read (empty while the device is down).
    pub rx: Vec<DlinkRxFacet>,
}

impl DlinkStats {
    pub fn snapshot(&self) -> DlinkFacet {
        let ring = self.ring.lock().ok().and_then(|r| r.clone());
        DlinkFacet {
            running: self.running.load(Ordering::Relaxed),
            starts: self.starts.load(Ordering::Relaxed),
            ends: self.ends.load(Ordering::Relaxed),
            blocks: self.blocks.load(Ordering::Relaxed),
            ring_frames: ring.as_ref().map_or(0, |r| r.frames_queued()),
            ring_dropped_frames: ring.as_ref().map_or(0, |r| r.dropped_frames()),
            rx: self.rx.lock().map(|r| r.clone()).unwrap_or_default(),
        }
    }
}

/// What the device runs with.
#[derive(Debug, Clone)]
pub struct DlinkLegConfig {
    pub config: DlinkConfig,
    /// The `dlink` participant's id and name (the stream name on each pushed block).
    pub participant: usize,
    pub name: String,
    pub sample_rate: u32,
    pub block_frames: usize,
}

/// The running device leg: [`DlinkLeg::stop`] stops the device (its goodbyes sent) and both threads.
pub struct DlinkLeg {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl DlinkLeg {
    /// Stop the device and wait for both threads (the device's own wait is `dlink::STOP_WAIT`).
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// One drain: every whole hub block queued in the ring, pushed into the participant's buffer.
/// Returns the blocks pushed.
pub fn drain_blocks(
    ring: &SampleRing,
    block_frames: usize,
    name: &str,
    participant: usize,
    jitter: &Mutex<Vec<JitterBuffer>>,
) -> u64 {
    let mut pushed = 0;
    while let Some(channels) = ring.pop_planar(block_frames) {
        let audio = DecodedAudio {
            stream_name: name.to_string(),
            channels,
            frames: block_frames,
        };
        // The arrival time, before the lock: the resampler's rate fit times every block by it.
        let arrived = Instant::now();
        if let Ok(mut jb) = jitter.lock() {
            if let Some(b) = jb.get_mut(participant) {
                b.push_at(&audio, arrived);
            }
        }
        pushed += 1;
    }
    pushed
}

/// Start the device leg: the `dlink` supervisor and the `dlink-rx` forwarder.
pub fn spawn_dlink(
    cfg: DlinkLegConfig,
    jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    stats: Arc<DlinkStats>,
) -> DlinkLeg {
    let ring = Arc::new(SampleRing::new(RING_FRAMES, cfg.config.rx.len()));
    if let Ok(mut r) = stats.ring.lock() {
        *r = Some(ring.clone());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let rx = {
        let (ring, stop, stats, name) =
            (ring.clone(), stop.clone(), stats.clone(), cfg.name.clone());
        let (block, pid) = (cfg.block_frames, cfg.participant);
        thread::Builder::new()
            .name("dlink-rx".into())
            .spawn(move || {
                let mut logged_drops = 0u64;
                let mut last_log: Option<Instant> = None;
                while !stop.load(Ordering::Relaxed) {
                    let n = drain_blocks(&ring, block, &name, pid, &jitter);
                    stats.blocks.fetch_add(n, Ordering::Relaxed);
                    // A full ring (this thread fell behind the pump) drops frames: one warning at
                    // most every DROP_LOG_FLOOR, carrying the total.
                    let dropped = ring.dropped_frames();
                    if dropped > logged_drops && last_log.is_none_or(|t| t.elapsed() >= DROP_LOG_FLOOR) {
                        tracing::warn!(participant = %name, dropped_frames = dropped, new = dropped - logged_drops, "dlink: the ring was full — Dante frames dropped");
                        logged_drops = dropped;
                        last_log = Some(Instant::now());
                    }
                    thread::sleep(DRAIN_INTERVAL);
                }
            })
            .expect("spawn the dlink-rx thread")
    };
    let sup = {
        let stop = stop.clone();
        thread::Builder::new()
            .name("dlink".into())
            .spawn(move || supervise(cfg, ring, stats, stop))
            .expect("spawn the dlink thread")
    };
    DlinkLeg {
        stop,
        threads: vec![rx, sup],
    }
}

/// One journal line per RX change: a connect at info, an error or a lost flow at warn.
fn log_rx_event(device: &str, rows: &[DlinkRxFacet], e: &RxEvent) {
    let row = |rx: usize| rows.iter().find(|r| r.rx == rx);
    match e {
        RxEvent::State {
            rx,
            from,
            to,
            status,
        } => {
            let r = row(*rx);
            let (name, tx_channel, tx_device, flow) = (
                r.map(|r| r.name.as_str()),
                r.and_then(|r| r.tx_channel.as_deref()),
                r.and_then(|r| r.tx_device.as_deref()),
                r.and_then(|r| r.flow),
            );
            if *to == Some("error") {
                tracing::warn!(%device, rx, ?name, ?tx_channel, ?tx_device, ?from, ?to, ?status, ?flow, "dlink: RX subscription failed");
            } else {
                tracing::info!(%device, rx, ?name, ?tx_channel, ?tx_device, ?from, ?to, ?status, ?flow, "dlink: RX subscription changed");
            }
        }
        RxEvent::Stalled { rx } => {
            let r = row(*rx);
            tracing::warn!(%device, rx, name = ?r.map(|r| r.name.as_str()), flow = ?r.and_then(|r| r.flow), packets = ?r.map(|r| r.packets), stall_ms = RX_STALL.as_millis(), "dlink: RX channel connected but no samples — the flow is lost");
        }
        RxEvent::Resumed { rx } => {
            tracing::info!(%device, rx, "dlink: RX channel receives samples again");
        }
    }
}

/// Sleep `d` in watch-sized steps; true once the hub asked to stop.
fn sleep_or_stop(d: Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        thread::sleep(WATCH_INTERVAL.min(end.saturating_duration_since(Instant::now())));
    }
    stop.load(Ordering::Relaxed)
}

fn supervise(
    cfg: DlinkLegConfig,
    ring: Arc<SampleRing>,
    stats: Arc<DlinkStats>,
    stop: Arc<AtomicBool>,
) {
    let link_cfg = cfg.config.link_config(cfg.sample_rate);
    let place = dlink::Place::Iface(cfg.config.iface.clone());
    let map = dlink::Map {
        rx: (0..cfg.config.rx.len()).collect(),
        tx: Vec::new(),
    };
    let device = &cfg.config.name;
    let iface = &cfg.config.iface;
    let mut failures: u32 = 0;
    let mut last_reason: Option<String> = None;
    loop {
        if sleep_or_stop(restart_backoff(failures), &stop) {
            return;
        }
        let started = Instant::now();
        let link = match dlink::Link::start(
            &link_cfg,
            &place,
            map.clone(),
            RxProcessor::new(ring.clone()),
        ) {
            Ok(link) => link,
            Err(e) => {
                let reason = format!("start failed: {e}");
                if last_reason.as_deref() != Some(reason.as_str()) {
                    tracing::warn!(%device, %iface, %reason, "dlink: the Dante device is down — retrying");
                    last_reason = Some(reason);
                }
                stats.ends.fetch_add(1, Ordering::Relaxed);
                failures = failures.saturating_add(1);
                continue;
            }
        };
        stats.starts.fetch_add(1, Ordering::Relaxed);
        stats.running.store(true, Ordering::Relaxed);
        tracing::info!(%device, %iface, channels = cfg.config.rx.len(), latency_ms = cfg.config.latency_ms, "dlink: the Dante device is up");
        let mut watch = RxWatch::default();
        let ended = loop {
            if stop.load(Ordering::Relaxed) {
                break None;
            }
            if let Some(why) = link.ended() {
                break Some(why);
            }
            let rows = rx_facets(&cfg.config.rx, &link.rx_channels());
            for e in watch.observe(Instant::now(), &rows) {
                log_rx_event(device, &rows, &e);
            }
            // The old rows are freed after the lock: hub-mix reads this mutex for its snapshot.
            let old = stats
                .rx
                .lock()
                .map(|mut r| std::mem::replace(&mut *r, rows));
            drop(old);
            thread::sleep(WATCH_INTERVAL);
        };
        let stopped = link.stop();
        stats.running.store(false, Ordering::Relaxed);
        let old = stats.rx.lock().map(|mut r| std::mem::take(&mut *r));
        drop(old);
        match ended {
            None => {
                tracing::info!(%device, ?stopped, "dlink: the Dante device stopped with the hub");
                return;
            }
            Some(why) => {
                stats.ends.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%device, reason = %why, ?stopped, "dlink: the Dante device ended — restarting");
                last_reason = Some(why);
                failures = if started.elapsed() >= STABLE_RUN {
                    1
                } else {
                    failures.saturating_add(1)
                };
            }
        }
    }
}
