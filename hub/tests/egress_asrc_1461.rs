//! Issue 1461 lane 2 (design comment 6376769274): the hub's EGRESS crossings -- the cutters' card
//! and the vestibule HDMI through aplay, the strih-pp program sink through pw-cat -- and the phones'
//! Janus ring cross the reader's clock with the resampler, never by writing a block one frame
//! short or long (the issue-1401 pipe servo) or splicing 1 ms out of a frame (the Janus ring).
//!
//! Graded black-box through the real pieces: [`PipeFillControl`] plans every write and
//! [`EgressQueue`] turns it into the samples it puts into the pipe; [`PacedRing`] is pushed by the
//! hub's block loop and popped by the paced Janus sender. The hub writes a 997 Hz sine (-6 dBFS,
//! stereo with the right channel the negated left) on its exact 5.33 ms grid; the reader takes the
//! pipe on its own clock `ppm` off the hub's. What the card plays is exactly the sequence written,
//! as long as the reader never starves and nothing is trimmed or refilled (all asserted), so every
//! 50 ms window of that sequence after the settle is graded with the issue's THD+N bar (one cut or
//! repeated frame is about -30 dB, the resampler leaves the i16 rounding).

use std::time::{Duration, Instant};

use intercom_hub::janus_pacing::{
    PacedRing, PopKind, FRAME_48K, RING_CAP_FRAMES, RING_TARGET_FRAMES,
};
use intercom_hub::pipe_fill::{
    EgressQueue, PipeFillControl, PipeFillPlan, PIPE_SAMPLE_INTERVAL, PW_GRAPH_BURST_FRAMES,
};

#[path = "support/thd_n.rs"]
mod thd_n;
use thd_n::{grade_windows, THD_N_MAX_DB, WINDOW};

const RATE: f64 = 48_000.0;
const RATE_HZ: u32 = 48_000;
const BLOCK: usize = 256;
/// 997 Hz, never 1 kHz: a 1 kHz tone repeats exactly every 1 ms (48 samples), so the Janus ring's
/// 1 ms splice would be invisible in it.
const TONE_HZ: f64 = 997.0;
const AMP: f64 = 16_384.0;
const MS: u64 = 1_000_000;
const US: u64 = 1_000;
/// The model's time step.
const STEP: u64 = 100 * US;
/// aplay's period and ALSA buffer on the card leg and the HDMI leg (`--period-size=256
/// --buffer-size=1024`).
const PERIOD: usize = 256;
const ALSA_BUFFER: usize = 1024;

/// The hub's sample `n` of the tone (its own clock).
fn tone(n: u64) -> i16 {
    let phase = 2.0 * std::f64::consts::PI * TONE_HZ * n as f64 / RATE;
    (AMP * phase.sin()).round() as i16
}

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

/// Who reads the pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reader {
    /// `pw-cat --playback`: one 1024-frame graph quantum per cycle of its sink's clock.
    PwCat,
    /// `aplay` on a hw PCM: one 256-frame period at a time into a 1024-frame ALSA buffer that the
    /// card (or HDMI) clock drains; it plays once the buffer is full.
    Aplay,
}

#[derive(Debug, Clone, Copy)]
struct Case {
    reader: Reader,
    /// The reader's clock against the hub's: + = it reads faster.
    ppm: f64,
    /// From this many seconds on the reader runs at the second ppm instead.
    step: Option<(f64, f64)>,
    settle_s: f64,
    measure_s: f64,
    seed: u64,
}

#[derive(Debug, Default)]
struct Played {
    /// The left channel the reader plays after the settle.
    left: Vec<i16>,
    /// Every played frame had right == -left (the channels never mix).
    mirrored: bool,
    /// pw-cat reads that found less than a quantum (its callback blocks: an xrun).
    starved: u64,
    /// aplay's ALSA buffer ran dry.
    xruns: u64,
    refills: u64,
    trims: u64,
}

fn pipe_case(reader: Reader, ppm: f64, seed: u64) -> Case {
    Case {
        reader,
        ppm,
        step: None,
        settle_s: 40.0,
        measure_s: 30.0,
        seed,
    }
}

/// One egress pipe from the spawn: the prime, the start hold, then the hub against the reader.
fn run_pipe(k: Case) -> Played {
    let mut rng = Rng(k.seed);
    let mut c = match k.reader {
        Reader::PwCat => PipeFillControl::new(RATE_HZ),
        Reader::Aplay => {
            PipeFillControl::with_quantum(RATE_HZ, PERIOD).with_reader_buffer(ALSA_BUFFER + PERIOD)
        }
    };
    let mut q = EgressQueue::new(2);
    let ppm_at = |t: u64| match k.step {
        Some((from, p)) if t as f64 >= from * 1e9 => p,
        _ => k.ppm,
    };
    let settle = (k.settle_s * 1e9) as u64;
    let end = ((k.settle_s + k.measure_s) * 1e9) as u64;
    // The hub's block n is due at n x 5.333 ms; the sink thread takes it from the egress channel
    // up to 50 us later.
    let block_at = |n: u64, rng: &mut Rng| n * 16_000_000 / 3 + rng.below(50 * US);
    let connect = 37 * MS;
    let mut out = Played {
        mirrored: true,
        ..Default::default()
    };
    let mut pipe = 0usize;
    // pw-cat: its clock's cycle grid and the time of its next quantum read.
    let mut read_grid = connect as f64;
    let mut next_read = read_grid;
    // aplay: its ALSA buffer level, the period it holds, whether the PCM runs.
    let (mut alsa, mut in_hand, mut playing) = (0.0f64, false, false);
    let mut n = 0u64;
    let mut next_block = block_at(0, &mut rng);
    let mut next_sample = MS;
    let mut t = 0u64;
    while t < end {
        let frames_per_ns = RATE * (1.0 + ppm_at(t) * 1e-6) / 1e9;
        match k.reader {
            Reader::PwCat => {
                if t as f64 >= next_read {
                    let q = PW_GRAPH_BURST_FRAMES;
                    if pipe >= q {
                        pipe -= q;
                    } else {
                        out.starved += 1;
                        pipe = 0;
                    }
                    // The next cycle of the sink's clock, read with 30 us of callback jitter.
                    read_grid += q as f64 / frames_per_ns;
                    next_read = read_grid + rng.below(60 * US) as f64 - 30_000.0;
                }
            }
            Reader::Aplay => {
                if playing {
                    alsa -= frames_per_ns * STEP as f64;
                    if alsa < 0.0 {
                        out.xruns += 1;
                        alsa = 0.0;
                        playing = false;
                    }
                }
                let mut progress = t >= connect;
                while progress {
                    progress = false;
                    if !in_hand && pipe >= PERIOD {
                        pipe -= PERIOD;
                        in_hand = true;
                        progress = true;
                    }
                    if in_hand && alsa + PERIOD as f64 <= ALSA_BUFFER as f64 {
                        alsa += PERIOD as f64;
                        in_hand = false;
                        progress = true;
                        if alsa >= ALSA_BUFFER as f64 {
                            playing = true;
                        }
                    }
                }
            }
        }
        if t >= next_block {
            let block: Vec<i16> = (0..BLOCK as u64)
                .flat_map(|i| {
                    let s = tone(n * BLOCK as u64 + i);
                    [s, -s]
                })
                .collect();
            let r = c.plan_block(t, pipe, BLOCK);
            match r.plan {
                PipeFillPlan::Drop => out.trims += 1,
                PipeFillPlan::TopUp { .. } if !r.first => out.refills += 1,
                _ => {}
            }
            if let Some(samples) = q.render(&r, &block) {
                pipe += samples.len() / 2;
                if t >= settle {
                    for f in samples.as_chunks::<2>().0 {
                        out.left.push(f[0]);
                        out.mirrored &= f[1] == f[0].wrapping_neg();
                    }
                }
            }
            n += 1;
            next_block = block_at(n, &mut rng).max(t + 1);
            next_sample = t + PIPE_SAMPLE_INTERVAL.as_nanos() as u64 + 20 * US + rng.below(60 * US);
        } else if t >= next_sample {
            c.sample(t, pipe);
            next_sample = t + PIPE_SAMPLE_INTERVAL.as_nanos() as u64 + 20 * US + rng.below(60 * US);
        }
        t += STEP;
    }
    out
}

/// The tone in the reader's samples at `ppm`: the hub's tone read by a clock `ppm` fast.
fn w0(ppm: f64) -> f64 {
    2.0 * std::f64::consts::PI * TONE_HZ / (RATE * (1.0 + ppm * 1e-6))
}

fn assert_pipe_clean(k: Case) {
    let p = run_pipe(k);
    let at = format!("{:?} at {:+} ppm (step {:?})", k.reader, k.ppm, k.step);
    assert_eq!(
        (p.starved, p.xruns, p.refills, p.trims),
        (0, 0, 0, 0),
        "{at}: the reader never starves, nothing is refilled or trimmed"
    );
    assert!(p.mirrored, "{at}: the channels never mix");
    let final_ppm = k.step.map_or(k.ppm, |(_, p)| p);
    let (worst, over, windows) = grade_windows(&p.left, w0(final_ppm));
    let expected = (k.measure_s * RATE / WINDOW as f64) as usize;
    assert!(windows + 2 >= expected, "{at}: {windows} windows measured");
    assert_eq!(
        over, 0,
        "{at}: {over} of {windows} 50 ms windows over {THD_N_MAX_DB} dB THD+N (worst {worst:.1} dB): a frame was inserted or dropped"
    );
}

#[test]
fn a_pw_cat_sink_at_the_hubs_rate_plays_the_tone_clean() {
    assert_pipe_clean(pipe_case(Reader::PwCat, 0.0, 1));
}

#[test]
fn a_drifting_pw_cat_sink_is_resampled_never_a_frame_dropped_or_repeated() {
    for (ppm, seed) in [(50.0, 11), (-50.0, 12), (200.0, 13), (-200.0, 14)] {
        assert_pipe_clean(pipe_case(Reader::PwCat, ppm, seed));
    }
}

#[test]
fn a_pw_cat_sink_whose_clock_steps_100ppm_is_followed_without_a_click() {
    assert_pipe_clean(Case {
        step: Some((40.0, 100.0)),
        measure_s: 60.0,
        ..pipe_case(Reader::PwCat, 0.0, 21)
    });
}

#[test]
fn an_aplay_card_at_the_hubs_rate_plays_the_tone_clean() {
    assert_pipe_clean(pipe_case(Reader::Aplay, 0.0, 31));
}

#[test]
fn a_drifting_aplay_card_or_hdmi_clock_is_resampled_never_a_frame_dropped_or_repeated() {
    for (ppm, seed) in [(50.0, 41), (-50.0, 42), (200.0, 43), (-200.0, 44)] {
        assert_pipe_clean(pipe_case(Reader::Aplay, ppm, seed));
    }
}

#[test]
fn an_aplay_card_whose_clock_steps_100ppm_is_followed_without_a_click() {
    assert_pipe_clean(Case {
        step: Some((40.0, -100.0)),
        measure_s: 60.0,
        ..pipe_case(Reader::Aplay, 0.0, 51)
    });
}

// --- the phones: the Janus ring --------------------------------------------------------------

#[derive(Debug, Default)]
struct RingRun {
    /// The popped audio after the settle.
    played: Vec<i16>,
    underflows: u64,
    trims: u64,
    /// Silent frames popped after the first audio frame.
    silent_after_start: u64,
}

/// The block loop pushes the mono tone on the hub's grid, the paced sender pops one 20 ms
/// frame a tick on its own grid. Times are on the sender's clock; the hub's runs `ppm` fast.
fn run_ring(ppm: f64, settle_s: f64, measure_s: f64, seed: u64) -> RingRun {
    let mut rng = Rng(seed);
    let t0 = Instant::now();
    let mut ring = PacedRing::new(RING_TARGET_FRAMES, RING_CAP_FRAMES);
    let block_s = BLOCK as f64 / RATE / (1.0 + ppm * 1e-6);
    let tick_s = FRAME_48K as f64 / RATE;
    let end = settle_s + measure_s;
    let mut out = RingRun::default();
    let (mut k, mut j) = (0u64, 0u64);
    let mut started = false;
    loop {
        let t_push = k as f64 * block_s + rng.below(30) as f64 * 1e-6;
        let t_pop = (j + 1) as f64 * tick_s + rng.below(100) as f64 * 1e-6;
        if t_push.min(t_pop) >= end {
            break;
        }
        if t_push <= t_pop {
            let mono: Vec<i16> = (0..BLOCK as u64)
                .map(|i| tone(k * BLOCK as u64 + i))
                .collect();
            ring.push_at(&mono, t0 + Duration::from_secs_f64(t_push));
            k += 1;
        } else {
            let (frame, kind) = ring.pop_frame();
            assert_eq!(frame.len(), FRAME_48K);
            match kind {
                PopKind::Audio => started = true,
                _ if started => out.silent_after_start += 1,
                _ => {}
            }
            if t_pop >= settle_s {
                out.played.extend_from_slice(&frame);
            }
            j += 1;
        }
    }
    out.underflows = ring.underflows();
    out.trims = ring.trims();
    out
}

#[test]
fn the_janus_ring_crosses_a_drifting_feed_with_the_resampler_never_a_splice() {
    for (ppm, seed) in [(62.5, 61), (-62.5, 62), (500.0, 63), (-500.0, 64)] {
        let r = run_ring(ppm, 30.0, 30.0, seed);
        assert_eq!(
            (r.underflows, r.trims, r.silent_after_start),
            (0, 0, 0),
            "{ppm:+} ppm: no underflow, trim or silent frame once audio flows"
        );
        let w = 2.0 * std::f64::consts::PI * TONE_HZ * (1.0 + ppm * 1e-6) / RATE;
        let (worst, over, windows) = grade_windows(&r.played, w);
        assert!(windows >= 590, "{ppm:+} ppm: {windows} windows");
        assert_eq!(
            over, 0,
            "{ppm:+} ppm: {over} of {windows} windows over {THD_N_MAX_DB} dB (worst {worst:.1} dB): the ring spliced"
        );
    }
}
