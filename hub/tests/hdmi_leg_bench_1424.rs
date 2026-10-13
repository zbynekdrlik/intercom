//! issue 1424 part 3: the HDMI leg's pipe against a modelled `aplay` on the HDMI hw PCM, two clocks.
//!
//! aplay reads its stdin one 256-frame period at a time and writes it into a 1024-frame ALSA buffer
//! that the HDMI audio clock drains; it starts playing once the buffer is full, and blocks on the
//! PCM while it holds a period that does not fit. The hub writes one 256-frame block per tick on its
//! own clock and reads the pipe every millisecond between ticks, exactly like the sink thread.
//!
//! The spawn must not starve the pipe (the prime carries aplay's buffer + the period it holds), the
//! leg must never refill, trim or xrun afterwards, and the pipe must sit at the 640-frame setpoint.
//! Std-only (Tier-0 replica).

use intercom_hub::hdmi_audio::{ALSA_HDMI_BUFFER_FRAMES, ALSA_HDMI_PERIOD_FRAMES};
use intercom_hub::hdmi_leg::aplay_pipe_control;
use intercom_hub::pipe_fill::PipeFillPlan;

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;
const NS: u64 = 1_000_000_000;
const STEP_NS: u64 = 100_000; // the model's 0.1 ms resolution

#[derive(Default)]
struct Run {
    topups_after_prime: u32,
    trims: u32,
    xruns: u32,
    corrections: u32,
    first: Option<PipeFillPlan>,
    depth_end: usize,
    setpoint: usize,
}

/// aplay against the pipe for `secs` seconds, the HDMI clock `ppm` off the hub's.
fn run(secs: u64, ppm: f64) -> Run {
    let mut c = aplay_pipe_control(48_000);
    let tick_ns = BLOCK as f64 * 1e9 / RATE; // the exact hub period
    let drain_per_ns = RATE * (1.0 + ppm * 1e-6) / 1e9;
    let mut pipe: usize = 0;
    let mut alsa: f64 = 0.0;
    let mut in_hand = false;
    let mut started = false;
    let mut next_tick = 0f64;
    let mut next_sample: u64 = 1_000_000;
    let mut r = Run::default();
    let mut t: u64 = 0;
    while t < secs * NS {
        // the HDMI clock drains the ALSA buffer once aplay started the PCM
        if started {
            alsa -= drain_per_ns * STEP_NS as f64;
            if alsa < 0.0 {
                r.xruns += 1;
                alsa = 0.0;
                started = false; // ALSA stops on an xrun; aplay re-prepares and restarts when full
            }
        }
        // aplay: read a period when it has none, put it into the PCM when it fits
        loop {
            let mut progress = false;
            if !in_hand && pipe >= ALSA_HDMI_PERIOD_FRAMES {
                pipe -= ALSA_HDMI_PERIOD_FRAMES;
                in_hand = true;
                progress = true;
            }
            if in_hand && alsa + ALSA_HDMI_PERIOD_FRAMES as f64 <= ALSA_HDMI_BUFFER_FRAMES as f64 {
                alsa += ALSA_HDMI_PERIOD_FRAMES as f64;
                in_hand = false;
                progress = true;
                if alsa >= ALSA_HDMI_BUFFER_FRAMES as f64 {
                    started = true;
                }
            }
            if !progress {
                break;
            }
        }
        // the hub's write on its tick, its 1 ms readings in between
        if t as f64 >= next_tick {
            let rep = c.plan_block(t, pipe, BLOCK);
            pipe += rep.plan.written_frames(BLOCK);
            match rep.plan {
                PipeFillPlan::TopUp { .. } if rep.first => r.first = Some(rep.plan),
                PipeFillPlan::TopUp { .. } => r.topups_after_prime += 1,
                PipeFillPlan::Drop => r.trims += 1,
                PipeFillPlan::ServoDrop | PipeFillPlan::ServoRepeat => r.corrections += 1,
                _ => {}
            }
            next_tick += tick_ns;
            next_sample = t + 1_000_000;
        } else if t >= next_sample {
            c.sample(t, pipe);
            next_sample += 1_000_000;
        }
        t += STEP_NS;
    }
    if let Some(d) = c.servo_depth() {
        r.depth_end = d.depth_frames;
        r.setpoint = d.setpoint_frames;
    }
    r
}

#[test]
fn the_spawn_primes_aplays_buffer_and_the_pipe_never_starves() {
    let r = run(60, 0.0);
    assert_eq!(
        r.first,
        Some(PipeFillPlan::TopUp {
            silence_frames: 512 + ALSA_HDMI_BUFFER_FRAMES + ALSA_HDMI_PERIOD_FRAMES
        }),
        "the prime = the pipe target + aplay's ALSA buffer + the period it holds"
    );
    assert_eq!(r.topups_after_prime, 0, "no refill after the prime");
    assert_eq!(r.trims, 0);
    assert_eq!(r.xruns, 0);
    assert_eq!(r.setpoint, 640);
    assert!(
        r.depth_end.abs_diff(640) <= 40,
        "the pipe sits at the setpoint: {}",
        r.depth_end
    );
}

#[test]
fn a_drifting_hdmi_clock_is_held_by_the_servo_without_refills_or_xruns() {
    for ppm in [-200.0, -50.0, 50.0, 200.0] {
        let r = run(120, ppm);
        assert_eq!(r.topups_after_prime, 0, "{ppm} ppm: no refill");
        assert_eq!(r.trims, 0, "{ppm} ppm: no trim");
        assert_eq!(r.xruns, 0, "{ppm} ppm: no xrun");
        assert!(r.corrections > 0, "{ppm} ppm: the servo corrects the drift");
        assert!(
            r.depth_end.abs_diff(640) <= 160,
            "{ppm} ppm: depth {} held near 640",
            r.depth_end
        );
    }
}
