//! Issue 1401, design 5980775411: a hub scheduling hiccup loses no audio.
//!
//! The block loop used to be a tokio task with `MissedTickBehavior::Skip`: a wake more than a
//! period late skipped the missed ticks, and every output lost those blocks (live 4.10.2026: 18
//! missed ticks in one hour, in clusters of 1-5, several right on the strih-lx dantesync NTP
//! bursts). Now the loop sleeps to absolute deadlines on the exact block grid ([`BlockGrid`]) and
//! a wake that finds k missed ticks runs them all at once, in order, before the current one, up to
//! [`CATCHUP_MAX_BLOCKS`] ([`catch_up_plan`], [`run_batch`]). Only the part beyond that is lost.
//!
//! These tests pin the plan, the grid (exact deadlines, no drift over 10 min of jittered wakes) and
//! a two-clock bench of the whole hub: two VBAN legs (a mono cambox at +20 ppm and the FOH feed in
//! its 51/52-frame packets at -20 ppm, each with 1 ms of arrival jitter) and two `pw-cat` egress
//! pipes (the program sink at the hub's rate, the operator's cans on a +50 ppm MiniFuse crystal)
//! around the real [`run_batch`] dispatch. A 15 ms stall every 60 s loses nothing and underruns
//! nothing; a 40 ms stall loses exactly the part beyond four blocks, a bounded, counted loss; the
//! old skip-at-once dispatch on the same wakes loses blocks on every output.

use std::time::Duration;

use intercom_hub::block_clock::{
    catch_up_plan, run_batch, BlockGrid, TickBatch, CATCHUP_MAX_BLOCKS,
};
use intercom_hub::janus_pacing::hub_block_period;
use intercom_hub::pipe_fill::{
    PipeFillControl, PipeFillPlan, PIPE_SAMPLE_INTERVAL, PW_GRAPH_BURST_FRAMES,
};
use intercom_hub::vban_jitter::{
    NetworkFill, PopPlan, VBAN_CAP_BLOCKS, VBAN_PROGRAM_CAP_BLOCKS, VBAN_PROGRAM_TARGET_BLOCKS,
    VBAN_TARGET_BLOCKS,
};

const RATE: u32 = 48_000;
const BLOCK: usize = 256;
const NS: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const US: u64 = 1_000;

fn grid_ns(n: u64) -> u64 {
    (u128::from(n) * BLOCK as u128 * 1_000_000_000 / u128::from(RATE)) as u64
}

// --- the plan ------------------------------------------------------------------------------------

#[test]
fn up_to_four_missed_ticks_are_caught_up_and_only_the_rest_is_lost() {
    assert_eq!(CATCHUP_MAX_BLOCKS, 4, "the design's 4 blocks (21.3 ms)");
    let plan = |missed| {
        let b = catch_up_plan(missed);
        (b.catch_up, b.lost)
    };
    assert_eq!(plan(0), (0, 0));
    assert_eq!(plan(1), (1, 0));
    assert_eq!(plan(2), (2, 0));
    assert_eq!(plan(4), (4, 0));
    assert_eq!(plan(5), (4, 1));
    assert_eq!(plan(7), (4, 3));
    assert_eq!(plan(1000), (4, 996));
    assert_eq!(catch_up_plan(3).cycles(), 4, "three late + the current one");
    // The headroom argument: the blocks that arrive while the loop is four ticks late (plus the
    // current tick, plus one tick of phase) fill a cambox leg from one block under its target to at
    // most its cap; every other buffer has more room.
    assert!(VBAN_CAP_BLOCKS - VBAN_TARGET_BLOCKS + 1 >= CATCHUP_MAX_BLOCKS as usize + 2);
    assert!(
        VBAN_PROGRAM_CAP_BLOCKS - VBAN_PROGRAM_TARGET_BLOCKS + 1 >= CATCHUP_MAX_BLOCKS as usize + 2
    );
}

#[test]
fn a_batch_runs_its_catch_up_cycles_first_then_the_current_one_with_the_lost_part() {
    let order = |batch: TickBatch| {
        let mut seen = Vec::new();
        run_batch(batch, |lost| seen.push(lost));
        seen
    };
    assert_eq!(
        order(TickBatch::default()),
        vec![0],
        "a punctual wake: one cycle"
    );
    assert_eq!(order(catch_up_plan(2)), vec![0, 0, 0]);
    assert_eq!(
        order(catch_up_plan(7)),
        vec![0, 0, 0, 0, 3],
        "four late cycles, then the current one gives up the three beyond them"
    );
}

// --- the grid ------------------------------------------------------------------------------------

#[test]
fn the_grid_deadlines_are_exact_and_never_summed_from_a_period() {
    let grid = BlockGrid::new(BLOCK, RATE);
    let period = hub_block_period(BLOCK, RATE);
    assert_eq!(grid.deadline(0), Duration::ZERO);
    assert_eq!(grid.deadline(1), period, "one tick = hub_block_period");
    for n in [2u64, 3, 187, 188, 112_500, 675_000, 5_400_000] {
        assert_eq!(grid.deadline(n).as_nanos() as u64, grid_ns(n), "tick {n}");
        // n periods summed would run early by up to n x 0.33 ns.
        let summed = period * n as u32;
        assert!(grid.deadline(n) >= summed && grid.deadline(n) - summed <= Duration::from_nanos(n));
    }
    // Ten minutes are exactly 112 500 ticks: the grid has no drift against a 48 kHz consumer.
    assert_eq!(grid.deadline(112_500), Duration::from_secs(600));
    assert_eq!(grid.deadline(675_000), Duration::from_secs(3600));
}

#[test]
fn take_due_counts_every_tick_due_by_the_wake_exactly() {
    // Against a brute-force search at every awkward instant around many deadlines.
    let probe = BlockGrid::new(BLOCK, RATE);
    let brute = |now: u64| (0..).take_while(|&n| grid_ns(n) <= now).last().unwrap();
    for n in [1u64, 2, 3, 4, 5, 6, 7, 8, 13, 100, 101] {
        let d = probe.deadline(n).as_nanos() as u64;
        for now in [d - 1, d, d + 1, d + 333, d + 5_333_332] {
            let mut grid = BlockGrid::new(BLOCK, RATE);
            let batch = grid.take_due(Duration::from_nanos(now)).unwrap();
            let last = brute(now);
            assert_eq!(batch.catch_up + batch.lost, last, "missed at {now} ns");
            assert_eq!(grid.next_tick(), last + 1, "the grid moved past {now} ns");
        }
    }
}

#[test]
fn an_early_wake_takes_nothing_and_a_lost_tick_is_never_run_later() {
    let mut grid = BlockGrid::new(BLOCK, RATE);
    assert_eq!(
        grid.take_due(Duration::ZERO),
        Some(TickBatch::default()),
        "tick 0"
    );
    assert_eq!(grid.next_tick(), 1);
    // Woken a nanosecond before tick 1: nothing is due, the grid stays.
    let early = grid.next_deadline() - Duration::from_nanos(1);
    assert_eq!(grid.take_due(early), None);
    assert_eq!(grid.next_tick(), 1);
    // On time.
    let t1 = grid.next_deadline();
    assert_eq!(grid.take_due(t1), Some(TickBatch::default()));
    // 40 ms late for tick 2: ticks 2..=9 are due, 7 missed = 4 late + 3 lost.
    let late = grid.next_deadline() + Duration::from_millis(40);
    let batch = grid.take_due(late).unwrap();
    assert_eq!((batch.catch_up, batch.lost), (4, 3));
    assert_eq!(grid.next_tick(), 10);
    assert!(
        grid.next_deadline() > late,
        "the lost ticks are behind the next deadline"
    );
}

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

    /// Uniform in 0..n.
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    fn gauss(&mut self) -> f64 {
        (-2.0 * self.unit().ln()).sqrt() * (std::f64::consts::TAU * self.unit()).cos()
    }
}

#[test]
fn the_grid_holds_over_10_min_of_jittered_wakes() {
    // A real-time wake is 5-80 us late; one wake in 500 hits a scheduling hiccup of 6-20 ms.
    let mut grid = BlockGrid::new(BLOCK, RATE);
    let mut rng = Rng(42);
    let end = Duration::from_secs(600);
    let (mut cycles, mut caught_up, mut lost, mut hiccups) = (0u64, 0u64, 0u64, 0u64);
    let mut relative = Duration::ZERO;
    let mut relative_cycles = 0u64;
    let period = hub_block_period(BLOCK, RATE);
    while grid.next_deadline() < end {
        let late = if rng.below(500) == 0 {
            hiccups += 1;
            Duration::from_nanos(6 * MS + rng.below(14 * MS))
        } else {
            Duration::from_nanos(5 * US + rng.below(75 * US))
        };
        let batch = grid.take_due(grid.next_deadline() + late).unwrap();
        cycles += batch.cycles();
        caught_up += batch.catch_up;
        lost += batch.lost;
        // The same wakes on a relative sleep (one period from each wake) for contrast.
        if relative < end {
            relative += period + late;
            relative_cycles += 1;
        }
    }
    assert!(hiccups > 150, "{hiccups}");
    assert!(
        caught_up >= hiccups,
        "every hiccup was caught up: {caught_up}"
    );
    assert_eq!(lost, 0, "no hiccup up to 20 ms loses a block");
    assert_eq!(
        cycles, 112_500,
        "exactly one cycle per tick of the 10 min, whatever the wakes"
    );
    assert_eq!(grid.next_tick(), 112_500);
    assert_eq!(grid.next_deadline(), end, "the grid did not drift");
    // A relative sleep would have fallen seconds behind the 48 kHz consumers.
    assert!(
        relative_cycles + 500 < 112_500,
        "a relative sleep drifts: {relative_cycles}"
    );
}

// --- the two-clock bench ---------------------------------------------------------------------------

/// One VBAN input leg: a sender on its own clock, packets with Gaussian one-way delay in FIFO
/// order, and the real `NetworkFill` controller on frame counts.
struct Leg {
    ctl: NetworkFill,
    fill: usize,
    packets: &'static [usize],
    rate: f64,
    jitter_sd_ms: f64,
    idx: usize,
    produced: usize,
    last_arrival: u64,
    next: (u64, usize),
    rng: Rng,
    primed_once: bool,
    ran_dry: u64,
    overruns: u64,
    silent_after_prime: u64,
}

impl Leg {
    fn new(packets: &'static [usize], blocks: (usize, usize), ppm: f64, seed: u64) -> Self {
        let mut leg = Leg {
            ctl: NetworkFill::new(blocks.0 * BLOCK, blocks.1 * BLOCK),
            fill: 0,
            packets,
            rate: f64::from(RATE) * (1.0 + ppm * 1e-6),
            jitter_sd_ms: 1.0,
            idx: 0,
            produced: 0,
            last_arrival: 0,
            next: (0, 0),
            rng: Rng(seed),
            primed_once: false,
            ran_dry: 0,
            overruns: 0,
            silent_after_prime: 0,
        };
        leg.next = leg.next_packet();
        leg
    }

    fn next_packet(&mut self) -> (u64, usize) {
        let n = self.packets[self.idx % self.packets.len()];
        self.idx += 1;
        self.produced += n;
        let send = self.produced as f64 / self.rate;
        let delay = (1.0 + self.jitter_sd_ms * self.rng.gauss()).max(0.0) * 1e-3;
        let arrival = (((send + delay) * 1e9) as u64).max(self.last_arrival);
        self.last_arrival = arrival;
        (arrival, n)
    }

    /// The receive task keeps pushing while the block loop is late.
    fn deliver_until(&mut self, t: u64) {
        while self.next.0 <= t {
            self.fill += self.next.1;
            if let Some(keep) = self.ctl.overrun_keep(self.fill) {
                self.fill = keep;
                self.overruns += 1;
            }
            self.next = self.next_packet();
        }
    }

    /// One cycle's pop, after giving up `lost` blocks as `JitterBuffer::skip_missed` does.
    fn pop(&mut self, lost: u64) {
        self.fill -= self.ctl.discard_for_missed_ticks(self.fill, BLOCK, lost);
        match self.ctl.plan_pop(self.fill, BLOCK) {
            PopPlan::Silent { ran_dry } => {
                self.ran_dry += u64::from(ran_dry);
                self.silent_after_prime += u64::from(self.primed_once);
            }
            PopPlan::Audio { skip, take } => {
                self.primed_once = true;
                self.fill -= skip + take;
            }
        }
    }
}

/// One `pw-cat` playback pipe: the real `PipeFillControl` fed by the sink thread (a write per
/// cycle, a reading every ~1 ms in between) and pw-cat reading a 1024-frame quantum per cycle of
/// its own clock.
struct Pipe {
    ctl: PipeFillControl,
    fill: usize,
    read_period: f64,
    first_read: Option<u64>,
    reads: u64,
    next_read: u64,
    next_sample: u64,
    rng: Rng,
    refills: u64,
    trims: u64,
    starved: u64,
}

impl Pipe {
    fn new(sink_ppm: f64, seed: u64) -> Self {
        Pipe {
            ctl: PipeFillControl::new(),
            fill: 0,
            read_period: PW_GRAPH_BURST_FRAMES as f64 * 1e9
                / (f64::from(RATE) * (1.0 + sink_ppm * 1e-6)),
            first_read: None,
            reads: 0,
            next_read: u64::MAX,
            next_sample: u64::MAX,
            rng: Rng(seed),
            refills: 0,
            trims: 0,
            starved: 0,
        }
    }

    /// pw-cat's reads and the sink thread's readings up to `t`, in time order.
    fn advance_to(&mut self, t: u64) {
        loop {
            let at = self.next_read.min(self.next_sample);
            if at > t {
                return;
            }
            if self.next_read <= self.next_sample {
                if self.fill >= PW_GRAPH_BURST_FRAMES {
                    self.fill -= PW_GRAPH_BURST_FRAMES;
                } else {
                    self.starved += 1;
                    self.fill = 0;
                }
                self.reads += 1;
                let jitter = self.rng.below(60 * US) as f64 - 30_000.0;
                let first = self.first_read.unwrap_or(0);
                self.next_read = first + (self.reads as f64 * self.read_period + jitter) as u64;
            } else {
                self.ctl.sample(at, self.fill);
                self.next_sample = at + self.sample_wait();
            }
        }
    }

    fn sample_wait(&mut self) -> u64 {
        PIPE_SAMPLE_INTERVAL.as_nanos() as u64 + 20 * US + self.rng.below(60 * US)
    }

    /// The sink thread writes one block at `t`.
    fn write(&mut self, t: u64) {
        self.advance_to(t);
        if self.first_read.is_none() {
            // pw-cat connects 37 ms after the prime.
            self.first_read = Some(t + 37 * MS);
            self.next_read = t + 37 * MS;
        }
        let r = self.ctl.plan_block(t, self.fill, BLOCK);
        match r.plan {
            PipeFillPlan::Drop => self.trims += 1,
            PipeFillPlan::TopUp { .. } if !r.first => self.refills += 1,
            _ => {}
        }
        self.fill += r.plan.written_frames(BLOCK);
        self.next_sample = t + self.sample_wait();
    }
}

/// The FOH program feed after the 96 -> 48 kHz decimation, and a cambox's mono talkback packet.
const FOH: &[usize] = &[51, 52];
const CAMBOX: &[usize] = &[128];

#[derive(Debug, Clone, Copy)]
struct Scenario {
    secs: u64,
    /// The block loop wakes this late once every `stall_every_s` (else 0-200 us, a real-time wake).
    stall: Duration,
    stall_every_s: u64,
    /// The pre-1401 dispatch: every missed tick given up at once.
    skip_at_once: bool,
    seed: u64,
}

#[derive(Debug, Default)]
struct Outcome {
    stalls: u64,
    ticks: u64,
    cycles: u64,
    caught_up: u64,
    lost: u64,
    /// cambox, FOH: (ran dry, overruns, silent blocks after the first audio).
    legs: [(u64, u64, u64); 2],
    /// program sink, cans: (refills, trims, starved pw-cat reads).
    pipes: [(u64, u64, u64); 2],
}

fn run(sc: Scenario) -> Outcome {
    let mut grid = BlockGrid::new(BLOCK, RATE);
    let mut legs = [
        Leg::new(CAMBOX, (VBAN_TARGET_BLOCKS, VBAN_CAP_BLOCKS), 20.0, sc.seed),
        Leg::new(
            FOH,
            (VBAN_PROGRAM_TARGET_BLOCKS, VBAN_PROGRAM_CAP_BLOCKS),
            -20.0,
            sc.seed ^ 1,
        ),
    ];
    let mut pipes = [Pipe::new(0.0, sc.seed ^ 2), Pipe::new(50.0, sc.seed ^ 3)];
    let mut rng = Rng(sc.seed ^ 4);
    let end = sc.secs * NS;
    let every = sc.stall_every_s * NS;
    let mut next_stall = every;
    let mut out = Outcome::default();
    loop {
        let due = grid.next_deadline().as_nanos() as u64;
        if due >= end {
            break;
        }
        let late = if due >= next_stall {
            next_stall += every;
            out.stalls += 1;
            sc.stall.as_nanos() as u64
        } else {
            rng.below(200 * US)
        };
        let now = due + late;
        let mut batch = grid.take_due(Duration::from_nanos(now)).unwrap();
        if sc.skip_at_once {
            batch = TickBatch {
                catch_up: 0,
                lost: batch.catch_up + batch.lost,
            };
        }
        out.caught_up += batch.catch_up;
        out.lost += batch.lost;
        for leg in &mut legs {
            leg.deliver_until(now);
        }
        let mut t = now;
        let mut cycles = 0;
        run_batch(batch, |lost| {
            cycles += 1;
            for leg in &mut legs {
                leg.pop(lost);
            }
            for pipe in &mut pipes {
                pipe.write(t);
            }
            t += 20 * US;
        });
        out.cycles += cycles;
    }
    out.ticks = grid.next_tick();
    for (o, l) in out.legs.iter_mut().zip(&legs) {
        *o = (l.ran_dry, l.overruns, l.silent_after_prime);
    }
    for (o, p) in out.pipes.iter_mut().zip(&pipes) {
        *o = (p.refills, p.trims, p.starved);
    }
    out
}

fn stalls_of(stall_ms: u64, secs: u64, skip_at_once: bool, seed: u64) -> Scenario {
    Scenario {
        secs,
        stall: Duration::from_millis(stall_ms),
        stall_every_s: 60,
        skip_at_once,
        seed,
    }
}

#[test]
fn a_15ms_stall_every_60s_loses_no_block_and_underruns_nothing() {
    let o = run(stalls_of(15, 1200, false, 1));
    let ctx = format!("{o:?}");
    assert_eq!(o.stalls, 19, "{ctx}");
    assert_eq!(o.lost, 0, "no block lost on any output\n{ctx}");
    assert_eq!(
        o.caught_up,
        2 * o.stalls,
        "each stall = 2 ticks run late\n{ctx}"
    );
    assert_eq!(o.cycles, o.ticks, "every tick's cycle ran\n{ctx}");
    for (i, (ran_dry, overruns, silent)) in o.legs.iter().enumerate() {
        assert_eq!((*ran_dry, *overruns, *silent), (0, 0, 0), "leg {i}\n{ctx}");
    }
    for (i, (refills, trims, starved)) in o.pipes.iter().enumerate() {
        assert_eq!((*refills, *trims, *starved), (0, 0, 0), "pipe {i}\n{ctx}");
    }
}

#[test]
fn the_old_skip_at_once_dispatch_loses_blocks_on_the_same_wakes() {
    // The bench can tell the dispatches apart: the pre-1401 Skip lost both missed blocks of every
    // stall on every output.
    let old = run(stalls_of(15, 1200, true, 1));
    assert_eq!(old.lost, 2 * old.stalls, "{old:?}");
    assert_eq!(old.cycles + old.lost, old.ticks, "{old:?}");
    let new = run(stalls_of(15, 1200, false, 1));
    assert_eq!(new.lost, 0, "{new:?}");
}

#[test]
fn a_40ms_stall_loses_exactly_the_part_beyond_four_blocks() {
    let o = run(stalls_of(40, 600, false, 3));
    let ctx = format!("{o:?}");
    assert_eq!(o.stalls, 9, "{ctx}");
    // 40 ms late = 7 missed ticks: 4 run late, 3 lost, every time.
    assert_eq!(o.caught_up, 4 * o.stalls, "{ctx}");
    assert_eq!(o.lost, 3 * o.stalls, "{ctx}");
    assert_eq!(o.cycles + o.lost, o.ticks, "{ctx}");
    // Past the catch-up a stall is a counted, bounded loss, never a run-away: at most one underrun
    // and one overrun trim per leg and one refill per pipe a stall. A 45 ms write gap is longer
    // than a pw-cat pipe's ~37 ms depth, and a cambox leg's cap trims while the loop is late.
    for (i, (ran_dry, overruns, _)) in o.legs.iter().enumerate() {
        assert!(
            *ran_dry <= o.stalls && *overruns <= o.stalls,
            "leg {i}\n{ctx}"
        );
    }
    for (i, (refills, trims, _)) in o.pipes.iter().enumerate() {
        assert!(*refills <= o.stalls && *trims == 0, "pipe {i}\n{ctx}");
    }
}
