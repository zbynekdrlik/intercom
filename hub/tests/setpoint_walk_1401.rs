//! Issue 1401, design 5981457044 (ROZHODNUTÉ 5981448853): a moved setpoint is walked GENTLY.
//!
//! A program feed's target follows its sender's gaps (`adaptive_target`), and every change goes
//! through `NetworkFill::set_target`. That used to hand the drift servo a one-block (256-frame)
//! error, the servo's STEEP zone: a raise was walked at up to 48 corrections a second for about
//! 5 s (47 measured in the growing-gap replay), the flutter review 1 of the VBAN-leg servo
//! rejected. Now the servo holds a setpoint that walks to the new target at most
//! `SERVO_WALK_MAX_PER_WINDOW` (7) frames a second, the gentle zone's own ceiling; the drift
//! correction is measured against that setpoint and keeps its full budget on top.
//!
//! These tests drive the real `NetworkFill` on frame counts, popping one 256-frame block on the
//! hub's exact grid and feeding a clean sender at a fixed rate offset. They count every correction
//! by the output frame it lands on and bound them in EVERY 1 s span (a sliding window), so no
//! alignment between a measured second and the servo's own window can hide a burst. The walk's own
//! share (`servo_walk_steps`) is bounded the same way: at most 7 in any second, also when the
//! drift servo corrects on top of it.

use intercom_hub::adaptive_target::program_target_blocks;
use intercom_hub::vban_jitter::{
    servo_corrections, vban_cap_blocks, NetworkFill, PopPlan, SERVO_DEADBAND_FRAMES,
    SERVO_WALK_MAX_PER_WINDOW, SERVO_WINDOW_FRAMES, VBAN_PROGRAM_TARGET_BLOCKS,
};

const BLOCK: usize = 256;
const FLOOR: usize = VBAN_PROGRAM_TARGET_BLOCKS * BLOCK;
const RAISED: usize = FLOOR + BLOCK;
const POPS_PER_S: usize = SERVO_WINDOW_FRAMES / BLOCK;

/// One servo correction: the output frame it landed on, a drop (else a repeat), and whether it was
/// a walk step (else a drift correction).
#[derive(Debug, Clone, Copy)]
struct Correction {
    at: usize,
    drop: bool,
    walk: bool,
}

/// One program-feed leg on the real controller: a sender `ppm` off the hub, its frames arriving
/// evenly (no jitter), one pop per hub block.
struct Leg {
    f: NetworkFill,
    fill: usize,
    per_pop: f64,
    owed: f64,
    out_frames: usize,
    corrections: Vec<Correction>,
    ran_dry: u64,
    overruns: u64,
    /// The pre-pop fills and setpoints of the current second, and every finished second's means.
    sec_fill: usize,
    sec_setpoint: usize,
    sec_pops: usize,
    seconds: Vec<(usize, usize)>,
}

impl Leg {
    /// A leg primed at `target` (cap by the derived rule) whose sender runs `ppm` off the hub.
    fn new(target: usize, ppm: f64) -> Self {
        let cap = vban_cap_blocks(target / BLOCK) * BLOCK;
        Leg {
            f: NetworkFill::new(target, cap),
            fill: target,
            per_pop: BLOCK as f64 * (1.0 + ppm * 1e-6),
            owed: 0.0,
            out_frames: 0,
            corrections: Vec::new(),
            ran_dry: 0,
            overruns: 0,
            sec_fill: 0,
            sec_setpoint: 0,
            sec_pops: 0,
            seconds: Vec::new(),
        }
    }

    fn run_s(&mut self, secs: usize) {
        for _ in 0..secs * POPS_PER_S {
            self.pop();
        }
    }

    fn pop(&mut self) {
        let before = self.f.stats();
        self.sec_fill += self.fill;
        self.sec_setpoint += self.f.setpoint();
        self.sec_pops += 1;
        match self.f.plan_pop(self.fill, BLOCK) {
            PopPlan::Silent { ran_dry } => self.ran_dry += u64::from(ran_dry),
            PopPlan::Audio { skip, take } => self.fill -= skip + take,
        }
        let after = self.f.stats();
        let walk = after.servo_walk_steps > before.servo_walk_steps;
        if after.servo_drops > before.servo_drops {
            self.corrections.push(Correction {
                at: self.out_frames,
                drop: true,
                walk,
            });
        }
        if after.servo_repeats > before.servo_repeats {
            self.corrections.push(Correction {
                at: self.out_frames,
                drop: false,
                walk,
            });
        }
        assert_eq!(
            after.setpoint_frames,
            self.f.setpoint(),
            "the stats carry it"
        );
        self.out_frames += BLOCK;
        if self.sec_pops == POPS_PER_S {
            self.seconds.push((
                self.sec_fill / self.sec_pops,
                self.sec_setpoint / self.sec_pops,
            ));
            (self.sec_fill, self.sec_setpoint, self.sec_pops) = (0, 0, 0);
        }
        self.owed += self.per_pop;
        let arrive = self.owed.floor();
        self.owed -= arrive;
        self.fill += arrive as usize;
        if let Some(keep) = self.f.overrun_keep(self.fill) {
            self.fill = keep;
            self.overruns += 1;
        }
    }

    /// The most corrections inside any span of one second of output from `from` on.
    fn max_in_any_second(&self, from: usize) -> usize {
        self.max_of_in_any_second(from, |_| true)
    }

    /// The most WALK steps inside any span of one second of output from `from` on.
    fn max_walk_in_any_second(&self, from: usize) -> usize {
        self.max_of_in_any_second(from, |c| c.walk)
    }

    fn max_of_in_any_second(&self, from: usize, keep: impl Fn(&Correction) -> bool) -> usize {
        let at: Vec<usize> = self
            .corrections
            .iter()
            .filter(|c| c.at >= from && keep(c))
            .map(|c| c.at)
            .collect();
        let mut best = 0;
        let mut lo = 0;
        for hi in 0..at.len() {
            while at[hi] - at[lo] >= SERVO_WINDOW_FRAMES {
                lo += 1;
            }
            best = best.max(hi - lo + 1);
        }
        best
    }

    fn count_from(&self, from: usize, drop: bool) -> usize {
        self.corrections
            .iter()
            .filter(|c| c.at >= from && c.drop == drop)
            .count()
    }

    /// The first output second (counted from `from`) whose setpoint is the target.
    fn walk_done_after_s(&self, from: usize) -> Option<usize> {
        let first = from / SERVO_WINDOW_FRAMES;
        self.seconds[first..]
            .iter()
            .position(|&(_, sp)| sp == self.f.target())
    }
}

#[test]
fn a_raised_target_is_walked_in_at_no_more_than_seven_corrections_a_second() {
    assert_eq!(
        SERVO_WALK_MAX_PER_WINDOW, 7,
        "the gentle zone's own ceiling"
    );
    assert_eq!(SERVO_WALK_MAX_PER_WINDOW, servo_corrections(128));
    let mut leg = Leg::new(FLOOR, 0.0);
    leg.run_s(30);
    assert!(
        leg.corrections.is_empty(),
        "a clean feed needs no correction"
    );
    let raise_at = leg.out_frames;
    leg.f.set_target(RAISED);
    assert_eq!(leg.f.target(), RAISED);
    assert_eq!(
        leg.f.setpoint(),
        FLOOR,
        "the servo still holds the old fill"
    );
    leg.run_s(60);
    let max = leg.max_in_any_second(raise_at);
    assert!(
        max <= SERVO_WALK_MAX_PER_WINDOW,
        "a raise is walked in the gentle zone only: {max} corrections in one second"
    );
    assert_eq!(leg.count_from(raise_at, true), 0, "a raise never drops");
    assert_eq!(
        leg.count_from(raise_at, false),
        BLOCK,
        "exactly the one block of setpoint error is repeated in"
    );
    assert_eq!(
        leg.f.stats().servo_walk_steps,
        BLOCK as u64,
        "every one a walk step"
    );
    // 256 frames at 7 a second: about 37 s, never the steep zone's 5 s.
    let done = leg.walk_done_after_s(raise_at).expect("the walk finishes");
    assert!((36..=40).contains(&done), "walked in {done} s");
    let (mean, _) = *leg.seconds.last().unwrap();
    assert!(
        mean.abs_diff(RAISED) <= SERVO_DEADBAND_FRAMES,
        "the fill sits at the new target: {mean}"
    );
    assert_eq!((leg.ran_dry, leg.overruns), (0, 0));
}

#[test]
fn a_lowered_target_is_walked_down_just_as_gently() {
    let mut leg = Leg::new(RAISED, 0.0);
    leg.run_s(30);
    let lower_at = leg.out_frames;
    leg.f.set_target(FLOOR);
    leg.run_s(60);
    let max = leg.max_in_any_second(lower_at);
    assert!(max <= SERVO_WALK_MAX_PER_WINDOW, "{max} in one second");
    assert_eq!(
        leg.count_from(lower_at, false),
        0,
        "a lowering never repeats"
    );
    assert_eq!(leg.count_from(lower_at, true), BLOCK);
    let done = leg.walk_done_after_s(lower_at).expect("the walk finishes");
    assert!((36..=40).contains(&done), "walked down in {done} s");
    assert_eq!((leg.ran_dry, leg.overruns), (0, 0));
}

#[test]
fn a_fill_already_past_the_setpoint_carries_the_walk_without_a_correction_against_it() {
    // A lowering walk, and 5 s in, 200 frames of the oldest audio go (a lost hub tick's give-up):
    // the fill is then past the setpoint toward the target. The setpoint follows it; no correction
    // ever pulls the fill back up against the walk.
    let mut leg = Leg::new(RAISED, 0.0);
    leg.run_s(30);
    let lower_at = leg.out_frames;
    leg.f.set_target(FLOOR);
    leg.run_s(5);
    leg.fill -= 200;
    leg.run_s(60);
    assert_eq!(
        leg.count_from(lower_at, false),
        0,
        "no repeat fights the walk"
    );
    let drops = leg.count_from(lower_at, true);
    assert!(
        drops <= BLOCK - 200 + SERVO_DEADBAND_FRAMES,
        "only what the give-up did not already take: {drops}"
    );
    assert!(leg.max_in_any_second(lower_at) <= SERVO_WALK_MAX_PER_WINDOW);
    assert_eq!(leg.f.setpoint(), FLOOR);
}

#[test]
fn drift_at_200_ppm_keeps_its_full_budget_while_a_walk_is_in_progress() {
    // The step-2 bound at +-200 ppm: the servo's view of the depth within 147 frames of where it
    // holds it. A walk must not starve the drift correction, nor the drift the walk.
    for ppm in [200.0, -200.0] {
        let mut leg = Leg::new(FLOOR, ppm);
        leg.run_s(60);
        let raise_at = leg.out_frames;
        leg.f.set_target(RAISED);
        leg.run_s(120);
        let first = raise_at / SERVO_WINDOW_FRAMES;
        // The drift's own equilibrium error before the raise (133 frames at 200 ppm).
        let before = leg.seconds[first - 5..first]
            .iter()
            .map(|&(mean, setpoint)| mean.abs_diff(setpoint))
            .max()
            .unwrap();
        for (s, &(mean, setpoint)) in leg.seconds[first..].iter().enumerate() {
            let error = mean.abs_diff(setpoint);
            assert!(
                error <= 147,
                "{ppm} ppm, {s} s after the raise: depth {mean} vs setpoint {setpoint}"
            );
            // While the walk runs it takes none of the drift's budget: the drift's error does not
            // grow to make up for corrections the walk took (with the walk's share spent first it
            // grew 133 -> 136). The second the walk ends steps it once by up to 3 frames, and the
            // drift takes that back.
            let slack = if setpoint < RAISED { 2 } else { 3 };
            assert!(
                error <= before + slack,
                "{ppm} ppm, {s} s after the raise: drift error {error} vs {before} before it"
            );
        }
        if ppm < 0.0 {
            // A slow sender needs a repeat about every 5000 frames: the raise never pauses them
            // (set_target keeps the window and its drift budget; restarting them left a 1 s gap).
            let gap = leg
                .corrections
                .windows(2)
                .filter(|w| w[0].at + 2 * SERVO_WINDOW_FRAMES >= raise_at)
                .filter(|w| w[0].at <= raise_at + 10 * SERVO_WINDOW_FRAMES)
                .map(|w| w[1].at - w[0].at)
                .max()
                .unwrap();
            assert!(
                gap <= SERVO_WINDOW_FRAMES / 4,
                "the drift paused for {gap} frames at the raise"
            );
        }
        // The drift's share of every second is spent first (its full budget), and the walk's
        // steps keep their own spacing, so under a heavy drift the walk yields: at -200 ppm the
        // drift's ~10 corrections a second leave room for about 3 walk steps (88 s measured).
        // At +200 ppm the sender itself carries the fill toward the raised target (11 s).
        let done = leg.walk_done_after_s(raise_at).expect("the walk finishes");
        assert!(done <= 100, "{ppm} ppm: the walk keeps going ({done} s)");
        let walk = leg.max_walk_in_any_second(raise_at);
        assert!(
            walk <= SERVO_WALK_MAX_PER_WINDOW,
            "{ppm} ppm: {walk} walk steps in one second"
        );
        // A second plans the drift's corrections plus at most 7 walk steps and spreads them all
        // across it: never bunched at the 1000-frame spacing at its start.
        let spread = SERVO_WINDOW_FRAMES / (servo_corrections(147) + SERVO_WALK_MAX_PER_WINDOW);
        let closest = leg
            .corrections
            .windows(2)
            .filter(|w| w[0].at >= raise_at)
            .map(|w| w[1].at - w[0].at)
            .min()
            .unwrap();
        assert!(
            closest >= spread,
            "{ppm} ppm: two corrections {closest} frames apart"
        );
        let (mean, _) = *leg.seconds.last().unwrap();
        assert!(mean.abs_diff(RAISED) <= 147, "{ppm} ppm: {mean}");
        let max = leg.max_in_any_second(raise_at);
        assert!(
            max <= servo_corrections(147) + SERVO_WALK_MAX_PER_WINDOW,
            "{ppm} ppm: {max} in one second"
        );
        assert_eq!((leg.ran_dry, leg.overruns), (0, 0), "{ppm} ppm");
    }
}

/// What one run on the FOH burst pattern measured from the target move on.
#[derive(Debug)]
struct BurstRun {
    /// The most corrections, and the most walk steps, in any 1 s span.
    max: usize,
    max_walk: usize,
    /// The walk steps in all.
    walked: u64,
    ran_dry: u64,
    overruns: u64,
}

/// The FOH sender's measured pattern at a FIXED long gap: 48 kHz audio handed out in a burst every
/// 6 ms, and every 402 ms the next burst held back `gap_us` (as in `adaptive_target_1401.rs`). The
/// target moves from `from` to `to` blocks after 120 s (`from == to`: no move, the control).
fn burst_pattern_walk(gap_us: u64, from: usize, to: usize) -> BurstRun {
    let mut leg = Leg::new(from * BLOCK, 0.0);
    // The sender's bursts are the only arrivals: no even feed, and the leg primes from empty.
    leg.per_pop = 0.0;
    leg.fill = 0;
    let (mut sent, mut burst_t) = (0u64, 6_000u64);
    let mut moved_at = 0;
    for pop in 0..240 * POPS_PER_S as u64 {
        let pop_us = pop * BLOCK as u64 * 1_000_000 / 48_000;
        loop {
            let offset = burst_t % 402_000;
            let arrive = if offset > 0 && offset < gap_us {
                burst_t - offset + gap_us
            } else {
                burst_t
            };
            if arrive > pop_us {
                break;
            }
            let produced = burst_t * 48_000 / 1_000_000;
            leg.fill += (produced - sent) as usize;
            sent = produced;
            if let Some(keep) = leg.f.overrun_keep(leg.fill) {
                leg.fill = keep;
                leg.overruns += 1;
            }
            burst_t += 6_000;
        }
        if pop == 120 * POPS_PER_S as u64 {
            leg.f.set_target(to * BLOCK);
            moved_at = leg.out_frames;
        }
        leg.pop();
    }
    BurstRun {
        max: leg.max_in_any_second(moved_at),
        max_walk: leg.max_walk_in_any_second(moved_at),
        walked: leg.f.stats().servo_walk_steps,
        ran_dry: leg.ran_dry,
        overruns: leg.overruns,
    }
}

#[test]
fn a_walk_on_the_foh_burst_pattern_takes_at_most_seven_walk_steps_a_second() {
    // The FOH sender's measured gaps, each at targets that cover it (the adaptive rule's own
    // number and one or two blocks above): a raise, a lowering, and a lowering from further up.
    for gap_us in [19_400, 25_000, 27_600, 31_000, 35_000] {
        let need = program_target_blocks(std::time::Duration::from_micros(gap_us), BLOCK, 48_000);
        for (from, to) in [(need, need + 1), (need + 1, need), (need + 2, need + 1)] {
            let r = burst_pattern_walk(gap_us, from, to);
            let ctx = format!("gap {gap_us} us, {from} -> {to} blocks: {r:?}");
            assert!(r.max_walk <= SERVO_WALK_MAX_PER_WINDOW, "{ctx}");
            // One block walked in steps; a wobble past the band toward the target carried a frame
            // or two for free in some rows.
            let walked = usize::try_from(r.walked).unwrap();
            assert!(
                walked <= BLOCK && BLOCK - walked <= 2,
                "one block walked\n{ctx}"
            );
            assert_eq!((r.ran_dry, r.overruns), (0, 0), "{ctx}");
            // The 1 s mean of a bursty sender wobbles (a window holds two or three long gaps), and
            // at some gap / target phases it leaves the servo's band now and then: the drift
            // servo answers it with one correction, with or without a walk (the control below).
            // An eighth correction in a second needs such a correction to land inside a walk's
            // span: measured only on the 35 ms rows. A setpoint that followed the wobble forward
            // (no band) left the low windows outside the band and added it on the 25 ms rows.
            let total = if gap_us >= 35_000 {
                SERVO_WALK_MAX_PER_WINDOW + 1
            } else {
                SERVO_WALK_MAX_PER_WINDOW
            };
            assert!(r.max <= total, "{ctx}");
        }
    }
    // The control: no target move at 35 ms, and the drift servo still corrects the wobble.
    let steady = burst_pattern_walk(35_000, 9, 9);
    assert_eq!(steady.walked, 0, "{steady:?}");
    assert!(
        steady.max >= 1,
        "the drift servo answers the 35 ms wobble by itself: {steady:?}"
    );
}

#[test]
fn a_walk_reversed_in_the_middle_of_a_second_never_corrects_the_wrong_way() {
    // A raise walk, and 10.5 s in (mid-second, about 70 frames walked) the target goes back down.
    // The unspent walk steps of that second toward the old target are dropped: not one repeat
    // after the reversal, the walked frames come back out as drops, and the leg ends at the target.
    let mut leg = Leg::new(FLOOR, 0.0);
    leg.run_s(30);
    leg.f.set_target(RAISED);
    leg.run_s(10);
    for _ in 0..POPS_PER_S / 2 {
        leg.pop();
    }
    let walked = leg.f.setpoint() - FLOOR;
    assert!(walked > 50, "mid-walk: {walked} frames walked");
    let reversed_at = leg.out_frames;
    leg.f.set_target(FLOOR);
    leg.run_s(40);
    assert_eq!(
        leg.count_from(reversed_at, false),
        0,
        "no repeat after the reversal"
    );
    assert_eq!(
        leg.count_from(reversed_at, true),
        walked,
        "the walked frames come back out"
    );
    assert!(leg.max_in_any_second(reversed_at) <= SERVO_WALK_MAX_PER_WINDOW);
    assert_eq!(leg.f.setpoint(), FLOOR);
    let (mean, setpoint) = *leg.seconds.last().unwrap();
    assert_eq!(setpoint, FLOOR);
    assert!(mean.abs_diff(FLOOR) <= SERVO_DEADBAND_FRAMES, "{mean}");
    assert_eq!((leg.ran_dry, leg.overruns), (0, 0));
}

#[test]
fn a_tick_lost_during_a_raise_walk_keeps_what_lies_under_the_target_floor() {
    // 5 s into a one-block raise the block loop loses one tick: the block that arrived meanwhile
    // sits in the leg, and `discard_for_missed_ticks` gives it up only down to the TARGET's floor
    // (target - half a block), which during a raise lies above the walked setpoint. What is kept
    // is a free step of the walk: the fill is ahead of the setpoint toward the target, the
    // setpoint follows it with no correction, and nothing pulls the fill back down.
    let mut leg = Leg::new(FLOOR, 0.0);
    leg.run_s(30);
    leg.f.set_target(RAISED);
    leg.run_s(5);
    let lost_at = leg.out_frames;
    let setpoint = leg.f.setpoint();
    leg.fill += BLOCK;
    let given_up = leg.f.discard_for_missed_ticks(leg.fill, BLOCK, 1);
    assert!(
        given_up > 0 && given_up < BLOCK,
        "part of the lost block is kept: {given_up} given up"
    );
    leg.fill -= given_up;
    let kept = BLOCK - given_up;
    leg.run_s(60);
    assert_eq!(leg.count_from(lost_at, true), 0, "no drop pulls it back");
    assert!(leg.max_in_any_second(lost_at) <= SERVO_WALK_MAX_PER_WINDOW);
    let repeats = leg.count_from(lost_at, false);
    assert!(
        repeats + kept <= RAISED - setpoint + SERVO_DEADBAND_FRAMES,
        "the kept {kept} frames shortened the walk: {repeats} repeats left"
    );
    assert_eq!(leg.f.setpoint(), RAISED);
    assert_eq!((leg.ran_dry, leg.overruns), (0, 0));
}

#[test]
fn set_target_moves_the_target_and_cap_and_a_prime_or_a_trim_ends_the_walk() {
    let cap = vban_cap_blocks(VBAN_PROGRAM_TARGET_BLOCKS) * BLOCK;
    // Not primed yet: the leg simply primes to the new target.
    let mut f = NetworkFill::new(FLOOR, cap);
    f.set_target(RAISED);
    assert_eq!((f.target(), f.setpoint()), (RAISED, RAISED));
    // Primed: the setpoint stays, the target moves and the cap with it, by the derived rule.
    let mut f = NetworkFill::new(FLOOR, cap);
    assert!(matches!(f.plan_pop(FLOOR, BLOCK), PopPlan::Audio { .. }));
    f.set_target(RAISED);
    assert_eq!((f.target(), f.setpoint()), (RAISED, FLOOR));
    let raised_cap = vban_cap_blocks(RAISED / BLOCK) * BLOCK;
    assert_eq!(f.overrun_keep(raised_cap), None, "the cap moved up");
    // The trim leaves the fill exactly at the target: the walk is over.
    assert_eq!(f.overrun_keep(raised_cap + 1), Some(RAISED));
    assert_eq!(f.setpoint(), RAISED);
    // An underrun's re-prime starts at the target too.
    f.set_target(FLOOR);
    assert_eq!(f.setpoint(), RAISED);
    assert_eq!(
        f.plan_pop(BLOCK - 1, BLOCK),
        PopPlan::Silent { ran_dry: true }
    );
    assert!(matches!(f.plan_pop(FLOOR, BLOCK), PopPlan::Audio { .. }));
    assert_eq!(f.setpoint(), FLOOR);
}
