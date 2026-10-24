//! Issue 1401: the two-clock + arrival-jitter bench for a VBAN network leg, re-pinned by issue 1461
//! onto the resampler leg (the single-frame servo it first measured is gone).
//!
//! A sender on its own clock (the FOH desk's Dante tick, a cambox headset ADC: `ppm` against the
//! hub) produces 48 kHz audio and sends it as packets; each packet reaches the hub after a Gaussian
//! one-way delay, in order (UDP on one LAN path never overtakes); the hub pops one 256-frame block
//! on its exact 5.333 ms grid, every tick waking up to 1 ms late (tokio rounds a deadline up to
//! the next ms). The bench drives the real [`NetworkFill`] controller on frame counts alone (every
//! packet reported with its arrival time, as the buffer does), so hours of simulated time run in
//! seconds, and it measures everything itself (the pre-pop depth ahead of the read point and its
//! 1 s means) rather than trusting the controller's numbers. Since issue 1461 a clean run means: no
//! silent block, no skip, no resync, every 1 s mean depth inside +-1/4 of the FIXED target, and the
//! resampler's ratio on the sender's rate.
//!
//! Realistic means: +-20 ppm (the measured `fohabl-strih` is +0.5 ppm) and a 1 ms sd jitter. The
//! live underrun pattern of the old buffer (one every 2-15 min with well under one block of margin,
//! reproduced on issue 1401 with a 0.2 ms sd) puts the real jitter far below that. The FOH feed is
//! 103 frames at 96 kHz per packet = 51/52 at 48 kHz after decimation (~934 packets/s); a cambox
//! sends 128 mono frames (375 packets/s).

use std::collections::VecDeque;

use intercom_hub::asrc::{ASRC_LOOKAHEAD, ASRC_MAX_PPM};
use intercom_hub::vban_jitter::{NetworkFill, PopPlan, VBAN_CAP_BLOCKS, VBAN_TARGET_BLOCKS};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;
const TARGET: usize = VBAN_TARGET_BLOCKS * BLOCK;
const CAP: usize = VBAN_CAP_BLOCKS * BLOCK;
/// Pops per 1 s measurement window (48 000 / 256 = 187.5, rounded up).
const WINDOW_POPS: usize = 188;
/// Depth and band statistics start after this (the start-up prime + the rate's first fit and
/// acquisition, a few seconds).
const SETTLE_S: f64 = 30.0;
/// The phase term's dead band: a quarter of the target.
const BAND: f64 = TARGET as f64 / 4.0;
/// "Recovered" after a stall = every 1 s mean pre-pop depth is back inside the dead band. (The
/// issue-1401 servo walked the fill back to within 64 frames of the target; the fixed-latency leg
/// re-primes exactly at the target and then never moves the depth inside the band.)
const RECOVERED_BAND: f64 = BAND;
/// The FOH program feed after the 96 -> 48 kHz decimation.
const FOH: &[usize] = &[51, 52];
/// A cambox's mono talkback packet.
const CAMBOX: &[usize] = &[128];

/// splitmix64 + Box-Muller: deterministic and std-only.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1).
    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn gauss(&mut self) -> f64 {
        (-2.0 * self.unit().ln()).sqrt() * (std::f64::consts::TAU * self.unit()).cos()
    }
}

#[derive(Debug, Clone, Copy)]
enum StallKind {
    /// The packets sent during the stall are held and then all arrive at its end.
    Burst,
    /// The packets sent during the stall never arrive.
    Lost,
}

#[derive(Debug, Clone, Copy)]
struct Stall {
    at_s: f64,
    len_s: f64,
    kind: StallKind,
    /// Repeats every this many seconds from `at_s` (a cambox muted and unmuted), else once.
    every_s: Option<f64>,
}

impl Stall {
    /// Whether a packet sent at `send` falls inside the stall; `Some(end)` = the stall's end.
    fn covering(&self, send: f64) -> Option<f64> {
        if send < self.at_s {
            return None;
        }
        let start = match self.every_s {
            Some(every) => self.at_s + ((send - self.at_s) / every).floor() * every,
            None => self.at_s,
        };
        (send < start + self.len_s).then_some(start + self.len_s)
    }
}

#[derive(Debug, Clone)]
struct Scenario {
    /// Frames per packet at 48 kHz, cycled.
    packets: &'static [usize],
    /// Packets sent together once the last one's audio exists (1 = evenly paced).
    burst: usize,
    /// The sender's clock against the hub's.
    ppm: f64,
    jitter_sd_ms: f64,
    base_delay_ms: f64,
    /// Every hub tick wakes up uniformly 0..this late.
    hub_late_ms: f64,
    /// One LOST hub tick (a block lost on every output; since design 5980775411 only the part of a
    /// late wake beyond the four ticks the block loop runs late) every this many seconds.
    hub_skip_every_s: Option<f64>,
    stall: Option<Stall>,
    secs: f64,
    seed: u64,
}

impl Scenario {
    fn new(packets: &'static [usize], ppm: f64, jitter_sd_ms: f64, secs: f64, seed: u64) -> Self {
        Scenario {
            packets,
            burst: 1,
            ppm,
            jitter_sd_ms,
            base_delay_ms: 1.0,
            hub_late_ms: 1.0,
            hub_skip_every_s: None,
            stall: None,
            secs,
            seed,
        }
    }
}

#[derive(Debug, Default)]
struct Outcome {
    /// Pops that found a primed stream short of a block (each one underrun).
    ran_dry: u64,
    /// Above-cap resyncs.
    overruns: u64,
    /// Silent blocks after the first audio (each one an audible gap).
    silent_after_prime: u64,
    /// Frames skipped at a re-prime after the first audio (each one a jump in the audio).
    skipped_after_prime: usize,
    /// The lowest pre-pop depth of an audio pop after the settle.
    min_depth: f64,
    /// The highest pre-pop depth after the settle.
    max_depth: f64,
    /// The largest distance of a 1 s mean pre-pop depth from the target after the settle.
    max_window_dev: f64,
    /// The largest distance of the resampler's ratio from the sender's rate (ppm, clamped to the
    /// resampler's range) at a window's end after the settle.
    max_rate_dev_ppm: f64,
    /// Frames given up at once after a missed hub tick.
    discarded: usize,
    /// After a stall: seconds from its end until the last time the leg was not primed or its 1 s
    /// mean depth was outside the recovered band.
    recovered_s: f64,
}

/// The sender side: lazily generated packets in arrival order.
struct Sender<'a> {
    sc: &'a Scenario,
    rng: Rng,
    rate: f64,
    idx: usize,
    produced: usize,
    group_send_t: f64,
    last_arrival: f64,
}

impl<'a> Sender<'a> {
    fn new(sc: &'a Scenario) -> Self {
        Sender {
            sc,
            rng: Rng(sc.seed),
            rate: RATE * (1.0 + sc.ppm * 1e-6),
            idx: 0,
            produced: 0,
            group_send_t: 0.0,
            last_arrival: 0.0,
        }
    }

    fn size(&self, i: usize) -> usize {
        self.sc.packets[i % self.sc.packets.len()]
    }

    /// The next packet that reaches the hub: `(arrival time s, frames)`.
    fn next(&mut self) -> (f64, usize) {
        loop {
            let i = self.idx;
            if i.is_multiple_of(self.sc.burst) {
                // A group leaves once the audio of its last packet exists.
                let group: usize = (i..i + self.sc.burst).map(|j| self.size(j)).sum();
                self.group_send_t = (self.produced + group) as f64 / self.rate;
            }
            let n = self.size(i);
            self.idx += 1;
            self.produced += n;
            let send = self.group_send_t;
            let delay =
                (self.sc.base_delay_ms + self.sc.jitter_sd_ms * self.rng.gauss()).max(0.0) * 1e-3;
            let mut arrival = (send + delay).max(self.last_arrival);
            if let Some((st, end)) = self
                .sc
                .stall
                .and_then(|st| st.covering(send).map(|e| (st, e)))
            {
                match st.kind {
                    StallKind::Lost => continue,
                    StallKind::Burst => arrival = arrival.max(end),
                }
            }
            self.last_arrival = arrival;
            return (arrival, n);
        }
    }
}

fn run(sc: &Scenario) -> Outcome {
    let mut ctl = NetworkFill::new(TARGET, CAP);
    let mut sender = Sender::new(sc);
    let mut rng = Rng(sc.seed ^ 0xA5A5_A5A5);
    let period = BLOCK as f64 / RATE;
    let pops = (sc.secs / period) as usize;
    let skip_every = sc.hub_skip_every_s.map(|s| ((s / period) as usize).max(2));
    let stall_end = sc
        .stall
        .filter(|st| st.every_s.is_none())
        .map(|st| st.at_s + st.len_s);

    let mut out = Outcome {
        min_depth: f64::INFINITY,
        ..Outcome::default()
    };
    let mut fill = 0usize;
    let mut next = sender.next();
    let mut primed_once = false;
    let mut window: VecDeque<f64> = VecDeque::with_capacity(WINDOW_POPS);
    let mut last_bad_t = 0.0f64;
    let mut missed = 0u64;
    let mut last_arrival: Option<f64> = None;
    let sender_ppm = sc.ppm.clamp(-ASRC_MAX_PPM, ASRC_MAX_PPM);

    for k in 1..=pops {
        if skip_every.is_some_and(|n| k.is_multiple_of(n)) {
            missed += 1;
            continue;
        }
        let t = k as f64 * period + rng.unit() * sc.hub_late_ms * 1e-3;
        while next.0 <= t {
            // The buffer's stale rule (`JitterBuffer::push_at`): a packet after more than 500 ms
            // of silence (a muted cambox unmuted) drops the old tail and starts the leg over.
            if last_arrival.is_some_and(|l| next.0 - l > 0.5) {
                ctl.restart_after_stall();
                fill = 0;
            }
            last_arrival = Some(next.0);
            fill += next.1;
            ctl.note_arrival(next.1, next.0);
            if let Some(keep) = ctl.overrun_keep(fill) {
                fill = keep;
                out.overruns += 1;
            }
            next = sender.next();
        }
        // The block loop gives up a lost tick's block before the pop, as `main.rs` does.
        let discard = ctl.discard_for_missed_ticks(fill, BLOCK, missed);
        fill -= discard;
        out.discarded += discard;
        missed = 0;
        let settled = t >= SETTLE_S;
        match ctl.plan_pop(fill, BLOCK) {
            PopPlan::Silent { ran_dry } => {
                out.ran_dry += u64::from(ran_dry);
                out.silent_after_prime += u64::from(primed_once);
                window.clear();
                last_bad_t = t;
            }
            PopPlan::Audio {
                skip,
                read,
                consume,
            } => {
                if primed_once {
                    out.skipped_after_prime += skip;
                }
                primed_once = true;
                // The depth this pop saw: the frames queued ahead of its read point.
                let depth = read.depth(fill - skip);
                fill -= skip + consume;
                if settled {
                    out.min_depth = out.min_depth.min(depth);
                    out.max_depth = out.max_depth.max(depth);
                }
                window.push_back(depth);
                if window.len() == WINDOW_POPS {
                    let mean = window.iter().sum::<f64>() / WINDOW_POPS as f64;
                    let dev = (mean - TARGET as f64).abs();
                    if settled {
                        out.max_window_dev = out.max_window_dev.max(dev);
                        let rate = ctl.stats().rate_ppm;
                        out.max_rate_dev_ppm = out.max_rate_dev_ppm.max((rate - sender_ppm).abs());
                    }
                    if dev > RECOVERED_BAND {
                        last_bad_t = t;
                    }
                    window.clear();
                }
            }
        }
    }
    out.recovered_s = stall_end.map_or(0.0, |end| (last_bad_t - end).max(0.0));
    out
}

/// The realistic-jitter verdict: not one gap, not one skip, not one resync, the depth never closer
/// than 2 ms to an underrun, every 1 s mean depth inside the dead band, and the ratio on the
/// sender's rate within `rate_tol_ppm`.
fn assert_clean(sc: &Scenario, o: &Outcome, rate_tol_ppm: f64) {
    let ctx = format!("{sc:?}\n{o:?}");
    assert_eq!(o.ran_dry, 0, "no underrun at realistic jitter\n{ctx}");
    assert_eq!(o.silent_after_prime, 0, "no silent block at all\n{ctx}");
    assert_eq!(o.skipped_after_prime, 0, "no frame skipped\n{ctx}");
    assert_eq!(o.overruns, 0, "no resync\n{ctx}");
    assert!(
        o.min_depth >= (BLOCK + ASRC_LOOKAHEAD + 2 * 48) as f64,
        "at least 2 ms of margin is left at the worst pop\n{ctx}"
    );
    assert!(o.max_depth <= CAP as f64, "{ctx}");
    assert!(
        o.max_window_dev <= BAND,
        "every 1 s mean depth inside +-1/4 of the target\n{ctx}"
    );
    assert!(
        o.max_rate_dev_ppm <= rate_tol_ppm,
        "the ratio follows the sender's clock\n{ctx}"
    );
}

// --- realistic jitter: zero underruns over hours ---------------------------------------------

#[test]
fn program_feed_8h_at_plus_20ppm_never_underruns() {
    let sc = Scenario::new(FOH, 20.0, 1.0, 8.0 * 3600.0, 1);
    let o = run(&sc);
    assert_clean(&sc, &o, 3.0);
}

#[test]
fn program_feed_8h_at_minus_20ppm_never_underruns() {
    let sc = Scenario::new(FOH, -20.0, 1.0, 8.0 * 3600.0, 2);
    let o = run(&sc);
    assert_clean(&sc, &o, 3.0);
}

#[test]
fn program_feed_at_the_measured_half_ppm_is_resampled_at_its_rate() {
    // The live fohabl-strih rate (+0.50 ppm, 4.10.2026): about one frame of drift every 40 s, which
    // the issue-1401 servo dropped as single frames; the resampler follows it continuously.
    let sc = Scenario::new(FOH, 0.5, 1.0, 2.0 * 3600.0, 11);
    let o = run(&sc);
    assert_clean(&sc, &o, 3.0);
}

#[test]
fn mono_cambox_leg_8h_at_plus_and_minus_20ppm_never_underruns() {
    for (ppm, seed) in [(20.0, 3), (-20.0, 4)] {
        let sc = Scenario::new(CAMBOX, ppm, 1.0, 8.0 * 3600.0, seed);
        let o = run(&sc);
        assert_clean(&sc, &o, 3.0);
    }
}

#[test]
fn cambox_headset_adc_at_plus_540ppm_is_absorbed_for_an_hour() {
    // The cam1 headset ADC measured 48 026 samples/s against the hub (issue 1345): with no rate
    // handling that was one overrun every ~5 s. The resampler (clamp 1000 ppm) follows it.
    let sc = Scenario::new(CAMBOX, 540.0, 1.0, 3600.0, 5);
    let o = run(&sc);
    assert_clean(&sc, &o, 3.0);
}

#[test]
fn a_bursty_program_sender_never_underruns() {
    // A Windows sender pushing five packets at once (~5.4 ms of audio, one 512-frame 96 kHz
    // driver buffer) at the measured +0.5 ppm, plus the same 1 ms jitter, for 2 h.
    let mut sc = Scenario::new(FOH, 0.5, 1.0, 2.0 * 3600.0, 6);
    sc.burst = 5;
    let o = run(&sc);
    assert_clean(&sc, &o, 3.0);
}

#[test]
fn missed_hub_ticks_are_read_past_at_once_and_the_depth_stays() {
    // One lost mix block every 30 s for an hour: each leaves one extra block in the buffer. The
    // block loop reads past it at once (the outputs already lost that block), so the latency stays
    // where it was and the rate controller never sees it.
    let mut sc = Scenario::new(FOH, 0.5, 1.0, 3600.0, 7);
    sc.hub_skip_every_s = Some(30.0);
    let o = run(&sc);
    let skipped = (sc.secs / 30.0).floor() as usize;
    assert_clean(&sc, &o, 3.0);
    assert!(
        o.discarded.abs_diff(skipped * BLOCK) <= BLOCK,
        "one block given up per missed tick: {} vs {}",
        o.discarded,
        skipped * BLOCK
    );
}

#[test]
fn frequent_missed_ticks_never_disturb_the_rate() {
    // A hub that misses a tick every 0.5 s while the cam1 ADC runs +540 ppm fast: the rate fit
    // times every packet on the hub's clock, so the missed blocks never bend it, and the drift
    // never piles up into resyncs.
    let mut sc = Scenario::new(CAMBOX, 540.0, 1.0, 600.0, 13);
    sc.hub_skip_every_s = Some(0.5);
    let o = run(&sc);
    assert_clean(&sc, &o, 3.0);
}

#[test]
fn a_cambox_muted_and_unmuted_every_20s_restarts_gently() {
    // A cambox stops sending for 2 s every 20 s (muted, then unmuted) for an hour at +20 ppm and
    // 1 ms jitter: every unmute primes again at the same target. Each mute is one ran-dry pop,
    // nothing overruns, and the depth stays inside the band between the mutes.
    let mut sc = Scenario::new(CAMBOX, 20.0, 1.0, 3600.0, 12);
    sc.stall = Some(Stall {
        at_s: 10.0,
        len_s: 2.0,
        kind: StallKind::Lost,
        every_s: Some(20.0),
    });
    let o = run(&sc);
    let mutes = ((sc.secs - 10.0) / 20.0).ceil() as u64;
    assert_eq!(o.ran_dry, mutes, "{o:?}");
    assert_eq!(o.overruns, 0, "{o:?}");
    assert!(o.max_window_dev <= BAND, "{o:?}");
}

// --- the bench can tell the policies apart ---------------------------------------------------

/// The pre-1401 network policy on the same arrivals: pop whatever is queued, zero-pad a short
/// block (one underrun once the leg has received audio), drop down to the cap above it. Returns
/// the zero-padded pops.
fn run_old_policy(sc: &Scenario) -> u64 {
    let mut sender = Sender::new(sc);
    let mut rng = Rng(sc.seed ^ 0xA5A5_A5A5);
    let period = BLOCK as f64 / RATE;
    let pops = (sc.secs / period) as usize;
    let (mut fill, mut received, mut short) = (0usize, false, 0u64);
    let mut next = sender.next();
    for k in 1..=pops {
        let t = k as f64 * period + rng.unit() * sc.hub_late_ms * 1e-3;
        while next.0 <= t {
            fill = (fill + next.1).min(CAP);
            received = true;
            next = sender.next();
        }
        if fill < BLOCK {
            short += u64::from(received);
            fill = 0;
        } else {
            fill -= BLOCK;
        }
    }
    short
}

#[test]
fn the_bench_tells_the_old_policy_from_the_new_one() {
    // The FOH feed at the measured +0.5 ppm with only 0.2 ms sd of jitter for 30 min: the old
    // policy zero-pads blocks (the live pattern), the new one on the SAME arrivals never does.
    let sc = Scenario::new(FOH, 0.5, 0.2, 1800.0, 10);
    let old = run_old_policy(&sc);
    assert!(old > 0, "the old policy zero-pads blocks: {old}");
    let o = run(&sc);
    assert_eq!(o.ran_dry, 0, "{o:?}");
    assert_eq!(o.silent_after_prime, 0, "{o:?}");
}

// --- a real stall: one counted, bounded outcome ----------------------------------------------

fn stall_scenario(kind: StallKind, seed: u64) -> Scenario {
    let mut sc = Scenario::new(FOH, 0.5, 1.0, 1200.0, seed);
    sc.stall = Some(Stall {
        at_s: 600.0,
        len_s: 0.060,
        kind,
        every_s: None,
    });
    sc
}

/// The 60 ms stall spans this many hub blocks.
fn stall_blocks() -> u64 {
    (0.060 / (BLOCK as f64 / RATE)).ceil() as u64
}

#[test]
fn a_60ms_stall_with_a_catch_up_burst_is_one_counted_underrun() {
    let sc = stall_scenario(StallKind::Burst, 8);
    let o = run(&sc);
    let ctx = format!("{o:?}");
    assert_eq!(o.ran_dry, 1, "the stall is exactly one underrun\n{ctx}");
    assert!(
        o.silent_after_prime <= stall_blocks(),
        "silence only while the stall lasts\n{ctx}"
    );
    assert!(
        o.overruns <= 2,
        "the 60 ms backlog trims at most twice\n{ctx}"
    );
    // The re-prime and the resync both land on the target: nothing to walk back.
    assert!(o.recovered_s <= 3.0, "back in the band within 3 s\n{ctx}");
}

#[test]
fn a_60ms_stall_that_loses_the_audio_is_one_counted_underrun() {
    let sc = stall_scenario(StallKind::Lost, 9);
    let o = run(&sc);
    let ctx = format!("{o:?}");
    assert_eq!(o.ran_dry, 1, "the stall is exactly one underrun\n{ctx}");
    assert_eq!(o.overruns, 0, "{ctx}");
    assert!(
        o.silent_after_prime <= stall_blocks() + VBAN_TARGET_BLOCKS as u64,
        "silence for the stall plus one re-prime to the target\n{ctx}"
    );
    assert!(o.recovered_s <= 3.0, "back in the band within 3 s\n{ctx}");
}
