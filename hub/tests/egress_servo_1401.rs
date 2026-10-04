//! Issue 1401, egress step 2 (ROZHODNUTÉ 5979509439 + 5979627274): a fixed start depth and a drift
//! servo on every `pw-cat` playback pipe (the strih program sink and the cutters' MiniFuse cans).
//!
//! - **Start hold.** pw-cat reads nothing until its stream runs, and the hub kept writing on top of
//!   the prime: a spawn started 62-85 ms deep, different after every restart. Until pw-cat first
//!   reads, a block that would take the fill above the target is now dropped, so its first read
//!   always finds exactly the prime.
//! - **Drift servo.** The cans' sink is clocked by the MiniFuse crystal, not the hub's timer, and
//!   the bound-only guards turned +-50 ppm into a dropped 5.3 ms block every ~107 s or a refill gap
//!   every ~14 min. The VBAN legs' servo (`NetworkFill::servo_step`) now holds the pipe at the depth
//!   the start hold leaves, with single-frame drops / repeats spread across a block.
//! - **The servo reads the time-weighted fill.** pw-cat takes four hub blocks at once, so the
//!   pre-write readings see a drift only as a whole-block step at each read/write phase crossing,
//!   and the servo would answer in bursts (design question 5979620130). The sink thread reads the
//!   pipe about every 1 ms between blocks, and the servo gets the trapezoid mean of those readings.
//!
//! The two-clock bench drives the real `PipeFillControl` on frame counts: the hub's block loop on
//! its 5.333 ms grid (tokio rounds every wake up to the next ms, plus up to 50 us of OS latency),
//! the sink thread's `recv_timeout(1 ms)` readings in between, and a pw-cat consumer reading one
//! 1024-frame quantum per cycle on its own clock. It measures the TRUE time-weighted pipe fill
//! itself from the exact event times, never the controller's numbers.

use intercom_hub::local_audio::{
    pipe_fill_bytes, pipe_write_bytes, LocalAudioStats, PipeFillWriter, PW_GRAPH_BURST_FRAMES,
};
use intercom_hub::pipe_fill::{
    pipe_servo_setpoint, stretch_interleaved, PipeFillControl, PipeFillPlan, PipeServoDepth,
    PipeWriteReport, PIPE_SAMPLE_INTERVAL, PIPE_TARGET_FRAMES,
};
use intercom_hub::vban_jitter::{NetworkFill, PopPlan, ServoStep};

/// The hub block on strih-lx (`[hub].block_frames`).
const BLOCK: usize = 256;
/// pw-cat's read: one graph quantum.
const QUANTUM: usize = PW_GRAPH_BURST_FRAMES;
/// The prime: the silence up to the target plus the prime's own block.
const PRIME: usize = PIPE_TARGET_FRAMES + BLOCK;
const SETPOINT: usize = pipe_servo_setpoint(BLOCK);

// --- the pieces ----------------------------------------------------------------------------------

#[test]
fn the_setpoint_is_the_prime_minus_half_a_quantum() {
    assert_eq!(
        SETPOINT,
        PIPE_TARGET_FRAMES + BLOCK - PW_GRAPH_BURST_FRAMES / 2
    );
    assert_eq!(SETPOINT, 1792, "37.3 ms at 48 kHz");
    // It follows the block: half a quantum under whatever the prime is.
    assert_eq!(pipe_servo_setpoint(512), PIPE_TARGET_FRAMES + 512 - 512);
    assert_eq!(
        PIPE_SAMPLE_INTERVAL.as_millis(),
        1,
        "about 1000 readings a second"
    );
}

#[test]
fn the_setpoint_is_the_time_average_the_start_hold_leaves() {
    // pw-cat's first read takes a quantum from the prime; the hub writes it back one block at a
    // time until the next read. Average the exact fill over one read cycle, for every phase of
    // the read inside a hub block: the mean over the phases is the setpoint.
    let write_period = 1.0_f64;
    let read_period = (QUANTUM / BLOCK) as f64 * write_period;
    let phases = 256;
    let mut sum = 0.0;
    for i in 0..phases {
        // The first write after the read comes `phi` later.
        let phi = (i as f64 + 0.5) / phases as f64 * write_period;
        let mut fill = (PRIME - QUANTUM) as f64;
        let mut area = fill * phi;
        let mut t = phi;
        while t < read_period {
            fill += BLOCK as f64;
            let next = (t + write_period).min(read_period);
            area += fill * (next - t);
            t = next;
        }
        sum += area / read_period;
    }
    let mean = sum / phases as f64;
    assert!((mean - SETPOINT as f64).abs() < 0.5, "{mean}");
}

#[test]
fn the_servo_step_is_exactly_the_pop_servo() {
    // The egress reuses the VBAN legs' servo: one controller fed through plan_pop (primed at the
    // target) and one through servo_step see the same fills and must decide the same.
    let target = 1536;
    let mut pop = NetworkFill::new(target, 2816);
    let mut step = NetworkFill::new(target, 2816);
    let mut corrections = 0;
    for i in 0..200_000usize {
        // A slow walk past the knee and back, with a block of ripple.
        let walk = (i as f64 / 2_000.0).sin() * 400.0;
        let ripple = (i % 4) * 64;
        let fill = (target as f64 + walk) as usize + ripple;
        let fill = if i == 0 { target } else { fill };
        let planned = pop.plan_pop(fill, BLOCK);
        let stepped = step.servo_step(fill, BLOCK);
        let expected = match stepped {
            ServoStep::Keep => BLOCK,
            ServoStep::Drop => BLOCK + 1,
            ServoStep::Repeat => BLOCK - 1,
        };
        assert_eq!(
            planned,
            PopPlan::Audio {
                skip: 0,
                take: expected
            },
            "pop {i} at fill {fill}"
        );
        if stepped != ServoStep::Keep {
            corrections += 1;
        }
    }
    assert!(corrections > 100, "the walk exercised the servo");
    let (a, b) = (pop.stats(), step.stats());
    assert_eq!(
        (a.servo_drops, a.servo_repeats, a.depth_frames),
        (b.servo_drops, b.servo_repeats, b.depth_frames)
    );
}

#[test]
fn stretch_interleaved_stretches_every_channel_on_its_own_and_keeps_both_ends() {
    // Left a ramp, right a constant: a correction must never mix the channels.
    let frames = BLOCK;
    let interleaved: Vec<i16> = (0..frames).flat_map(|f| [(f as i16) * 10, -700]).collect();
    for out in [frames - 1, frames + 1] {
        let s = stretch_interleaved(&interleaved, 2, out);
        assert_eq!(s.len(), out * 2);
        let left: Vec<i16> = s.iter().step_by(2).copied().collect();
        let right: Vec<i16> = s.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(left[0], 0);
        assert_eq!(*left.last().unwrap(), ((frames - 1) as i16) * 10);
        assert!(
            left.windows(2).all(|w| w[0] <= w[1]),
            "the ramp stays a ramp"
        );
        assert!(
            right.iter().all(|&r| r == -700),
            "the right channel untouched"
        );
    }
    // Same length = unchanged; four channels keep their own frames too.
    assert_eq!(stretch_interleaved(&interleaved, 2, frames), interleaved);
    let four: Vec<i16> = (0..frames * 4).map(|i| (i % 4) as i16).collect();
    let s = stretch_interleaved(&four, 4, frames + 1);
    assert_eq!(s.len(), (frames + 1) * 4);
    assert!(s.as_chunks::<4>().0.iter().all(|c| *c == [0, 1, 2, 3]));
}

#[test]
fn each_plan_writes_exactly_its_frames() {
    let block: Vec<i16> = (0..BLOCK * 2).map(|i| i as i16 + 1).collect();
    let bytes = |plan| pipe_write_bytes(plan, &block, 2).map(|b| b.len() / 4);
    for plan in [
        PipeFillPlan::Write,
        PipeFillPlan::ServoDrop,
        PipeFillPlan::ServoRepeat,
        PipeFillPlan::TopUp {
            silence_frames: 300,
        },
        PipeFillPlan::Drop,
        PipeFillPlan::StartHold,
    ] {
        let written = plan.written_frames(BLOCK);
        let expect = if written == 0 { None } else { Some(written) };
        assert_eq!(bytes(plan), expect, "{plan:?}");
    }
    assert_eq!(PipeFillPlan::ServoDrop.written_frames(BLOCK), BLOCK - 1);
    assert_eq!(PipeFillPlan::ServoRepeat.written_frames(BLOCK), BLOCK + 1);
    // A corrected block is the stretched block, both ends kept.
    let drop = pipe_write_bytes(PipeFillPlan::ServoDrop, &block, 2).unwrap();
    let expect: Vec<u8> = stretch_interleaved(&block, 2, BLOCK - 1)
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    assert_eq!(drop, expect);
}

// --- the start hold ------------------------------------------------------------------------------

#[test]
fn the_start_hold_drops_blocks_until_pw_cat_first_reads() {
    let mut c = PipeFillControl::new();
    let ms = 1_000_000u64;
    let prime = c.plan_block(0, 0, BLOCK);
    assert_eq!(
        prime,
        PipeWriteReport {
            fill_frames: 0,
            plan: PipeFillPlan::TopUp {
                silence_frames: PIPE_TARGET_FRAMES
            },
            first: true,
        }
    );
    assert!(!c.started());
    // pw-cat is still connecting: every block is held back and the pipe stays at the prime.
    for k in 1..30u64 {
        c.sample(k * 5 * ms + ms, PRIME);
        let r = c.plan_block(k * 5 * ms + 3 * ms, PRIME, BLOCK);
        assert_eq!(r.plan, PipeFillPlan::StartHold, "block {k}");
        assert!(!r.first);
    }
    assert!(c.servo_depth().is_none(), "no servo before pw-cat reads");
    // pw-cat's first read: the hold is over, and the block is written.
    c.sample(200 * ms, PRIME - QUANTUM);
    assert!(c.started());
    let r = c.plan_block(201 * ms, PRIME - QUANTUM, BLOCK);
    assert_eq!(r.plan, PipeFillPlan::Write);
    assert_eq!(c.servo_depth().map(|d| d.setpoint_frames), Some(SETPOINT));
}

#[test]
fn a_real_pipe_holds_the_prime_until_it_is_read() {
    use std::io::Read;
    let (mut reader, writer) = std::io::pipe().expect("pipe");
    let probe = writer.try_clone().expect("clone the write end");
    let mut sink = PipeFillWriter::new(writer, 2);
    let stats = LocalAudioStats::default();
    let block: Vec<i16> = vec![5; BLOCK * 2];
    let fill = || pipe_fill_bytes(&probe).unwrap() / 4;

    stats.record_write(&sink.write_block(&block).unwrap());
    assert_eq!(fill(), PRIME);
    for _ in 0..10 {
        sink.sample_fill().unwrap();
        let r = sink.write_block(&block).unwrap();
        stats.record_write(&r);
        assert_eq!(r.plan, PipeFillPlan::StartHold);
        assert_eq!(fill(), PRIME, "nothing piles up on the prime");
    }
    // pw-cat takes its first quantum.
    let mut quantum = vec![0u8; QUANTUM * 4];
    reader.read_exact(&mut quantum).unwrap();
    sink.sample_fill().unwrap();
    let r = sink.write_block(&block).unwrap();
    stats.record_write(&r);
    stats.record_servo_depth(sink.servo_depth());
    assert_eq!(r.plan, PipeFillPlan::Write);
    assert_eq!(fill(), PRIME - QUANTUM + BLOCK);
    let f = stats.snapshot();
    assert_eq!(f.pipe_start_holds, 10);
    assert_eq!(
        f.tx_blocks, 2,
        "the prime and the first block after the read"
    );
    assert_eq!((f.pipe_trims, f.pipe_refills), (0, 0));
    assert_eq!(f.pipe_setpoint_frames, SETPOINT as u64);
}

#[test]
fn the_facet_counts_holds_and_servo_corrections() {
    let stats = LocalAudioStats::default();
    let report = |plan| PipeWriteReport {
        fill_frames: 1800,
        plan,
        first: false,
    };
    stats.record_write(&report(PipeFillPlan::StartHold));
    stats.record_write(&report(PipeFillPlan::ServoDrop));
    stats.record_write(&report(PipeFillPlan::ServoDrop));
    stats.record_write(&report(PipeFillPlan::ServoRepeat));
    stats.record_write(&report(PipeFillPlan::Write));
    stats.record_servo_depth(Some(PipeServoDepth {
        depth_frames: 1801,
        setpoint_frames: SETPOINT,
    }));
    // None (a respawned pw-cat that has not read yet) keeps the last published values.
    stats.record_servo_depth(None);
    let f = stats.snapshot();
    assert_eq!(f.pipe_start_holds, 1);
    assert_eq!((f.pipe_servo_drops, f.pipe_servo_repeats), (2, 1));
    assert_eq!(f.tx_blocks, 4, "a corrected block is still a written block");
    assert_eq!((f.pipe_trims, f.pipe_refills), (0, 0));
    assert_eq!(
        (f.pipe_depth_frames, f.pipe_setpoint_frames),
        (1801, SETPOINT as u64)
    );
    let v = serde_json::to_value(f).unwrap();
    for key in [
        "pipe_start_holds",
        "pipe_servo_drops",
        "pipe_servo_repeats",
        "pipe_depth_frames",
        "pipe_setpoint_frames",
    ] {
        assert!(v.get(key).is_some(), "{key} in {v}");
    }
}

#[test]
fn the_sink_thread_reads_the_pipe_between_blocks() {
    // The sink thread spawns pw-cat and is not unit-testable; anchor that it waits for a block at
    // most one sample interval, reads the pipe when none came, and publishes the servo's depth.
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/local_audio.rs");
    let src = std::fs::read_to_string(&p).expect("read local_audio.rs");
    let sink_loop = src.find("fn local_sink_loop(").expect("the sink thread");
    let source_loop = src
        .find("fn local_source_loop(")
        .expect("the capture thread");
    let body = &src[sink_loop..source_loop];
    assert!(body.contains("rx.recv_timeout(PIPE_SAMPLE_INTERVAL)"));
    let timeout = body
        .find("Err(RecvTimeoutError::Timeout)")
        .expect("no block yet");
    let sample = body.find("sink.sample_fill()").expect("read the pipe");
    assert!(timeout < sample);
    assert!(body.contains("stats.record_servo_depth(sink.servo_depth())"));
    assert!(body.contains("Err(RecvTimeoutError::Disconnected) => return"));
}

// --- the two-clock bench -------------------------------------------------------------------------

/// splitmix64: deterministic and std-only.
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
}

const NS: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;

#[derive(Debug, Clone, Copy)]
struct Spawn {
    /// The sink's clock against the hub's: + = pw-cat reads faster (the fill drains).
    sink_ppm: f64,
    /// pw-cat's first read, this long after the prime.
    connect_ns: u64,
    secs: u64,
    seed: u64,
}

/// Per-second time integrals of a piecewise fill (`area[s]` in frame-ns).
struct PerSecond {
    area: Vec<f64>,
}

impl PerSecond {
    fn new(secs: u64) -> Self {
        PerSecond {
            area: vec![0.0; secs as usize + 1],
        }
    }

    /// Add a segment from `from` to `to` whose fill goes linearly from `a` to `b` (a step: a == b).
    fn add(&mut self, from: u64, to: u64, a: f64, b: f64) {
        let mut t = from;
        while t < to {
            let sec = (t / NS) as usize;
            let next = ((sec as u64 + 1) * NS).min(to);
            let at = |x: u64| a + (b - a) * (x - from) as f64 / (to - from) as f64;
            if let Some(slot) = self.area.get_mut(sec) {
                *slot += (at(t) + at(next)) / 2.0 * (next - t) as f64;
            }
            t = next;
        }
    }

    fn means(&self, secs: u64) -> Vec<f64> {
        self.area[..secs as usize]
            .iter()
            .map(|a| a / NS as f64)
            .collect()
    }
}

#[derive(Debug, Default)]
struct Outcome {
    /// The fill pw-cat's first read found.
    first_read_fill: usize,
    trims: u64,
    refills: u64,
    holds: u64,
    /// pw-cat reads that found less than a quantum (would block its callback: an xrun).
    starved: u64,
    drops: u64,
    repeats: u64,
    /// Servo corrections in each second of hub time.
    corrections: Vec<u32>,
    /// The TRUE time-weighted pipe fill in each second, from the exact event times.
    depth: Vec<f64>,
    /// The fill as the hub can see it in each second: the trapezoids of its own readings (before
    /// and after each write, and every sample in between), what the servo is fed.
    seen: Vec<f64>,
}

impl Outcome {
    fn total_corrections(&self) -> u64 {
        self.drops + self.repeats
    }

    fn max_corrections_from(&self, s: usize) -> u32 {
        self.corrections[s..].iter().copied().max().unwrap_or(0)
    }

    fn max_error(values: &[f64]) -> f64 {
        values
            .iter()
            .map(|d| (d - SETPOINT as f64).abs())
            .fold(0.0, f64::max)
    }

    fn max_depth_error_from(&self, s: usize) -> f64 {
        Self::max_error(&self.depth[s..])
    }

    fn max_seen_error_from(&self, s: usize) -> f64 {
        Self::max_error(&self.seen[s..])
    }

    fn mean_depth_from(&self, s: usize) -> f64 {
        self.depth[s..].iter().sum::<f64>() / (self.depth.len() - s) as f64
    }
}

/// One pw-cat spawn: the prime, the start hold, then `secs` of the hub against the sink.
fn run(sp: Spawn) -> Outcome {
    let mut rng = Rng(sp.seed);
    let mut c = PipeFillControl::new();
    let mut out = Outcome {
        corrections: vec![0; sp.secs as usize],
        ..Default::default()
    };
    let end = sp.secs * NS;
    // The hub's block k is due at k x 5.333 ms; tokio wakes it at the next whole ms, then the OS
    // and the channel hop add up to 50 us.
    let block_at = |k: u64, rng: &mut Rng| {
        let due = k * 16_000_000 / 3;
        due.div_ceil(MS) * MS + rng.below(50_000)
    };
    // pw-cat reads one quantum per cycle of its own clock, with 30 us of callback jitter.
    let read_period = QUANTUM as f64 * 1e9 / (48_000.0 * (1.0 + sp.sink_ppm * 1e-6));
    let mut true_area = PerSecond::new(sp.secs);
    let mut seen_area = PerSecond::new(sp.secs);
    let mut k = 0u64;
    let mut next_block = block_at(0, &mut rng);
    let first_read = next_block + sp.connect_ns;
    let mut j = 0u64;
    let mut next_read = first_read;
    let mut next_sample = u64::MAX;
    let mut fill = 0usize;
    let mut last_t = 0u64;
    // The last reading the hub took (or what its last write left) and when.
    let (mut seen_t, mut seen_fill) = (0u64, 0usize);
    let mut read_seen = false;
    while next_block.min(next_sample) < end {
        let t = next_block.min(next_sample);
        // pw-cat reads that happen before the sink thread looks.
        while next_read <= t {
            true_area.add(last_t, next_read, fill as f64, fill as f64);
            last_t = next_read;
            if !read_seen {
                out.first_read_fill = fill;
                read_seen = true;
            }
            if fill >= QUANTUM {
                fill -= QUANTUM;
            } else {
                out.starved += 1;
                fill = 0;
            }
            j += 1;
            let jitter = rng.below(60_000) as f64 - 30_000.0;
            next_read = first_read + (j as f64 * read_period + jitter) as u64;
        }
        true_area.add(last_t, t, fill as f64, fill as f64);
        last_t = t;
        seen_area.add(seen_t, t, seen_fill as f64, fill as f64);
        seen_t = t;
        if next_block <= next_sample {
            let r = c.plan_block(t, fill, BLOCK);
            let sec = (t / NS) as usize;
            match r.plan {
                PipeFillPlan::Drop => out.trims += 1,
                PipeFillPlan::StartHold => out.holds += 1,
                PipeFillPlan::TopUp { .. } if !r.first => out.refills += 1,
                PipeFillPlan::ServoDrop => {
                    out.drops += 1;
                    out.corrections[sec] += 1;
                }
                PipeFillPlan::ServoRepeat => {
                    out.repeats += 1;
                    out.corrections[sec] += 1;
                }
                _ => {}
            }
            fill += r.plan.written_frames(BLOCK);
            k += 1;
            next_block = block_at(k, &mut rng).max(t + 1);
        } else {
            c.sample(t, fill);
        }
        seen_fill = fill;
        // recv_timeout(1 ms) from this wake, plus the wake-up latency.
        next_sample = t + PIPE_SAMPLE_INTERVAL.as_nanos() as u64 + 20_000 + rng.below(60_000);
    }
    out.depth = true_area.means(sp.secs);
    out.seen = seen_area.means(sp.secs);
    out
}

fn spawn(sink_ppm: f64, secs: u64, seed: u64) -> Spawn {
    Spawn {
        sink_ppm,
        connect_ns: 37 * MS,
        secs,
        seed,
    }
}

#[test]
fn every_spawn_starts_at_the_prime_and_settles_to_the_same_depth() {
    // 16 connect delays, 3..~120 ms, so pw-cat's first read falls at every phase of a hub block.
    let mut depths = Vec::new();
    for i in 0..16u64 {
        let connect_ns = 3 * MS + i * 7_770_000;
        let o = run(Spawn {
            sink_ppm: 0.0,
            connect_ns,
            secs: 120,
            seed: 0x1401 + i,
        });
        let at = format!("connect {:.2} ms", connect_ns as f64 / 1e6);
        assert_eq!(
            o.first_read_fill, PRIME,
            "{at}: the first read finds the prime"
        );
        assert!(o.holds > 0 || connect_ns < 6 * MS, "{at}");
        assert_eq!((o.trims, o.refills, o.starved), (0, 0, 0), "{at}");
        let first = &o.corrections[..20];
        assert!(o.max_corrections_from(0) <= 12, "{at}: {first:?}");
        // What the servo holds (the fill its readings show) is within 20 frames by 60 s.
        let seen = o.max_seen_error_from(60);
        assert!(seen <= 20.0, "{at}: the measured depth is off by {seen:.1}");
        // The true depth: the 1 ms readings place each read only within a millisecond, which
        // leaves a few frames on top of the servo's 16-frame band.
        let truth = o.max_depth_error_from(60);
        assert!(truth <= 30.0, "{at}: the true depth is off by {truth:.1}");
        depths.push(o.mean_depth_from(60));
    }
    // The same depth after every restart: all 16 within 1 ms (48 frames) of each other (the
    // rejected per-spawn setpoint varied by about 5 ms).
    let lo = depths.iter().copied().fold(f64::MAX, f64::min);
    let hi = depths.iter().copied().fold(f64::MIN, f64::max);
    assert!(hi - lo <= 48.0, "{lo:.1}..{hi:.1}");
}

#[test]
fn a_sink_at_the_hubs_rate_gets_no_corrections_after_settling() {
    let o = run(spawn(0.0, 3600, 7));
    assert_eq!((o.trims, o.refills, o.starved), (0, 0, 0));
    assert_eq!(o.max_corrections_from(60), 0, "{:?}", &o.corrections[..80]);
    assert!(o.max_seen_error_from(60) <= 20.0);
    assert!(o.max_depth_error_from(60) <= 30.0);
}

#[test]
fn a_50_ppm_sink_is_held_for_an_hour_with_a_few_corrections_a_second() {
    for (ppm, seed) in [(50.0, 11), (-50.0, 12)] {
        let o = run(spawn(ppm, 3600, seed));
        assert_eq!((o.trims, o.refills, o.starved), (0, 0, 0), "{ppm} ppm");
        let max = o.max_corrections_from(60);
        assert!(max <= 4, "{ppm} ppm: {max} corrections in one second");
        let err = o.max_depth_error_from(120);
        assert!(err <= 60.0, "{ppm} ppm: the true depth is off by {err:.1}");
        // The corrections are the drift itself: 50 ppm = 2.4 frames a second.
        let expected = 50e-6 * 48_000.0 * 3600.0;
        let net = o.drops as f64 - o.repeats as f64;
        assert!(
            (net.abs() - expected).abs() < expected * 0.05,
            "{ppm} ppm: net {net} vs {expected}"
        );
        assert_eq!(net > 0.0, ppm < 0.0, "a slow sink fills the pipe: drops");
    }
}

#[test]
fn a_200_ppm_sink_is_held_for_an_hour_without_a_guard() {
    for (ppm, seed) in [(200.0, 21), (-200.0, 22)] {
        let o = run(spawn(ppm, 3600, seed));
        assert_eq!((o.trims, o.refills, o.starved), (0, 0, 0), "{ppm} ppm");
        // About 10 a second: the drift is 9.6 frames a second.
        let rate = o.total_corrections() as f64 / 3600.0;
        assert!((9.0..=11.0).contains(&rate), "{ppm} ppm: {rate:.2}/s");
        let mut per_second = o.corrections[60..].to_vec();
        per_second.sort_unstable();
        let median = per_second[per_second.len() / 2];
        assert!((9..=11).contains(&median), "{ppm} ppm: median {median}/s");
        // The bench's seconds are not the servo's windows: one second can take the tail of one
        // window and the head of the next (each at most 11 here), never more than 15.
        let max = o.max_corrections_from(60);
        assert!(max <= 15, "{ppm} ppm: {max} corrections in one second");
        let err = o.max_depth_error_from(120);
        assert!(err <= 150.0, "{ppm} ppm: the true depth is off by {err:.1}");
    }
}
