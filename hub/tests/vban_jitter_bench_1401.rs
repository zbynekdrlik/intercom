//! Issue 1401: the two-clock + arrival-jitter bench for a VBAN network leg.
//!
//! A sender on its own clock (the FOH desk's Dante tick, a cambox headset ADC: `ppm` against the
//! hub) produces 48 kHz audio and sends it as packets; each packet reaches the hub after a Gaussian
//! one-way delay, in order (UDP on one LAN path never overtakes); the hub pops one 256-frame block
//! on its exact 5.333 ms grid, every tick waking up to 1 ms late (tokio rounds a deadline up to
//! the next ms). The bench drives the real [`NetworkFill`] controller on frame counts alone, so
//! hours of simulated time run in seconds, and it measures everything itself (the pre-pop fill,
//! its 1 s means, the spacing of the corrections) rather than trusting the controller's numbers.
//!
//! Realistic means: +-20 ppm (the measured `fohabl-strih` is +0.5 ppm) and a 1 ms sd jitter. The
//! live underrun pattern of the old buffer (one every 2-15 min with well under one block of margin,
//! reproduced on issue 1401 with a 0.2 ms sd) puts the real jitter far below that. The FOH feed is
//! 103 frames at 96 kHz per packet = 51/52 at 48 kHz after decimation (~934 packets/s); a cambox
//! sends 128 mono frames (375 packets/s).

use std::collections::VecDeque;

use intercom_hub::vban_jitter::{
    NetworkFill, PopPlan, SERVO_DEADBAND_FRAMES, SERVO_MIN_SPACING_FRAMES, VBAN_CAP_BLOCKS,
    VBAN_TARGET_BLOCKS,
};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;
const TARGET: usize = VBAN_TARGET_BLOCKS * BLOCK;
const CAP: usize = VBAN_CAP_BLOCKS * BLOCK;
/// Pops per 1 s measurement window (48 000 / 256 = 187.5, rounded up).
const WINDOW_POPS: usize = 188;
/// Fill and band statistics start after this (start-up prime + the servo's first settle).
const SETTLE_S: f64 = 30.0;
/// "Recovered" after a stall = every 1 s mean pre-pop fill is back within this of the target.
const RECOVERED_BAND: usize = 64;
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
    /// Above-cap trims.
    overruns: u64,
    /// Silent blocks after the first audio (each one an audible gap).
    silent_after_prime: u64,
    drops: u64,
    repeats: u64,
    /// The lowest pre-pop fill of an audio pop after the settle.
    min_fill: usize,
    /// The highest pre-pop fill after the settle.
    max_fill: usize,
    /// The largest distance of a 1 s mean pre-pop fill from the target after the settle.
    max_window_dev: usize,
    /// The fewest output frames between two corrections after the settle.
    min_correction_gap: usize,
    /// Frames given up at once after a missed hub tick.
    discarded: usize,
    /// The most corrections inside one 1 s window after the settle (the servo spreads them).
    max_window_corrections: u64,
    /// After a stall: seconds from its end until the last time the leg was not primed or its 1 s
    /// mean fill was outside the servo band.
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
        min_fill: usize::MAX,
        min_correction_gap: usize::MAX,
        ..Outcome::default()
    };
    let mut fill = 0usize;
    let mut next = sender.next();
    let mut primed_once = false;
    let mut audio_frames = 0usize;
    let mut last_correction: Option<usize> = None;
    let mut window: VecDeque<usize> = VecDeque::with_capacity(WINDOW_POPS);
    let mut window_corrections = 0u64;
    let mut last_bad_t = 0.0f64;
    let mut missed = 0u64;

    for k in 1..=pops {
        if skip_every.is_some_and(|n| k.is_multiple_of(n)) {
            missed += 1;
            continue;
        }
        let t = k as f64 * period + rng.unit() * sc.hub_late_ms * 1e-3;
        while next.0 <= t {
            fill += next.1;
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
        let pre = fill;
        match ctl.plan_pop(fill, BLOCK) {
            PopPlan::Silent { ran_dry } => {
                out.ran_dry += u64::from(ran_dry);
                out.silent_after_prime += u64::from(primed_once);
                window.clear();
                window_corrections = 0;
                last_bad_t = t;
            }
            PopPlan::Audio { skip, take } => {
                primed_once = true;
                fill -= skip + take;
                audio_frames += BLOCK;
                if take != BLOCK {
                    window_corrections += 1;
                    if take > BLOCK {
                        out.drops += 1;
                    } else {
                        out.repeats += 1;
                    }
                    if let (Some(prev), true) = (last_correction, settled) {
                        out.min_correction_gap = out.min_correction_gap.min(audio_frames - prev);
                    }
                    last_correction = Some(audio_frames);
                }
                if settled {
                    out.min_fill = out.min_fill.min(pre);
                    out.max_fill = out.max_fill.max(pre);
                }
                window.push_back(pre);
                if window.len() == WINDOW_POPS {
                    let mean = window.iter().sum::<usize>() / WINDOW_POPS;
                    let dev = mean.abs_diff(TARGET);
                    if settled {
                        out.max_window_dev = out.max_window_dev.max(dev);
                        out.max_window_corrections =
                            out.max_window_corrections.max(window_corrections);
                    }
                    if dev > RECOVERED_BAND {
                        last_bad_t = t;
                    }
                    window.clear();
                    window_corrections = 0;
                }
            }
        }
    }
    out.recovered_s = stall_end.map_or(0.0, |end| (last_bad_t - end).max(0.0));
    out
}

/// The drift the servo must take out: frames the sender gains (+) or loses (-) over the run.
fn drift_frames(sc: &Scenario) -> f64 {
    sc.ppm * 1e-6 * RATE * sc.secs
}

/// The realistic-jitter verdict: not one gap, not one trim, the servo exactly takes out the
/// drift at <= 1 ms/s, and the 1 s mean fill never leaves the band by more than the servo's lag.
fn assert_clean(sc: &Scenario, o: &Outcome, extra_net_drops: f64) {
    let ctx = format!("{sc:?}\n{o:?}");
    assert_eq!(o.ran_dry, 0, "no underrun at realistic jitter\n{ctx}");
    assert_eq!(o.silent_after_prime, 0, "no silent block at all\n{ctx}");
    assert_eq!(o.overruns, 0, "no overrun trim\n{ctx}");
    assert!(
        o.min_fill >= BLOCK + 2 * 48,
        "at least 2 ms of margin is left at the worst pop\n{ctx}"
    );
    assert!(o.max_fill <= CAP, "{ctx}");
    assert!(
        o.min_correction_gap >= SERVO_MIN_SPACING_FRAMES,
        "<= 1 ms/s of corrections\n{ctx}"
    );
    let net = o.drops as f64 - o.repeats as f64;
    let expected = drift_frames(sc) + extra_net_drops;
    let slack = (2 * BLOCK) as f64;
    assert!(
        (net - expected).abs() <= slack,
        "the servo takes out exactly the drift: net {net} vs {expected}\n{ctx}"
    );
    // No hunting: the jitter does not make it correct back and forth.
    let total = (o.drops + o.repeats) as f64;
    assert!(
        total <= expected.abs() * 1.05 + slack,
        "corrections {total} vs the drift {expected}\n{ctx}"
    );
}

/// The corrections a slow drift needs (0.5-20 ppm = up to about one frame per second) are spread
/// out, never a second-long burst at the full 1 ms/s rate (47 corrections in one second): a frame
/// or two per servo second, so a measured second that straddles two servo seconds, plus the
/// jitter's wobble, sees a handful at most, and no two land within 2000 output frames.
fn assert_spread(o: &Outcome) {
    assert!(
        o.max_window_corrections <= 6,
        "a slow drift is corrected a frame or two per second, not in bursts\n{o:?}"
    );
    assert!(
        o.min_correction_gap >= 2 * SERVO_MIN_SPACING_FRAMES,
        "the corrections are spread, not back to back\n{o:?}"
    );
}

/// After a fresh prime the offset left (a packet of granularity, a late hub wake, the jitter) is
/// inside the gentle zone: at most 7 corrections a servo second, so a measured second that
/// straddles two sees at most 14, never the 47 of the full rate.
fn assert_spread_start_up(o: &Outcome) {
    assert!(
        o.max_window_corrections <= 14,
        "a start-up offset is corrected gently, not at the full rate\n{o:?}"
    );
}

// --- realistic jitter: zero underruns over hours ---------------------------------------------

#[test]
fn program_feed_8h_at_plus_20ppm_never_underruns() {
    let sc = Scenario::new(FOH, 20.0, 1.0, 8.0 * 3600.0, 1);
    let o = run(&sc);
    assert_clean(&sc, &o, 0.0);
    assert_spread(&o);
    assert!(o.max_window_dev <= SERVO_DEADBAND_FRAMES + 32, "{o:?}");
}

#[test]
fn program_feed_8h_at_minus_20ppm_never_underruns() {
    let sc = Scenario::new(FOH, -20.0, 1.0, 8.0 * 3600.0, 2);
    let o = run(&sc);
    assert_clean(&sc, &o, 0.0);
    assert_spread(&o);
    assert!(o.max_window_dev <= SERVO_DEADBAND_FRAMES + 32, "{o:?}");
}

#[test]
fn program_feed_at_the_measured_half_ppm_is_corrected_a_frame_at_a_time() {
    // The live fohabl-strih rate (+0.50 ppm, 4.10.2026): about one frame of drift every 40 s.
    let sc = Scenario::new(FOH, 0.5, 1.0, 2.0 * 3600.0, 11);
    let o = run(&sc);
    assert_clean(&sc, &o, 0.0);
    assert_spread(&o);
}

#[test]
fn mono_cambox_leg_8h_at_plus_and_minus_20ppm_never_underruns() {
    for (ppm, seed) in [(20.0, 3), (-20.0, 4)] {
        let sc = Scenario::new(CAMBOX, ppm, 1.0, 8.0 * 3600.0, seed);
        let o = run(&sc);
        assert_clean(&sc, &o, 0.0);
        assert_spread(&o);
        assert!(o.max_window_dev <= SERVO_DEADBAND_FRAMES + 32, "{o:?}");
    }
}

#[test]
fn cambox_headset_adc_at_plus_540ppm_is_absorbed_for_an_hour() {
    // The cam1 headset ADC measured 48 026 samples/s against the hub (issue 1345): with no servo
    // that was one overrun every ~5 s.
    let sc = Scenario::new(CAMBOX, 540.0, 1.0, 3600.0, 5);
    let o = run(&sc);
    assert_clean(&sc, &o, 0.0);
}

#[test]
fn a_bursty_program_sender_never_underruns() {
    // A Windows sender pushing five packets at once (~5.4 ms of audio, one 512-frame 96 kHz
    // driver buffer) at the measured +0.5 ppm, plus the same 1 ms jitter, for 2 h.
    let mut sc = Scenario::new(FOH, 0.5, 1.0, 2.0 * 3600.0, 6);
    sc.burst = 5;
    let o = run(&sc);
    assert_clean(&sc, &o, 0.0);
    assert_spread(&o);
}

#[test]
fn missed_hub_ticks_are_given_up_at_once_never_walked_back() {
    // One lost mix block every 30 s for an hour: each leaves one extra block in the buffer. The
    // block loop gives it up at once (the outputs already lost that block), so the servo only sees
    // the drift and never walks a block off at the full rate.
    let mut sc = Scenario::new(FOH, 0.5, 1.0, 3600.0, 7);
    sc.hub_skip_every_s = Some(30.0);
    let o = run(&sc);
    let skipped = (sc.secs / 30.0).floor() as usize;
    assert_clean(&sc, &o, 0.0);
    assert_spread(&o);
    assert!(
        o.discarded.abs_diff(skipped * BLOCK) <= BLOCK,
        "one block given up per missed tick: {} vs {}",
        o.discarded,
        skipped * BLOCK
    );
}

#[test]
fn frequent_missed_ticks_never_starve_the_servo() {
    // A hub that misses a tick every 0.5 s while a cambox ADC runs +540 ppm fast (issue 1345,
    // cam1): giving up the missed blocks must not cancel the servo's budget for the second, or the
    // drift piles up into overruns.
    let mut sc = Scenario::new(CAMBOX, 540.0, 1.0, 600.0, 13);
    sc.hub_skip_every_s = Some(0.5);
    let o = run(&sc);
    let ctx = format!("{o:?}");
    assert_eq!(o.overruns, 0, "{ctx}");
    assert_eq!(o.ran_dry, 0, "{ctx}");
    let net = o.drops as f64 - o.repeats as f64;
    let drift = drift_frames(&sc);
    assert!(
        (net - drift).abs() <= (2 * BLOCK) as f64,
        "the servo still takes out the drift: {net} vs {drift}\n{ctx}"
    );
}

#[test]
fn a_cambox_muted_and_unmuted_every_20s_restarts_gently() {
    // A cambox stops sending for 2 s every 20 s (muted, then unmuted) for an hour at +20 ppm and
    // 1 ms jitter: every unmute primes again. Each mute is one ran-dry pop, nothing overruns, and
    // the start-up offset a prime can leave is corrected gently, never at the full rate.
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
    assert_spread_start_up(&o);
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
    // The re-prime and the overrun trim both land on the target: nothing to walk back.
    assert!(o.recovered_s <= 3.0, "back in the band within 3 s\n{ctx}");
    assert!(o.min_correction_gap >= SERVO_MIN_SPACING_FRAMES, "{ctx}");
    assert_spread_start_up(&o);
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
    assert_spread_start_up(&o);
}
