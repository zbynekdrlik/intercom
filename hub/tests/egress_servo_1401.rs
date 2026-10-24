//! Issue 1401, egress step 2 (ROZHODNUTÉ 5979509439 + 5979627274): a fixed start depth and a held
//! depth on every playback pipe (the strih program sink and the cutters' cans); since issue 1461
//! lane 2 the depth is held by the resampler, never by a single-frame servo.
//!
//! - **Start hold.** pw-cat reads nothing until its stream runs, and the hub kept writing on top of
//!   the prime: a spawn started 62-85 ms deep, different after every restart. Until pw-cat first
//!   reads, a block that would take the fill above the target is now dropped, so its first read
//!   always finds exactly the prime.
//! - **The clock crossing.** The cans' sink is clocked by the card's crystal, not the hub's timer,
//!   and the bound-only guards turned +-50 ppm into a dropped 5.3 ms block every ~107 s or a refill
//!   gap every ~14 min. Issue 1401 held the pipe with the VBAN legs' single-frame servo; issue 1461
//!   (design comment 6376769274) replaced it with lane 1's resampler: the ratio follows the reader's
//!   rate (the least-squares slope of its consumed frames over 64 s), a phase term acts only outside
//!   +-1/4 of the fixed setpoint. No frame is inserted or dropped (graded on a tone in
//!   `egress_asrc_1461.rs`); here the frame-count bench pins the depth and the guards.
//! - **The depth is the time-weighted fill.** pw-cat takes four hub blocks at once, so the
//!   pre-write readings see a drift only as a whole-block step at each read/write phase crossing
//!   (design question 5979620130). The sink thread reads the pipe about every 1 ms between blocks,
//!   and each block's depth is the trapezoid mean of those readings.
//! - **A refill needs a starved pipe (step 4, design 5981457044).** The fill must read under one
//!   block on every reading for at least one hub period: a momentary low between a pw-cat read and
//!   the next write or catch-up burst is not starvation. The prime and the trim act on one reading.
//!
//! The two-clock bench drives the real `PipeFillControl` on frame counts: the hub's block loop on
//! its 5.333 ms grid (tokio rounds every wake up to the next ms, plus up to 50 us of OS latency),
//! the sink thread's `recv_timeout(1 ms)` readings in between, and a pw-cat consumer reading one
//! 1024-frame quantum per cycle on its own clock. It measures the TRUE time-weighted pipe fill
//! itself from the exact event times, never the controller's numbers.

use intercom_hub::janus_pacing::hub_block_period;
use intercom_hub::local_audio::{
    pipe_fill_bytes, LocalAudioStats, PipeFillWriter, PW_GRAPH_BURST_FRAMES,
};
use intercom_hub::pipe_fill::{
    pipe_setpoint, EgressQueue, PipeAsrcStats, PipeFillControl, PipeFillPlan, PipeWriteReport,
    PIPE_HIGH_FRAMES, PIPE_SAMPLE_INTERVAL, PIPE_TARGET_FRAMES,
};

/// The hub block on strih-snv (`[hub].block_frames`).
const BLOCK: usize = 256;
/// The hub's (and every pw-cat pipe's) sample rate.
const RATE: u32 = 48_000;
/// pw-cat's read: one graph quantum.
const QUANTUM: usize = PW_GRAPH_BURST_FRAMES;
/// The prime: the silence up to the target plus the prime's own block.
const PRIME: usize = PIPE_TARGET_FRAMES + BLOCK;
const SETPOINT: usize = pipe_setpoint(BLOCK);
/// The phase term's dead band: a quarter of the setpoint (448 frames).
const BAND: f64 = SETPOINT as f64 / 4.0;

// --- the pieces ----------------------------------------------------------------------------------

#[test]
fn the_setpoint_is_the_prime_minus_half_a_quantum() {
    assert_eq!(
        SETPOINT,
        PIPE_TARGET_FRAMES + BLOCK - PW_GRAPH_BURST_FRAMES / 2
    );
    assert_eq!(SETPOINT, 1792, "37.3 ms at 48 kHz");
    // It follows the block: half a quantum under whatever the prime is.
    assert_eq!(pipe_setpoint(512), PIPE_TARGET_FRAMES + 512 - 512);
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
fn each_write_puts_exactly_its_reports_frames_into_the_pipe() {
    // Issue 1461 re-pin: the plan no longer stretches a block by one frame; every write is the
    // top-up's silence plus what the resampler rendered, and a trimmed or held block writes nothing
    // but still leaves the queue (the queue never falls behind the controller).
    let mut c = PipeFillControl::new(RATE);
    let mut q = EgressQueue::new(2);
    let block: Vec<i16> = (0..BLOCK * 2).map(|i| i as i16 + 1).collect();
    let ms = 1_000_000u64;
    let mut t = 0;
    let mut check = |c: &mut PipeFillControl, fill: usize, t: u64| -> PipeWriteReport {
        let r = c.plan_block(t, fill, BLOCK);
        let written = q.render(&r, &block).map_or(0, |s| s.len() / 2);
        assert_eq!(written, r.written_frames(), "{r:?}");
        r
    };
    let prime = check(&mut c, 0, t);
    assert!(
        matches!(prime.plan, PipeFillPlan::TopUp { .. }) && prime.first,
        "{prime:?}"
    );
    assert_eq!(
        prime.written_frames(),
        PRIME,
        "the prime: the target plus one block"
    );
    t += 5 * ms;
    assert_eq!(check(&mut c, PRIME, t).plan, PipeFillPlan::StartHold);
    c.sample(t + ms, PRIME - QUANTUM);
    t += 5 * ms;
    let w = check(&mut c, PRIME - QUANTUM, t);
    assert_eq!(w.plan, PipeFillPlan::Write);
    assert_eq!(
        w.written_frames(),
        BLOCK,
        "ratio 1 before the first fit: a whole block"
    );
    t += 5 * ms;
    let trim = check(&mut c, PIPE_HIGH_FRAMES + 1, t);
    assert_eq!(trim.plan, PipeFillPlan::Drop);
    assert_eq!(trim.written_frames(), 0);
    assert!(
        trim.render.consume > 0,
        "a trimmed block still leaves the queue"
    );
}

// --- the start hold ------------------------------------------------------------------------------

#[test]
fn the_start_hold_drops_blocks_until_pw_cat_first_reads() {
    let mut c = PipeFillControl::new(RATE);
    let ms = 1_000_000u64;
    let prime = c.plan_block(0, 0, BLOCK);
    assert_eq!(prime.fill_frames, 0);
    assert!(prime.first);
    assert_eq!(prime.written_frames(), PRIME);
    assert!(matches!(prime.plan, PipeFillPlan::TopUp { .. }));
    assert!(!c.started());
    // pw-cat is still connecting: every block is held back and the pipe stays at the prime.
    for k in 1..30u64 {
        c.sample(k * 5 * ms + ms, PRIME);
        let r = c.plan_block(k * 5 * ms + 3 * ms, PRIME, BLOCK);
        assert_eq!(r.plan, PipeFillPlan::StartHold, "block {k}");
        assert!(!r.first);
    }
    assert!(
        c.asrc_stats().is_none(),
        "no resampler numbers before pw-cat reads"
    );
    // pw-cat's first read: the hold is over, and the block is written.
    c.sample(200 * ms, PRIME - QUANTUM);
    assert!(c.started());
    let r = c.plan_block(201 * ms, PRIME - QUANTUM, BLOCK);
    assert_eq!(r.plan, PipeFillPlan::Write);
    assert_eq!(c.asrc_stats().map(|d| d.setpoint_frames), Some(SETPOINT));
}

#[test]
fn the_depth_is_measured_from_pw_cats_first_read() {
    // The hold ends either at a 1 ms reading (here after a 400 ms hub stall, so the hold level
    // would weigh in heavily) or at the pre-write reading itself. Either way the depth's first
    // sample is the fill after the read, never the hold level before it.
    let step = 5 * MS;
    let after_read = PRIME - QUANTUM;
    // Every later block reads 1280 before its write and leaves 1536 after it: a trapezoid of 1408.
    let later = (after_read + after_read + BLOCK) / 2;
    for drop_seen_at_a_sample in [true, false] {
        let mut c = PipeFillControl::new(RATE);
        c.plan_block(0, 0, BLOCK);
        assert_eq!(
            c.plan_block(step, PRIME, BLOCK).plan,
            PipeFillPlan::StartHold
        );
        let mut t = 400 * MS;
        if drop_seen_at_a_sample {
            c.sample(t, after_read);
            t += MS;
        }
        assert_eq!(c.plan_block(t, after_read, BLOCK).plan, PipeFillPlan::Write);
        for _ in 0..187 {
            t += step;
            c.plan_block(t, after_read, BLOCK);
        }
        // One full second (188 blocks): the first sample (1280) and 187 trapezoids of 1408.
        let expected = ((after_read + 187 * later) as f64 / 188.0).round() as usize;
        assert_eq!(
            c.asrc_stats().map(|d| d.depth_frames),
            Some(expected),
            "drop seen at a sample: {drop_seen_at_a_sample}"
        );
    }
}

#[test]
fn a_guard_restarts_the_rate_fit_and_keeps_the_ratio() {
    // Issue 1461 re-pin (was: a guard forgets the servo's pending corrections). A trim or a refill
    // is the last resort, and the reads around it are a step in the reader's consumed-frames
    // series: the fit starts over (like a VBAN leg's underrun / resync), the ratio stays.
    for guard_fill in [PIPE_HIGH_FRAMES + 1, 0] {
        let mut c = PipeFillControl::new(RATE);
        c.plan_block(0, 0, BLOCK);
        let step = 16_000_000 / 3;
        let mut t = step;
        let mut fill = PRIME;
        // 20 s of pw-cat reading a quantum every four blocks, the hub writing what it renders.
        for k in 0..3750u64 {
            if k % 4 == 3 {
                c.sample(t - step / 2, fill - QUANTUM);
                fill -= QUANTUM;
            }
            let r = c.plan_block(t, fill, BLOCK);
            fill += r.written_frames();
            t += step;
        }
        let before = c.asrc_stats().expect("running");
        assert!(
            before.rate_fit_ppm.is_some(),
            "a fit after 20 s: {before:?}"
        );
        c.sample(t, guard_fill);
        t += 6 * MS;
        let guard = c.plan_block(t, guard_fill, BLOCK).plan;
        assert!(
            matches!(guard, PipeFillPlan::Drop | PipeFillPlan::TopUp { .. }),
            "{guard:?}"
        );
        if matches!(guard, PipeFillPlan::TopUp { .. }) {
            // A refill re-primes: the start hold again until pw-cat's next read.
            assert!(c.asrc_stats().is_none(), "held after the refill");
            c.sample(t + MS, PRIME - QUANTUM);
        }
        let after = c.asrc_stats().expect("running");
        assert_eq!(
            after.rate_fit_ppm, None,
            "the fit starts over after {guard:?}"
        );
        assert_eq!(after.rate_ppm, before.rate_ppm, "the ratio is kept");
    }
}

#[test]
fn a_real_pipe_holds_the_prime_until_it_is_read() {
    use std::io::Read;
    let (mut reader, writer) = std::io::pipe().expect("pipe");
    let probe = writer.try_clone().expect("clone the write end");
    let mut sink = PipeFillWriter::new(writer, 2, RATE);
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
    stats.record_asrc_stats(sink.asrc_stats());
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
fn a_pw_cat_that_exits_during_the_start_hold_is_noticed() {
    // Review round 1: during the hold the sink writes nothing, so it never gets the EPIPE that
    // made the sink thread respawn a dead pw-cat. A child that died before its first read must
    // still be noticed, by the 1 ms readings and by a held block alike.
    let (reader, writer) = std::io::pipe().expect("pipe");
    let mut sink = PipeFillWriter::new(writer, 2, RATE);
    let block: Vec<i16> = vec![3; BLOCK * 2];
    sink.write_block(&block).unwrap();
    sink.sample_fill().unwrap();
    assert_eq!(
        sink.write_block(&block).unwrap().plan,
        PipeFillPlan::StartHold
    );
    drop(reader); // pw-cat exits before reading
    let e = sink
        .sample_fill()
        .expect_err("a dead reader is noticed between blocks");
    assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe);
    let e = sink
        .write_block(&block)
        .expect_err("a held block notices it too");
    assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe);
}

#[test]
fn the_facet_counts_holds_and_shows_the_resampler() {
    let stats = LocalAudioStats::default();
    let report = |plan| PipeWriteReport {
        fill_frames: 1800,
        plan,
        ..Default::default()
    };
    stats.record_write(&report(PipeFillPlan::StartHold));
    stats.record_write(&report(PipeFillPlan::Write));
    stats.record_write(&report(PipeFillPlan::Write));
    let f = stats.snapshot();
    assert_eq!(f.pipe_rate_fit_ppm, None, "no fit before the first one");
    stats.record_asrc_stats(Some(PipeAsrcStats {
        depth_frames: 1801,
        setpoint_frames: SETPOINT,
        rate_ppm: -49.8,
        rate_fit_ppm: Some(-50.1),
        phase_frames: 9.0,
    }));
    // None (a respawned pw-cat that has not read yet) keeps the last published values.
    stats.record_asrc_stats(None);
    let f = stats.snapshot();
    assert_eq!(f.pipe_start_holds, 1);
    assert_eq!(f.tx_blocks, 2);
    assert_eq!((f.pipe_trims, f.pipe_refills), (0, 0));
    assert_eq!(
        (f.pipe_depth_frames, f.pipe_setpoint_frames),
        (1801, SETPOINT as u64)
    );
    assert_eq!(f.pipe_rate_ppm, -49.8);
    assert_eq!(f.pipe_rate_fit_ppm, Some(-50.1));
    assert_eq!(f.pipe_asrc_phase_frames, 9.0);
    let v = serde_json::to_value(f).unwrap();
    for key in [
        "pipe_start_holds",
        "pipe_rate_ppm",
        "pipe_rate_fit_ppm",
        "pipe_asrc_phase_frames",
        "pipe_depth_frames",
        "pipe_setpoint_frames",
    ] {
        assert!(v.get(key).is_some(), "{key} in {v}");
    }
    for gone in ["pipe_servo_drops", "pipe_servo_repeats"] {
        assert!(v.get(gone).is_none(), "{gone} is gone: {v}");
    }
}

#[test]
fn the_sink_thread_reads_the_pipe_between_blocks() {
    // The sink thread spawns pw-cat and is not unit-testable; anchor that it waits for a block at
    // most one sample interval, reads the pipe when none came, and publishes the resampler's
    // numbers.
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
    let sample = body
        .find("if let Err(e) = sink.sample_fill() {")
        .expect("read the pipe, and act on its error");
    assert!(timeout < sample);
    // A dead pw-cat seen by a 1 ms reading ends the loop, so the thread respawns it.
    let after = &body[sample..];
    let brk = after
        .find("break e;")
        .expect("a failed reading breaks the loop");
    assert!(brk < after.find('}').expect("the if block"), "{after:.120}");
    assert!(body.contains("stats.record_asrc_stats(sink.asrc_stats())"));
    assert!(body.contains("Err(RecvTimeoutError::Disconnected) => return"));
}

// --- the starvation-only refill (step 4, design 5981457044) -------------------------------------

/// A controller past its prime and pw-cat's first read, with the next reading due at 2 ms.
fn running() -> (PipeFillControl, u64) {
    let mut c = PipeFillControl::new(RATE);
    assert!(c.plan_block(0, 0, BLOCK).first, "the prime");
    c.sample(MS, PRIME - QUANTUM);
    assert!(c.started());
    (c, 2 * MS)
}

fn period() -> u64 {
    hub_block_period(BLOCK, RATE).as_nanos() as u64
}

fn is_top_up(plan: PipeFillPlan) -> bool {
    matches!(plan, PipeFillPlan::TopUp { .. })
}

#[test]
fn one_low_reading_is_no_refill_only_a_whole_hub_period_of_them_is() {
    // A momentary low: two pw-cat reads landed in a catch-up gap, the burst is about to arrive.
    let (mut c, t) = running();
    let r = c.plan_block(t, 100, BLOCK);
    assert!(!is_top_up(r.plan), "one low pre-write reading: {r:?}");
    assert_eq!(r.fill_frames, 100);
    // The burst's next block reads the block just written: the low is over.
    let r = c.plan_block(t + 20_000, 356, BLOCK);
    assert!(!is_top_up(r.plan), "{r:?}");

    // Low on a 1 ms reading and on the pre-write reading one nanosecond short of a hub period.
    let (mut c, t) = running();
    c.sample(t, 100);
    let r = c.plan_block(t + period() - 1, 100, BLOCK);
    assert!(!is_top_up(r.plan), "under one period: {r:?}");

    // A whole hub period of low readings: starved, topped up to the target, and not the prime.
    let (mut c, t) = running();
    c.sample(t, 100);
    let r = c.plan_block(t + period(), 100, BLOCK);
    assert_eq!(
        r.plan,
        PipeFillPlan::TopUp {
            silence_frames: PIPE_TARGET_FRAMES - 100
        }
    );
    assert_eq!(r.written_frames(), PIPE_TARGET_FRAMES - 100 + BLOCK);
    assert!(!r.first);
}

#[test]
fn a_reading_of_one_block_or_more_ends_the_low_run() {
    let (mut c, t) = running();
    c.sample(t, 100);
    c.sample(t + 2 * MS, BLOCK);
    c.sample(t + 3 * MS, 100);
    // A hub period after the FIRST low reading, but the run restarted at 3 ms.
    let r = c.plan_block(t + period() + MS, 100, BLOCK);
    assert!(!is_top_up(r.plan), "{r:?}");
    let r = c.plan_block(t + 3 * MS + period(), 100, BLOCK);
    assert!(is_top_up(r.plan), "{r:?}");
}

#[test]
fn a_blocked_pw_cat_that_swallows_every_block_is_refilled_within_one_hub_period() {
    // The live failure the guard exists for: pw-cat sits in its fread of a whole quantum and takes
    // every block the moment it lands, so the pipe reads 0 on every reading, also right after a
    // write. The run of low readings is not broken by the writes themselves.
    let (mut c, mut t) = running();
    let step = period();
    let first = c.plan_block(t, 0, BLOCK);
    assert!(!is_top_up(first.plan), "{first:?}");
    let mut refilled_at = None;
    for k in 1..=3u64 {
        for ms in 1..5 {
            c.sample(t + ms * MS, 0);
        }
        t += step;
        if is_top_up(c.plan_block(t, 0, BLOCK).plan) {
            refilled_at = Some(k);
            break;
        }
    }
    assert_eq!(refilled_at, Some(1), "refilled at the next block's write");
}

#[test]
fn the_prime_and_the_trim_still_act_on_one_reading_and_a_top_up_ends_the_run() {
    let mut c = PipeFillControl::new(RATE);
    let prime = c.plan_block(0, 0, BLOCK);
    assert!(prime.first && is_top_up(prime.plan), "{prime:?}");
    let (mut c, t) = running();
    assert_eq!(
        c.plan_block(t, PIPE_HIGH_FRAMES + 1, BLOCK).plan,
        PipeFillPlan::Drop,
        "one reading above the trim mark drops the block"
    );
    // After a refill the pipe holds the target: an older low reading no longer counts.
    let (mut c, t) = running();
    c.sample(t, 0);
    assert!(is_top_up(c.plan_block(t + period(), 0, BLOCK).plan));
    let r = c.plan_block(t + period() + 20_000, 0, BLOCK);
    assert!(
        !is_top_up(r.plan),
        "a new run starts after the top-up: {r:?}"
    );
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
    /// The TRUE time-weighted pipe fill in each second, from the exact event times.
    depth: Vec<f64>,
    /// The fill as the hub can see it in each second: the trapezoids of its own readings (before
    /// and after each write, and every sample in between), what the phase term is fed.
    seen: Vec<f64>,
    /// The resampler's ratio (ppm) and last fit at the end.
    rate_ppm: f64,
    rate_fit_ppm: Option<f64>,
}

impl Outcome {
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
    let mut c = PipeFillControl::new(RATE);
    let mut out = Outcome::default();
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
            match r.plan {
                PipeFillPlan::Drop => out.trims += 1,
                PipeFillPlan::StartHold => out.holds += 1,
                PipeFillPlan::TopUp { .. } if !r.first => out.refills += 1,
                _ => {}
            }
            fill += r.written_frames();
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
    if let Some(s) = c.asrc_stats() {
        out.rate_ppm = s.rate_ppm;
        out.rate_fit_ppm = s.rate_fit_ppm;
    }
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

/// The ratio a sink `ppm` fast needs: hub frames per written frame, in ppm off 1.
fn ratio_ppm(sink_ppm: f64) -> f64 {
    (1.0 / (1.0 + sink_ppm * 1e-6) - 1.0) * 1e6
}

#[test]
fn every_spawn_starts_at_the_prime_and_holds_its_depth_inside_the_band() {
    // 16 connect delays, 3..~120 ms, so pw-cat's first read falls at every phase of a hub block.
    // Issue 1461 re-pin: the single-frame servo walked every spawn to within 48 frames of the
    // setpoint; the resampler's phase term acts only outside +-1/4 of it (the design), so a spawn
    // keeps the depth its first read's phase gave it (+-128 frames, +-2.7 ms), fixed for the run.
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
        // Measured: within 133 frames, true and seen, over the 16 phases.
        let truth = o.max_depth_error_from(10);
        assert!(truth <= 150.0, "{at}: the true depth is off by {truth:.1}");
        let seen = o.max_seen_error_from(10);
        assert!(
            seen <= 150.0,
            "{at}: the measured depth is off by {seen:.1}"
        );
        // Fixed from the start: the depth of the first measured seconds is the depth at the end.
        let early = o.depth[10..20].iter().sum::<f64>() / 10.0;
        let late = o.mean_depth_from(100);
        assert!(
            (early - late).abs() <= 20.0,
            "{at}: the depth moved {early:.1} -> {late:.1}"
        );
    }
}

#[test]
fn a_sink_at_the_hubs_rate_holds_its_depth_with_no_guard() {
    // Measured: true 97, seen 97 (the spawn's read phase), ratio 0.0.
    let o = run(spawn(0.0, 3600, 7));
    assert_eq!((o.trims, o.refills, o.starved), (0, 0, 0));
    assert!(o.max_seen_error_from(10) <= 150.0);
    assert!(o.max_depth_error_from(10) <= 150.0);
    assert!(o.rate_ppm.abs() < 1.0, "ratio {:+.1} ppm", o.rate_ppm);
}

/// One drifting sink for an hour: no guard, the depth inside the dead band (no phase term needed),
/// the ratio (and its fit) the sink's clock within 1 ppm.
fn assert_held(ppm: f64, seed: u64) {
    let o = run(spawn(ppm, 3600, seed));
    assert_eq!((o.trims, o.refills, o.starved), (0, 0, 0), "{ppm} ppm");
    let err = o.max_depth_error_from(60);
    assert!(err <= 160.0, "{ppm} ppm: the true depth is off by {err:.1}");
    let seen = o.max_seen_error_from(60);
    assert!(
        seen < BAND,
        "{ppm} ppm: the measured depth is off by {seen:.1}"
    );
    let want = ratio_ppm(ppm);
    assert!(
        (o.rate_ppm - want).abs() < 1.0,
        "{ppm} ppm: ratio {:+.1} vs {want:+.1}",
        o.rate_ppm
    );
    let fit = o.rate_fit_ppm.expect("a fit");
    assert!((fit - want).abs() < 1.0, "{ppm} ppm: fit {fit:+.1}");
}

#[test]
fn a_50_ppm_sink_is_held_for_an_hour_by_the_ratio() {
    // Measured: true depth off by 87 / 114, seen 95 / 124 (the spawn's phase plus the walk before
    // the first fit), the ratio within 0.5 ppm of the sink.
    assert_held(50.0, 11);
    assert_held(-50.0, 12);
}

#[test]
fn a_200_ppm_sink_is_held_for_an_hour_without_a_guard() {
    // Measured: true 55 / 133, seen 62 / 140, the ratio within 0.3 ppm.
    assert_held(200.0, 21);
    assert_held(-200.0, 22);
}
